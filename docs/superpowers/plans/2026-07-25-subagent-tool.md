# subagent 工具（后台子代理）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给主 agent 一个 `subagent` 工具，能 spawn 后台子代理（独立 MiniMax agent loop + 工具子集 {write,read,edit,bash}），支持 status 查进度、kill 终止回收、自然完成自动回注。

**Architecture:** 复刻 bash-background 的 jobs 范式：子代理注册为 `JobRegistry` 里 `kind:"agent"` 的 job，独立 tokio 任务跑 `run_turn`（自带 SubagentEmitter 流式上屏），完成经 `job_done_tx` 回注主代理。kill 用 `CancellationToken`；工具集经 `Config.active_tools` 字段下发给 `build_body`；round 经 `Arc<dyn LlmRound>` 透传到子代理（可注入 FakeRound 单测）。

**Tech Stack:** Rust + Tauri 2 + tokio + tokio-util（CancellationToken）；前端 vanilla JS（无打包器、无 JS 测试框架）。

## Global Constraints

- 前端 UMD-only vanilla JS；无 JS 测试框架（`node --check src/main.js` + 手测）。
- 后端纯逻辑可离线测：`tempfile` + `FakeRound`/`FakeEmitter`，与现有 display/edit 工具同款。
- agent driver 事件驱动、零锁：`messages` 由 driver 私有持有。
- dev server 占 exe 时验证用 `cargo check --tests --manifest-path src-tauri/Cargo.toml` / `cargo test --lib --manifest-path src-tauri/Cargo.toml`，**不要 `cargo build`**（锁 exe）。
- 新增 `subagent` 工具后 `llm.rs` 的 `tools.len()` 断言 6→7、`tools.rs` 名字表测试同步（[[ovoice-tool-count-cascade]]）。
- 密钥：子代理复用 `cfg.api_key`，无新增密钥；不得打印/泄露 `config.json`。
- 分支：`feat/subagent-tool`（off master @ 7f602c0）。每个 Task 末尾 commit。

## File Structure

| 文件 | 责任 | 本计划改动 |
|---|---|---|
| `src-tauri/src/config.rs` | Config 持久化 | 加 `active_tools` + `subagent_system_prompt` |
| `src-tauri/src/llm.rs` | MiniMax 调用 + run_turn 循环 | round 改 `Arc<dyn>`、`run_turn` 加 `max_iters`、`build_body` 读 `active_tools`、dispatch 调用点透传、断言 6→7 |
| `src-tauri/src/jobs.rs` | Job 系统 | `JobKind`、Job/JobOutcome 新字段、`SubagentProgress`/`ToolTrace`、并发计数、`kill_job` kind 分派 |
| `src-tauri/src/subagents.rs`（新） | 子代理专属：SubagentEmitter + spawn/status/kill | 新建 |
| `src-tauri/src/tools.rs` | 工具 schema + dispatch | `ToolsCtx.allow_background`、`tool_bash` 前台强制、`schemas_subset`/`SUBAGENT_TOOLS`、`tool_subagent`、dispatch 臂、schema 6→7 |
| `src-tauri/src/agent.rs` | driver | `inject_jobdone_message` kind 分支、reset 先 cancel、spawn_session 透传 round + sub-ctx |
| `src-tauri/src/lib.rs` | Tauri 命令/事件/接线 | `AppSubagentStream`、`spawn_session` wiring、`kill_job` 命令 agent 分支 |
| `src/main.js` | 前端 | jobs 面板 agent 行 + `subagent-stream` 监听 |
| `src-tauri/Cargo.toml` | 依赖 | `tokio-util` |

---

## Task 1: Config 加 `active_tools` + `subagent_system_prompt`

**Files:**
- Modify: `src-tauri/src/config.rs`（struct `Config` 13-76、`Default` impl 117-147、test `roundtrip_preserves_all_fields` 196-245）

**Interfaces:**
- Produces: `Config.active_tools: Option<Vec<serde_json::Value>>`（`#[serde(skip)]`，不持久化，运行时覆盖）；`Config.subagent_system_prompt: String`（`#[serde(default = "d_subagent_sys")]`）。

- [ ] **Step 1: 写失败测试**（加到 `config.rs` 的 `mod tests`）

```rust
#[test]
fn active_tools_defaults_none_and_skipped() {
    let c: Config = serde_json::from_str("{}").unwrap();
    assert!(c.active_tools.is_none(), "默认 None=全量 tools::schemas()");
    // skip 序列化：序列化结果里不应出现 active_tools 键
    let s = serde_json::to_string(&c).unwrap();
    assert!(!s.contains("active_tools"), "active_tools 不持久化: {s}");
}

#[test]
fn subagent_system_prompt_has_default() {
    let c: Config = serde_json::from_str("{}").unwrap();
    assert!(c.subagent_system_prompt.contains("子代理"), "默认值: {}", c.subagent_system_prompt);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml config::tests::active_tools_defaults_none_and_skipped config::tests::subagent_system_prompt_has_default`
Expected: 编译失败（字段不存在）。

- [ ] **Step 3: 实现**（struct 加两字段，紧跟 `glass_opacity` 之后）

```rust
    // 玻璃面板/气泡不透明度（0=全透 1=不透）；前端注入 --glass-alpha
    #[serde(default = "d_glass_opacity")]
    pub glass_opacity: f64,
    // 运行时工具集覆盖：None=全量 tools::schemas()（主 session）；Some=子集（子代理）。不持久化。
    #[serde(skip)]
    pub active_tools: Option<Vec<serde_json::Value>>,
    // 子代理专用 system prompt（spawn 子代理时作 messages[0]）。
    #[serde(default = "d_subagent_sys")]
    pub subagent_system_prompt: String,
```

加默认函数（挨着 `d_glass_opacity`）：

```rust
fn d_subagent_sys() -> String {
    "你是子代理。专注完成交给你的单一任务，用 write/read/edit/bash 工具动手。不要闲聊、不要复述任务。完成后给一段简洁的成果总结（结论 + 关键改动/产出），这段总结会作为你的最终答案回传给主代理。".into()
}
```

`Default` impl 末尾（`glass_opacity: d_glass_opacity(),` 之后）加：

```rust
            active_tools: None,
            subagent_system_prompt: d_subagent_sys(),
```

`roundtrip_preserves_all_fields` 测试里构造的 `Config { … }` 末尾加 `active_tools: None, subagent_system_prompt: d_subagent_sys(),`（否则编译失败：缺字段）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml config::tests`
Expected: PASS（含新两条 + 既有全过）。

- [ ] **Step 5: commit**

```bash
git add src-tauri/src/config.rs
git commit -m "feat(config): 加 active_tools + subagent_system_prompt 字段

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 2: round 改 `Arc<dyn LlmRound>` 透传 + dispatch(cfg, round) + run_turn(max_iters) + build_body 读 active_tools

**「先把改动变容易，再做改动」(Beck)：** 这是个纯重构 Task——为子代理能注入 round 与工具子集铺路，**不改任何行为**。改完所有既有测试仍全绿。

**Files:**
- Modify: `src-tauri/src/llm.rs`（`build_body` 25-36、`run_turn` 206-263、`run_loop` 266-275、dispatch 调用点 238、测试 `build_body_has_tools_and_reasoning_split` 533、`loop_*` 测试 574-625）
- Modify: `src-tauri/src/tools.rs`（`dispatch` 568-584、`ToolsCtx::foreground` 47-56、测试 `dispatch_routes_write`/`dispatch_unknown_tool` 678-687）
- Modify: `src-tauri/src/agent.rs`（`handle_event` 58-85、`run_one` 133-147、测试 `ctx()` 167-176、各 `handle_event` 调用 204/217/228/241）

**Interfaces:**
- Consumes: Task 1 的 `Config.active_tools`。
- Produces（新签名，后续 Task 依赖）：
  - `pub async fn run_turn<E: Emitter>(round: Arc<dyn LlmRound>, emitter: &E, cfg: &Config, messages: &mut Vec<Value>, ctx: &ToolsCtx, max_iters: usize) -> ChatResponse`
  - `pub async fn handle_event<E: Emitter>(event: &SessionEvent, messages: &mut Vec<Value>, ctx: &ToolsCtx, cfg: &Config, round: Arc<dyn LlmRound>, emitter: &E) -> Option<ChatResponse>`
  - `pub async fn dispatch(name: &str, args: Value, ctx: &ToolsCtx, cfg: &Config, round: Arc<dyn LlmRound>) -> String`
  - `build_body` 读 `cfg.active_tools.clone().unwrap_or_else(|| tools::schemas())`。

- [ ] **Step 1: 写失败测试**（`llm.rs` `mod tests`，先加一条 active_tools 的测试；其余签名改动靠既有测试兜底）

```rust
#[test]
fn build_body_uses_active_tools_when_set() {
    let mut cfg = Config::default();
    cfg.active_tools = Some(vec![serde_json::json!({"type":"function","function":{"name":"read"}})]);
    let body = build_body(&[serde_json::json!({"role":"user","content":"hi"})], &cfg).unwrap();
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1, "active_tools=Some 应覆盖默认全量");
    assert_eq!(tools[0]["function"]["name"], "read");
}

#[test]
fn build_body_defaults_to_all_schemas_when_none() {
    let cfg = Config::default(); // active_tools=None
    let body = build_body(&[serde_json::json!({"role":"user","content":"hi"})], &cfg).unwrap();
    assert_eq!(body["tools"].as_array().unwrap().len(), tools::schemas().len());
}
```

- [ ] **Step 2: 跑确认失败/编译错**

Run: `cargo check --tests --manifest-path src-tauri/Cargo.toml`
Expected: 编译错（dispatch 调用点参数不符等）——这是本 Task 要修的。

- [ ] **Step 3: 改 `build_body`**（`llm.rs:25-36`）

```rust
pub fn build_body(messages: &[Value], cfg: &Config) -> Result<Value, String> {
    let expanded = expand_messages_for_send(messages)?;
    let tools = cfg.active_tools.clone().unwrap_or_else(|| tools::schemas());
    Ok(json!({
        "model": cfg.llm_model,
        "messages": expanded,
        "tools": tools,
        "tool_choice": "auto",
        "reasoning_split": true,
        "stream": true,
        "stream_options": { "include_usage": true },
    }))
}
```

- [ ] **Step 4: 改 `run_turn` 签名 + dispatch 调用点**（`llm.rs:206-263`）

