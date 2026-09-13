//! 常驻 Agent Session：事件驱动 driver。v2 核心翻转——messages 是**每轮从 history 重建**的派生视图，
//! 不再跨轮持有。history 是唯一真相之源（append-only jsonl）。
//! 本模块只放「可纯逻辑测」的部分：事件类型、handle_event 编排（jobdone_body 注入正文）。
use crate::config::Config;
use crate::llm::{self, ChatResponse};
use crate::tools::ToolsCtx;
use crate::jobs::JobOutcome;
use crate::ask_user::AskUserRequest;
use tauri::{AppHandle, Emitter as TauriEmitter, Manager};

#[derive(Debug, Clone)]
pub enum SessionEvent {
    UserMessage { text: String, attachments: Vec<crate::AttachmentRef>, source: String },
    ContextNote { text: String },
    JobDone(JobOutcome),
    Reset,
    /// 内部信号：60s idle ticker 投递；driver 串行 dispatch_dream（不并发 dream）。不入 history。
    DreamCheck,
    /// 内部信号：dream 工具投递；driver 串行 dispatch_dream(force=true)——跳过 idle/token 阈值，
    /// 仍守 in_flight/has_new 门。与 DreamCheck 同：不入 history。工具返回同步收据，整理结果下一轮反映。
    DreamRequest,
    /// 助手经 attach 工具请求纳入图片：driver 构造一条带图 user 消息（下轮 M3 vision）。
    /// 不冒充用户：文本中性（inject_text）；history 存 kind=user（复用用户拖图管道）。
    InjectAttachment { attachments: Vec<crate::AttachmentRef>, caption: String },
    /// 内部信号：tool_ask_user 投递；driver 派发给前端 + 注册 oneshot 等待 AwaitUserAnswer。
    /// 不入 history（跟 DreamCheck 同）；ask_user 等待期间不接受第二个 ask_user（单飞防嵌套）。
    AwaitUserRequest(AskUserRequest),
    /// 用户答/跳过/超时/取消：driver 路由回对应 oneshot；同时落 history（kind=tool_result,配对 ask_user call_id）。
    AwaitUserAnswer {
        question_id: String,
        call_id: String,
        answer: Option<String>,
        skipped: bool,
        timed_out: bool,
        cancelled: bool,
        auto_submitted: bool,
        fallback_no_recommended: bool,
    },
}

/// JobOutcome → 注入正文字符串（v2：作为 external 事件的 what 落 history）。
fn jobdone_body(o: &JobOutcome) -> String {
    match o.kind {
        crate::jobs::JobKind::Process => {
            if o.ok {
                format!("[后台任务 #{} 完成] 退出码 {}\n{}", o.job_id, o.code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()), o.tail)
            } else {
                format!("[后台任务 #{} 失败]\n{}", o.job_id, o.tail)
            }
        }
        crate::jobs::JobKind::Agent => {
            let cap = o.label.as_deref().map(|c| format!("（{c}）")).unwrap_or_default();
            let answer = o.answer.clone().unwrap_or_default();
            match (&o.note, o.ok) {
                (None, true) => format!("[子代理 #{} 完成{}]\n{}", o.job_id, cap, answer),
                (None, false) => format!("[子代理 #{} 失败{}]\n{}", o.job_id, cap, answer),
                (Some(note), _) => format!("[子代理 #{} {}{}；部分产出：\n{}]", o.job_id, note, cap, answer),
            }
        }
    }
}

/// 系统注入 user 消息的中性文本（不冒充用户下指令）。
/// M3 须明白：图是助手自己要看，响应接 assistant 上一轮的自检意图——故文本只陈述事实，不下命令。
fn inject_text(caption: &str) -> String {
    if caption.trim().is_empty() {
        "[系统：助手通过 attach 请求纳入以下图片]".to_string()
    } else {
        format!("[系统：助手通过 attach 请求纳入以下图片：{caption}]")
    }
}

/// 用户提示词前缀快照落盘：仅 user-role 事件（user/subagent_result/external）且 enabled 且 prefix 非空时，
/// 把当前 config 的 prefix 文本快照写进 data["prefix"]。返回是否写入。
/// 纯函数（不碰 history writer / IO），便于单测；handle_event 在 append 前调用。
pub fn maybe_snapshot_prefix(current: &mut crate::history::HistoryEvent, cfg: &Config) -> bool {
    if matches!(current.kind.as_str(), "user" | "subagent_result" | "external")
        && cfg.user_prompt_prefix_enabled
        && !cfg.user_prompt_prefix.trim().is_empty()
    {
        current.data.insert("prefix".into(), serde_json::json!(cfg.user_prompt_prefix));
        true
    } else {
        false
    }
}

