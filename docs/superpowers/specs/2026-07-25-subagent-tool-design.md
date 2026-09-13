# subagent 工具（后台子代理）设计

> 状态：设计经用户确认 + gstack plan-eng-review（含 outside voice）修订（2026-07-25）。下一步 writing-plans 出实现计划。
> 关联代码：`src-tauri/src/{agent.rs, llm.rs, tools.rs, jobs.rs, lib.rs}` + `src/main.js`。
> 末尾「Review 修订记录」列出 eng-review 阶段相对初版的所有变更。

## 目标

给主 agent 一个 `subagent` 工具，能 spawn **独立的后台子代理**（一个完整的 MiniMax agent loop：全新消息列表 + 子集工具 + 共享 workspace + 自带 Emitter 实时上屏），并在不阻塞主代理的前提下：

1. **追踪进度**——主代理可随时 `status` 取子代理近段输出快照（已跑轮数、最近工具、长耗时工具是否在跑、部分产出）。
2. **主动终止并回收**——主代理可 `kill` 掐掉跑飞/烧 token/等太久的子代理，并**拿回中断时的部分结果**。
3. **自动回注**——自然完成的子代理，结果作为消息注入主代理上下文并触发新一轮。

设计复刻 ovoice 已有的「bash background → jobs」范式：把「一个 Windows 进程」换成「一个独立 agent loop」，复用 `JobRegistry` 的 id/状态/list_jobs/前端面板/`job_done_tx` 回注通道，用 `kind` 判别区分 process 与 agent。

## 决策汇总

| 维度 | 决策 |
|---|---|
| 子代理能力 | 完整子代理：独立 MiniMax agent loop + 工具子集，跑多轮直到 Stop / MAX_ITERS / cancel |
| 子代理工具集 | `{write, read, edit, bash}` —— 不含 `display`/`edit_card`（不向主对话吐卡片）、不含 `subagent`（**递归深度天然锁 1 层**） |
| 结果交付 | 自然完成 = 自动回注（复刻 JobDone 路径）；另支持主动 `status`/`kill` |
| 管理粒度 | 单工具 `subagent` + `action ∈ {spawn, status, kill}`（主代理仍只 +1 工具） |
| 后台生命周期架构 | **方案 C**：复用 `JobRegistry` + `kind` 判别；kill 用新 `CancellationToken`（进程 kill 仍走原 Windows Job Object） |
| 工具集参数化 | **`Config.active_tools` 字段**（None=全量 / Some=子集），`build_body` 读它；**不改 `LlmRound`/`run_turn` 签名** |
| round/cfg 供给子代理 | **经 `dispatch` 透传** `cfg` + `Arc<dyn LlmRound>`（不塞 ToolsCtx）；round 可注入 → 子代理 run_turn 端到端可单测 |
| progress 存储 | **独立 `Arc<Mutex<SubagentProgress>>`**（不锁 registry）；`Job` 只存其 `Arc` 引用 + 终态 answer |
| 工作区 | 子代理共用主代理 `workspace` |
| 子代理 bash | **强制前台**（忽略 `background`），杜绝跨层 `job_done` 注入 |
| 子代理 MAX_ITERS | 调小为 **8**（控 token；主代理仍 12） |
| workspace 并发锁 | **v1 不加**（靠 prompt 约束分工 + 文档提示；per-path 咨询锁列后续） |

## 全局约束（实现须遵守）

- 无打包器 / 前端 UMD-only vanilla JS；无 JS 测试框架（前端 `node --check src/main.js` + 手测）。
- 后端纯逻辑须可离线测：`resolve_path` + `tempfile` + `FakeRound`/`FakeEmitter` 模式（同 display/edit 工具）。
- agent driver 事件驱动、零锁：`messages` 由 driver 私有持有；`build_body` 每轮重发全量 messages。
- **工具计数级联**（[[ovoice-tool-count-cascade]]）：主代理新增 `subagent` → 同步 `llm.rs` 的 `tools.len()` 断言（6→7）与 `tools.rs` 名字表测试。
- dev server 占 exe 时用 `cargo check --tests` / `cargo test --lib`，不要 `cargo build`（[[ovoice-dev-server-cargo-lock]]）。
- 密钥安全：子代理复用主代理 config 里的 MiniMax key，无新增密钥处理；不得打印/泄露 `config.json`。

