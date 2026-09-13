// ovoice — MiniMax LLM + TTS, 百度 ASR 语音输入。
pub mod agent;
pub mod ask_user;
pub mod asr;
pub mod audio;
pub mod bash;
pub mod baidu;
pub mod config;
pub mod context;
pub mod dream_trigger;
pub mod extract;
pub mod history;
pub mod input;
pub mod jobs;
pub mod llm;
pub mod mem_cli;
pub mod mem_dream;
pub mod memory;
pub mod recorder;
pub mod scheduler;
pub mod shell;
pub mod subagents;
pub mod tasks;
pub mod tools;
pub mod tts;
pub mod voice_hotkey;

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};
use async_trait::async_trait;
use std::path::PathBuf;

use asr::{asr_one_shot, AsrState};

/// AppEmitter：把循环内事件通过 Tauri emit 推给前端。pub(crate) 供 agent.rs 构造。
pub(crate) struct AppEmitter {
    pub(crate) app: AppHandle,
}
#[async_trait]
impl llm::Emitter for AppEmitter {
    async fn thinking(&self, text: &str) { let _ = self.app.emit("llm-thinking", text); }
    async fn content(&self, text: &str) { let _ = self.app.emit("llm-content", text); }
    async fn tool_call(&self, name: &str, args: &str) { let _ = self.app.emit("llm-tool-call", json!({ "name": name, "args": args })); }
    async fn tool_result(&self, name: &str, result: &str) { let _ = self.app.emit("llm-tool-result", json!({ "name": name, "result": result })); }
    async fn turn_start(&self) { let _ = self.app.emit("chat-turn-start", json!({})); }
    async fn turn_end(&self) { let _ = self.app.emit("chat-turn-end", json!({})); }
    async fn error(&self, msg: &str) { let _ = self.app.emit("chat-error", json!({ "message": msg })); }
    async fn usage(&self, u: &Value) { let _ = self.app.emit("chat-usage", u); }
    async fn retry(&self, attempt: usize, reason: &str) {
        let _ = self.app.emit("chat-retry", json!({ "attempt": attempt, "reason": reason }));
    }
}

/// 把 Job 状态变化外推为 job-update 事件（Jobs 面板消费）。
struct AppJobUpdate(AppHandle);
impl jobs::JobUpdate for AppJobUpdate {
    fn update(&self, job: &jobs::Job) { let _ = self.0.emit("job-update", job.clone()); }
}

/// 子代理流式 delta → 前端 "subagent-stream" 事件（jobs 面板 agent 行展开区消费）。
struct AppSubagentStream(AppHandle);
impl subagents::SubagentStream for AppSubagentStream {
    fn delta(&self, job_id: &jobs::JobId, payload: serde_json::Value) {
        let mut p = payload;
        if let Some(obj) = p.as_object_mut() { obj.insert("id".into(), serde_json::json!(job_id)); }
        let _ = self.0.emit("subagent-stream", p);
    }
}

#[derive(serde::Serialize)]
struct SpeakResponse {
    audio_base64: String,
    format: String,
    size: usize,
}

/// 背景图查询结果：url 为 data URL（CSP=null 故可用）；未启用 / 路径空 / 读不到 → None。
/// opacity=背景明暗遮罩，glass=玻璃面板/气泡不透明度，均透传前端注入 CSS 变量。
#[derive(serde::Serialize)]
struct BgResponse {
    url: Option<String>,
    opacity: f64,
    glass: f64,
}

/// 与常驻 Session 对话：把文本事件投给 driver（driver 串行跑 turn）。send 失败(driver 死)返 Err（C2）。
#[tauri::command]
async fn chat(text: String, attachments: Vec<crate::AttachmentRef>, app: AppHandle) -> Result<(), String> {
    let tx = app.state::<agent::SessionHandle>().inner().tx.clone();
    tx.send(agent::SessionEvent::UserMessage { text, attachments, source: "user".into() }).await
        .map_err(|_| "session 已关闭".to_string())
}

/// 重置 Session：清 messages + JobRegistry（G4），重注入 system。前端气泡也清。
#[tauri::command]
fn reset_session(app: AppHandle) -> Result<(), String> {
    let tx = app.state::<agent::SessionHandle>().inner().tx.clone();
    let _ = tx.blocking_send(agent::SessionEvent::Reset);
    Ok(())
}

/// 静默上下文：往 session 历史追加一条 user 消息但**不触发** turn（右区 drop 用）。
/// driver 私有持有 messages，故命令只能发 ContextNote 事件（见 ARCH-2）。
#[tauri::command]
async fn append_context_note(text: String, app: AppHandle) -> Result<(), String> {
    let tx = app.state::<agent::SessionHandle>().inner().tx.clone();
    tx.send(agent::SessionEvent::ContextNote { text }).await
        .map_err(|_| "session 已关闭".to_string())
}

/// Jobs 面板快照。
#[tauri::command]
fn list_jobs(app: AppHandle) -> Vec<jobs::Job> {
    app.state::<jobs::SharedRegistry>().inner().lock().unwrap().list()
}

/// 终止后台 Job（人为终止不唤醒模型）。
#[tauri::command]
async fn kill_job(id: String, app: AppHandle) -> Result<(), String> {
    let registry = app.state::<jobs::SharedRegistry>().inner().clone();
    let upd: std::sync::Arc<dyn jobs::JobUpdate> = std::sync::Arc::new(AppJobUpdate(app.clone()));
    if jobs::kill_job(id, &registry, &upd) { Ok(()) } else { Err("任务不存在".into()) }
}

/// 读取 Job 完整日志（套 READ_MAX 截断，G4）。
#[tauri::command]
fn read_job_log(id: String, app: AppHandle) -> Result<String, String> {
    let cache = std::path::PathBuf::from(crate::config::load(&app).cache_dir);
    let bytes = std::fs::read(cache.join(".ovoice-jobs").join(format!("{id}.log")))
        .map_err(|e| e.to_string())?;
    Ok(tools::truncate(&tools::decode_output(&bytes).trim(), tools::READ_MAX, "\n…[已截断]"))
}

/// 定时任务：读 scheduler.json（全局开关 + 任务列表），前端面板渲染用。
#[tauri::command]
fn list_timers(app: AppHandle) -> crate::scheduler::SchedulerConfig {
    let cache = std::path::PathBuf::from(crate::config::load(&app).cache_dir);
    crate::scheduler::load_from(&cache)
}