/// 处理一个事件：落 history（经单写 writer）→ 若该事件触发 turn，从 history 重建 messages 并 run_turn。
/// Reset/ContextNote 不跑 turn（仅落 history）。纯逻辑：注入 round/emitter + 真实 writer + tempdir 即可离线测。
///
/// 关键正确性（免 flush 竞态 + 免重复）：每轮顺序固定为
/// **`read_all(history_dir)` → `history.append(current)` → `events.push(current)` → `build_messages(&events,..)`**。
/// read_all 在 append 之前 → 磁盘上绝无 current → 本地 push 只补一份 → 无重复；本地 push 确定 → 无需等 flush。
pub async fn handle_event<E: llm::Emitter>(
    event: &SessionEvent,
    history: &crate::history::HistoryWriterHandle,
    history_dir: &std::path::Path,
    cache: &std::path::Path,
    ctx: &ToolsCtx,
    cfg: &Config,
    round: std::sync::Arc<dyn llm::LlmRound>,
    emitter: &E,
) -> Option<ChatResponse> {
    let now = now_ms();
    let mut current = match event {
        SessionEvent::UserMessage { text, attachments, source } => {
            let mut ev = crate::history::HistoryEvent::user(now, "main", text, attachments);
            ev.data.insert("source".into(), serde_json::json!(source));
            ev
        }
        SessionEvent::ContextNote { text } => {
            crate::history::HistoryEvent::external(now, text, "", None)
        }
        SessionEvent::JobDone(o) => match o.kind {
            crate::jobs::JobKind::Agent => crate::history::HistoryEvent::subagent_result(
                now, &o.job_id, &o.answer.clone().unwrap_or_default(), &format!("thread=agent:{}", o.job_id), o.ok),
            crate::jobs::JobKind::Process => crate::history::HistoryEvent::external(now, &jobdone_body(o), "", None),
        },
        SessionEvent::Reset => {
            crate::history::HistoryEvent::marker(now, "reset", history.current_seq())
        }
        SessionEvent::InjectAttachment { attachments, caption } => {
            crate::history::HistoryEvent::user(now, "main", &inject_text(caption), attachments)
        }
        // 用户回答 ask_user：按 tool_result 落 history,与 assistant 的 ask_user tool_call 配对。
        // 这样 LLM 重建消息时看到「自己调了工具、工具返回了用户的选择」,语义干净;
        // UI 也归工具结果卡,不产生假用户气泡。
        SessionEvent::AwaitUserAnswer { call_id, answer, skipped, timed_out, cancelled, auto_submitted, fallback_no_recommended, .. } => {
            let body = ask_user_result_text(answer.as_deref(), *skipped, *timed_out, *cancelled, *auto_submitted, *fallback_no_recommended);
            crate::history::HistoryEvent::tool_result(now, "main", "ask_user", &body, call_id)
        }
        // DreamCheck / DreamRequest 是 driver 内部信号：run_one 在 handle_event 之前已路由到 dispatch_dream 并 return；
        // 永不应进入 handle_event。exhaustive match 要求覆盖此臂。
        // AwaitUserRequest 同款：tool_ask_user 返 AskUserHandle → driver 转 AwaitUserRequest emit 给前端,
        // run_one 在 handle_event 之前已注册 oneshot + return;不进 handle_event。
        SessionEvent::DreamCheck | SessionEvent::DreamRequest
        | SessionEvent::AwaitUserRequest(_) => unreachable!("Dream*/AwaitUserRequest 由 run_one 路由,不应进 handle_event"),
    };
    // 本地 push 须带 writer 的下一个 seq（构造器给 0）：否则 current.seq=0 < last_marker，
    // 被 build_messages 的 marker 窗口（seq > last_marker）排除 → rebuild 只剩 [system]
    // → MiniMax 400 "chat content is empty"。current_seq() 在 append 之前读 = writer 即将盖的 seq（> 所有旧事件/marker）。
    current.seq = history.current_seq();
    maybe_snapshot_prefix(&mut current, cfg);
    // 正确性顺序：先 read_all（current 尚未落盘）→ append → 本地 push（确定性补 current，免 flush 等待、免重复）
    let mut events = crate::history::read_all(history_dir);
    history.append(current.clone());
    events.push(current);

    let triggers_turn = matches!(event,
        SessionEvent::UserMessage { .. } | SessionEvent::JobDone(_)
        | SessionEvent::InjectAttachment { .. }
        | SessionEvent::AwaitUserAnswer { .. });
    if !triggers_turn {
        return None; // ContextNote（external）、Reset（marker）：仅落 history，下轮重建自然反映
    }
    let pinned = crate::context::load_pinned(cache, &cfg.system_prompt);
    let mut messages = crate::context::build_messages(&events, &pinned, cfg.dream_cap_turns as usize);
    // 中断句柄:set token 供 interrupt_task 命令 cancel;turn 结束(正常/中断/出错)统一 clear。
    let token = tokio_util::sync::CancellationToken::new();
    ctx.interrupt.set(token);
    let res = llm::run_turn(round, emitter, cfg, &mut messages, ctx, cfg.max_tool_iters as usize, "main").await;
    ctx.interrupt.clear();
    Some(res)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// 常驻 Session 的句柄：放进 app state；命令只 tx.send()。Sender 本身 Send+Sync，无需 Mutex。
pub struct SessionHandle {
    pub tx: tokio::sync::mpsc::Sender<SessionEvent>,
}

const CHANNEL_CAP: usize = 64; // G3 bounded 背压

/// 启动常驻 driver；返回放进 app state 的句柄。
/// v2：spawn history writer（单写 task），持有 history_dir；每轮 run_one 经 handle_event 重建 messages。
/// registry / job_update 由调用方传入，与 list_jobs/kill_job 命令共享同一 registry。
/// 公共签名固定 4 参（lib.rs 调用，不改 lib.rs）。
///
/// **启动时序正确性（C1 修复）：** 本函数从 setup 同步主线程被调用，那里**无 Tokio runtime
/// thread-local guard**。`history::spawn_writer` 内部 `tokio::spawn` 必须在 runtime 上下文里——
/// 故 history writer spawn + ToolsCtx 构造都放进 `tauri::async_runtime::spawn` 异步块首行
/// （async_runtime::spawn 不需要 thread-local guard，但其内部 await 点后即在 runtime 上，
/// 此时 `tokio::spawn` 可用）。
pub fn spawn_session(
    app: AppHandle,
    registry: crate::jobs::SharedRegistry,
    job_update: std::sync::Arc<dyn crate::jobs::JobUpdate>,
    sub_stream: std::sync::Arc<dyn crate::subagents::SubagentStream>,
) -> SessionHandle {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<SessionEvent>(CHANNEL_CAP);
    let cfg = crate::config::load(&app);
    let workspace = std::path::PathBuf::from(&cfg.workspace_dir);
    let cache = std::path::PathBuf::from(&cfg.cache_dir);
    let _ = std::fs::create_dir_all(&workspace);
    // 固定子目录结构（projects/scripts/datas），首启即建、可见；AGENT.md 规范 agent 在此分类存放。
    for sub in ["projects", "scripts", "datas", "render"] {
        let _ = std::fs::create_dir_all(workspace.join(sub));
    }
    let _ = std::fs::create_dir_all(&cache);
    let history_dir = cache.join("history");
    let (job_done_tx, mut job_done_rx) = tokio::sync::mpsc::channel::<crate::jobs::JobOutcome>(CHANNEL_CAP);
    // cfg/workspace/cache/history_dir 入 Arc 以便 move 进 spawned task（多 clone 友好，借用清白）
    let cfg = std::sync::Arc::new(cfg);
    let workspace_rc = std::sync::Arc::new(workspace);
    let cache_rc = std::sync::Arc::new(cache);
    let history_dir_rc = std::sync::Arc::new(history_dir);
    // dream 触发状态：driver 单持（与 idle ticker 共享同一 Arc<Mutex>）。启动播种提取前沿但不 armed。
    let trigger = std::sync::Arc::new(std::sync::Mutex::new({
        let mut t = crate::dream_trigger::DreamTrigger::new();
        t.seed_from_history(&crate::history::read_all(&history_dir_rc));
        t
    }));
    // idle ticker：每 60s 投 DreamCheck（driver 串行判定，不并发 dream）。
    // ticker 与 driver 共享 trigger Arc（保活 + 单飞门防并发）；发送 DreamCheck 经 driver 的 select 投递。
    {
        let tx2 = tx.clone();
        let _trigger_keepalive = trigger.clone();
        tauri::async_runtime::spawn(async move {
            let _ = _trigger_keepalive; // 保活：ticker 期间 trigger Arc 不被释放
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                let _ = tx2.send(SessionEvent::DreamCheck).await;
            }
        });
    }
    // scheduler runner：per-task 精确 timer（每任务独立 sleep 到自己触发点）+ 10s watcher 同步配置。
    let scheduler_runner = crate::scheduler::SchedulerRunner::new(tx.clone(), (*cache_rc).clone());
    app.manage(scheduler_runner.clone());
    scheduler_runner.start();
    // 中断句柄:ctx(读 token)与 app.state(interrupt_task 命令 cancel)共享同一 Arc 实例。
    let interrupt = crate::tools::InterruptHandle::new();
    app.manage(interrupt.clone());
    let session_tx_for_ctx = tx.clone(); // 给主 driver ctx 用；tx 本身留给 SessionHandle { tx }
    // ask_user 重投 AwaitUserAnswer 回主循环用 + SessionHandle 返出再 clone 一份。
    let driver_tx_for_ask = tx.clone();
    let handle_tx = tx.clone();
    tauri::async_runtime::spawn(async move {
        // C1 修复：spawn_writer（内含 tokio::spawn）必须在 runtime 上下文里——故放本异步块首行。
        let history = crate::history::spawn_writer((*history_dir_rc).clone(), crate::history::local_offset_secs());
        let job_writer = crate::jobs::spawn_job_writer((*cache_rc).join(".ovoice-jobs"), crate::history::local_offset_secs());
        // writer 入 app state：force_quit（T5）从 state 取，复用主 writer 保单写不变量（不另 spawn）
        app.manage(job_writer.clone());
        // legacy 迁移：旧 u64 时代的 {n}.log → legacy-{n}.log + 写 legacy 记录（spec D5）
        {
            let jobs_dir = (*cache_rc).join(".ovoice-jobs");
            crate::jobs::migrate_legacy_logs(&jobs_dir, crate::history::local_offset_secs(), &job_writer);
        }
        // 启动 load 今日 job jsonl：重建 registry + 恢复 counter + 悬空 running 标 forced_exit
        // （spec §5.2：阶段2 仅 load+标记+显示；主动触发汇报 turn 属阶段3）
        {
            let mut r = registry.lock().unwrap();
            crate::jobs::load_today(&(*cache_rc).join(".ovoice-jobs"), crate::history::local_offset_secs(), &mut r, &job_writer);
        }
        let ctx = ToolsCtx {
            workspace: (*workspace_rc).clone(),
            cache: (*cache_rc).clone(),
            jobs: registry.clone(),
            job_done_tx,
            job_update,
            minimax_region: cfg.minimax_region.clone(),
            allow_background: true,
            subagent_stream: sub_stream,
            history: history.clone(),
            job_writer,
            session_tx: session_tx_for_ctx,
            interrupt: interrupt.clone(),
            tasks: app.state::<crate::tasks::DbActorHandle>().inner().clone(),
        };
        // ask_user 单飞挂起表：question_id → oneshot sender。
        // 共享注册表(lib.rs setup 时 manage),user_answered 命令与 driver 共用——
        // 命令直 fire oneshot,绕过等待期间被阻塞的主循环(防自锁)。
        let pending_ask = app.state::<crate::ask_user::SharedPendingAsk>().inner().clone();
        loop {
            tokio::select! {
                Some(o) = job_done_rx.recv() => {
                    run_one(&SessionEvent::JobDone(o), &history, &history_dir_rc, &cache_rc, &ctx, &app, &trigger, &pending_ask, &driver_tx_for_ask).await;
                }
                ev = rx.recv() => match ev {
                    Some(e) => {
                        run_one(&e, &history, &history_dir_rc, &cache_rc, &ctx, &app, &trigger, &pending_ask, &driver_tx_for_ask).await;
                    }
                    None => break,
                }
            }
        }
    });
    SessionHandle { tx: handle_tx }
}

