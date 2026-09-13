# 常驻 Agent Session + 后台 Job 系统 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 ovoice 的 LLM agent 能把 mmx 魔法般的长任务（视频/音乐生成）丢到后台、触发即走，完成后回调唤醒模型汇报；为此建立常驻事件驱动 Session + 进程型 Job 系统 + Jobs 面板。

**Architecture:** 后端新增 `agent.rs`（常驻 Session：state 只放 `SessionHandle{tx}`，driver 任务私有持有 `messages`+`rx`，串行排干 `mpsc` inbox）和 `jobs.rs`（`JobRegistry` + 进程型后台 job：专职 pipe-drainer 防 stdout 死锁、Job Object 句柄入 registry、guardian 完成回调 emit 前查 Killed）。`llm.rs` 的 `run_loop` 循环体重构为每事件一次的 `run_turn`（复用 `LlmRound`/`Emitter`/`HttpRound`/`consume_sse`）。`tools.rs` 的 `dispatch` 改收 `ToolsCtx`，`bash` 加 `background`/`timeout_secs`/`env`(allowlist)。前端 `chat(text)` + 事件驱动渲染（`chat-turn-start/end` 界定气泡）+ Jobs 面板。

**Tech Stack:** Tauri v2 · Rust (edition 2021, tokio, reqwest stream, windows-sys JobObjects) · 前端裸 ES module（无 bundler）· MiniMax OpenAI 兼容 API（`reasoning_split`+stream）· 测试 `cargo test`（inline `#[cfg(test)]`，`tokio::test`，`FakeRound`/`FakeEmitter` 注入）。

## Global Constraints

