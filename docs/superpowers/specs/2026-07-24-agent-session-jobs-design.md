# ovoice 常驻 Agent Session + 后台 Job 系统 设计 (v2, eng-review 修订)

> 日期：2026-07-24 · 分支：`feat/agent-session-jobs` · v2 已并入 `/plan-eng-review` 的 20 项 findings。
> 前序：mmx CLI 可用性见 `memory/mmx-cli-setup.md`；评审报告见 `docs/superpowers/reviews/2026-07-24-agent-session-jobs-eng-review.md`。

## 背景与动机

现状（`llm::run_loop`）：每条用户消息 → `chat(messages)` 整段传入历史 → 跑一个最多 12 轮的**阻塞**工具循环 → 返回 `ChatResponse`。工具 `tools::dispatch(…).await` **同步阻塞**整个循环；bash 工具 30s 硬超时、`kill_on_drop`+JobObject 在返回时即杀进程；无 env 注入。

问题：mmx CLI 的长任务（`mmx video generate` 数分钟、`mmx music generate` 数十秒）会被 30s 超时杀掉，且阻塞循环期间 agent 无法做任何别的事。

目标：让 agent **触发即走**——开一个后台任务拿句柄就收尾，任务完成后由**回调**重新唤醒模型对用户汇报；并为此建立一套**常驻主 agent + 统一 Job 系统 + Jobs 面板**的基础设施，为二期「子 agent」铺路。

## 范围（首期 = 方案 A）

**做：** 常驻 `Session`（事件驱动 driver）+ 进程型后台 `Job`/`JobRegistry` + 完成回调注入 + `bash` 工具增强（前台/后台/env/超时）+ mmx 调优 + 前端（历史后移适配 + Jobs 面板）。

**不做（明确推迟）：** 子 agent（SubAgent 型 Job，二期复用本基础设施）· 跨 app 重启持久化（内存态，退出即清）· 给模型的 job 管理工具（v1 fire-and-forget，取消/查看走面板）· 多会话/多窗口 · FE 测试基建（裸 ES module，走 `/qa` 手测）。

## 架构总览

新增 `agent.rs`、`jobs.rs`；`llm.rs` 保留单轮机制（`LlmRound`/`HttpRound`/`Emitter`/`RoundResult`/`consume_sse`），其 `run_loop` 循环体重构为「每事件一次 turn」（`run_turn`）。`tools.rs` 的 `dispatch` 改收 `ToolsCtx`。

**关键：state 与 driver 所有权分离（review A2）。** Tauri state 只放 `SessionHandle{ tx }`；`messages`+`rx` 由 driver 任务**私有持有**——零共享可变状态、零锁。命令只 `tx.send()`。

```
agent.rs   SessionHandle{tx}（state） + driver 任务（owns messages+rx）+ SessionEvent + run_turn
jobs.rs    Job / JobStatus / JobRegistry（Arc<Mutex>） + spawn_process_job + pipe-drainer + guardian
tools.rs   ToolsCtx + bash（background/timeout_secs/env[allowlist]）；read/write
lib.rs     chat(text) / reset_session / list_jobs / kill_job / read_job_log；注册 Session+JobRegistry
main.js    事件驱动渲染（turn-start/turn-end 界定气泡）+ jobs-view 面板
```

### 数据流：一次「触发即走」的完整链路

```
前端 send ──chat(text)──▶ SessionHandle.tx.send(UserMessage)
                              │
driver (owns messages, rx) ◀──┘  串行排干 inbox（bounded 64）
  │
  ├─ emit chat-turn-start ──▶ 前端新建 assistant 三段气泡
  ├─ run_turn（流式 emit llm-thinking/llm-content/llm-tool-call）
  │     └─ 模型调 bash{background:true}
  │           └─ spawn_process_job ──▶ 立即返回 {job_id,running} 给模型
  ├─ 模型 Stop（"生成中，好了叫你"）
  └─ emit chat-turn-end ──▶ 前端解锁 send、收尾气泡

（后台）guardian: pipe-drainer 持续读 stdout/stderr→log；timeout 看门狗
        └─ 完成 ──▶ tx.send(JobDone) + emit job-update{Done}

driver ◀─ JobDone ── 注入 user 消息 [后台任务 #N 完成] ── run_turn（汇报）── chat-turn-end
```

## 组件详设

### §1 `agent.rs` — 常驻 Session

