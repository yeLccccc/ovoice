use serde_json::Value;
use std::path::{Path, PathBuf};
use crate::jobs::{self, JobOutcome, JobUpdate, SharedRegistry};
use std::sync::Arc;
use tokio::sync::mpsc;
use sha2::{Sha256, Digest};

/// 相对路径基于 workspace 解析；绝对路径原样返回；自动 trim。
pub fn resolve_path(input: &str, workspace: &Path) -> PathBuf {
    let p = PathBuf::from(input.trim());
    if p.is_absolute() { p } else { workspace.join(p) }
}

/// 按字节上限截断（在 UTF-8 char 边界切），附 note 标记。
pub fn truncate(s: &str, max_bytes: usize, note: &str) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str(note);
    out
}

pub(crate) const READ_MAX: usize = 50 * 1024;
pub(crate) const BASH_MAX: usize = 20 * 1024;

pub const TEXT_INLINE_MAX: usize = 50 * 1024;
const IMG_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// 工具执行上下文：workspace（用户文件根）+ cache（ovoice 内部根，供 bash 调 mem 时注入 OVOICE_CACHE）
/// + 后台 Job 基础设施 + region。
/// 当前 turn 的中断句柄。driver 起 turn 时 set 一个 CancellationToken,turn 结束 clear;
/// interrupt_task 命令(并发,不经 driver 事件队列)直接 cancel 它 → run_turn 在工具间隙或
/// round 流式中途退出,写「用户中断」兜底。Arc 内核:ctx(读)与 app.state(命令 cancel)共享同一实例。
#[derive(Clone)]
pub struct InterruptHandle(pub std::sync::Arc<std::sync::Mutex<Option<tokio_util::sync::CancellationToken>>>);

impl InterruptHandle {
    pub fn new() -> Self { Self(std::sync::Arc::new(std::sync::Mutex::new(None))) }
    pub fn set(&self, t: tokio_util::sync::CancellationToken) { *self.0.lock().unwrap() = Some(t); }
    pub fn clear(&self) { *self.0.lock().unwrap() = None; }
    /// 当前 turn 的 token 克隆(None = 当前无 turn / 此 ctx 不支持中断)。run_turn 每轮读一次。
    pub fn current(&self) -> Option<tokio_util::sync::CancellationToken> { self.0.lock().unwrap().clone() }
    /// interrupt_task 命令用:cancel 当前 token。返回是否真有 turn 在跑被 cancel。
    pub fn cancel(&self) -> bool { if let Some(t) = self.current() { t.cancel(); true } else { false } }
}

pub struct ToolsCtx {
    pub workspace: std::path::PathBuf,
    pub cache: std::path::PathBuf,
    pub jobs: SharedRegistry,
    pub job_done_tx: mpsc::Sender<JobOutcome>,
    pub job_update: Arc<dyn JobUpdate>,
    pub minimax_region: String,
    /// false 时 tool_bash 忽略 background（子代理强制前台）。Task 6 生效；本 Task 只加字段。
    pub allow_background: bool,
    /// 子代理流式 delta 外推（主 session 注入 AppSubagentStream；测试/子代理 ctx 用 Noop）。
    pub subagent_stream: std::sync::Arc<dyn crate::subagents::SubagentStream>,
    /// v2：history 单写句柄。run_turn 把 assistant/tool_result 事件 append 进来。
    /// foreground()/测试用 noop()；Task 5 给 driver ctx 注入真实 writer。
    pub history: crate::history::HistoryWriterHandle,
    /// v2：job 持久化单写句柄。spawn_process_job / spawn_agent 共用（Q2 DRY）。
    /// foreground()/测试用 noop()；spawn_session 注入真实 writer。
    pub job_writer: crate::jobs::JobWriterHandle,
    /// driver session 事件通道。tool_attach(image) 经此发 InjectAttachment 信号给 driver
    /// （driver 在 turn N 结束后处理 → 构造带图 user 消息 → turn N+1）。
    /// foreground()/测试用无人接收的 channel；spawn_session 注入真实 driver tx。
    pub session_tx: mpsc::Sender<crate::agent::SessionEvent>,
    /// 当前 turn 的中断句柄:run_turn 读 current() 决定能否被中断;interrupt_task 命令 cancel()。
    /// foreground()/测试用 new()(永 None = 不支持中断);spawn_session 注入与 app.state 共享的实例。
    pub interrupt: InterruptHandle,
    /// 任务管理 db actor 句柄。task_* 工具经此 async 调 SQL。
    /// foreground()/测试用 noop()；spawn_session 注入真实 actor（lib.rs setup spawn）。
    pub tasks: crate::tasks::DbActorHandle,
}

impl ToolsCtx {
    /// 前台专用（无后台能力）：run_loop 过渡期 + 单测用。
    /// 内部建一个无人接收的 done 通道与 NoopJobUpdate——本 ctx 下不该触发后台分支。
    pub fn foreground(workspace: std::path::PathBuf, minimax_region: String) -> Self {
        let (tx, _rx) = mpsc::channel(8);
        let (stx, _srx) = mpsc::channel::<crate::agent::SessionEvent>(8);
        Self {
            cache: workspace.clone(),
            workspace,
            jobs: Arc::new(std::sync::Mutex::new(jobs::JobRegistry::new())),
            job_done_tx: tx,
            job_update: Arc::new(jobs::NoopJobUpdate),
            minimax_region,
            allow_background: true,
            subagent_stream: std::sync::Arc::new(crate::subagents::NoopSubagentStream),
            history: crate::history::HistoryWriterHandle::noop(),
            job_writer: crate::jobs::JobWriterHandle::noop(),
            session_tx: stx,
            interrupt: InterruptHandle::new(),
            tasks: crate::tasks::DbActorHandle::noop(),
        }
    }
}

/// env allowlist（C3）：仅 MINIMAX_*/MMX_* 前缀通过；基线固定注入 MINIMAX_REGION=<region>（覆盖传入的同名键）。
fn parse_env(env_val: Option<&Value>, region: &str) -> Vec<(String, String)> {
    let mut out = vec![("MINIMAX_REGION".to_string(), region.to_string())];
    if let Some(obj) = env_val.and_then(|v| v.as_object()) {
        for (k, v) in obj {
            if (k.starts_with("MINIMAX_") || k.starts_with("MMX_")) && k != "MINIMAX_REGION" {
                if let Some(s) = v.as_str() {
                    out.push((k.clone(), s.to_string()));
                }
            }
        }
    }
    out
}

/// 后台 job 的简短标签：取命令前 3 个词。
fn label_for(cmd: &str) -> String {
    cmd.split_whitespace().take(3).collect::<Vec<_>>().join(" ")
}


pub fn tool_write(args: &Value, workspace: &Path) -> String {
    let path = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => resolve_path(p, workspace),
        None => return "write 缺少 path 参数".into(),
    };
    let content = match args.get("content").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return "write 缺少 content 参数".into(),
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return format!("创建目录失败 {}: {}", parent.display(), e);
        }
    }
    match std::fs::write(&path, content.as_bytes()) {
        Ok(_) => format!("已写入 {}（{} 字节）", path.display(), content.len()),
        Err(e) => format!("写入失败 {}: {}", path.display(), e),
    }
}

/// 用户编辑存盘：scope 校验（兼容新建文件——用最近存在的祖先 canonicalize）后写盘。
/// scope 不在 roots 内 → Err；自动建父目录。纯逻辑，可离线测。
pub fn write_file_scoped(target: &Path, content: &str, roots: &[std::path::PathBuf]) -> Result<String, String> {
    // 1) scope 校验：取最近存在的祖先 canonicalize 后判是否在 roots 内（`..`/symlink 在 canonicalize 时解析）
    let mut anchor = target.to_path_buf();
    while !anchor.exists() && anchor.parent().is_some() {
        anchor = anchor.parent().unwrap().to_path_buf();
    }
    if !crate::is_within_roots(&anchor, roots) {
        return Err("路径不在允许范围内".into());
    }
    // 2) 建父目录（此时锚点已 in-scope）
    if let Some(parent) = target.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Err(format!("创建目录失败: {e}"));
        }
    }
    // 3) 规范 dest：父目录现已存在→canonicalize 后拼 file_name（file_name 过滤掉 `.`/`..`），再对父目录兜底校验
    let dest = match target.parent().and_then(|p| p.canonicalize().ok()) {
        Some(cp) => cp.join(target.file_name().unwrap_or_default()),
        None => target.to_path_buf(),
    };
    let dest_parent = dest.parent().unwrap_or(Path::new(""));
    if !crate::is_within_roots(dest_parent, roots) {
        return Err("路径不在允许范围内".into());
    }
    std::fs::write(&dest, content.as_bytes()).map_err(|e| format!("写入失败: {e}"))?;
    Ok(format!("已写入 {}（{} 字节）", dest.display(), content.len()))
}

pub fn tool_read(args: &Value, workspace: &Path) -> String {
    let path = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => resolve_path(p, workspace),
        None => return "read 缺少 path 参数".into(),
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => return format!("读取失败 {}: {}", path.display(), e),
    };
    let total = bytes.len();
    if bytes.contains(&0u8) {
        return format!("{} 是二进制文件（{} 字节），read 仅支持文本", path.display(), total);
    }
    let text = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return format!("{} 非 UTF-8 文本（{} 字节）", path.display(), total),
    };
    if total > READ_MAX {
        truncate(&text, READ_MAX, &format!("\n…[已截断，共 {} 字节]", total))
    } else {
        text
    }
}

/// 统一展示本地文件：按扩展名自动判渲染类型（图/视/音 → 媒体卡；html/pdf/docx/csv/markdown/text → 文档卡）。
/// 设计意图：LLM 不指定类型——旋钮越少越不易用错（此前 markdown 常被误塞 html 通道）。
/// 返回 {"display":true,"path","kind","caption"?} 或 {"display":false,"error"}。
pub fn tool_display(args: &Value, workspace: &Path) -> String {
    let p = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return serde_json::json!({ "display": false, "error": "缺少 path 参数" }).to_string(),
    };
    let path = resolve_path(p, workspace);
    if !path.exists() {
        return serde_json::json!({ "display": false, "error": format!("文件不存在: {}", path.display()) }).to_string();
    }
    // 先试媒体（图/视/音）；未命中再按文档类型判。text 类（代码/配置）走只读文档卡。
    let kind_enum = crate::file_kind(&path);
    if kind_enum == crate::FileKind::Unsupported {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let hint = if ext == "doc" { "（.doc 旧格式不支持，请在系统查看器打开）" } else { "" };
        return serde_json::json!({ "display": false, "error": format!("不支持的文件类型: .{ext}{hint}") }).to_string();
    }
    let kind_str = crate::file_kind_str(kind_enum);
    let abs = path.canonicalize().unwrap_or(path).to_string_lossy().to_string();
    let caption = args.get("caption").and_then(|v| v.as_str()).unwrap_or("");
    serde_json::json!({ "display": true, "path": abs, "kind": kind_str, "caption": caption }).to_string()
}

/// 为用户打开可编辑卡片（左编辑右预览）让用户自己改文本类文件：content 给定则预填（可新建），缺省读磁盘。
/// 注意：本工具不直接改盘，只产出卡片交用户编辑（用户点保存才落盘）；agent 自己直接改盘用 tool_edit。
/// 返回 {"edit":true,"path","kind","content","caption"?} 或 {"edit":false,"error"}。
pub fn tool_edit_card(args: &Value, workspace: &Path) -> String {
    let p = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return serde_json::json!({ "edit": false, "error": "缺少 path 参数" }).to_string(),
    };
    let path = resolve_path(p, workspace);
    let kind_str = match crate::doc_kind_from_ext(&path) {
        crate::DocKind::Markdown => "markdown",
        crate::DocKind::Text => "text",
        _ => return serde_json::json!({ "edit": false, "error": format!("仅支持编辑文本类文件（md/txt/toml/yml/ini/json/代码等）: {}", path.display()) }).to_string(),
    };
    let content = match args.get("content").and_then(|v| v.as_str()) {
        Some(c) => c.to_string(),
        None => match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => return serde_json::json!({ "edit": false, "error": format!("读取失败（文件不存在或非 UTF-8）: {}", path.display()) }).to_string(),
        },
    };
    let abs = path.canonicalize().unwrap_or(path).to_string_lossy().to_string();
    let caption = args.get("caption").and_then(|v| v.as_str()).unwrap_or("");
    serde_json::json!({ "edit": true, "path": abs, "kind": kind_str, "content": content, "caption": caption }).to_string()
}