- Tauri v2：`withGlobalTauri:true`、window `"main"`、`frontendDist="../src"`、**CSP=null**。
- **改前端（src/*.html/js/css）必须重新 `cargo build`**——Tauri 在 build 时把 `../src` 编进 exe（`generate_context!`），只重启旧 exe 看不到前端改动。
- **改 Rust 前 `taskkill //F //IM ovoice.exe`**（exe 被占用 → `cargo build` 报「拒绝访问」）。
- Windows bash：`cmd /C chcp 65001>nul && {cmd}`、`stdin(Stdio::null())`、Job Object `KILL_ON_JOB_CLOSE`、`tokio::process`。
- 后台 job：`kill_on_drop=false`（句柄搬进 registry）；stdout/stderr **必须 pipe-drain**（缓冲~4KB 满则死锁）；Job Object 句柄存 `Job._job_handle` 活过 `child.wait()`；assign 失败 → 判 Failed 不开 job。
- 数值常量：`MAX_ITERS=12`（单 turn 内工具轮次）· `MAX_RUNNING=8`（并发后台 job）· `session_tx` bounded `64` · 超时 前台 `60`s / 后台 `600`s · `READ_MAX=50*1024` · `BASH_MAX=20*1024`。
- env allowlist：仅 `MINIMAX_*`/`MMX_*` 前缀；基线固定注入 `MINIMAX_REGION=<config.minimax_region>`（默认 `"cn"`）。
- 完成回调：注入 `user` 角色 `[后台任务 #N 完成/失败]…`；`kill_job` 不触发回调（guardian emit 前查 `Killed`）。
- **不持久化**：Session + JobRegistry 内存态，退出即清。
- 历史窗口化：每轮前若 messages>40 条，折叠旧 tool 结果为 `[旧工具结果已省略]`。
- `src-tauri/.env`（MINIMAX_API_KEY）**禁止提交**（已 gitignore）；`config.json` 用户已改（speed:1.5）勿还原。
- commit message 以 `Co-Authored-By: Claude <noreply@anthropic.com>` 结尾。
- 现有单测（`run_loop`/`tool_bash`/`consume_sse`/`ToolCallAccum`/`resolve_path`/`truncate`）重构后必须保持绿（回归铁律）。

---

## File Structure

| 文件 | 职责 | 动作 |
|---|---|---|
| `src-tauri/src/config.rs` | `Config` 加 `minimax_region` 字段 | Modify |
| `src-tauri/src/jobs.rs` | `Job`/`JobStatus`/`JobRegistry`/`JobOutcome`/`JobUpdate` + `spawn_process_job` + `kill_job` | Create |
| `src-tauri/src/tools.rs` | `ToolsCtx` + `dispatch(ctx)` + `bash` background/env allowlist + schema | Modify |
| `src-tauri/src/llm.rs` | `Emitter` 扩 `turn_start/turn_end/error`；`run_loop` 体抽成 `run_turn` | Modify |
| `src-tauri/src/agent.rs` | `SessionEvent`/`SessionHandle`/`run_turn`/`spawn_session`/driver/windowing | Create |
| `src-tauri/src/lib.rs` | `AppEmitter` 覆写新事件 + `JobUpdate` impl + 注册 state + 命令 + `invoke_handler` | Modify |
| `src/main.js` | `chat(text)` + 去 `messages` + turn 事件 + jobs-view 渲染 | Modify |
| `src/index.html` | `jobs-view` + 顶栏「任务」按钮 | Modify |
| `src/styles.css` | jobs 面板样式 | Modify |

依赖顺序（compile-safe）：config → jobs(types) → jobs(spawn) → jobs(kill) → tools(ctx) → llm(Emitter+run_turn) → agent(run_turn+session) → lib(接线) → frontend。

---

### Task 1: config 加 minimax_region

**Files:**
- Modify: `src-tauri/src/config.rs`

**Interfaces:**
- Produces: `Config.minimax_region: String`（serde 默认 `"cn"`），供 ToolsCtx 注入 bash env 基线。

- [ ] **Step 1: 加字段（带测试）**

在 `config.rs` 的 `Config` struct 里加字段（找现有 `pub workspace_dir` 同级，serde `#[serde(default)]` 保持向后兼容）：

```rust
#[serde(default = "default_region")]
pub minimax_region: String,
```
并在 `default_voices()` 同级加：
```rust
fn default_region() -> String { "cn".into() }
```

在 `config.rs` 的 `#[cfg(test)] mod tests` 末尾加：
```rust
#[test]
fn minimax_region_defaults_to_cn() {
    let j = serde_json::json!({});
    let c: Config = serde_json::from_value(j).unwrap();
    assert_eq!(c.minimax_region, "cn");
}
```

- [ ] **Step 2: 跑测试**

Run: `cd src-tauri && cargo test --lib config::tests::minimax_region_defaults_to_cn`
Expected: PASS。

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/config.rs
git commit -m "feat(config): minimax_region 字段(默认 cn)" -m "Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 2: jobs.rs 类型骨架

**Files:**
- Create: `src-tauri/src/jobs.rs`
- Modify: `src-tauri/src/lib.rs`（加 `pub mod jobs;`）

**Interfaces:**
- Produces: `JobId=u64`、`enum JobStatus`、`struct Job`、`struct JobRegistry`、`struct JobOutcome`、`trait JobUpdate`、`const MAX_RUNNING: usize=8`。

- [ ] **Step 1: 写 jobs.rs（含 registry 单测）**

```rust
//! 后台 Job 系统：进程型 Job + JobRegistry。
//! 独立于 agent.rs —— 完成信号走 mpsc::Sender<JobOutcome>，状态推送走 JobUpdate trait。
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub type JobId = u64;
pub const MAX_RUNNING: usize = 8;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    Done { code: i32 },
    Failed { reason: String },
    Killed,
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: JobId,
    pub label: String,
    pub status: JobStatus,
    pub log_path: PathBuf,
    pub started_at: u64,           // epoch ms
    pub finished_at: Option<u64>,
}

/// guardian 完成时投递给 driver 的结果（driver 据此注入 [后台任务 #N 完成]）。
#[derive(Debug, Clone)]
pub struct JobOutcome {
    pub job_id: JobId,
    pub code: Option<i32>,
    pub tail: String,
    pub ok: bool,
}

/// Job 状态变化的外推抽象（真实实现走 AppHandle emit job-update；测试用录音实现）。
pub trait JobUpdate: Send + Sync {
    fn update(&self, job: &Job);
}

/// 不做事的 JobUpdate（lib.rs 接线前的占位 + 测试默认）。
pub struct NoopJobUpdate;
impl JobUpdate for NoopJobUpdate {
    fn update(&self, _job: &Job) {}
}

pub struct JobRegistry {
    pub jobs: HashMap<JobId, Job>,
    next_id: u64,
    /// 运行中 job 计数（用于 MAX_RUNNING 上限）。
    running: usize,
}

impl JobRegistry {
    pub fn new() -> Self { Self { jobs: HashMap::new(), next_id: 1, running: 0 } }

    /// 达上限返回 false（A3）。
    pub fn can_spawn(&self) -> bool { self.running < MAX_RUNNING }

    /// 登记 Running，返回 JobId；内部自增计数（调用方确保已 can_spawn）。
    pub fn register(&mut self, label: String, log_path: PathBuf, started_at: u64) -> JobId {
        let id = self.next_id;
        self.next_id += 1;
        self.running += 1;
        self.jobs.insert(id, Job {
            id, label, status: JobStatus::Running, log_path, started_at, finished_at: None,
        });
        id
    }

    pub fn get(&self, id: JobId) -> Option<&Job> { self.jobs.get(&id) }

    /// 置终态；若从 Running 转出则递减 running 计数（完成/失败/杀都释放并发槽）。
    pub fn finish(&mut self, id: JobId, status: JobStatus, finished_at: u64) {
        if let Some(j) = self.jobs.get_mut(&id) {
            let was_running = matches!(j.status, JobStatus::Running);
            j.status = status;
            j.finished_at = Some(finished_at);
            if was_running { self.running = self.running.saturating_sub(1); }
        }
    }

    pub fn list(&self) -> Vec<Job> { self.jobs.values().cloned().collect() }
}

impl Default for JobRegistry { fn default() -> Self { Self::new() } }

pub type SharedRegistry = Arc<Mutex<JobRegistry>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch() -> u64 { 0 }

    #[test]
    fn register_increments_id_and_running() {
        let mut r = JobRegistry::new();
        let a = r.register("a".into(), PathBuf::from("/a"), epoch());
        let b = r.register("b".into(), PathBuf::from("/b"), epoch());
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert!(r.can_spawn());
    }

    #[test]
    fn cap_blocks_after_max() {
        let mut r = JobRegistry::new();
        for _ in 0..MAX_RUNNING { r.register("x".into(), PathBuf::from("/"), epoch()); }
        assert!!(!r.can_spawn(), "达上限应禁止再开");
    }

    #[test]
    fn finish_releases_running_slot() {
        let mut r = JobRegistry::new();
        for _ in 0..MAX_RUNNING { r.register("x".into(), PathBuf::from("/"), epoch()); }
        assert!(!r.can_spawn());
        r.finish(1, JobStatus::Done { code: 0 }, epoch());
        assert!(r.can_spawn(), "完成后释放并发槽");
    }

    #[test]
    fn finish_idempotent_on_running_decrement() {
        let mut r = JobRegistry::new();
        let id = r.register("x".into(), PathBuf::from("/"), epoch());
        r.finish(id, JobStatus::Done { code: 0 }, epoch());
        let before = r.running;
        r.finish(id, JobStatus::Killed, epoch()); // 重复 finish 不应再递减
        assert_eq!(r.running, before);
    }
}
```

- [ ] **Step 2: 在 lib.rs 注册模块**

`src-tauri/src/lib.rs` 顶部模块声明区（`pub mod tools;` 同级）加：
```rust
pub mod jobs;
```

- [ ] **Step 3: 跑测试**

Run: `cd src-tauri && cargo test --lib jobs::tests`
Expected: 4 passed。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/jobs.rs src-tauri/src/lib.rs
git commit -m "feat(jobs): Job/JobRegistry 类型骨架 + 并发上限" -m "Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 3: spawn_process_job（pipe-drainer + guardian + JobObject + assign 硬校验）

**Files:**
- Modify: `src-tauri/src/jobs.rs`（加 `spawn_process_job`、复用 `tools::win_job`、`tools::truncate`/`decode_output`）

**Interfaces:**
- Consumes: `tools::win_job::Job`（KILL_ON_JOB_CLOSE）、`tools::truncate`、`tools::decode_output`（如 decode_output 是私有，改为 `pub(crate)`）。
- Produces: `pub async fn spawn_process_job(cmd, env, cwd, timeout_secs, label, registry: SharedRegistry, done_tx, update: Arc<dyn JobUpdate>) -> Result<JobId, String>`。

**前置：** `tools.rs` 的 `fn decode_output` 改 `pub(crate)`（Task 5 统一处理；本任务先用，若私有则在本任务 Step 0 顺手改）。

- [ ] **Step 0: 暴露 decode_output**

`tools.rs` 把 `fn decode_output(bytes: &[u8]) -> String` 改为 `pub(crate) fn decode_output(...)`。并把 `win_job` 模块内 `Job` 的字段/方法保持 `pub`（已是）。

- [ ] **Step 1: 写 spawn_process_job + 单测（放 jobs.rs 末尾，在 `#[cfg(test)]` 之前）**

```rust
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::tools::{self, win_job};

/// 后台跑一条 cmd 命令。立即返回 JobId；完成时 done_tx 投递 JobOutcome。
///
/// 关键：专职 pipe-drainer 持续读 stdout/stderr → log 文件（不 drain 则管道缓冲满、子进程 write 阻塞假死）。
/// Job Object 句柄存进 Job._job_handle（活过 child.wait()；app 退出句柄关闭→KILL_ON_JOB_CLOSE 杀树）。
pub async fn spawn_process_job(
    command: String,
    env_vars: Vec<(String, String)>,     // 已 allowlist 过的 (k,v)
    cwd: &std::path::Path,
    timeout_secs: u64,
    label: String,
    registry: SharedRegistry,
    done_tx: mpsc::Sender<JobOutcome>,
    update: Arc<dyn JobUpdate>,
) -> Result<JobId, String> {
    // 并发上限（A3）
    {
        let mut r = registry.lock().unwrap();
        if !r.can_spawn() { return Err("已达并发上限(8)".into()); }
    }

    // 确保 .ovoice-jobs 目录
    let jobs_dir = cwd.join(".ovoice-jobs");
    let _ = std::fs::create_dir_all(&jobs_dir);

    let started = now_ms();
    let (id, log_path) = {
        let mut r = registry.lock().unwrap();
        let id = r.register(label.clone(), PathBuf::new(), started); // log_path 占位，下面填
        (id, jobs_dir.join(format!("{id}.log")))
    };
    {
        let mut r = registry.lock().unwrap();
        if let Some(j) = r.jobs.get_mut(&id) { j.log_path = log_path.clone(); }
        let snap = r.get(id).cloned().unwrap();
        drop(r);
        update.update(&snap);
    }

    let full = format!("chcp 65001>nul && {}", command);
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").arg(&full)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false); // 关键：句柄归 registry
    for (k, v) in &env_vars { cmd.env(k, v); }

    let mut child = cmd.spawn().map_err(|e| format!("启动失败: {e}"))?;
    let pid = child.id();

    // Job Object（A4：assign 失败 → 判 Failed 不开 job，避免孤儿）
    let job_handle: Option<win_job::Job> = match (win_job::Job::create(), pid) {
        (Some(j), Some(pid)) => { j.assign_pid(pid); Some(j) }
        (Some(j), None) => Some(j),
        _ => {
            // assign/create 失败：杀掉刚起的子进程，判 Failed
            let _ = child.start_kill();
            let mut r = registry.lock().unwrap();
            r.finish(id, JobStatus::Failed { reason: "Job Object 创建/分配失败".into() }, now_ms());
            let snap = r.get(id).cloned().unwrap();
            drop(r);
            update.update(&snap);
            return Err("Job Object 创建/分配失败".into());
        }
    };
    // 句柄存进 registry（活过 wait；app 退出 → 关闭 → KILL_ON_JOB_CLOSE）
    {
        let mut r = registry.lock().unwrap();
        if let Some(j) = r.jobs.get_mut(&id) { j._job_handle = job_handle; }
    }

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let log_for_drain = log_path.clone();
    let id_for_drain = id;
    // pipe-drainer（A1）：持续合并 stdout+stderr → log 文件，防止管道满死锁。
    let drainer = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut s) = stdout { s.read_to_end(&mut buf).await.ok(); }
        if let Some(mut e) = stderr { e.read_to_end(&mut buf).await.ok(); }
        let _ = std::fs::write(&log_for_drain, &buf);
        let _ = id_for_drain; // tail 在 guardian 读 log
    });

    let reg2 = registry.clone();
    let upd2 = update.clone();
    let done2 = done_tx.clone();
    let log_for_guard = log_path.clone();
    // guardian：超时/完成 → 投递 JobOutcome；emit 前查 Killed（Task 4 加查 Killed）。
    tokio::spawn(async move {
        let res = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await;
        let _ = drainer.await; // 确保 log 写完
        let tail = read_tail(&log_for_guard);
        let (ok, code_opt, reason) = match res {
            Ok(Ok(s)) => (true, s.code(), None),
            Ok(Err(_)) => (false, None, Some("进程 wait 失败".into())),
            Err(_) => (false, None, Some(format!("超时({timeout_secs}s)"))),
        };
        let status = match (&reason, code_opt) {
            (Some(r), _) => JobStatus::Failed { reason: r.clone() },
            (None, Some(c)) => JobStatus::Done { code: c },
            (None, None) => JobStatus::Failed { reason: "无退出码".into() },
        };
        let killed_already = {
            let r = reg2.lock().unwrap();
            matches!(r.get(id).map(|j| &j.status), Some(JobStatus::Killed))
        };
        {
            let mut r = reg2.lock().unwrap();
            if !killed_already { r.finish(id, status.clone(), now_ms()); }
            let snap = r.get(id).cloned().unwrap();
            drop(r);
            upd2.update(&snap);
        }
        if !killed_already {
            let _ = done2.send(JobOutcome { job_id: id, code: code_opt, tail, ok }).await;
        }
        // killed_already: 人为终止，不唤醒模型，也不投递
    });

    Ok(id)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn read_tail(log: &std::path::Path) -> String {
    const TAIL: usize = 4 * 1024;
    let bytes = std::fs::read(log).unwrap_or_default();
    let text = tools::decode_output(&bytes);
    tools::truncate(text.trim(), TAIL, "\n…[已截断]")
}
```

为让 `Job` 持有句柄，Task 2 的 `struct Job` 加字段（改 Task 2 的 Job 定义）：
```rust
#[serde(skip)]
pub _job_handle: Option<crate::tools::win_job::Job>,
```
（`#[serde(skip)]` 避免 Serialize 句柄；`tools::win_job` 模块需 `pub`——在 tools.rs 里把 `mod win_job` 改 `pub(crate) mod win_job`。）

- [ ] **Step 2: 加单测（jobs.rs 的 tests mod）**

```rust
    use tokio::sync::mpsc;
    async fn drain_all(mut rx: mpsc::Receiver<JobOutcome>) -> Vec<JobOutcome> {
        let mut out = vec![];
        while let Ok(o) = rx.try_recv() { out.push(o); }
        // 给后台任务一点时间
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        while let Ok(o) = rx.try_recv() { out.push(o); }
        out
    }

    #[tokio::test]
    async fn spawn_echo_completes_done() {
        let reg: SharedRegistry = Arc::new(Mutex::new(JobRegistry::new()));
        let (tx, mut rx) = mpsc::channel(8);
        let dir = tempfile::tempdir().unwrap();
        let id = spawn_process_job(
            "echo hello".into(), vec![], dir.path(), 15, "echo".into(),
            reg.clone(), tx, Arc::new(NoopJobUpdate),
        ).await.unwrap();
        let out = drain_all(&mut rx).await;
        assert_eq!(out.len(), 1);
        assert!(out[0].ok, "echo 应成功");
        let r = reg.lock().unwrap();
        assert!(matches!(r.get(id).unwrap().status, JobStatus::Done { code: 0 }));
    }

    #[tokio::test]
    async fn spawn_timeout_marks_failed() {
        let reg: SharedRegistry = Arc::new(Mutex::new(JobRegistry::new()));
        let (tx, mut rx) = mpsc::channel(8);
        let dir = tempfile::tempdir().unwrap();
        let id = spawn_process_job(
            "ping -n 60 127.0.0.1".into(), vec![], dir.path(), 2, "hang".into(),
            reg.clone(), tx, Arc::new(NoopJobUpdate),
        ).await.unwrap();
        let out = drain_all(&mut rx).await;
        assert_eq!(out.len(), 1);
        assert!(!out[0].ok);
        let r = reg.lock().unwrap();
        assert!(matches!(r.get(id).unwrap().status, JobStatus::Failed { .. }));
    }

    #[tokio::test]
    async fn spawn_pipe_drain_does_not_block_on_large_output() {
        // A1 关键测试：写 >4KB stdout；若不 drain，子进程会阻塞、guardian 超时判 Failed。
        let reg: SharedRegistry = Arc::new(Mutex::new(JobRegistry::new()));
        let (tx, mut rx) = mpsc::channel(8);
        let dir = tempfile::tempdir().unwrap();
        // cmd 生成 ~20KB 输出
        let cmd = r#"for /L %i in (1,1,2000) do @echo line%i"#;
        let id = spawn_process_job(
            cmd.into(), vec![], dir.path(), 20, "chatty".into(),
            reg.clone(), tx, Arc::new(NoopJobUpdate),
        ).await.unwrap();
        let out = drain_all(&mut rx).await;
        assert_eq!(out.len(), 1);
        assert!(out[0].ok, "大输出必须因 drain 正常完成，而非死锁超时");
        let r = reg.lock().unwrap();
        assert!(matches!(r.get(id).unwrap().status, JobStatus::Done { .. }));
    }

    #[tokio::test]
    async fn spawn_rejected_at_cap() {
        let reg: SharedRegistry = Arc::new(Mutex::new(JobRegistry::new()));
        let (tx, _rx) = mpsc::channel(64);
        let dir = tempfile::tempdir().unwrap();
        for _ in 0..MAX_RUNNING {
            spawn_process_job("ping -n 60 127.0.0.1".into(), vec![], dir.path(), 60, "x".into(),
                reg.clone(), tx.clone(), Arc::new(NoopJobUpdate)).await.unwrap();
        }
        let err = spawn_process_job("echo hi".into(), vec![], dir.path(), 5, "over".into(),
            reg.clone(), tx, Arc::new(NoopJobUpdate)).await;
        assert!(err.is_err(), "达上限应拒绝");
    }
```

- [ ] **Step 3: 跑测试**

Run: `cd src-tauri && cargo test --lib jobs::tests`
Expected: 4 个新测 + Task 2 的 4 个全 PASS（共 8）。`spawn_pipe_drain_does_not_block_on_large_output` 若 FAIL 说明 drain 没生效——回头查 drainer。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/jobs.rs src-tauri/src/tools.rs
git commit -m "feat(jobs): spawn_process_job + pipe-drainer + JobObject 句柄 + assign 硬校验" -m "Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 4: kill_job + guardian 查 Killed

**Files:**
- Modify: `src-tauri/src/jobs.rs`

**Interfaces:**
- Produces: `pub fn kill_job(id, registry, update) -> bool`（杀树、置 Killed、不投递 JobDone）。

- [ ] **Step 1: 加 kill_job + 测试**

在 jobs.rs（`spawn_process_job` 之后）加：
```rust
/// 人为终止：关闭 Job Object 句柄杀整棵树、置 Killed、推 job-update。
/// **不投递 JobOutcome**——guardian 见 Killed 也不再发（kill 不唤醒模型）。
pub fn kill_job(id: JobId, registry: &SharedRegistry, update: &Arc<dyn JobUpdate>) -> bool {
    let mut r = registry.lock().unwrap();
    let Some(j) = r.jobs.get_mut(&id) else { return false; };
    // 关句柄 → KILL_ON_JOB_CLOSE 杀树；取出置 None
    let _ = j._job_handle.take();
    let was_running = matches!(j.status, JobStatus::Running);
    if was_running {
        j.status = JobStatus::Killed;
        j.finished_at = Some(now_ms());
    }
    let snap = j.clone();
    drop(r);
    update.update(&snap);
    true
}
```
（guardian 在 Task 3 已有 `killed_already` 检查：先锁查 `JobStatus::Killed`，是则不 finish、不投递。本任务确保 kill_job 先置 Killed。）

在 tests mod 加：
```rust
    #[tokio::test]
    async fn kill_does_not_emit_jobdone() {
        let reg: SharedRegistry = Arc::new(Mutex::new(JobRegistry::new()));
        let (tx, mut rx) = mpsc::channel(8);
        let dir = tempfile::tempdir().unwrap();
        let id = spawn_process_job(
            "ping -n 60 127.0.0.1".into(), vec![], dir.path(), 60, "hang".into(),
            reg.clone(), tx, Arc::new(NoopJobUpdate),
        ).await.unwrap();
        // 杀掉
        assert!(kill_job(id, &reg, &Arc::new(NoopJobUpdate)));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let mut got = vec![];
        while let Ok(o) = rx.try_recv() { got.push(o); }
        assert!(got.is_empty(), "人为终止不应投递 JobOutcome（不唤醒模型）");
        let r = reg.lock().unwrap();
        assert!(matches!(r.get(id).unwrap().status, JobStatus::Killed));
    }
```

- [ ] **Step 2: 跑测试**

Run: `cd src-tauri && cargo test --lib jobs::tests::kill_does_not_emit_jobdone`
Expected: PASS。

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/jobs.rs
git commit -m "feat(jobs): kill_job 杀树置 Killed，不唤醒模型" -m "Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 5: ToolsCtx + dispatch(ctx) + bash background/env allowlist + schema

**Files:**
- Modify: `src-tauri/src/tools.rs`、`src-tauri/src/llm.rs`（更新 run_loop 内 dispatch 调用）

**Interfaces:**
- Produces: `struct ToolsCtx`、`pub async fn dispatch(name, args, ctx: &ToolsCtx) -> String`、更新 `schemas()`（bash 加 background/timeout_secs/env + mmx description）。
- Consumes: `jobs::*`（Task 2-4）、`config.minimax_region`（Task 1）。

- [ ] **Step 1: 加 ToolsCtx + 改 tool_bash 支持 background/env**

tools.rs 顶部加：
```rust
use crate::jobs::{self, JobOutcome, JobUpdate, SharedRegistry};
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct ToolsCtx {
    pub workspace: std::path::PathBuf,
    pub jobs: SharedRegistry,
    pub job_done_tx: mpsc::Sender<JobOutcome>,
    pub job_update: Arc<dyn JobUpdate>,
    pub minimax_region: String,
}
```
改 `tool_bash` 为前台分支（保持现有前台逻辑，超时来自参数；env 基线 + allowlist 注入）：
```rust
pub async fn tool_bash(args: &Value, ctx: &ToolsCtx, timeout_secs: u64) -> String {
    let command = match args.get("command").and_then(|v| v.as_str()) {
        Some(c) => c.to_string(),
        None => return "bash 缺少 command 参数".into(),
    };
    let background = args.get("background").and_then(|v| v.as_bool()).unwrap_or(false);
    let env_vars = parse_env(args.get("env"), &ctx.minimax_region);

    if background {
        return match jobs::spawn_process_job(
            command, env_vars, &ctx.workspace, timeout_secs,
            label_for(&command), ctx.jobs.clone(), ctx.job_done_tx.clone(), ctx.job_update.clone(),
        ).await {
            Ok(id) => format!(r#"{{"job_id":{id},"status":"running","log":".ovoice-jobs/{id}.log"}}"#),
            Err(e) => format!("后台启动失败: {e}"),
        };
    }

    // 前台：沿用原阻塞逻辑（env 注入 + chcp + JobObject 杀树超时）
    let full = format!("chcp 65001>nul && {}", command);
    let mut cmd = tokio::process::Command::new("cmd");
    cmd.arg("/C").arg(&full)
        .current_dir(&ctx.workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in &env_vars { cmd.env(k, v); }
    // …（保留现有 spawn + JobObject + timeout + decode_output + truncate 返回逻辑不变）
    # region 前台执行（同原 tool_bash 135-198 行的 spawn/timeout/decode 部分，env 改用 env_vars）
    // 【实现者：把原 tool_bash 的 spawn…wait_with_output…decode_output…truncate 那段原样搬进这里，
    //   只把 command 换成 full、workspace 换成 &ctx.workspace。】
    # endregion
    unimplemented!("搬原前台逻辑")
}
```
> 注：上面 `# region`/`unimplemented!` 只是占位提示——**实际实现时把原 `tool_bash`（135-198 行）的 spawn→JobObject→timeout→wait_with_output→decode_output→truncate 那段原样移入**，command 用 `full`、cwd 用 `&ctx.workspace`、env 循环用 `env_vars`。不要留 `unimplemented!`。

加辅助函数：
```rust
/// env allowlist（C3）：仅 MINIMAX_*/MMX_*；基线固定加 MINIMAX_REGION。
fn parse_env(env_val: Option<&Value>, region: &str) -> Vec<(String, String)> {
    let mut out = vec![("MINIMAX_REGION".into(), region.into())];
    if let Some(obj) = env_val.and_then(|v| v.as_object()) {
        for (k, v) in obj {
            if (k.starts_with("MINIMAX_") || k.starts_with("MMX_")) && k != "MINIMAX_REGION" {
                if let Some(s) = v.as_str() { out.push((k.clone(), s.into())); }
            }
        }
    }
    out
}