```rust
pub enum SessionEvent { UserMessage{text}, JobDone{job_id,code:Option<i32>,tail:String,ok:bool}, Reset }

// state 里只放句柄（review A2）
pub struct SessionHandle { tx: mpsc::Sender<SessionEvent> }
// driver 私有持有：
struct DriverState { messages: Vec<Value>, rx: mpsc::Receiver<SessionEvent>, /* cfg/workspace/app */ }
```

- **单实例**（app state），`spawn_session(app)` 在启动时创建 driver 任务并返回 `SessionHandle`。
- **system_prompt 注入（review G1）**：`spawn_session` 创建 driver 时，从 `config::load` 读 `system_prompt`，置 `messages = [{role:"system", content: system_prompt}]`。**首条 UserMessage 之前 system 已在**——避免 persona 静默丢失。
- driver：`while let Some(ev) = rx.recv() { handle_turn(ev).await }` —— **串行**，天然解决重入。
- **channel 背压（review G3）**：`mpsc::channel(64)` bounded；`tx.send().await` 满时等待（driver 串行，正常不会满）；driver 死亡 → `send` 返 Err。
- `handle_turn`：`UserMessage`→push user；`JobDone`→push 注入消息；`Reset`→清空 messages **并清 JobRegistry**（review G4）后重注入 system，continue。然后 `run_turn`。
- **run_turn**：每事件开头 **emit `chat-turn-start`**（前端据此新建气泡，review G1），复用 `HttpRound`+`AppEmitter` 跑最多 `MAX_ITERS=12` 轮：
  - `ToolCalls` → 逐个执行；background bash 立即返回 job_id 不阻塞；foreground bash 阻塞。
  - `Stop`/上限 → 结束，**emit `chat-turn-end`**。
  - round 出错 → **emit `chat-error{message}`**，结束 turn（永不 panic，保留 partial history）。

### §2 `jobs.rs` — Job 系统与进程型 Job

```rust
pub type JobId = u64;
pub enum JobStatus { Running, Done{code:i32}, Failed{reason:String}, Killed }
pub struct Job { id, label, status, log_path, started_at:u64, finished_at:Option<u64>, _job_handle: Option<win_job::Job> }
pub struct JobRegistry { jobs: HashMap<JobId, Job>, next_id: u64, /* running 计数 */ }  // Arc<Mutex>
const MAX_RUNNING: usize = 8;   // review A3
```

- `spawn_process_job(cmd, env, cwd, timeout, label, tx, app)`：
  - 沿用 `tool_bash` spawn 方式（`cmd /C chcp 65001>nul && {cmd}`、`current_dir(workspace)`、`stdin(null)`、stdout/stderr **piped**），**关掉 `kill_on_drop`**。
  - **Job Object assign 失败 → 判 Failed 不开 job（review A4）**：assign 不再静默 no-op；失败直接返回错。
  - **Job Object 句柄存进 `Job._job_handle`（review G2）**：活过 `child.wait()`，registry 持有；app 退出时句柄关闭 → `KILL_ON_JOB_CLOSE` 杀树（无孤儿）。
  - **专职 pipe-drainer 任务（review A1，P1）**：持续读 stdout/stderr → 追加写 `workspace/.ovoice-jobs/<id>.log`。**不可不 drain**——缓冲区(~4KB)满则子进程 write 阻塞、假死锁。
  - **guardian 任务**：`tokio::time::timeout(timeout, child.wait())`：
    - 完成 → 读 tail(~4KB) → **先查 `job.status`，若已 `Killed` 则不发 JobDone（review G2 kill 竞态）**；否则 `tx.send(JobDone{ok:true,…})` + `job-update{Done}`。
    - 超时 → Job Object 杀树 → `JobDone{ok:false}` + `job-update{Failed}`。
  - **并发上限（review A3）**：running 计数 ≥ `MAX_RUNNING`(8) → 不开，返回「已达并发上限(8)」。完成/失败/杀后释放计数；保留 Job 元数据供面板历史，重句柄随进程退出而释。
  - 返回 `JobId`。
- `kill_job(id)`：Job Object 关句柄杀树；置 `Killed`；emit `job-update`；**不发 JobDone**（guardian 见 Killed 也不再发）。
- `list_jobs()` / `get_job(id)`：面板快照。

### §3 `tools.rs` — bash 工具增强 + ToolsCtx

```rust
pub struct ToolsCtx { workspace: PathBuf, jobs: Arc<Mutex<JobRegistry>>, session_tx: mpsc::Sender<SessionEvent>, app: AppHandle, cfg: Config }
pub async fn dispatch(name: &str, args: Value, ctx: &ToolsCtx) -> String  // review C1：收 ctx 而非散参
```