/// 定时任务：新建或更新（task_id 空=新建后端生成；非空=更新并保留 last_fired 不重置计时）。
#[tauri::command]
fn upsert_timer(app: AppHandle, task: crate::scheduler::TimerTask) -> Result<String, String> {
    let cache = std::path::PathBuf::from(crate::config::load(&app).cache_dir);
    let mut cfg = crate::scheduler::load_from(&cache);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let id = if task.task_id.trim().is_empty() {
        crate::scheduler::gen_task_id()
    } else {
        task.task_id.clone()
    };
    if let Some(existing) = cfg.tasks.iter_mut().find(|t| t.task_id == id) {
        let mut updated = task.clone();
        updated.task_id = id.clone();
        updated.last_fired_ts = existing.last_fired_ts;       // 更新不重置 interval 计时
        updated.last_fired_date = existing.last_fired_date.clone();
        *existing = updated;
    } else {
        let mut new_task = task.clone();
        new_task.task_id = id.clone();
        new_task.last_fired_ts = now;                          // 新建从现在起算
        new_task.last_fired_date = String::new();
        cfg.tasks.push(new_task);
    }
    cfg.global_enabled = true; // 前端保存即开全局开关，免得建了不生效
    crate::scheduler::save_to(&cache, &cfg)?;
    if let Some(r) = app.try_state::<std::sync::Arc<crate::scheduler::SchedulerRunner>>() { r.inner().reload(); }
    Ok(id)
}

/// 定时任务：删除一个（只删配置，不删已发生的触发历史）。
#[tauri::command]
fn delete_timer(app: AppHandle, task_id: String) -> Result<(), String> {
    let cache = std::path::PathBuf::from(crate::config::load(&app).cache_dir);
    let mut cfg = crate::scheduler::load_from(&cache);
    cfg.tasks.retain(|t| t.task_id != task_id);
    crate::scheduler::save_to(&cache, &cfg)?;
    if let Some(r) = app.try_state::<std::sync::Arc<crate::scheduler::SchedulerRunner>>() { r.inner().reload(); }
    Ok(())
}

/// 定时任务：全局开关（一键暂停/恢复所有触发，不删配置/历史）。
#[tauri::command]
fn set_scheduler_global(app: AppHandle, enabled: bool) -> Result<(), String> {
    let cache = std::path::PathBuf::from(crate::config::load(&app).cache_dir);
    let mut cfg = crate::scheduler::load_from(&cache);
    cfg.global_enabled = enabled;
    crate::scheduler::save_to(&cache, &cfg)?;
    if let Some(r) = app.try_state::<std::sync::Arc<crate::scheduler::SchedulerRunner>>() { r.inner().reload(); }
    Ok(())
}

/// 用户确认强制退出：所有 running job 标 failed+forced_exit + 写 jsonl + 退出。
/// writer 从 app state 取（spawn_session T3 manage 的主 writer，保单写不变量——不另 spawn）。
#[tauri::command]
async fn force_quit(app: AppHandle) -> Result<(), String> {
    let registry = app.state::<jobs::SharedRegistry>().inner().clone();
    let writer = app.state::<jobs::JobWriterHandle>().inner().clone();
    let _n = jobs::mark_running_as_forced_exit(&registry, &writer);
    tokio::time::sleep(std::time::Duration::from_millis(150)).await; // 给主 writer flush forced_exit 事件
    app.exit(0);
    Ok(())
}

/// 中断当前正在跑的 turn(用户点「中断」)。并发执行——不经 driver 事件队列(否则排在 turn 后面=没用):
/// 直接 cancel ctx.interrupt 里的 token → run_turn 在工具间隙或 round 流式中途退出,写「用户中断」兜底。
/// 返回是否有 turn 真被 cancel(false = 没在跑 / 已结束)。
#[tauri::command]
async fn interrupt_task(app: AppHandle) -> Result<bool, String> {
    let cancelled = app
        .try_state::<crate::tools::InterruptHandle>()
        .map(|h| h.cancel())
        .unwrap_or(false);
    Ok(cancelled)
}

/// ask_user 回答：前端 modal 确认 / 跳过 / 超时 → **直接 fire oneshot**（不过 driver 通道）。
///
/// 关键：driver 主循环在 ask_user 等待期间被 handle_await_user_request 的 select! 阻塞,
/// 若本命令把 AwaitUserAnswer 事件投回通道,主循环无法消费 → 自锁直到超时。
/// 故这里从共享注册表取 sender 直接触发 oneshot;driver 解冻后自行转 AwaitUserAnswer
/// 事件重投通道走 history/turn 路径。
///
/// auto_submitted=true：前端倒计时归零时主动提交（用推荐项 label 兜底）。
/// fallback_no_recommended=true：超时且无推荐项。
#[tauri::command]
async fn user_answered(
    question_id: String,
    answer: Option<String>,
    skipped: bool,
    auto_submitted: bool,
    fallback_no_recommended: bool,
    app: AppHandle,
) -> Result<(), String> {
    let timed_out = auto_submitted || fallback_no_recommended;
    let (skipped_flag, answer_val) = if skipped {
        (true, None)
    } else if answer.as_deref().map(|s| !s.is_empty()).unwrap_or(false) {
        (false, answer)
    } else if auto_submitted || fallback_no_recommended {
        // 前端主动兜底：answer 即推荐项 label（auto_submitted=true），或空（fallback_no_recommended=true）
        (false, answer)
    } else {
        // 空答案 + 非跳过 + 非超时：保守走 skip
        (true, None)
    };
    let result = crate::ask_user::AskUserResult {
        question_id: question_id.clone(),
        answer: answer_val,
        skipped: skipped_flag,
        timed_out,
        cancelled: false,
        auto_submitted,
        fallback_no_recommended,
    };
    let registry = app.state::<crate::ask_user::SharedPendingAsk>().inner().clone();
    if crate::ask_user::fire_answer(&registry, &question_id, result) {
        Ok(())
    } else {
        Err(format!("问题 {question_id} 不在等待中（已超时/已答/重复提交）"))
    }
}

/// 把文本转成语音，返回 base64 音频（可在前端 `<audio>` 直接播放）。
#[tauri::command]
async fn speak(text: String, app: AppHandle) -> Result<SpeakResponse, String> {
    let cfg = config::load(&app);
    // 防御性：即便前端误传了含 <think> 的原文，也先剥掉，避免朗读推理内容。
    let text = llm::strip_think(&text);
    let audio = tts::synthesize(text, &cfg).await?;
    let audio_base64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        &audio.bytes,
    );
    Ok(SpeakResponse {
        audio_base64,
        format: audio.format,
        size: audio.bytes.len(),
    })
}