fn label_for(cmd: &str) -> String {
    cmd.split_whitespace().take(3).collect::<Vec<_>>().join(" ")
}
```

改 `dispatch`：
```rust
pub async fn dispatch(name: &str, args: Value, ctx: &ToolsCtx) -> String {
    match name {
        "write" => tool_write(&args, &ctx.workspace),
        "read" => tool_read(&args, &ctx.workspace),
        "bash" => {
            let t = args.get("timeout_secs").and_then(|v| v.as_u64()).unwrap_or(60);
            tool_bash(&args, ctx, t).await
        }
        other => format!("未知工具: {}", other),
    }
}
```

- [ ] **Step 2: 更新 llm.rs run_loop 的 dispatch 调用（过渡）**

`run_loop` 里把：
```rust
Ok(args) => tools::dispatch(&name, args, workspace).await,
```
改为构造前台 ctx 再调用。但 `run_loop` 没有 jobs/sender——用前台专用构造器。在 tools.rs 加：
```rust
impl ToolsCtx {
    /// 前台专用（无后台能力）：run_loop 过渡期 + 测试用。
    pub fn foreground(workspace: std::path::PathBuf, minimax_region: String) -> Self {
        let (tx, _rx) = mpsc::channel(8); // 占位 sender，后台分支在本 ctx 下不该被触发
        Self {
            workspace, jobs: Arc::new(Mutex::new(jobs::JobRegistry::new())),
            job_done_tx: tx, job_update: Arc::new(jobs::NoopJobUpdate), minimax_region,
        }
    }
}
```
`run_loop` 改：
```rust
let ctx = tools::ToolsCtx::foreground(workspace.to_path_buf(), cfg.minimax_region.clone());
...
Ok(args) => tools::dispatch(&name, args, &ctx).await,
```

- [ ] **Step 3: 更新 schemas()（bash 加参 + mmx description）**

tools.rs `schemas()` 的 bash 项改为：
```rust
serde_json::json!({
    "type":"function",
    "function":{
        "name":"bash",
        "description":"在 Windows cmd 中执行命令（工作目录=workspace）。mmx CLI 已全局可用、区域(region)已预设。短任务(mmx text/speech/search/quota、echo、git)前台即可；长任务(mmx video/music generate 等需数分钟)务必 background:true。媒体产物落 --out 或 minimax-output/，用相对路径回看。",
        "parameters":{
            "type":"object",
            "properties":{
                "command":{"type":"string","description":"shell 命令（cmd /C 运行）"},
                "background":{"type":"boolean","description":"true=后台运行(立即返回 job_id，完成后另行通知)；false(默认)=前台阻塞"},
                "timeout_secs":{"type":"integer","description":"超时秒数，前台默认60、后台默认600"},
                "env":{"type":"object","description":"注入子进程的环境变量(仅允许 MINIMAX_*/MMX_* 前缀)"}
            },
            "required":["command"]
        }
    }
})
```

- [ ] **Step 4: 测试**

tools.rs tests mod 加：
```rust
    #[test]
    fn env_allowlist_drops_non_prefixed() {
        let v = serde_json::json!({"PATH":"x","MINIMAX_REGION":"global","MMX_CONFIG_DIR":"/tmp","EVIL":"1"});
        let out = parse_env(Some(&v), "cn");
        let keys: Vec<&str> = out.iter().map(|(k,_)| k.as_str()).collect();
        assert!(keys.contains(&"MINIMAX_REGION")); // 基线 cn 覆盖了传入的 global
        assert_eq!(out.iter().find(|(k,_)| k=="MINIMAX_REGION").unwrap().1, "cn");
        assert!(keys.contains(&"MMX_CONFIG_DIR"));
        assert!(!keys.iter().any(|k| *k=="PATH" || *k=="EVIL"));
        let region = out.iter().find(|(k,_)| k=="MINIMAX_REGION").unwrap();
        assert_eq!(region.1, "cn");
    }