/// 处理一个事件（driver 内部）：handle_event 落 history + 触发 turn；Reset 在 handle_event 写完 marker 后
/// 由 driver 专属逻辑 cancel agent jobs（CancellationToken drop ≠ cancel，防孤儿 tokio 烧 token）
/// + 重置 registry + 通知前端清屏（chat-reset）。
///
/// DreamCheck（来自 60s idle ticker）不进 handle_event，直接 dispatch_dream（cap/idle 串行判定）；其余
/// 事件走 handle_event；UserMessage / JobDone 在 turn 后再 dispatch_dream（轮间 cap 检查）。
async fn run_one(
    e: &SessionEvent,
    history: &crate::history::HistoryWriterHandle,
    history_dir: &std::sync::Arc<std::path::PathBuf>,
    cache: &std::sync::Arc<std::path::PathBuf>,
    ctx: &ToolsCtx,
    app: &AppHandle,
    trigger: &std::sync::Arc<std::sync::Mutex<crate::dream_trigger::DreamTrigger>>,
    pending_ask: &crate::ask_user::SharedPendingAsk,
    driver_tx: &tokio::sync::mpsc::Sender<SessionEvent>,
) {
    // 用户活动（user 消息 / external 拖入 / 用户答 ask_user）：记一笔供 idle 触发 + armed 门
    if matches!(e, SessionEvent::UserMessage { .. } | SessionEvent::ContextNote { .. } | SessionEvent::AwaitUserAnswer { .. }) {
        trigger.lock().unwrap().note_activity(now_ms());
    }
    // 每轮重读 config：save_config 只写盘不刷新 driver 持有的 cfg Arc，故启动时的旧值会一直被
    // 复用 → system_prompt / user_prompt_prefix / dream_* / max_tool_iters 等轮内字段保存后不生效
    // （旧版靠 reset_session 清历史来"换 system_prompt"，既无效又毁历史）。这里每轮从盘重读，
    // 与 load_pinned 每轮重读 SOUL/AGENT/MEMORY 同一模式。启动固定字段（workspace/cache/minimax_region）
    // 不读 fresh_cfg，仍走 ctx / 启动解析的目录，改它们需重启（符合直觉）。
    let fresh_cfg = std::sync::Arc::new(crate::config::load(app));
    // DreamCheck / DreamRequest：driver 串行 dispatch（不并发 dream，prepare 的 in_flight 兜底）
    // DreamRequest 来自 dream 工具 → force=true（跳过阈值，仍守 in_flight/has_new 门）
    if matches!(e, SessionEvent::DreamCheck) {
        dispatch_dream(trigger, history_dir, cache, &fresh_cfg, false).await;
        return;
    }
    if matches!(e, SessionEvent::DreamRequest) {
        dispatch_dream(trigger, history_dir, cache, &fresh_cfg, true).await;
        return;
    }
    let emit = crate::AppEmitter { app: app.clone() };
    let round: std::sync::Arc<dyn llm::LlmRound> = std::sync::Arc::new(crate::llm::HttpRound);
    // AwaitUserAnswer 事件现在只来自 handle_await_user_request 自身（oneshot 解冻后经
    // driver_tx 重投）——oneshot 由 user_answered 命令直 fire（不过通道,防主循环自锁）,
    // 这里无需查表路由,直接走 handle_event 落 history + 触发 turn。
    // 系统注入带图 user 消息：落 history 前先 emit 前端图卡（live 路径；history 路径由 buildHistoryBubbles 重建）
    if let SessionEvent::InjectAttachment { attachments, caption } = e {
        if let Some(a) = attachments.first() {
            let _ = TauriEmitter::emit(app, "injected-attachment", serde_json::json!({
                "staged_path": a.staged_path, "kind": a.kind, "caption": caption,
            }));
        }
    }
    // T5: JobDone 回调——emit job-callback 让前端 live 插折叠回调卡（带 job_id + 传给 agent 的原文 body）。
    // 与 chat-turn-start 分离：不动 Emitter trait 签名；前端 listen 本事件先插卡，再收 chat-turn-start 的 LLM 回复。
    if let SessionEvent::JobDone(o) = e {
        let body = jobdone_body(o);
        let kind_str = match o.kind {
            crate::jobs::JobKind::Process => "process",
            crate::jobs::JobKind::Agent => "agent",
        };
        let _ = TauriEmitter::emit(app, "job-callback", serde_json::json!({
            "job_id": o.job_id,
            "kind": kind_str,
            "body": body,
        }));
    }
    let resp = handle_event(e, history, history_dir.as_ref(), cache.as_ref(), ctx, fresh_cfg.as_ref(), round, &emit).await;
    // ask_user 等待：run_turn 返 ChatResponse.awaiting = Some(Handle)。driver 接管:
    // 用 Handle.request 经 handle_await_user_request 走"emit 前端 + 等答"流程。
    if let Some(handle) = resp.and_then(|r| r.awaiting) {
        // 活补发 tool_result 事件占位? 不——结果未知,先不补;等解冻后重投时再补(见下)。
        handle_await_user_request(&handle.request, &handle.call_id, pending_ask, ctx, app, driver_tx).await;
    }
    if matches!(e, SessionEvent::Reset) {
        // driver 专属：cancel 所有 agent jobs → 重置 registry → 通知前端清屏
        {
            let mut r = ctx.jobs.lock().unwrap();
            let tokens: Vec<tokio_util::sync::CancellationToken> = r.jobs.values()
                .filter(|j| matches!(j.kind, crate::jobs::JobKind::Agent))
                .filter_map(|j| j.cancel.clone()).collect();
            for t in tokens { t.cancel(); }
            *r = crate::jobs::JobRegistry::new_with_max(fresh_cfg.max_subagents as usize);
        }
        let _ = TauriEmitter::emit(app, "chat-reset", ());
    }
    // 轮间 cap 触发：UserMessage / JobDone 在 turn 后检查（dream 异步后台跑、不打断当前 turn）
    if matches!(e, SessionEvent::UserMessage { .. } | SessionEvent::JobDone(_)) {
        dispatch_dream(trigger, history_dir, cache, &fresh_cfg, false).await;
    }
}