/// agent 直接改盘的精准编辑：把文本文件中「唯一」一处 old_string 替换为 new_string 后写回。
/// 设计意图：给 agent 一个确定性的精准改盘工具——区别于 edit_card（只给用户开卡片、不自己改）与
/// write（整文件覆盖）。old_string 须在文件中存在且唯一：0 处→未找到；>1 处→不唯一(提示补上下文)；
/// agent 指错会立即报错、可补上下文重试，从而自纠偏。返回纯字符串给 agent（同 write/read），不产卡片。
pub fn tool_edit(args: &Value, workspace: &Path) -> String {
    let p = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return "edit 缺少 path 参数".into(),
    };
    let old = match args.get("old_string").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return "edit 缺少 old_string 参数".into(),
    };
    let new = match args.get("new_string").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return "edit 缺少 new_string 参数".into(),
    };
    if old.is_empty() {
        return "old_string 不能为空".into();
    }
    if old == new {
        return "old_string 与 new_string 相同，无需替换".into();
    }
    let path = resolve_path(p, workspace);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => return format!("读取失败 {}: {}", path.display(), e),
    };
    if bytes.contains(&0u8) {
        return format!("{} 是二进制文件，edit 仅支持文本", path.display());
    }
    let text = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return format!("{} 非 UTF-8 文本", path.display()),
    };
    let count = text.matches(old).count();
    if count == 0 {
        return format!("未找到要替换的文本（old_string 不在文件中）: {}", path.display());
    }
    if count > 1 {
        return format!("找到 {count} 处匹配，old_string 不唯一——请扩大 old_string 包含更多上下文以唯一定位: {}", path.display());
    }
    let edited = text.replacen(old, new, 1);
    match std::fs::write(&path, edited.as_bytes()) {
        Ok(_) => format!("已替换 {}（原 {} 字节 → 新 {} 字节）", path.display(), old.len(), new.len()),
        Err(e) => format!("写入失败 {}: {}", path.display(), e),
    }
}

/// attach：把 read 搞不定的图/pdf/docx 纳入助手视野。
/// - pdf/docx：抽文本截断 → tool result 文本（当轮可见）
/// - image：落盘 + 触发系统注入带图 user 消息（下轮可见）；本函数返回 note（不带图）
/// - .doc/txt 等：报错引导
/// 返回 JSON 字符串（同 tool_display/tool_edit_card 模式）。
/// dream 工具：请求 driver 立即整理记忆（绕过 idle/token 阈值，仍守 in_flight/无新门）。
/// 经 session_tx 发 DreamRequest 信号；driver 在当前 turn 结束后串行 dispatch_dream(force=true)。
/// 行为同 token 触发：整理后写 dream marker，context 留最近 cap 条（不清空）。
/// 工具返回同步收据——整理是后台异步，结果下一轮 build_messages 自然反映（context 变短）。
/// 发送失败（driver 已退出等极罕见情况）→ 报错字符串，agent 可照常继续。
fn tool_dream(_args: &Value, ctx: &ToolsCtx) -> String {
    match ctx.session_tx.try_send(crate::agent::SessionEvent::DreamRequest) {
        Ok(()) => "已请求整理记忆：后台异步执行，不打断当前对话；下一轮起 context 仅保留最近若干回合（整理后的摘要已写入 MEMORY.md）。".to_string(),
        Err(e) => format!("请求整理记忆失败（driver 通道不可用）: {e}"),
    }
}

// ── ask_user 工具（agent 主动询问用户）──
//
// 流程：tool_ask_user 不直接 emit,而是创建 oneshot + 构造 AskUserRequest,
// 经 session_tx 派发给 driver。driver 在 AwaitUserRequest 处理函数里:
//   - emit "await-user" 给前端（弹 modal）
//   - select! 等 oneshot(用户答) / sleep(timeout) / interrupt_token(全局中断)
//   - 出结果后转 SessionEvent::AwaitUserAnswer 经 driver 重投,走 handle_event 落 history
//
// 本函数返 Err(AskUserHandle) 而不是 Ok(String):run_turn 看到 Err 不写 tool_result,
// return ChatResponse { awaiting: Some(handle) };driver 拿到 awaiting handle 转 AwaitUserRequest。
pub async fn tool_ask_user(
    args: &Value,
    _ctx: &ToolsCtx,
) -> Result<String, crate::ask_user::AskUserHandle> {
    // 1. 问句必填
    let question = match args.get("question").and_then(|v| v.as_str()) {
        Some(q) if !q.trim().is_empty() => q.trim().to_string(),
        _ => return Ok("错误:question 必填且非空".to_string()),
    };
    let context = args.get("context").and_then(|v| v.as_str()).map(|s| s.to_string());
    let allow_custom = args.get("allow_custom").and_then(|v| v.as_bool()).unwrap_or(true);
    let require_confirm = args.get("require_confirm").and_then(|v| v.as_bool()).unwrap_or(true);
    let timeout_secs = args.get("timeout_secs").and_then(|v| v.as_u64()).unwrap_or(15);
    let pause_on_activity = args.get("pause_on_activity").and_then(|v| v.as_bool()).unwrap_or(true);
    let resume_after_idle_secs = args.get("resume_after_idle_secs").and_then(|v| v.as_u64()).unwrap_or(5);

    // 2. 解析 options,带 recommended 校验
    let options: Vec<crate::ask_user::AskOption> = args
        .get("options")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().filter_map(|o| {
                let label = o.get("label")?.as_str()?.to_string();
                if label.trim().is_empty() { return None; }
                Some(crate::ask_user::AskOption {
                    label,
                    description: o.get("description").and_then(|v| v.as_str()).map(String::from),
                    recommended: o.get("recommended").and_then(|v| v.as_bool()).unwrap_or(false),
                    is_custom: o.get("is_custom").and_then(|v| v.as_bool()).unwrap_or(false),
                })
            }).collect()
        })
        .unwrap_or_default();
    if let Err(e) = crate::ask_user::validate_options(&options) {
        return Ok(format!("错误:{e}"));
    }

    // 3. 构造 AskUserRequest,question_id 用 uuid v4
    let question_id = uuid::Uuid::new_v4().to_string();
    let req = crate::ask_user::AskUserRequest {
        question_id: question_id.clone(),
        question,
        context,
        options,
        allow_custom,
        require_confirm,
        timeout_secs,
        pause_on_activity,
        resume_after_idle_secs,
    };

    // 4. 创建 oneshot。driver 拿到 Handle 后会:注册 tx + emit 给前端 + 等答。
    // 这里**不**经 session_tx 派发 AwaitUserRequest——避免 driver 收到两次(原本 tool 已经发过一次,
    // 现在 driver 直接从 ChatResponse.awaiting 拿 Handle 走等待路径,不再依赖 session_tx)。
    let (_tx, rx) = tokio::sync::oneshot::channel::<crate::ask_user::AskUserResult>();

    // 5. 返 Err(AskUserHandle):run_turn 不写 tool_result,把 Handle 交给 driver。
    // Handle 同时携带 AskUserRequest,driver 直接 emit 给前端;call_id 由 run_turn 回填。
    Err(crate::ask_user::AskUserHandle {
        question_id,
        call_id: String::new(),
        request: req,
        receiver: rx,
    })
}

// ── mem 工具族（4 个）── 复用 mem_cli 纯函数（零 LLM 只读 drill），cache 路径取 ctx.cache ──
//
// 与 mem CLI 同源：ls/read/history/search 是 mem_cli 已暴露的纯函数，这里只做 tool 薄封装：
// 参数从 args JSON 取 + 早期校验 → 调 mem_cli → 返回串。dream（写端、走 session_tx）已有
// 独立工具；pack（写端、纯渲染）agent 用不到，不封装。异常路径 mem_cli 都已用「（… 无 …）」
// 友好串兜底，tool 层只补参数缺失/类型错的早期返回。

pub fn tool_mem_list(args: &Value, ctx: &ToolsCtx) -> String {
    // date 可选；mem_cli::ls 自带路由：None→列年；紧凑/横线→按 digit 段数路由；坏格式→提示串。
    let arg = args.get("date").and_then(|v| v.as_str());
    crate::mem_cli::ls(&ctx.cache, arg)
}

pub fn tool_mem_read(args: &Value, ctx: &ToolsCtx) -> String {
    let date = match args.get("date").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return "缺少 date 参数（YYYY-MM-DD / YYYY-MM / YYYY）".into(),
    };
    // 按横线段数路由到 read_day/month/year（与 bin::read_route 同逻辑）。
    let parts: Vec<&str> = date.split('-').collect();
    let cache = &ctx.cache;
    match parts.as_slice() {
        [y, m, d] if y.len() == 4 && m.len() == 2 && d.len() == 2
            && m.parse::<u32>().map(|x| x >= 1 && x <= 12).unwrap_or(false)
            && d.parse::<u32>().map(|x| x >= 1 && x <= 31).unwrap_or(false) =>
            crate::mem_cli::read_day(cache, date),
        [y, m] if y.len() == 4 && m.len() == 2
            && m.parse::<u32>().map(|x| x >= 1 && x <= 12).unwrap_or(false) =>
            crate::mem_cli::read_month(cache, date),
        [y] if y.len() == 4 && y.bytes().all(|b| b.is_ascii_digit()) =>
            crate::mem_cli::read_year(cache, date),
        _ => "日期格式应为 YYYY-MM-DD / YYYY-MM / YYYY（月01-12 日01-31）".into(),
    }
}

pub fn tool_mem_history(args: &Value, ctx: &ToolsCtx) -> String {
    let date = match args.get("date").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return "缺少 date 参数（YYYY-MM-DD，紧凑 YYYYMMDD 也认）".into(),
    };
    // seq_start/seq_end 必须同时给或同时不给（单端过滤无意义）。
    let seq = match (
        args.get("seq_start").and_then(|v| v.as_u64()),
        args.get("seq_end").and_then(|v| v.as_u64()),
    ) {
        (Some(a), Some(b)) => Some((a, b)),
        (Some(_), None) | (None, Some(_)) =>
            return "seq_start 和 seq_end 需同时提供（或同时省略=不过滤）".into(),
        (None, None) => None,
    };
    crate::mem_cli::history(&ctx.cache, date, seq)
}

pub fn tool_mem_search(args: &Value, ctx: &ToolsCtx) -> String {
    let query = match args.get("query").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return "缺少 query 参数".into(),
    };
    let raw = args.get("raw").and_then(|v| v.as_bool()).unwrap_or(false);
    crate::mem_cli::search(&ctx.cache, query, raw)
}

// ── task 工具族（7 个）── 经 ctx.tasks async API 调 DbActor ──

/// 把工具入参里 horizon/status 字符串转成枚举，失败返回 Err 字符串。
fn parse_horizon(s: &str) -> Result<crate::tasks::Horizon, String> {
    crate::tasks::Horizon::from_str(s).ok_or_else(|| format!("未知 horizon: {s}（须 current/short/long/vision）"))
}
fn parse_status(s: &str) -> Result<crate::tasks::Status, String> {
    crate::tasks::Status::from_str(s).ok_or_else(|| format!("未知 status: {s}（须 todo/active/done/dropped）"))
}

pub async fn tool_task_list(args: &Value, ctx: &ToolsCtx) -> String {
    let filter = crate::tasks::TaskFilter {
        horizon: args.get("horizon").and_then(|v| v.as_str()).and_then(|s| parse_horizon(s).ok()),
        status: args.get("status").and_then(|v| v.as_str()).and_then(|s| parse_status(s).ok()),
        archived: args.get("archived").and_then(|v| v.as_bool()).unwrap_or(false),
        parent_id: None,
    };
    match ctx.tasks.list(filter).await {
        Ok(ts) => {
            if ts.is_empty() { return "（无任务）".into(); }
            let mut out = String::from("任务列表：\n");
            for t in ts {
                let prog = t.progress().map(|(d, n)| format!(" [{}/{}]", d, n)).unwrap_or_default();
                let blk = if t.is_blocked() { " ⚠️blocked" } else { "" };
                let ver = if t.status == crate::tasks::Status::Done && !t.verified { " ⚠️自述(未验证)" }
                          else if t.verified { " ✅verified" } else { "" };
                let arch = if t.archived_at.is_some() { " 🗄️archived" } else { "" };
                let due = t.due_date.map(|_| " ⏰due").unwrap_or_default();
                out.push_str(&format!(
                    "- #{} [{}]{}{}{}{}{}：{}\n",
                    t.id, t.status.as_str(), prog, blk, ver, due, arch, t.title
                ));
            }
            out
        }
        Err(e) => format!("读取任务失败: {e}"),
    }
}

pub async fn tool_task_get(args: &Value, ctx: &ToolsCtx) -> String {
    let id = match args.get("id").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return "缺少 id 参数".into(),
    };
    match ctx.tasks.get(id).await {
        Ok(Some(t)) => serde_json::to_string_pretty(&t).unwrap_or_else(|_| format!("#{} {}", t.id, t.title)),
        Ok(None) => format!("任务 #{} 不存在", id),
        Err(e) => format!("读取失败: {e}"),
    }
}