```
既有 `dispatch_routes_write`/`dispatch_unknown_tool` 等改为用 `ToolsCtx::foreground`（改测试里 dispatch 调用签名）。`bash_echo`/`bash_timeout_kills` 改为传 `&ToolsCtx::foreground(dir.path().into(), "cn".into())`。

Run: `cd src-tauri && cargo test --lib tools::tests`
Expected: PASS（含 allowlist + 改签名后的旧测）。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/tools.rs src-tauri/src/llm.rs
git commit -m "feat(tools): ToolsCtx + bash background/env allowlist + mmx description" -m "Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 6: llm.rs Emitter 扩 turn 事件 + run_loop 体抽成 run_turn

**Files:**
- Modify: `src-tauri/src/llm.rs`

**Interfaces:**
- Produces: `Emitter` 加 `turn_start/turn_end/error`（默认 no-op，AppEmitter 后面覆写）；`pub async fn run_turn<R,E>(round, emitter, cfg, messages: &mut Vec<Value>, ctx: &ToolsCtx) -> ChatResponse`。`run_loop` 变薄壳调 `run_turn`（保 chat() 旧路径可编译 + 旧测试绿）。

- [ ] **Step 1: 扩 Emitter trait（默认 no-op）**

llm.rs 的 Emitter trait 改为：
```rust
#[async_trait]
pub trait Emitter: Send + Sync {
    async fn thinking(&self, _text: &str) {}
    async fn content(&self, _text: &str) {}
    async fn tool_call(&self, _name: &str, _args: &str) {}
    async fn tool_result(&self, _name: &str, _result: &str) {}
    async fn turn_start(&self) {}
    async fn turn_end(&self) {}
    async fn error(&self, _msg: &str) {}
}
```
（原 4 个方法改默认 no-op 不影响 AppEmitter——它仍 impl 这 4 个，新 3 个用默认。既有录音 FakeEmitter 若 impl 了这 4 个也仍编译。）

- [ ] **Step 2: 抽 run_turn（消息窗口化在 Task 7）**

把 `run_loop` 的 `for i in 0..MAX_ITERS { … }` 循环体抽成 `run_turn`，参数改为 `&mut Vec<Value>` + `&ToolsCtx`，开头 emit `turn_start`、结尾 emit `turn_end`/`error`：
```rust
pub async fn run_turn<R: LlmRound, E: Emitter>(
    round: &R, emitter: &E, cfg: &Config,
    messages: &mut Vec<Value>, ctx: &tools::ToolsCtx,
) -> ChatResponse {
    emitter.turn_start().await;
    let mut history: Vec<Value> = vec![];
    let mut last_content = String::new();
    for i in 0..MAX_ITERS {
        let resp = match round.round(messages, cfg, emitter).await {
            Ok(r) => r,
            Err(e) => { emitter.error(&e).await;
                return ChatResponse { content: String::new(), history, error: Some(e) }; }
        };
        last_content = resp.content.clone();
        match resp.finish {
            FinishReason::ToolCalls => {
                messages.push(resp.assistant_message.clone());
                history.push(resp.assistant_message);
                for call in resp.tool_calls {
                    let name = call["function"]["name"].as_str().unwrap_or("").to_string();
                    let args_str = call["function"]["arguments"].as_str().unwrap_or("{}").to_string();
                    emitter.tool_call(&name, &args_str).await;
                    let result = match serde_json::from_str::<Value>(&args_str) {
                        Err(e) => format!("参数解析失败: {e}"),
                        Ok(args) => tools::dispatch(&name, args, ctx).await,
                    };
                    emitter.tool_result(&name, &result).await;
                    let m = json!({ "role": "tool", "tool_call_id": call["id"], "content": result });
                    messages.push(m.clone());
                    history.push(m);
                }
                continue;
            }
            FinishReason::Stop => {
                history.push(resp.assistant_message.clone());
                emitter.turn_end().await;
                return ChatResponse { content: last_content, history, error: None };
            }
        }
    }
    let content = if last_content.trim().is_empty() {
        "（已达工具调用上限，未产生最终答复）".to_string()
    } else { format!("{last_content}\n\n_（已达工具调用上限）_") };
    history.push(json!({ "role": "assistant", "content": &content }));
    emitter.turn_end().await;
    ChatResponse { content, history, error: None }
}
```

`run_loop` 改薄壳（保 chat() 旧路径 + 旧测试绿）：
```rust
pub async fn run_loop<R: LlmRound, E: Emitter>(
    round: R, emitter: E, cfg: &Config, mut messages: Vec<Value>, workspace: &Path,
) -> ChatResponse {
    let ctx = tools::ToolsCtx::foreground(workspace.to_path_buf(), cfg.minimax_region.clone());
    run_turn(&round, &emitter, cfg, &mut messages, &ctx).await
}
```

- [ ] **Step 3: 跑测试（回归铁律）**

Run: `cd src-tauri && cargo test --lib llm::tests`
Expected: 既有 run_loop 测试（MAX_ITERS/partial-history 等）全 PASS（行为不变，只是走了 run_turn）。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/llm.rs
git commit -m "refactor(llm): Emitter 扩 turn 事件 + run_loop 体抽成 run_turn" -m "Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 7: agent.rs — SessionEvent + run_turn 编排 + 历史窗口化

**Files:**
- Create: `src-tauri/src/agent.rs`
- Modify: `src-tauri/src/lib.rs`（加 `pub mod agent;`）

**Interfaces:**
- Consumes: `llm::{run_turn, ChatResponse, HttpRound}`、`tools::ToolsCtx`、`jobs::JobOutcome`、`config::Config`、`Emitter`。
- Produces: `enum SessionEvent`、`fn window_messages(&mut Vec<Value>)`、`fn inject_jobdone_message(messages, outcome)`、`async fn handle_event(event, messages, ctx, round, emitter) -> ChatResponse`（纯逻辑，FakeRound 可测）。

- [ ] **Step 1: 写 agent.rs（事件类型 + 注入 + 窗口化 + handle_event，含 FakeRound 测试）**

```rust
//! 常驻 Agent Session：事件驱动 driver（在 lib.rs 里起 tokio 任务持有 messages+rx）。
//! 本模块只放「可纯逻辑测」的部分：事件类型、消息注入、窗口化、handle_event 编排。
use crate::config::Config;
use crate::llm::{self, ChatResponse, Emitter, LlmRound, RoundResult, FinishReason};
use crate::tools::ToolsCtx;
use crate::jobs::JobOutcome;
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub enum SessionEvent {
    UserMessage { text: String },
    JobDone(JobOutcome),
    Reset,
}