/// 读取配置（前端打开设置页时调用）。
#[tauri::command]
fn get_config(app: AppHandle) -> config::Config {
    config::load(&app)
}

/// 保存配置到磁盘。max_subagents 热应用到已活的 JobRegistry（不必重启即生效）。
#[tauri::command]
fn save_config(app: AppHandle, cfg: config::Config) -> Result<(), String> {
    {
        let reg = app.state::<jobs::SharedRegistry>().inner();
        reg.lock().unwrap().max_agents = cfg.max_subagents as usize;
    }
    config::save(&app, &cfg)
}

/// 用户编辑存盘（前端→磁盘）：scope 到 workspace+app_data；复用 write_file_scoped 纯逻辑。
#[tauri::command]
fn write_file(path: String, content: String, app: AppHandle) -> Result<String, String> {
    let cfg = config::load(&app);
    let ws = std::path::PathBuf::from(&cfg.workspace_dir);
    let cache = std::path::PathBuf::from(&cfg.cache_dir);
    let appdata = app.path().app_data_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let target = tools::resolve_path(&path, &ws);
    tools::write_file_scoped(&target, &content, &[ws, cache, appdata])
}

/// 把用户选/拖的文件复制进工作区 attachments（去重 + 尺寸校验 + kind + 文本提取）。
/// 逐个处理、逐个返回（一个超大不连累其它）。
#[tauri::command]
fn stage_attachments(paths: Vec<String>, app: AppHandle) -> Vec<StagedFile> {
    let cfg = config::load(&app);
    let ws = std::path::PathBuf::from(&cfg.workspace_dir);
    let cache = std::path::PathBuf::from(&cfg.cache_dir);
    let dir = cache.join("attachments"); // 附件副本归 cache（ovoice 内部产物）
    paths.iter().map(|p| {
        let src = tools::resolve_path(p, &ws); // 源文件按用户 workspace 解析
        tools::stage_one(&src, &dir, cfg.max_attachment_mb)
    }).collect()
}

/// 用系统默认程序打开文件/路径。
/// 不走 tauri-plugin-opener（其 path scope 在 Windows 上对 `\\?\` 规范化路径匹配失效，
/// 恒报 "Not allowed to open path"）。后端直接调系统命令：后端可信，且 agent 本就有 bash 权限。
#[tauri::command]
fn open_in_system(path: String) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        std::process::Command::new("cmd")
            .args(["/C", "start", "", path.as_str()])
            .creation_flags(CREATE_NO_WINDOW) // 不弹黑色 cmd 闪窗
            .spawn()
            .map_err(|e| format!("打开失败：{e}"))?;
        return Ok("已用系统默认程序打开".to_string());
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(&path).spawn()
            .map_err(|e| format!("打开失败：{e}"))?;
        return Ok("已用系统默认程序打开".to_string());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(&path).spawn()
            .map_err(|e| format!("打开失败：{e}"))?;
        return Ok("已用系统默认程序打开".to_string());
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    {
        let _ = path;
        Err("不支持的平台".to_string())
    }
}

/// 读取背景图：未启用 / 路径空 / 文件读不到 → url=None（前端回退纯主题底色）。
/// 启用且可读 → 内联 data URL（base64）。opacity 透传前端用于明暗遮罩。
#[tauri::command]
fn get_background(app: AppHandle) -> BgResponse {
    let cfg = config::load(&app);
    let opacity = cfg.bg_opacity;
    let glass = cfg.glass_opacity;
    if !cfg.bg_enabled {
        return BgResponse { url: None, opacity, glass };
    }
    let trimmed = cfg.bg_path.trim();
    if trimmed.is_empty() {
        return BgResponse { url: None, opacity, glass };
    }
    let path = std::path::PathBuf::from(trimmed);
    let url = match std::fs::read(&path) {
        Ok(bytes) => {
            let mime = mime_from_ext(&path);
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
            Some(format!("data:{mime};base64,{b64}"))
        }
        Err(_) => None, // 文件被删/移动/无权限 → 优雅回退
    };
    BgResponse { url, opacity, glass }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum MediaKind { Image, Video, Audio, Unsupported }

pub(crate) fn media_kind_from_ext(path: &std::path::Path) -> MediaKind {
    match path.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("png") | Some("jpg") | Some("jpeg") | Some("webp") | Some("gif") | Some("bmp") | Some("svg") | Some("ico") => MediaKind::Image,
        Some("mp4") | Some("webm") | Some("mov") | Some("mkv") | Some("m4v") => MediaKind::Video,
        Some("mp3") | Some("wav") | Some("flac") | Some("ogg") | Some("m4a") | Some("aac") | Some("opus") => MediaKind::Audio,
        _ => MediaKind::Unsupported,
    }
}

/// 文档类型（display/edit_card 用；与 MediaKind 分离，加法式不动 media 代码）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum DocKind { Html, Pdf, Docx, Csv, Markdown, Text, Unsupported }

/// 统一文件类型（media+doc 合并）：display 与 stage_attachments 共用，新增扩展名只改这里。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum FileKind { Image, Video, Audio, Html, Pdf, Docx, Csv, Markdown, Text, Unsupported }

/// 按扩展名判统一类型：先 media（图/视/音），未命中再 doc。
pub(crate) fn file_kind(path: &std::path::Path) -> FileKind {
    match media_kind_from_ext(path) {
        MediaKind::Image => FileKind::Image,
        MediaKind::Video => FileKind::Video,
        MediaKind::Audio => FileKind::Audio,
        MediaKind::Unsupported => match doc_kind_from_ext(path) {
            DocKind::Html => FileKind::Html,
            DocKind::Pdf => FileKind::Pdf,
            DocKind::Docx => FileKind::Docx,
            DocKind::Csv => FileKind::Csv,
            DocKind::Markdown => FileKind::Markdown,
            DocKind::Text => FileKind::Text,
            DocKind::Unsupported => FileKind::Unsupported,
        },
    }
}

/// FileKind → 前端/JSON 契约字符串（与 tool_display 既有 kind 字面量一致）。
pub(crate) fn file_kind_str(k: FileKind) -> &'static str {
    match k {
        FileKind::Image => "image", FileKind::Video => "video", FileKind::Audio => "audio",
        FileKind::Html => "html", FileKind::Pdf => "pdf", FileKind::Docx => "docx",
        FileKind::Csv => "csv", FileKind::Markdown => "markdown", FileKind::Text => "text",
        FileKind::Unsupported => "unsupported",
    }
}

