# 阶段2：job 持久化 + 关闭提醒 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把内存型 job 系统（`JobId=u64`、不持久化、重启白板、退出丢运行中任务）改成持久化型：`JobId=String YYYYMMDD-NN`、按日 jsonl 事件流（append-only，跟 spawn 日）、启动 load 今日重建 registry、退出时 running 任务弹确认 + 强制退出标 `failed+reason=forced_exit`、旧数字 id 迁移 `legacy-<n>`。

**Architecture:** 复用 `history.rs` 已验证的「单写 task + 按日 jsonl + read_all fold」模式，在 `jobs.rs` 新增平行的 `JobWriter`（**不泛型合并**——对话事件流与 job 事件流领域不同，泛化是过度设计）。`JobWriterHandle` 加进 `ToolsCtx`（与 `history` handle 同模式），`spawn_process_job` / `spawn_agent` 共用（Q2 DRY）。启动在 `spawn_session` 的 async block 里 load 今日 → fold → 重建 registry + 恢复 `next_seq` + 悬空 running 标 forced_exit。关闭在 `on_window_event` 拦 `CloseRequested`。

**Tech Stack:** Rust（Tauri 2 后端）+ 原生 JS（前端）。`tokio::sync::mpsc` 单写者、`serde_json`、`tempfile`+`FakeRound`/`FakeEmitter` 离线测。无新依赖。

## Global Constraints

- **JobId 类型 `u64 → String`，格式 `YYYYMMDD-NN`**（spec §1 / D1）：NN 2 位补零（01–99），超 99 自动扩展（100、101…）；当天序号启动从持久化恢复（今日 max+1），跨日从 01 reset。
- **数据保留硬约束**（spec §数据保留原则 / D2）：`.ovoice-jobs/YYYY-MM-DD.jsonl` + `.ovoice-jobs/{id}.log` **永不清理、永不归档、永不删**。append-only。按日切分仅便读，不删旧。
- **持久化路径**：`{cache_dir}/.ovoice-jobs/YYYY-MM-DD.jsonl`（跟 spawn 日，A5）；进程日志仍 `{cache_dir}/.ovoice-jobs/{id}.log`（id 现 String）。
- **schema 版本**（Q3）：jsonl 每条事件带 `"schema":1`。
- **load 时按 id fold 取最后一条**（spec §4）：同 id 多事件（running→done / running→failed），状态取最后。
- **复用 `history.rs` 模式**：`HistoryWriterHandle`/`spawn_writer`/`read_all`/`date_from_ts_local` 是模板，**照抄语义到 jobs.rs**，不抽公共泛型（领域不同）。
- **阶段边界**：「重启主动触发汇报 turn」属阶段3（依赖调度模型）。阶段2 启动**只 load + 标 forced_exit + Jobs 面板显示，不唤醒主 agent**。
- **dev server 锁 exe**（`[[ovoice-dev-server-cargo-lock]]`）：验证用 `cargo check --tests --manifest-path src-tauri/Cargo.toml` + `cargo test --lib --manifest-path src-tauri/Cargo.toml`（阶段2 有真单测，可跑 tempfile 离线测），**不要 `cargo build`/`cargo run`**（dev server 占 target/debug/ovoice.exe）。
- **pinned 每轮重读**（`[[ovoice-pinned-files-per-turn-reload]]`）：改 `SOUL.md`/`AGENT.md`/`MEMORY.md` 无需重编译（load_pinned per-turn）。
- **live vs history 双路径**（`[[ovoice-live-vs-history-render-paths]]`）：若动 Jobs 卡渲染，须同时改 `setupAgentEvents`（live）+ `buildHistoryBubbles`（history）。本阶段 Jobs 卡 job_id 显示用 `${j.id}`（String/u64 都适配），无渲染逻辑改动。
- **工具计数级联**（`[[ovoice-tool-count-cascade]]`）：本阶段**不动工具数量**（read_job_log 参数类型变，不增减工具），`llm.rs tools.len()` 断言不变。
- **提交信息以 `Co-Authored-By: Claude <noreply@anthropic.com>` 结尾**；分支 `feat/job-persistence`，严禁直接落 master。
- 所有命令行开发 Windows PowerShell；本仓库 Bash 工具=Git Bash；ovoice `tool_bash` 跑 Windows cmd。

## File Structure

| 文件 | 责任 | 本阶段改动 |
|---|---|---|
| `src-tauri/src/jobs.rs` | Job 领域：JobId/Job/JobStatus/JobOutcome/JobRegistry + spawn_process_job + JobWriter | **主战场**：JobId→String、id 生成、JobEvent+JobWriter+load_today+fold、spawn 写 jsonl、legacy 迁移 |
| `src-tauri/src/subagents.rs` | 子代理 spawn/kill/status（共用 JobRegistry） | spawn_agent 经 `ctx.job_writer` 写 jsonl（DRY） |
| `src-tauri/src/history.rs` | 对话历史 writer（不动，仅作模板参考） | `subagent_result(agent_id: u64)` → `&str`（JobId 波及） |
| `src-tauri/src/agent.rs` | driver：spawn_session/handle_event/run_one/jobdone_body | spawn_session 加 load_today + JobWriter spawn；subagent_result 调用适配 |
| `src-tauri/src/tools.rs` | ToolsCtx + agent 工具 dispatch | ToolsCtx 加 `job_writer` 字段；bash bg 返 JSON job_id 加引号 |
| `src-tauri/src/lib.rs` | Tauri 命令 + setup + on_window_event | kill_job/read_job_log id→String；CloseRequested 拦截；force_quit 命令 |
| `src/main.js` | 前端 | 关闭确认对话框（listen confirm-close + 调 force_quit） |
| `src-tauri/defaults/AGENT.md` | agent 提示词（pinned） | job_id 格式 + read_job_log 说明 |

---

### Task 1: JobId `u64→String` + today-based id 生成（内存 counter）

**Goal：** 类型变更席卷全链路编译通过；id 生成 `YYYYMMDD-NN`（内存 counter，跨日 reset，超 99 扩展）。**不接持久化**（counter 内存版，T4 加 load 恢复）。

**Files:**
- Modify: `src-tauri/src/jobs.rs:9`（`type JobId`）、`:84-149`（JobRegistry register/next_id）、测试 `:362-369`
- Modify: `src-tauri/src/history.rs:52`（`subagent_result` agent_id 类型）
- Modify: `src-tauri/src/agent.rs:67-68`（subagent_result 调用）
- Modify: `src-tauri/src/lib.rs:109,117`（kill_job/read_job_log id 类型）
- Modify: `src-tauri/src/tools.rs:457`（bash bg JSON job_id 加引号）
- Modify: `src/main.js`（job_id 显示——自然适配，仅确认）

**Interfaces:**
- Produces: `pub type JobId = String;`；`JobRegistry::register(...) -> JobId`（返 String）；`JobRegistry` 新字段 `current_day: String` + `next_seq: u64`。

- [ ] **Step 1: 写失败测试（id 格式 + 跨日 reset + 超 99 扩展 + 序号递增）**

在 `jobs.rs` 测试模块加（用 `std::time::SystemTime` 算今日，与实现一致）：

```rust
fn today_compact() -> String {
    // 与实现一致：epoch ms + 本地偏移 → YYYYMMDD（去横杠）
    let now = now_ms();
    let off = crate::history::local_offset_secs();
    let date = crate::history::date_from_ts_local(now, off); // YYYY-MM-DD
    date.replace('-', "")
}

#[test]
fn register_generates_today_seq_id() {
    let mut r = JobRegistry::new();
    let a = r.register(JobKind::Process, "a".into(), PathBuf::from("/a"), now_ms());
    let b = r.register(JobKind::Process, "b".into(), PathBuf::from("/b"), now_ms());
    let prefix = format!("{}-", today_compact());
    assert!(a.starts_with(&prefix), "id 应以今日日期开头: {a}");
    assert!(b.starts_with(&prefix), "id 应以今日日期开头: {b}");
    assert_eq!(a, format!("{}01", prefix), "第 1 个序号 01");
    assert_eq!(b, format!("{}02", prefix), "第 2 个序号 02");
}

#[test]
fn register_extends_past_99() {
    let mut r = JobRegistry::new();
    r.next_seq = 99;
    r.current_day = today_compact();
    let id = r.register(JobKind::Process, "x".into(), PathBuf::from("/"), now_ms());
    let prefix = format!("{}-", today_compact());
    assert_eq!(id, format!("{}100", prefix), "超 99 不补零，扩展到 100");
}

#[test]
fn register_resets_seq_on_new_day() {
    let mut r = JobRegistry::new();
    r.current_day = "19990101".into(); // 昨日
    r.next_seq = 50;
    let id = r.register(JobKind::Process, "x".into(), PathBuf::from("/"), now_ms());
    let prefix = format!("{}-", today_compact());
    assert_eq!(id, format!("{}01", prefix), "跨日 next_seq 应 reset 到 01");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml jobs::tests::register_generates_today_seq_id`