`bash` schema 新增可选参：

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `command` | string | 必填 | cmd 命令 |
| `background` | bool | false | true→后台 Job 立即返 job_id；false→前台阻塞 |
| `timeout_secs` | u64 | 前台 60 / 后台 600 | 超时杀进程 |
| `env` | object{str:str} | {} | **仅允许 `MINIMAX_*`/`MMX_*` 前缀**（review C3 allowlist），其余忽略 |

- 前台：现行行为，返回 `退出码 N\n<body>`（20KB 截断），超时默认 60s。env 基线 `MINIMAX_REGION=<cfg.minimax_region>` 叠加 per-call allowlisted env。
- 后台：`spawn_process_job(…)` 立即返回 `{"job_id":N,"status":"running","log":".ovoice-jobs/N.log"}`。

### §4 完成回调消息格式

`JobDone` → driver 注入 `user` 角色消息（OpenAI 兼容安全）：成功 `[后台任务 #N 完成] 退出码 0\n<tail>`；失败 `[后台任务 #N 失败] <reason>\n<tail>`。多个 JobDone 串行排队，逐个触发独立 turn。`kill_job` 不触发（人为终止不唤醒）。

### §5 mmx 调优

- `config.minimax_region: String`（默认 `"cn"`），bash env 基线注入。（mmx 鉴权独立于 `config.api_key`——走 `~/.mmx/config.json`，已预置。）
- 超时分层：前台 60s / 后台 600s，可 `timeout_secs` 覆盖。
- 重写 `bash` description：mmx 全局可用、region 已预设、媒体走 `--out`/`minimax-output/`、**长媒体(video/music)用 `background:true`**。
- **不教 `--async`+轮询（review G2）**：v1 无轮询原语；视频就 `mmx video generate …`（同步）当后台 job 跑，job 等它完成即可。`--async`+poll 留二期。

### §6 前端（`main.js` / `index.html` / `styles.css`）

**历史后移 + turn 边界（review G1）：**
- `chat(messages)` → `chat(text)`；移除前端 `messages` 数组与 `res.history` 回填；纯事件渲染。
- **气泡边界靠事件**：监听 `chat-turn-start`（新建 assistant 三段气泡）/ `llm-content`（append 到当前气泡）/ `chat-turn-end`（收尾+解锁 send）/ `chat-error`（显错+解锁 send）。解决多 turn/JobDone 交错串气泡。
- send 门控：发送时 `chatBusy=true`，`chat-turn-end`/`chat-error` 时 false。
- `save_config` 成功 → 后端 `reset_session`（清 history+registry，重注入 system）。

**Jobs 面板（新 `jobs-view`，与 chat/settings 并列切换）：**
- 行：`#id 标签` + 状态 chip + 耗时 + 输出 tail + 「终止」「查看完整日志」。
- `job-update` 增量更新；打开时 `list_jobs` 拉快照。「终止」→ `kill_job(id)`；「查看完整」→ `read_job_log(id)`。

### §7 历史窗口化（review P1）

每轮 run_turn 前，若 `messages` > 40 条或总文本 > 阈值，**截断/折叠旧 tool 结果**（替换为 `[旧工具结果已省略]`），控制单轮 API token + 内存（MiniMax completion ~90% 是 reasoning，长会话成本否则炸裂）。完整 summarization 进 TODO。

## 命令 / 事件契约

**命令：**
| 命令 | 签名 | 说明 |
|---|---|---|
| `chat` | `async fn chat(text, app) -> Result<(),String>` | `tx.send(UserMessage).await`；**send 失败(driver 死)返 Err（review C2）** |
| `reset_session` | `fn reset_session(app) -> Result<(),String>` | 清 messages+**JobRegistry**（G4），重注入 system |
| `list_jobs` | `fn list_jobs(app) -> Vec<Job>` | 面板快照 |
| `kill_job` | `async fn kill_job(id, app) -> Result<(),String>` | 终止后台 Job（不回调） |
| `read_job_log` | `fn read_job_log(id, app) -> Result<String,String>` | 读日志，**套 READ_MAX(50KB) 截断（review G4）** |
| 既有 | `speak`/`get_config`/`save_config`/`asr_one_shot` | 不变 |

**事件：**
| 事件 | 载荷 | 说明 |
|---|---|---|
| `chat-turn-start` | `{}` | **新**（G1）：turn 开始，前端新建气泡 |
| `llm-thinking`/`llm-content`/`llm-tool-call`/`llm-tool-result` | 同现状 | 单轮内增量 |
| `chat-turn-end` | `{}` | turn 结束 → 收尾+解锁 send |
| `chat-error` | `{message}` | 本轮出错 |
| `job-update` | `{id,label,status,code?,tail?,started_at,finished_at?}` | Job 状态 → 面板 |