/// 把后台任务完成注入为 user 角色消息（OpenAI 兼容安全）。
pub fn inject_jobdone_message(messages: &mut Vec<Value>, o: &JobOutcome) {
    let body = if o.ok {
        format!("[后台任务 #{} 完成] 退出码 {}\n{}", o.job_id, o.code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()), o.tail)
    } else {
        format!("[后台任务 #{} 失败]\n{}", o.job_id, o.tail)
    };
    messages.push(json!({ "role": "user", "content": body }));
}

/// 历史窗口化（P1）：messages 超 40 条时，把旧 tool 结果折叠为占位（控单轮 token + 内存）。
pub fn window_messages(messages: &mut Vec<Value>) {
    const MAX_KEEP: usize = 40;
    if messages.len() <= MAX_KEEP { return; }
    let drop_n = messages.len() - MAX_KEEP;
    // system(0) 保留；从第 1 条起折叠旧 tool 消息
    let mut folded = 0;
    for m in messages.iter_mut().skip(1) {
        if folded >= drop_n { break; }
        if m.get("role").and_then(|v| v.as_str()) == Some("tool") {
            *m = json!({ "role": "tool", "content": "[旧工具结果已省略]" });
            folded += 1;
        }
    }
}

/// 处理一个事件：构造起始消息（Reset 清空+重注入 system），再 run_turn。
/// 纯逻辑：注入 round/emitter 即可离线测（FakeRound + FakeEmitter）。
pub async fn handle_event<R: LlmRound, E: Emitter>(
    event: &SessionEvent, messages: &mut Vec<Value>, ctx: &ToolsCtx,
    cfg: &Config, round: &R, emitter: &E,
) -> Option<ChatResponse> {
    match event {
        SessionEvent::UserMessage { text } => {
            messages.push(json!({ "role": "user", "content": text }));
        }
        SessionEvent::JobDone(o) => inject_jobdone_message(messages, o),
        SessionEvent::Reset => {
            messages.clear();
            messages.push(json!({ "role": "system", "content": cfg.system_prompt }));
            return None; // 重置不跑 turn
        }
    }
    window_messages(messages);
    Some(llm::run_turn(round, emitter, cfg, messages, ctx).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::{JobOutcome, SharedRegistry, JobRegistry, NoopJobUpdate};
    use std::sync::{Arc, Mutex};
    use async_trait::async_trait;

    struct FakeEmitter { content: Mutex<String> }
    #[async_trait]
    impl Emitter for FakeEmitter {
        async fn content(&self, t: &str) { self.content.lock().unwrap().push_str(t); }
        async fn tool_call(&self, _: &str, _: &str) {}
        async fn tool_result(&self, _: &str, _: &str) {}
        async fn turn_end(&self) {}
        async fn error(&self, _: &str) {}
    }

    fn ctx() -> ToolsCtx {
        let (_tx, _rx) = tokio::sync::mpsc::channel(8);
        ToolsCtx {
            workspace: ".".into(),
            jobs: Arc::new(Mutex::new(JobRegistry::new())) as SharedRegistry,
            job_done_tx: _tx,
            job_update: Arc::new(NoopJobUpdate),
            minimax_region: "cn".into(),
        }
    }
    fn cfg() -> Config {
        let mut c = serde_json::from_value::<Config>(serde_json::json!({"system_prompt":"你是助手"})).unwrap();
        c.system_prompt = "你是助手".into();
        c
    }

    // 一个返回 background bash tool_call 然后 Stop 的 FakeRound
    struct ScriptedRound { steps: Mutex<Vec<RoundResult>> }
    #[async_trait]
    impl LlmRound for ScriptedRound {
        async fn round(&self, _: &[Value], _: &Config, _: &dyn Emitter) -> Result<RoundResult, String> {
            let mut s = self.steps.lock().unwrap();
            if s.is_empty() { return Ok(RoundResult {
                content: "完成".into(), reasoning: String::new(), tool_calls: vec![],
                finish: FinishReason::Stop, usage: None,
                assistant_message: json!({"role":"assistant","content":"完成"}) }); }
            let r = s.remove(0);
            Ok(r)
        }
    }

    #[tokio::test]
    async fn usermessage_then_stop_runs_turn() {
        let round = ScriptedRound { steps: Mutex::new(vec![]) };
        let emit = FakeEmitter { content: Mutex::new(String::new()) };
        let mut msgs = vec![json!({"role":"system","content":"你是助手"})];
        let c = cfg(); let x = ctx();
        let res = handle_event(&SessionEvent::UserMessage { text: "hi".into() }, &mut msgs, &x, &c, &round, &emit).await;
        assert!(res.is_some());
        assert_eq!(res.unwrap().content, "完成");
        assert!(msgs.iter().any(|m| m["role"]=="user" && m["content"]=="hi"));
    }

    #[tokio::test]
    async fn jobdone_injects_message_and_runs_turn() {
        let round = ScriptedRound { steps: Mutex::new(vec![]) };
        let emit = FakeEmitter { content: Mutex::new(String::new()) };
        let mut msgs = vec![json!({"role":"system","content":"你是助手"})];
        let c = cfg(); let x = ctx();
        let o = JobOutcome { job_id: 7, code: Some(0), tail: "ok".into(), ok: true };
        handle_event(&SessionEvent::JobDone(o), &mut msgs, &x, &c, &round, &emit).await;
        let injected = msgs.iter().find(|m| m["role"]=="user").unwrap();
        assert!(injected["content"].as_str().unwrap().contains("#7 完成"));
    }

    #[tokio::test]
    async fn reset_clears_and_reinjects_system() {
        let round = ScriptedRound { steps: Mutex::new(vec![]) };
        let emit = FakeEmitter { content: Mutex::new(String::new()) };
        let mut msgs = vec![json!({"role":"system","content":"旧"}), json!({"role":"user","content":"x"})];
        let c = cfg(); let x = ctx();
        let res = handle_event(&SessionEvent::Reset, &mut msgs, &x, &c, &round, &emit).await;
        assert!(res.is_none(), "Reset 不跑 turn");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "你是助手"); // 用 cfg.system_prompt 重注入
    }

    #[test]
    fn window_folds_old_tool_results() {
        let mut msgs: Vec<Value> = vec![json!({"role":"system","content":"s"})];
        for i in 0..50 { msgs.push(json!({"role":"tool","content":format!("r{i}")})); }
        window_messages(&mut msgs);
        let folded = msgs.iter().filter(|m| m["content"]=="[旧工具结果已省略]").count();
        assert!(folded > 0, "超 40 条应折叠旧 tool 结果");
    }
}
```

- [ ] **Step 2: 注册模块 + 跑测试**

lib.rs 加 `pub mod agent;`。
Run: `cd src-tauri && cargo test --lib agent::tests`
Expected: 4 PASS。

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/agent.rs src-tauri/src/lib.rs
git commit -m "feat(agent): SessionEvent + handle_event + 历史窗口化（FakeRound 可测）" -m "Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 8: SessionHandle + spawn_session + driver + 命令

**Files:**
- Modify: `src-tauri/src/agent.rs`（加 SessionHandle/spawn_session/driver）、`src-tauri/src/lib.rs`（chat/reset_session 命令改用 session；注册 state）

**Interfaces:**
- Produces: `struct SessionHandle { tx }`、`pub fn spawn_session(app) -> SessionHandle`、`pub async fn chat_command(text, app) -> Result<(),String>`、`pub fn reset_session_command(app) -> Result<(),String>`。driver 把 job_done_rx 的 JobOutcome 转 SessionEvent::JobDone。

- [ ] **Step 1: agent.rs 加 SessionHandle + spawn_session + driver**

```rust
use tauri::{AppHandle, Manager};
use tokio::sync::mpsc;
use std::sync::Arc;