Expected: 编译失败（`next_seq`/`current_day` 字段不存在）或断言失败（id 是 `1`/`2`）。

- [ ] **Step 3: 改 JobId 类型 + JobRegistry id 生成**

`jobs.rs:9`：
```rust
pub type JobId = String;
```

`jobs.rs` 顶部 import 区（`use std::path::PathBuf;` 附近）加：
```rust
use crate::history::{date_from_ts_local, local_offset_secs};
```

`JobRegistry` struct（替换 `next_id: u64,`）：
```rust
pub struct JobRegistry {
    pub jobs: HashMap<JobId, Job>,
    /// 今日日期 YYYYMMDD（跨日 reset next_seq 的依据）。
    current_day: String,
    /// 今日已分配的下一个序号（1-based）。
    next_seq: u64,
    /// 运行中 process job 计数（用于 MAX_RUNNING 上限）。
    running: usize,
    /// 运行中 agent job 计数（用于 MAX_AGENTS 上限）。
    pub running_agents: usize,
    /// 子代理最大并发数（可配置，默认 MAX_AGENTS=4）。
    pub max_agents: usize,
    #[cfg(windows)]
    pub(crate) handles: HashMap<JobId, crate::tools::win_job::Job>,
}
```

`JobRegistry::new()`（替换 `next_id: 1,`）：
```rust
pub fn new() -> Self {
    Self {
        jobs: HashMap::new(),
        current_day: today_compact(),
        next_seq: 1,
        running: 0,
        running_agents: 0,
        max_agents: MAX_AGENTS,
        #[cfg(windows)]
        handles: HashMap::new(),
    }
}
```

`register`（替换 `let id = self.next_id; self.next_id += 1;`）：
```rust
    /// 登记 Running，返回 JobId；内部自增计数（调用方确保已 can_spawn）。
    pub fn register(&mut self, kind: JobKind, label: String, log_path: PathBuf, started_at: u64) -> JobId {
        let today = today_compact();
        if today != self.current_day {
            self.current_day = today.clone();
            self.next_seq = 1; // 跨日 reset
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        let id = format!("{today}-{}", if seq <= 99 { format!("{seq:02}") } else { seq.to_string() });
        match kind { JobKind::Process => self.running += 1, JobKind::Agent => self.running_agents += 1 }
        self.jobs.insert(id.clone(), Job {
            id: id.clone(), kind, label, status: JobStatus::Running, log_path, started_at, finished_at: None,
            cancel: None, progress: None, answer: None, suppress_inject: false,
        });
        id
    }
```

`jobs.rs` 文件底部（`fn now_ms` 之前）加 helper：
```rust
/// 今日日期 YYYYMMDD（本地，去横杠）。与 history::date_from_ts_local 同源（spec §1：id 编码 spawn 日）。
fn today_compact() -> String {
    date_from_ts_local(now_ms(), local_offset_secs()).replace('-', "")
}
```

- [ ] **Step 4: 改 history.rs subagent_result 签名（agent_id u64→&str）**

`history.rs:52`：
```rust
    pub fn subagent_result(ts: u64, agent_id: &str, summary: &str, refs: &str) -> Self {
        let mut d = Map::new();
        d.insert("agent_id".into(), json!(agent_id));
        d.insert("summary".into(), json!(summary));
        d.insert("ref".into(), json!(refs));
        Self { seq: 0, ts, thread: "main".into(), kind: "subagent_result".into(), data: d }
    }
```

- [ ] **Step 5: 改 agent.rs subagent_result 调用**

`agent.rs:67-68`（`o.job_id` 现是 String，传 `&o.job_id`）：
```rust
        SessionEvent::JobDone(o) => match o.kind {
            crate::jobs::JobKind::Agent => crate::history::HistoryEvent::subagent_result(
                now, &o.job_id, &o.answer.clone().unwrap_or_default(), &format!("thread=agent:{}", o.job_id)),
            crate::jobs::JobKind::Process => crate::history::HistoryEvent::external(now, &jobdone_body(o), "", None),
        },
```

- [ ] **Step 6: 改 lib.rs kill_job / read_job_log 参数类型**

`lib.rs:109` 和 `lib.rs:117`：
```rust
async fn kill_job(id: String, app: AppHandle) -> Result<(), String> {
```
```rust
fn read_job_log(id: String, app: AppHandle) -> Result<String, String> {
```
（函数体内 `format!("{id}.log")` 自然适配 String。）

- [ ] **Step 7: 改 tools.rs bash bg 返回 JSON（job_id 加引号）**

`tools.rs:457`（`{id}` 现是 String，JSON 值须带引号）：
```rust
                Ok(id) => format!(r#"{{"job_id":"{id}","status":"running","log":"用 read_job_log(id) 读取"}}"#),
```

- [ ] **Step 8: 修既有测试断言（id 从 u64 数字改 String）**

`jobs.rs:362-369` `register_increments_id_and_running`：删 `assert_eq!(a, 1); assert_eq!(b, 2);`（id 不再是数字），保留 `can_spawn` 断言。改后：
```rust
    #[test]
    fn register_increments_id_and_running() {
        let mut r = JobRegistry::new();
        let _a = r.register(JobKind::Process, "a".into(), PathBuf::from("/a"), epoch());
        let _b = r.register(JobKind::Process, "b".into(), PathBuf::from("/b"), epoch());
        // id 格式由 register_generates_today_seq_id 覆盖；此处仅验 running 计数。
        assert!(r.can_spawn(JobKind::Process));
    }
```

其他用 `1`/`2` 作 id 的测试（`finish_releases_running_slot` 用 `r.finish(1, ...)`、`finish_idempotent_on_running_decrement` 用 `r.register` 返回值、`spawn_*` 测试用 `id` 变量）——`finish(id: JobId)` 现收 String，`r.finish(1, ...)` 编译错。逐个改成用 `register` 返回的 id 变量：

`jobs.rs` 测试里所有 `r.finish(1, ...)` → 先 `let id = r.register(...); r.finish(id.clone(), ...)`。涉及 `finish_releases_running_slot`（:383）、`finish_idempotent_on_running_decrement`（:388-395）。`spawn_echo_completes_done` 等用 `let id = spawn_process_job(...)` 返回值，已适配（id 现是 String，`r.get(id)` 收 `&str`，String 自动 deref）。

agent.rs 测试 `jobdone_agent_appends_subagent_result_and_runs`（:412）的 `JobOutcome { job_id: 3, ... }` → `job_id: "3".into()`，断言 `sr.data["agent_id"] == json!(3)` → `json!("3")`。

subagents.rs 测试 `SubagentEmitter { job_id: 1, ... }`（:115 等多处）→ `job_id: "1".into()`。

- [ ] **Step 9: 全量编译 + 测试**

Run: `cargo check --tests --manifest-path src-tauri/Cargo.toml`
Expected: 0 error（warning 暂记）。
Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml`
Expected: 全 PASS（含新增 3 个 id 测试）。
Run: `node --check src/main.js`
Expected: 无输出（exit 0）。

- [ ] **Step 10: Commit**

```bash
git add -A && git commit -m "feat(jobs): JobId u64→String + YYYYMMDD-NN id 生成（T1）

type JobId=String；register 生成 {YYYYMMDD}-{NN}（NN 2 位补零，超 99
扩展），跨日 reset。内存 counter（T4 接 load 恢复）。全链路类型适配：
history subagent_result agent_id→&str、agent.rs 调用、lib kill_job/
read_job_log id→String、tools.rs bash bg JSON job_id 加引号、测试断言。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 2: JobEvent schema + JobWriter（jsonl 读写，照 history.rs 模式）

**Goal：** 在 jobs.rs 新增 JobEvent（持久化 schema）+ JobWriterHandle + spawn_job_writer + read_jobs_jsonl + fold_by_id。**纯逻辑，不接线**（T3 接 spawn，T4 接 load）。