## 工具形态

单个工具 `subagent`，按 `action` 分派。返回均为纯字符串（同 `bash`/`write`，前端走默认 `appendToolCard`/`fillToolResult`，无新分支）。

```jsonc
{
  "name": "subagent",
  "description": "启动并管理后台子代理（一个独立的迷你 agent，自带 write/read/edit/bash 工具去完成你交办的子任务）。spawn 后不阻塞，完成自动把结果发回给你；status 可查进度/最近工具/长耗时工具；kill 可终止并拿回部分结果。子代理上下文是干净的（不含你的历史）。递归不嵌套。",
  "parameters": {
    "type": "object",
    "required": ["action"],
    "properties": {
      "action": { "type": "string", "enum": ["spawn", "status", "kill"] },
      "prompt":  { "type": "string", "description": "spawn 必填：交给子代理的任务描述（成为其首条 user 消息）" },
      "caption": { "type": "string", "description": "spawn 可选：给人看的简短标题（jobs 面板那行）" },
      "id":      { "type": "integer", "description": "status/kill 必填：目标子代理 id" }
    }
  }
}
```

行为：
- **spawn**：`{prompt, caption?}` → 注册 job（`kind=Agent`）→ `async_runtime::spawn` 独立任务跑子代理 → 立即返回 `"已启动子代理 #N（caption），完成后自动把结果发回给你。可随时 subagent status #N 看进度，或 subagent kill #N 终止并回收。"`。并发满则返回 `"子代理并发已满（4/4），请等某个完成后再 spawn。"`。
- **status**：`{id}` → 读 `Arc<Mutex<SubagentProgress>>` 快照 → 返回紧凑进度串（见「进度快照」）。非阻塞、不改状态。id 不存在/非 agent → 提示。
- **kill**：`{id}` → 锁内**原子 clone** 当前 progress 快照 → 触发 cancel → 返回回收串（见「kill + 回收」）。

## 子代理工具子集与参数化

- `tools.rs`：`pub const SUBAGENT_TOOLS: &[&str] = &["write","read","edit","bash"];` + `pub fn schemas_subset(names: &[&str]) -> Vec<Value>`（按名筛选 `schemas()` 产物）。子代理用 `schemas_subset(SUBAGENT_TOOLS)`。
- **drift 防护测试**：断言 `schemas_subset(SUBAGENT_TOOLS).len() == 4` 且不含 `"subagent"`/`"display"`/`"edit_card"`（防后人加工具时静默 widening，也锁定"递归不嵌套"）。
- **工具集走 cfg 字段（不改 trait 签名）**：`Config` 加 `active_tools: Option<Vec<Value>>`（`None` = 全量 `tools::schemas()`，`Some` = 该子集）。`build_body` 内：`let tools = cfg.active_tools.clone().unwrap_or_else(|| tools::schemas());`。
  - 主 session：`active_tools = None`（全量 7）。
  - 子代理：构造 `sub_cfg` 时 `active_tools = Some(schemas_subset(SUBAGENT_TOOLS))`。
  - **`LlmRound::round` / `run_turn` / `handle_event` 签名零改**（避免 trait + 所有 `FakeRound`/`ScriptedRound` 测试桩的机械 churn）。
- `dispatch` 不改路由（按 name；子代理 schema 不含 display/edit_card/subagent，hallucinate 了返"未知工具"兜底）。

## 后台生命周期（复用 jobs + kind 判别）

### `jobs.rs` 结构变更