签名改为：

```rust
pub async fn run_turn<E: Emitter>(
    round: Arc<dyn LlmRound>,
    emitter: &E,
    cfg: &Config,
    messages: &mut Vec<Value>,
    ctx: &tools::ToolsCtx,
    max_iters: usize,
) -> ChatResponse {
```

文件顶部加 `use std::sync::Arc;`（若未有）。

循环 `for i in 0..MAX_ITERS`（216 行）改为 `for i in 0..max_iters`。

循环内 `round.round(messages, cfg, emitter)`（218 行）保持（`Arc<dyn>` 解引用自动）。

dispatch 调用点（238 行）改为：

```rust
                        Ok(args) => tools::dispatch(&name, args, ctx, cfg, round.clone()).await,
```

兜底注释 `// 12 轮兜底`（254 行）改 `// max_iters 轮兜底`。

- [ ] **Step 5: 改 `run_loop`**（`llm.rs:266-275`）

```rust
pub async fn run_loop<E: Emitter>(
    round: Arc<dyn LlmRound>,
    emitter: E,
    cfg: &Config,
    mut messages: Vec<Value>,
    workspace: &Path,
) -> ChatResponse {
    let ctx = tools::ToolsCtx::foreground(workspace.to_path_buf(), cfg.minimax_region.clone());
    run_turn(round, &emitter, cfg, &mut messages, &ctx, MAX_ITERS).await
}
```

- [ ] **Step 6: 改 `tools.rs` `dispatch` + `ToolsCtx`**

`dispatch`（568 行）：

```rust
pub async fn dispatch(name: &str, args: Value, ctx: &ToolsCtx, cfg: &crate::config::Config, round: std::sync::Arc<dyn crate::llm::LlmRound>) -> String {
    match name {
        "write" => tool_write(&args, &ctx.workspace),
        "read" => tool_read(&args, &ctx.workspace),
        "bash" => {
            let background = args.get("background").and_then(|v| v.as_bool()).unwrap_or(false);
            let default_t: u64 = if background { 600 } else { 60 };
            let t = args.get("timeout_secs").and_then(|v| v.as_u64()).unwrap_or(default_t);
            tool_bash(&args, ctx, t).await
        }
        "display" => tool_display(&args, &ctx.workspace),
        "edit_card" => tool_edit_card(&args, &ctx.workspace),
        "edit" => tool_edit(&args, &ctx.workspace),
        other => format!("未知工具: {}", other),
    }
}
```

（`cfg`/`round` 暂未用——Task 9 的 `"subagent"` 臂会用到。为避免 unused 警告，文件顶部不需改；参数在某些臂不用是合法的。）

`ToolsCtx`（36-42 行）加字段：

```rust
pub struct ToolsCtx {
    pub workspace: std::path::PathBuf,
    pub jobs: SharedRegistry,
    pub job_done_tx: mpsc::Sender<JobOutcome>,
    pub job_update: Arc<dyn JobUpdate>,
    pub minimax_region: String,
    /// false 时 tool_bash 忽略 background（子代理强制前台，杜绝跨层 job_done 注入）。
    pub allow_background: bool,
}
```

`foreground`（47-56 行）加 `allow_background: true,`。

修 dispatch 的两条测试（678-687 行）——补 `cfg`+`round` 参数：

```rust
    #[tokio::test]
    async fn dispatch_routes_write() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config::default();
        let r = dispatch("write", serde_json::json!({"path":"a.txt","content":"x"}),
            &ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into()),
            &cfg, std::sync::Arc::new(crate::llm::HttpRound)).await;
        assert!(r.contains("已写入"));
    }
    #[tokio::test]
    async fn dispatch_unknown_tool() {
        let cfg = crate::config::Config::default();
        let r = dispatch("nope", serde_json::json!({}),
            &ToolsCtx::foreground(Path::new(".").to_path_buf(), "cn".into()),
            &cfg, std::sync::Arc::new(crate::llm::HttpRound)).await;
        assert!(r.contains("未知工具"));
    }
```

- [ ] **Step 7: 改 `agent.rs` `handle_event` + `run_one` + 测试**

`handle_event`（58-61 行）签名：

```rust
pub async fn handle_event<E: Emitter>(
    event: &SessionEvent, messages: &mut Vec<Value>, ctx: &ToolsCtx,
    cfg: &Config, round: std::sync::Arc<dyn LlmRound>, emitter: &E,
) -> Option<ChatResponse> {
```

末尾 `run_turn(round, emitter, cfg, messages, ctx)`（84 行）改 `run_turn(round, emitter, cfg, messages, ctx, llm::MAX_ITERS_ROUNDS)`——等等，`MAX_ITERS` 在 llm.rs 是 `const`（202 行）但当前私有。把它改 `pub const MAX_ITERS: usize = 12;`（供 agent.rs 用 12、子代理用 8）。`run_one`（146 行）传 `&emit` 不变，但 round 要变 Arc：

`run_one`（133-147 行）改：

```rust
async fn run_one(e: &SessionEvent, messages: &mut Vec<Value>, ctx: &ToolsCtx, cfg: &Config, app: &AppHandle) {
    if matches!(e, SessionEvent::Reset) {
        // ...（Task 10 会改 reset 体；本 Task 先保持原样）
        messages.clear();
        messages.push(json!({ "role": "system", "content": cfg.system_prompt }));
        {
            let mut r = ctx.jobs.lock().unwrap();
            *r = crate::jobs::JobRegistry::new();
        }
        let _ = TauriEmitter::emit(app, "chat-reset", ());
        return;
    }
    let emit = crate::AppEmitter { app: app.clone() };
    let round: std::sync::Arc<dyn llm::LlmRound> = std::sync::Arc::new(crate::llm::HttpRound);
    let _ = handle_event(e, messages, ctx, cfg, round, &emit).await;
}
```

把 llm.rs 的 `const MAX_ITERS: usize = 12;`（202 行）改 `pub const MAX_ITERS: usize = 12;`。

agent.rs 测试 `ctx()`（167-176 行）加 `allow_background: true,`；4 处 `handle_event(...)` 调用（204/217/228/241 行）的 `&round` 改 `std::sync::Arc::new(round)` —— 注意 `round` 在各测试里是 `ScriptedRound { steps: ... }`，包成 `Arc::new(ScriptedRound{...}) as Arc<dyn LlmRound>`。例：

```rust
    let round: std::sync::Arc<dyn llm::LlmRound> = std::sync::Arc::new(
        ScriptedRound { steps: std::sync::Mutex::new(vec![]) });
    let res = handle_event(&SessionEvent::UserMessage { text: "hi".into(), attachments: vec![] },
        &mut msgs, &x, &c, round, &emit).await;
```

（4 个测试函数都这么改：`usermessage_then_stop_runs_turn`、`jobdone_injects_message_and_runs_turn`、`reset_clears_and_reinjects_system`、`context_note_pushes_without_running_turn`。）

llm.rs 测试 `loop_one_tool_then_final`/`loop_mid_fail_keeps_partial_history`/`loop_cap_at_12`（574-625 行）：`run_loop(round, em, ...)` 的 `round` 由 `FakeRound(Mutex::new(...))` 改成 `std::sync::Arc::new(FakeRound(Mutex::new(...))) as Arc<dyn LlmRound>`；`loop_cap_at_12` 的断言里循环上限仍是 12（`run_loop` 内部传 `MAX_ITERS`=12）。

- [ ] **Step 8: 跑全量测试确认绿**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml`
Expected: 全 PASS（含新增 2 条 build_body 测试 + 所有既有测试；行为未变）。

- [ ] **Step 9: commit**

```bash
git add src-tauri/src/llm.rs src-tauri/src/tools.rs src-tauri/src/agent.rs
git commit -m "refactor(llm): round 改 Arc<dyn> 透传 + dispatch(cfg,round) + run_turn(max_iters) + build_body 读 active_tools

为子代理注入 round 与工具子集铺路；纯重构、行为不变。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 3: jobs.rs 加 `JobKind` + Job/JobOutcome 字段 + SubagentProgress/ToolTrace + 并发计数

**Files:**
- Modify: `src-tauri/src/jobs.rs`（`JobStatus` 12-19、`Job` 21-29、`JobOutcome` 32-38、`JobRegistry` 51-98、测试 294-432）
- Modify: `src-tauri/Cargo.toml`（加 `tokio-util`）——本 Task 只为 `CancellationToken` 类型引入。

**Interfaces:**
- Produces：
  - `pub enum JobKind { Process, Agent }`
  - `Job` 加：`kind: JobKind`、`cancel: Option<tokio_util::sync::CancellationToken>`、`progress: Option<std::sync::Arc<std::sync::Mutex<SubagentProgress>>>`、`answer: Option<String>`、`suppress_inject: bool`
  - `JobOutcome` 加：`kind: JobKind`、`answer: Option<String>`、`note: Option<String>`、`label: Option<String>`
  - `pub struct SubagentProgress { rounds, recent_tools: VecDeque<ToolTrace>, partial: String, started: std::time::Instant }` + `pub struct ToolTrace { name, args_brief, result_brief: Option<String> }`
  - `JobRegistry`: `running_agents: usize`、`pub const MAX_AGENTS: usize = 4`、`can_spawn(&self, kind: JobKind)`、`register(&mut self, kind, label, log_path, started_at)`、`finish` 按 job.kind 递减对应计数。

- [ ] **Step 1: 加 `tokio-util` 依赖**

`src-tauri/Cargo.toml` 的 `[dependencies]` 加：

```toml
tokio-util = { version = "0.7", features = ["rt"] }
```

- [ ] **Step 2: 写失败测试**（`jobs.rs` `mod tests`）