**Files:**
- Modify: `src-tauri/src/jobs.rs`（追加 JobEvent/JobWriter/fold + 测试）

**Interfaces:**
- Produces:
  - `pub struct JobEvent { schema, id, kind, label, status, started_at, finished_at, code, tail, note, reason, deps }`（serde，flatten 友好）
  - `#[derive(Clone)] pub struct JobWriterHandle { tx: UnboundedSender<JobEvent> }` + `append(ev)` + `noop()`
  - `pub fn spawn_job_writer(jobs_dir: PathBuf, offset_secs: i64) -> JobWriterHandle`
  - `pub fn read_jobs_jsonl(jobs_dir: &Path, date: &str) -> Vec<JobEvent>`（读指定日期文件）
  - `pub fn fold_by_id(events: &[JobEvent]) -> HashMap<JobId, JobEvent>`（同 id 取最后）

- [ ] **Step 1: 写失败测试（round-trip + fold + schema + 跨天跟 spawn 日 + append-only）**

`jobs.rs` 测试模块加：
```rust
    use crate::history::date_from_ts_local;

    fn je(id: &str, status: &str, ts: u64) -> JobEvent {
        JobEvent { schema: 1, id: id.into(), kind: JobKind::Process, label: "x".into(),
            status: status.into(), started_at: ts, finished_at: None, code: None, tail: None,
            note: None, reason: None, deps: vec![] }
    }

    #[tokio::test]
    async fn job_writer_roundtrip_and_fold() {
        let dir = tempfile::tempdir().unwrap();
        let jd = dir.path().join(".ovoice-jobs");
        let w = spawn_job_writer(jd.clone(), 0);
        // 同 id 两条：running → done
        w.append(je("20260727-01", "running", 1_000));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut done = je("20260727-01", "done", 2_000); done.code = Some(0); done.finished_at = Some(2_000);
        w.append(done);
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let evs = read_jobs_jsonl(&jd, &date_from_ts_local(1_000, 0));
        assert_eq!(evs.len(), 2, "两条事件都落盘");
        let folded = fold_by_id(&evs);
        assert_eq!(folded.len(), 1, "fold 后一个 id");
        assert_eq!(folded["20260727-01"].status, "done", "fold 取最后一条");
        assert_eq!(folded["20260727-01"].code, Some(0));
    }

    #[test]
    fn fold_takes_last_by_started_at_order() {
        // events 顺序即文件顺序（已按 ts 升序写）；fold 取同 id 最后出现
        let evs = vec![je("X", "running", 1), je("Y", "running", 2), je("X", "failed", 3)];
        let f = fold_by_id(&evs);
        assert_eq!(f["X"].status, "failed");
        assert_eq!(f["Y"].status, "running");
    }

    #[tokio::test]
    async fn job_writer_writes_dated_file_by_spawn_day() {
        // A5：jsonl 跟 spawn 日（started_at 决定文件），不跟完成日
        let dir = tempfile::tempdir().unwrap();
        let jd = dir.path().join(".ovoice-jobs");
        let w = spawn_job_writer(jd.clone(), 0);
        // spawn 日 2026-07-26 23:55，完成日 2026-07-27 00:10（均 UTC；offset=0）
        let spawn_day = 1_785_024_000_000u64 + 23 * 3600_000; // 2026-07-26 23:00 UTC
        w.append(je("20260726-01", "running", spawn_day));
        let mut done = je("20260726-01", "done", spawn_day + 600_000); // 10 分钟后（跨天）
        done.finished_at = Some(spawn_day + 600_000);
        w.append(done);
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        // 两条都应落 spawn 日文件（2026-07-26.jsonl），完成日文件不存在
        assert!(jd.join("2026-07-26.jsonl").exists(), "事件落 spawn 日");
        assert!(!jd.join("2026-07-27.jsonl").exists(), "完成日不该有文件");
    }

    #[tokio::test]
    async fn job_writer_append_only_grows() {
        let dir = tempfile::tempdir().unwrap();
        let jd = dir.path().join(".ovoice-jobs");
        let w = spawn_job_writer(jd.clone(), 0);
        w.append(je("X", "running", 1_000));
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert_eq!(read_jobs_jsonl(&jd, &date_from_ts_local(1_000, 0)).len(), 1);
        w.append(je("Y", "running", 2_000));
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert_eq!(read_jobs_jsonl(&jd, &date_from_ts_local(1_000, 0)).len(), 2, "append-only 增长非覆盖");
    }

    #[test]
    fn job_event_has_schema_version() {
        let e = je("X", "running", 1);
        let line = serde_json::to_string(&e).unwrap();
        assert!(line.contains(r#""schema":1"#), "事件须带 schema 版本");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml jobs::tests::job_writer_roundtrip_and_fold`
Expected: 编译失败（JobEvent/spawn_job_writer 未定义）。

- [ ] **Step 3: 实现 JobEvent + JobWriterHandle + spawn_job_writer + read_jobs_jsonl + fold_by_id**

在 `jobs.rs` `SharedRegistry` 类型别名之后（约 :153）、`spawn_process_job` 之前插入：

```rust
// ───────────────────────────────────────────────────────────────────────────
// 持久化：按日 jsonl 事件流（照 history.rs 模式，领域独立不泛型合并）。
// 单写不变量：所有 append 经同一 writer task + 同一 mpsc 落盘，串行写不交错。
// ───────────────────────────────────────────────────────────────────────────

/// jsonl 一条 job 事件（append-only 事件流）。schema 字段向前兼容；status 字符串
/// （"running"/"done"/"failed"/"killed"）；code/tail/note/reason 视 status 可选。
/// deps 阶段3 用，阶段2 永远空 Vec（schema 占位）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobEvent {
    pub schema: u32,
    pub id: JobId,
    #[serde(rename = "type")]
    pub kind: JobKind,
    pub label: String,
    pub status: String, // "running" | "done" | "failed" | "killed"
    pub started_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deps: Vec<JobId>,
}

impl JobEvent {
    /// spawn 时登记 running（kind 来自 Job）。
    pub fn started(job: &Job) -> Self {
        Self { schema: 1, id: job.id.clone(), kind: job.kind, label: job.label.clone(),
            status: "running".into(), started_at: job.started_at, finished_at: None,
            code: None, tail: None, note: None, reason: None, deps: vec![] }
    }
    /// JobStatus → status 字符串（fold/load 用）。
    pub fn status_str(s: &JobStatus) -> &'static str {
        match s {
            JobStatus::Running => "running",
            JobStatus::Done { .. } => "done",
            JobStatus::Failed { .. } => "failed",
            JobStatus::Killed => "killed",
        }
    }
}

/// Producer 句柄：clone 进 ToolsCtx，所有 job 事件经此投递到单写 task。
#[derive(Clone)]
pub struct JobWriterHandle {
    tx: tokio::sync::mpsc::UnboundedSender<JobEvent>,
}
impl JobWriterHandle {
    /// 不落盘的句柄（测试/前台 ctx 用：send 到无人接收 channel，事件丢弃）。
    pub fn noop() -> Self {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<JobEvent>();
        Self { tx }
    }
    /// 投递一条事件（writer task 落盘）。无界 channel：写盘慢时暂存。
    pub fn append(&self, ev: JobEvent) { let _ = self.tx.send(ev); }
}

/// 启动单写 task：独占 .ovoice-jobs/ 文件句柄，串行 recv → 按 started_at 选日期文件 → append。
/// 跨天自动开新文件（append 模式，永不覆盖）。**前置：须在 Tokio runtime 上下文调用**
/// （spawn_session 的 async block 首行 / #[tokio::test]）。
pub fn spawn_job_writer(jobs_dir: PathBuf, offset_secs: i64) -> JobWriterHandle {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<JobEvent>();
    let _ = std::fs::create_dir_all(&jobs_dir);
    let handle = JobWriterHandle { tx };
    tokio::spawn(async move {
        let mut open: Option<(String, std::fs::File)> = None;
        while let Some(ev) = rx.recv().await {
            let date = crate::history::date_from_ts_local(ev.started_at, offset_secs);
            let need_new = match &open { Some((d, _)) => d != &date, None => true };
            if need_new {
                match std::fs::OpenOptions::new()
                    .create(true).append(true)
                    .open(jobs_dir.join(format!("{date}.jsonl")))
                {
                    Ok(f) => open = Some((date.clone(), f)),
                    Err(e) => { eprintln!("[jobs] 打开 {date}.jsonl 失败: {e}"); continue; }
                }
            }
            if let Some((_, f)) = open.as_mut() {
                use std::io::Write;
                let line = serde_json::to_string(&ev).unwrap_or_default();
                if let Err(e) = writeln!(f, "{line}") { eprintln!("[jobs] 写失败: {e}"); }
            }
        }
    });
    handle
}

/// 读指定日期的 job jsonl（跳坏行）。按文件顺序（= 写入顺序 = ts 升序）返回。
pub fn read_jobs_jsonl(jobs_dir: &Path, date: &str) -> Vec<JobEvent> {
    let path = jobs_dir.join(format!("{date}.jsonl"));
    let text = match std::fs::read_to_string(&path) { Ok(s) => s, Err(_) => return vec![] };
    let mut evs = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() { continue; }
        match serde_json::from_str::<JobEvent>(line) {
            Ok(e) => evs.push(e),
            Err(e) => eprintln!("[jobs] 跳过坏行 {path:?}: {e}"),
        }
    }
    evs
}

/// 同 id 多事件 fold 取最后一条（按输入顺序，= ts 升序）。
pub fn fold_by_id(events: &[JobEvent]) -> HashMap<JobId, JobEvent> {
    let mut m = HashMap::new();
    for e in events { m.insert(e.id.clone(), e.clone()); }
    m
}
```