```rust
#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobKind { Process, Agent }

pub struct Job {
    pub id: JobId,
    pub kind: JobKind,                       // 新增；现有 process 默认 Process
    pub status: JobStatus,
    pub caption: Option<String>,             // 新增（agent 用；process 可 None）
    pub proc:   Option<现有进程相关字段>,      // 现有 process 专属句柄等，原样不动（agent 时为 None）
    pub cancel: Option<tokio_util::sync::CancellationToken>, // 新增（agent 用）
    pub progress: Option<Arc<Mutex<SubagentProgress>>>,       // 新增（agent 实时快照，独立锁）
    pub answer: Option<String>,              // 新增（agent 最终/回收答案）
}

pub struct SubagentProgress {
    pub rounds: usize,                       // 已观察到的 tool_call 数（近似轮数）
    pub recent_tools: VecDeque<ToolTrace>,   // 最近 ~5 个
    pub partial: String,                     // 最近 ~200 字产出（content 流累积）
    pub started: std::time::Instant,
}
pub struct ToolTrace { pub name: String, pub args_brief: String, pub result_brief: Option<String> }

// JobOutcome 加判别字段（扁平结构 + Option，最小化现有 process 用法改动）：
pub struct JobOutcome {
    pub job_id: JobId,
    pub kind: JobKind,                       // 新增；前端按 kind 渲染 process vs agent
    pub caption: Option<String>,             // 新增
    pub ok: bool,
    // process 用（agent 时为默认值）：
    pub code: Option<i32>,
    pub tail: String,
    // agent 用（process 时为 None）：
    pub answer: Option<String>,              // 最终 or 回收的部分产出
    pub note: Option<String>,                // 注入文案前缀（如「被用户终止」）；process/自然失败为 None
}
```

- 并发计数：`JobRegistry` 新增 `running_agents: usize` + `const MAX_AGENTS: usize = 4`（与现有 `MAX_RUNNING = 8` 独立）。`can_spawn(kind)` / `register(kind, …)` / `finish(kind)` 按 kind 分别增减。
- `kill_job(id)` 加 agent 分支：`kind==Agent` → 取 `cancel` token `.cancel()`（process 分支保持原 Windows Job Object 逻辑不动）。
- `list_jobs` 序列化带上 `kind`/`caption`/`status`/`progress` 摘要/`answer`，供前端按 kind 渲染。

### spawn（`tool_subagent`，在 `tools.rs`；agent loop 跑在独立 tokio 任务）

1. `ctx.jobs.register(kind=Agent, caption)` → 拿 `JobId`，装入新 `CancellationToken`，装入新 `Arc<Mutex<SubagentProgress>>`（progress 独立锁，见「进度快照」）。
2. `tauri::async_runtime::spawn`（不 await；闭包 `move` 捕获 `cfg.clone()`、`round: Arc<dyn LlmRound>`（经 dispatch 透传）、`sub_ctx`、`sub_cfg`、`Arc<progress>`、`cancel`、`job_id`）：
   - 构造子代理 messages：`[ system(subagent_system_prompt), user(prompt) ]`。
   - 构造 `sub_ctx`：复用主 `ctx` 的 `workspace`/`jobs`/`job_done_tx`/`job_update`/`minimax_region`，`allow_background = false`。
   - 构造 `sub_cfg`：`cfg.clone()` + `active_tools = Some(schemas_subset(SUBAGENT_TOOLS))`。
   - 构造 `SubagentEmitter { progress: Arc<progress>, job_update, app, job_id }`（见「实时上屏协议」）。
   - **cancel 接入**：用 `tokio::select!` 包 `run_turn`——
     ```rust
     let res = tokio::select! {
         _ = cancel.cancelled() => Err("cancelled".into()),
         r = llm::run_turn(&*round, &sub_emit, &sub_cfg, &mut sub_messages, &sub_ctx) => r.map(|c| c.content),
     };
     ```
     cancel 命中 → drop `run_turn` future → 中断在飞的 reqwest/child（drop 干净，不损坏 messages）；回收 = 最后一次 `content` 流累积的 `progress.partial`。
   - **终态写入（one-writer rule + 防 kill/complete 竞态）**：**只有这个 spawned 任务写终态**。完成时在**同一把 registry 锁内** check-and-finish（复刻 `jobs.rs:223-236` process 的原子模式）：
     - 锁内读当前 status；若已 `Killed`（killer 先到）→ **不 send、不再 finish**，直接退出。
     - 否则按 `res`：`Ok` → `finish(id, Done, answer=content)` + `job_done_tx.send(JobOutcome{kind:Agent, answer, ok:true, caption, note:None, ..})`；`Err` → `finish(id, Failed{reason})` + `send(JobOutcome{kind:Agent, answer:Some(reason), ok:false, note:None, ..})`。
3. spawn 函数立即返回 `JobId`（格式化为上面的 spawn 文案串）。