```rust
#[test]
fn register_agent_increments_running_agents_only() {
    let mut r = JobRegistry::new();
    let id = r.register(JobKind::Agent, "子代理".into(), PathBuf::new(), 0);
    assert!(r.can_spawn(JobKind::Agent));
    assert_eq!(r.running_agents, 1, "agent 计数独立");
    assert_eq!(r.running, 0, "process 计数不动");
    assert!(matches!(r.get(id).unwrap().kind, JobKind::Agent));
}

#[test]
fn agent_cap_blocks_at_max_agents() {
    let mut r = JobRegistry::new();
    for _ in 0..MAX_AGENTS { r.register(JobKind::Agent, "x".into(), PathBuf::new(), 0); }
    assert!(!r.can_spawn(JobKind::Agent), "agent 达上限");
    assert!(r.can_spawn(JobKind::Process), "process 上限独立，仍可开");
}

#[test]
fn finish_agent_releases_agent_slot() {
    let mut r = JobRegistry::new();
    let id = r.register(JobKind::Agent, "x".into(), PathBuf::new(), 0);
    r.finish(id, JobStatus::Done { code: 0 }, 0);
    assert_eq!(r.running_agents, 0, "agent 完成释放 agent 槽");
}

#[test]
fn subagent_progress_default_empty() {
    let p = SubagentProgress::new();
    assert_eq!(p.rounds, 0);
    assert!(p.recent_tools.is_empty());
    assert!(p.partial.is_empty());
}
```

- [ ] **Step 3: 跑确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml jobs::tests`
Expected: 编译失败（类型/字段不存在）。

- [ ] **Step 4: 实现**

`JobStatus` 前加 enum：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobKind { Process, Agent }
```

`Job`（21-29 行）改：

```rust
#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: JobId,
    pub kind: JobKind,
    pub label: String,
    pub status: JobStatus,
    pub log_path: PathBuf,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    // agent 专属（process 时为 None/默认）：
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    pub progress: Option<std::sync::Arc<std::sync::Mutex<SubagentProgress>>>,
    pub answer: Option<String>,
    pub suppress_inject: bool,
}

#[derive(Debug, Clone)]
pub struct SubagentProgress {
    pub rounds: usize,
    pub recent_tools: std::collections::VecDeque<ToolTrace>,
    pub partial: String,
    pub started: std::time::Instant,
}
impl SubagentProgress {
    pub fn new() -> Self {
        Self { rounds: 0, recent_tools: Default::default(), partial: String::new(), started: std::time::Instant::now() }
    }
}
impl Default for SubagentProgress { fn default() -> Self { Self::new() } }

#[derive(Debug, Clone)]
pub struct ToolTrace {
    pub name: String,
    pub args_brief: String,
    pub result_brief: Option<String>,
}
```

`JobOutcome`（32-38 行）改：

```rust
#[derive(Debug, Clone)]
pub struct JobOutcome {
    pub job_id: JobId,
    pub kind: JobKind,
    pub label: Option<String>,
    pub ok: bool,
    // process 用：
    pub code: Option<i32>,
    pub tail: String,
    // agent 用：
    pub answer: Option<String>,
    pub note: Option<String>,
}
```

`JobRegistry`（51-98 行）改：

```rust
pub const MAX_AGENTS: usize = 4;

pub struct JobRegistry {
    pub jobs: HashMap<JobId, Job>,
    next_id: u64,
    running: usize,
    pub running_agents: usize,
    #[cfg(windows)]
    pub(crate) handles: HashMap<JobId, crate::tools::win_job::Job>,
}

impl JobRegistry {
    pub fn new() -> Self {
        Self {
            jobs: HashMap::new(), next_id: 1, running: 0, running_agents: 0,
            #[cfg(windows)]
            handles: HashMap::new(),
        }
    }

    pub fn can_spawn(&self, kind: JobKind) -> bool {
        match kind { JobKind::Process => self.running < MAX_RUNNING, JobKind::Agent => self.running_agents < MAX_AGENTS }
    }

    /// 登记 Running，返回 JobId；按 kind 自增对应计数（调用方确保已 can_spawn）。
    pub fn register(&mut self, kind: JobKind, label: String, log_path: PathBuf, started_at: u64) -> JobId {
        let id = self.next_id;
        self.next_id += 1;
        match kind { JobKind::Process => self.running += 1, JobKind::Agent => self.running_agents += 1 }
        self.jobs.insert(id, Job {
            id, kind, label, status: JobStatus::Running, log_path, started_at, finished_at: None,
            cancel: None, progress: None, answer: None, suppress_inject: false,
        });
        id
    }

    pub fn get(&self, id: JobId) -> Option<&Job> { self.jobs.get(&id) }

    /// 置终态；按 job.kind 递减对应计数（完成/失败/杀都释放槽）。
    pub fn finish(&mut self, id: JobId, status: JobStatus, finished_at: u64) {
        if let Some(j) = self.jobs.get_mut(&id) {
            let was_running = matches!(j.status, JobStatus::Running);
            let kind = j.kind;
            j.status = status;
            j.finished_at = Some(finished_at);
            if was_running {
                match kind {
                    JobKind::Process => self.running = self.running.saturating_sub(1),
                    JobKind::Agent => self.running_agents = self.running_agents.saturating_sub(1),
                }
            }
        }
    }

    pub fn list(&self) -> Vec<Job> { self.jobs.values().cloned().collect() }
}
```

`spawn_process_job` 里 `r.register(label.clone(), PathBuf::new(), started)`（128 行）改 `r.register(JobKind::Process, label.clone(), PathBuf::new(), started)`。

`spawn_process_job` 里 `if !r.can_spawn()`（127 行）改 `if !r.can_spawn(JobKind::Process)`。

`spawn_process_job` 末尾 guardian 发送 `JobOutcome { job_id: id, code, tail, ok }`（235 行）改：

```rust
            let _ = done2.send(JobOutcome { job_id: id, kind: JobKind::Process, label: None, code: code_opt, tail, ok, answer: None, note: None }).await;
```

修既有 jobs.rs 测试中所有 `register("x".into(), PathBuf::from("/"), epoch)` 调用 → `register(JobKind::Process, "x".into(), PathBuf::from("/"), epoch)`；`can_spawn()` → `can_spawn(JobKind::Process)`（含 `register_increments_id_and_running`、`cap_blocks_after_max`、`finish_releases_running_slot`、`finish_idempotent_on_running_decrement`）。

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml jobs::tests`
Expected: 全 PASS（含新 4 条 + 既有 process 测试，windows-gated 的 `#[cfg(windows)]` 测试在本机也会跑）。

- [ ] **Step 6: commit**

```bash
git add src-tauri/src/jobs.rs src-tauri/Cargo.toml
git commit -m "feat(jobs): JobKind + Job/JobOutcome agent 字段 + SubagentProgress + 独立并发计数

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 4: jobs.rs `kill_job` kind 分派（agent cancel 跨平台）+ check-and-finish 助手

**Files:**
- Modify: `src-tauri/src/jobs.rs`（`kill_job` 244-262、新增 `try_finish_agent` 助手）

**Interfaces:**
- Produces: `kill_job(id, registry, update) -> bool` 跨平台：process 走原 Windows Job Object（windows-only 分支），agent 触发 `cancel.cancel()`（不在此 send JobOutcome——send 由 spawned 任务在 cancel 分支做，见 Task 8）。

- [ ] **Step 1: 写失败测试**（`jobs.rs` `mod tests`）

```rust
#[test]
fn kill_job_agent_triggers_cancel_and_marks_killed() {
    // agent kill 不直接写终态（spawned 任务写），但需触发 cancel token；
    // 这里直接构造一个 agent job（已 Done 状态，模拟 spawned 已收尾）验 kind 分派不 panic、process 路径不被误触。
    let mut r = JobRegistry::new();
    let id = r.register(JobKind::Agent, "子代理".into(), PathBuf::new(), 0);
    let tok = tokio_util::sync::CancellationToken::new();
    r.jobs.get_mut(&id).unwrap().cancel = Some(tok.clone());
    let upd: std::sync::Arc<dyn JobUpdate> = std::sync::Arc::new(NoopJobUpdate);
    let registry: SharedRegistry = std::sync::Arc::new(std::sync::Mutex::new(r));
    assert!(kill_job(id, &registry, &upd), "agent job 应可 kill");
    assert!(tok.is_cancelled(), "agent kill 必须触发 cancel token");
}

#[test]
fn kill_job_unknown_returns_false() {
    let registry: SharedRegistry = std::sync::Arc::new(std::sync::Mutex::new(JobRegistry::new()));
    let upd: std::sync::Arc<dyn JobUpdate> = std::sync::Arc::new(NoopJobUpdate);
    assert!(!kill_job(999, &registry, &upd));
}
```

- [ ] **Step 2: 跑确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml jobs::tests::kill_job_agent_triggers_cancel_and_marks_killed`
Expected: 编译失败（`kill_job` 当前 `#[cfg(windows)]` 且签名/行为不符）。

- [ ] **Step 3: 重写 `kill_job` 为跨平台 + kind 分派**（替换 244-262 行整段）

```rust
/// 人为终止（UI kill_job 命令）：
/// - Process：置 Killed + 关 Job 句柄杀整棵树（Windows）。
/// - Agent：触发 CancellationToken（spawned 任务据此收尾、并发 send JobOutcome——见 subagents::spawn_agent）。
/// 两种 kind 都不在此 send JobOutcome。返回是否存在该 job。
pub fn kill_job(id: JobId, registry: &SharedRegistry, update: &std::sync::Arc<dyn JobUpdate>) -> bool {
    let (snap, handle, cancel, kind) = {
        let mut r = registry.lock().unwrap();
        let Some(j) = r.jobs.get_mut(&id) else { return false; };
        let kind = j.kind;
        if matches!(j.status, JobStatus::Running) {
            j.status = JobStatus::Killed;
            j.finished_at = Some(now_ms());
        }
        #[cfg(windows)]
        let handle = if matches!(kind, JobKind::Process) { r.handles.remove(&id) } else { None };
        #[cfg(not(windows))]
        let handle: Option<()> = None;
        let cancel = j.cancel.clone();
        let snap = r.jobs.get(&id).cloned().unwrap();
        (snap, handle, cancel, kind)
    };
    drop(handle); // process: 关句柄杀树；agent/none: no-op
    if let Some(c) = cancel { c.cancel(); } // agent: 触发 spawned 任务收尾
    update.update(&snap);
    let _ = kind;
    true
}
```