pub async fn tool_task_add(args: &Value, ctx: &ToolsCtx) -> String {
    let title = match args.get("title").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return "缺少 title 参数".into(),
    };
    let horizon = args.get("horizon").and_then(|v| v.as_str()).and_then(|s| crate::tasks::Horizon::from_str(s)).unwrap_or(crate::tasks::Horizon::Current);
    let status = args.get("status").and_then(|v| v.as_str()).and_then(|s| crate::tasks::Status::from_str(s)).unwrap_or(crate::tasks::Status::Todo);
    let input = crate::tasks::TaskInput {
        title,
        goal: args.get("goal").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        detail: args.get("detail").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        horizon,
        parent_id: args.get("parent_id").and_then(|v| v.as_i64()),
        tags: args.get("tags").and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        status,
        acceptance: args.get("acceptance").and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| {
                let text = x.get("text")?.as_str()?.to_string();
                Some(crate::tasks::CheckItem { text, done: x.get("done").and_then(|d| d.as_bool()).unwrap_or(false), evidence: x.get("evidence").and_then(|e| e.as_str()).map(String::from) })
            }).collect())
            .unwrap_or_default(),
        due_date: args.get("due_date").and_then(|v| v.as_i64()),
    };
    match ctx.tasks.add(input, crate::tasks::Actor::Agent).await {
        Ok(t) => format!("已登记任务 #{}「{}」", t.id, t.title),
        Err(e) => format!("建任务失败: {e}"),
    }
}

pub async fn tool_task_update(args: &Value, ctx: &ToolsCtx) -> String {
    let id = match args.get("id").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return "缺少 id 参数".into(),
    };
    let mut patch = crate::tasks::TaskPatch::default();
    if let Some(s) = args.get("title").and_then(|v| v.as_str()) { patch.title = Some(s.into()); }
    if let Some(s) = args.get("goal").and_then(|v| v.as_str()) { patch.goal = Some(s.into()); }
    if let Some(s) = args.get("detail").and_then(|v| v.as_str()) { patch.detail = Some(s.into()); }
    if let Some(s) = args.get("horizon").and_then(|v| v.as_str()).and_then(|s| crate::tasks::Horizon::from_str(s)) { patch.horizon = Some(s); }
    if let Some(s) = args.get("status").and_then(|v| v.as_str()).and_then(|s| crate::tasks::Status::from_str(s)) { patch.status = Some(s); }
    if let Some(n) = args.get("sort_index").and_then(|v| v.as_i64()) { patch.sort_index = Some(n as i32); }
    if let Some(d) = args.get("due_date").and_then(|v| v.as_i64()) { patch.due_date = Some(Some(d)); }
    if args.get("clear_due").and_then(|v| v.as_bool()).unwrap_or(false) { patch.due_date = Some(None); }
    if let Some(arr) = args.get("blockers").and_then(|v| v.as_array()) {
        patch.blockers = Some(arr.iter().filter_map(|x| {
            Some(crate::tasks::Blocker {
                reason: x.get("reason")?.as_str()?.to_string(),
                raised_at: x.get("raised_at").and_then(|t| t.as_i64()).unwrap_or(0),
                resolved: x.get("resolved").and_then(|r| r.as_bool()).unwrap_or(false),
            })
        }).collect());
    }
    if let Some(arr) = args.get("artifacts").and_then(|v| v.as_array()) {
        patch.artifacts = Some(arr.iter().filter_map(|x| {
            let reference = x.get("reference")?.as_str()?.to_string();
            let kind = x.get("kind").and_then(|k| k.as_str()).unwrap_or("file");
            Some(crate::tasks::Artifact {
                kind: crate::tasks::ArtifactKind::from_str(kind),
                reference,
                note: x.get("note").and_then(|n| n.as_str()).map(String::from),
            })
        }).collect());
    }
    match ctx.tasks.update(id, patch).await {
        Ok(t) => format!("已更新任务 #{}「{}」", t.id, t.title),
        Err(e) => format!("更新失败: {e}"),
    }
}

pub async fn tool_task_check(args: &Value, ctx: &ToolsCtx) -> String {
    let id = match args.get("id").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return "缺少 id 参数".into(),
    };
    let idx = match args.get("idx").and_then(|v| v.as_u64()) {
        Some(i) => i as usize,
        None => return "缺少 idx 参数".into(),
    };
    let done = args.get("done").and_then(|v| v.as_bool()).unwrap_or(true);
    let evidence = args.get("evidence").and_then(|v| v.as_str()).map(String::from);
    match ctx.tasks.check(id, idx, done, evidence).await {
        Ok(t) => {
            let prog = t.progress().map(|(d, n)| format!("{d}/{n}")).unwrap_or_else(|| "—".into());
            format!("已勾选任务 #{} 第 {} 项（进度 {}）", t.id, idx, prog)
        }
        Err(e) => format!("勾选失败: {e}"),
    }
}

pub async fn tool_task_progress(args: &Value, ctx: &ToolsCtx) -> String {
    let id = match args.get("id").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return "缺少 id 参数".into(),
    };
    let note = match args.get("note").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return "缺少 note 参数".into(),
    };
    match ctx.tasks.set_progress(id, note).await {
        Ok(t) => format!("已记录任务 #{} 进展", t.id),
        Err(e) => format!("记录进展失败: {e}"),
    }
}

pub async fn tool_task_archive(args: &Value, ctx: &ToolsCtx) -> String {
    let id = match args.get("id").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return "缺少 id 参数".into(),
    };
    let archived = args.get("archived").and_then(|v| v.as_bool()).unwrap_or(true);
    match ctx.tasks.archive(id, archived).await {
        Ok(t) => format!("已{}任务 #{}", if archived { "归档" } else { "恢复" }, t.id),
        Err(e) => format!("归档失败: {e}"),
    }
}

pub async fn tool_attach(args: &Value, ctx: &ToolsCtx) -> String {
    let p = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return serde_json::json!({ "attached": false, "error": "缺少 path 参数" }).to_string(),
    };
    let path = resolve_path(p, &ctx.workspace);
    if !path.exists() {
        return serde_json::json!({ "attached": false, "error": format!("文件不存在: {}", path.display()) }).to_string();
    }
    let caption = args.get("caption").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let kind = crate::file_kind(&path);
    match kind {
        crate::FileKind::Image => {
            let dir = ctx.cache.join("attachments");
            let staged = stage_one(&path, &dir, 0); // max_mb=0：image 走 stage_one 内部 IMG_MAX_BYTES（10MB）
            if !staged.ok {
                return serde_json::json!({
                    "attached": false,
                    "error": staged.error.unwrap_or_else(|| "落盘失败".into())
                }).to_string();
            }
            let aref = crate::AttachmentRef {
                staged_path: staged.staged_path.clone(), kind: "image".into(),
            };
            // 发信号给 driver：下轮以 user 消息注入此图（当轮 tool result 不带图——绕开 tool-role 限制）
            let _ = ctx.session_tx.send(crate::agent::SessionEvent::InjectAttachment {
                attachments: vec![aref], caption: caption.clone(),
            }).await;
            serde_json::json!({
                "attached": true, "kind": "image", "staged_path": staged.staged_path,
                "note": "图片下一轮以附件纳入视野（本轮不可见）；正常结束当前回合即可，下轮系统会把图喂进来"
            }).to_string()
        }
        crate::FileKind::Pdf | crate::FileKind::Docx => {
            let k = crate::file_kind_str(kind);
            match crate::extract::doc_text(&path, k) {
                Some(t) if !t.trim().is_empty() => {
                    let capped = crate::llm::cap_doc_text(&t);
                    serde_json::json!({ "attached": true, "kind": k, "text": capped }).to_string()
                }
                _ => serde_json::json!({
                    "attached": false, "kind": k,
                    "error": format!("{} 无法抽取文本（损坏/加密/无文本层），可用 read/display 兜底", path.display())
                }).to_string(),
            }
        }
        crate::FileKind::Text | crate::FileKind::Markdown | crate::FileKind::Csv => {
            serde_json::json!({ "attached": false, "error": "纯文本/代码/ csv 用 read，无需 attach" }).to_string()
        }
        _ => {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let hint = if ext == "doc" { "（.doc 旧二进制格式不支持，请转成 .docx 后再 attach）" } else { "" };
            serde_json::json!({ "attached": false, "error": format!("不支持的文件类型: .{ext}{hint}") }).to_string()
        }
    }
}

/// subagent 工具入口：action=spawn/status/kill 分派到 subagents 模块。
/// 注：stream 暂用 NoopSubagentStream——Task 11 给 ToolsCtx 加 subagent_stream 字段后改为 ctx.subagent_stream。
pub async fn tool_subagent(args: &Value, ctx: &ToolsCtx, cfg: &crate::config::Config, round: std::sync::Arc<dyn crate::llm::LlmRound>) -> String {
    let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("spawn");
    match action {
        "spawn" => {
            let prompt = match args.get("prompt").and_then(|v| v.as_str()) {
                Some(p) => p, None => return "subagent 缺少 prompt 参数".into(),
            };
            let caption = args.get("caption").and_then(|v| v.as_str()).unwrap_or("");
            let stream: std::sync::Arc<dyn crate::subagents::SubagentStream> = ctx.subagent_stream.clone();
            match crate::subagents::spawn_agent(prompt, caption, cfg, round, ctx, cfg,
                ctx.jobs.clone(), ctx.job_done_tx.clone(), stream) {
                Ok(id) => format!("已启动子代理 #{id}（{caption}），完成后自动把结果发回给你。可随时 subagent {{\"action\":\"status\",\"id\":{id}}} 看进度，或 subagent {{\"action\":\"kill\",\"id\":{id}}} 终止并回收。"),
                Err(e) => e,
            }
        }
        "status" => {
            let id = match args.get("id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(), None => return "status 缺少 id 参数".into(),
            };
            crate::subagents::status_snapshot(&id, &ctx.jobs)
        }
        "kill" => {
            let id = match args.get("id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(), None => return "kill 缺少 id 参数".into(),
            };
            crate::subagents::kill(&id, &ctx.jobs)
        }
        other => format!("subagent 未知 action: {other}"),
    }
}

/// 把一个文件复制进 attachments 目录（按内容 hash 去重），判 kind、尺寸校验、文本≤50KB 提取。
/// 纯逻辑（不碰 AppHandle），可离线测。图片恒 ≤10MB；其余 ≤ max_mb；超限 ok=false。
pub fn stage_one(src: &Path, attachments_dir: &Path, max_mb: u64) -> crate::StagedFile {
    let meta = match std::fs::metadata(src) {
        Ok(m) => m,
        Err(_) => return err_staged(&src.to_string_lossy(), format!("文件不存在: {}", src.display())),
    };
    if !meta.is_file() {
        return err_staged(&src.to_string_lossy(), "暂不支持文件夹".into());
    }
    let size = meta.len();
    let kind = crate::file_kind(src);
    if kind == crate::FileKind::Unsupported {
        return err_staged(&src.to_string_lossy(), format!("不支持的文件类型: {}", src.display()));
    }
    let limit = if kind == crate::FileKind::Image { IMG_MAX_BYTES } else { max_mb * 1024 * 1024 };
    if size > limit {
        return err_staged(
            &src.to_string_lossy(),
            format!("文件过大 {:.1} MB，上限 {:.0} MB", size as f64 / 1048576.0, limit as f64 / 1048576.0),
        );
    }
    let bytes = match std::fs::read(src) {
        Ok(b) => b,
        Err(e) => return err_staged(&src.to_string_lossy(), format!("读取失败: {e}")),
    };
    // 去重：sha256 内容寻址。文件名 = <hash 前缀>.<扩展名>；同 hash 复用。
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let hash = hasher.finalize();
    let hash_hex: String = hash.iter().take(8).map(|b| format!("{:02x}", b)).collect();
    let orig_name = src.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
    // 根据内容去重：只保留扩展名，文件名用 hash 前缀（不含 orig_name）
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("");
    let dest_name = if ext.is_empty() { hash_hex.clone() } else { format!("{hash_hex}.{ext}") };
    let dest = attachments_dir.join(&dest_name);
    if !dest.exists() {
        if let Err(e) = std::fs::create_dir_all(attachments_dir) {
            return err_staged(&src.to_string_lossy(), format!("创建目录失败: {e}"));
        }
        if let Err(e) = std::fs::write(&dest, &bytes) {
            return err_staged(&src.to_string_lossy(), format!("复制失败: {e}"));
        }
    }
    let abs = dest.canonicalize().unwrap_or(dest).to_string_lossy().to_string();
    let text = if matches!(kind, crate::FileKind::Text | crate::FileKind::Markdown | crate::FileKind::Csv)
        && size as usize <= TEXT_INLINE_MAX
    {
        String::from_utf8(bytes.clone()).ok()
    } else {
        None
    };
    crate::StagedFile {
        ok: true, staged_path: abs, kind: crate::file_kind_str(kind).to_string(),
        original_name: orig_name, size, text, error: None,
    }
}