- [ ] **Step 4: 编译 + 测试**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml jobs::tests`
Expected: 全 PASS（含新增 5 个 writer/fold 测试）。

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(jobs): JobEvent + JobWriter 按日 jsonl 事件流（T2）

照 history.rs 模式（单写 task + 按日 jsonl + fold），领域独立不泛型
合并。JobEvent schema=1 字段（id/type/label/status/started_at/
finished_at/code/tail/note/reason/deps）；spawn_job_writer 按 started_at
选 spawn 日文件（A5）；read_jobs_jsonl 读指定日期；fold_by_id 取最后。
纯逻辑，未接线（T3 接 spawn，T4 接 load）。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 3: 接线——spawn 写 jsonl（process + agent 共用 writer，Q2 DRY）

**Goal：** `ToolsCtx` 加 `job_writer` 字段；`spawn_session` 启动 `spawn_job_writer`；`spawn_process_job` / `spawn_agent` 在 register + finish 时写 JobEvent。

**Files:**
- Modify: `src-tauri/src/tools.rs:37-69`（ToolsCtx struct + foreground）
- Modify: `src-tauri/src/tools.rs:440-460`（tool_bash 调 spawn_process_job 传 writer）
- Modify: `src-tauri/src/tools.rs:270-280`（tool_subagent 调 spawn_agent，经 ctx）
- Modify: `src-tauri/src/jobs.rs:156-295`（spawn_process_job 加 writer 参数 + register/finish 写事件）
- Modify: `src-tauri/src/subagents.rs:372-499`（spawn_agent 经 sub_ctx.job_writer 写事件）
- Modify: `src-tauri/src/agent.rs:118-187`（spawn_session 构造 ToolsCtx 加 job_writer + spawn_job_writer）
- Modify: `src-tauri/src/agent.rs:319-351` tests（ctx_with 补字段）+ `src-tauri/src/subagents.rs:208`（agent_ctx 补字段）

**Interfaces:**
- Consumes: T1 `JobId=String`、T2 `JobEvent::started(&Job)` / `JobWriterHandle::append` / `JobWriterHandle::noop()`
- Produces: `ToolsCtx.job_writer: jobs::JobWriterHandle`；`spawn_process_job(..., writer: JobWriterHandle, ...)`（新参数）。

- [ ] **Step 1: 写失败测试（spawn process 后 jsonl 有 running+done 两条）**

`jobs.rs` 测试加：
```rust
    #[cfg(windows)]
    #[tokio::test]
    async fn spawn_process_writes_running_then_done_events() {
        let reg: SharedRegistry = Arc::new(Mutex::new(JobRegistry::new()));
        let (tx, mut rx) = mpsc::channel(8);
        let dir = tempfile::tempdir().unwrap();
        let jd = dir.path().join(".ovoice-jobs");
        let w = spawn_job_writer(jd.clone(), 0);
        let id = spawn_process_job(
            "echo hi".into(), vec![], dir.path(), dir.path(), 15, "echo".into(),
            reg.clone(), tx, Arc::new(NoopJobUpdate), w.clone(),
        ).await.unwrap();
        let _ = drain_all(&mut rx).await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let today = crate::history::date_from_ts_local(now_ms(), crate::history::local_offset_secs());
        let evs = read_jobs_jsonl(&jd, &today);
        let folded = fold_by_id(&evs);
        assert_eq!(folded.len(), 1, "一个 job id");
        let e = &folded[&id];
        assert_eq!(e.status, "done", "终态应为 done");
        assert_eq!(e.id, id);
    }
```
（`spawn_process_job` 签名加了 `writer` 参数 → 既有测试调用全部要补 `Arc::new(NoopJobUpdate)` 后加 `jobs::JobWriterHandle::noop()`。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo check --tests --manifest-path src-tauri/Cargo.toml`
Expected: 编译失败（`spawn_process_job` 参数数不匹配 / `ToolsCtx` 无 `job_writer` 字段）。

- [ ] **Step 3: 给 spawn_process_job 加 writer 参数 + register/finish 写事件**

`jobs.rs:156` 签名（在 `update: Arc<dyn JobUpdate>,` 后加 `writer: JobWriterHandle,`）：
```rust
pub async fn spawn_process_job(
    command: String,
    env_vars: Vec<(String, String)>,
    cwd: &std::path::Path,
    cache: &std::path::Path,
    timeout_secs: u64,
    label: String,
    registry: SharedRegistry,
    done_tx: mpsc::Sender<JobOutcome>,
    update: Arc<dyn JobUpdate>,
    writer: JobWriterHandle,
) -> Result<JobId, String> {
```

register 块（`:179-184`，登记后写 running 事件）改：
```rust
    let (id, log_path) = {
        let mut r = registry.lock().unwrap();
        if !r.can_spawn(JobKind::Process) { return Err("已达并发上限(8)".into()); }
        let id = r.register(JobKind::Process, label.clone(), PathBuf::new(), started);
        if let Some(j) = r.jobs.get(&id) { writer.append(JobEvent::started(j)); }
        (id, jobs_dir.join(format!("{id}.log")))
    };
```

guardian 内 `finish` 后（`:282-288`，写终态事件）——在 `upd2.update(&snap);` 之后、`is_killed` 判断之前补一段写 done/failed 事件。在 `let killed_already = { ... };` 块内、`drop(r);` 之前不行（snap 在 drop 后）。改为：snap 取出后写事件。把 guardian 的 `let killed_already = { ... };` 块替换为：
```rust
        let killed_already = {
            let mut r = reg2.lock().unwrap();
            let is_killed = matches!(r.get(id).map(|j| &j.status), Some(JobStatus::Killed));
            if !is_killed { r.finish(id, status, now_ms()); }
            let snap = r.get(id).cloned().unwrap();
            drop(r);
            // 写终态 JobEvent（done/failed；killed 不写——人为终止不落 job jsonl，与不唤醒模型一致）
            if !is_killed {
                writer2.append(terminal_event(&snap));
            }
            upd2.update(&snap);
            is_killed
        };
```
并在 spawn 块顶部 clone writer（`let log_for_guard = log_path.clone();` 附近）加：
```rust
    let writer2 = writer.clone();
```
`jobs.rs` 底部 helper（`fn today_compact` 附近）加：
```rust
/// Job 终态 → JobEvent（done/failed）。killed 不调用此函数（人为终止不落 jsonl）。
fn terminal_event(j: &Job) -> JobEvent {
    let (status, code, reason) = match &j.status {
        JobStatus::Done { code } => ("done", Some(*code), None),
        JobStatus::Failed { reason } => ("failed", None, Some(reason.clone())),
        _ => ("failed", None, Some("未知终态".into())),
    };
    let tail = if j.log_path.as_os_str().is_empty() { None } else { Some(read_tail(&j.log_path)) };
    JobEvent { schema: 1, id: j.id.clone(), kind: j.kind, label: j.label.clone(),
        status: status.into(), started_at: j.started_at, finished_at: j.finished_at,
        code, tail, note: None, reason, deps: vec![] }
}
```