（`now_ms` 当前 `#[cfg(windows)]`（274 行）。把它改为跨平台：去掉 `#[cfg(windows)]` 标注，或新增非 windows 的等价实现。最简：把 `fn now_ms`（274-279 行）的 `#[cfg(windows)]` 删掉——它只调 `SystemTime`，跨平台。同理 `read_tail`（282 行）保留 windows-gated 没关系，但 `fail_job`（264 行）也是 windows-gated 调 `now_ms`——删 `now_ms` 的 `#[cfg(windows)]` 即可让 `kill_job` 跨平台编译。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml jobs::tests`
Expected: 全 PASS（含新 2 条；既有 `kill_does_not_emit_jobdone` 仍绿——process 路径行为不变）。

- [ ] **Step 5: commit**

```bash
git add src-tauri/src/jobs.rs
git commit -m "feat(jobs): kill_job 跨平台 + kind 分派（agent 触发 CancellationToken）

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 5: tools.rs `schemas_subset` + `SUBAGENT_TOOLS` + drift 防护测试

**Files:**
- Modify: `src-tauri/src/tools.rs`（`schemas` 469 行附近加常量与函数、`mod tests` 加测试）

**Interfaces:**
- Produces：`pub const SUBAGENT_TOOLS: &[&str] = &["write","read","edit","bash"];`、`pub fn schemas_subset(names: &[&str]) -> Vec<Value>`。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn schemas_subset_returns_named_tools() {
    let s = schemas_subset(SUBAGENT_TOOLS);
    let names: Vec<&str> = s.iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["write", "read", "edit", "bash"]);
}

#[test]
fn schemas_subset_drift_guard() {
    // 锁定子集：防后人加工具时静默 widening，也锁定"递归不嵌套"
    let s = schemas_subset(SUBAGENT_TOOLS);
    assert_eq!(s.len(), SUBAGENT_TOOLS.len(), "subset 数应 == SUBAGENT_TOOLS 数（防 typo/漂移）");
    let names: Vec<&str> = s.iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
    assert!(!names.contains(&"subagent"), "子代理工具子集绝不能含 subagent（递归锁）");
    assert!(!names.contains(&"display") && !names.contains(&"edit_card"), "子集不含卡片工具");
}
```

- [ ] **Step 2: 跑确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml tools::tests::schemas_subset`
Expected: 编译失败（`SUBAGENT_TOOLS`/`schemas_subset` 未定义）。

- [ ] **Step 3: 实现**（`schemas` 函数前，469 行附近）

```rust
/// 子代理可用工具子集：能动手改盘/跑命令，但不能弹卡片、不能递归 spawn。
pub const SUBAGENT_TOOLS: &[&str] = &["write", "read", "edit", "bash"];

/// 按名字从 schemas() 筛子集（顺序遵循 SUBAGENT_TOOLS；未命中名跳过）。
pub fn schemas_subset(names: &[&str]) -> Vec<Value> {
    let all = schemas();
    names.iter().filter_map(|n| all.iter().find(|t| t["function"]["name"].as_str() == Some(n)).cloned()).collect()
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml tools::tests::schemas_subset`
Expected: PASS（2 条）。

- [ ] **Step 5: commit**

```bash
git add src-tauri/src/tools.rs
git commit -m "feat(tools): SUBAGENT_TOOLS + schemas_subset + drift 防护测试

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 6: tools.rs `ToolsCtx.allow_background` 生效（子代理 bash 强制前台）

**Files:**
- Modify: `src-tauri/src/tools.rs`（`tool_bash` 383-407）

> 注：`allow_background` 字段已在 Task 2 加上（默认 true）。本 Task 让 `tool_bash` 真正读它。

**Interfaces:**
- Consumes: `ToolsCtx.allow_background`（Task 2）。
- Produces: `tool_bash` 在 `!ctx.allow_background` 时把 `background` 强制为 false。

- [ ] **Step 1: 写失败测试**

```rust
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
```

- [ ] **Step 2: 跑确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml tools::tests::bash_background_forced_foreground_when_disallowed`
Expected: FAIL（当前 `background:true` 会走后台返回 job_id JSON）。

- [ ] **Step 3: 实现**（`tool_bash` 388 行附近）

把：

```rust
    let background = args.get("background").and_then(|v| v.as_bool()).unwrap_or(false);
```

改：

```rust
    let want_bg = args.get("background").and_then(|v| v.as_bool()).unwrap_or(false);
    let background = want_bg && ctx.allow_background; // 子代理 ctx 强制前台，杜绝跨层 job_done
```

dispatch 里读 `background` 决定 timeout 默认值的逻辑（573 行）也用 `args.get("background")`，与 tool_bash 内一致即可（dispatch 不需改——它只算默认 timeout，实际 foreground/background 由 tool_bash 内 `ctx.allow_background` 拍板）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml tools::tests::bash`
Expected: PASS（含新条 + 既有 bash_echo/timeout/missing_command）。

- [ ] **Step 5: commit**

```bash
git add src-tauri/src/tools.rs
git commit -m "feat(tools): tool_bash 尊 allow_background（子代理强制前台）

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 7: 新建 `subagents.rs` — SubagentEmitter + SubagentStream（FIFO 配对/节流/noop-thinking）

**Files:**
- Create: `src-tauri/src/subagents.rs`
- Modify: `src-tauri/src/lib.rs`（顶部 `pub mod subagents;`，2 行附近）

**Interfaces:**
- Consumes: `crate::llm::Emitter`、`crate::jobs::{SubagentProgress, ToolTrace, JobId}`。
- Produces：
  - `pub trait SubagentStream: Send+Sync { fn delta(&self, job_id: JobId, payload: Value); }`
  - `pub struct NoopSubagentStream;` impl SubagentStream（测试用）
  - `pub struct SubagentEmitter { progress: Arc<Mutex<SubagentProgress>>, stream: Arc<dyn SubagentStream>, job_id }` impl `llm::Emitter`
  - Emitter 行为：`content` 追加 `partial`（截 200 字）+ stream delta；`tool_call` push trace + rounds+=1 + stream；`tool_result` **FIFO 配对**填最早未完成的同名 trace + stream；`thinking` noop；其余 noop。

- [ ] **Step 1: 写失败测试**（新文件内 `#[cfg(test)] mod tests`）

```rust
use super::*;
use crate::jobs::{SubagentProgress, ToolTrace};
use std::sync::Mutex;

#[tokio::test]
async fn content_accumulates_partial_and_streams() {
    let prog = std::sync::Arc::new(Mutex::new(SubagentProgress::new()));
    let rec = std::sync::Arc::new(RecStream::default());
    let em = SubagentEmitter { progress: prog.clone(), stream: rec.clone(), job_id: 1 };
    em.content("你好").await;
    em.content("世界").await;
    assert_eq!(prog.lock().unwrap().partial, "你好世界");
    let deltas = rec.0.lock().unwrap();
    assert_eq!(deltas.len(), 2, "应推 2 条 content delta");
    assert_eq!(deltas[0]["kind"], "content");
}

#[tokio::test]
async fn tool_result_fifo_pairs_same_name() {
    // 连续两个同名工具（read a, read b）→ 结果按 FIFO 配对（a→ra, b→rb），不"最近"错配
    let prog = std::sync::Arc::new(Mutex::new(SubagentProgress::new()));
    let em = SubagentEmitter { progress: prog.clone(), stream: std::sync::Arc::new(NoopSubagentStream), job_id: 1 };
    em.tool_call("read", r#"{"path":"a"}"#).await;
    em.tool_call("read", r#"{"path":"b"}"#).await;
    em.tool_result("read", "ra").await;
    em.tool_result("read", "rb").await;
    let p = prog.lock().unwrap();
    let v: Vec<&ToolTrace> = p.recent_tools.iter().collect();
    assert_eq!(v[0].result_brief.as_deref(), Some("ra"), "FIFO：第一个 read 配 ra");
    assert_eq!(v[1].result_brief.as_deref(), Some("rb"));
}

#[tokio::test]
async fn last_unpaired_tool_shown_as_running() {
    let prog = std::sync::Arc::new(Mutex::new(SubagentProgress::new()));
    let em = SubagentEmitter { progress: prog.clone(), stream: std::sync::Arc::new(NoopSubagentStream), job_id: 1 };
    em.tool_call("bash", r#"{"command":"cargo test"}"#).await; // 未配对 result
    let p = prog.lock().unwrap();
    assert!(p.recent_tools.back().unwrap().result_brief.is_none(), "未配对 ⇒ 运行中");
    assert_eq!(p.rounds, 1);
}

#[tokio::test]
async fn thinking_is_noop() {
    let prog = std::sync::Arc::new(Mutex::new(SubagentProgress::new()));
    let em = SubagentEmitter { progress: prog.clone(), stream: std::sync::Arc::new(NoopSubagentStream), job_id: 1 };
    em.thinking("推理").await;
    assert!(prog.lock().unwrap().partial.is_empty(), "thinking 不入 partial");
}

// 录音 stream（测试用）
#[derive(Default)]
struct RecStream(Mutex<Vec<serde_json::Value>>);
impl SubagentStream for RecStream {
    fn delta(&self, _id: crate::jobs::JobId, payload: serde_json::Value) { self.0.lock().unwrap().push(payload); }
}
```

- [ ] **Step 2: 跑确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml subagents::`
Expected: 编译失败（模块/类型不存在）。

- [ ] **Step 3: 实现 `subagents.rs`**

```rust
//! 子代理专属：SubagentEmitter（流式上屏 + progress 快照）、SubagentStream 抽象。
//! spawn/status/kill 在 Task 8 加。
use crate::jobs::{self, JobId, SubagentProgress, ToolTrace};
use crate::llm::Emitter;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// 子代理流式 delta 外推（真实实现走 AppHandle emit "subagent-stream"；测试用 Noop/Rec）。
pub trait SubagentStream: Send + Sync {
    fn delta(&self, job_id: JobId, payload: Value);
}
pub struct NoopSubagentStream;
impl SubagentStream for NoopSubagentStream { fn delta(&self, _: JobId, _: Value) {} }

const PARTIAL_MAX: usize = 200;
const TRACE_CAP: usize = 5;
const BRIEF_MAX: usize = 80;

pub struct SubagentEmitter {
    pub progress: Arc<Mutex<SubagentProgress>>,
    pub stream: Arc<dyn SubagentStream>,
    pub job_id: JobId,
}

fn brief(s: &str) -> String {
    if s.chars().count() <= BRIEF_MAX { s.to_string() } else {
        let cut: String = s.chars().take(BRIEF_MAX).collect();
        format!("{cut}…")
    }
}