## ASCII 图（review C5）

**Job 状态机：**
```
                 spawn
                   │
                   ▼
              ┌─────────┐  超时/进程失败(*)   ┌────────┐
              │ Running │────────────────────▶│ Failed │ ──▶ JobDone(ok:false)+job-update
              └────┬────┘                     └────────┘
        wait 完成   │  正常 exit(*)
                   ▼  ┌──────┐
              ┌──────────┐ ──▶ JobDone(ok:true)+job-update
              │   Done   │
              └──────────┘
        kill_job
          │
          ▼
      ┌─────────┐  ──▶ job-update（不发 JobDone；guardian 见 Killed 也不再发）
      │ Killed  │
      └─────────┘
 (*) guardian emit 前先查 status；若 Killed 则静默不发。
```

**Session 事件流：**
```
UserMessage ─┐                       ┌─ chat-turn-start
JobDone ─────┼─▶ driver(inbox, 串行) ─┼─ run_turn(MAX 12 轮) ─ emit llm-* ─▶ chat-turn-end
Reset ───────┘   (messages 私有)      └─ 出错 ─▶ chat-error
```

## 测试策略（TDD、可注入、离线）

- **Session driver**（`FakeRound` + 假 registry）：① UserMessage→bg tool_call→spawn+返 job_id+Stop+turn-start/end；② JobDone→注入消息+新一轮；③ 重入串行；④ error→chat-error。**+ 新增**：⑤ `Reset` 清 messages+registry（G4）；⑥ chat `tx.send` 失败返 Err（C2）；⑦ 两 JobDone 并发完成串行有序。
- **Job 系统**：spawn echo→Done；超时→Failed；kill→Killed 不回调。**+ 新增（review T1/T2）**：⑧ **pipe-drain 正确性**——起写 >4KB stdout 的进程，断言正常完成不阻塞（backs A1，关键）；⑨ assign 失败→Failed（A4）；⑩ registry 超限(MAX_RUNNING)→拒绝（A3）。
- **tools**：前台/后台分支；**env allowlist** 拒绝非 `MINIMAX_*/MMX_*` 键（C3）。
- **回归（铁律，强制）**：现有 `run_loop`(MAX_ITERS/partial-history)、`tool_bash`(echo/timeout)、`consume_sse`/`ToolCallAccum`/`resolve_path`/`truncate` 单测重构后保持绿。
- **[→E2E 手测清单，给 /qa]**：完整视频生成链、chat-turn-start/end 气泡边界、前台 mmx chat 阻塞+流式、Jobs 面板实时更新、面板终止、关 app 杀 job 无孤儿。

## 风险与权衡

- 历史后移触现有 chat 链路（前端 `messages`、`chat` 签名、事件路由）——以 `chat-turn-start/end`+`chat-error` 契约收口。
- 后台无界增长：MAX_RUNNING(8) + 面板可终止；完成 job 释重句柄、留元数据。
- 额度：后台+全自动=无人值守烧 mmx quota；v1 不加确认门，面板可见可终止兜底（昂贵命令确认开关留 TODO）。
- stdout 编码：chcp 65001→UTF-8，`decode_output` 兜底；tail 取末 ~4KB 按 char 边界截断（复用 `truncate`）。
- GUI PATH：已验证 spawn 路径能解析 mmx；个别机器 nodejs 不在系统 PATH 需文档提示。

## 决策记录（含 eng-review 20 项）

范围 A（全量含面板）/ 不持久化 / 历史后移 chat(text) / fire-and-forget+面板 / 超时 60·600 / 完成回调走 user 消息 — ✅
**架构**：A1 pipe-drain 显式 · A2 SessionHandle 拆分 · A3 并发上限 8 · A4 assign 失败硬校验 — ✅
**代码质量**：C1 ToolsCtx · C2 chat send 失败 · C3 env allowlist · C5 ASCII×2 — ✅（C4 mmx 认证独立：文档澄清）
**测试**：T1 pipe-drain · T2 边界 unit 组 · T4 FE 冒烟清单（+回归铁律强制）— ✅
**性能**：P1 历史窗口化（P2 日志清理→TODO）— ✅
**Outside Voice**：G1 turn 边界事件+system 注入 · G2 kill/guardian cancel+去 --async+句柄存 registry · G3 session_tx bounded(64) · G4 Reset 清 registry+read_job_log READ_MAX — ✅