pub struct SessionHandle { pub tx: mpsc::Sender<SessionEvent> }

const CHANNEL_CAP: usize = 64; // G3 bounded

/// 启动常驻 driver；返回放进 app state 的句柄。
/// system_prompt 在此注入（G1）：messages[0] = system(cfg.system_prompt)。
pub fn spawn_session(app: AppHandle) -> SessionHandle {
    let (tx, mut rx) = mpsc::channel::<SessionEvent>(CHANNEL_CAP);

    let cfg = crate::config::load(&app);
    let workspace = std::path::PathBuf::from(&cfg.workspace_dir);
    let _ = std::fs::create_dir_all(&workspace);

    // ToolsCtx 共享：jobs registry + job_done 通道 + AppEmitter
    let registry: SharedRegistry = Arc::new(std::sync::Mutex::new(crate::jobs::JobRegistry::new()));
    let (job_done_tx, mut job_done_rx) = mpsc::channel::<crate::jobs::JobOutcome>(CHANNEL_CAP);
    let job_update: Arc<dyn crate::jobs::JobUpdate> = Arc::new(crate::AppJobUpdate(app.clone()));
    let ctx = ToolsCtx {
        workspace: workspace.clone(),
        jobs: registry.clone(),
        job_done_tx,
        job_update,
        minimax_region: cfg.minimax_region.clone(),
    };

    let app_for_driver = app.clone();
    let ctx_for_driver = clone_ctx(&ctx); // 见下
    tokio::spawn(async move {
        let mut messages: Vec<Value> = vec![json!({ "role": "system", "content": cfg.system_prompt.clone() })];
        loop {
            tokio::select! {
                // job 完成 → 转 SessionEvent::JobDone
                Some(o) = job_done_rx.recv() => {
                    let ev = SessionEvent::JobDone(o);
                    run_one(&ev, &mut messages, &ctx_for_driver, &cfg, &app_for_driver).await;
                }
                ev = rx.recv() => match ev {
                    Some(e) => { run_one(&e, &mut messages, &ctx_for_driver, &cfg, &app_for_driver).await; }
                    None => break,
                }
            }
        }
    });

    SessionHandle { tx }
}