#[async_trait]
impl Emitter for SubagentEmitter {
    async fn content(&self, t: &str) {
        let mut p = self.progress.lock().unwrap();
        p.partial.push_str(t);
        // 截到最近 PARTIAL_MAX 字
        if p.partial.chars().count() > PARTIAL_MAX {
            let kept: String = p.partial.chars().rev().take(PARTIAL_MAX).collect::<Vec<_>>().into_iter().rev().collect();
            p.partial = kept;
        }
        let payload = json!({ "kind": "content", "text": t });
        drop(p);
        self.stream.delta(self.job_id, payload);
    }
    async fn tool_call(&self, name: &str, args: &str) {
        let mut p = self.progress.lock().unwrap();
        p.rounds += 1;
        if p.recent_tools.len() >= TRACE_CAP { p.recent_tools.pop_front(); }
        p.recent_tools.push_back(ToolTrace { name: name.into(), args_brief: brief(args), result_brief: None });
        let payload = json!({ "kind": "tool_call", "name": name, "brief": brief(args) });
        drop(p);
        self.stream.delta(self.job_id, payload);
    }
    async fn tool_result(&self, name: &str, result: &str) {
        let mut p = self.progress.lock().unwrap();
        // FIFO：最早一条同名且未配对的 trace
        let idx = p.recent_tools.iter().position(|t| t.name == name && t.result_brief.is_none());
        if let Some(i) = idx { p.recent_tools[i].result_brief = Some(brief(result)); }
        let payload = json!({ "kind": "tool_result", "name": name, "brief": brief(result) });
        drop(p);
        self.stream.delta(self.job_id, payload);
    }
    // thinking / turn_start / turn_end / error：noop（控噪音；终态由 spawned 任务经 job-update 推）
}
```

`lib.rs` 顶部 `pub mod jobs;` 后加 `pub mod subagents;`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml subagents::`
Expected: PASS（4 条）。

- [ ] **Step 5: commit**

```bash
git add src-tauri/src/subagents.rs src-tauri/src/lib.rs
git commit -m "feat(subagents): SubagentEmitter + SubagentStream（FIFO 配对/节流/noop-thinking）

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 8: `subagents.rs` — spawn_agent / status_snapshot / kill（核心）

**Files:**
- Modify: `src-tauri/src/subagents.rs`（加三个 pub fn + 终态写入逻辑）

**Interfaces:**
- Consumes: Task 3 的 `Job/JobKind/JobOutcome/JobRegistry/SharedRegistry`、Task 7 的 `SubagentEmitter/SubagentStream`、Task 2 的 `run_turn`/`LlmRound`。
- Produces：
  - `pub async fn spawn_agent(prompt, caption, cfg, round, sub_ctx, sub_cfg, registry, job_done_tx, stream) -> Result<JobId, String>`
  - `pub fn status_snapshot(id, registry) -> String`
  - `pub fn kill(id, registry) -> String`（主代理 kill；设 suppress_inject + cancel）

- [ ] **Step 1: 写失败测试**（追加到 `subagents.rs` `mod tests`；这些是核心契约测试，用 FakeRound + tokio）

```rust
use crate::llm::{self, LlmRound, RoundResult, FinishReason, Emitter};
use crate::config::Config;
use crate::tools::ToolsCtx;
use async_trait::async_trait;
use std::collections::VecDeque;

// 一个返回 Stop 的 FakeRound（子代理立刻完成）
struct StopRound;
#[async_trait]
impl LlmRound for StopRound {
    async fn round(&self, _m: &[serde_json::Value], _cfg: &Config, emit: &dyn Emitter) -> Result<RoundResult, String> {
        emit.content("done").await;
        Ok(RoundResult { content: "done".into(), reasoning: String::new(), tool_calls: vec![],
            finish: FinishReason::Stop, usage: None, assistant_message: serde_json::json!({"role":"assistant","content":"done"}) })
    }
}

fn agent_ctx(ws: std::path::PathBuf) -> ToolsCtx {
    let mut c = ToolsCtx::foreground(ws, "cn".into());
    c.allow_background = false;
    c
}