/// ask_user Rust 端安全网宽限秒数:前端 timer（感知活动、可暂停）是权威计时,
/// Rust 只在前端无响应（webview 崩溃/关闭）时于 timeout+宽限后兜底。
const ASK_RUST_SAFETY_GRACE_SECS: u64 = 120;

/// ask_user 终态 → 文本（history tool_result 内容 + 前端活补发共用）。
/// 纯函数便于单测。前缀让 LLM 明确区分「用户亲答」与「兜底/拒绝」。
fn ask_user_result_text(
    answer: Option<&str>,
    skipped: bool,
    timed_out: bool,
    cancelled: bool,
    auto_submitted: bool,
    fallback_no_recommended: bool,
) -> String {
    match (answer, skipped, timed_out, cancelled) {
        // auto_submitted 必然伴随 timed_out=true（超时兜底才自动提交）——这里不能约束 timed_out=false
        (Some(a), false, _, false) if auto_submitted => format!("[用户超时未答,已自动提交推荐项] {a}"),
        (Some(a), false, false, false) => a.to_string(),
        (None, true, _, _) => "[用户主动跳过本次询问]".to_string(),
        (None, false, true, _) if fallback_no_recommended => "[用户超时未答且无推荐项,请自行决策]".to_string(),
        (None, false, true, _) => "[用户超时未答]".to_string(),
        (_, _, _, true) => "[询问被外部事件打断（全局中断）]".to_string(),
        (Some(a), false, true, false) => format!("[用户超时未答] {a}"),
        _ => "[用户未提供回答]".to_string(),
    }
}