async fn run_one(e: &SessionEvent, messages: &mut Vec<Value>, ctx: &ToolsCtx, cfg: &Config, app: &AppHandle) {
    if matches!(e, SessionEvent::Reset) {
        // 清 messages + registry（G4），重注入 system
        messages.clear();
        messages.push(json!({ "role": "system", "content": cfg.system_prompt }));
        { let mut r = ctx.jobs.lock().unwrap(); r.jobs.clear(); r.running = 0; }
        let _ = app.emit("chat-reset", ()); // 前端可清气泡
        return;
    }
    let emit = crate::AppEmitter { app: app.clone() };
    let _ = handle_event(e, messages, ctx, cfg, &crate::llm::HttpRound, &emit).await;
}
```
> `clone_ctx`：因 ToolsCtx 含 `mpsc::Sender`(可 clone)、`Arc`(可 clone)，加：
```rust
fn clone_ctx(c: &ToolsCtx) -> ToolsCtx {
    ToolsCtx {
        workspace: c.workspace.clone(), jobs: c.jobs.clone(),
        job_done_tx: c.job_done_tx.clone(), job_update: c.job_update.clone(),
        minimax_region: c.minimax_region.clone(),
    }
}
```

- [ ] **Step 2: lib.rs 加 AppEmitter 的事件覆写 + AppJobUpdate + 命令 + state**

lib.rs：
```rust
use tauri::Emitter;
#[async_trait]
impl llm::Emitter for AppEmitter {
    async fn thinking(&self, t: &str) { let _ = self.app.emit("llm-thinking", t); }
    async fn content(&self, t: &str) { let _ = self.app.emit("llm-content", t); }
    async fn tool_call(&self, n: &str, a: &str) { let _ = self.app.emit("llm-tool-call", json!({"name":n,"args":a})); }
    async fn tool_result(&self, n: &str, r: &str) { let _ = self.app.emit("llm-tool-result", json!({"name":n,"result":r})); }
    async fn turn_start(&self) { let _ = self.app.emit("chat-turn-start", json!({})); }
    async fn turn_end(&self) { let _ = self.app.emit("chat-turn-end", json!({})); }
    async fn error(&self, m: &str) { let _ = self.app.emit("chat-error", json!({"message":m})); }
}

/// 把 Job 状态变化外推为 job-update 事件（面板消费）。
struct AppJobUpdate(AppHandle);
impl jobs::JobUpdate for AppJobUpdate {
    fn update(&self, job: &jobs::Job) { let _ = self.0.emit("job-update", job); }
}

#[tauri::command]
async fn chat(text: String, app: AppHandle) -> Result<(), String> {
    let s = app.state::<std::sync::Mutex<agent::SessionHandle>>().inner();
    let tx = { s.lock().unwrap().tx.clone() };
    tx.send(agent::SessionEvent::UserMessage { text }).await
        .map_err(|_| "session 已关闭".to_string()) // C2
}

#[tauri::command]
fn reset_session(app: AppHandle) -> Result<(), String> {
    let s = app.state::<std::sync::Mutex<agent::SessionHandle>>().inner();
    let tx = { s.lock().unwrap().tx.clone() };
    let _ = tx.blocking_send(agent::SessionEvent::Reset);
    Ok(())
}

#[tauri::command]
fn list_jobs(app: AppHandle) -> Vec<jobs::Job> {
    let r = app.state::<SharedRegistry2>().inner();
    r.lock().unwrap().list()
}

#[tauri::command]
async fn kill_job(id: u64, app: AppHandle) -> Result<(), String> {
    let r = app.state::<SharedRegistry2>().inner().clone();
    let upd: Arc<dyn jobs::JobUpdate> = Arc::new(AppJobUpdate(app));
    if jobs::kill_job(id, &r, &upd) { Ok(()) } else { Err("任务不存在".into()) }
}

#[tauri::command]
fn read_job_log(id: u64, app: AppHandle) -> Result<String, String> {
    let ws = std::path::PathBuf::from(crate::config::load(&app).workspace_dir);
    let bytes = std::fs::read(ws.join(".ovoice-jobs").join(format!("{id}.log"))).map_err(|e| e.to_string())?;
    Ok(tools::truncate(&crate::tools::decode_output(&bytes).trim(), tools::READ_MAX, "\n…[已截断]"))
}
```
`type SharedRegistry2 = Arc<std::sync::Mutex<jobs::JobRegistry>>;`（注意：driver 的 ctx.jobs 与 list_jobs/kill_job 需共享**同一** registry——见 Step 3 注册时传同一 Arc）。

- [ ] **Step 3: 注册 state + invoke_handler + setup spawn_session**

lib.rs `run()`：
```rust
// registry 在 spawn_session 内创建，但 list_jobs/kill_job 也要访问 → 改为外部建、传进 spawn_session
```
为共享同一 registry，改造：在 `setup` 里先建 `let registry: SharedRegistry2 = Arc::new(Mutex::new(jobs::JobRegistry::new()));`，把它**同时** `.manage(Mutex::new(registry.clone()))`（给 list_jobs/kill_job）和传给 `agent::spawn_session(app, registry, job_update)`。改 `spawn_session` 签名加 `registry: SharedRegistry2` 参数，内部不再自建。

`.manage(Mutex::new(session_handle))` 放 `spawn_session` 返回后。`invoke_handler` 加 `chat`（已是）、`reset_session`、`list_jobs`、`kill_job`、`read_job_log`。setup 里：
```rust
let registry: Arc<Mutex<jobs::JobRegistry>> = Arc::new(Mutex::new(jobs::JobRegistry::new()));
app.manage(registry.clone());
let session = agent::spawn_session(app.handle().clone(), registry);
app.manage(Mutex::new(session));
```

- [ ] **Step 4: 编译 + 跑全量测试**

Run: `cd src-tauri && cargo build`（若 `ovoice.exe 拒绝访问` → 先 `taskkill //F //IM ovoice.exe`）。
Run: `cd src-tauri && cargo test --lib`
Expected: 全 PASS（含回归 + jobs + agent + tools 新测）。旧 `chat(messages)` 命令已删（被新 `chat(text)` 取代）——确认 lib.rs 里旧 `async fn chat(messages...)` 已移除。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/agent.rs src-tauri/src/lib.rs
git commit -m "feat(agent): SessionHandle + driver + chat/reset/list/kill/readlog 命令接线" -m "Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 9: 前端 — chat(text) + 事件驱动渲染 + Jobs 面板

**Files:**
- Modify: `src/main.js`、`src/index.html`、`src/styles.css`

**Interfaces:**
- Consumes: 命令 `chat(text)`/`reset_session`/`list_jobs`/`kill_job`/`read_job_log`；事件 `chat-turn-start`/`llm-content`/`llm-thinking`/`llm-tool-call`/`llm-tool-result`/`chat-turn-end`/`chat-error`/`job-update`/`chat-reset`。

- [ ] **Step 1: index.html — 顶栏「任务」按钮 + jobs-view**

顶栏（settings-btn 同级）加：
```html
<button id="jobs-btn" type="button">任务</button>
```
在 `settings-view` 之后加：
```html
<section id="jobs-view" hidden>
  <header class="row"><h2>后台任务</h2><button id="jobs-back" type="button" class="link">返回</button></header>
  <div id="jobs-list"></div>
</section>
```

- [ ] **Step 2: styles.css — jobs 面板样式**

```css
#jobs-list { display:flex; flex-direction:column; gap:8px; }
.job-row { border:1px solid var(--border,#ddd); border-radius:8px; padding:8px 10px; }
.job-row .jr-head { display:flex; justify-content:space-between; gap:8px; }
.job-row .chip { font-size:12px; padding:1px 6px; border-radius:10px; background:#eef; }
.job-row .chip.done{background:#e6f7e6;} .job-row .chip.failed{background:#fde;} .job-row .chip.killed{background:#eee;}
.job-row pre { margin:4px 0 0; white-space:pre-wrap; max-height:120px; overflow:auto; font-size:12px; }
.job-row .jr-actions { margin-top:6px; display:flex; gap:6px; }
```

- [ ] **Step 3: main.js — 去 messages + chat(text) + turn 事件 + jobs 面板**