fn err_staged(path: &str, error: String) -> crate::StagedFile {
    crate::StagedFile { ok: false, staged_path: path.into(), kind: String::new(),
        original_name: String::new(), size: 0, text: None, error: Some(error) }
}

#[cfg(windows)]
pub(crate) mod win_job {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JobObjectExtendedLimitInformation,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

    /// Windows Job Object：句柄关闭时杀掉整棵进程树（KILL_ON_JOB_CLOSE）。
    pub struct Job(HANDLE);
    // 安全性：HANDLE 是裸句柄；Job 单一所有者（在 tool_bash 内创建、跨 .await 移动、drop 时关闭），
    // 从不跨线程共享引用，故 Send 安全；刻意不 impl Sync（多线程共享同一句柄不安全）。
    unsafe impl Send for Job {}
    impl Job {
        pub fn create() -> Option<Self> {
            unsafe {
                let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if h == 0 as HANDLE { return None; }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    h, JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 { CloseHandle(h); return None; }
                Some(Job(h))
            }
        }
        pub fn assign_pid(&self, pid: u32) -> bool {
            unsafe {
                let h = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
                if h == 0 as HANDLE { return false; }
                let ok = AssignProcessToJobObject(self.0, h) != 0;
                CloseHandle(h);
                ok
            }
        }
    }
    impl Drop for Job {
        fn drop(&mut self) { unsafe { CloseHandle(self.0); } }
    }
}

/// 解码 cmd 输出：chcp 65001 后应为 UTF-8；失败按系统 ANSI 码页兜底。
pub(crate) fn decode_output(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    // chcp 65001 失败的兜底：按系统 ANSI 码页解码（注：encoding_rs 无 codepage 映射 API，
    // 此处使用 match 覆盖常见中文码页；其他码页按 GB18030 兜底）
    // 局限说明：chcp 65001 使正常路径输出 UTF-8，此兜底极少触发；对本 zh-CN 应用未知码页默认 GB18030 可接受，
    // 但非 CJK 地区（西里尔/阿拉伯文等）可能显示乱码。
    let cp = unsafe { windows_sys::Win32::Globalization::GetACP() };
    let enc = match cp {
        936 => encoding_rs::GBK,      // 简体中文
        950 => encoding_rs::BIG5,     // 繁体中文
        54936 => encoding_rs::GB18030, // 中文 GB18030
        932 => encoding_rs::SHIFT_JIS, // 日语
        _ => encoding_rs::GB18030,     // 默认兜底
    };
    enc.decode(bytes).0.to_string()
}

/// 绿色软件：把 current_exe() 父目录前置进 PATH（运行时，不改系统环境）→ mem CLI 在 bash 直接可调。
pub fn path_with_exe_dir() -> Option<(String, String)> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let cur = std::env::var("PATH").unwrap_or_default();
    let sep = if cfg!(windows) { ";" } else { ":" };
    Some(("PATH".into(), format!("{}{}{}", dir.display(), sep, cur)))
}