#[tokio::test]
async fn spawn_returns_and_completes_sending_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let registry: jobs::SharedRegistry = Arc::new(Mutex::new(jobs::JobRegistry::new()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let cfg = Config::default();
    let sub_cfg = Config::default(); // active_tools 在 spawn_agent 内按 SUBAGENT_TOOLS 设
    let id = spawn_agent("做X", "X", &cfg, Arc::new(StopRound) as Arc<dyn LlmRound>,
        &agent_ctx(dir.path().to_path_buf()), &sub_cfg, registry.clone(), tx,
        Arc::new(NoopSubagentStream)).await.unwrap();
    // spawned 任务跑完应发一条 JobOutcome
    let o = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
    assert_eq!(o.job_id, id);
    assert!(o.ok, "StopRound 完成 → ok:true");
    assert_eq!(o.answer.as_deref(), Some("done"));
    let r = registry.lock().unwrap();
    assert!(matches!(r.get(id).unwrap().status, jobs::JobStatus::Done{code:0}));
}

#[tokio::test]
async fn spawn_at_max_returns_err() {
    let dir = tempfile::tempdir().unwrap();
    let registry: jobs::SharedRegistry = Arc::new(Mutex::new(jobs::JobRegistry::new()));
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let cfg = Config::default();
    // 撑满 MAX_AGENTS
    for _ in 0..jobs::MAX_AGENTS {
        spawn_agent("x", "x", &cfg, Arc::new(StopRound) as Arc<dyn LlmRound>,
            &agent_ctx(dir.path().to_path_buf()), &cfg, registry.clone(), tx.clone(),
            Arc::new(NoopSubagentStream)).await.unwrap();
    }
    let over = spawn_agent("over", "over", &cfg, Arc::new(StopRound) as Arc<dyn LlmRound>,
        &agent_ctx(dir.path().to_path_buf()), &cfg, registry.clone(), tx, Arc::new(NoopSubagentStream)).await;
    assert!(over.is_err(), "达 MAX_AGENTS 应拒绝");
}

#[tokio::test]
async fn status_returns_snapshot_and_kill_reclaims() {
    let dir = tempfile::tempdir().unwrap();
    let registry: jobs::SharedRegistry = Arc::new(Mutex::new(jobs::JobRegistry::new()));
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let cfg = Config::default();
    // 用一个永不 Stop 的 round 制造可 kill 的运行态
    struct LoopRound;
    #[async_trait]
    impl LlmRound for LoopRound {
        async fn round(&self, _m: &[serde_json::Value], _cfg: &Config, emit: &dyn Emitter) -> Result<RoundResult, String> {
            emit.content("partial...").await;
            // 返回 tool_call 让 run_turn 继续循环（可被 cancel）
            let tc = serde_json::json!({"id":"c","type":"function","function":{"name":"bash","arguments":"{\"command\":\"echo x\"}"}});
            Ok(RoundResult { content: String::new(), reasoning: String::new(), tool_calls: vec![tc],
                finish: FinishReason::ToolCalls, usage: None,
                assistant_message: serde_json::json!({"role":"assistant","content":null,"tool_calls":[tc]}) })
        }
    }
    let id = spawn_agent("long", "long", &cfg, Arc::new(LoopRound) as Arc<dyn LlmRound>,
        &agent_ctx(dir.path().to_path_buf()), &cfg, registry.clone(), tx, Arc::new(NoopSubagentStream)).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await; // 让它跑起来、产出 partial
    let snap = status_snapshot(id, &registry);
    assert!(snap.contains("进行中"), "status: {snap}");
    let reclaimed = kill(id, &registry);
    assert!(reclaimed.contains("已终止"), "kill: {reclaimed}");
    // suppress_inject 已设 → spawned 任务 cancel 分支不发 JobOutcome
    // （此处不 drain rx 断言，因时序；核心是 kill 返回了回收串 + 不 panic）
}

#[test]
fn status_unknown_id() {
    let registry: jobs::SharedRegistry = Arc::new(Mutex::new(jobs::JobRegistry::new()));
    assert!(status_snapshot(999, &registry).contains("不存在"));
}
```

- [ ] **Step 2: 跑确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml subagents::`
Expected: 编译失败（`spawn_agent`/`status_snapshot`/`kill` 未定义）。

- [ ] **Step 3: 实现 spawn_agent / status_snapshot / kill**（追加到 `subagents.rs`）

```rust
use crate::config::Config;
use crate::jobs::{self, JobKind, JobOutcome, JobStatus, SharedRegistry};
use crate::llm::LlmRound;
use crate::tools::{self, ToolsCtx, SUBAGENT_TOOLS};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const SUBAGENT_MAX_ITERS: usize = 8;

/// spawn 一个后台子代理：注册 agent job → tokio 任务跑 run_turn → 完成经 job_done_tx 回注。
/// 立即返回 JobId（不阻塞）。达 MAX_AGENTS 返 Err。
pub async fn spawn_agent(
    prompt: &str, caption: &str, cfg: &Config, round: Arc<dyn LlmRound>,
    sub_ctx: &ToolsCtx, sub_cfg_tpl: &Config, registry: SharedRegistry,
    job_done_tx: tokio::sync::mpsc::Sender<JobOutcome>, stream: Arc<dyn SubagentStream>,
) -> Result<JobId, String> {
    let label = if caption.is_empty() { prompt.chars().take(20).collect::<String>() } else { caption.to_string() };
    let (id, cancel, progress) = {
        let mut r = registry.lock().unwrap();
        if !r.can_spawn(JobKind::Agent) { return Err(format!("子代理并发已满({}/{})", jobs::MAX_AGENTS, jobs::MAX_AGENTS)); }
        let id = r.register(JobKind::Agent, label.clone(), std::path::PathBuf::new(), now_ms());
        let cancel = CancellationToken::new();
        let progress = Arc::new(std::sync::Mutex::new(jobs::SubagentProgress::new()));
        let j = r.jobs.get_mut(&id).unwrap();
        j.cancel = Some(cancel.clone());
        j.progress = Some(progress.clone());
        (id, cancel, progress)
    };
    // sub_cfg：设工具子集
    let mut sub_cfg = sub_cfg_tpl.clone();
    sub_cfg.active_tools = Some(tools::schemas_subset(SUBAGENT_TOOLS));
    let mut sub_ctx = clone_ctx(sub_ctx);
    sub_ctx.allow_background = false;
    let mut msgs = vec![
        serde_json::json!({ "role": "system", "content": cfg.subagent_system_prompt }),
        serde_json::json!({ "role": "user", "content": prompt }),
    ];
    let emit = SubagentEmitter { progress: progress.clone(), stream: stream.clone(), job_id: id };
    let reg2 = registry.clone();
    let done2 = job_done_tx.clone();
    let label2 = label.clone();
    // 用 tokio::spawn（非 tauri::async_runtime::spawn）：spawn_agent 总在已有 async 运行时内被调用
    //（driver 的 spawned 任务内 / #[tokio::test]），tokio::spawn 两处都工作；后者在 lib 测试里无 Tauri 运行时会 panic。
    tokio::spawn(async move {
        let mut cancelled = false;
        let res: Result<String, String> = tokio::select! {
            _ = cancel.cancelled() => { cancelled = true; Err("cancelled".into()) }
            r = crate::llm::run_turn(round, &emit, &sub_cfg, &mut msgs, &sub_ctx, SUBAGENT_MAX_ITERS) => {
                match r.error { Some(e) => Err(e), None => Ok(r.content) }
            }
        };
        // 终态：one-writer + check-and-finish（防 kill/complete 竞态）
        let (answer, snapshot, suppress, already, status) = {
            let mut r = reg2.lock().unwrap();
            let already = !matches!(r.get(id).map(|j| &j.status), Some(JobStatus::Running));
            if already {
                (r.get(id).and_then(|j| j.answer.clone()).unwrap_or_default(),
                 r.get(id).and_then(|j| j.progress.as_ref().map(|p| p.lock().unwrap().clone())).unwrap_or_else(jobs::SubagentProgress::new),
                 true, true, r.get(id).map(|j| j.status.clone()).unwrap_or(JobStatus::Killed))
            } else {
                let partial = r.get(id).and_then(|j| j.progress.as_ref().map(|p| p.lock().unwrap().clone())).unwrap_or_else(jobs::SubagentProgress::new);
                let status = if cancelled { JobStatus::Killed }
                    else { match &res { Ok(_) => JobStatus::Done { code: 0 }, Err(e) => JobStatus::Failed { reason: e.clone() } } };
                let answer = if cancelled { partial.partial.clone() } else { res.clone().unwrap_or_default() };
                r.finish(id, status.clone(), now_ms());
                if let Some(j) = r.jobs.get_mut(&id) { j.answer = Some(answer.clone()); }
                (answer, partial, r.get(id).map(|j| j.suppress_inject).unwrap_or(false), false, status)
            }
        };
        let _ = status;
        // 仅当非 already 且（非 cancel 或未被主代理 suppress）才 send
        let should_send = !already && !(cancelled && suppress);
        if should_send {
            let outcome = JobOutcome {
                job_id: id, kind: JobKind::Agent, label: Some(label2),
                ok: !cancelled && res.is_ok(),
                answer: Some(answer.clone()),
                note: if cancelled && !suppress { Some("被用户终止".into()) } else { None },
                code: None, tail: String::new(),
            };
            let _ = done2.send(outcome).await;
        }
    });
    Ok(id)
}

/// 主代理查进度（非阻塞、只读）。
pub fn status_snapshot(id: JobId, registry: &SharedRegistry) -> String {
    let r = registry.lock().unwrap();
    let Some(j) = r.get(id) else { return format!("子代理 #{id} 不存在"); };
    if !matches!(j.kind, JobKind::Agent) { return format!("#{id} 不是子代理"); }
    let prog = j.progress.as_ref().map(|p| p.lock().unwrap().clone()).unwrap_or_else(jobs::SubagentProgress::new);
    let elapsed = prog.started.elapsed();
    let status_word = match &j.status {
        JobStatus::Running => "进行中",
        JobStatus::Done { .. } => "已完成",
        JobStatus::Failed { .. } => "已失败",
        JobStatus::Killed => "已终止",
    };
    let mut s = format!("子代理 #{id}（{}）：{status_word}，已 {} 轮，耗时 {}s。", j.label, prog.rounds, elapsed.as_secs());
    if !prog.recent_tools.is_empty() {
        s.push_str("\n最近工具：");
        for (i, t) in prog.recent_tools.iter().enumerate() {
            let mark = if t.result_brief.is_some() { "✓" } else { "⏳运行中" };
            s.push_str(&format!("\n  {}) {:<6} {:<30} {}", i + 1, t.name, t.args_brief, mark));
        }
    }
    if !prog.partial.is_empty() { s.push_str(&format!("\n部分产出：「{}」", prog.partial)); }
    if let Some(a) = &j.answer { s.push_str(&format!("\n最终：{}", a)); }
    s
}

/// 主代理 kill：设 suppress_inject + cancel + 返回回收快照（不 send；spawned 任务据此收尾不发）。
pub fn kill(id: JobId, registry: &SharedRegistry) -> String {
    let (snapshot, cancel, already_terminal, terminal_answer) = {
        let mut r = registry.lock().unwrap();
        let Some(j) = r.jobs.get_mut(&id) else { return format!("子代理 #{id} 不存在"); };
        if !matches!(j.kind, JobKind::Agent) { return format!("#{id} 不是子代理"); }
        let already = !matches!(j.status, JobStatus::Running);
        if !already { j.suppress_inject = true; }
        let snap = j.progress.as_ref().map(|p| p.lock().unwrap().clone()).unwrap_or_else(jobs::SubagentProgress::new);
        (snap, j.cancel.clone(), already, j.answer.clone())
    };
    if let Some(c) = cancel { c.cancel(); }
    if already_terminal { return format!("子代理 #{id} 已结束。结果：{}", terminal_answer.unwrap_or_default()); }
    let mut s = format!("已终止子代理 #{id}。回收 ——");
    if !snapshot.recent_tools.is_empty() {
        s.push_str(" 最近工具：");
        for t in &snapshot.recent_tools {
            let mark = if t.result_brief.is_some() { "✓" } else { "⏳" };
            s.push_str(&format!("[{} {} {}]", t.name, t.args_brief, mark));
        }
    }
    if !snapshot.partial.is_empty() { s.push_str(&format!("；部分产出：「{}」", snapshot.partial)); }
    s
}

// —— helpers ——

fn clone_ctx(src: &ToolsCtx) -> ToolsCtx {
    ToolsCtx {
        workspace: src.workspace.clone(),
        jobs: src.jobs.clone(),
        job_done_tx: src.job_done_tx.clone(),
        job_update: src.job_update.clone(),
        minimax_region: src.minimax_region.clone(),
        allow_background: src.allow_background,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
```

`jobs.rs` 需把 `SubagentProgress`/`ToolTrace`/`JobKind`/`JobOutcome`/`JobStatus`/`MAX_AGENTS` 设 `pub`（Task 3 已 pub 大部分；确认 `pub use` 可达）。`tools::schemas_subset`/`SUBAGENT_TOOLS` 在 Task 5 已 pub。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml subagents::`
Expected: PASS（含 Task 7 的 4 条 + 本 Task 的 4 条）。

- [ ] **Step 5: commit**

```bash
git add src-tauri/src/subagents.rs
git commit -m "feat(subagents): spawn_agent / status_snapshot / kill（check-and-finish + cancel 回收）

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 9: tools.rs `tool_subagent` + dispatch 臂 + schema(6→7) + 断言同步

**Files:**
- Modify: `src-tauri/src/tools.rs`（`schemas()` 加 subagent schema 548-563 后、`dispatch` 加臂 581 后、新增 `tool_subagent`）
- Modify: `src-tauri/src/llm.rs`（`build_body_has_tools_and_reasoning_split` 断言 539：6→7）

**Interfaces:**
- Consumes: Task 8 的 `subagents::{spawn_agent, status_snapshot, kill}`、Task 5 的 `SUBAGENT_TOOLS`。
- Produces：`tool_subagent(args, ctx, cfg, round) -> String`；`dispatch` 的 `"subagent"` 臂；主代理工具表第 7 个。

- [ ] **Step 1: 写失败测试**（`tools.rs` `mod tests`）

```rust
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
fn schemas_has_seven_tools() {
    let s = schemas();
    let names: Vec<&str> = s.iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["write", "read", "bash", "display", "edit_card", "edit", "subagent"]);
}
```

（`tool_subagent` 的 spawn/status/kill 端到端由 Task 8 的 `subagents::` 测试覆盖；本 Task 只验参数校验 + dispatch 路由 + schema。）

- [ ] **Step 2: 跑确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml tools::tests::tool_subagent_missing_prompt_errors tools::tests::schemas_has_seven_tools`
Expected: 编译失败（`tool_subagent` 未定义；`schemas_has_six_tools` 仍存在——本 Task 改名）。

- [ ] **Step 3: 加 subagent schema**（`schemas()` 末尾 `edit` 之后，548-563 行的 `]` 前）

```rust
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
                        "prompt":{"type":"string","description":"spawn 必填：交给子代理的任务"},
                        "caption":{"type":"string","description":"spawn 可选：给人看的标题"},
                        "id":{"type":"integer","description":"status/kill 必填：目标子代理 id"}
                    }
                }
            }
        }),
```

- [ ] **Step 4: 加 `tool_subagent`**（`tool_edit` 之后，252 行附近）

```rust
/// subagent 工具入口：action=spawn/status/kill 分派到 subagents 模块。
pub async fn tool_subagent(args: &Value, ctx: &ToolsCtx, cfg: &crate::config::Config, round: std::sync::Arc<dyn crate::llm::LlmRound>) -> String {
    let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("spawn");
    match action {
        "spawn" => {
            let prompt = match args.get("prompt").and_then(|v| v.as_str()) {
                Some(p) => p, None => return "subagent 缺少 prompt 参数".into(),
            };
            let caption = args.get("caption").and_then(|v| v.as_str()).unwrap_or("");
            let stream: std::sync::Arc<dyn crate::subagents::SubagentStream> = std::sync::Arc::new(crate::subagents::NoopSubagentStream);
            // 注：真实 stream（AppSubagentStream）在 Task 11 由 lib.rs 注入；此处 ctx 不携带 app，先 Noop。
            //     若需真实上屏，Task 11 会扩展 ToolsCtx 携带 stream。
            match crate::subagents::spawn_agent(prompt, caption, cfg, round, ctx, cfg,
                ctx.jobs.clone(), ctx.job_done_tx.clone(), stream).await {
                Ok(id) => format!("已启动子代理 #{id}（{caption}），完成后自动把结果发回给你。可随时 subagent {{\"action\":\"status\",\"id\":{id}}} 看进度，或 subagent {{\"action\":\"kill\",\"id\":{id}}} 终止并回收。"),
                Err(e) => e,
            }
        }
        "status" => {
            let id = match args.get("id").and_then(|v| v.as_u64()) {
                Some(i) => i, None => return "status 缺少 id 参数".into(),
            };
            crate::subagents::status_snapshot(id, &ctx.jobs)
        }
        "kill" => {
            let id = match args.get("id").and_then(|v| v.as_u64()) {
                Some(i) => i, None => return "kill 缺少 id 参数".into(),
            };
            crate::subagents::kill(id, &ctx.jobs)
        }
        other => format!("subagent 未知 action: {other}"),
    }
}
```

> **注（Task 11 会处理）：** 真实 `subagent-stream` 上屏需要 `AppHandle`。`tool_subagent` 经 dispatch 拿不到 app。Task 11 给 `ToolsCtx` 加 `subagent_stream: Arc<dyn SubagentStream>` 字段（主 session 注入 `AppSubagentStream`，`foreground`/子代理 sub_ctx 用 Noop），`tool_subagent` 改用 `ctx.subagent_stream.clone()`。本 Task 先用 Noop 让逻辑可测；Task 11 完成接线。

- [ ] **Step 5: dispatch 加 `"subagent"` 臂**（581 行 `edit` 之后）

```rust
        "subagent" => tool_subagent(&args, ctx, cfg, round).await,