### `ToolsCtx` 变更（eng-review 修订：不塞 cfg/round）

`run_turn` 本就单独收 `&Config` 和 round——**不把 cfg/round 塞进 ToolsCtx**（避免双源、避免 Config 含密钥被多层克隆）。改为**经 `dispatch` 透传**：

- `dispatch(name, args, ctx, cfg, round)` 加 `cfg: &Config` + `round: Arc<dyn LlmRound>` 两参（`run_turn` 在 `llm.rs` 调 dispatch 处两者都在作用域，直接传入）。`Arc<dyn LlmRound>` 让 round 可注入：主 session 注入 `Arc::new(HttpRound)`，测试注入 `FakeRound`——子代理 run_turn 端到端可单测。
- `ToolsCtx` 只加一个字段：
```rust
pub struct ToolsCtx {
    pub workspace: PathBuf,
    pub jobs: SharedRegistry,
    pub job_done_tx: mpsc::Sender<JobOutcome>,
    pub job_update: Arc<dyn JobUpdate>,
    pub minimax_region: String,
    pub allow_background: bool,       // 新增：主 session=true，子代理=false
}
```
- `tool_bash` 在 `!ctx.allow_background` 时**忽略 `background:true`**，强制前台（防子代理后台 bash 跨层注入主 driver）。
- `tool_subagent(args, ctx, cfg, round)`：用透传来的 cfg+round 按上文 spawn 子代理。
- 测试桩（`ToolsCtx::foreground`、`agent.rs::ctx()`）加 `allow_background:true`；`dispatch` 测试调用补 `&cfg` + `Arc::new(FakeRound)`。

### 子代理 system prompt

精简默认（放 `config.rs` 字段 `subagent_system_prompt` 带默认值，可配置）：

> 你是子代理。专注完成交给你的单一任务，用 write/read/edit/bash 工具动手。不要闲聊、不要复述任务。完成后给一段简洁的成果总结（结论 + 关键改动/产出），这段总结会作为你的最终答案回传给主代理。

## 进度快照（`status` 返回）

progress 存独立 `Arc<Mutex<SubagentProgress>>`（**不锁 registry**）。`SubagentEmitter` 写它，`status`/`kill` 读它：

- `content(t)` → 追加 `partial`（截到最近 ~200 字）。**节流**：`partial` 在 emitter 内按 ~100ms 或每 N token 才更新一次（避免每 token 都写锁），非节流窗口内只缓冲在 emitter 私有字段。
- `tool_call(name, args)` → push `ToolTrace{name, args_brief(≤80字), result_brief:None}`（cap 5），`rounds += 1`。
- `tool_result(name, result)` → **FIFO 配对**：找**最早一条 `result_brief` 仍 `None` 的同名 trace** 填上（≤80字）。Emitter 签名无 tool_call_id，连续同名工具靠 FIFO 正确配对（"最近一条"会错配）。
- `thinking(_)` → **noop**（控噪音；reasoning 不上屏、不入 partial）。
- **最后一条 trace 的 `result_brief` 仍 `None`** ⇒ 该工具「⏳运行中」（即「长耗时工具的情况」）。

`status` 产出紧凑串，例：
```
子代理 #3（重构 parser）：进行中，已 4 轮，耗时 1m12s。
最近工具：
  1) read  src/parser.rs      ✓
  2) bash  cargo test         ⏳运行中
部分产出：「已定位到 lexer 的 off-by-one，正在补测试…」
```
完成态额外带 `answer` 摘要。

## 实时上屏协议（eng-review 新增：定义流式事件）

现有 `JobUpdate` 是 `fn update(&self, job: &Job)`——fire-and-forget 整 struct，**不是流式 delta**。子代理需要逐 token / 逐工具上屏，故定义独立流：

- 新 Tauri 事件 `subagent-stream`，payload `{ id, kind: "content"|"tool_call"|"tool_result"|"done", text?, name?, brief? }`。
- `SubagentEmitter` 经 `app.emit("subagent-stream", payload)` 直推（不挤占 `job-update`；`job-update` 仍只用于终态/状态变更的整 Job 快照）。
- 前端 jobs 面板的 agent 行监听 `subagent-stream`（按 id 过滤）渲染展开区。
- 终态时也发一条 `subagent-stream {kind:"done"}` + 一次 `job-update`（整 Job，带 answer）。