/// ask_user 等待：driver 主循环内联处理（不走 run_one）。
/// 流程：
///   1. 校验单飞（pending_ask 非空 → 已有 ask_user 在等，直接报错给 tool_result）
///   2. 创建 oneshot，注册 question_id → sender
///   3. emit "await-user" 给前端（带完整请求负载）
///   4. tokio::select! 三臂等：oneshot recv / sleep(timeout) / driver 中断（ctx.interrupt）
///   5. 出结果：构造 AwaitUserAnswer 事件经 tx 重投 driver 主循环（让它走 handle_event 落 history + 触发 turn）
///
/// 注意：本函数是阻塞的（等用户答或超时）。driver 主循环一次只能等一个 ask_user，
/// 但其他事件（DreamCheck / JobDone 等）仍由外层 select! 监听——内层只是 ask_user 这次 answer 的 select。
async fn handle_await_user_request(
    req: &crate::ask_user::AskUserRequest,
    call_id: &str,
    pending_ask: &crate::ask_user::SharedPendingAsk,
    ctx: &ToolsCtx,
    app: &AppHandle,
    driver_tx: &tokio::sync::mpsc::Sender<SessionEvent>,
) {
    // 1. 单飞：已有 ask_user 在等 → 不允许嵌套。静默丢弃 + log 警告。
    if !pending_ask.lock().unwrap().is_empty() {
        eprintln!(
            "[ask_user] 嵌套拒绝:已有 question_id={:?} 在等待;新请求 question_id={} 被丢弃",
            pending_ask.lock().unwrap().keys().next(),
            req.question_id
        );
        return;
    }

    // 2. oneshot 入共享注册表（user_answered 命令经它直 fire,绕过阻塞中的主循环——防自锁）
    let (tx, rx) = tokio::sync::oneshot::channel::<crate::ask_user::AskUserResult>();
    pending_ask.lock().unwrap().insert(req.question_id.clone(), tx);

    // 3. emit "await-user" 给前端。payload 完整（前端按需渲染）。
    let payload = serde_json::json!({
        "question_id": req.question_id,
        "question": req.question,
        "context": req.context,
        "options": req.options,
        "allow_custom": req.allow_custom,
        "require_confirm": req.require_confirm,
        "timeout_secs": req.timeout_secs,
        "pause_on_activity": req.pause_on_activity,
        "resume_after_idle_secs": req.resume_after_idle_secs,
    });
    if let Err(e) = TauriEmitter::emit(app, "await-user", payload) {
        eprintln!("[ask_user] emit 失败: {e}");
        pending_ask.lock().unwrap().remove(&req.question_id);
        return;
    }

    // 4. select! 等：oneshot 答 / 超时 / 全局中断
    let recommended_label = crate::ask_user::pick_recommended_label(&req.options);

    // 拿当前 interrupt token 快照（如果中断被 cancel,select! 立即 ready）
    let interrupt_token = ctx.interrupt.current();

    let result = tokio::select! {
        biased;
        // 全局中断（interrupt_task 命令） → cancelled
        _ = async {
            if let Some(tok) = interrupt_token {
                tok.cancelled().await;
            } else {
                // 没 token（理论上不会发生，因为 ask_user 必在 run_turn 里跑）→ 永久 pending
                std::future::pending::<()>().await;
            }
        } => {
            crate::ask_user::AskUserResult {
                question_id: req.question_id.clone(),
                answer: None,
                skipped: false,
                timed_out: false,
                cancelled: true,
                auto_submitted: false,
                fallback_no_recommended: false,
            }
        }
        // 用户答 / 跳过（前端 → AwaitUserAnswer → run_one → oneshot send）
        r = rx => r.unwrap_or_else(|_| crate::ask_user::AskUserResult {
            question_id: req.question_id.clone(),
            answer: None,
            skipped: false,
            timed_out: true,
            cancelled: false,
            auto_submitted: false,
            fallback_no_recommended: recommended_label.is_none(),
        }),
        // 超时
        // Rust 端超时 = 安全网（前端无响应/崩溃时兜底）,不是权威计时。
        // 前端 timer 才是权威:它感知打字/悬停活动并暂停续倒,到期自己 invoke user_answered。
        // Rust 若按原 timeout_secs 计时,会在用户打字期间（前端暂停中）抢先触发,
        // 自动提交推荐项、丢弃用户未提交的输入——故加宽限期,把权威让给前端。
        _ = tokio::time::sleep(std::time::Duration::from_secs(
            req.timeout_secs + ASK_RUST_SAFETY_GRACE_SECS,
        )) => {
            match &recommended_label {
                Some(label) => crate::ask_user::AskUserResult {
                    question_id: req.question_id.clone(),
                    answer: Some(label.clone()),
                    skipped: false,
                    timed_out: true,
                    cancelled: false,
                    auto_submitted: true,
                    fallback_no_recommended: false,
                },
                None => crate::ask_user::AskUserResult {
                    question_id: req.question_id.clone(),
                    answer: None,
                    skipped: false,
                    timed_out: true,
                    cancelled: false,
                    auto_submitted: false,
                    fallback_no_recommended: true,
                },
            }
        }
    };

    // 5. 清表（答案已被消费;防重复 fire）
    pending_ask.lock().unwrap().remove(&req.question_id);

    // 6. 把结果转成 AwaitUserAnswer session event,经 driver_tx 重投回主循环
    //    + 活补发 llm-tool-result:与之前 run_turn emit 的 tool_call 卡配对,UI 即时显示结果
    let body = ask_user_result_text(
        result.answer.as_deref(), result.skipped, result.timed_out,
        result.cancelled, result.auto_submitted, result.fallback_no_recommended,
    );
    let _ = TauriEmitter::emit(app, "llm-tool-result", serde_json::json!({
        "name": "ask_user", "result": body,
    }));
    let answer_event = SessionEvent::AwaitUserAnswer {
        question_id: result.question_id.clone(),
        call_id: call_id.to_string(),
        answer: result.answer.clone(),
        skipped: result.skipped,
        timed_out: result.timed_out,
        cancelled: result.cancelled,
        auto_submitted: result.auto_submitted,
        fallback_no_recommended: result.fallback_no_recommended,
    };
    let _ = driver_tx.send(answer_event).await;
}