/// 两层缓存策略：可编辑文本（md/代码）每次拉最新（no-store）；产物型（html/pdf/docx/csv/图/视/音）缓存 1h。
/// 产物型被重写后最多滞后 1h 自动刷新——兼顾性能与"别永久 stale"。
pub(crate) fn cache_control_for(path: &std::path::Path) -> &'static str {
    match file_kind(path) {
        FileKind::Markdown | FileKind::Text => "no-store",
        _ => "public, max-age=3600",
    }
}

/// stage 后的附件结果（返前端：预览条 / 本地渲染用）。
#[derive(serde::Serialize)]
pub struct StagedFile {
    pub ok: bool,
    pub staged_path: String,      // 绝对路径（attachments/<ts>_<名>）
    pub kind: String,             // file_kind_str
    pub original_name: String,
    pub size: u64,
    pub text: Option<String>,     // 文本类且 ≤50KB 才填
    pub error: Option<String>,    // ok=false 时填
}

/// 发送时随消息带的附件引用（前端 → chat 命令）。
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct AttachmentRef {
    pub staged_path: String,
    pub kind: String,
}

/// 按扩展名判文档类型。.doc 旧二进制 → Unsupported（客户端不支持）。
pub(crate) fn doc_kind_from_ext(path: &std::path::Path) -> DocKind {
    use DocKind::*;
    match path.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("html") | Some("htm") => Html,
        Some("pdf") => Pdf,
        Some("docx") => Docx,
        Some("csv") => Csv,
        Some("md") | Some("markdown") => Markdown,
        // 纯文本/配置/代码：皆归 Text，edit_card 可编辑（预览走 <pre> 原文，非 markdown）
        Some("txt") | Some("text") | Some("log")
        | Some("toml") | Some("yaml") | Some("yml") | Some("ini") | Some("cfg") | Some("conf")
        | Some("properties") | Some("env") | Some("json") | Some("xml")
        | Some("py") | Some("js") | Some("mjs") | Some("ts") | Some("tsx") | Some("jsx")
        | Some("rs") | Some("go") | Some("java") | Some("kt") | Some("c") | Some("h")
        | Some("cpp") | Some("hpp") | Some("cs") | Some("rb") | Some("php") | Some("swift")
        | Some("sh") | Some("bash") | Some("zsh") | Some("bat") | Some("cmd") | Some("ps1")
        | Some("css") | Some("scss") | Some("less") => Text,
        _ => Unsupported,
    }
}

/// 按扩展名猜 MIME（data URL 需要；未知扩展名按 application/octet-stream 兜底）。
pub(crate) fn mime_from_ext(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("bmp") => "image/bmp",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        Some("mkv") => "video/x-matroska",
        Some("m4v") => "video/x-m4v",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("flac") => "audio/flac",
        Some("ogg") => "audio/ogg",
        Some("m4a") => "audio/mp4",
        Some("aac") => "audio/aac",
        Some("opus") => "audio/ogg",
        Some("html") | Some("htm") => "text/html",
        Some("pdf") => "application/pdf",
        Some("csv") => "text/csv",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("md") | Some("markdown") => "text/markdown",
        Some("txt") | Some("text") | Some("log") => "text/plain",
        _ => "application/octet-stream",
    }
}

/// 路径是否落在任一 root 下（先 canonicalize 双方；文件需存在才能 canonicalize）。
pub(crate) fn is_within_roots(path: &std::path::Path, roots: &[std::path::PathBuf]) -> bool {
    let canon = match path.canonicalize() { Ok(p) => p, Err(_) => return false };
    roots.iter().any(|r| r.canonicalize().map(|rc| canon.starts_with(rc)).unwrap_or(false))
}

/// 解析 `bytes=start-end` / `bytes=start-` / `bytes=-suffix`；返 (start, end_inclusive)。
pub(crate) fn parse_range(header: &str, total: u64) -> Option<(u64, u64)> {
    if total == 0 { return None; } // 空文件无有效区间，避免 total-1 下溢成 u64::MAX
    let h = header.strip_prefix("bytes=")?.trim();
    let (s, e) = h.split_once('-')?;
    if s.is_empty() {
        // 后缀：最后 N 字节
        let n: u64 = e.trim().parse().ok()?;
        if n == 0 { return None; }
        let start = total.saturating_sub(n);
        return Some((start, total - 1));
    }
    let start: u64 = s.trim().parse().ok()?;
    if start >= total { return None; }
    let end = if e.trim().is_empty() { total - 1 } else {
        let v: u64 = e.trim().parse().ok()?;
        v.min(total - 1)
    };
    if end < start { return None; }
    Some((start, end))
}

/// 首次启动在 cache 写默认 SOUL/AGENT/MEMORY；已存在则不动（用户文件神圣）。
fn bootstrap_defaults(cache: &std::path::Path) {
    let soul = cache.join("SOUL.md");
    if !soul.exists() {
        let _ = std::fs::write(&soul, SOUL_MD_DEFAULT);
    }
    let agent = cache.join("AGENT.md");
    if !agent.exists() {
        let _ = std::fs::write(&agent, AGENT_MD_DEFAULT);
    }
    let _ = crate::memory::ensure_memory_skeleton(cache);
}

/// v2 workspace/cache 分离的一次性迁移：把旧版本堆在 workspace 根的 ovoice 内部产物
/// （SOUL/AGENT/MEMORY.md、memory/、history/、attachments/、.ovoice-jobs/）搬到 cache。
/// 逐项判：cache 已有则跳过（不覆盖），workspace 没有则跳过。同卷 rename、跨卷 copy+remove。
fn migrate_legacy_cache(workspace: &std::path::Path, cache: &std::path::Path) {
    let _ = std::fs::create_dir_all(cache);
    const NAMES: &[&str] = &["SOUL.md", "AGENT.md", "MEMORY.md", "memory", "history", "attachments", ".ovoice-jobs"];
    for name in NAMES {
        let src = workspace.join(name);
        let dst = cache.join(name);
        if dst.exists() || !src.exists() { continue; }
        if let Err(e) = move_path(&src, &dst) {
            eprintln!("[migrate] 迁移 {} 失败（可在 cache 手动放置）: {}", name, e);
        }
    }
}