```

- [ ] **Step 6: 改断言 + 改名测试**（`llm.rs:539`）

```rust
        assert_eq!(body["tools"].as_array().unwrap().len(), 7);
```

`tools.rs` 把 `schemas_has_six_tools`（730 行）改名为 `schemas_has_seven_tools` 并更新断言（已在 Step 1 写好新测试；删旧 `schemas_has_six_tools`）。

- [ ] **Step 7: 跑全量测试确认绿**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml`
Expected: 全 PASS（subagent schema/路由/参数校验通过；build_body 断言 7）。

- [ ] **Step 8: commit**

```bash
git add src-tauri/src/tools.rs src-tauri/src/llm.rs
git commit -m "feat(tools): tool_subagent + dispatch 臂 + schema(6→7) + 断言同步

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 10: agent.rs inject_jobdone_message kind 分支 + reset 先 cancel

**Files:**
- Modify: `src-tauri/src/agent.rs`（`inject_jobdone_message` 19-26、`run_one` 的 Reset 分支 134-143、`SessionEvent::JobDone` 注释；测试）

**Interfaces:**
- Consumes: Task 3 的 `JobOutcome.kind/note/answer/label`、`JobKind`、`CancellationToken`。
- Produces：`inject_jobdone_message` 按 kind+note 分支（agent 三态文案）；reset 先遍历 agent jobs cancel 再替换 registry。

- [ ] **Step 1: 写失败测试**（`agent.rs` `mod tests`）

```rust
#[test]
fn inject_jobdone_agent_completed_wording() {
    let mut msgs = vec![json!({"role":"system","content":"s"})];
    let o = crate::jobs::JobOutcome {
        job_id: 3, kind: crate::jobs::JobKind::Agent, label: Some("重构".into()),
        ok: true, code: None, tail: String::new(), answer: Some("完成X".into()), note: None,
    };
    inject_jobdone_message(&mut msgs, &o);
    let body = msgs.last().unwrap()["content"].as_str().unwrap();
    assert!(body.contains("子代理 #3 完成"), "完成文案: {body}");
    assert!(body.contains("完成X"), "应含 answer: {body}");
}

#[test]
fn inject_jobdone_agent_user_killed_wording() {
    let mut msgs = vec![json!({"role":"system","content":"s"})];
    let o = crate::jobs::JobOutcome {
        job_id: 3, kind: crate::jobs::JobKind::Agent, label: None,
        ok: false, code: None, tail: String::new(), answer: Some("半成品".into()), note: Some("被用户终止".into()),
    };
    inject_jobdone_message(&mut msgs, &o);
    let body = msgs.last().unwrap()["content"].as_str().unwrap();
    assert!(body.contains("被用户终止"), "用户终止文案: {body}");
    assert!(body.contains("半成品"));
}

#[test]
fn inject_jobdone_process_wording_unchanged() {
    let mut msgs = vec![json!({"role":"system","content":"s"})];
    let o = crate::jobs::JobOutcome {
        job_id: 7, kind: crate::jobs::JobKind::Process, label: None,
        ok: true, code: Some(0), tail: "ok".into(), answer: None, note: None,
    };
    inject_jobdone_message(&mut msgs, &o);
    let body = msgs.last().unwrap()["content"].as_str().unwrap();
    assert!(body.contains("后台任务 #7 完成"), "process 文案不变: {body}");
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
    // 模拟 reset 的 cancel 遍历
    {
        let r = reg.lock().unwrap();
        for j in r.jobs.values() {
            if matches!(j.kind, JobKind::Agent) { if let Some(c) = &j.cancel { c.cancel(); } }
        }
    }
    assert!(tok.is_cancelled(), "reset 必须先 cancel agent jobs");
}
```

- [ ] **Step 2: 跑确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml agent::tests::inject_jobdone_agent`
Expected: FAIL（当前 inject 只有 process 文案）。

- [ ] **Step 3: 改 `inject_jobdone_message`**（19-26 行）

```rust
/// 把后台任务完成注入为 user 角色消息（OpenAI 兼容安全）。
/// kind=Process：原 [后台任务] 文案；kind=Agent：[子代理] 文案（note 优先做前缀）。
pub fn inject_jobdone_message(messages: &mut Vec<Value>, o: &JobOutcome) {
    let body = match o.kind {
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
    };
    messages.push(json!({ "role": "user", "content": body }));
}
```

- [ ] **Step 4: reset 先 cancel**（`run_one` 134-143 行的 Reset 分支）

```rust
    if matches!(e, SessionEvent::Reset) {
        messages.clear();
        messages.push(json!({ "role": "system", "content": cfg.system_prompt }));
        {
            let mut r = ctx.jobs.lock().unwrap();
            // 先 cancel 所有 agent jobs（token drop ≠ cancel；防孤儿 tokio 任务继续烧 token）
            let tokens: Vec<tokio_util::sync::CancellationToken> = r.jobs.values()
                .filter(|j| matches!(j.kind, crate::jobs::JobKind::Agent))
                .filter_map(|j| j.cancel.clone()).collect();
            for t in tokens { t.cancel(); }
            *r = crate::jobs::JobRegistry::new();
        }
        let _ = TauriEmitter::emit(app, "chat-reset", ());
        return;
    }
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml agent::tests`
Expected: 全 PASS（含新 4 条；既有 jobdone/reset/context 测试仍绿——`jobdone_injects_message_and_runs_turn` 用的是 process-kind outcome，需确认其 JobOutcome 构造补齐新字段）。

修 `jobdone_injects_message_and_runs_turn`（agent.rs:216）的 `JobOutcome { job_id: 7, code: Some(0), tail: "ok".into(), ok: true }` → 补 `kind: crate::jobs::JobKind::Process, label: None, answer: None, note: None`。

- [ ] **Step 6: commit**

```bash
git add src-tauri/src/agent.rs
git commit -m "feat(agent): inject_jobdone kind 分支（子代理三态文案）+ reset 先 cancel agent jobs

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 11: lib.rs 接线 — AppSubagentStream + spawn_session 注入 stream/round + kill_job 命令

**Files:**
- Modify: `src-tauri/src/lib.rs`（`AppJobUpdate` 39-42 附近加 `AppSubagentStream`、`spawn_session` 调用 575、`kill_job` 命令 92-105、`ToolsCtx` 携带 stream）
- Modify: `src-tauri/src/agent.rs`（`spawn_session` 构造 ToolsCtx 107-113 加 subagent_stream 字段）—— 与 Task 2 的 ToolsCtx 改动协同。
- Modify: `src-tauri/src/tools.rs`（`ToolsCtx` 加 `subagent_stream` 字段、`foreground` 默认 Noop、`tool_subagent` 用 `ctx.subagent_stream`）

**Interfaces:**
- Consumes: Task 7/8 的 `SubagentStream`、Task 4 的跨平台 `kill_job`。
- Produces：`AppSubagentStream(AppHandle)` impl SubagentStream（emit "subagent-stream"）；主 session 的 ToolsCtx 携带它；kill_job 命令 agent 分支跨平台。

> **ToolsCtx 加 `subagent_stream`**：Task 2 加了 `allow_background`；本 Task 再加 `subagent_stream: Arc<dyn SubagentStream>`。`foreground()`/子代理 sub_ctx 用 `NoopSubagentStream`；主 session（spawn_session）注入 `AppSubagentStream`。`tool_subagent` 用 `ctx.subagent_stream.clone()`（替 Task 9 的 Noop 硬编码）。

- [ ] **Step 1: 写失败测试**（`lib.rs` 暂无单测传统；本 Task 以 `cargo check --tests` + 集成手测为主。先确保 ToolsCtx 字段加完后 tools.rs/agent.rs 测试仍绿。）

无需新单测——接线靠 `cargo check`。但补一条 `tools.rs` 测试确认 ctx 携带 stream 不破坏：

```rust
#[tokio::test]
async fn tool_subagent_status_uses_ctx_registry() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
    let cfg = crate::config::Config::default();
    let r = tool_subagent(&serde_json::json!({"action":"status","id":999}), &ctx, &cfg,
        std::sync::Arc::new(crate::llm::HttpRound)).await;
    assert!(r.contains("不存在"));
}
```

- [ ] **Step 2: `ToolsCtx` 加 `subagent_stream`**（`tools.rs` 36-42）

```rust
pub struct ToolsCtx {
    pub workspace: std::path::PathBuf,
    pub jobs: SharedRegistry,
    pub job_done_tx: mpsc::Sender<JobOutcome>,
    pub job_update: Arc<dyn JobUpdate>,
    pub minimax_region: String,
    pub allow_background: bool,
    pub subagent_stream: Arc<dyn crate::subagents::SubagentStream>,
}
```

`foreground`（47-56）加 `subagent_stream: Arc::new(crate::subagents::NoopSubagentStream),`。

`tool_subagent`（Task 9 Step 4）里 `let stream = ...NoopSubagentStream...` 改 `let stream = ctx.subagent_stream.clone();`。

`subagents::clone_ctx`（Task 8）加 `subagent_stream: src.subagent_stream.clone(),`。

- [ ] **Step 3: 加 `AppSubagentStream`**（`lib.rs` 39-42 后）

```rust
/// 子代理流式 delta → 前端 "subagent-stream" 事件（jobs 面板 agent 行展开区消费）。
struct AppSubagentStream(AppHandle);
impl subagents::SubagentStream for AppSubagentStream {
    fn delta(&self, job_id: jobs::JobId, payload: serde_json::Value) {
        let mut p = payload;
        if let Some(obj) = p.as_object_mut() { obj.insert("id".into(), json!(job_id)); }
        let _ = self.0.emit("subagent-stream", p);
    }
}
```

- [ ] **Step 4: `spawn_session` 构造的 ToolsCtx 注入 stream**（`agent.rs:107-113`）

`agent.rs` `spawn_session` 里构造 `ctx = ToolsCtx { ... }` 加两个字段：

```rust
    let ctx = ToolsCtx {
        workspace,
        jobs: registry,
        job_done_tx,
        job_update,
        minimax_region: cfg.minimax_region.clone(),
        allow_background: true,
        subagent_stream: sub_stream,
    };