- [ ] **Step 4: spawn_agent 经 sub_ctx.job_writer 写事件（DRY）**

`subagents.rs:372` 签名不变（经 `sub_ctx`）。在 register 块（`:388-405`）内、`sub_ctx.job_update.update(&snap);`（:407）之前加：
```rust
    sub_ctx.job_writer.append(jobs::JobEvent::started(&snap));
```
spawned task 终态块（`:466` `r.finish(id, status, now_ms());` 之后、`if let Some(j) = r.jobs.get_mut(&id)` 之前）加写终态事件。在 `let (answer, suppress, already) = { ... };` 块内、`drop` 隐含前取 snap 写事件——改为块末尾补：
```rust
                r.finish(id, status, now_ms());
                if let Some(j) = r.jobs.get_mut(&id) { j.answer = Some(answer.clone()); }
                let snap_for_event = r.get(id).cloned();
                let sup = r.get(id).map(|j| j.suppress_inject).unwrap_or(false);
                (answer, sup, false, snap_for_event)
```
（元组多带一个 `snap_for_event`——需同步改解构：`let (answer, suppress, already, snap_opt) = { ... };`，且 `already=true` 分支返回 `(.., None)`。）
然后在 spawned task 里 `if !already && !(cancelled && suppress) { ... done2.send ... }` 之前加：
```rust
        if let Some(snap) = snap_opt {
            if !cancelled { sub_ctx2.job_writer.append(jobs::JobEvent::started(&snap)); } // placeholder 注：实际写终态
        }
```
**注**：subagents.rs 没有 `terminal_event`（它在 jobs.rs 是私有）——把 `jobs.rs` 的 `terminal_event` 改 `pub(crate)`，subagents.rs 调 `jobs::terminal_event(&snap)`。spawned task 顶部 clone：`let writer3 = sub_ctx.job_writer.clone();`（sub_ctx 在 move 进 task 前需 clone writer）。

> **实现者注意：** subagents.rs 的 spawned task 已经 `move`，需在 `tokio::spawn(async move {...})` 之前 `let writer_for_task = sub_ctx.job_writer.clone();` 并在 task 内用 `writer_for_task`（非 `sub_ctx.job_writer`，因 sub_ctx 部分字段已 move）。终态事件写法 = `writer_for_task.append(jobs::terminal_event(&snap))`，仅在 `!cancelled` 且 `!already` 时写（与 should_send 一致；cancelled 已有 note 走 JobOutcome，jsonl 不重复记 killed）。

- [ ] **Step 5: ToolsCtx 加 job_writer 字段 + 所有构造点补**

`tools.rs:50`（`pub history: ...` 之后）加：
```rust
    /// v2：job 持久化单写句柄。spawn_process_job / spawn_agent 共用（Q2 DRY）。
    /// foreground()/测试用 noop()；spawn_session 注入真实 writer。
    pub job_writer: crate::jobs::JobWriterHandle,
```

`tools.rs:67` foreground（`history: ...noop(),` 之后）加：
```rust
            job_writer: crate::jobs::JobWriterHandle::noop(),
```

`tools.rs:440-460` tool_bash 调 spawn_process_job（`:455`）补 writer 参数：
```rust
                label_for(&command), ctx.jobs.clone(), ctx.job_done_tx.clone(), ctx.job_update.clone(),
                ctx.job_writer.clone(),
```

`tools.rs:270-280` tool_subagent 调 spawn_agent——spawn_agent 经 sub_ctx，不直接传 writer，但 `clone_ctx`（subagents.rs:600）要带 job_writer。

`subagents.rs:600` `clone_ctx`（`history: src.history.clone(),` 之后）加：
```rust
        job_writer: src.job_writer.clone(),
```
`subagents.rs:208` 测试 `agent_ctx`（`history` 字段附近——agent_ctx 用 foreground 构造，foreground 已带 noop writer，无需改；确认 foreground 补字段后编译过）。

`agent.rs:163-173` spawn_session 构造 ToolsCtx——先 spawn writer 再构造 ctx：
```rust
        let history = crate::history::spawn_writer((*history_dir_rc).clone(), crate::history::local_offset_secs());
        let job_writer = crate::jobs::spawn_job_writer((*cache_rc).join(".ovoice-jobs"), crate::history::local_offset_secs());
        // writer 入 app state：force_quit（T5）从 state 取，复用主 writer 保单写不变量（不另 spawn）
        app.manage(job_writer.clone());
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
        };
```

`agent.rs:343-351` tests `ctx_with`（`history: h,` 之后）加：
```rust
            job_writer: crate::jobs::JobWriterHandle::noop(),
```

- [ ] **Step 6: 修既有 spawn 测试调用（补 writer 参数）**

`jobs.rs` 所有 `spawn_process_job(...)` 测试调用（`spawn_echo_completes_done`/`spawn_writes_log_to_cache_not_cwd`/`spawn_timeout_marks_failed`/`spawn_pipe_drain_does_not_block_on_large_output`/`spawn_rejected_at_cap`/`kill_does_not_emit_jobdone`）——末尾 `Arc::new(NoopJobUpdate)` 后加 `jobs::JobWriterHandle::noop()`（或测试内 `spawn_job_writer` 真写）。

`subagents.rs` spawn_agent 测试（经 `agent_ctx`，foreground 已带 noop writer，签名不变，**无需改调用**——确认编译过即可）。

- [ ] **Step 7: 编译 + 全测试**

Run: `cargo check --tests --manifest-path src-tauri/Cargo.toml`
Expected: 0 error。
Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml`
Expected: 全 PASS（含 `spawn_process_writes_running_then_done_events`）。

- [ ] **Step 8: Commit**

```bash
git add -A && git commit -m "feat(jobs): spawn 写 job jsonl（process+agent 共用 writer，T3 DRY）

ToolsCtx 加 job_writer 字段；spawn_session 启动 spawn_job_writer。
spawn_process_job/spawn_agent register 时写 running 事件、终态写
done/failed（killed 不落 jsonl，与不唤醒模型一致）。terminal_event
pub(crate) 供 subagents 复用（Q2 DRY）。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 4: 启动 load 今日 → 重建 registry + counter 恢复 + 悬空 running 标 forced_exit

**Goal：** `spawn_session` 启动时 `load_today`：读今日 jsonl → fold → 把 job 填回 registry（含恢复 `next_seq` = 今日 max+1）+ 悬空 running（jsonl 最后一条是 running）标 `failed+reason=forced_exit` 并补写 jsonl。

**Files:**
- Modify: `src-tauri/src/jobs.rs`（新增 `load_today` + `apply_event_to_registry` helper）
- Modify: `src-tauri/src/agent.rs:160-187`（spawn_session async block 调 load_today）

**Interfaces:**
- Consumes: T2 `read_jobs_jsonl`/`fold_by_id`、T3 `JobWriterHandle`
- Produces: `pub fn load_today(jobs_dir: &Path, offset_secs: i64, registry: &mut JobRegistry, writer: &JobWriterHandle)`。

- [ ] **Step 1: 写失败测试（load 恢复 counter + 悬空 running 标 forced_exit + fold 正确）**