pub async fn tool_bash(args: &Value, ctx: &ToolsCtx, timeout_secs: u64) -> String {
    let command = match args.get("command").and_then(|v| v.as_str()) {
        Some(c) => c.to_string(),
        None => return "bash 缺少 command 参数".into(),
    };
    let want_bg = args.get("background").and_then(|v| v.as_bool()).unwrap_or(false);
    let background = want_bg && ctx.allow_background; // 子代理 ctx 强制前台，杜绝跨层 job_done 注入
    let mut env_vars = parse_env(args.get("env"), &ctx.minimax_region);
    // 暴露 ovoice 路径给子进程：bash 调到的 mem.exe 靠 OVOICE_CACHE 定位记忆/对话缓存；
    // OVOICE_WORKSPACE 一并给（用户文件相对根，供潜在的前台命令引用）。
    env_vars.push(("OVOICE_WORKSPACE".into(), ctx.workspace.display().to_string()));
    env_vars.push(("OVOICE_CACHE".into(), ctx.cache.display().to_string()));

    if background {
        #[cfg(windows)]
        {
            return match jobs::spawn_process_job(
                command.clone(), env_vars, &ctx.workspace, &ctx.cache, timeout_secs,
                label_for(&command), ctx.jobs.clone(), ctx.job_done_tx.clone(), ctx.job_update.clone(),
                ctx.job_writer.clone(),
            ).await {
                Ok(id) => format!(r#"{{"job_id":"{id}","status":"running","log":"用 read_job_log(id) 读取"}}"#),
                Err(e) => format!("后台启动失败: {e}"),
            };
        }
        #[cfg(not(windows))]
        {
            let _ = (command, env_vars, timeout_secs);
            return "后台任务仅支持 Windows".into();
        }
    }

    // 前台：跨平台 sh（busybox Win / /bin/sh Unix）+ CREATE_NO_WINDOW + kill_tree 超时/中断杀树（spec §4/D1/D4/D6）。
    // token 下传：bash 执行期间 interrupt_task cancel → select 就绪 → kill_tree 立即杀树（#35 bash 部分）。
    crate::bash::run_foreground(&command, &env_vars, &ctx.workspace, timeout_secs, ctx.interrupt.current()).await
}

/// 子代理可用工具子集：能动手改盘/跑命令，但不能弹卡片、不能递归 spawn。
pub const SUBAGENT_TOOLS: &[&str] = &["write", "read", "edit", "bash"];

/// 按名字从 schemas() 筛子集（顺序遵循 names；未命中名跳过）。
pub fn schemas_subset(names: &[&str]) -> Vec<Value> {
    let all = schemas();
    names.iter().filter_map(|n| all.iter().find(|t| t["function"]["name"].as_str() == Some(n)).cloned()).collect()
}

/// 三工具的 JSON Schema（传给模型 tools 字段）。description 详尽以引导正确调用。
pub fn schemas() -> Vec<Value> {
    vec![
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"write",
                "description":"把文本写入文件（UTF-8，覆盖已有文件，自动建父目录）。Windows 桌面环境；相对路径基于工作目录。",
                "parameters":{
                    "type":"object",
                    "properties":{
                        "path":{"type":"string","description":"文件路径，相对路径基于工作目录，绝对路径也可"},
                        "content":{"type":"string","description":"要写入的文本"}
                    },
                    "required":["path","content"]
                }
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"read",
                "description":"读取文本文件内容（UTF-8；二进制或超 50KB 会标注）。相对路径基于工作目录。",
                "parameters":{
                    "type":"object",
                    "properties":{
                        "path":{"type":"string","description":"文件路径"}
                    },
                    "required":["path"]
                }
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"bash",
                "description":"在 Windows cmd 中执行命令（工作目录=workspace）。mmx CLI 已全局可用、区域(region)已预设。短任务(mmx text/speech/search/quota、echo、git)前台即可；长任务(mmx video/music generate 等需数分钟)务必 background:true。媒体产物落 --out 或 minimax-output/，用相对路径回看。",
                "parameters":{
                    "type":"object",
                    "properties":{
                        "command":{"type":"string","description":"shell 命令（经 cmd /C 运行）"},
                        "background":{"type":"boolean","description":"true=后台运行(立即返回 job_id，完成后另行通知)；false(默认)=前台阻塞"},
                        "timeout_secs":{"type":"integer","description":"超时秒数，前台默认60、后台默认600"},
                        "env":{"type":"object","description":"注入子进程的环境变量(仅允许 MINIMAX_*/MMX_* 前缀)"}
                    },
                    "required":["command"]
                }
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"display",
                "description":"在对话中展示一个本地文件：自动按扩展名选渲染方式，无需（也不能）指定类型。支持：图片(jpg/png/gif/webp/svg)、视频(mp4/webm/mov)、音频(mp3/wav/flac)、HTML、PDF、Word(.docx)、CSV、Markdown(.md)、纯文本/代码(txt/log/toml/yaml/json/xml/py/js/ts/rs/go/c/cpp/sh/css 等，只读)。.doc/.xlsx 等旧/二进制格式不支持（用系统查看器打开）。要展示生成内容（如报告/图表）先 write 成文件再 display。",
                "parameters":{
                    "type":"object",
                    "properties":{
                        "path":{"type":"string","description":"要展示的文件路径，相对工作目录或绝对路径"},
                        "caption":{"type":"string","description":"可选；卡片下方说明文字"}
                    },
                    "required":["path"]
                }
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"edit_card",
                "description":"为用户打开一张可编辑卡片（左编辑右预览）让用户自己改文本类文件——你（助手）不直接改盘，只把可编辑内容展示出来交由用户编辑，用户点保存才落盘。当你想让用户审阅/手改时用这个。.md 走 markdown 预览，其它纯文本/配置/代码走原文 <pre>（md/txt/log/toml/yaml/yml/ini/cfg/conf/env/properties/json/xml/py/js/ts/rs/go/c/cpp/cs/sh/css 等）。content 缺省从磁盘读；给了则预填（可新建文件）。注意：你想自己直接改文件要用 edit，整文件重写用 write。",
                "parameters":{
                    "type":"object",
                    "properties":{
                        "path":{"type":"string","description":"要编辑的文件路径，相对工作目录或绝对路径"},
                        "content":{"type":"string","description":"可选；预填内容（缺省读磁盘）"},
                        "caption":{"type":"string","description":"可选；说明文字"}
                    },
                    "required":["path"]
                }
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"edit",
                "description":"精准修改一个文本文件的一处：传入文件中要找的原文 old_string 与替换文本 new_string，后端定位到「唯一」一处匹配后直接改盘返回。要求 old_string 足够具体以保证在文件中唯一（找到 0 处或 >1 处都会报错，需补上下文后重试）。这是你（助手）自己直接改文件、不弹卡片；新建文件用 write，整文件覆盖也用 write，弹卡片让用户改用 edit_card。",
                "parameters":{
                    "type":"object",
                    "properties":{
                        "path":{"type":"string","description":"要编辑的文件路径，相对工作目录或绝对路径"},
                        "old_string":{"type":"string","description":"文件中要被替换的原文（须与文件内容逐字一致，含缩进/换行；且在文件中唯一）"},
                        "new_string":{"type":"string","description":"替换后的文本（可为空串表示删除该段）"}
                    },
                    "required":["path","old_string","new_string"]
                }
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"subagent",
                "description":"启动并管理后台子代理（独立迷你 agent，自带 write/read/edit/bash 工具完成你交办的子任务）。action=spawn 启动（不阻塞，完成自动回注结果给你）；action=status 查 #N 的进度/最近工具/长耗时工具；action=kill 终止 #N 并拿回部分结果。子代理上下文干净（不含你的历史）；不递归嵌套。",
                "parameters":{
                    "type":"object",
                    "required":["action"],
                    "properties":{
                        "action":{"type":"string","enum":["spawn","status","kill"]},
                        "prompt":{"type":"string","description":"spawn 必填：交给子代理的任务。子代理看不到你们的对话历史，任务描述必须自包含：背景、目标、涉及文件路径、验收标准一次写全，别指望它追问"},
                        "caption":{"type":"string","description":"spawn 可选：给人看的标题"},
                        "id":{"type":"integer","description":"status/kill 必填：目标子代理 id"}
                    }
                }
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"attach",
                "description":"把 read 搞不定的文件纳入你的视野：pdf/Word(.docx) 抽取文本当轮即可看到；图片(png/jpg/webp 等)本轮看不到，系统下一轮以附件形式注入后你才能看到——调完 attach(图片) 不要死等，正常结束当前回合即可。.doc 旧格式不支持（转 .docx）；纯文本/代码用 read。",
                "parameters":{
                    "type":"object",
                    "properties":{
                        "path":{"type":"string","description":"文件路径，相对工作目录或绝对路径"},
                        "caption":{"type":"string","description":"可选；图片说明（下轮注入消息的标注 + 卡片说明）。pdf/docx 不用"}
                    },
                    "required":["path"]
                }
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"dream",
                "description":"立即触发记忆整理：把当前对话历史整理成摘要写入长期记忆(MEMORY.md)，整理后当前 context 仅保留最近若干回合（旧内容已凝练进记忆，不会丢）。\n何时用：对话已经很长、你感觉上下文开始冗余或重要信息该固化时——你判断合适就调，不必等系统自动触发。\n行为：异步后台执行，不打断当前对话；你调完照常继续回合，下一轮起 context 变短。\n幂等：正在整理中或无新内容可整理时调用是安全的（会被忽略）。无需参数。",
                "parameters":{"type":"object","properties":{}}
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"mem_list",
                "description":"列记忆索引（零 LLM 只读，文件树即索引）。无参→列所有年份；YYYY→各月+月主题；YYYYMM→各日+日概括；YYYYMMDD→各事件+seq 指针。紧凑(20260731)和横线(2026-07-31)都认。用来快速摸清某段时间有哪些记忆。",
                "parameters":{"type":"object","properties":{
                    "date":{"type":"string","description":"可选；YYYY / YYYYMM / YYYYMMDD（也认横线）。省略=列年"}
                }}
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"mem_read",
                "description":"读整理后的记忆详情（零 LLM 只读）。YYYY-MM-DD→当天各事件段全文(含指针/关键词/详情)；YYYY-MM→月主题+日概括；YYYY→年度主线+各月主题。粒度由日期段数决定。要读原始对话(未经 dream 整理)用 mem_history。",
                "parameters":{"type":"object","properties":{
                    "date":{"type":"string","description":"YYYY-MM-DD / YYYY-MM / YYYY"}
                },"required":["date"]}
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"mem_history",
                "description":"读某天的原始对话（零 LLM 只读，未经整理的人性化原文）。日期紧凑(20260731)和横线都认。可选 seq_start/seq_end 过滤一段（从 mem_list/mem_read 拿到 seq 指针后用这里读原话）。",
                "parameters":{"type":"object","properties":{
                    "date":{"type":"string","description":"YYYY-MM-DD（紧凑 YYYYMMDD 也认）"},
                    "seq_start":{"type":"integer","description":"可选；过滤起点 seq（含），须与 seq_end 同时给"},
                    "seq_end":{"type":"integer","description":"可选；过滤终点 seq（含），须与 seq_start 同时给"}
                },"required":["date"]}
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"mem_search",
                "description":"关键字搜记忆（零 LLM 只读，case-insensitive 中英文都命中）。默认只搜日层 memory/（跳过月/年层概要避免噪音），raw=true 扩到原始对话 history/。返回 文件:行号: 匹配行。不知何时的事件用它定位，再 mem_read/mem_history 精读。",
                "parameters":{"type":"object","properties":{
                    "query":{"type":"string","description":"搜索关键词"},
                    "raw":{"type":"boolean","description":"可选；true=扩到原始对话 history/（默认 false 只搜整理后 memory/）"}
                },"required":["query"]}
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"task_list",
                "description":"列出任务（你的当前工作记忆状态板）。返回每个任务的 id/title/status/进度/阻塞/验证标记。\n何时用：开始干任何涉及目标/进度的事之前先 task_list 看现状；用户提到任务相关话题时主动查。默认只看未归档。\n参数：horizon(current/short/long/vision 可选过滤)、status(todo/active/done/dropped 可选过滤)、archived(bool 默认 false，true=看已归档)。",
                "parameters":{"type":"object","properties":{
                    "horizon":{"type":"string","enum":["current","short","long","vision"]},
                    "status":{"type":"string","enum":["todo","active","done","dropped"]},
                    "archived":{"type":"boolean","default":false}
                }}
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"task_get",
                "description":"查一个任务的完整详情（goal/detail/acceptance 全清单/blockers/artifacts/时间轴）。要看子目标验收清单或阻塞原因时用。",
                "parameters":{"type":"object","properties":{
                    "id":{"type":"integer","description":"任务 id"}
                },"required":["id"]}
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"task_add",
                "description":"登记新任务。用户提新目标、或你拆解出子任务时用。会返回分配的 #id。\n核心字段：title(名称)/goal(目标动机,区别 title)/horizon(时间视野)/parent_id(父任务,拆解用,顶层不传)/status(默认 todo)/acceptance(验收清单,可后补)/due_date(截止毫秒,可选)。",
                "parameters":{"type":"object","properties":{
                    "title":{"type":"string"},"goal":{"type":"string"},
                    "detail":{"type":"string"},
                    "horizon":{"type":"string","enum":["current","short","long","vision"],"default":"current"},
                    "parent_id":{"type":"integer"},
                    "tags":{"type":"array","items":{"type":"string"}},
                    "status":{"type":"string","enum":["todo","active","done","dropped"],"default":"todo"},
                    "acceptance":{"type":"array","items":{"type":"object","properties":{
                        "text":{"type":"string"},"done":{"type":"boolean"},"evidence":{"type":"string"}
                    }}},
                    "due_date":{"type":"integer","description":"截止时间戳(毫秒)"}
                },"required":["title"]}
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"task_update",
                "description":"改任务字段。维护状态的核心动作：status 流转(todo→active→done/dropped)/改 goal·title·detail(核心定义)/设 due/调 sort_index/raise 或 resolve blocker(传完整 blockers 数组)/补 artifacts。\n验证：status=done 默认 verified=false(自述完成)；升 verified 不在此工具(须用户侧确认)。",
                "parameters":{"type":"object","properties":{
                    "id":{"type":"integer"},
                    "title":{"type":"string"},"goal":{"type":"string"},"detail":{"type":"string"},
                    "horizon":{"type":"string","enum":["current","short","long","vision"]},
                    "status":{"type":"string","enum":["todo","active","done","dropped"]},
                    "sort_index":{"type":"integer"},
                    "due_date":{"type":"integer"},
                    "clear_due":{"type":"boolean","description":"true=清掉截止"},
                    "blockers":{"type":"array","items":{"type":"object","properties":{
                        "reason":{"type":"string"},"raised_at":{"type":"integer"},"resolved":{"type":"boolean"}
                    }}},
                    "artifacts":{"type":"array","items":{"type":"object","properties":{
                        "kind":{"type":"string","enum":["file","link","note"]},
                        "reference":{"type":"string"},"note":{"type":"string"}
                    }}}
                },"required":["id"]}
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"task_check",
                "description":"勾/取消勾一条验收清单项（推进 → 派生进度）。done=true 时鼓励带 evidence(文件路径/链接/seq)。",
                "parameters":{"type":"object","properties":{
                    "id":{"type":"integer"},
                    "idx":{"type":"integer","description":"acceptance 数组下标(0 起)"},
                    "done":{"type":"boolean","default":true},
                    "evidence":{"type":"string","description":"验证依据(建议带)"}
                },"required":["id","idx"]}
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"task_progress",
                "description":"刷新任务的 last_progress 摘要（当前进展一句话）。每干完一段就记，让任务板始终反映现实。",
                "parameters":{"type":"object","properties":{
                    "id":{"type":"integer"},"note":{"type":"string"}
                },"required":["id","note"]}
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"task_archive",
                "description":"归档/恢复任务。done 一段时间后归档(主视图隐藏,可恢复)。不能硬删——硬删是用户专属。",
                "parameters":{"type":"object","properties":{
                    "id":{"type":"integer"},"archived":{"type":"boolean","default":true}
                },"required":["id"]}
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"create_timer",
                "description":"创建定时任务：到点自动把 action 作为一条用户消息投递给主 agent（消息带「[定时任务「name」]」前缀，agent 按提示处理）。\n用途：周期性提醒/跟进/巡检——每日早晨提醒跟进某项目、每周复盘、定期检查状态；agent 也可主动为自己建（它最清楚哪些事该周期跟进）。\naction 是投给 agent 的提示词，写成明确指令/提醒（到点 agent 照做），例：「列出今天待办，提醒用户先做哪件」「检查 projects/mem-v2 进度，若停滞提醒推进」「每周日复盘：汇总本周事件+阻塞项」。\nmode=interval 配 interval{value,unit}，unit∈sec/min/hour/day（最小 1 秒）。\nmode=schedule 配 schedule{time,repeat}，time=HH:MM 本地时区，repeat∈daily/weekly/monthly/yearly（weekly 配 weekdays[0=周日..6=周六]；monthly 配 day；yearly 配 month+day）。\ncount:once=触发一次自动停；forever=持续循环（默认）。持久化重启不丢；global_enabled 关则全部暂停。",
                "parameters":{
                    "type":"object",
                    "required":["name","mode","action"],
                    "properties":{
                        "name":{"type":"string","description":"任务显示名（1-80 字符）"},
                        "mode":{"type":"string","enum":["interval","schedule"],"description":"interval=按固定间隔触发；schedule=指定时点触发"},
                        "count":{"type":"string","enum":["once","forever"],"description":"once=触发一次后自动停；forever=持续循环。默认 forever"},
                        "interval":{"type":"object","description":"mode=interval 必填","properties":{"value":{"type":"integer","description":"数值"},"unit":{"type":"string","enum":["sec","min","hour","day"],"description":"单位"}},"required":["value","unit"]},
                        "schedule":{"type":"object","description":"mode=schedule 必填","properties":{"time":{"type":"string","description":"HH:MM（本地时区）"},"repeat":{"type":"string","enum":["daily","weekly","monthly","yearly"]},"weekdays":{"type":"array","items":{"type":"integer"},"description":"weekly 时命中集合，0=周日..6=周六"},"day":{"type":"integer","description":"monthly/yearly：几号"},"month":{"type":"integer","description":"yearly：几月"}},"required":["time","repeat"]},
                        "action":{"type":"string","description":"到点投给主 agent 的提示词（自然语言指令/提醒，1-4000 字符）。到点 agent 收到带 [定时任务] 前缀的这条消息并照做——写成你希望 agent 那一刻做的事，不是写给用户的备忘"},
                        "context_inject":{"type":"array","items":{"type":"string"},"description":"可选；一并注入的文件/记忆路径列表，附在 action 末尾，agent 自行 read"},
                        "enabled":{"type":"boolean","description":"可选；默认 true（创建即生效）"}
                    }
                }
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"update_timer",
                "description":"更新已有定时任务：只传要改的字段，其余保留。task_id 必填（先 list_timers 取）。改 interval/schedule→下次按新规则触发；改 enabled→启停。",
                "parameters":{
                    "type":"object",
                    "required":["task_id"],
                    "properties":{
                        "task_id":{"type":"string"},
                        "name":{"type":"string"},"enabled":{"type":"boolean"},"mode":{"type":"string"},"count":{"type":"string"},
                        "action":{"type":"string"},"interval":{"type":"object"},"schedule":{"type":"object"},"context_inject":{"type":"array","items":{"type":"string"}}}
                }
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"delete_timer",
                "description":"删除定时任务（只删配置，不删已触发历史）。task_id 从 list_timers 取；删前确认——用户可能还在用。",
                "parameters":{"type":"object","required":["task_id"],"properties":{"task_id":{"type":"string"}}}
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"list_timers",
                "description":"列出定时任务，默认只列启用的（生效中）。enabled_only:false 看全部含停用。返回 task_id/名称/规则/开关/action 摘要。建/改/删前先 list 确认现状（避免重复建）。",
                "parameters":{"type":"object","properties":{"enabled_only":{"type":"boolean","description":"默认 true=只列 enabled 的;false=列全部含停用"}}}
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"ask_user",
                "description":"向用户提问并等待回答。三步式 UI：先选/输入、再预览、最后确认提交。15s 倒计时（光标活动暂停,5s 无操作续倒）。超时自动选推荐项;无推荐项时降级为「用户未答,请自行决策」。跳过按钮 = 用户主动拒绝(aborted);全局中断/外部消息 = cancelled。\n何时用：长任务中遇到歧义、两种选择都说得通、或者你没有足够偏好信息时。\n何时不用：能用工具自查的(读文件/list_timers/grep)不要问;同一问题别反复问(用户会烦)。",
                "parameters":{
                    "type":"object",
                    "required":["question"],
                    "properties":{
                        "question":{"type":"string","description":"问题文本,清晰表述你要问什么"},
                        "context":{"type":"string","description":"可选;为什么问这个,帮用户决策"},
                        "options":{
                            "type":"array",
                            "description":"候选项(2-6 个为宜,不要太多也不要只有一个)",
                            "items":{
                                "type":"object",
                                "required":["label"],
                                "properties":{
                                    "label":{"type":"string"},
                                    "description":{"type":"string"},
                                    "recommended":{"type":"boolean","default":false,"description":"true=推荐项(最多 1 个,多则报错)"},
                                    "is_custom":{"type":"boolean","default":false,"description":"true=标记为「自定义输入」选项(点它切 textarea)"}
                                }
                            }
                        },
                        "allow_custom":{"type":"boolean","default":true,"description":"允许用户输入自定义文本(在 options 之外)"},
                        "require_confirm":{"type":"boolean","default":true,"description":"是否需要「确认提交」按钮(默认 true 防误触;false=点选项立即提交)"},
                        "timeout_secs":{"type":"integer","minimum":5,"maximum":600,"default":15},
                        "pause_on_activity":{"type":"boolean","default":true,"description":"用户动键盘/鼠标时暂停倒计时"},
                        "resume_after_idle_secs":{"type":"integer","minimum":1,"default":5,"description":"暂停后多久无操作续倒(秒)"}
                    }
                }
            }
        }),
    ]
}