/// 移动一个文件或目录：优先 rename（同卷原子）；跨卷 rename 失败 → copy 后删除源。
fn move_path(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            copy_recursive(src, dst)?;
            if src.is_dir() { std::fs::remove_dir_all(src)?; } else { std::fs::remove_file(src)?; }
            Ok(())
        }
    }
}

fn copy_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst).map(|_| ())
    }
}

const SOUL_MD_DEFAULT: &str = include_str!("../defaults/SOUL.md");
const AGENT_MD_DEFAULT: &str = include_str!("../defaults/AGENT.md");

/// display：before_seq 之前的 limit 条可见事件（向上翻）。
#[tauri::command]
fn history_tail(limit: u64, before_seq: Option<u64>, app: AppHandle) -> Vec<crate::history::HistoryEvent> {
    let cache = std::path::PathBuf::from(crate::config::load(&app).cache_dir);
    let evs = crate::history::read_all(&cache.join("history"));
    crate::history::visible_tail(&evs, limit, before_seq)
}

/// display：after_seq 之后的 limit 条可见事件（向下翻）。
#[tauri::command]
fn history_head(limit: u64, after_seq: Option<u64>, app: AppHandle) -> Vec<crate::history::HistoryEvent> {
    let cache = std::path::PathBuf::from(crate::config::load(&app).cache_dir);
    let evs = crate::history::read_all(&cache.join("history"));
    crate::history::visible_head(&evs, limit, after_seq)
}

// ── 任务管理用户侧 command（跟 agent task 工具共用同一 DbActor）──

#[tauri::command]
async fn list_tasks(
    db: tauri::State<'_, crate::tasks::DbActorHandle>,
    horizon: Option<String>,
    status: Option<String>,
    archived: Option<bool>,
) -> Result<Vec<crate::tasks::Task>, String> {
    let filter = crate::tasks::TaskFilter {
        horizon: horizon.as_deref().and_then(crate::tasks::Horizon::from_str),
        status: status.as_deref().and_then(crate::tasks::Status::from_str),
        archived: archived.unwrap_or(false),
        parent_id: None,
    };
    db.inner().list(filter).await
}

#[tauri::command]
async fn add_task(
    db: tauri::State<'_, crate::tasks::DbActorHandle>,
    input: serde_json::Value,
) -> Result<crate::tasks::Task, String> {
    let i = crate::tasks::TaskInput {
        title: input.get("title").and_then(|v| v.as_str()).ok_or("缺少 title")?.to_string(),
        goal: input.get("goal").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        detail: input.get("detail").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        horizon: input.get("horizon").and_then(|v| v.as_str())
            .and_then(crate::tasks::Horizon::from_str).unwrap_or(crate::tasks::Horizon::Current),
        parent_id: input.get("parent_id").and_then(|v| v.as_i64()),
        tags: input.get("tags").and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        status: input.get("status").and_then(|v| v.as_str())
            .and_then(crate::tasks::Status::from_str).unwrap_or(crate::tasks::Status::Todo),
        acceptance: input.get("acceptance").and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| {
                let text = x.get("text")?.as_str()?.to_string();
                Some(crate::tasks::CheckItem {
                    text,
                    done: x.get("done").and_then(|d| d.as_bool()).unwrap_or(false),
                    evidence: x.get("evidence").and_then(|e| e.as_str()).map(String::from),
                })
            }).collect())
            .unwrap_or_default(),
        due_date: input.get("due_date").and_then(|v| v.as_i64()),
    };
    db.inner().add(i, crate::tasks::Actor::User).await
}

#[tauri::command]
async fn update_task(
    db: tauri::State<'_, crate::tasks::DbActorHandle>,
    id: i64,
    patch: serde_json::Value,
) -> Result<crate::tasks::Task, String> {
    let mut p = crate::tasks::TaskPatch::default();
    if let Some(s) = patch.get("title").and_then(|v| v.as_str()) { p.title = Some(s.into()); }
    if let Some(s) = patch.get("goal").and_then(|v| v.as_str()) { p.goal = Some(s.into()); }
    if let Some(s) = patch.get("detail").and_then(|v| v.as_str()) { p.detail = Some(s.into()); }
    if let Some(h) = patch.get("horizon").and_then(|v| v.as_str()).and_then(crate::tasks::Horizon::from_str) { p.horizon = Some(h); }
    if let Some(s) = patch.get("status").and_then(|v| v.as_str()).and_then(crate::tasks::Status::from_str) { p.status = Some(s); }
    if let Some(n) = patch.get("sort_index").and_then(|v| v.as_i64()) { p.sort_index = Some(n as i32); }
    if let Some(d) = patch.get("due_date").and_then(|v| v.as_i64()) { p.due_date = Some(Some(d)); }
    if patch.get("clear_due").and_then(|v| v.as_bool()).unwrap_or(false) { p.due_date = Some(None); }
    if let Some(v) = patch.get("verified").and_then(|v| v.as_bool()) { p.verified = Some(v); }
    if let Some(arr) = patch.get("blockers").and_then(|v| v.as_array()) {
        p.blockers = Some(arr.iter().filter_map(|x| {
            Some(crate::tasks::Blocker {
                reason: x.get("reason")?.as_str()?.to_string(),
                raised_at: x.get("raised_at").and_then(|t| t.as_i64()).unwrap_or(0),
                resolved: x.get("resolved").and_then(|r| r.as_bool()).unwrap_or(false),
            })
        }).collect());
    }
    if let Some(arr) = patch.get("artifacts").and_then(|v| v.as_array()) {
        p.artifacts = Some(arr.iter().filter_map(|x| {
            let reference = x.get("reference")?.as_str()?.to_string();
            let kind = x.get("kind").and_then(|k| k.as_str()).unwrap_or("file");
            Some(crate::tasks::Artifact {
                kind: crate::tasks::ArtifactKind::from_str(kind),
                reference,
                note: x.get("note").and_then(|n| n.as_str()).map(String::from),
            })
        }).collect());
    }
    if let Some(arr) = patch.get("acceptance").and_then(|v| v.as_array()) {
        // 用户侧整组替换 acceptance（agent 用 task_check 逐项；用户编辑弹层整组写）
        let items: Vec<crate::tasks::CheckItem> = arr.iter().filter_map(|x| {
            let text = x.get("text")?.as_str()?.to_string();
            Some(crate::tasks::CheckItem {
                text,
                done: x.get("done").and_then(|d| d.as_bool()).unwrap_or(false),
                evidence: x.get("evidence").and_then(|e| e.as_str()).map(String::from),
            })
        }).collect();
        p.acceptance = Some(items);
    }
    db.inner().update(id, p).await
}