`jobs.rs` 测试加：
```rust
    #[tokio::test]
    async fn load_today_restores_seq_and_marks_forced_exit() {
        let dir = tempfile::tempdir().unwrap();
        let jd = dir.path().join(".ovoice-jobs");
        // session 1：写 2 个完成 + 1 个悬空 running（app 强制退出没写终态）
        let w1 = spawn_job_writer(jd.clone(), 0);
        let today = crate::history::date_from_ts_local(now_ms(), crate::history::local_offset_secs());
        w1.append(je("20260727-01", "running", now_ms()));
        let mut d1 = je("20260727-01", "done", now_ms()); d1.code = Some(0); d1.finished_at = Some(now_ms());
        w1.append(d1);
        w1.append(je("20260727-02", "running", now_ms())); // 悬空（无终态）
        w1.append(je("20260727-03", "running", now_ms()));
        let mut d3 = je("20260727-03", "done", now_ms()); d3.code = Some(0); d3.finished_at = Some(now_ms());
        w1.append(d3);
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        drop(w1);

        // session 2：load
        let w2 = spawn_job_writer(jd.clone(), 0);
        let mut r = JobRegistry::new();
        load_today(&jd, 0, &mut r, &w2);
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        // counter 恢复：今日 max seq=3 → next_seq=4
        assert_eq!(r.next_seq, 4, "next_seq 应 = 今日 max+1");
        // 02 悬空 → forced_exit failed
        let j02 = r.get("20260727-02").unwrap();
        assert!(matches!(&j02.status, JobStatus::Failed { reason } if reason == "forced_exit"),
            "悬空 running 应标 forced_exit，实际: {:?}", j02.status);
        // 01/03 done 正常
        assert!(matches!(r.get("20260727-01").unwrap().status, JobStatus::Done { .. }));
        assert!(matches!(r.get("20260727-03").unwrap().status, JobStatus::Done { .. }));
        // forced_exit 事件已补写 jsonl（02 最后一条应是 failed+forced_exit）
        let evs2 = read_jobs_jsonl(&jd, &today);
        let j02_ev = evs2.iter().filter(|e| e.id == "20260727-02").last().unwrap();
        assert_eq!(j02_ev.status, "failed");
        assert_eq!(j02_ev.reason.as_deref(), Some("forced_exit"));
    }

    #[tokio::test]
    async fn load_today_empty_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let jd = dir.path().join(".ovoice-jobs");
        let w = spawn_job_writer(jd.clone(), 0);
        let mut r = JobRegistry::new();
        load_today(&jd, 0, &mut r, &w); // 无文件不 panic
        assert_eq!(r.next_seq, 1);
        assert!(r.jobs.is_empty());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml jobs::tests::load_today_restores_seq_and_marks_forced_exit`
Expected: 编译失败（`load_today` 未定义）。

- [ ] **Step 3: 实现 load_today**

`jobs.rs`（`fold_by_id` 之后）加：
```rust
/// 启动 load 今日 job jsonl → fold → 填 registry（已完成/失败态）+ 恢复 next_seq
/// （今日 max 序号 +1）+ 悬空 running（最后一条 status=running）标 failed+forced_exit
/// 并补写 jsonl（spec §5.2：重启汇报用，阶段3 触发 turn；阶段2 仅标记+显示）。
pub fn load_today(jobs_dir: &Path, offset_secs: i64, registry: &mut JobRegistry, writer: &JobWriterHandle) {
    let today = crate::history::date_from_ts_local(now_ms(), offset_secs);
    let evs = read_jobs_jsonl(jobs_dir, &today);
    if evs.is_empty() { return; }
    let folded = fold_by_id(&evs);
    let today_compact_str = today.replace('-', "");
    let mut max_seq: u64 = 0;
    for (id, e) in &folded {
        // 仅今日 id（同前缀 YYYYMMDD-）填回
        let prefix = format!("{today_compact_str}-");
        if !id.starts_with(&prefix) { continue; }
        // 解析序号恢复 counter
        if let Some(seq_part) = id.strip_prefix(&prefix) {
            if let Ok(n) = seq_part.parse::<u64>() { if n > max_seq { max_seq = n; } }
        }
        let status = parse_status_from_event(e);
        let job = Job {
            id: id.clone(), kind: e.kind, label: e.label.clone(),
            status: status.clone(), log_path: jobs_dir.join(format!("{id}.log")),
            started_at: e.started_at, finished_at: e.finished_at,
            cancel: None, progress: None,
            answer: e.tail.clone(),  // 进程 tail 暂存 answer 位（ Jobs 面板不显示 answer for process）
            suppress_inject: false,
        };
        registry.jobs.insert(id.clone(), job);
        // 悬空 running → 标 forced_exit + 补写 jsonl
        if matches!(status, JobStatus::Running) {
            if let Some(j) = registry.jobs.get_mut(id) {
                j.status = JobStatus::Failed { reason: "forced_exit".into() };
                j.finished_at = Some(now_ms());
            }
            writer.append(JobEvent {
                schema: 1, id: id.clone(), kind: e.kind, label: e.label.clone(),
                status: "failed".into(), started_at: e.started_at, finished_at: Some(now_ms()),
                code: None, tail: None, note: Some("app 强制退出时未完成".into()),
                reason: Some("forced_exit".into()), deps: vec![],
            });
        }
    }
    registry.current_day = today_compact_str;
    registry.next_seq = max_seq + 1;
}

/// JobEvent.status 字符串 → JobStatus。
fn parse_status_from_event(e: &JobEvent) -> JobStatus {
    match e.status.as_str() {
        "running" => JobStatus::Running,
        "done" => JobStatus::Done { code: e.code.unwrap_or(0) },
        "failed" => JobStatus::Failed { reason: e.reason.clone().unwrap_or_else(|| "未知".into()) },
        "killed" => JobStatus::Killed,
        other => JobStatus::Failed { reason: format!("未知 status: {other}") },
    }
}
```

- [ ] **Step 4: spawn_session 接 load_today**

`agent.rs:160-185` async block（`let job_writer = ...spawn_job_writer(...)` 之后、`let ctx = ToolsCtx {...}` 之前）加：
```rust
        // 启动 load 今日 job jsonl：重建 registry + 恢复 counter + 悬空 running 标 forced_exit
        // （spec §5.2：阶段2 仅 load+标记+显示；主动触发汇报 turn 属阶段3）
        {
            let mut r = registry.lock().unwrap();
            crate::jobs::load_today(&(*cache_rc).join(".ovoice-jobs"), crate::history::local_offset_secs(), &mut r, &job_writer);
        }
```

- [ ] **Step 5: 编译 + 测试**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml jobs::tests`
Expected: 全 PASS（含 load_today 2 测试）。

- [ ] **Step 6: 手测准备（dev server 由用户跑）**

subagent 无法跑 GUI。load 后 Jobs 面板要看到当天任务，需用户手测：dev server → 跑个后台任务 → 关 app → 重开 → Jobs 面板应显示该任务 +（若未正常完成）标 failed+forced_exit。

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "feat(jobs): 启动 load 今日 jsonl → 重建 registry + counter 恢复（T4）

load_today：读今日 jsonl fold → 填回 registry jobs + next_seq = 今日
max+1 + 悬空 running（最后一条 running）标 failed+reason=forced_exit
并补写 jsonl。spawn_session async block 启动时调（阶段边界：仅 load+
标记+显示，不唤醒主 agent——汇报 turn 属阶段3）。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 5: 关闭流程（CloseRequested 拦截 + 前端确认 + force_quit 标 forced_exit）

**Goal：** `on_window_event` 拦 `CloseRequested`：有 running job → `prevent_close` + emit `confirm-close`；前端弹确认（列 running）→ 确认调 `force_quit` 命令（全 running 标 failed+forced_exit 写 jsonl + `app.exit(0)`）；取消则不动（已 prevent_close）。

**Files:**
- Modify: `src-tauri/src/lib.rs:778-795`（on_window_event 加 CloseRequested 分支）
- Modify: `src-tauri/src/lib.rs:117` 之后（新增 force_quit 命令）+ `:796` invoke_handler 注册
- Modify: `src-tauri/src/jobs.rs`（新增 `mark_running_as_forced_exit` helper）
- Modify: `src/main.js`（listen confirm-close + 确认对话框 + 调 force_quit）

**Interfaces:**
- Consumes: T3 `JobWriterHandle`、T4 `load_today` 同款 forced_exit 写法
- Produces: `#[tauri::command] async fn force_quit(app: AppHandle) -> Result<(), String>`；`jobs::mark_running_as_forced_exit(&registry, &writer)`。

- [ ] **Step 1: 写失败测试（force_quit 标 running 全 failed+forced_exit + 写 jsonl）**

`jobs.rs` 测试加：
```rust
    #[test]
    fn mark_running_as_forced_exit_marks_and_writes() {
        let dir = tempfile::tempdir().unwrap();
        let jd = dir.path().join(".ovoice-jobs");
        let w = spawn_job_writer(jd.clone(), 0);
        let mut r = JobRegistry::new();
        let id1 = r.register(JobKind::Process, "p".into(), jd.join("1.log"), now_ms());
        let _id2 = r.register(JobKind::Agent, "a".into(), PathBuf::new(), now_ms());
        let registry: SharedRegistry = Arc::new(Mutex::new(r));
        mark_running_as_forced_exit(&registry, &w);
        let r = registry.lock().unwrap();
        assert!(matches!(&r.get(&id1).unwrap().status, JobStatus::Failed { reason } if reason=="forced_exit"));
        assert!(r.running == 0 && r.running_agents == 0, "并发槽应释放");
        drop(r);
        // jsonl 补写 forced_exit 事件（flush 由 writer task 异步——此单测验 registry 状态即可，
        // jsonl 落盘在 load_today 测试已覆盖 forced_exit 写法）
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml jobs::tests::mark_running_as_forced_exit_marks_and_writes`
Expected: 编译失败（`mark_running_as_forced_exit` 未定义）。