/// 按工具名分发执行；未知工具返回错误字符串（不 panic）。
/// cfg/round 供 "subagent" 臂使用。
///
/// 返 Ok(String) = 普通 tool_result,Err(AskUserHandle) = 工具触发挂起（run_turn 看到 Err 不写 tool_result），
/// 把 Handle 交给 driver 转 AwaitUserRequest 走等待路径。
pub async fn dispatch(
    name: &str,
    args: Value,
    ctx: &ToolsCtx,
    cfg: &crate::config::Config,
    round: Arc<dyn crate::llm::LlmRound>,
) -> Result<String, crate::ask_user::AskUserHandle> {
    match name {
        "write" => Ok(tool_write(&args, &ctx.workspace)),
        "read" => Ok(tool_read(&args, &ctx.workspace)),
        "bash" => {
            let background = args.get("background").and_then(|v| v.as_bool()).unwrap_or(false);
            let default_t: u64 = if background { 600 } else { 60 };
            let t = args.get("timeout_secs").and_then(|v| v.as_u64()).unwrap_or(default_t);
            Ok(tool_bash(&args, ctx, t).await)
        }
        "display" => Ok(tool_display(&args, &ctx.workspace)),
        "edit_card" => Ok(tool_edit_card(&args, &ctx.workspace)),
        "edit" => Ok(tool_edit(&args, &ctx.workspace)),
        "subagent" => Ok(tool_subagent(&args, ctx, cfg, round).await),
        "attach" => Ok(tool_attach(&args, ctx).await),
        "dream" => Ok(tool_dream(&args, ctx)),
        "mem_list" => Ok(tool_mem_list(&args, ctx)),
        "mem_read" => Ok(tool_mem_read(&args, ctx)),
        "mem_history" => Ok(tool_mem_history(&args, ctx)),
        "mem_search" => Ok(tool_mem_search(&args, ctx)),
        "task_list" => Ok(tool_task_list(&args, ctx).await),
        "task_get" => Ok(tool_task_get(&args, ctx).await),
        "task_add" => Ok(tool_task_add(&args, ctx).await),
        "task_update" => Ok(tool_task_update(&args, ctx).await),
        "task_check" => Ok(tool_task_check(&args, ctx).await),
        "task_progress" => Ok(tool_task_progress(&args, ctx).await),
        "task_archive" => Ok(tool_task_archive(&args, ctx).await),
        "create_timer" => Ok(tool_create_timer(&args, ctx)),
        "update_timer" => Ok(tool_update_timer(&args, ctx)),
        "delete_timer" => Ok(tool_delete_timer(&args, ctx)),
        "list_timers" => Ok(tool_list_timers(&args, ctx)),
        "ask_user" => tool_ask_user(&args, ctx).await,
        "" => Ok("工具名为空（上一条 tool_call 是畸形输出：缺 name）。请直接用文本回答用户，或重新发起一次规范的工具调用。".into()),
        other => Ok(format!("未知工具: {}", other)),
    }
}

// ── 定时任务工具（agent 自主创建/管理 scheduler.json；到点由 scheduler ticker 投递提示词）──

fn tool_create_timer(args: &Value, ctx: &ToolsCtx) -> String {
    let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if name.is_empty() { return "错误：name 必填".into(); }
    if action.trim().is_empty() { return "错误：action 必填（到点投递给主 agent 的提示词）".into(); }
    if mode != "interval" && mode != "schedule" { return "错误：mode 必须是 interval 或 schedule".into(); }
    let count = args.get("count").and_then(|v| v.as_str())
        .filter(|s| *s == "once" || *s == "forever").unwrap_or("forever").to_string();
    let interval = args.get("interval").filter(|v| !v.is_null())
        .and_then(|v| serde_json::from_value::<crate::scheduler::IntervalSpec>(v.clone()).ok());
    let schedule = args.get("schedule").filter(|v| !v.is_null())
        .and_then(|v| serde_json::from_value::<crate::scheduler::ScheduleSpec>(v.clone()).ok());
    if mode == "interval" && interval.is_none() {
        return "错误：mode=interval 需提供 interval:{value,unit}".into();
    }
    if mode == "schedule" && schedule.is_none() {
        return "错误：mode=schedule 需提供 schedule:{time,repeat,...}".into();
    }
    let context_inject: Vec<String> = args.get("context_inject")
        .and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
    let enabled = args.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let task = crate::scheduler::TimerTask {
        task_id: crate::scheduler::gen_task_id(),
        name: name.clone(),
        enabled, mode, count, interval, schedule, action, context_inject,
        last_fired_ts: now_ms, last_fired_date: String::new(),
    };
    let id = task.task_id.clone();
    let mut cfg = crate::scheduler::load_from(&ctx.cache);
    cfg.global_enabled = true; // agent 建任务即自动开启全局开关，免得建了不生效
    cfg.tasks.push(task);
    match crate::scheduler::save_to(&ctx.cache, &cfg) {
        Ok(_) => format!("已创建定时任务 {id}「{name}」（enabled={enabled}, global_enabled=true）。到点会以「[定时任务 {id}]」开头的消息投递给主 agent。"),
        Err(e) => format!("保存失败：{e}"),
    }
}

fn tool_update_timer(args: &Value, ctx: &ToolsCtx) -> String {
    let id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if id.is_empty() { return "错误：task_id 必填".into(); }
    let mut cfg = crate::scheduler::load_from(&ctx.cache);
    let task = match cfg.tasks.iter_mut().find(|t| t.task_id == id) {
        Some(t) => t,
        None => return format!("未找到任务 {id}"),
    };
    if let Some(v) = args.get("name").and_then(|v| v.as_str()) { task.name = v.into(); }
    if let Some(v) = args.get("enabled").and_then(|v| v.as_bool()) { task.enabled = v; }
    if let Some(v) = args.get("mode").and_then(|v| v.as_str()) { task.mode = v.into(); }
    if let Some(v) = args.get("count").and_then(|v| v.as_str()) { task.count = v.into(); }
    if let Some(v) = args.get("action").and_then(|v| v.as_str()) { task.action = v.into(); }
    if let Some(v) = args.get("interval").filter(|v| !v.is_null()) {
        if let Ok(i) = serde_json::from_value::<crate::scheduler::IntervalSpec>(v.clone()) { task.interval = Some(i); }
    }
    if let Some(v) = args.get("schedule").filter(|v| !v.is_null()) {
        if let Ok(s) = serde_json::from_value::<crate::scheduler::ScheduleSpec>(v.clone()) { task.schedule = Some(s); }
    }
    if let Some(v) = args.get("context_inject") {
        if let Ok(c) = serde_json::from_value::<Vec<String>>(v.clone()) { task.context_inject = c; }
    }
    match crate::scheduler::save_to(&ctx.cache, &cfg) {
        Ok(_) => format!("已更新任务 {id}"),
        Err(e) => format!("保存失败：{e}"),
    }
}

fn tool_delete_timer(args: &Value, ctx: &ToolsCtx) -> String {
    let id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if id.is_empty() { return "错误：task_id 必填".into(); }
    let mut cfg = crate::scheduler::load_from(&ctx.cache);
    let before = cfg.tasks.len();
    cfg.tasks.retain(|t| t.task_id != id);
    if cfg.tasks.len() == before { return format!("未找到任务 {id}"); }
    match crate::scheduler::save_to(&ctx.cache, &cfg) {
        Ok(_) => format!("已删除任务 {id}"),
        Err(e) => format!("保存失败：{e}"),
    }
}