#[tauri::command]
async fn archive_task(
    db: tauri::State<'_, crate::tasks::DbActorHandle>,
    id: i64,
    archived: Option<bool>,
) -> Result<crate::tasks::Task, String> {
    db.inner().archive(id, archived.unwrap_or(true)).await
}

#[tauri::command]
async fn delete_task(
    db: tauri::State<'_, crate::tasks::DbActorHandle>,
    id: i64,
) -> Result<bool, String> {
    db.inner().delete(id).await
}

#[cfg(test)]
mod media_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn kind_image_video_audio_unsupported() {
        assert!(matches!(media_kind_from_ext(Path::new("a.png")), MediaKind::Image));
        assert!(matches!(media_kind_from_ext(Path::new("a.JPG")), MediaKind::Image));
        assert!(matches!(media_kind_from_ext(Path::new("a.mp4")), MediaKind::Video));
        assert!(matches!(media_kind_from_ext(Path::new("a.webm")), MediaKind::Video));
        assert!(matches!(media_kind_from_ext(Path::new("a.mp3")), MediaKind::Audio));
        assert!(matches!(media_kind_from_ext(Path::new("a.wav")), MediaKind::Audio));
        assert!(matches!(media_kind_from_ext(Path::new("a.txt")), MediaKind::Unsupported));
        assert!(matches!(media_kind_from_ext(Path::new("noext")), MediaKind::Unsupported));
    }

    #[test]
    fn cache_control_editable_vs_immutable() {
        // 可编辑文本：每次拉最新
        assert_eq!(cache_control_for(Path::new("a.md")), "no-store");
        assert_eq!(cache_control_for(Path::new("a.txt")), "no-store");
        assert_eq!(cache_control_for(Path::new("a.py")), "no-store");
        assert_eq!(cache_control_for(Path::new("a.json")), "no-store");
        // 产物型：缓存 1h
        assert_eq!(cache_control_for(Path::new("a.html")), "public, max-age=3600");
        assert_eq!(cache_control_for(Path::new("a.pdf")), "public, max-age=3600");
        assert_eq!(cache_control_for(Path::new("a.docx")), "public, max-age=3600");
        assert_eq!(cache_control_for(Path::new("a.csv")), "public, max-age=3600");
        assert_eq!(cache_control_for(Path::new("a.png")), "public, max-age=3600");
        assert_eq!(cache_control_for(Path::new("a.mp4")), "public, max-age=3600");
        assert_eq!(cache_control_for(Path::new("a.mp3")), "public, max-age=3600");
    }

    #[test]
    fn mime_covers_media() {
        assert_eq!(mime_from_ext(Path::new("a.mp4")), "video/mp4");
        assert_eq!(mime_from_ext(Path::new("a.webm")), "video/webm");
        assert_eq!(mime_from_ext(Path::new("a.mp3")), "audio/mpeg");
        assert_eq!(mime_from_ext(Path::new("a.wav")), "audio/wav");
        assert_eq!(mime_from_ext(Path::new("a.flac")), "audio/flac");
    }

    #[test]
    fn is_within_roots_allows_inside_blocks_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let inside = root.join("sub/a.mp4");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(&inside, b"x").unwrap();
        let inside = inside.canonicalize().unwrap();
        assert!(is_within_roots(&inside, &[root.clone()]));
        // 绝对路径但不在 root 下
        let other = tempfile::tempdir().unwrap();
        let outside = other.path().canonicalize().unwrap();
        assert!(!is_within_roots(&outside, &[root]));
    }

    #[test]
    fn parse_range_forms() {
        assert_eq!(parse_range("bytes=0-1023", 2000), Some((0, 1023)));
        assert_eq!(parse_range("bytes=500-", 2000), Some((500, 1999)));   // 开放右端→到 total-1
        assert_eq!(parse_range("bytes=-500", 2000), Some((1500, 1999)));  // 后缀：最后500字节
        assert_eq!(parse_range("bytes=0-", 2000), Some((0, 1999)));
        assert_eq!(parse_range("garbage", 2000), None);
        assert_eq!(parse_range("bytes=9999-", 2000), None); // 起点越界
        // 空文件 total=0：任何形式都 None（防 total-1 下溢）
        assert_eq!(parse_range("bytes=0-", 0), None);
        assert_eq!(parse_range("bytes=-500", 0), None);
    }

    #[test]
    fn doc_kind_covers_each_ext() {
        use DocKind::*;
        assert!(matches!(doc_kind_from_ext(Path::new("a.html")), Html));
        assert!(matches!(doc_kind_from_ext(Path::new("a.HTM")), Html));
        assert!(matches!(doc_kind_from_ext(Path::new("a.pdf")), Pdf));
        assert!(matches!(doc_kind_from_ext(Path::new("a.docx")), Docx));
        assert!(matches!(doc_kind_from_ext(Path::new("a.csv")), Csv));
        assert!(matches!(doc_kind_from_ext(Path::new("a.md")), Markdown));
        assert!(matches!(doc_kind_from_ext(Path::new("a.markdown")), Markdown));
        assert!(matches!(doc_kind_from_ext(Path::new("a.txt")), Text));
        assert!(matches!(doc_kind_from_ext(Path::new("a.log")), Text));
        // 配置/代码类也归 Text（edit_card 可编辑）
        assert!(matches!(doc_kind_from_ext(Path::new("a.toml")), Text));
        assert!(matches!(doc_kind_from_ext(Path::new("a.yml")), Text));
        assert!(matches!(doc_kind_from_ext(Path::new("a.yaml")), Text));
        assert!(matches!(doc_kind_from_ext(Path::new("a.ini")), Text));
        assert!(matches!(doc_kind_from_ext(Path::new("a.json")), Text));
        assert!(matches!(doc_kind_from_ext(Path::new("a.py")), Text));
    }
    #[test]
    fn doc_kind_doc_and_unknown_unsupported() {
        // .doc 旧二进制明确不支持（客户端无库）
        assert!(matches!(doc_kind_from_ext(Path::new("old.doc")), DocKind::Unsupported));
        assert!(matches!(doc_kind_from_ext(Path::new("x")), DocKind::Unsupported));
        assert!(matches!(doc_kind_from_ext(Path::new("a.exe")), DocKind::Unsupported));
    }
    #[test]
    fn file_kind_covers_all_categories() {
        use FileKind::*;
        assert_eq!(file_kind(Path::new("a.png")), Image);
        assert_eq!(file_kind(Path::new("b.mp4")), Video);
        assert_eq!(file_kind(Path::new("c.mp3")), Audio);
        assert_eq!(file_kind(Path::new("d.html")), Html);
        assert_eq!(file_kind(Path::new("e.pdf")), Pdf);
        assert_eq!(file_kind(Path::new("f.docx")), Docx);
        assert_eq!(file_kind(Path::new("g.csv")), Csv);
        assert_eq!(file_kind(Path::new("h.md")), Markdown);
        assert_eq!(file_kind(Path::new("i.py")), Text);
        assert_eq!(file_kind(Path::new("x.exe")), Unsupported);
    }
    #[test]
    fn mime_covers_doc_kinds() {
        // display_doc 的 html/pdf iframe 依赖正确 Content-Type（media:// 用 mime_from_ext）
        assert_eq!(mime_from_ext(Path::new("a.html")), "text/html");
        assert_eq!(mime_from_ext(Path::new("A.HTM")), "text/html");
        assert_eq!(mime_from_ext(Path::new("a.pdf")), "application/pdf");
        assert_eq!(mime_from_ext(Path::new("a.csv")), "text/csv");
        assert_eq!(mime_from_ext(Path::new("a.docx")), "application/vnd.openxmlformats-officedocument.wordprocessingml.document");
        assert_eq!(mime_from_ext(Path::new("a.md")), "text/markdown");
        assert_eq!(mime_from_ext(Path::new("a.txt")), "text/plain");
    }

    #[test]
    fn bootstrap_creates_defaults_if_absent() {
        let dir = tempfile::tempdir().unwrap();
        bootstrap_defaults(dir.path());
        assert!(dir.path().join("SOUL.md").exists());
        let agent = std::fs::read_to_string(dir.path().join("AGENT.md")).unwrap();
        assert!(agent.contains("mem"), "AGENT.md 应含 mem 用法: {agent}");
        let mem = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        assert!(mem.contains("## 日") && mem.contains("## 年"));
    }
    #[test]
    fn bootstrap_does_not_overwrite_existing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SOUL.md"), "我的灵魂").unwrap();
        bootstrap_defaults(dir.path());
        assert_eq!(std::fs::read_to_string(dir.path().join("SOUL.md")).unwrap(), "我的灵魂", "不覆盖用户文件");
    }
    #[test]
    fn migrate_legacy_cache_moves_internal_items() {
        // 模拟旧版本：workspace 根堆了 SOUL.md / memory/ / history/
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let cache = tmp.path().join("cache");
        std::fs::create_dir_all(ws.join("memory/2026/07")).unwrap();
        std::fs::create_dir_all(ws.join("history")).unwrap();
        std::fs::write(ws.join("SOUL.md"), "灵魂").unwrap();
        std::fs::write(ws.join("memory/2026/07/2026-07-26.md"), "evt").unwrap();
        std::fs::write(ws.join("history/2026-07-26.jsonl"), "{}").unwrap();
        // 用户的文件（report.md）必须留在 workspace，不被动
        std::fs::write(ws.join("report.md"), "我的报告").unwrap();

        migrate_legacy_cache(&ws, &cache);

        // 内部产物已搬到 cache
        assert_eq!(std::fs::read_to_string(cache.join("SOUL.md")).unwrap(), "灵魂");
        assert!(cache.join("memory/2026/07/2026-07-26.md").exists());
        assert!(cache.join("history/2026-07-26.jsonl").exists());
        // 源已移走（同卷 rename）
        assert!(!ws.join("SOUL.md").exists());
        assert!(!ws.join("memory").exists());
        assert!(!ws.join("history").exists());
        // 用户文件不动
        assert_eq!(std::fs::read_to_string(ws.join("report.md")).unwrap(), "我的报告");
    }
    #[test]
    fn migrate_legacy_cache_does_not_overwrite_existing_cache() {
        // cache 已有 SOUL.md（用户新装/重置过）→ workspace 里的旧版不覆盖
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let cache = tmp.path().join("cache");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("SOUL.md"), "新灵魂").unwrap();
        std::fs::write(ws.join("SOUL.md"), "旧灵魂").unwrap();

        migrate_legacy_cache(&ws, &cache);

        assert_eq!(std::fs::read_to_string(cache.join("SOUL.md")).unwrap(), "新灵魂", "cache 已有不覆盖");
        // workspace 的旧版保留（没搬走）
        assert!(ws.join("SOUL.md").exists(), "未迁移的源应保留");
    }
    #[test]
    fn move_path_falls_back_to_copy_across_volume() {
        // 跨卷 rename 会失败 → move_path 退化为 copy+remove（同卷模拟：rename 成功，这里只验回退路径存在）
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.txt");
        let dst = tmp.path().join("dst.txt");
        std::fs::write(&src, "data").unwrap();
        move_path(&src, &dst).unwrap();
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "data");
        assert!(!src.exists(), "源应被移除");
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 从 src-tauri/.env 加载 MINIMAX_API_KEY（配置页里的 key 优先；此为回落）。
    let _ = dotenvy::dotenv();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AsrState::default())
        .register_uri_scheme_protocol("media", |ctx, request| {
            use std::borrow::Cow;
            use tauri::http::Response;
            let app = ctx.app_handle();
            // CORS：只放行本应用自身 Origin（见下方 response 构造）。沙箱 iframe Origin=null → 不放行。
            let origin = request.headers().get("origin")
                .and_then(|v| v.to_str().ok())
                .filter(|o| !o.is_empty() && *o != "null")
                .map(|o| o.to_string());
            // 1. 取路径：URI path 去掉前导 '/'，percent-decode
            let raw = request.uri().path();
            let encoded = raw.trim_start_matches('/');
            let decoded = percent_encoding::percent_decode_str(encoded).decode_utf8_lossy();
            let path = PathBuf::from(decoded.as_ref());
            // 2. scope 校验：必须在 workspace / cache / app_data / 用户配置的 media_roots 下
            let cfg = crate::config::load(&app);
            let ws = PathBuf::from(&cfg.workspace_dir);
            let cache = PathBuf::from(&cfg.cache_dir);
            let appdata = app.path().app_data_dir().unwrap_or_else(|_| PathBuf::from("."));
            let mut roots = vec![ws, cache, appdata];
            for r in &cfg.media_roots { roots.push(PathBuf::from(r)); }
            let canon = match path.canonicalize() {
                Ok(p) => p,
                Err(_) => return Response::builder().status(404).body::<Cow<[u8]>>(b"not found".to_vec().into()).unwrap(),
            };
            if !is_within_roots(&canon, &roots) {
                return Response::builder().status(403).body::<Cow<[u8]>>(b"forbidden".to_vec().into()).unwrap();
            }
            // 3. 读文件大小 + mime
            let total = match std::fs::metadata(&canon).map(|m| m.len()) {
                Ok(n) => n,
                Err(_) => return Response::builder().status(404).body::<Cow<[u8]>>(b"not found".to_vec().into()).unwrap(),
            };
            let mime = mime_from_ext(&canon);
            // 4. Range 处理
            let range = request.headers().get("range")
                .and_then(|v| v.to_str().ok())
                .and_then(|h| parse_range(h, total));
            let (status, body, extra) = match range {
                Some((s, e)) => {
                    let len = (e - s + 1) as usize;
                    let mut f = match std::fs::File::open(&canon) {
                        Ok(f) => f, Err(_) => return Response::builder().status(404).body::<Cow<[u8]>>(vec![].into()).unwrap(),
                    };
                    use std::io::{Read, Seek, SeekFrom};
                    let _ = f.seek(SeekFrom::Start(s));
                    let mut buf = vec![0u8; len];
                    let read = f.read(&mut buf).unwrap_or(0);
                    buf.truncate(read);
                    (206, buf, format!("bytes {s}-{e}/{total}"))
                }
                None => {
                    let buf = std::fs::read(&canon).unwrap_or_default();
                    (200, buf, String::new())
                }
            };
            let clen = body.len().to_string();
            let mut b = Response::builder().status(status)
                .header("content-type", mime)
                .header("accept-ranges", "bytes")
                .header("content-length", clen)
                .header("cache-control", cache_control_for(&canon));
            // CORS：反射本应用 Origin（csv/docx 的 fetch 来自 tauri.localhost）。沙箱 HTML iframe
            // Origin=null 不放行 → 读不到文件（防 exfil，尤其 app_data/config.json 含 API 密钥）。
            if let Some(o) = &origin {
                b = b.header("access-control-allow-origin", o).header("vary", "Origin");
            }
            if status == 206 { b = b.header("content-range", extra); }
            b.body::<Cow<[u8]>>(body.into()).unwrap()
        })
        .setup(|app| {
            // 启动全局推说话：双击 Ctrl 按住录音、松开即识别。
            let handle = app.handle().clone();
            let asr = app.state::<AsrState>().inner().clone();
            voice_hotkey::spawn(handle, asr);

            // 常驻 Agent Session + 共享 JobRegistry（driver 与 list/kill 命令共享同一 Arc）
            let cfg0 = crate::config::load(app.handle());
            let ws0 = std::path::PathBuf::from(&cfg0.workspace_dir);
            let cache0 = std::path::PathBuf::from(&cfg0.cache_dir);
            let _ = std::fs::create_dir_all(&ws0);
            let _ = std::fs::create_dir_all(&cache0);
            // 一次性迁移：旧版本把内部产物堆在 workspace 根 → 搬到 cache（cache 已有则不覆盖）
            migrate_legacy_cache(&ws0, &cache0);
            bootstrap_defaults(&cache0);
            // 任务管理 DbActor：专用 OS 线程独占 Connection（cache_dir/tasks.db）
            // 必须在 spawn_session 之前 manage——spawn_session 内部 app.state::<DbActorHandle>() 取它
            let db_path = cache0.join("tasks.db");
            let db_actor = crate::tasks::spawn_db_actor(db_path, app.handle().clone());
            app.manage(db_actor);
            // ask_user 等待注册表:user_answered 命令与 driver 共用(命令直 fire oneshot,防主循环自锁)
            app.manage(crate::ask_user::new_registry());
            let registry: jobs::SharedRegistry =
                std::sync::Arc::new(std::sync::Mutex::new(jobs::JobRegistry::new_with_max(cfg0.max_subagents as usize)));
            let job_update: std::sync::Arc<dyn jobs::JobUpdate> =
                std::sync::Arc::new(AppJobUpdate(app.handle().clone()));
            let sub_stream: std::sync::Arc<dyn subagents::SubagentStream> =
                std::sync::Arc::new(AppSubagentStream(app.handle().clone()));
            let session = agent::spawn_session(app.handle().clone(), registry.clone(), job_update, sub_stream);
            app.manage(registry);
            app.manage(session);
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::DragDrop(drag) = event {
                match drag {
                    tauri::DragDropEvent::Enter { position, .. }
                    | tauri::DragDropEvent::Over { position } => {
                        let _ = window.emit("dnd-hover", json!({ "x": position.x, "y": position.y }));
                    }
                    tauri::DragDropEvent::Leave => {
                        let _ = window.emit("dnd-hover", Value::Null);
                    }
                    tauri::DragDropEvent::Drop { paths, position } => {
                        let ps: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
                        let _ = window.emit("dnd-drop", json!({ "paths": ps, "x": position.x, "y": position.y }));
                    }
                    _ => {}
                }
            }
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // 有 running job → 阻止本次关闭 + emit 前端确认；无 running → 不拦，正常关
                let running: Vec<serde_json::Value> = {
                    let r = window.app_handle().state::<jobs::SharedRegistry>().inner().lock().unwrap();
                    r.jobs.values()
                        .filter(|j| matches!(j.status, jobs::JobStatus::Running))
                        .map(|j| serde_json::json!({ "id": j.id, "label": j.label, "kind": match j.kind { jobs::JobKind::Process => "process", jobs::JobKind::Agent => "agent" } }))
                        .collect()
                };
                if !running.is_empty() {
                    api.prevent_close();
                    let _ = window.emit("confirm-close", serde_json::json!({ "jobs": running }));
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            chat,
            speak,
            get_config,
            save_config,
            write_file,
            stage_attachments,
            open_in_system,
            get_background,
            asr_one_shot,
            reset_session,
            append_context_note,
            list_jobs,
            kill_job,
            read_job_log,
            list_timers,
            upsert_timer,
            delete_timer,
            set_scheduler_global,
            force_quit,
            interrupt_task,
            user_answered,
            history_tail,
            history_head,
            list_tasks,
            add_task,
            update_task,
            archive_task,
            delete_task,
        ])
        .run(tauri::generate_context!())
        .expect("error while running ovoice application");
}