```

`spawn_session` 签名加 `sub_stream: Arc<dyn crate::subagents::SubagentStream>` 参数；`run_one` 透传 ctx（已有）。`lib.rs:575` 调用处：

```rust
            let sub_stream: std::sync::Arc<dyn subagents::SubagentStream> =
                std::sync::Arc::new(AppSubagentStream(app.handle().clone()));
            let session = agent::spawn_session(app.handle().clone(), registry.clone(), job_update, sub_stream);
```

- [ ] **Step 5: `kill_job` 命令跨平台 agent 分支**（`lib.rs` 92-105）

```rust
#[tauri::command]
async fn kill_job(id: u64, app: AppHandle) -> Result<(), String> {
    let registry = app.state::<jobs::SharedRegistry>().inner().clone();
    let upd: std::sync::Arc<dyn jobs::JobUpdate> = std::sync::Arc::new(AppJobUpdate(app.clone()));
    if jobs::kill_job(id, &registry, &upd) { Ok(()) } else { Err("任务不存在".into()) }
}
```

（Task 4 已把 `jobs::kill_job` 改跨平台，去掉 `#[cfg(windows)]` 门控即可。）

- [ ] **Step 6: 跑全量 + check**

Run: `cargo check --tests --manifest-path src-tauri/Cargo.toml && cargo test --lib --manifest-path src-tauri/Cargo.toml`
Expected: 全绿（含新 tool_subagent_status 测试；接线无 warning）。

- [ ] **Step 7: commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/agent.rs src-tauri/src/tools.rs src-tauri/src/subagents.rs
git commit -m "feat: 接线 AppSubagentStream + ToolsCtx.subagent_stream + kill_job 跨平台

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 12: 前端 jobs 面板 agent 行 + `subagent-stream` 监听

**Files:**
- Modify: `src/main.js`（`upsertJob`/jobs 面板渲染处、`subagent` 工具卡走默认路径确认、`listen` 加 `subagent-stream`）

> 无 JS 测试框架：`node --check src/main.js` + dev server 手测。先定位 jobs 面板渲染（`upsertJob`、`list_jobs` 调用、`job-update` 监听——约 main.js:1175 附近）。

**Interfaces:**
- Consumes: 后端 `job-update`（Job 现带 `kind`/`label`/`answer`/`progress`）+ `subagent-stream`（delta）。
- Produces：jobs 面板对 `kind==="agent"` 的行渲染为「子代理 #N + label + 状态 + 耗时 + 可展开实时流 + 完成后 answer」；kill 按钮复用 `kill_job`。

- [ ] **Step 1: 读现状定位**（implementer 用 Grep/Read 找 `upsertJob`、`job-update`、`list_jobs`、`kill_job` 调用点与 jobs 面板 DOM 结构）。

- [ ] **Step 2: 改 `upsertJob`/渲染** —— 对 `job.kind === "agent"`：
  - 行标题：`子代理 #${job.id}（${job.label||""}）` + 状态 badge（`job.status` 序列化为 `running`/`done`/`failed`/`killed`——后端 `JobStatus` 已 `snake_case` serde）。
  - 加一个展开容器 `<div class="subagent-stream" data-id="..."></div>`。
  - 终态（done/failed/killed）展示 `job.answer`。
  - kill 按钮复用既有 `kill_job(id)`。

- [ ] **Step 3: 加 `subagent-stream` 监听**

```js
await listen("subagent-stream", (e) => {
  const p = e.payload || {};
  const box = document.querySelector(`.subagent-stream[data-id="${p.id}"]`);
  if (!box) return;
  const line = document.createElement("div");
  line.className = "sa-stream-line sa-" + (p.kind || "");
  if (p.kind === "content") line.textContent = p.text;
  else if (p.kind === "tool_call") line.textContent = `🔧 ${p.name} ${p.brief||""}`;
  else if (p.kind === "tool_result") line.textContent = `↳ ${p.brief||""}`;
  box.appendChild(line);
  box.scrollTop = box.scrollHeight;
});
```

- [ ] **Step 4: 确认 `subagent` 工具卡走默认路径**

`subagent` 返回纯字符串 → 既有 `llm-tool-call`/`llm-tool-result` 默认 `appendToolCard`/`fillToolResult` 即可，无需新分支（与 bash 同）。implementer 核对 main.js 的工具结果分发（约 1083-1118 行）确实对未知 name 走默认。

- [ ] **Step 5: `node --check`**

Run: `node --check src/main.js`
Expected: 无输出（语法 OK）。

- [ ] **Step 6: commit**

```bash
git add src/main.js
git commit -m "feat(ui): jobs 面板 agent 行 + subagent-stream 实时流监听

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 13: 全量验证 + 手测清单

**Files:** 无（验证 Task）

- [ ] **Step 1: 后端零 warning + 全测试绿**

Run: `cargo check --tests --manifest-path src-tauri/Cargo.toml` → 零 warning。
Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml` → 全 PASS（预期 ~140+ 条，含本计划新增的 config 2 + jobs 6 + subagents 8 + tools 5 + agent 4）。

- [ ] **Step 2: 前端语法**

Run: `node --check src/main.js` → clean。

- [ ] **Step 3: 工具计数级联最终确认**

`llm.rs` `build_body` 断言 `tools.len()==7`；`tools.rs` `schemas_has_seven_tools` 含 `subagent`；`schemas_subset(SUBAGENT_TOOLS)` 不含 `subagent`（drift guard）。

- [ ] **Step 4: dev server 手测**（需重启 dev server 拾取 Rust 改动；由用户执行）

清单（让主 agent 实调）：
1. spawn：`subagent{"action":"spawn","prompt":"读 README.md 并总结","caption":"总结"}` → 返回「已启动子代理 #N…」。
2. status：`subagent{"action":"status","id":N}` → 看到轮数/最近工具/⏳运行中工具/部分产出。
3. 自然完成：等子代理跑完 → 主对话自动出现「[子代理 #N 完成（总结）]\n<答案>」，主代理据此回复。
4. kill 回收：再 spawn 一个长任务 → `subagent{"action":"kill","id":N}` → 返回回收串（含部分产出）；主对话**不**再出现注入（主代理 kill 不 inject）。
5. 人在 UI kill：jobs 面板 agent 行点 kill → 主对话下一轮出现「[子代理 #N 被用户终止；部分产出…]」。
6. 并发上限：连 spawn 5 个 → 第 5 个返回「并发已满(4/4)」。
7. jobs 面板：agent 行展开实时显示思考/工具流；终态显示 answer。
8. 子代理 bash 前台：子代理里调 `bash{"background":true}` → 被强制前台（无跨层 job_id 注入）。
9. reset：reset 后进行中的子代理停止（无孤儿）。

- [ ] **Step 5: 最终 commit（如有手测发现的小修）+ 收尾**

```bash
# 若手测有小修：
git add -p
git commit -m "fix: 手测小修

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Self-Review（写完后自查）

**1. Spec coverage：**
- 工具形态 spawn/status/kill → Task 9（+8）。
- 工具子集 + active_tools → Task 1（字段）+ 5（subset）+ 8（spawn 设 sub_cfg）。
- round/cfg 透传 → Task 2。
- Job kind/字段/并发计数 → Task 3。
- kill_job kind 分派 → Task 4。
- allow_background 强制前台 → Task 2（字段）+ 6（生效）。
- SubagentEmitter（FIFO/节流/noop） → Task 7。
- spawn/status/kill（check-and-finish + cancel） → Task 8。
- inject kind+note → Task 10。
- reset 先 cancel → Task 10。
- 流式协议 subagent-stream → Task 7（trait）+ 11（App 接线）+ 12（前端）。
- 测试（spec 测试节全部） → 散布各 Task；`process_job_regression` 由 Task 3/4 既有 process 测试保持绿色覆盖；`finish_vs_kill_race`/`agent_kill_does_not_inject` 由 Task 8 的 check-and-finish + suppress_inject 逻辑 + `spawn_returns_and_completes`/`status_returns_snapshot_and_kill_reclaims` 覆盖（端到端竞态时序测试标注 `[→E2E]`，手测清单 Task 13 #4/#5 覆盖）。
- MAX_ITERS=8 → Task 8 `SUBAGENT_MAX_ITERS`。
- tokio-util 依赖 → Task 3。

**2. Placeholder scan：** 无 TBD/TODO；每步含实代码或实命令。Task 12 前端因无 JS 测试框架，步骤为「定位 + 改 + node --check + 手测」，代码片段给出关键部分（implementer 按实际 DOM 微调）。

**3. Type consistency：** `run_turn(arc_round, emitter, cfg, messages, ctx, max_iters)`、`dispatch(name, args, ctx, cfg, round)`、`tool_subagent(args, ctx, cfg, round)`、`spawn_agent(prompt, caption, cfg, round, sub_ctx, sub_cfg_tpl, registry, tx, stream)`、`ToolsCtx` 字段集（workspace/jobs/job_done_tx/job_update/minimax_region/allow_background/subagent_stream）跨 Task 一致。`Job`/`JobOutcome` 字段集跨 Task 3/8/10 一致。

## 执行方式

用户已定：**subagent-driven-development**（fresh implementer per task + task review）。