删掉 `let messages = [...]` 与所有 `messages.push`/`res.history` 回填。`activeAssistantWrap` 在 `chat-turn-start` 时创建。改 submit 处理：
```javascript
let chatBusy = false;
let activeAssistantWrap = null;

form.addEventListener("submit", async (e) => {
  e.preventDefault();
  if (micState !== "idle" || chatBusy) return;
  const text = input.value.trim();
  if (!text) return;
  input.value = ""; input.style.height = "auto";
  chatBusy = true; sendBtn.disabled = true;
  addBubble("user", text);
  try { await invoke("chat", { text }); }
  catch (err) { addBubble("assistant", "发送失败: " + err); chatBusy = false; sendBtn.disabled = false; }
});
```
事件监听（替换原 `setupAgentEvents`）：
```javascript
async function setupAgentEvents() {
  await listen("chat-turn-start", () => {
    // 新建 assistant 三段气泡（G1：按 turn 边界定气泡）
    const wrap = document.createElement("div");
    wrap.className = "bubble assistant";
    wrap.innerHTML = '<details class="bubble-reasoning" hidden><summary>思考过程</summary><div class="reasoning-body"></div></details>'
      + '<div class="bubble-tools"></div>'
      + '<div class="bubble-text"><span class="typing"><i></i><i></i><i></i></span></div>'
      + '<div class="actions"></div>';
    list.appendChild(wrap); scrollBottom();
    activeAssistantWrap = wrap;
  });
  await listen("llm-thinking", (e) => appendReasoning(str(e)));
  await listen("llm-tool-call", (e) => { const p=e.payload||{}; appendToolCard(p.name||"?", p.args||""); });
  await listen("llm-tool-result", (e) => { const p=e.payload||{}; fillToolResult(p.name||"?", p.result||""); });
  await listen("llm-content", (e) => appendContent(str(e)));
  await listen("chat-turn-end", () => {
    if (activeAssistantWrap) {
      const t = activeAssistantWrap.querySelector(".bubble-text");
      t.classList.add("md");
      const txt = t.textContent.trim();
      if (txt) { t.innerHTML = renderMarkdown(txt); enhanceMarkdown(t); attachSpeak(activeAssistantWrap); }
      activeAssistantWrap = null;
    }
    chatBusy = false; sendBtn.disabled = false; input.focus(); scrollBottom();
  });
  await listen("chat-error", (e) => {
    if (activeAssistantWrap) { activeAssistantWrap.querySelector(".bubble-text").textContent = "出错: " + ((e.payload&&e.payload.message)||"");
      activeAssistantWrap.classList.add("error"); activeAssistantWrap = null; }
    chatBusy = false; sendBtn.disabled = false;
  });
  await listen("chat-reset", () => { list.innerHTML = ""; addBubble("assistant", "（已重置）"); });
  await listen("job-update", (e) => renderJobRow(e.payload));
}
function str(e){ return typeof e.payload === "string" ? e.payload : (e.payload && e.payload.text) || ""; }
```
（`appendReasoning`/`appendToolCard`/`fillToolResult`/`appendContent` 逻辑同现状，不改；`renderMarkdown`/`enhanceMarkdown`/`attachSpeak` 同现状。）

设置保存改为调 reset_session：
```javascript
configForm.addEventListener("submit", async (e) => {
  e.preventDefault();
  const cfg = readForm();
  // …保存中…
  try {
    await invoke("save_config", { cfg });
    await invoke("reset_session"); // system_prompt 即时生效（G4 清 history+registry）
    applySystemPrompt(cfg.system_prompt);
    showChat();
  } catch (err) { alert("保存失败: " + err); }
  // …finally…
});
```
Jobs 面板：
```javascript
const jobsView = document.getElementById("jobs-view");
document.getElementById("jobs-btn").onclick = async () => {
  chatView.hidden = true; settingsView.hidden = true; jobsView.hidden = false;
  const jobs = await invoke("list_jobs");
  document.getElementById("jobs-list").innerHTML = "";
  jobs.forEach(renderJobRow);
};
document.getElementById("jobs-back").onclick = () => { jobsView.hidden = true; chatView.hidden = false; };

function renderJobRow(j) {
  if (!j) return;
  const list = document.getElementById("jobs-list");
  let row = list.querySelector(`.job-row[data-id="${j.id}"]`);
  if (!row) { row = document.createElement("div"); row.className="job-row"; row.dataset.id=j.id; list.prepend(row); }
  const cls = j.status.running?"":Object.keys(j.status)[0];
  row.innerHTML = `<div class="jr-head"><b>#${j.id} ${j.label}</b><span class="chip ${cls}">${statusText(j.status)}</span></div>`
    + `<pre></pre><div class="jr-actions"><button data-k>终止</button><button data-l>完整日志</button></div>`;
  // 末 tail（从 log 读太重；这里 job-update 带 tail 时塞入；若无则点「完整日志」拉）
  row.querySelector("[data-k]").onclick = async () => { try{ await invoke("kill_job",{id:j.id}); }catch(e){ alert(e); } };
  row.querySelector("[data-l]").onclick = async () => { try{ const t = await invoke("read_job_log",{id:j.id}); alert(t); }catch(e){ alert(e); } };
}
function statusText(s){ if(s.running)return"运行中"; if(s.done)return`完成(${s.done.code})`; if(s.failed)return"失败"; if(s.killed)return"已终止"; return"?"; }
```

- [ ] **Step 4: build + 手测（FE 改必须 cargo build）**

Run: `cd src-tauri && cargo build`（Tauri 把 ../src 编进 exe）。然后 `pnpm tauri dev` 手测清单（T4）：
- 发普通消息 → chat-turn-start 建气泡 → llm-content 增量 → chat-turn-end 渲染 markdown。
- 让模型调 `bash{background:true,"command":"echo hi"}` → tool 卡出现 → 完成后新气泡「#1 完成」。
- 两个后台任务背靠背完成 → 各自独立气泡（不串）。
- 点「任务」→ 面板列 job → 点「终止」→ 状态变「已终止」、不弹完成消息。
- 设置改 system_prompt 保存 → 对话重置。

- [ ] **Step 5: Commit**

```bash
git add src/main.js src/index.html src/styles.css
git commit -m "feat(ui): chat(text) + turn 事件界定气泡 + Jobs 面板" -m "Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 10: 全量回归 + smoke 文档

**Files:**
- Modify: `docs/superpowers/reviews/2026-07-24-agent-session-jobs-eng-review.md`（在末尾追加「实现冒烟结果」）或新建 `docs/superpowers/smoke/2026-07-24-agent-session-jobs.md`

- [ ] **Step 1: 全量测试**

Run: `cd src-tauri && cargo test --lib`
Expected: 全 PASS（config/jobs/tools/llm/agent + 回归）。

- [ ] **Step 2: 真机 smoke（mmx 真任务）**

`taskkill //F //IM ovoice.exe` → `pnpm tauri dev`。对 agent 说「生成一段视频：夕阳下猫坐窗边」，确认：
- 模型调 `bash{background:true,"command":"mmx video generate --prompt ... --download cat.mp4"}`；
- 立即返回 job_id、对话气泡「生成中」；
- 面板看到 Running → Done；
- 完成后新气泡汇报 + 文件落在 workspace/minimax-output/ 或 cat.mp4。
记录耗时（验证 600s 后台超时够用）。

- [ ] **Step 3: 记冒烟结果 + Commit**

把结果写入 `docs/superpowers/smoke/2026-07-24-agent-session-jobs.md`（含耗时、发现的问题）。
```bash
git add docs/superpowers/smoke/2026-07-24-agent-session-jobs.md
git commit -m "docs(smoke): agent-session-jobs 真机冒烟（mmx 视频）" -m "Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Self-Review（写完后自查，已修正）

**1. Spec coverage：**
- 常驻 Session/driver/messages 后移 → Task 7-8 ✅
- 进程型 Job + pipe-drain + JobObject 句柄 + assign 硬校验 → Task 3 ✅
- 并发上限 8 → Task 2(can_spawn)+Task3(spawn 查) ✅
- kill 不回调 + guardian 查 Killed → Task 4 ✅
- ToolsCtx + bash background + env allowlist → Task 5 ✅
- Emitter turn 事件 + run_turn → Task 6 ✅
- system 注入（spawn_session）→ Task 8 ✅
- turn 边界事件（前端气泡）→ Task 9 ✅
- session_tx bounded(64) → Task 8 CHANNEL_CAP ✅
- chat send 失败返 Err → Task 8 chat ✅
- reset 清 registry → Task 8 run_one ✅
- read_job_log READ_MAX → Task 8 ✅
- 历史窗口化 → Task 7 window_messages ✅
- mmx description + 去 --async → Task 5 schema（不教 --async）✅
- 测试 T1(pipe-drain)/T2(边界 unit)/T4(FE smoke) + 回归 → Task 3/7/9/10 ✅
- 无遗漏。

**2. Placeholder scan：** Task 3 Step 1 的 `# region`/`unimplemented!` 是给实现者的明确指引（搬原前台逻辑），已在注释里写清「不要留 unimplemented」——实现者须产出真实代码。无 TBD/TODO/「适当处理」。

**3. Type consistency：** `ToolsCtx` 字段在 Task 5 定义、Task 7/8 一致使用；`Job._job_handle` 在 Task 2 加、Task 3/4 用；`SessionEvent` 在 Task 7 定义、Task 8 用；`SharedRegistry` = `Arc<Mutex<JobRegistry>>` 全程一致；`Emitter` 新方法 `turn_start/turn_end/error` 在 Task 6 定义、Task 8 AppEmitter 覆写、Task 9 前端消费事件名 `chat-turn-start/end`、`chat-error` 对应。