/// dream dispatch：决策（持锁，同步）→ 占位 prepare（持锁，set in_flight）
/// → spawn 执行 mem_dream::run_dream_to_idle（不持锁跨 await）
/// → 成功后 re-seed frontier from history + pack MEMORY.md + finish。
///
/// 接线 mem_dream（三层 jsonl + 活动段 + 索引卡），替旧 dream.rs execute_dream。
/// DreamTrigger 保留「何时触发」逻辑（idle/cap/in_flight/armed）；
/// mem_dream 负责「怎么提取」（marker in history = frontier）。
async fn dispatch_dream(
    trigger: &std::sync::Arc<std::sync::Mutex<crate::dream_trigger::DreamTrigger>>,
    history_dir: &std::sync::Arc<std::path::PathBuf>,
    cache: &std::sync::Arc<std::path::PathBuf>,
    cfg: &std::sync::Arc<Config>,
    force: bool,
) {
    // 1. 决策（持锁）：check 自带 armed/in_flight 门；force 走 check_forced（跳过阈值，仍守门）。
    let now = now_ms();
    let evs = crate::history::read_all(history_dir);
    let decision = {
        let t = trigger.lock().unwrap();
        if force {
            t.check_forced(&evs)
        } else {
            t.check(&evs, now, cfg.dream_idle_secs, cfg.dream_context_trigger_tokens)
        }
    };
    // 触发原因（仅日志；dream 行为不区分——都全量整理）
    let reason: &'static str = match decision {
        Some(crate::dream_trigger::TriggerReason::Idle) => "idle",
        Some(crate::dream_trigger::TriggerReason::ContextLen) => "token",
        None => return,
    };
    eprintln!("[dream] 触发: reason={reason}, 全量整理");
    // 2. 占位（持锁）：set in_flight。
    let prepared = {
        let mut t = trigger.lock().unwrap();
        crate::dream_trigger::prepare_dream(&mut t)
    };
    if prepared.is_none() { return; }

    // 3. spawn：mem_dream::run_dream_to_idle（内部读 events + 循环 + 写 marker）
    //    + pack（从三层 jsonl 重拼 MEMORY.md）+ re-seed trigger
    let trigger2 = trigger.clone();
    let cache2 = cache.as_ref().to_path_buf();
    let cfg2 = (**cfg).clone();
    tauri::async_runtime::spawn(async move {
        // 从 Config 构造 DreamCfg（dream 全量整理，不分触发原因）
        let dream_cfg = crate::mem_dream::DreamCfg {
            api_key: if cfg2.api_key.is_empty() { None } else { Some(cfg2.api_key.clone()) },
            model: cfg2.llm_model.clone(),
            region: cfg2.minimax_region.clone(),
            max_rounds: cfg2.dream_merge_max_rounds as u32,
            batch_max_events: cfg2.dream_batch_max_events as usize,
        };
        let flags = crate::mem_dream::DreamFlags::default();

        // dream day:读 history → 切活动段 → 分批 LLM → 写日层 jsonl + marker
        let r = crate::mem_dream::run_dream_to_idle(&cache2, &dream_cfg, flags).await;

        // month/year（原原本本 mem cli：extract_month 当月 + extract_year 当年，同函数）
        // P2 内部排当天/当月 → 只在非当天/非当月有新时调 LLM（day 整理当天不触发 month）
        if r.is_ok() {
            let now_ms = now_ms();
            let off = crate::history::local_offset_secs();
            let today = crate::history::date_from_ts_local(now_ms, off);
            let parts: Vec<&str> = today.split('-').collect();
            let year: i32 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(1970);
            let month: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
            let _ = crate::mem_dream::extract_month(&cache2, (year, month), &dream_cfg, flags).await;
            let _ = crate::mem_dream::extract_year(&cache2, year, &dream_cfg, flags).await;
        }

        {
            let mut t = trigger2.lock().unwrap();
            match &r {
                Ok(stats) => {
                    // re-seed frontier from history（marker 已由 run_dream_to_idle 写入 history）
                    let evs_after = crate::history::read_all(&cache2.join("history"));
                    t.seed_from_history(&evs_after);
                    eprintln!("[dream] 完成: {stats:?}");
                }
                Err(e) => eprintln!("[dream] 失败（不推进 marker，下次 idle/cap 重试）: {e}"),
            }
            t.dream_finished();
        }

        if r.is_ok() {
            // pack:从三层 jsonl 重拼 MEMORY.md（纯渲染,无 LLM）
            let pack_result = crate::mem_cli::pack(&cache2);
            eprintln!("[dream] {pack_result}");
            // idle 触发:写 reset marker → 下轮 build_messages 清空 context(只剩 MEMORY.md)
            // token 触发:不写 reset(dream marker 被 build_messages 忽略 → context 带 cap 条)
            if reason == "idle" {
                let off = crate::history::local_offset_secs();
                match crate::mem_dream::write_reset_marker(&cache2, off) {
                    Ok(seq) => eprintln!("[dream] idle: 写 reset marker seq={seq}, context 下轮清空"),
                    Err(e) => eprintln!("[dream] idle: reset marker 写入失败: {e}"),
                }
            }
            // TODO: Tauri emit("memory-updated") 通知前端刷新 pinned MEMORY.md
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LlmRound, RoundResult, FinishReason};
    use crate::jobs::{JobOutcome, SharedRegistry, JobRegistry, NoopJobUpdate};
    use std::sync::{Arc, Mutex};
    use async_trait::async_trait;

    #[test]
    fn ask_user_result_text_covers_terminal_states() {
        // 用户亲答:原文,无前缀
        assert_eq!(ask_user_result_text(Some("方案B"), false, false, false, false, false), "方案B");
        // 超时+推荐:标注自动提交
        assert!(ask_user_result_text(Some("方案B"), false, true, false, true, false).contains("自动提交推荐项"));
        // 超时+无推荐:交回 agent 自决
        assert!(ask_user_result_text(None, false, true, false, false, true).contains("请自行决策"));
        // 跳过 = 用户拒绝
        assert!(ask_user_result_text(None, true, false, false, false, false).contains("跳过"));
        // 全局中断
        assert!(ask_user_result_text(None, false, false, true, false, false).contains("打断"));
    }

    struct FakeEmitter { content: Mutex<String> }
    #[async_trait]
    impl llm::Emitter for FakeEmitter {
        async fn content(&self, t: &str) { self.content.lock().unwrap().push_str(t); }
        async fn tool_call(&self, _: &str, _: &str) {}
        async fn tool_result(&self, _: &str, _: &str) {}
        async fn turn_end(&self) {}
        async fn error(&self, _: &str) {}
    }

    fn ws_setup() -> (tempfile::TempDir, crate::history::HistoryWriterHandle, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let hd = dir.path().join("history");
        let h = crate::history::spawn_writer(hd.clone(), 0);
        (dir, h, hd)
    }
    fn ctx_with(ws: std::path::PathBuf, h: crate::history::HistoryWriterHandle) -> ToolsCtx {
        let (_tx, _rx) = tokio::sync::mpsc::channel(8);
        let (stx, _srx) = tokio::sync::mpsc::channel(8);
        ToolsCtx {
            workspace: ws.clone(), cache: ws, jobs: Arc::new(Mutex::new(JobRegistry::new())) as SharedRegistry,
            job_done_tx: _tx, job_update: Arc::new(NoopJobUpdate), minimax_region: "cn".into(),
            allow_background: true, subagent_stream: Arc::new(crate::subagents::NoopSubagentStream),
            history: h,
            job_writer: crate::jobs::JobWriterHandle::noop(),
            session_tx: stx,
            interrupt: crate::tools::InterruptHandle::new(),
            tasks: crate::tasks::DbActorHandle::noop(),
        }
    }
    fn cfg() -> Config {
        serde_json::from_value::<Config>(serde_json::json!({"system_prompt":"你是助手","dream_cap_turns":50})).unwrap()
    }

    #[tokio::test]
    async fn usermessage_appended_and_turn_runs() {
        let (dir, h, hd) = ws_setup();
        let round: Arc<dyn LlmRound> = Arc::new(StopRound);
        let emit = FakeEmitter { content: Mutex::new(String::new()) };
        let x = ctx_with(dir.path().to_path_buf(), h.clone());
        let c = cfg();
        let resp = handle_event(
            &SessionEvent::UserMessage { text: "你好".into(), attachments: vec![], source: "user".into() },
            &h, &hd, dir.path(), &x, &c, round, &emit).await;
        assert!(resp.is_some(), "UserMessage 应跑 turn");
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let evs = crate::history::read_all(&hd);
        assert!(evs.iter().any(|e| e.kind=="user" && e.data["text"]=="你好"));
        assert!(evs.iter().any(|e| e.kind=="assistant"), "turn 应落 assistant 事件");
    }

    #[tokio::test]
    async fn context_note_appends_external_no_turn() {
        let (dir, h, hd) = ws_setup();
        let round: Arc<dyn LlmRound> = Arc::new(StopRound);
        let emit = FakeEmitter { content: Mutex::new(String::new()) };
        let x = ctx_with(dir.path().to_path_buf(), h.clone());
        let c = cfg();
        let resp = handle_event(
            &SessionEvent::ContextNote { text: "[拖入] a.pdf".into() },
            &h, &hd, dir.path(), &x, &c, round, &emit).await;
        assert!(resp.is_none(), "ContextNote 不跑 turn");
        assert!(emit.content.lock().unwrap().is_empty(), "不应流正文");
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let evs = crate::history::read_all(&hd);
        assert!(evs.iter().any(|e| e.kind=="external"), "应落 external 事件");
    }

    #[tokio::test]
    async fn reset_appends_marker_no_turn() {
        let (dir, h, hd) = ws_setup();
        let round: Arc<dyn LlmRound> = Arc::new(StopRound);
        let emit = FakeEmitter { content: Mutex::new(String::new()) };
        let x = ctx_with(dir.path().to_path_buf(), h.clone());
        let c = cfg();
        let resp = handle_event(&SessionEvent::Reset, &h, &hd, dir.path(), &x, &c, round, &emit).await;
        assert!(resp.is_none(), "Reset 不跑 turn");
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let evs = crate::history::read_all(&hd);
        let m = evs.iter().find(|e| e.kind=="marker").unwrap();
        assert_eq!(m.data["marker"], serde_json::json!("reset"));
    }

    #[tokio::test]
    async fn jobdone_agent_appends_subagent_result_and_runs() {
        let (dir, h, hd) = ws_setup();
        let round: Arc<dyn LlmRound> = Arc::new(StopRound);
        let emit = FakeEmitter { content: Mutex::new(String::new()) };
        let x = ctx_with(dir.path().to_path_buf(), h.clone());
        let c = cfg();
        let o = JobOutcome { job_id: "3".into(), kind: crate::jobs::JobKind::Agent, label: None,
            ok: true, code: None, tail: String::new(), answer: Some("完成X".into()), note: None };
        let resp = handle_event(&SessionEvent::JobDone(o), &h, &hd, dir.path(), &x, &c, round, &emit).await;
        assert!(resp.is_some(), "JobDone(Agent) 应跑 turn");
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let evs = crate::history::read_all(&hd);
        let sr = evs.iter().find(|e| e.kind=="subagent_result").unwrap();
        assert_eq!(sr.data["agent_id"], serde_json::json!("3"));
        assert_eq!(sr.data["summary"], serde_json::json!("完成X"));
        assert_eq!(sr.data["ref"], serde_json::json!("thread=agent:3"));
    }

    #[tokio::test]
    async fn rebuild_starts_after_reset_marker() {
        // user1 + turn → reset marker → user2 + turn：第 2 轮重建应只含 user2（reset 清空）
        let (dir, h, hd) = ws_setup();
        let x = ctx_with(dir.path().to_path_buf(), h.clone());
        let c = cfg();
        let mk = |t: &str| SessionEvent::UserMessage { text: t.into(), attachments: vec![], source: "user".into() };
        // 第 1 轮
        let _ = handle_event(&mk("第一句"), &h, &hd, dir.path(), &x, &c,
            Arc::new(StopRound) as Arc<dyn LlmRound>, &FakeEmitter { content: Mutex::new(String::new()) }).await;
        // reset
        let _ = handle_event(&SessionEvent::Reset, &h, &hd, dir.path(), &x, &c,
            Arc::new(StopRound) as Arc<dyn LlmRound>, &FakeEmitter { content: Mutex::new(String::new()) }).await;
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        // 第 2 轮：手工模拟 handle_event 的重建段，验只含 user2
        let mut events = crate::history::read_all(&hd);
        let cur = crate::history::HistoryEvent::user(0, "main", "第二句", &[]);
        events.push(cur);
        let pinned = crate::context::PinnedBlocks { system_prompt: "S".into(), soul: None, agent: None, memory: None };
        let msgs = crate::context::build_messages(&events, &pinned, 50);
        let user_texts: Vec<&str> = msgs.iter()
            .filter(|m| m["role"]=="user").map(|m| m["content"].as_str().unwrap_or("")).collect();
        assert!(user_texts.iter().all(|t| t.contains("第二句") || !t.contains("第一句")),
            "reset 后第一句不应进 context：{user_texts:?}");
    }

    #[test]
    fn reset_cancels_agent_jobs_before_replacing_registry() {
        use crate::jobs::{JobKind, SharedRegistry, JobRegistry};
        let reg: SharedRegistry = std::sync::Arc::new(std::sync::Mutex::new(JobRegistry::new()));
        let tok = tokio_util::sync::CancellationToken::new();
        {
            let mut r = reg.lock().unwrap();
            let id = r.register(JobKind::Agent, "x".into(), std::path::PathBuf::new(), 0);
            r.jobs.get_mut(&id).unwrap().cancel = Some(tok.clone());
        }
        // 模拟 reset 的 cancel 遍历（与实现一致：先 cancel agent jobs，再替换）
        {
            let r = reg.lock().unwrap();
            for j in r.jobs.values() {
                if matches!(j.kind, JobKind::Agent) { if let Some(c) = &j.cancel { c.cancel(); } }
            }
        }
        assert!(tok.is_cancelled(), "reset 必须先 cancel agent jobs");
    }

    struct StopRound;
    #[async_trait]
    impl LlmRound for StopRound {
        async fn round(&self, _: &[serde_json::Value], _: &Config, _: &dyn llm::Emitter) -> Result<RoundResult, String> {
            Ok(RoundResult { content:"done".into(), reasoning:String::new(), tool_calls:vec![],
                finish:FinishReason::Stop, usage:None,
                assistant_message:serde_json::json!({"role":"assistant","content":"done"}) })
        }
    }

    // 修 regression：新 user 的本地 push seq 须 = current_seq()，否则 seq=0 被 marker 窗口排除
    struct AssertUserRound { needle: String }
    #[async_trait]
    impl LlmRound for AssertUserRound {
        async fn round(&self, messages: &[serde_json::Value], _: &Config, _: &dyn llm::Emitter) -> Result<RoundResult, String> {
            let found = messages.iter().any(|m| m["role"]=="user" && m["content"].as_str().map(|c| c.contains(&self.needle)).unwrap_or(false));
            assert!(found, "rebuild 应含新 user「{}」（marker 窗口不该排除它）: {messages:?}", self.needle);
            Ok(RoundResult { content:"done".into(), reasoning:String::new(), tool_calls:vec![],
                finish:FinishReason::Stop, usage:None,
                assistant_message:serde_json::json!({"role":"assistant","content":"done"}) })
        }
    }

    #[tokio::test]
    async fn handle_event_new_user_included_after_marker() {
        // 修：本地 push 的 current.seq 须 = current_seq()（> marker），否则 seq=0 被 marker 窗口排除 → rebuild 只剩 [system] → 400
        let (dir, h, hd) = ws_setup();
        let x = ctx_with(dir.path().to_path_buf(), h.clone());
        let c = cfg();
        let emit = FakeEmitter { content: std::sync::Mutex::new(String::new()) };
        // 第 1 轮 user（落 user + assistant）
        let _ = handle_event(&SessionEvent::UserMessage { text: "第一句".into(), attachments: vec![], source: "user".into() }, &h, &hd, dir.path(), &x, &c, Arc::new(StopRound) as Arc<dyn LlmRound>, &emit).await;
        // reset（写 marker，盖戳到 seq N）
        let _ = handle_event(&SessionEvent::Reset, &h, &hd, dir.path(), &x, &c, Arc::new(StopRound) as Arc<dyn LlmRound>, &emit).await;
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        // 第 2 轮：新 user —— AssertUserRound 验它进了 rebuild（marker 窗口没排除）
        let resp = handle_event(&SessionEvent::UserMessage { text: "第二句".into(), attachments: vec![], source: "user".into() }, &h, &hd, dir.path(), &x, &c, Arc::new(AssertUserRound { needle: "第二句".into() }) as Arc<dyn LlmRound>, &emit).await;
        assert!(resp.is_some(), "新 user 应触发 turn");
    }

    #[tokio::test]
    async fn injectattachment_appended_as_user_with_image_and_runs_turn() {
        let (dir, h, hd) = ws_setup();
        let round: Arc<dyn LlmRound> = Arc::new(StopRound);
        let emit = FakeEmitter { content: Mutex::new(String::new()) };
        let x = ctx_with(dir.path().to_path_buf(), h.clone());
        let c = cfg();
        let aref = crate::AttachmentRef { staged_path: "/tmp/fake.png".into(), kind: "image".into() };
        let resp = handle_event(
            &SessionEvent::InjectAttachment { attachments: vec![aref.clone()], caption: "草图".into() },
            &h, &hd, dir.path(), &x, &c, round, &emit).await;
        assert!(resp.is_some(), "InjectAttachment 应触发 turn");
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let evs = crate::history::read_all(&hd);
        // 落 kind=user 带 attachments（复用用户拖图事件类型）
        let u = evs.iter().find(|e| e.kind == "user").expect("应落 user 事件");
        let atts = u.data["attachments"].as_array().unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0]["kind"], "image");
        assert_eq!(atts[0]["staged_path"], "/tmp/fake.png");
        // 文本中性、含 caption、不冒充用户下指令
        let text = u.data["text"].as_str().unwrap();
        assert!(text.contains("系统"), "注入文本须中性标注系统: {text}");
        assert!(text.contains("草图"), "含 caption: {text}");
        assert!(!text.contains("请描述"), "不能冒充用户下指令: {text}");
    }

    #[test]
    fn inject_text_neutral_with_and_without_caption() {
        assert!(inject_text("").contains("系统"));
        assert!(!inject_text("").contains("请描述"));
        let with = inject_text("我的草图");
        assert!(with.contains("系统") && with.contains("我的草图"));
    }

    #[test]
    fn snapshot_prefix_on_user_when_enabled() {
        let cfg = crate::config::Config {
            user_prompt_prefix_enabled: true,
            user_prompt_prefix: "PFX".into(),
            ..crate::config::Config::default()
        };
        let mut ev = crate::history::HistoryEvent::user(1000, "main", "hi", &[]);
        assert!(maybe_snapshot_prefix(&mut ev, &cfg), "enabled+非空 user 事件应落 prefix");
        assert_eq!(ev.data.get("prefix").and_then(|v| v.as_str()), Some("PFX"));
    }

    #[test]
    fn no_snapshot_when_disabled() {
        let cfg = crate::config::Config::default(); // enabled=false
        let mut ev = crate::history::HistoryEvent::user(1000, "main", "hi", &[]);
        assert!(!maybe_snapshot_prefix(&mut ev, &cfg));
        assert!(ev.data.get("prefix").is_none(), "disabled 不落 prefix");
    }

    #[test]
    fn no_snapshot_when_prefix_empty() {
        let cfg = crate::config::Config {
            user_prompt_prefix_enabled: true,
            user_prompt_prefix: "   ".into(),
            ..crate::config::Config::default()
        };
        let mut ev = crate::history::HistoryEvent::user(1000, "main", "hi", &[]);
        assert!(!maybe_snapshot_prefix(&mut ev, &cfg), "prefix 空白不落");
        assert!(ev.data.get("prefix").is_none());
    }

    #[test]
    fn no_snapshot_on_marker() {
        let cfg = crate::config::Config {
            user_prompt_prefix_enabled: true,
            user_prompt_prefix: "PFX".into(),
            ..crate::config::Config::default()
        };
        let mut ev = crate::history::HistoryEvent::marker(1000, "reset", 5);
        assert!(!maybe_snapshot_prefix(&mut ev, &cfg), "marker 事件不落 prefix");
        assert!(ev.data.get("prefix").is_none());
    }

    #[test]
    fn snapshot_on_all_user_role_kinds() {
        let cfg = crate::config::Config {
            user_prompt_prefix_enabled: true,
            user_prompt_prefix: "PFX".into(),
            ..crate::config::Config::default()
        };
        let cases = [
            crate::history::HistoryEvent::user(1000, "main", "u", &[]),
            crate::history::HistoryEvent::subagent_result(1000, "1", "s", "r", true),
            crate::history::HistoryEvent::external(1000, "拖", "f", None),
        ];
        for mut ev in cases {
            let kind = ev.kind.clone();
            assert!(maybe_snapshot_prefix(&mut ev, &cfg), "kind={kind} 应落 prefix");
            assert_eq!(ev.data.get("prefix").and_then(|v| v.as_str()), Some("PFX"));
        }
    }
}