## kill + 回收

同一「snapshot + cancel + 标记 Killed」机制，两种发起方；**killer 只翻 cancel + 读快照，不写终态**（one-writer：终态只由 spawned 任务写）：

- **主代理主动 kill**（`subagent action=kill`）：锁 `progress` 原子 clone 快照（含 `partial`）→ `cancel.cancel()` → spawned 任务的 select! 命中 → 其 check-and-finish 见 status 未终态则 `finish(Killed)`（**不 send**，因 killer 已把快照返回主代理）→ kill 工具返回串：`"已终止子代理 #N。回收 —— 最近工具：[…]；部分产出：「…」"`。主代理本轮立刻拿到。
  - 防 kill/complete 竞态：若 spawned 任务在 cancel 前**已自然完成并 send**，则 kill 时 status 已 Done/Failed → kill 返回 `"子代理 #N 已完成（或失败），无需终止。结果：<answer>"`（读 `answer`），不重复 send。
- **人在 UI 上 kill**（`kill_job` 命令，`kind==Agent`）：锁内 clone 快照 → `cancel` → spawned 任务 `finish(Killed)` → **send `JobOutcome{kind:Agent, answer:Some(snapshot), ok:false, note:Some("被用户终止")}`** → driver 注入 `"[子代理 #N 被用户终止；部分产出：…]"` → 主代理下一轮看到。
- 自然完成 / 失败：见上文 spawn 终态写入。

## 完成回注（driver，`agent.rs`）

- `SessionEvent::JobDone` 路径不动（driver 的 `select!` 收到后 `inject_jobdone_message` + `run_turn`，已验证会唤醒主代理）。
- `inject_jobdone_message` 按 `o.kind` 分支：
  - `Process` → 原文案不变。
  - `Agent` →（`note` 优先做前缀；无 `note` 按 `ok` 判完成/失败）
    - `ok:true`              → `[子代理 #N 完成（caption）]\n<answer>`
    - `ok:false, note=None`  → `[子代理 #N 失败（caption）]\n<answer 或错误>`
    - `ok:false, note=Some`  → `[子代理 #N {note}；部分产出：\n<answer>]`（用户 kill 走这条，`note="被用户终止"`）
    - 主代理主动 kill **不 send**（kill 工具返回串已交回），故 inject 永不见该情形。
- `kind`（前端渲染）与 `note`（注入文案）服务不同消费者，两者皆留。

## reset（`agent.rs`，**后端**——非前端）

`agent.rs:140` 现状直接 `*r = JobRegistry::new()`。**CancellationToken drop ≠ cancel**（必须显式 `.cancel()`），故 reset 必须：

1. lock 旧 registry；遍历 jobs，对每个 `kind==Agent` 调 `cancel.cancel()`（process jobs 的 Windows Job Object 句柄随 Arc drop 自然清理，维持现状）。
2. **再** `*r = JobRegistry::new()`。

（注：reset 对 process jobs 直接丢 registry → process 孤儿是**既有行为**，不在本 spec 范围，独立 issue。）

## 前端（`src/main.js`）

- `subagent` 工具返回纯字符串 → 走默认 `appendToolCard`/`fillToolResult`（同 bash），**无新分支**。
- jobs 面板（`upsertJob` / `list_jobs` / `job-update` 监听）：
  - `kind === "agent"` 的行渲染为：`子代理 #N + caption + 状态 badge + 耗时`，**可展开**区监听 `subagent-stream`（按 id）实时展示 content/tool 摘要；终态展示 `answer`。
  - kill 按钮复用现有 `kill_job` 调用（后端按 kind 分派）。

## 边界 / 错误处理