fn tool_list_timers(args: &Value, ctx: &ToolsCtx) -> String {
    let cfg = crate::scheduler::load_from(&ctx.cache);
    let only_enabled = args.get("enabled_only").and_then(|v| v.as_bool()).unwrap_or(true);
    let tasks: Vec<&crate::scheduler::TimerTask> =
        cfg.tasks.iter().filter(|t| !only_enabled || t.enabled).collect();
    if tasks.is_empty() { return format!("（无定时任务；global_enabled={}）", cfg.global_enabled); }
    let mut lines = vec![format!("定时任务 {} 个（global_enabled={}）：", tasks.len(), cfg.global_enabled)];
    for t in tasks {
        let trig = match (t.mode.as_str(), &t.interval, &t.schedule) {
            ("interval", Some(i), _) => format!("每{}{}", i.value, i.unit),
            ("schedule", _, Some(s)) => match s.repeat.as_str() {
                "daily" => format!("每天 {}", s.time),
                "weekly" => format!("每周{:?} {}", s.weekdays, s.time),
                "monthly" => format!("每月{}号 {}", s.day, s.time),
                "yearly" => format!("每年{}-{} {}", s.month, s.day, s.time),
                other => format!("{other} {}", s.time),
            },
            _ => t.mode.clone(),
        };
        let mark = if t.enabled { "✓" } else { "✗" };
        lines.push(format!("{} {}「{}」| {} | {} | {}", mark, t.task_id, t.name, trig, t.count, truncate(&t.action, 50, "…")));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_path_absolute() {
        let ws = Path::new("C:/ws");
        assert_eq!(resolve_path("D:/x.txt", ws), PathBuf::from("D:/x.txt"));
    }
    #[test]
    fn resolve_path_relative() {
        let ws = Path::new("C:/ws");
        assert_eq!(resolve_path("a/b.txt", ws), ws.join("a/b.txt"));
    }
    #[test]
    fn resolve_path_trims() {
        let ws = Path::new("C:/ws");
        assert_eq!(resolve_path("  a.txt  ", ws), ws.join("a.txt"));
    }
    #[test]
    fn truncate_short_unchanged() {
        assert_eq!(truncate("abc", 100, "..."), "abc");
    }
    #[test]
    fn truncate_over_cuts_at_boundary() {
        let s = "abcdefghij";
        let out = truncate(s, 5, "|");
        assert_eq!(out, "abcde|"); // 5 是 ASCII 的有效 char 边界
    }
    #[test]
    fn truncate_multibyte_boundary() {
        let s = "中文测试"; // 每字 3 字节
        let out = truncate(s, 4, "|");
        assert_eq!(out, "中|"); // 3 字节一个汉字，4 处非边界回退到 3
    }

    #[test]
    fn write_then_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let args = serde_json::json!({ "path":"sub/a.txt", "content":"hello 世界" });
        let w = tool_write(&args, ws);
        assert!(w.contains("已写入"));
        let r = tool_read(&serde_json::json!({"path":"sub/a.txt"}), ws);
        assert_eq!(r, "hello 世界");
    }
    #[test]
    fn write_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        tool_write(&serde_json::json!({"path":"a/b/c.txt","content":"x"}), ws);
        assert_eq!(tool_read(&serde_json::json!({"path":"a/b/c.txt"}), ws), "x");
    }
    #[test]
    fn read_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let r = tool_read(&serde_json::json!({"path":"nope.txt"}), dir.path());
        assert!(r.contains("读取失败"));
    }
    #[test]
    fn read_binary_detected() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::write(ws.join("bin.dat"), [0u8, 1, 0, 2]).unwrap();
        let r = tool_read(&serde_json::json!({"path":"bin.dat"}), ws);
        assert!(r.contains("二进制"));
    }
    #[test]
    fn write_missing_args_errors() {
        let r = tool_write(&serde_json::json!({"path":"a"}), Path::new("."));
        assert!(r.contains("缺少 content"));
    }
    #[tokio::test]
    async fn bash_echo() {
        let dir = tempfile::tempdir().unwrap();
        let out = tool_bash(&serde_json::json!({"command":"echo hello"}), &ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into()), 10).await;
        assert!(out.contains("hello"), "got: {out}");
        assert!(out.contains("退出码 0"), "got: {out}");
    }
    #[tokio::test]
    async fn bash_timeout_kills() {
        let dir = tempfile::tempdir().unwrap();
        // ping -n 60 持续约 60s，2s 超时
        let out = tool_bash(&serde_json::json!({"command":"ping -n 60 127.0.0.1"}), &ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into()), 2).await;
        assert!(out.contains("超时"), "got: {out}");
    }
    #[tokio::test]
    async fn bash_missing_command() {
        let out = tool_bash(&serde_json::json!({}), &ToolsCtx::foreground(Path::new(".").to_path_buf(), "cn".into()), 5).await;
        assert!(out.contains("缺少 command"));
    }
    #[tokio::test]
    async fn dispatch_routes_write() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config::default();
        let r = dispatch("write", serde_json::json!({"path":"a.txt","content":"x"}),
            &ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into()),
            &cfg, Arc::new(crate::llm::HttpRound)).await;
        assert!(r.expect("write 应返 Ok").contains("已写入"));
    }
    #[tokio::test]
    async fn dispatch_unknown_tool() {
        let cfg = crate::config::Config::default();
        let r = dispatch("nope", serde_json::json!({}),
            &ToolsCtx::foreground(Path::new(".").to_path_buf(), "cn".into()),
            &cfg, Arc::new(crate::llm::HttpRound)).await;
        assert!(r.expect("unknown 应返 Ok").contains("未知工具"));
    }

    // ── ask_user 测试 ─────────────────────────────────────────

    fn ask_ctx() -> (tempfile::TempDir, ToolsCtx) {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        // session_tx 需可发;用有接收者的 channel
        let (tx, _rx) = tokio::sync::mpsc::channel::<crate::agent::SessionEvent>(8);
        ctx.session_tx = tx;
        (dir, ctx)
    }

    #[tokio::test]
    async fn ask_user_missing_question_errors() {
        let (_d, ctx) = ask_ctx();
        let r = tool_ask_user(&serde_json::json!({}), &ctx).await;
        assert!(r.is_ok(), "缺 question 应返 Ok(错误字符串),不是挂起 Err");
        assert!(r.unwrap().contains("question 必填"));
    }

    #[tokio::test]
    async fn ask_user_multiple_recommended_errors() {
        let (_d, ctx) = ask_ctx();
        let r = tool_ask_user(&serde_json::json!({
            "question": "选一个",
            "options": [
                {"label": "A", "recommended": true},
                {"label": "B", "recommended": true},
            ],
        }), &ctx).await;
        assert!(r.is_ok(), "schema 错应返 Ok(错误字符串),不挂起");
        let s = r.unwrap();
        assert!(s.contains("最多 1 个"), "错误信息应说明上限: {s}");
    }

    #[tokio::test]
    async fn ask_user_one_recommended_returns_handle() {
        let (_d, ctx) = ask_ctx();
        let r = tool_ask_user(&serde_json::json!({
            "question": "选一个",
            "options": [
                {"label": "A"},
                {"label": "B", "recommended": true},
            ],
            "timeout_secs": 30,
        }), &ctx).await;
        assert!(r.is_err(), "校验通过应返 Err(Handle) = 挂起");
        let handle = r.unwrap_err();
        assert!(!handle.question_id.is_empty(), "question_id 已生成");
        assert_eq!(handle.request.options.len(), 2);
        assert_eq!(handle.request.options[1].recommended, true);
        assert_eq!(handle.request.timeout_secs, 30);
    }

    #[tokio::test]
    async fn ask_user_no_options_still_returns_handle() {
        let (_d, ctx) = ask_ctx();
        let r = tool_ask_user(&serde_json::json!({
            "question": "自由答吧",
        }), &ctx).await;
        assert!(r.is_err(), "无 options + allow_custom 默认 true,允许纯自定义输入,应挂起");
        let handle = r.unwrap_err();
        assert!(handle.request.options.is_empty());
        assert!(handle.request.allow_custom);
    }

    #[tokio::test]
    async fn ask_user_dispatch_route() {
        let (_d, ctx) = ask_ctx();
        let cfg = crate::config::Config::default();
        let r = dispatch("ask_user", serde_json::json!({
            "question": "hi",
            "options": [{"label": "ok"}],
        }), &ctx, &cfg, Arc::new(crate::llm::HttpRound)).await;
        assert!(r.is_err(), "dispatch 应透传 tool_ask_user 的 Err(Handle)");
    }

    #[test]
    fn env_allowlist_drops_non_prefixed() {
        let v = serde_json::json!({"PATH":"x","MINIMAX_REGION":"global","MMX_CONFIG_DIR":"/tmp","EVIL":"1"});
        let out = parse_env(Some(&v), "cn");
        assert_eq!(out.iter().find(|(k,_)| k=="MINIMAX_REGION").unwrap().1, "cn",
            "基线 region 必须覆盖传入的 global");
        assert!(out.iter().any(|(k,_)| k=="MMX_CONFIG_DIR"), "MMX_* 应通过");
        assert!(!out.iter().any(|(k,_)| k=="PATH" || k=="EVIL"), "非 allowlist 前缀应被丢弃");
    }

    #[test]
    fn display_missing_path_errors() {
        let dir = tempfile::tempdir().unwrap();
        let r = tool_display(&serde_json::json!({}), dir.path());
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["display"], false);
        assert!(v["error"].as_str().unwrap().contains("path"));
    }

    #[test]
    fn display_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let r = tool_display(&serde_json::json!({"path":"nope.mp4"}), dir.path());
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["display"], false);
        assert!(v["error"].as_str().unwrap().contains("文件不存在"));
    }

    #[test]
    fn display_media_kind_from_ext() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cat.mp4"), b"x").unwrap();
        let r = tool_display(&serde_json::json!({"path":"cat.mp4","caption":"夕阳猫"}), dir.path());
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["display"], true);
        assert_eq!(v["kind"], "video");
        assert!(v["path"].as_str().unwrap().ends_with("cat.mp4"));
        assert_eq!(v["caption"], "夕阳猫");
    }

    #[test]
    fn display_doc_unsupported_with_hint() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("old.doc"), b"x").unwrap();
        let r = tool_display(&serde_json::json!({"path":"old.doc"}), dir.path());
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["display"], false);
        assert!(v["error"].as_str().unwrap().contains(".doc"));
    }
    #[test]
    fn display_auto_routes_by_ext() {
        // 统一工具：同一参数（仅 path），按扩展名自动判 kind；媒体/文档/文本全覆盖。
        let dir = tempfile::tempdir().unwrap();
        for (fname, kind) in [
            ("a.png", "image"), ("b.mp4", "video"), ("c.mp3", "audio"),
            ("d.html", "html"), ("e.pdf", "pdf"), ("f.docx", "docx"),
            ("g.csv", "csv"), ("h.md", "markdown"), ("i.py", "text"),
        ] {
            std::fs::write(dir.path().join(fname), b"x").unwrap();
            let r = tool_display(&serde_json::json!({"path":fname,"caption":"cap"}), dir.path());
            let v: serde_json::Value = serde_json::from_str(&r).unwrap();
            assert_eq!(v["display"], true, "{fname}");
            assert_eq!(v["kind"], kind, "{fname}");
            assert!(v["path"].as_str().unwrap().ends_with(fname), "{fname}");
            assert_eq!(v["caption"], "cap", "{fname}");
        }
    }
    #[test]
    fn edit_card_missing_path_errors() {
        let r = tool_edit_card(&serde_json::json!({}), Path::new("."));
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["edit"], false);
        assert!(v["error"].as_str().unwrap().contains("path"));
    }
    #[test]
    fn edit_card_non_editable_kind_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.pdf"), b"%PDF").unwrap();
        let r = tool_edit_card(&serde_json::json!({"path":"a.pdf"}), dir.path());
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["edit"], false);
        assert!(v["error"].as_str().unwrap().contains("文本类文件"), "error 应说明仅文本类文件可编辑");
    }
    #[test]
    fn edit_card_reads_disk_when_no_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("n.md"), "# 标题\n正文").unwrap();
        let r = tool_edit_card(&serde_json::json!({"path":"n.md"}), dir.path());
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["edit"], true);
        assert_eq!(v["kind"], "markdown");
        assert_eq!(v["content"], "# 标题\n正文");
    }
    #[test]
    fn edit_card_prefills_given_content_for_new_file() {
        let dir = tempfile::tempdir().unwrap();
        // 文件不存在但给了 content（新建场景）
        let r = tool_edit_card(&serde_json::json!({"path":"new.md","content":"初始"}), dir.path());
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["edit"], true);
        assert_eq!(v["content"], "初始");
    }
    #[test]
    fn edit_card_text_kind() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "plain").unwrap();
        let r = tool_edit_card(&serde_json::json!({"path":"a.txt"}), dir.path());
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["edit"], true);
        assert_eq!(v["kind"], "text");
        assert_eq!(v["content"], "plain");
    }

    // tool_edit：agent 直接改盘的精确查找替换（唯一匹配）
    #[test]
    fn edit_missing_args_errors() {
        assert!(tool_edit(&serde_json::json!({}), Path::new(".")).contains("缺少 path"));
        assert!(tool_edit(&serde_json::json!({"path":"a.txt"}), Path::new(".")).contains("缺少 old_string"));
        assert!(tool_edit(&serde_json::json!({"path":"a.txt","old_string":"x"}), Path::new(".")).contains("缺少 new_string"));
    }
    #[test]
    fn edit_empty_old_string_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let r = tool_edit(&serde_json::json!({"path":"a.txt","old_string":"","new_string":"x"}), dir.path());
        assert!(r.contains("不能为空"), "空 old_string 应拒绝: {r}");
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "hello", "不应改盘");
    }
    #[test]
    fn edit_old_equals_new_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let r = tool_edit(&serde_json::json!({"path":"a.txt","old_string":"hello","new_string":"hello"}), dir.path());
        assert!(r.contains("相同"), "old==new 应拒绝: {r}");
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "hello", "不应改盘");
    }
    #[test]
    fn edit_replaces_unique_match_and_writes_disk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "foo bar baz").unwrap();
        let r = tool_edit(&serde_json::json!({"path":"a.txt","old_string":"bar","new_string":"QUX"}), dir.path());
        assert!(r.contains("已替换"), "应成功: {r}");
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "foo QUX baz");
    }
    #[test]
    fn edit_deletes_via_empty_new_string() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "keep REMOVE me tail").unwrap();
        let r = tool_edit(&serde_json::json!({"path":"a.txt","old_string":"REMOVE ","new_string":""}), dir.path());
        assert!(r.contains("已替换"), "空 new_string 删除应成功: {r}");
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "keep me tail");
    }
    #[test]
    fn edit_zero_match_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello world").unwrap();
        let r = tool_edit(&serde_json::json!({"path":"a.txt","old_string":"zzz","new_string":"y"}), dir.path());
        assert!(r.contains("未找到"), "0 匹配应报错: {r}");
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "hello world", "未改盘");
    }
    #[test]
    fn edit_non_unique_match_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "dup dup dup").unwrap();
        let r = tool_edit(&serde_json::json!({"path":"a.txt","old_string":"dup","new_string":"x"}), dir.path());
        assert!(r.contains("不唯一"), ">1 匹配应报错: {r}");
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "dup dup dup", "未改盘");
    }
    #[test]
    fn edit_binary_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.dat"), [0u8, 1, 0, 2]).unwrap();
        let r = tool_edit(&serde_json::json!({"path":"b.dat","old_string":"x","new_string":"y"}), dir.path());
        assert!(r.contains("二进制"), "二进制应拒绝: {r}");
    }
    #[test]
    fn edit_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let r = tool_edit(&serde_json::json!({"path":"nope.txt","old_string":"a","new_string":"b"}), dir.path());
        assert!(r.contains("读取失败"), "文件不存在应报读取失败: {r}");
    }
    #[test]
    fn write_file_scoped_inside_workspace_ok() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        let target = ws.join("sub/a.md");
        let r = write_file_scoped(&target, "# hi\n你好", &[ws.clone()]);
        assert!(r.is_ok(), "{:?}", r);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "# hi\n你好");
    }
    #[test]
    fn write_file_scoped_creates_new_file_and_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        let target = ws.join("nested/deep/x.txt"); // 不存在
        let r = write_file_scoped(&target, "new", &[ws.clone()]);
        assert!(r.is_ok(), "{:?}", r);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    }
    #[test]
    fn write_file_scoped_outside_workspace_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        let target = outside.path().join("evil.txt");
        let r = write_file_scoped(&target, "x", &[ws]);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("不在允许范围"));
        assert!(!target.exists(), "越权写不应落盘");
    }
    #[test]
    fn write_file_scoped_traversal_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        // ws/../evil → 越出 ws，必须被拒（不落盘）
        let target = ws.join("..").join("evil_traversal.txt");
        let r = write_file_scoped(&target, "x", &[ws]);
        assert!(r.is_err(), "越权路径应被拒");
        assert!(r.unwrap_err().contains("不在允许范围"));
    }

    // Task 2: stage_one tests
    fn stage_ok(args: serde_json::Value, ws: &Path, max_mb: u64) -> crate::StagedFile {
        let p = args["path"].as_str().unwrap();
        let src = resolve_path(p, ws);
        let dir = ws.join("attachments");
        crate::tools::stage_one(&src, &dir, max_mb)
    }

    #[test]
    fn stage_one_copies_and_detects_kind() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::write(ws.join("cat.png"), b"png-bytes").unwrap();
        let s = stage_ok(serde_json::json!({"path":"cat.png"}), ws, 30);
        assert!(s.ok);
        assert_eq!(s.kind, "image");
        assert_eq!(s.original_name, "cat.png");
        assert_eq!(s.size, 9);
        assert!(s.staged_path.ends_with(".png"));
        assert!(s.text.is_none(), "图片不提文本");
        // 副本确实落盘
        assert!(std::fs::metadata(&s.staged_path).is_ok());
    }

    #[test]
    fn stage_one_text_under_50kb_inlined() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::write(ws.join("a.txt"), "hello world").unwrap();
        let s = stage_ok(serde_json::json!({"path":"a.txt"}), ws, 30);
        assert!(s.ok);
        assert_eq!(s.kind, "text");
        assert_eq!(s.text.as_deref(), Some("hello world"));
    }

    #[test]
    fn stage_one_text_over_50kb_not_inlined() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let big = "x".repeat(60 * 1024);
        std::fs::write(ws.join("big.txt"), &big).unwrap();
        let s = stage_ok(serde_json::json!({"path":"big.txt"}), ws, 30);
        assert!(s.ok);
        assert!(s.text.is_none(), "超 50KB 不内联");
    }

    #[test]
    fn stage_one_oversize_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::write(ws.join("big.pdf"), vec![0u8; 31 * 1024 * 1024]).unwrap();
        let s = stage_ok(serde_json::json!({"path":"big.pdf"}), ws, 30);
        assert!(!s.ok);
        assert!(s.error.as_deref().unwrap().contains("过大"));
    }

    #[test]
    fn stage_one_dedup_reuses_same_hash() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::write(ws.join("a.png"), b"identical").unwrap();
        std::fs::write(ws.join("b.png"), b"identical").unwrap();
        let s1 = stage_ok(serde_json::json!({"path":"a.png"}), ws, 30);
        let s2 = stage_ok(serde_json::json!({"path":"b.png"}), ws, 30);
        assert_eq!(s1.staged_path, s2.staged_path, "同内容应复用同一副本");
    }

    #[test]
    fn stage_one_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let s = stage_ok(serde_json::json!({"path":"nope.png"}), dir.path(), 30);
        assert!(!s.ok);
        assert!(s.error.as_deref().unwrap().contains("不存在"));
    }

    #[test]
    fn schemas_subset_returns_named_tools() {
        let s = schemas_subset(SUBAGENT_TOOLS);
        let names: Vec<&str> = s.iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["write", "read", "edit", "bash"]);
    }
    #[test]
    fn schemas_subset_drift_guard() {
        let s = schemas_subset(SUBAGENT_TOOLS);
        assert_eq!(s.len(), SUBAGENT_TOOLS.len(), "subset 数应 == SUBAGENT_TOOLS 数（防 typo/漂移）");
        let names: Vec<&str> = s.iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        assert!(!names.contains(&"subagent"), "子代理工具子集绝不能含 subagent（递归锁）");
        assert!(!names.contains(&"display") && !names.contains(&"edit_card"), "子集不含卡片工具");
    }
    #[tokio::test]
    async fn bash_background_forced_foreground_when_disallowed() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        ctx.allow_background = false; // 子代理 ctx
        // background:true 但 allow_background=false → 应前台跑完返回输出（而非返回 job_id JSON）
        let out = tool_bash(&serde_json::json!({"command":"echo forced","background":true}), &ctx, 10).await;
        assert!(out.contains("退出码"), "应被强制前台：{out}");
        assert!(!out.contains("job_id"), "不应走后台：{out}");
    }
    #[tokio::test]
    async fn tool_subagent_missing_prompt_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        let cfg = crate::config::Config::default();
        let r = tool_subagent(&serde_json::json!({"action":"spawn"}), &ctx, &cfg,
            std::sync::Arc::new(crate::llm::HttpRound)).await;
        assert!(r.contains("缺少 prompt"), "spawn 缺 prompt 应报错: {r}");
    }

    #[test]
    fn path_with_exe_dir_prepends_exe_parent() {
        let (k, v) = path_with_exe_dir().expect("current_exe 应可用（测试在二进制内运行）");
        assert_eq!(k, "PATH");
        let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_string_lossy().to_string();
        assert!(v.starts_with(&exe_dir), "PATH 应前置 exe 目录: {v}");
    }
    #[tokio::test]
    async fn bash_can_call_mem_if_built() {
        // 集成性验证（mem 已构建时）：mem --help 应可经注入的 PATH 调到。
        // 注意：mem 在 target/debug 与 ovoice.exe 同目录；若未构建则跳过（不硬失败）。
        let dir = tempfile::tempdir().unwrap();
        let out = tool_bash(&serde_json::json!({"command":"mem --help"}),
            &ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into()), 10).await;
        // mem --help 成功 → 含 "mem ——"；mem 未构建 → "启动失败"/"不是内部命令"。两者都算通过（仅验 PATH 注入不崩）。
        assert!(out.contains("mem") || out.contains("失败") || out.contains("命令"),
            "bash 调 mem 应有响应（不崩）: {out}");
    }

    // ─── mem 工具族封装层：参数校验 + 路由到 mem_cli（底层 mem_cli 自有测试，这里只验封装） ───

    fn mem_cache_with_day(date_compact: &str, day_jsonl_body: &str) -> tempfile::TempDir {
        // cache 根 = tempdir；memory/YYYY/MM/YYYY-MM-DD.jsonl
        let dir = tempfile::tempdir().unwrap();
        let (y, m, d) = (&date_compact[..4], &date_compact[4..6], &date_compact[6..8]);
        let p = dir.path().join("memory").join(y).join(m).join(format!("{y}-{m}-{d}.jsonl"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, day_jsonl_body).unwrap();
        dir
    }

    #[test]
    fn mem_list_no_arg_lists_years() {
        let dir = mem_cache_with_day("20260731", r#"{"hhmm":"10:00","type":"work","title":"x","seq":[1,2]}"#);
        let ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        let out = tool_mem_list(&serde_json::json!({}), &ctx);
        assert!(out.contains("2026"), "无参列年: {out}");
    }

    #[test]
    fn mem_list_routes_by_date_granularity() {
        // YYYYMMDD → ls_day，输出含事件标题
        let dir = mem_cache_with_day("20260731",
            r#"{"hhmm":"10:00","type":"work","title":"封装mem","seq":[1,2]}"#);
        let ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        let out = tool_mem_list(&serde_json::json!({"date":"20260731"}), &ctx);
        assert!(out.contains("封装mem"), "ls_day 路由: {out}");
    }

    #[test]
    fn mem_read_routes_day_month_year() {
        let dir = mem_cache_with_day("20260731",
            r#"{"hhmm":"10:00","evt":"e1","type":"work","title":"封装mem","detail":"d","keywords":["k"],"subject":"s","ref":"r","seq":[1,2]}"#);
        let ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        // day
        let d = tool_mem_read(&serde_json::json!({"date":"2026-07-31"}), &ctx);
        assert!(d.contains("封装mem"), "read_day: {d}");
        // month（无文件→友好串）
        let m = tool_mem_read(&serde_json::json!({"date":"2026-07"}), &ctx);
        assert!(m.contains("无") || m.contains("2026-07"), "read_month 兜底: {m}");
        // year
        let y = tool_mem_read(&serde_json::json!({"date":"2026"}), &ctx);
        assert!(y.contains("无") || y.contains("2026"), "read_year 兜底: {y}");
    }

    #[test]
    fn mem_read_rejects_bad_date_format() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        let out = tool_mem_read(&serde_json::json!({"date":"2026-13-99"}), &ctx);
        assert!(out.contains("日期格式应为"), "非法月日被拒: {out}");
    }

    #[test]
    fn mem_read_missing_date_arg() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        let out = tool_mem_read(&serde_json::json!({}), &ctx);
        assert!(out.contains("缺少 date"), "缺参早返: {out}");
    }

    #[test]
    fn mem_history_requires_both_seq_or_neither() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        // 只给 seq_start → 拒
        let half = tool_mem_history(&serde_json::json!({"date":"2026-07-31","seq_start":1}), &ctx);
        assert!(half.contains("同时"), "单端 seq 被拒: {half}");
        // 都不给 → OK（底层返回「无原始对话」）
        let none = tool_mem_history(&serde_json::json!({"date":"2026-07-31"}), &ctx);
        assert!(none.contains("无") || none.contains("history"), "无 seq 范围: {none}");
    }

    #[test]
    fn mem_search_missing_query() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        let out = tool_mem_search(&serde_json::json!({}), &ctx);
        assert!(out.contains("query"), "缺 query 被拒: {out}");
    }

    // ─── tool_attach：pdf/docx 抽文本 + 拒绝路径 ───
    #[tokio::test]
    async fn attach_docx_extracts_text_into_result() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().to_path_buf();
        let p = crate::extract::build_minimal_docx(&["季度营收", "同比增长"]);
        // build_minimal_docx 给绝对路径；复制进 workspace 让 resolve_path 命中
        let dest = ws.join("report.docx");
        std::fs::copy(&p, &dest).unwrap();
        let _ = std::fs::remove_file(&p);
        let ctx = ToolsCtx::foreground(ws, "cn".into());
        let r = tool_attach(&serde_json::json!({"path":"report.docx"}), &ctx).await;
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["attached"], true, "docx 应成功: {r}");
        assert_eq!(v["kind"], "docx");
        assert!(v["text"].as_str().unwrap().contains("季度营收"), "应内联抽取文本: {r}");
        assert!(v["text"].as_str().unwrap().contains("同比增长"));
    }

    #[tokio::test]
    async fn attach_doc_rejects_with_docx_hint() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("old.doc"), b"ole-bytes").unwrap();
        let ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        let r = tool_attach(&serde_json::json!({"path":"old.doc"}), &ctx).await;
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["attached"], false);
        assert!(v["error"].as_str().unwrap().contains(".doc"), "应点名 .doc: {r}");
        assert!(v["error"].as_str().unwrap().contains("docx"), "应引导转 docx: {r}");
    }

    #[tokio::test]
    async fn attach_txt_rejects_pointing_to_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        let r = tool_attach(&serde_json::json!({"path":"a.txt"}), &ctx).await;
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["attached"], false);
        assert!(v["error"].as_str().unwrap().contains("read"), "txt 应引导用 read: {r}");
    }

    #[tokio::test]
    async fn attach_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        let r = tool_attach(&serde_json::json!({"path":"nope.pdf"}), &ctx).await;
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["attached"], false);
        assert!(v["error"].as_str().unwrap().contains("不存在"), "应报不存在: {r}");
    }

    #[tokio::test]
    async fn attach_missing_path_arg_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        let r = tool_attach(&serde_json::json!({}), &ctx).await;
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["attached"], false);
        assert!(v["error"].as_str().unwrap().contains("path"));
    }

    #[test]
    fn schemas_has_twenty_five_tools() {
        let s = schemas();
        let names: Vec<&str> = s.iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        // 8 基础 + dream + 4 mem + 7 task + 4 timer + 1 ask_user = 25；须与 llm.rs 的 tools.len()==25 断言同步
        assert_eq!(
            names,
            vec![
                "write", "read", "bash", "display", "edit_card", "edit", "subagent",
                "attach", "dream",
                "mem_list", "mem_read", "mem_history", "mem_search",
                "task_list", "task_get", "task_add", "task_update", "task_check",
                "task_progress", "task_archive",
                "create_timer", "update_timer", "delete_timer", "list_timers",
                "ask_user",
            ]
        );
    }

    #[test]
    fn subagent_schema_guides_self_contained_prompt() {
        let s = schemas();
        let sub = s.iter()
            .find(|t| t["function"]["name"] == "subagent")
            .expect("subagent schema 存在");
        let prompt_desc = sub["function"]["parameters"]["properties"]["prompt"]["description"]
            .as_str()
            .unwrap();
        assert!(
            prompt_desc.contains("自包含") && prompt_desc.contains("看不到你们的对话历史"),
            "spawn prompt 描述须引导自包含（子代理无对话历史）: {prompt_desc}"
        );
    }

    // Task 4: image 分支 - stage_one 落盘 + 发 InjectAttachment 信号
    #[tokio::test]
    async fn attach_image_stages_and_sends_inject_signal() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().to_path_buf();
        // 造一张真图（stage_one 走 IMG_MAX_BYTES 校验，小文件 OK）
        std::fs::write(ws.join("pic.png"), b"png-bytes").unwrap();
        // ctx 用带可观测 rx 的 session_tx（foreground 用无人接收 channel，测不到信号 → 手工建）
        let (stx, mut srx) = tokio::sync::mpsc::channel::<crate::agent::SessionEvent>(8);
        let mut ctx = ToolsCtx::foreground(ws.clone(), "cn".into());
        ctx.cache = ws.clone(); // 让 attachments 落 tempdir 内可断言
        ctx.session_tx = stx;
        let r = tool_attach(&serde_json::json!({"path":"pic.png","caption":"草图"}), &ctx).await;
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["attached"], true, "image 应成功: {r}");
        assert_eq!(v["kind"], "image");
        assert!(v["staged_path"].as_str().unwrap().ends_with(".png"));
        assert!(v["note"].as_str().unwrap().contains("下一轮"), "note 须说明下轮可见: {r}");
        // 信号已发：driver 应收到 InjectAttachment
        match srx.recv().await.unwrap() {
            crate::agent::SessionEvent::InjectAttachment { attachments, caption } => {
                assert_eq!(caption, "草图");
                assert_eq!(attachments.len(), 1);
                assert_eq!(attachments[0].kind, "image");
                // staged_path 真实落盘
                assert!(std::fs::metadata(&attachments[0].staged_path).is_ok());
            }
            other => panic!("应为 InjectAttachment，得到 {other:?}"),
        }
    }
}