- [ ] **Step 3: 实现 mark_running_as_forced_exit**

`jobs.rs`（`load_today` 之后）加：
```rust
/// 退出时所有 running job 标 failed+reason=forced_exit + 补写 jsonl（spec §5.1）。
/// 释放并发槽。用于 force_quit 命令。
pub fn mark_running_as_forced_exit(registry: &SharedRegistry, writer: &JobWriterHandle) -> usize {
    let mut count = 0;
    let snaps: Vec<Job> = {
        let mut r = registry.lock().unwrap();
        let to_mark: Vec<JobId> = r.jobs.iter()
            .filter(|(_, j)| matches!(j.status, JobStatus::Running))
            .map(|(id, _)| id.clone()).collect();
        for id in &to_mark {
            if let Some(j) = r.jobs.get_mut(id) {
                j.status = JobStatus::Failed { reason: "forced_exit".into() };
                j.finished_at = Some(now_ms());
                count += 1;
            }
        }
        // 释放并发槽：重算 running/running_agents（finish 已置终态但未走 finish() 计数）
        r.running = r.jobs.values().filter(|j| matches!(j.kind, JobKind::Process) && matches!(j.status, JobStatus::Running)).count();
        r.running_agents = r.jobs.values().filter(|j| matches!(j.kind, JobKind::Agent) && matches!(j.status, JobStatus::Running)).count();
        to_mark.iter().filter_map(|id| r.get(id).cloned()).collect()
    };
    for j in snaps {
        writer.append(JobEvent {
            schema: 1, id: j.id.clone(), kind: j.kind, label: j.label.clone(),
            status: "failed".into(), started_at: j.started_at, finished_at: j.finished_at,
            code: None, tail: None, note: Some("app 强制退出时未完成".into()),
            reason: Some("forced_exit".into()), deps: vec![],
        });
    }
    count
}
```

- [ ] **Step 4: lib.rs 加 force_quit 命令 + on_window_event 拦 CloseRequested**

`lib.rs:122` 之后（read_job_log 之后）加命令：
```rust
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
```

`lib.rs:778` on_window_event（在 `if let tauri::WindowEvent::DragDrop(drag) = event { ... }` 之后、闭包结束前）加：
```rust
            if let tauri::WindowEvent::CloseRequested(api) = event {
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
```

`lib.rs:810`（invoke_handler 的 `read_job_log,` 之后）注册：
```rust
            force_quit,
```

- [ ] **Step 5: 编译 + 测试**

Run: `cargo check --tests --manifest-path src-tauri/Cargo.toml && cargo test --lib --manifest-path src-tauri/Cargo.toml jobs::tests::mark_running_as_forced_exit_marks_and_writes`
Expected: 0 error + 测试 PASS。

- [ ] **Step 6: main.js 加确认对话框 + 调 force_quit**

`src/main.js` 在 `setupAgentEvents` 内（`await listen("job-callback", ...)` 之后，约 :1680）加：
```js
  // 关闭确认：后端 CloseRequested 检测到 running job → emit confirm-close
  await listen("confirm-close", async (e) => {
    const jobs = (e.payload && e.payload.jobs) || [];
    if (!jobs.length) return;
    const list = jobs.map((j) => `  · #${j.id}（${j.kind === "agent" ? "子代理" : "后台任务"}）${j.label || ""}`).join("\n");
    const ok = confirm(`还有 ${jobs.length} 个任务在跑：\n${list}\n\n确定退出？退出后这些任务判失败（forced_exit）。`);
    if (ok) {
      try { await invoke("force_quit"); }
      catch (err) { alert("强制退出失败: " + err); }
    }
    // 取消则不调 force_quit——后端已 prevent_close，窗保持开
  });
```

- [ ] **Step 7: node --check**

Run: `node --check src/main.js`
Expected: 无输出（exit 0）。

- [ ] **Step 8: Commit**

```bash
git add -A && git commit -m "feat(jobs): 关闭流程——CloseRequested 拦截 + 前端确认 + force_quit（T5）

on_window_event 拦 CloseRequested：有 running job → prevent_close +
emit confirm-close（带 job 列表）；前端 confirm 对话框 → 确认调
force_quit（全 running 标 failed+forced_exit 写 jsonl + app.exit）；
取消则窗保持开。mark_running_as_forced_exit 释放并发槽。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 6: legacy 迁移（旧数字 `.log` → `legacy-<n>.log`）

**Goal：** 启动扫描 `.ovoice-jobs/*.log`，纯数字文件名（旧 u64 时代）→ 重命名 `legacy-<n>.log` + 写今日 jsonl 一条 legacy 记录（`id=legacy-<n>`, `status=failed`, `reason=legacy`），Jobs 面板可见 + `read_job_log("legacy-<n>")` 可读。

**Files:**
- Modify: `src-tauri/src/jobs.rs`（新增 `migrate_legacy_logs`）
- Modify: `src-tauri/src/agent.rs`（spawn_session 调 migrate_legacy_logs，在 load_today 之前）

**Interfaces:**
- Consumes: T2 `JobWriterHandle`、`.ovoice-jobs` 目录约定
- Produces: `pub fn migrate_legacy_logs(jobs_dir: &Path, offset_secs: i64, writer: &JobWriterHandle)`。

- [ ] **Step 1: 写失败测试**

`jobs.rs` 测试加：
```rust
    #[tokio::test]
    async fn migrate_legacy_renames_numeric_logs_and_records() {
        let dir = tempfile::tempdir().unwrap();
        let jd = dir.path().join(".ovoice-jobs");
        std::fs::create_dir_all(&jd).unwrap();
        std::fs::write(jd.join("1.log"), b"old output 1").unwrap();
        std::fs::write(jd.join("42.log"), b"old output 42").unwrap();
        std::fs::write(jd.join("20260727-01.log"), b"new format").unwrap(); // 不动
        let w = spawn_job_writer(jd.clone(), 0);
        migrate_legacy_logs(&jd, 0, &w);
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        // 旧文件已改名
        assert!(jd.join("legacy-1.log").exists(), "1.log → legacy-1.log");
        assert!(jd.join("legacy-42.log").exists(), "42.log → legacy-42.log");
        assert!(!jd.join("1.log").exists() && !jd.join("42.log").exists(), "旧名应消失");
        assert!(jd.join("20260727-01.log").exists(), "新格式 .log 不动");
        // 今日 jsonl 有 2 条 legacy 记录
        let today = crate::history::date_from_ts_local(now_ms(), crate::history::local_offset_secs());
        let evs = read_jobs_jsonl(&jd, &today);
        let legacy_ids: Vec<&String> = evs.iter().map(|e| &e.id).filter(|id| id.starts_with("legacy-")).collect();
        assert_eq!(legacy_ids.len(), 2, "应记 2 条 legacy");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml jobs::tests::migrate_legacy_renames_numeric_logs_and_records`
Expected: 编译失败（`migrate_legacy_logs` 未定义）。

- [ ] **Step 3: 实现 migrate_legacy_logs**

`jobs.rs`（`mark_running_as_forced_exit` 之后）加：
```rust
/// 迁移旧 u64 时代的 {n}.log → legacy-{n}.log（spec D5：旧 .log 保留，数字 id → legacy-<n>）。
/// 纯数字文件名才迁；新格式（YYYYMMDD-NN.log / legacy-N.log）不动。给每个旧 log 写一条
/// legacy job 记录（status=failed, reason=legacy）到今日 jsonl，Jobs 面板可见。
/// 幂等：已迁移过的（无纯数字 .log）下次启动 no-op。
pub fn migrate_legacy_logs(jobs_dir: &Path, offset_secs: i64, writer: &JobWriterHandle) {
    let entries = match std::fs::read_dir(jobs_dir) { Ok(rd) => rd, Err(_) => return };
    let today = crate::history::date_from_ts_local(now_ms(), offset_secs);
    for e in entries.flatten() {
        let path = e.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else { continue };
        if ext != "log" { continue; }
        // 仅纯数字文件名（旧 u64 id）才迁
        if !stem.chars().all(|c| c.is_ascii_digit()) || stem.is_empty() { continue; }
        let new_path = jobs_dir.join(format!("legacy-{stem}.log"));
        if std::fs::rename(&path, &new_path).is_err() { continue; }
        let mtime = std::fs::metadata(&new_path).ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64).unwrap_or_else(now_ms);
        writer.append(JobEvent {
            schema: 1, id: format!("legacy-{stem}"), kind: JobKind::Process,
            label: format!("(旧任务 #{stem})"), status: "failed".into(),
            started_at: mtime, finished_at: Some(mtime),
            code: None, tail: None, note: Some("旧版本任务，已迁移".into()),
            reason: Some("legacy".into()), deps: vec![],
        });
        let _ = &today; // 事件由 writer 按 started_at(=mtime) 选文件，通常落 mtime 当日
    }
}
```