- **并发上限**：达 `MAX_AGENTS=4`，spawn 返回「已满」串（不阻塞、不排队）。
- **rate limit**：4 子代理 + 主代理 = 最多 5 路并发 SSE。MiniMax 429 → `round()` Err → 子代理判失败并注入（`run_turn` 无重试）。`MAX_AGENTS=4` 即并发缓解；文档提示「429 会杀掉子代理」。
- **MAX_ITERS**：子代理 8（控 token），到顶按 Stop 处理，返回当前 content。
- **id 不存在 / 非 agent**：status/kill 返回提示串，不 panic。
- **kill/complete 竞态**：spawned 任务终态写入走 check-and-finish（见 spawn），killer 不写终态——杜绝重复注入。
- **cancel 中途**：select! drop run_turn future；回收 = 最后 partial（kill 在 round 中途可能拿到略旧的快照，kill 语义可接受，文档提示）。
- **共享 workspace 并发写**（已知、v1 不加锁）：由 spawn 的 `prompt` 约束分工；文档提示。per-path 咨询锁列后续，**本期不做**（用户确认 v1 不加）。
- **子代理 bash 强制前台**：`ToolsCtx.allow_background`，杜绝跨层注入。

## 测试

后端纯逻辑离线测（tempfile + FakeRound/FakeEmitter，同 display/edit 款）：

- `tool_subagent_missing_prompt_errors`
- `tool_subagent_spawn_returns_string_and_registers_agent_job`
- `tool_subagent_respects_max_agents`（撑满 4 后返「已满」）
- `tool_subagent_status_returns_snapshot`（FakeEmitter 往 progress 灌数据后验快照串含工具名/⏳）
- `tool_subagent_status_unknown_id`
- `tool_subagent_kill_returns_reclaimed_partial`（验 cancel 触发 + 返回串含部分产出 + job 状态 Killed）
- `tool_result_fifo_pairing`（连续同名工具靠 FIFO 正确配对，不错配）
- `long_running_tool_shown_as_running`（tool_call 无配对 tool_result ⇒ ⏳）
- `inject_jobdone_agent_wording`（agent 完成/失败/被用户终止 三种文案 vs process）
- `human_kill_auto_injects`（kill_job(agent) 经 JobOutcome → inject「被用户终止」）
- `build_body_uses_subset_for_subagent`（sub_cfg.active_tools=Some → build_body 收到 4 个工具；主 cfg None → 7 个）
- `schemas_subset_drift_guard`（subset len==4 且不含 subagent/display/edit_card）
- `kill_job_cancels_agent`（注入 token，验 status→Killed）
- `cancel_during_tool_returns_last_partial`（FakeRound 阻塞 + 中途 cancel，验 kill 返回非空 partial）`[→E2E/集成]`
- `finish_vs_kill_race_single_outcome`（ScriptedRound 即将 Stop 时并行 kill，断言 inject 与 kill-return 恰其一，不重复）`[→E2E/集成]`
- `agent_kill_does_not_inject`（主代理 kill 后 timeout-drain `job_done_rx`，断言无 JobOutcome）
- `subagent_failure_sends_ok_false`（run_turn 返 Err → JobOutcome ok:false → 注入失败文案）
- `subagent_run_turn_uses_fake_round_to_completion`（注入 FakeRound，端到端跑完，验 answer 回注）`[→E2E/集成]`
- `reset_session_cancels_agent_jobs`（reset 后 agent job 均 cancel；顺序：cancel 先于 registry 替换）
- `tool_bash_foreground_in_subagent`（`allow_background=false` 时 background 标志被忽略）
- `process_job_still_works_after_kind`（**回归**：kind 改动不破现有 bash background register/finish/kill）

前端：`node --check src/main.js` + 手测（jobs 面板 agent 行展开/实时流/kill；主代理实调 spawn→status→kill→回收；自然完成自动回注）。

## 新依赖

- `tokio-util`（`CancellationToken`）。`Cargo.toml` 加 `tokio-util = { version = "0.7", features = ["rt"] }`。

## 文件改动清单