- [ ] **Step 4: spawn_session 调 migrate_legacy_logs**

`agent.rs`（T4 加的 `load_today` 块之前）加：
```rust
        // legacy 迁移：旧 u64 时代的 {n}.log → legacy-{n}.log + 写 legacy 记录（spec D5）
        {
            let jobs_dir = (*cache_rc).join(".ovoice-jobs");
            crate::jobs::migrate_legacy_logs(&jobs_dir, crate::history::local_offset_secs(), &job_writer);
        }
```
（在 `let job_writer = ...spawn_job_writer(...)` 之后、load_today 之前。顺序：spawn writer → migrate（可能写 legacy 事件）→ load_today（读今日含 legacy 记录）→ 重建 registry。）

- [ ] **Step 5: 编译 + 测试**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml jobs::tests::migrate_legacy_renames_numeric_logs_and_records`
Expected: PASS。

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat(jobs): legacy 迁移——旧 {n}.log → legacy-<n>.log（T6 D5）

启动扫描 .ovoice-jobs/，纯数字文件名（旧 u64 时代）→ rename legacy-
<n>.log + 写今日 jsonl 一条 legacy 记录（failed+reason=legacy）。
新格式 .log 不动。幂等。spawn_session 顺序：spawn writer → migrate
→ load_today。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 7: AGENT.md 更新（job_id 格式 + read_job_log 说明）

**Goal：** 更新 `src-tauri/defaults/AGENT.md`（+ `cache/AGENT.md` 同步）的「后台任务协议」段，补 `job_id` 格式 `YYYYMMDD-NN` 说明 + `read_job_log(id)` 用 String id。pinned 文件，每轮重读，无需重编译。

**Files:**
- Modify: `src-tauri/defaults/AGENT.md`（「后台任务协议」段）
- Modify: `cache/AGENT.md`（运行时副本同步——若存在）

- [ ] **Step 1: 读现状定位「后台任务协议」段**

Run: `grep -n "后台任务协议" src-tauri/defaults/AGENT.md cache/AGENT.md`
找到 T4（阶段1）加的段落。读其内容确认格式。

- [ ] **Step 2: Edit 补 job_id 格式说明**

在「后台任务协议」段补一条（job_id 现是 `YYYYMMDD-NN` 字符串，如 `20260727-01`）：
```markdown
- job_id 现是日期序号字符串 `YYYYMMDD-NN`（如 `20260727-01`），不再是纯数字。重启后当天任务会重新加载显示；跨日序号从 01 重新开始。`read_job_log(id)` 用此字符串 id 读取完整进程日志。
```

- [ ] **Step 3: 同步 cache/AGENT.md**

若 `cache/AGENT.md` 存在（运行时 pinned 副本），同步同样改动。若不存在，跳过（load_pinned 会从 defaults 读）。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/defaults/AGENT.md cache/AGENT.md 2>/dev/null
git commit -m "docs(agent): AGENT.md 补 job_id YYYYMMDD-NN 格式说明（T7）

后台任务协议段补：job_id 是日期序号字符串（非数字）、重启加载当天、
read_job_log 用字符串 id。pinned 文件每轮重读，无需重编译。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Self-Review

**1. Spec coverage（spec §1/§4/§5 + 改动清单-阶段2 + 测试矩阵-阶段2）：**
- §1 job_id 格式（YYYYMMDD-NN / u64→String / NN / 跨日 reset / 重启恢复 / 超 99）→ **T1 + T4**（T1 生成算法 + T4 load 恢复）。
- §4 持久化（按日 jsonl / append-only / schema / fold / 跟 spawn 日 / 字段清单）→ **T2 + T3**（T2 writer/schema/fold + T3 接线写）。
- §4 Q2 DRY（jobs+subagents 共用 writer）→ **T3**（JobWriterHandle 进 ToolsCtx，process+agent 共用）。
- §5.1 关闭流程（CloseRequested + 弹确认 + forced_exit 标 failed + 放行/取消）→ **T5**。
- §5.2 启动 load（读今日 → fold → 重建 registry → Jobs 面板；forced-exit failed 标记）→ **T4**。
- §5.2 重启汇报（主动触发 turn）→ **阶段边界：不在本 plan**（依赖阶段3 调度模型，spec 改动清单阶段3 明确）。
- D5 legacy 迁移（数字 id → legacy-<n>，旧 .log 保留）→ **T6**。
- 测试矩阵-阶段2（round-trip/schema/跨天/并发写、job_id 跨日 reset/重启恢复/超99/legacy、关闭流程弹确认/强制退出标 failed/取消阻止）→ 散布 T1/T2/T4/T5/T6 测试。**并发写安全**复用 history.rs 已验证的单写者模式（T2 照抄），未单列新测（writer 模式同源，已有 history.rs 覆盖）。
- 改动清单-阶段2 各项（jobs.rs/subagents.rs/lib.rs/main.js/迁移）→ 全覆盖。

**2. Placeholder scan：** 无 TBD/TODO；每 Step 给 complete Rust/JS 代码或 exact old/new。**注**：T3 Step 4 subagents.rs spawned task 接线较细（元组多带 snap_opt + writer clone），实现者须照「实现者注意」框处理——这是复杂接线而非 placeholder，给了明确路径。

**3. Type consistency：**
- `JobId = String` 全链路一致（T1 定，T2-T7 用）。
- `JobEvent` schema 字段（schema/id/type/label/status/started_at/finished_at/code/tail/note/reason/deps）T2 定、T3-T6 用，一致。
- `JobWriterHandle::append(JobEvent)` + `noop()` T2 定、T3/T4/T5/T6 用，一致。
- `terminal_event` T3 定为 `pub(crate)` 供 subagents.rs 用——**注意**：T3 Step 3 把它留 jobs.rs 私有，Step 4 又说改 `pub(crate)`，实现者须确保改 `pub(crate) fn terminal_event`（subagents.rs 跨模块调）。
- `load_today`/`mark_running_as_forced_exit`/`migrate_legacy_logs` 签名（`&Path`/`&SharedRegistry`/`&JobWriterHandle`）T4/T5/T6 定，agent.rs 调用一致。
- `force_quit` 命令 + `confirm-close` 事件 + `force_quit` invoke 前后端命名一致。

**4. 风险/边界提示（实现者 + reviewer 注意）：**
- **T3 接线最复杂**（ToolsCtx 加字段波及所有构造点：spawn_session/foreground/agent_ctx/tests/clone_ctx）。实现者跑全量 `cargo check --tests` 确认无遗漏构造点。
- **T4 `Job.answer = e.tail`** 是妥协（process 的 tail 暂存 answer 位让 Jobs 面板能显示片段）——reviewer 可质疑，但阶段2 Jobs 面板不强求 process tail 显示（read_job_log 拉全文），此赋值仅为不丢信息。可接受。
- **T5 `force_quit` 的 writer flush**：app.exit(0) 后 writer task 可能未 flush 完。用 150ms sleep 兜底（spec 数据保留——尽力落盘，丢也不致命，下次启动 load 时该 job 还是 running → 再标 forced_exit）。
- **dev server 锁 exe**：全程 `cargo check --tests` + `cargo test --lib`，不 `cargo build`/`cargo run`。手测（关闭确认 UI / load 后 Jobs 面板）由用户在 dev server 跑。

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-27-job-persistence.md`. Two execution options:

**1. Subagent-Driven (recommended)** — 与阶段1 一致：每任务派 fresh implementer + task reviewer，T1→T7 顺序执行（依赖链线性），最后 whole-branch review。

**2. Inline Execution** — 本会话内 executing-plans 批量执行 + checkpoint。

**Which approach?**