- `src-tauri/src/tools.rs`：`tool_subagent` + `SUBAGENT_TOOLS` + `schemas_subset`；`dispatch` 加 `"subagent"` 臂 + 加 `cfg`/`round` 参数；`schemas()` 加 subagent schema（6→7）；`ToolsCtx` 加 `allow_background`；`tool_bash` 尊 `allow_background`；测试。
- `src-tauri/src/llm.rs`：`build_body` 读 `cfg.active_tools`；`llm.rs` 的 `tools.len()` 断言 6→7；`run_turn` 调 `dispatch` 处传 `cfg`+`round`。
- `src-tauri/src/jobs.rs`：`JobKind`、`Job`/`JobOutcome` 新字段、`running_agents`/`MAX_AGENTS`、`register/finish/can_spawn/kill_job` 按 kind 分派；`SubagentProgress`/`ToolTrace`；check-and-finish 原子终态；测试。
- `src-tauri/src/agent.rs`：`inject_jobdone_message` 按 kind+note 分支；`spawn_session` 注入 `Arc::new(HttpRound)` 经 dispatch；`reset` 先遍历 agent jobs cancel 再替换 registry；测试桩 `ctx()` 同步。
- `src-tauri/src/lib.rs`：`subagent-stream` 事件透传（SubagentEmitter 用 app.emit）。
- `src/main.js`：jobs 面板 `kind==="agent"` 行渲染 + 展开实时流（监听 `subagent-stream`）+ answer；kill 复用。
- `src-tauri/Cargo.toml`：`tokio-util`。
- `src-tauri/src/config.rs`：`active_tools` 字段 + `subagent_system_prompt` 默认值。

## 不做（YAGNI / v1 外）

- 子代理嵌套（递归 >1 层）——子集不含 `subagent`，天然禁。
- per-path 文件锁 / workspace 隔离（用户确认 v1 不加）。
- 子代理主动 `wait`（阻塞等待）——status 轮询 + 自动回注已够。
- 子代理之间的消息互通 / 结果共享。
- 子代理持久化 / 跨 session 恢复。
- MiniMax 429 重试 / 共享 rate limiter。
- reset 对 process jobs 的孤儿清理（既有行为，独立 issue）。

## Review 修订记录（2026-07-25，gstack plan-eng-review + outside voice）

相对初版的变更：
1. **工具集参数化**：从「贯穿 `run_turn`/`round`/`handle_event` 签名」改为「`Config.active_tools` 字段」——不动 trait，消除 `FakeRound`/`ScriptedRound` 全部测试桩 churn（fork-1）。
2. **round/cfg 供给**：从「ToolsCtx 加 cfg」改为「经 `dispatch` 透传 cfg + `Arc<dyn LlmRound>`」——避免 cfg 双源与密钥多层克隆，round 可注入使子代理 run_turn 端到端可单测（fork-2 + outside-voice #1）。
3. **progress 存储**：从「塞 Job（registry 锁）」改为「独立 `Arc<Mutex<SubagentProgress>>`」——避免每 token 锁 registry，status/kill 不与流写竞争（fork-3 + outside-voice #7）。
4. **kill/complete 竞态**（outside-voice #3，CRITICAL）：spawned 任务终态写入走 check-and-finish 原子（复刻 process 的 `jobs.rs:223-236`），one-writer rule，杜绝 kill 后重复注入。
5. **实时上屏协议**（outside-voice #13）：定义独立 `subagent-stream` 事件（delta payload），不挤占 `job-update`（后者只发整 Job）。
6. **cancel 接入**（outside-voice #5）：明确 `select!` 包 `run_turn`（drop future 中断 reqwest/child），不把 cancel token 塞进 `run_turn` 签名（避免 churn）；回收 = 最后 partial。
7. **tool_result FIFO 配对**（eng-review）：连续同名工具按「最早未完成」配对，非「最近」，避免错配。
8. **reset 顺序**（outside-voice #11/#12）：cancel 必须在 registry 替换**之前**；token drop≠cancel；归属后端 `agent.rs`。
9. **`SUBAGENT_TOOLS` const + drift 测试**（outside-voice #10）：锁子集，防静默 widening。
10. **子代理 MAX_ITERS=8**（eng-review，控 token）。
11. **rate limit 文档化**（outside-voice #6）：429→子代理失败。
12. **`thinking`=noop**（eng-review）。
13. **新增测试**：fifo_pairing、cancel_during_tool、finish_vs_kill_race、agent_kill_does_not_inject、subagent_failure、subagent_run_turn_fake_round、schemas_subset_drift_guard、process_job_regression。
14. **维持**：workspace v1 不加锁（outside-voice #17 挑战，用户确认保持）；方案 C 复用 jobs（outside-voice #8 偏好 B，用户确认保持 C）；`kind`+`note` 皆留（服务不同消费者，outside-voice #2 建议合并，未采纳）。
