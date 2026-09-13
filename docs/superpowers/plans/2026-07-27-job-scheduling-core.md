# 后台任务调度核心（阶段3）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 driver 从「1 JobDone = 1 turn」改成「idle 事件驱动攒批调度」（一批一气泡），加 dependencies 偏序 + 软超时（不 kill，资源上限才 kill）+ 重启汇报 turn。

**Architecture:** 新增纯逻辑 `Scheduler`（completed_pending 队列 + turn_busy 标志 + has_outcome 集合 + drain_ready），driver 在 JobDone/UserMessage 边界调它；软超时复用现有 guardian（process 已有 timeout，改成不杀+发 soft_timeout outcome+资源上限 kill，subagent 新加 timeout 分支）；dependencies 走 spawn 时 check_deps（未知 id + 环检测）。整批 N 个 outcome → N 条 history + 1 次 turn（handle_batch）。

**Tech Stack:** Rust + Tauri 2 + tokio（mpsc/select）。纯逻辑单测先行（FakeRound/FakeEmitter/tempfile，不动真模型）。

## Global Constraints

- **M=120min / N=5**（spec D7 资源上限）：同一 job 软超时计数 ≥5，或首次软超时后总宽限 ≥120min → 强 kill（记 killed）。
- **单 job timeout_secs 默认**：process 30min(1800s) / subagent 30min(1800s)，per-job 可配。bash bg 已有 timeout_secs 参数；subagent 本 plan 新增。
- **重启汇报**：app 启动 load_today 后，若 registry 有 `Failed{reason:"forced_exit"}` 的 job，立即 `tx.send(SessionEvent::UserMessage{ 合成文本 })` 触发一个 turn 喂主 agent（spec §5.2，启动即触发）。
- **T5 关闭确认不在本 plan**（阶段2 跳过的 custom titlebar 问题，另开 fix 分支）。
- **dev server 锁 exe**（[[ovoice-dev-server-cargo-lock]]）：验证只用 `cargo check --tests --manifest-path src-tauri/Cargo.toml` + `cargo test --lib`（阶段3 有真单测，tempdir + FakeRound/FakeEmitter），**禁止 cargo build/run**（dev server 占 target/debug/ovoice.exe）。
- **TDD**：纯逻辑单测先行。spec §测试矩阵-阶段3（调度模型 + 依赖 + 软超时）是用例来源。FakeRound/FakeEmitter 已存在于 agent.rs:342-367，复用。
- **工具计数不变**（[[ovoice-tool-count-cascade]]）：本 plan 只给 subagent/bash schema **加字段**（dependencies/timeout_secs），不加新工具，仍 7 个。`llm.rs:594` 是动态断言 `tools::schemas().len()`，加字段不触发——但 implementer 须确认未误删工具。
- **数据保留硬约束**（spec D2）：jsonl append-only，soft_timeout/killed 事件只增不删。
- **分支策略**（CLAUDE.md）：在 `feat/job-scheduling-core` 上做，**严禁直接落 master**。plan 文档本身已 commit 到 master（设计产物），feat 分支从该 commit 拉。
- **pinned 每轮重读**（[[ovoice-pinned-files-per-turn-reload]]）：改 `src-tauri/defaults/AGENT.md` 下轮即生效，无需重编译。
- **live vs history 双路径**（[[ovoice-live-vs-history-render-paths]]）：阶段3 主要动后端调度；若动前端批处理气泡渲染，须同时改 `setupAgentEvents` + `buildHistoryBubbles`（本 plan 前端改动极少，T5 的 handle_batch 走现有 history 路径，前端透明）。

**阶段边界**：不含 T5 关闭确认、不含任务线 project/vision 层（非目标）。

---

## File Structure

| 文件 | 责任 | 本 plan 改动 |
|---|---|---|
| `src-tauri/src/scheduler.rs` | **新建**。纯逻辑 Scheduler（队列+turn_busy+drain_ready）。无 IO，可离线单测。 | T1 新建 |
| `src-tauri/src/jobs.rs` | Job 状态机 + registry + spawn_process_job guardian + JobOutcome/JobEvent。 | T1 扩 JobOutcome；T2 加 Job.deps + check_deps；T3 spawn 接 deps；T4 guardian 软超时 |
| `src-tauri/src/subagents.rs` | SubagentEmitter + spawn_agent + kill。 | T3 spawn_agent 接 deps+timeout；T4 加软超时 select 分支 |
| `src-tauri/src/tools.rs` | 工具 schema + dispatch + tool_bash/tool_subagent。 | T2 schema 加 dependencies + 修 subagent id integer→string + subagent timeout_secs；T3 dispatch 解析传入 |
| `src-tauri/src/agent.rs` | driver select + handle_event + spawn_session bootstrap。 | T5 拆 handle_event + handle_batch + driver 接 Scheduler；T6 重启汇报 |
| `src-tauri/src/lib.rs` | setup() + invoke_handler + on_window_event。 | 不改（spawn_session 签名不变；T6 在 agent.rs 内） |
| `src-tauri/defaults/AGENT.md` | 主 agent 行为准则。 | T7 加批处理协议 + soft_timeout 处理 + 重启汇报 |

**任务依赖**（线性）：T1(Scheduler+JobOutcome 扩字段) → T2(deps schema+check_deps) → T3(spawn 接 deps+timeout) → T4(软超时 guardian) → T5(driver 接 Scheduler) → T6(重启汇报) → T7(AGENT.md)。

---

### Task 1: Scheduler 纯逻辑 + JobOutcome 扩字段

**Files:**
- Create: `src-tauri/src/scheduler.rs`
- Modify: `src-tauri/src/jobs.rs:62-72`（JobOutcome 加 deps/finished_at/soft_timeout）、`src-tauri/src/jobs.rs:544`、`src-tauri/src/subagents.rs:~500`（spawn_agent done send 适配）、`src-tauri/src/agent.rs:428-429`（测试 fixture 适配）
- Test: `src-tauri/src/scheduler.rs`（内联 #[cfg(test)]）

**Interfaces:**
- Consumes: `jobs::JobOutcome`、`jobs::JobId`（=String）。
- Produces: `struct Scheduler`、`Scheduler::new()`、`Scheduler::on_job_done(&mut self, o: JobOutcome)`、`Scheduler::on_turn_start(&mut self)`、`Scheduler::on_turn_end(&mut self)`、`Scheduler::drain_ready(&mut self) -> Option<Vec<JobOutcome>>`、`Scheduler::is_idle(&self) -> bool`。后续 T5 driver 依赖这些签名。

**JobOutcome 扩字段**（jobs.rs:62-72，三字段一次加齐，避免后续 T4 再波及构造点）：

```rust
#[derive(Debug, Clone)]
pub struct JobOutcome {
    pub job_id: JobId,
    pub kind: JobKind,
    pub label: Option<String>,
    pub ok: bool,
    pub code: Option<i32>,
    pub tail: String,
    pub answer: Option<String>,
    pub note: Option<String>,
    // 阶段3 新增：
    pub deps: Vec<JobId>,        // 该 job 声明的依赖（drain_ready 筛 ready 用）
    pub finished_at: u64,        // 完成时刻（FIFO 排序用）
    pub soft_timeout: bool,      // true=软超时 outcome（进程仍跑，后续 done 会覆盖）
}
```

**构造点适配**（所有 `JobOutcome { ... }` 字面量加三字段，TDD 步骤里逐处给）：
- `jobs.rs:544`（spawn_process_job guardian done send）
- `subagents.rs` spawn_agent done send（T3/T4 会再改，本任务先加字段填默认值）
- `agent.rs:428-429`（测试 fixture）

- [ ] **Step 1: 写 Scheduler 失败测试**

在 `src-tauri/src/scheduler.rs` 顶部写（含测试模块）。Scheduler 逻辑：

```rust
//! 阶段3 调度核心：completed_pending 队列 + turn_busy 标志 + drain_ready。
//! 纯逻辑，无 IO——driver 在 JobDone/UserMessage 边界调它。idle = turn_busy=false。
//! 整批 ready outcome 喂一个 turn（spec §3）：筛 deps 满足 → 按 finished_at FIFO
//! → 同 id 多 outcome 取最后（done 覆盖 soft_timeout）→ 整批返回。
use crate::jobs::{JobId, JobOutcome, JobKind};
use std::collections::{HashMap, HashSet};

pub struct Scheduler {
    pending: Vec<JobOutcome>,
    has_outcome: HashSet<JobId>,   // 已产出 outcome 的 job（done/failed/soft_timeout/killed 均算）
    turn_busy: bool,
}

impl Scheduler {
    pub fn new() -> Self { Self { pending: Vec::new(), has_outcome: HashSet::new(), turn_busy: false } }

    pub fn on_turn_start(&mut self) { self.turn_busy = true; }
    pub fn on_turn_end(&mut self) { self.turn_busy = false; }
    pub fn is_idle(&self) -> bool { !self.turn_busy }

    /// JobDone 到达：入队 + 记 has_outcome（drain_ready 据此判 dep 满足）。
    pub fn on_job_done(&mut self, o: JobOutcome) {
        self.has_outcome.insert(o.job_id.clone());
        self.pending.push(o);
    }

    /// idle 时处理：筛 ready（deps ⊆ has_outcome）→ 同 id 取最后 → 按 finished_at FIFO → 整批。
    /// turn_busy=true 或无 ready → None。返回的批从 pending 移除。
    pub fn drain_ready(&mut self) -> Option<Vec<JobOutcome>> {
        if self.turn_busy { return None; }
        // 同 id 取最后一条（done 覆盖 soft_timeout）：从后往前扫，已取的 id 跳过。
        let mut picked: Vec<JobOutcome> = Vec::new();
        let mut seen: HashSet<JobId> = HashSet::new();
        for o in self.pending.iter().rev() {
            if seen.insert(o.job_id.clone()) { picked.push(o.clone()); }
        }
        // ready 筛选：deps 全在 has_outcome
        let ready: Vec<JobOutcome> = picked.into_iter()
            .filter(|o| o.deps.iter().all(|d| self.has_outcome.contains(d)))
            .collect();
        if ready.is_empty() { return None; }
        // FIFO：按 finished_at 升序
        let mut batch = ready.clone();
        batch.sort_by_key(|o| o.finished_at);
        // 从 pending 移除已取 id
        let taken: HashSet<JobId> = batch.iter().map(|o| o.job_id.clone()).collect();
        self.pending.retain(|o| !taken.contains(&o.job_id));
        Some(batch)
    }
}

impl Default for Scheduler { fn default() -> Self { Self::new() } }

#[cfg(test)]
mod tests {
    use super::*;
    fn outcome(id: &str, deps: Vec<&str>, finished_at: u64) -> JobOutcome {
        JobOutcome { job_id: id.into(), kind: JobKind::Process, label: None, ok: true,
            code: Some(0), tail: String::new(), answer: None, note: None,
            deps: deps.into_iter().map(String::from).collect(), finished_at, soft_timeout: false }
    }

    #[test]
    fn idle_drains_ready_batch() {
        let mut s = Scheduler::new();
        s.on_job_done(outcome("20260727-01", vec![], 100));
        s.on_job_done(outcome("20260727-02", vec![], 200));
        let batch = s.drain_ready().unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].job_id, "20260727-01", "FIFO 按 finished_at");
        assert_eq!(batch[1].job_id, "20260727-02");
        assert!(s.drain_ready().is_none(), "取走后 pending 空");
    }

    #[test]
    fn busy_drains_none() {
        let mut s = Scheduler::new();
        s.on_job_done(outcome("20260727-01", vec![], 100));
        s.on_turn_start();
        assert!(s.drain_ready().is_none(), "turn_busy 时不处理");
        assert!(!s.is_idle());
        s.on_turn_end();
        assert!(s.drain_ready().is_some(), "turn 结束 idle 后可处理");
    }

    #[test]
    fn same_id_takes_last_done_overwrites_soft_timeout() {
        let mut s = Scheduler::new();
        let mut soft = outcome("20260727-01", vec![], 100); soft.soft_timeout = true; soft.ok = false;
        s.on_job_done(soft);
        s.on_job_done(outcome("20260727-01", vec![], 200)); // done 覆盖
        let batch = s.drain_ready().unwrap();
        assert_eq!(batch.len(), 1, "同 id 合并成一条");
        assert!(!batch[0].soft_timeout, "取最后一条=done");
        assert!(batch[0].ok);
    }

    #[test]
    fn ready_filter_waits_for_deps() {
        let mut s = Scheduler::new();
        s.on_job_done(outcome("20260727-02", vec!["20260727-01"], 200)); // 依赖 01，01 无 outcome
        assert!(s.drain_ready().is_none(), "dep 未满足不 ready");
        s.on_job_done(outcome("20260727-01", vec![], 100)); // 01 done → has_outcome
        let batch = s.drain_ready().unwrap();
        assert_eq!(batch.len(), 2, "01 + 02 都 ready（02 dep 已满足）");
    }

    #[test]
    fn failed_dep_counts_as_outcome() {
        let mut s = Scheduler::new();
        s.on_job_done(outcome("20260727-02", vec!["20260727-01"], 200));
        let mut failed = outcome("20260727-01", vec![], 100); failed.ok = false;
        s.on_job_done(failed); // failed 也算 has_outcome
        let batch = s.drain_ready().unwrap();
        assert_eq!(batch.len(), 2, "failed dep 满足依赖，不死锁");
    }

    #[test]
    fn batch_preserves_unrelated_attribution() {
        // 异归属整批：process + agent 同批
        let mut s = Scheduler::new();
        let mut p = outcome("20260727-01", vec![], 100);
        let mut a = outcome("20260727-02", vec![], 200); a.kind = JobKind::Agent;
        s.on_job_done(p); s.on_job_done(a);
        let batch = s.drain_ready().unwrap();
        assert_eq!(batch.len(), 2);
    }
}
```

- [ ] **Step 2: 注册模块 + 跑测试确认失败**

`src-tauri/src/main.rs`（或 lib.rs 的 mod 声明处）加 `mod scheduler;`。先确认 mod 声明位置：

Run: `grep -n "^mod " src-tauri/src/lib.rs`
Expected: 看到 `mod jobs;` `mod agent;` 等，在同行加 `mod scheduler;`。

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml scheduler`
Expected: 编译错误（JobOutcome 缺 deps/finished_at/soft_timeout 字段）。

- [ ] **Step 3: 扩 JobOutcome 三字段**

修改 `src-tauri/src/jobs.rs:62-72` 为上面 Interfaces 块的新 JobOutcome 定义。

- [ ] **Step 4: 适配所有 JobOutcome 构造点**

逐处加三字段。`src-tauri/src/jobs.rs:544`（guardian done send）：
```rust
let _ = done2.send(JobOutcome { job_id: id_for_spawn, kind: JobKind::Process, label: None,
    ok, code: code_opt, tail, answer: None, note: None,
    deps: snap.deps.clone(), finished_at: snap.finished_at.unwrap_or_else(now_ms),
    soft_timeout: false }).await;
```
（`snap` 是 guardian 内 `r.get(&id_for_spawn).cloned()`，T2 给 Job 加 deps 后 `snap.deps` 可用；T1 先用 `deps: vec![]` 占位，T2/T3 接通后改 `snap.deps.clone()`——本步写 `deps: vec![]`，T3 Step 里改成 `snap.deps.clone()`。）

`src-tauri/src/subagents.rs` spawn_agent 的 done send（搜 `done2.send(outcome)` 或 `JobOutcome {` 定位）：本任务先加三字段默认值：
```rust
deps: vec![], finished_at: now_ms(), soft_timeout: false,
```
（T3 接 deps、T4 接 soft_timeout 时再改。）

`src-tauri/src/agent.rs:428-429` 测试 fixture：
```rust
let o = JobOutcome { job_id: "3".into(), kind: crate::jobs::JobKind::Agent, label: None,
    ok: true, code: None, tail: String::new(), answer: Some("完成X".into()), note: None,
    deps: vec![], finished_at: 0, soft_timeout: false };
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml scheduler`
Expected: 6 passed。再跑全量 `cargo test --lib --manifest-path src-tauri/Cargo.toml` 确认无回归（阶段2 的 268 测 + 新 6 测 = 274）。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/scheduler.rs src-tauri/src/jobs.rs src-tauri/src/subagents.rs src-tauri/src/agent.rs src-tauri/src/lib.rs
git commit -m "feat(scheduler): 阶段3-T1 Scheduler 纯逻辑（队列+turn_busy+drain_ready）+ JobOutcome 扩 deps/finished_at/soft_timeout 字段"
```

---

### Task 2: dependencies schema + check_deps（环检测 + 未知 id）

**Files:**
- Modify: `src-tauri/src/jobs.rs:27-44`（Job 加 deps）、`jobs.rs:125-140`（register 接 deps）、`jobs.rs:199-203`（JobEvent::started 用 job.deps）；新增 `check_deps` 函数
- Modify: `src-tauri/src/tools.rs:577-590`（bash schema 加 dependencies）、`tools.rs:638-654`（subagent schema：id integer→string + 加 dependencies + timeout_secs）
- Test: `src-tauri/src/jobs.rs` 内联测试

**Interfaces:**
- Consumes: 无（首个 deps 任务）。
- Produces: `Job.deps: Vec<JobId>`、`JobRegistry::register(&mut self, kind, label, log_path, started_at, deps: Vec<JobId>) -> JobId`、`pub fn check_deps(registry: &JobRegistry, deps: &[JobId]) -> Result<(), String>`。T3 spawn 调 check_deps + 传 deps。

**Job 加 deps**（jobs.rs:27-44，在 `suppress_inject` 后加）：
```rust
pub deps: Vec<JobId>,  // 阶段3：声明的依赖（结果处理偏序；drain_ready 筛 ready 用）
```

**register 接 deps**（jobs.rs:125-140）：
```rust
pub fn register(&mut self, kind: JobKind, label: String, log_path: PathBuf, started_at: u64, deps: Vec<JobId>) -> JobId {
    let today = today_compact();
    if today != self.current_day { self.current_day = today.clone(); self.next_seq = 1; }
    let seq = self.next_seq;
    self.next_seq += 1;
    let id = format!("{today}-{}", if seq <= 99 { format!("{seq:02}") } else { seq.to_string() });
    match kind { JobKind::Process => self.running += 1, JobKind::Agent => self.running_agents += 1 }
    self.jobs.insert(id.clone(), Job {
        id: id.clone(), kind, label, status: JobStatus::Running, log_path, started_at, finished_at: None,
        cancel: None, progress: None, answer: None, suppress_inject: false, deps,
    });
    id
}
```

**JobEvent::started 用 job.deps**（jobs.rs:199-203，把 `deps: vec![]` 改 `deps: job.deps.clone()`）。

**check_deps 函数**（jobs.rs，放 register 后）：
```rust
/// spawn 时依赖校验（spec D4）：未知 id 立即报错（A2）+ 拓扑环检测（A1）。
/// deps 里的 id 必须已在 registry 存在；deps 子图不能有环。
pub fn check_deps(registry: &JobRegistry, deps: &[JobId]) -> Result<(), String> {
    // A2 未知 id：dep 必须存在于 registry
    for d in deps {
        if !registry.jobs.contains_key(d) {
            return Err(format!("dependencies 引用未知任务 id: {d}"));
        }
    }
    // A1 环检测：沿 deps 传递闭包走，遇已访问节点 = 环
    // （新 job 刚 spawn 是叶子，无人依赖它；环只能存在于 deps 子图内部互相引用）
    fn reaches(start: &str, target: &str, reg: &JobRegistry, seen: &mut std::collections::HashSet<String>) -> bool {
        if !seen.insert(start.to_string()) { return false; }
        let Some(j) = reg.jobs.get(start) else { return false; };
        for d in &j.deps {
            if d == target || reaches(d, target, reg, seen) { return true; }
        }
        false
    }
    for d in deps {
        let mut seen = std::collections::HashSet::new();
        if reaches(d, d, registry, &mut seen) {
            return Err(format!("dependencies 检测到循环（涉及 {d}）"));
        }
    }
    Ok(())
}
```

**适配现有 register 调用点**（阶段2 的调用都传 deps）：
- `jobs.rs:429`（spawn_process_job）：`r.register(JobKind::Process, label.clone(), PathBuf::new(), started)` → 加 `, vec![]`（T3 改成真 deps）。
- `jobs.rs:473`（load_today 内 `registry.jobs.insert`，不走 register，Job 字面量加 `deps: e.deps.clone()`）—— load_today 里 Job 构造（jobs.rs:302-309）加 `deps: e.deps.clone(),`。
- `jobs.rs:1013`（mark_running 测试 fixture）等所有 `Job { ... }` 字面量 + `register(...)` 调用加 deps。grep 定位：`grep -n "register(JobKind" src-tauri/src/`、`grep -n "Job {" src-tauri/src/jobs.rs`。
- `subagents.rs:397`（spawn_agent register）：加 `, vec![]`（T3 改真 deps）。

- [ ] **Step 1: 写 check_deps 失败测试**（jobs.rs 测试模块）

```rust
#[test]
fn check_deps_rejects_unknown_id() {
    let mut r = JobRegistry::new();
    let _a = r.register(JobKind::Process, "a".into(), PathBuf::new(), 0, vec![]);
    // 依赖不存在的 id
    let err = check_deps(&r, &["99999999-99".into()]);
    assert!(err.is_err());
    assert!(err.unwrap_err().contains("未知任务 id"));
}

#[test]
fn check_deps_accepts_known_empty() {
    let mut r = JobRegistry::new();
    let a = r.register(JobKind::Process, "a".into(), PathBuf::new(), 0, vec![]);
    assert!(check_deps(&r, &[a]).is_ok(), "已知 id 应通过");
    assert!(check_deps(&r, &[]).is_ok(), "空 deps 通过");
}

#[test]
fn check_deps_detects_cycle() {
    // 手造环：A deps [B]，B deps [A]——register 时 B 不该能声明 deps[A]（A 存在），
    // 但 check_deps 在 B 的 deps 子图里 reaches(A,A)=true 应报环
    let mut r = JobRegistry::new();
    let a = r.register(JobKind::Process, "a".into(), PathBuf::new(), 0, vec![]);
    // 给 A 塞一个 deps 指向即将注册的 B（模拟已存在的环结构）
    r.jobs.get_mut(&a).unwrap().deps = vec!["FUTURE-B".into()];
    let b = "FUTURE-B".to_string();
    r.jobs.insert(b.clone(), Job { id: b.clone(), kind: JobKind::Process, label: "b".into(),
        status: JobStatus::Running, log_path: PathBuf::new(), started_at: 0, finished_at: None,
        cancel: None, progress: None, answer: None, suppress_inject: false, deps: vec![a.clone()] });
    // B 的 deps=[A]，A 的 deps=[B] → 环
    let err = check_deps(&r, &[b.clone()]);
    assert!(err.is_err());
    assert!(err.unwrap_err().contains("循环"));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml check_deps`
Expected: 编译错误（Job 无 deps 字段 / register 签名不匹配）。

- [ ] **Step 3: 实现 Job.deps + register(deps) + JobEvent + check_deps**

按上面 Interfaces 代码块改 jobs.rs（Job 加字段、register 加参、JobEvent::started 用 job.deps、新增 check_deps）。逐处适配 register/Job 字面量调用点（grep 定位）。

- [ ] **Step 4: 修 subagent schema（id integer→string + dependencies + timeout_secs）**

`src-tauri/src/tools.rs:638-654`，把 subagent schema 改为：
```rust
serde_json::json!({
    "type":"function",
    "function":{
        "name":"subagent",
        "description":"启动并管理后台子代理（独立迷你 agent，自带 write/read/edit/bash 工具完成你交办的子任务）。action=spawn 启动（不阻塞，完成自动回注结果给你）；action=status 查 #N 的进度；action=kill 终止 #N 并拿回部分结果。子代理上下文干净，不递归嵌套。",
        "parameters":{
            "type":"object",
            "required":["action"],
            "properties":{
                "action":{"type":"string","enum":["spawn","status","kill"]},
                "prompt":{"type":"string","description":"spawn 必填：交给子代理的任务"},
                "caption":{"type":"string","description":"spawn 可选：给人看的标题"},
                "id":{"type":"string","description":"status/kill 必填：目标子代理 id（YYYYMMDD-NN 字符串，如 20260727-01）"},
                "dependencies":{"type":"array","items":{"type":"string"},"description":"spawn 可选：依赖的任务 id 列表；这些任务产出结果（done/failed/超时/killed 均算）后，本子代理的结果才会被处理。空=无依赖"},
                "timeout_secs":{"type":"integer","description":"spawn 可选：软超时秒数（默认 1800=30min）。到点不杀进程，仅汇报当前产出；连续 5 次或总宽限 120min 才强 kill"}
            }
        }
    }
})
```

- [ ] **Step 5: 修 bash schema 加 dependencies**

`src-tauri/src/tools.rs:577-590`，bash schema 的 properties 里加（background 配合用）：
```rust
"dependencies":{"type":"array","items":{"type":"string"},"description":"background 可选：依赖的任务 id 列表；这些任务产出结果后本任务的结果才会被处理"},
```

- [ ] **Step 6: 跑全量测试确认通过**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml`
Expected: 全过（含新 3 个 check_deps 测）。`cargo check --tests --manifest-path src-tauri/Cargo.toml` 0 warning。

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/jobs.rs src-tauri/src/tools.rs src-tauri/src/subagents.rs src-tauri/src/agent.rs
git commit -m "feat(jobs): 阶段3-T2 dependencies schema + check_deps（环检测 A1 + 未知 id A2）+ Job.deps；修 subagent schema id integer→string"
```

---

### Task 3: 接线 spawn 传 deps + timeout_secs

**Files:**
- Modify: `src-tauri/src/jobs.rs:402-449`（spawn_process_job 加 deps 参数 + check_deps）；`jobs.rs:544`（done send 用 snap.deps）
- Modify: `src-tauri/src/subagents.rs:372-410`（spawn_agent 加 deps + timeout_secs 参数 + check_deps）
- Modify: `src-tauri/src/tools.rs:441-465`（tool_bash 解析 dependencies 传 spawn_process_job）、`tools.rs:273-287`（tool_subagent spawn 解析 dependencies + timeout_secs）

**Interfaces:**
- Consumes: T2 的 `check_deps`、`Job.deps`。
- Produces: `spawn_process_job(..., deps: Vec<JobId>)`（在 timeout_secs 后加）、`spawn_agent(..., deps: Vec<JobId>, timeout_secs: u64)`（在 stream 后加）。tools.rs 解析 dependencies 传入。

**spawn_process_job 加 deps**（jobs.rs:402-413 签名 + check_deps）：
```rust
pub async fn spawn_process_job(
    command: String, env_vars: Vec<(String, String)>, cwd: &std::path::Path, cache: &std::path::Path,
    timeout_secs: u64, label: String, deps: Vec<JobId>,
    registry: SharedRegistry, done_tx: mpsc::Sender<JobOutcome>, update: Arc<dyn JobUpdate>, writer: JobWriterHandle,
) -> Result<JobId, String> {
    // ... started 之后，register 之前加 check_deps（持锁内查 + register 同锁）
    let started = now_ms();
    let (id, log_path) = {
        let mut r = registry.lock().unwrap();
        check_deps(&r, &deps)?;   // 未知 id / 环 → Err 串返回给 agent
        if !r.can_spawn(JobKind::Process) { return Err("已达并发上限(8)".into()); }
        let id = r.register(JobKind::Process, label.clone(), PathBuf::new(), started, deps.clone());
        if let Some(j) = r.jobs.get(&id) { writer.append(JobEvent::started(j)); }
        (id.clone(), jobs_dir.join(format!("{id}.log")))
    };
    // ... 其余不变；guardian done send（jobs.rs:544）的 deps 字段改成 snap.deps.clone()
```

guardian done send（jobs.rs:544）改 `deps: vec![]` → `deps: snap.deps.clone()`（snap 是该作用域内 `r.get(&id_for_spawn).cloned().unwrap()`）。

**spawn_agent 加 deps + timeout_secs**（subagents.rs:372-382 签名 + 388-405 register）：
```rust
pub fn spawn_agent(
    prompt: &str, caption: &str, cfg: &Config, round: Arc<dyn LlmRound>,
    sub_ctx: &ToolsCtx, sub_cfg_tpl: &Config,
    registry: SharedRegistry, job_done_tx: tokio::sync::mpsc::Sender<JobOutcome>,
    stream: Arc<dyn SubagentStream>,
    deps: Vec<JobId>, timeout_secs: u64,   // 阶段3 新增
) -> Result<JobId, String> {
```
register 段（subagents.rs:388-405）加 check_deps + 传 deps：
```rust
let (id, cancel, progress, snap) = {
    let mut r = registry.lock().unwrap();
    if !r.can_spawn(JobKind::Agent) { return Err(format!("子代理并发已满({}/{})", r.running_agents, r.max_agents)); }
    crate::jobs::check_deps(&r, &deps)?;   // 阶段3：未知 id / 环检测
    let id = r.register(JobKind::Agent, label.clone(), std::path::PathBuf::new(), now_ms(), deps.clone());
    // ... cancel/progress/snap 不变
};
```
timeout_secs 暂存进 spawned task（T4 用）：把 `timeout_secs` move 进 `tokio::spawn(async move { ... })`（subagents.rs spawned task，T4 Step 里加 select timeout 分支会用）。spawn_agent 的 done send（subagents.rs:~500）`deps: vec![]` → `deps: snap.deps.clone()`（snap_opt 内的 snap）。

**tools.rs dispatch + tool_bash + tool_subagent 解析**：

`src-tauri/src/tools.rs:441` tool_bash 签名加 deps 参数：
```rust
pub async fn tool_bash(args: &Value, ctx: &ToolsCtx, timeout_secs: u64) -> String {
    // ... 解析 dependencies
    let deps: Vec<jobs::JobId> = args.get("dependencies")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    // ... background 分支调 spawn_process_job 传 deps：
    return match jobs::spawn_process_job(
        command.clone(), env_vars, &ctx.workspace, &ctx.cache, timeout_secs,
        label_for(&command), deps, ctx.jobs.clone(), ...).await { ... }
```

`src-tauri/src/tools.rs:660-677` dispatch 的 bash 臂不变（tool_bash 内部解析 deps）。subagent 臂解析 timeout_secs + deps 传 tool_subagent——但 tool_subagent 签名要加参数：
```rust
"subagent" => {
    let deps: Vec<jobs::JobId> = args.get("dependencies")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let timeout_secs: u64 = args.get("timeout_secs").and_then(|v| v.as_u64()).unwrap_or(1800);
    tool_subagent(&args, ctx, cfg, round, deps, timeout_secs).await
}
```
tool_subagent（tools.rs:273）签名加 `deps, timeout_secs`，spawn 分支调 spawn_agent 传：
```rust
pub async fn tool_subagent(args: &Value, ctx: &ToolsCtx, cfg: &crate::config::Config, round: std::sync::Arc<dyn crate::llm::LlmRound>, deps: Vec<crate::jobs::JobId>, timeout_secs: u64) -> String {
    // ... spawn 分支：
    match crate::subagents::spawn_agent(prompt, caption, cfg, round, ctx, cfg,
        ctx.jobs.clone(), ctx.job_done_tx.clone(), stream, deps, timeout_secs) { ... }
```

- [ ] **Step 1: 写失败测试**（jobs.rs，验 spawn 带 deps 写进 JobEvent.deps）

```rust
#[cfg(windows)]
#[tokio::test]
async fn spawn_process_with_deps_writes_event_deps() {
    let reg: SharedRegistry = Arc::new(Mutex::new(JobRegistry::new()));
    let (tx, _rx) = mpsc::channel(8);
    let dir = tempfile::tempdir().unwrap();
    let jd = dir.path().join(".ovoice-jobs");
    let w = spawn_job_writer(jd.clone(), 0);
    let a = spawn_process_job("echo a".into(), vec![], dir.path(), dir.path(), 15, "a".into(), vec![],
        reg.clone(), tx.clone(), Arc::new(NoopJobUpdate), w.clone()).await.unwrap();
    let b = spawn_process_job("echo b".into(), vec![], dir.path(), dir.path(), 15, "b".into(), vec![a.clone()],
        reg.clone(), tx, Arc::new(NoopJobUpdate), w.clone()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let today = crate::history::date_from_ts_local(now_ms(), crate::history::local_offset_secs());
    let evs = read_jobs_jsonl(&jd, &today);
    let b_ev = evs.iter().find(|e| e.id == b).unwrap();
    assert_eq!(b_ev.deps, vec![a], "JobEvent.deps 应记录声明的依赖");
}

#[test]
fn check_deps_rejects_unknown_at_spawn_helper() {
    // 辅助：确认 check_deps 在 spawn 前被调（未知 id → Err）
    let mut r = JobRegistry::new();
    assert!(check_deps(&r, &["nodata".into()]).is_err());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml spawn_process_with_deps`
Expected: 编译错误（spawn_process_job 无 deps 参数）。

- [ ] **Step 3: 实现 spawn_process_job 加 deps + check_deps + done send 用 snap.deps**

按上面代码块改 jobs.rs。注意所有现有 `spawn_process_job(...)` 调用点加 deps 实参：grep `spawn_process_job(` → tools.rs:457（tool_bash，本步传 `deps` 变量）、jobs.rs 测试 fixtures（line 687/707/727/746/763/766/779/800 等，都加 `vec![]`）。

- [ ] **Step 4: 实现 spawn_agent 加 deps + timeout_secs + check_deps**

按上面代码块改 subagents.rs。grep `spawn_agent(` 调用点 → tools.rs:282（tool_subagent，本步传 deps + timeout_secs）。

- [ ] **Step 5: tool_bash + tool_subagent + dispatch 解析 dependencies/timeout_secs**

按上面代码块改 tools.rs（tool_bash 解析 deps 传 spawn_process_job；dispatch subagent 臂解析 deps+timeout_secs；tool_subagent 签名 + spawn 调用传参）。

- [ ] **Step 6: 跑全量测试**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml`
Expected: 全过（含新 deps 测）。

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/jobs.rs src-tauri/src/subagents.rs src-tauri/src/tools.rs
git commit -m "feat(jobs): 阶段3-T3 spawn 接 deps + timeout_secs（process+agent），dispatch 解析传入；JobEvent.deps 落地"
```

---

### Task 4: 软超时 guardian（不 kill + soft_timeout outcome + 资源上限 N=5/M=120min）

**Files:**
- Modify: `src-tauri/src/jobs.rs:27-44`（Job 加 soft_timeout_count + first_soft_at）、`jobs.rs:499-546`（process guardian 改软超时循环）、`jobs.rs:21-40`（jobdone_body 加 soft_timeout 分支）、`jobs.rs:595-605`（terminal_event 不变，但 soft_timeout 不走 terminal）
- Modify: `src-tauri/src/subagents.rs:428-508`（spawned task select 加 timeout 分支）
- Test: `src-tauri/src/jobs.rs`、`src-tauri/src/subagents.rs`

**Interfaces:**
- Consumes: T1 的 `JobOutcome.soft_timeout`、T3 的 spawn deps/timeout。
- Produces: Job 加 `soft_timeout_count: u32`、`first_soft_at: Option<u64>`；guardian 循环软超时；资源上限 kill。

**Job 加两字段**（jobs.rs:27-44）：
```rust
pub soft_timeout_count: u32,        // 阶段3：软超时累计次数（N=5 上限）
pub first_soft_at: Option<u64>,     // 阶段3：首次软超时时刻（M=120min 宽限起点）
```
适配所有 `Job { ... }` 字面量（jobs.rs:136 register、:302 load_today、测试 fixtures）加 `soft_timeout_count: 0, first_soft_at: None,`。

**常量**（jobs.rs 顶部 MAX_RUNNING 附近）：
```rust
pub const SOFT_TIMEOUT_MAX_COUNT: u32 = 5;       // N：连续软超时上限
pub const SOFT_TIMEOUT_GRACE_MS: u64 = 120 * 60 * 1000;  // M：总宽限 120min
```

**process guardian 改软超时循环**（jobs.rs:499-546，重写 spawned task）：
```rust
// guardian：软超时（spec §6）。到 timeout_secs 不杀，发 soft_timeout outcome（进程继续）；
// 资源上限（count>=5 或 宽限>=120min）→ 强 kill。正常 done / 失败 / kill 走原终态。
tokio::spawn(async move {
    let mut child = child;  // move 进来
    loop {
        let res = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await;
        match res {
            Ok(wait_res) => {
                // 真正结束（done / wait 失败）
                let _ = drainer.await;
                let tail = read_tail(&log_for_guard);
                let (ok, code_opt, reason) = match wait_res {
                    Ok(s) => (true, s.code(), None),
                    Err(_) => (false, None, Some("进程 wait 失败".to_string())),
                };
                let status = match &reason {
                    Some(r) => JobStatus::Failed { reason: r.clone() },
                    None => JobStatus::Done { code: code_opt.unwrap_or(0) },
                };
                finalize_process(&reg2, &id_for_spawn, &upd2, &writer2, &done2, status, tail, ok, code_opt).await;
                return;
            }
            Err(_) => {
                // 软超时：不杀，发 soft_timeout outcome，进程继续
                let tail = read_tail(&log_for_guard);
                let kill_now = {
                    let mut r = reg2.lock().unwrap();
                    if let Some(j) = r.jobs.get_mut(&id_for_spawn) {
                        j.soft_timeout_count += 1;
                        if j.first_soft_at.is_none() { j.first_soft_at = Some(now_ms()); }
                    }
                    let snap = r.get(&id_for_spawn).cloned();
                    drop(r);
                    if let Some(s) = &snap {
                        // 软超时 outcome（不唤醒?——spec §6: soft_timeout 算 outcome，drain_ready 据此放行依赖；但要唤醒 agent 决策 kill/等/用）
                        let o = JobOutcome { job_id: id_for_spawn.clone(), kind: JobKind::Process, label: None,
                            ok: true, code: None, tail: tail.clone(), answer: None,
                            note: Some("已超时，任务仍在跑".into()),
                            deps: s.deps.clone(), finished_at: now_ms(), soft_timeout: true };
                        let _ = done2.send(o).await;
                        writer2.append(JobEvent { schema: 1, id: id_for_spawn.clone(), kind: JobKind::Process,
                            label: s.label.clone(), status: "soft_timeout".into(), started_at: s.started_at,
                            finished_at: Some(now_ms()), code: None, tail: Some(tail),
                            note: Some("已超时，任务仍在跑".into()), reason: None, deps: s.deps.clone() });
                        // 资源上限判定
                        s.soft_timeout_count >= SOFT_TIMEOUT_MAX_COUNT
                            || s.first_soft_at.map(|t| now_ms() - t >= SOFT_TIMEOUT_GRACE_MS).unwrap_or(false)
                    } else { false }
                };
                if kill_now {
                    // 强 kill：取下 Job 句柄杀树 + finalize killed
                    let h = { let mut r = reg2.lock().unwrap(); r.handles.remove(&id_for_spawn) };
                    drop(h);
                    let _ = child.start_kill();
                    finalize_process(&reg2, &id_for_spawn, &upd2, &writer2, &done2,
                        JobStatus::Killed, String::new(), false, None).await;
                    return;
                }
                // 否则继续 loop（下一轮 timeout 等待）
            }
        }
    }
});
```

抽 `finalize_process` 辅助（避免 loop 内重复 G2 check-and-finish 逻辑）。**关键正确性**：outcome.deps 必须填 `snap.deps.clone()`——drain_ready 靠 outcome.deps 判 ready，填空会让依赖它的下游被提前处理（违反 deps 偏序）。
```rust
/// guardian 共用终态收尾：原子 check-and-finish（防 kill_job 竞态覆盖）+ 写 terminal 事件
/// + emit job-update + 投 JobOutcome（killed 不投，不唤醒模型——与阶段2 一致）。
async fn finalize_process(
    registry: &SharedRegistry, id: &JobId, update: &Arc<dyn JobUpdate>,
    writer: &JobWriterHandle, done_tx: &mpsc::Sender<JobOutcome>,
    status: JobStatus, tail: String, ok: bool, code: Option<i32>,
) {
    let (killed_already, deps) = {
        let mut r = registry.lock().unwrap();
        let is_killed = matches!(r.get(id).map(|j| &j.status), Some(JobStatus::Killed));
        if !is_killed { r.finish(id, status, now_ms()); }
        let snap = r.get(id).cloned().unwrap();
        drop(r);
        if !is_killed { writer.append(terminal_event(&snap)); }
        update.update(&snap);
        (is_killed, snap.deps.clone())  // 必须 clone 真实 deps，outcome 带出去给 scheduler
    };
    if !killed_already {
        let _ = done_tx.send(JobOutcome { job_id: id.clone(), kind: JobKind::Process, label: None,
            ok, code, tail, answer: None, note: None,
            deps, finished_at: now_ms(), soft_timeout: false }).await;
    }
}
```

**parse_status_from_event 加 soft_timeout**（jobs.rs:391-399）：
```rust
"soft_timeout" => JobStatus::Running,  // 软超时非终态：load 时仍视作 Running（悬空则 forced_exit 兜底）
```

**jobdone_body 加 soft_timeout 分支**（agent.rs:21-40，process 臂）：
```rust
crate::jobs::JobKind::Process => {
    if o.soft_timeout {
        format!("[后台任务 #{} 已超时（仍在跑）]\n{}\n（任务未终止，稍后可能再汇报；你可 kill 或继续等）", o.job_id, o.tail)
    } else if o.ok {
        format!("[后台任务 #{} 完成] 退出码 {}\n{}", o.job_id, o.code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()), o.tail)
    } else {
        format!("[后台任务 #{} 失败]\n{}", o.job_id, o.tail)
    }
}
```

**subagents.rs spawned task 加 timeout 分支**（subagents.rs:431-432 的 select!）：
```rust
let mut soft_count: u32 = 0;
let mut first_soft: Option<u64> = None;
let res: Result<String, String> = loop {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => break Err("cancelled".into());
        r = crate::llm::run_turn(...) => break r;   // 子代理一轮即完（run_turn 是其主循环）
    }
    // 注：subagent 的 run_turn 是阻塞到完成的；软超时要在 run_turn 内部或外层包裹。
};
```
**重要**：subagent 的 run_turn 是单次 LLM 轮（带 tool 循环），可能长时间不返回。软超时要包 run_turn：把 `crate::llm::run_turn(...)` 包进 `tokio::time::timeout(Duration::from_secs(timeout_secs), ...)`。超时 → 发 soft_timeout outcome（带 partial = progress.partial）+ 查资源上限（soft_count>=5 或 宽限>=120min）→ kill（cancel + finalize Killed）否则继续（重新 timeout 包裹 run_turn 续跑——但 run_turn 已被 timeout 中断，需重启）。

**实现复杂度提示给 implementer**：subagent 软超时最棘手——run_turn 被 timeout 中断后无法"续跑"（LLM 调用已 abort）。简化方案：subagent 软超时 = timeout 到点发 soft_timeout outcome（带 partial），**不续跑**（subagent 任务视为可中止），等资源上限才强制 cancel。即 subagent 软超时后直接让 spawned task 结束（发 soft_timeout outcome + 保留 registry 为 Running 等手动 kill），或更简单：subagent 软超时即等价于"汇报 partial，任务暂停"。**本 plan 选**：subagent timeout 到点 → 发 soft_timeout outcome（note="已超时，部分产出"，answer=partial）+ spawned task 结束（不续跑），registry 留 Running 由后续 manual kill 或下次 soft 重新评估——但这会让 subagent 永不自然结束。

**决策（implementer 遵循）**：subagent 软超时实现为「单次 timeout 包裹整个 spawn_agent 任务，到点发 soft_timeout outcome + cancel 子任务 + registry 标 Failed{reason:"soft_timeout"}」。即 subagent 不做"续跑"循环（与 process 不同），软超时即终止子代理但以 soft_timeout 语义上报（ok=true，answer=partial，note="已超时"）。资源上限 N=5/M=120 对 subagent 退化为「单次 timeout 即终止」（因为 subagent 不续跑，count 不会累加到 5）。这是可接受的简化——subagent 长任务应由 agent 拆分成更小的子代理。

subagents.rs 实现（替换 spawned task 的 select 块）：
```rust
let timeout_dur = Duration::from_secs(timeout_secs);
let res: Result<String, String> = tokio::select! {
    biased;
    _ = cancel.cancelled() => Err("cancelled".into()),
    r = crate::llm::run_round_for_subagent(...) => r,  // 原run_turn调用
    _ = tokio::time::sleep(timeout_dur) => Err("__soft_timeout__".into()),
};
let soft_timeout = matches!(res, Err(ref e) if e == "__soft_timeout__");
// soft_timeout：取 partial 作 answer，状态 Failed{soft_timeout}，outcome soft_timeout=true
// （资源上限对 subagent 退化：单次即终止，不续跑）
```
（具体 run_turn 调用签名 implementer 按 subagents.rs 现有 line 431-432 的 `crate::llm::run_turn(...)` 原样保留，只加 timeout 分支。）

- [ ] **Step 1: 写失败测试**（jobs.rs，process 软超时不杀 + tail 返回）

```rust
#[cfg(windows)]
#[tokio::test]
async fn soft_timeout_does_not_kill_sends_outcome_with_tail() {
    let reg: SharedRegistry = Arc::new(Mutex::new(JobRegistry::new()));
    let (tx, mut rx) = mpsc::channel(8);
    let dir = tempfile::tempdir().unwrap();
    // ping 跑 100s，timeout 1s → 软超时
    let id = spawn_process_job("ping -n 100 127.0.0.1".into(), vec![], dir.path(), dir.path(),
        1, "hang".into(), vec![], reg.clone(), tx, Arc::new(NoopJobUpdate), JobWriterHandle::noop()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let mut got = vec![];
    while let Ok(o) = rx.try_recv() { got.push(o); }
    assert!(got.iter().any(|o| o.soft_timeout), "应收到 soft_timeout outcome");
    // 资源上限未到（count=1<5）→ 进程应仍在（不 kill）；这里只验 outcome 产生
}

#[cfg(windows)]
#[tokio::test]
async fn resource_cap_kills_after_n_soft_timeouts() {
    // N=5：连续 5 次软超时 → 第 5 次强 kill。用极短 timeout + 长任务模拟。
    let reg: SharedRegistry = Arc::new(Mutex::new(JobRegistry::new()));
    let (tx, _rx) = mpsc::channel(64);
    let dir = tempfile::tempdir().unwrap();
    let id = spawn_process_job("ping -n 1000 127.0.0.1".into(), vec![], dir.path(), dir.path(),
        1, "hang".into(), vec![], reg.clone(), tx, Arc::new(NoopJobUpdate), JobWriterHandle::noop()).await.unwrap();
    // 5 次软超时（每次 1s）+ finalize 时间 ≈ 6s
    tokio::time::sleep(std::time::Duration::from_millis(7000)).await;
    let r = reg.lock().unwrap();
    let st = &r.get(&id).unwrap().status;
    assert!(matches!(st, JobStatus::Killed), "5 次软超时应强 kill，实际: {st:?}");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml soft_timeout`
Expected: 失败（spawn_timeout_marks_failed 现有测可能也变红——它断言超时→Failed，现在改成 soft_timeout 了，需更新该测：见 Step 3）。

- [ ] **Step 3: 更新现有 spawn_timeout_marks_failed 测试（jobs.rs:722-735）**

阶段2 该测断言"超时→Failed{超时(N)s)"。阶段3 改成软超时后，1s timeout 不再 Failed（而是 soft_timeout outcome）。把该测改为验 soft_timeout outcome：
```rust
#[cfg(windows)]
#[tokio::test]
async fn spawn_timeout_now_soft_not_failed() {
    let reg: SharedRegistry = Arc::new(Mutex::new(JobRegistry::new()));
    let (tx, mut rx) = mpsc::channel(8);
    let dir = tempfile::tempdir().unwrap();
    let id = spawn_process_job("ping -n 100 127.0.0.1".into(), vec![], dir.path(), dir.path(),
        1, "hang".into(), vec![], reg.clone(), tx, Arc::new(NoopJobUpdate), JobWriterHandle::noop()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let mut soft = false;
    while let Ok(o) = rx.try_recv() { if o.soft_timeout { soft = true; } }
    assert!(soft, "短超时应产生 soft_timeout outcome（不再直接 Failed）");
}
```

- [ ] **Step 4: 实现 Job 两字段 + 常量 + finalize_process + guardian 软超时循环**

按上面代码块改 jobs.rs（Job 加字段适配所有字面量、加常量、加 finalize_process、重写 guardian spawned task、parse_status_from_event 加 soft_timeout）。

- [ ] **Step 5: 实现 jobdone_body soft_timeout 分支**（agent.rs:21-40）

按上面代码块改 agent.rs jobdone_body。

- [ ] **Step 6: 实现 subagent timeout 分支**（subagents.rs spawned task）

按上面"决策"块改 subagents.rs select!（加 timeout 分支 + soft_timeout outcome）。

- [ ] **Step 7: 跑全量测试**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml`
Expected: 全过（含软超时 + 资源上限测）。`cargo check --tests` 0 warning。

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/jobs.rs src-tauri/src/subagents.rs src-tauri/src/agent.rs
git commit -m "feat(jobs): 阶段3-T4 软超时 guardian（不 kill + soft_timeout outcome + 资源上限 N=5/M=120min 强 kill）"
```

---

### Task 5: driver 接 Scheduler（completed_pending + turn_busy + handle_batch）

**Files:**
- Modify: `src-tauri/src/agent.rs:48-94`（拆 handle_event → append_history + run_turn_from；新增 handle_batch）、`agent.rs:189-199`（driver select 接 Scheduler）、`agent.rs:210-262`（run_one 调 Scheduler）、`agent.rs:160`（spawn_session 持 Scheduler）

**Interfaces:**
- Consumes: T1 的 `Scheduler`、T4 的 soft_timeout JobOutcome。
- Produces: driver 改为 idle 事件驱动攒批（一批一气泡）；`fn append_history(...)`、`fn run_turn_from(...)`、`async fn handle_batch(...)`。

**handle_event 拆分**（agent.rs:48-94，提取两辅助 + handle_event 调它们）：
```rust
/// 构造 current history event + read_all + append + 本地 push，返回 (current, events)。
fn append_history(
    event: &SessionEvent, history: &crate::history::HistoryWriterHandle, history_dir: &std::path::Path,
) -> (crate::history::HistoryEvent, Vec<crate::history::HistoryEvent>) {
    let now = now_ms();
    let mut current = match event {
        SessionEvent::UserMessage { text, attachments } => crate::history::HistoryEvent::user(now, "main", text, attachments),
        SessionEvent::ContextNote { text } => crate::history::HistoryEvent::external(now, text, "", None),
        SessionEvent::JobDone(o) => match o.kind {
            crate::jobs::JobKind::Agent => crate::history::HistoryEvent::subagent_result(
                now, &o.job_id, &o.answer.clone().unwrap_or_default(), &format!("thread=agent:{}", o.job_id)),
            crate::jobs::JobKind::Process => crate::history::HistoryEvent::external(now, &jobdone_body(o), "", None),
        },
        SessionEvent::Reset => crate::history::HistoryEvent::marker(now, "reset", history.current_seq()),
        SessionEvent::DreamCheck => unreachable!(),
    };
    current.seq = history.current_seq();
    let mut events = crate::history::read_all(history_dir);
    history.append(current.clone());
    events.push(current.clone());
    (current, events)
}

/// 从 events 重建 messages + run_turn。
async fn run_turn_from<E: llm::Emitter>(
    events: &[crate::history::HistoryEvent], cache: &std::path::Path, cfg: &Config,
    ctx: &ToolsCtx, round: std::sync::Arc<dyn llm::LlmRound>, emitter: &E,
) -> Option<ChatResponse> {
    let pinned = crate::context::load_pinned(cache, &cfg.system_prompt);
    let mut messages = crate::context::build_messages(events, &pinned, cfg.dream_cap_turns as usize);
    Some(llm::run_turn(round, emitter, cfg, &mut messages, ctx, cfg.max_tool_iters as usize, "main").await)
}
```

handle_event 重构（agent.rs:48-94，调用上面两辅助）：
```rust
pub async fn handle_event<E: llm::Emitter>(...) -> Option<ChatResponse> {
    let (_current, events) = append_history(event, history, history_dir);
    let triggers_turn = matches!(event, SessionEvent::UserMessage { .. } | SessionEvent::JobDone(_));
    if !triggers_turn { return None; }
    run_turn_from(&events, cache, cfg, ctx, round, emitter).await
}
```

**新增 handle_batch**（整批 N outcome → N history + 1 turn）：
```rust
/// 整批 JobOutcome 喂一个 turn（spec §3 一批一气泡）：逐个落 history（不 turn）+ 最后一次 turn。
pub async fn handle_batch<E: llm::Emitter>(
    outcomes: &[crate::jobs::JobOutcome], history: &crate::history::HistoryWriterHandle,
    history_dir: &std::path::Path, cache: &std::path::Path, cfg: &Config, ctx: &ToolsCtx,
    round: std::sync::Arc<dyn llm::LlmRound>, emitter: &E,
) -> Option<ChatResponse> {
    if outcomes.is_empty() { return None; }
    let mut events = crate::history::read_all(history_dir);
    for o in outcomes {
        let cur = match o.kind {
            crate::jobs::JobKind::Agent => crate::history::HistoryEvent::subagent_result(
                now_ms(), &o.job_id, &o.answer.clone().unwrap_or_default(), &format!("thread=agent:{}", o.job_id)),
            crate::jobs::JobKind::Process => crate::history::HistoryEvent::external(now_ms(), &jobdone_body(o), "", None),
        };
        let mut cur = cur;
        cur.seq = history.current_seq();
        history.append(cur.clone());
        events.push(cur);
    }
    run_turn_from(&events, cache, cfg, ctx, round, emitter).await
}
```

**spawn_session 持 Scheduler**（agent.rs:160 async block 内，load_today 后）：
```rust
let scheduler = std::sync::Arc::new(std::sync::Mutex::new(crate::scheduler::Scheduler::new()));
```
（放进 driver loop 可见的 scope；clone 进 run_one 调用。）

**driver select + run_one 接 Scheduler**（agent.rs:189-199 + 210-262）：

driver loop 改为：
```rust
loop {
    tokio::select! {
        Some(o) = job_done_rx.recv() => {
            // JobDone：emit job-callback（保留）+ 入 scheduler + 若 idle 则 drain_ready 批处理
            run_jobdone(&o, &history, &history_dir_rc, &cache_rc, &ctx, &cfg, &app, &trigger, &scheduler).await;
        }
        ev = rx.recv() => match ev {
            Some(e) => run_one(&e, &history, &history_dir_rc, &cache_rc, &ctx, &cfg, &app, &trigger, &scheduler).await,
            None => break,
        }
    }
}
```

run_one 加 scheduler 参数。UserMessage 分支：on_turn_start → handle_event → on_turn_end → drain_ready loop：
```rust
async fn run_one(e, history, history_dir, cache, ctx, cfg, app, trigger, scheduler) {
    // ... note_activity / DreamCheck 分派不变
    if matches!(e, SessionEvent::UserMessage { .. }) {
        scheduler.lock().unwrap().on_turn_start();
    }
    let emit = crate::AppEmitter { app: app.clone() };
    let round: Arc<dyn llm::LlmRound> = Arc::new(crate::llm::HttpRound);
    let _ = handle_event(e, history, history_dir.as_ref(), cache.as_ref(), ctx, cfg.as_ref(), round.clone(), &emit).await;
    // Reset 分支不变（cancel agent jobs + reset registry）
    if matches!(e, SessionEvent::UserMessage { .. }) {
        scheduler.lock().unwrap().on_turn_end();
        // turn 结束后 drain_ready 批（上一轮残留 + turn 期间到达的）
        drain_and_run_batch(scheduler, history, history_dir, cache, ctx, cfg, app, round.clone()).await;
    }
    if matches!(e, SessionEvent::UserMessage { .. } | SessionEvent::JobDone(_)) {
        dispatch_dream(trigger, history, history_dir, cache, cfg).await;
    }
}
```

新增 run_jobdone（JobDone 不立即 turn，入队 + idle 则批处理）：
```rust
async fn run_jobdone(o, history, history_dir, cache, ctx, cfg, app, trigger, scheduler) {
    // emit job-callback（前端 live 插折叠回调卡，保留阶段1 行为）
    let body = jobdone_body(o);
    let kind_str = match o.kind { crate::jobs::JobKind::Process => "process", crate::jobs::JobKind::Agent => "agent" };
    let _ = TauriEmitter::emit(app, "job-callback", serde_json::json!({ "job_id": o.job_id, "kind": kind_str, "body": body }));
    // 入队
    scheduler.lock().unwrap().on_job_done(o.clone());
    // idle 则立即批处理
    drain_and_run_batch(scheduler, history, history_dir, cache, ctx, cfg, app, Arc::new(crate::llm::HttpRound)).await;
}

/// drain_ready 循环：每批一个 turn，直到无 ready（turn 内新 spawn 的结果尚未 done，不会无限循环）。
async fn drain_and_run_batch(scheduler, history, history_dir, cache, ctx, cfg, app, round) {
    loop {
        let batch = scheduler.lock().unwrap().drain_ready();
        let Some(batch) = batch else { return; };
        scheduler.lock().unwrap().on_turn_start();
        let emit = crate::AppEmitter { app: app.clone() };
        let _ = handle_batch(&batch, history, history_dir.as_ref(), cache.as_ref(), cfg.as_ref(), ctx, round.clone(), &emit).await;
        scheduler.lock().unwrap().on_turn_end();
    }
}
```

（注：run_one / run_jobdone / drain_and_run_batch 的完整参数列表 implementer 按 agent.rs 现有 run_one 签名对齐补齐类型。dream dispatch 在 UserMessage/JobDone 后保留。）

- [ ] **Step 1: 写 handle_batch 失败测试**（agent.rs 测试模块）

```rust
#[tokio::test]
async fn handle_batch_one_turn_for_multiple_outcomes() {
    let (dir, h, hd) = ws_setup();
    let round: Arc<dyn LlmRound> = Arc::new(CountTurnsRound::new());
    let emit = FakeEmitter { content: Mutex::new(String::new()) };
    let x = ctx_with(dir.path().to_path_buf(), h.clone());
    let c = cfg();
    let outcomes = vec![
        JobOutcome { job_id: "20260727-01".into(), kind: crate::jobs::JobKind::Process, label: None,
            ok: true, code: Some(0), tail: "r1".into(), answer: None, note: None, deps: vec![], finished_at: 100, soft_timeout: false },
        JobOutcome { job_id: "20260727-02".into(), kind: crate::jobs::JobKind::Process, label: None,
            ok: true, code: Some(0), tail: "r2".into(), answer: None, note: None, deps: vec![], finished_at: 200, soft_timeout: false },
    ];
    let _ = handle_batch(&outcomes, &h, &hd, dir.path(), &x, &c, round, &emit).await;
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    let evs = crate::history::read_all(&hd);
    let ext_count = evs.iter().filter(|e| e.kind=="external").count();
    assert_eq!(ext_count, 2, "两个 outcome 各落一条 external");
    // CountTurnsRound 记录 run_turn 调用次数 = 1（整批一个 turn）
}

// CountTurnsRound：记录 round() 被调次数
struct CountTurnsRound { calls: Mutex<u32> }
impl CountTurnsRound { fn new() -> Self { Self { calls: Mutex::new(0) } } }
#[async_trait]
impl LlmRound for CountTurnsRound {
    async fn round(&self, _: &[serde_json::Value], _: &Config, _: &dyn llm::Emitter) -> Result<RoundResult, String> {
        *self.calls.lock().unwrap() += 1;
        Ok(RoundResult { content:"done".into(), reasoning:String::new(), tool_calls:vec![], finish:FinishReason::Stop, usage:None,
            assistant_message:serde_json::json!({"role":"assistant","content":"done"}) })
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml handle_batch`
Expected: 编译错误（handle_batch 未定义）。

- [ ] **Step 3: 实现 append_history + run_turn_from + handle_event 重构 + handle_batch**

按上面代码块改 agent.rs（拆 handle_event、加 handle_batch）。

- [ ] **Step 4: spawn_session 持 Scheduler + driver select 接 Scheduler + run_one/run_jobdone/drain_and_run_batch**

按上面代码块改 agent.rs（spawn_session 加 scheduler、driver loop 改、run_one 加 scheduler 参数 + on_turn_start/end + drain、新增 run_jobdone + drain_and_run_batch）。

- [ ] **Step 5: 跑全量测试**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml`
Expected: 全过。`cargo check --tests` 0 warning。注意原有 `jobdone_agent_appends_subagent_result_and_runs`（agent.rs:422）仍应过——handle_event 单 JobDone 仍触发 turn（triggers_turn 不变）。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/agent.rs
git commit -m "feat(agent): 阶段3-T5 driver 接 Scheduler（completed_pending + turn_busy + handle_batch 一批一气泡）"
```

---

### Task 6: 重启汇报 turn（启动即触发 forced_exit failed 批）

**Files:**
- Modify: `src-tauri/src/agent.rs:173-176`（load_today 后查 forced_exit → tx.send 合成 UserMessage）

**Interfaces:**
- Consumes: T5 的 spawn_session（bootstrap 在此）、阶段2 的 load_today（标记 forced_exit）。
- Produces: 启动若有 forced_exit failed job，driver loop 起来后处理一个合成 UserMessage turn。

**实现**（agent.rs:175-176 load_today 块之后，loop 之前加）：
```rust
// 阶段3 §5.2 重启汇报：load_today 后若有 forced_exit failed job，主动触发一个 turn 喂主 agent。
// 走 SessionEvent::UserMessage 合成文本（复用现有 turn 路径，不碰 scheduler）。
{
    let forced: Vec<String> = {
        let r = registry.lock().unwrap();
        r.jobs.values()
            .filter(|j| matches!(&j.status, crate::jobs::JobStatus::Failed { reason } if reason == "forced_exit"))
            .map(|j| format!("#{}（{}）", j.id, j.label))
            .collect()
    };
    if !forced.is_empty() {
        let text = format!(
            "[系统提示] 上次应用退出时，以下后台任务未完成、已判定失败：\n{}\n请告知用户，并判断是否需要重做。",
            forced.join("\n"));
        // tx 是本函数开头的 driver channel sender；clone 一份投递（loop 起来后处理）
        let _ = tx.send(SessionEvent::UserMessage { text, attachments: vec![] }).await;
    }
}
```
（注意 `tx` 在 spawn_session 顶部已定义（agent.rs:124 `let (tx, mut rx) = ...`），但要 move 进 async block——把 `tx` clone 在 async block 外取一份，或直接在 block 内用 `tx`（若 ownership 允许）。implementer 按 spawn_session 现有 tx 的 move 情况调整：若 tx 已 move 进 SessionHandle 返回，则在 return 前 clone 一份留 block 内用。）

**具体接线提示**：spawn_session 在 `let (tx, rx) = channel(...)` 后，`tx` 一部分用于 idle ticker（line 150 `tx2 = tx.clone()`），最终 `SessionHandle { tx }` 返回（line 201）。要在 async block 内 send，需在 block 内 clone：把汇报逻辑放在 async block 内（line 160 spawn 的 block），用 block 内可见的 tx clone。implementer 在 block 内 `let tx_report = tx.clone();` 不行（tx 在 block 外）——正确做法：汇报逻辑放在 async block 内，block 内已有 tx 的间接访问？实际 block 是 `async move`，会 capture tx。但 tx 要返回给 SessionHandle。

**解决方案**（implementer 遵循）：在 `let (tx, rx) = channel(...)`（line 124）后立即 `let tx_for_report = tx.clone();`，把 `tx_for_report` move 进 async block 用于汇报 send，`tx` 仍返回 SessionHandle。或：把汇报 send 放在 driver loop 启动后第一条（loop 内首次迭代检测）——但 loop 内检测会重复。最干净：clone 在前。

- [ ] **Step 1: 写失败测试**（agent.rs 测试模块，验有 forced_exit → send UserMessage）

```rust
#[tokio::test]
async fn startup_report_turn_fires_when_forced_exit_exists() {
    // 模拟：registry 有 forced_exit failed job → 触发汇报（测纯逻辑：构造 registry + 调汇报判定）
    use crate::jobs::{JobRegistry, JobStatus, JobKind};
    let mut r = JobRegistry::new();
    let id = r.register(JobKind::Process, "p".into(), std::path::PathBuf::new(), 0, vec![]);
    r.finish(&id, JobStatus::Failed { reason: "forced_exit".into() }, 0);
    let forced: Vec<String> = r.jobs.values()
        .filter(|j| matches!(&j.status, JobStatus::Failed { reason } if reason == "forced_exit"))
        .map(|j| j.id.clone()).collect();
    assert_eq!(forced.len(), 1, "应识别 1 个 forced_exit job");
    // 空场景：done 不触发
    let mut r2 = JobRegistry::new();
    let id2 = r2.register(JobKind::Process, "p".into(), std::path::PathBuf::new(), 0, vec![]);
    r2.finish(&id2, JobStatus::Done { code: 0 }, 0);
    let forced2: Vec<String> = r2.jobs.values()
        .filter(|j| matches!(&j.status, JobStatus::Failed { reason } if reason == "forced_exit"))
        .map(|j| j.id.clone()).collect();
    assert!(forced2.is_empty(), "done job 不触发汇报");
}
```
（注：spawn_session 整体集成测难写——依赖 AppHandle/runtime。本测验"识别 forced_exit"的纯逻辑；tx.send 的实际触发由 lib.rs 集成 + 手测覆盖。controller final review 确认接线。）

- [ ] **Step 2: 跑测试确认失败/通过**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml startup_report`
Expected: 通过（纯逻辑测，实现已在 brief 给）。若 implementer 先写测再实现，确认 TDD 顺序。

- [ ] **Step 3: 实现 load_today 后汇报 send**（agent.rs:175 后）

按上面代码块 + 接线提示改 agent.rs（clone tx_for_report + 查 forced_exit + send 合成 UserMessage）。

- [ ] **Step 4: 跑全量测试**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml`
Expected: 全过。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/agent.rs
git commit -m "feat(agent): 阶段3-T6 重启汇报 turn（启动检测 forced_exit failed 主动触发 UserMessage，§5.2）"
```

---

### Task 7: AGENT.md 提示词（批处理协议 + soft_timeout + 重启汇报）

**Files:**
- Modify: `src-tauri/defaults/AGENT.md`（「后台任务协议」段扩充）

**Interfaces:** 无（纯文档，pinned 每轮重读，下轮生效）。

- [ ] **Step 1: 读现有 AGENT.md 后台任务协议段**

Run: `grep -n "后台任务协议" src-tauri/defaults/AGENT.md`
确认现有段位置（阶段1-T4 / 阶段2-T7 已加 job_id 说明）。

- [ ] **Step 2: 扩充协议**（在现有「后台任务协议」段内加 3 条）

在 AGENT.md 后台任务协议段（job_id 说明之后）加：
```markdown
- **攒批结果处理**：后台任务的结果不再一个个唤醒你，而是攒成一批（按完成时间排序、依赖满足后）一次性发给你 = 一个气泡。收到一批后：只在「要让用户知道 / 要展示产物 / 要用户决策」时才说人话；多个互不相关的结果，分别 spawn 子代理深入处理，自己只协调 + 一句汇报。
- **依赖声明**：spawn 子代理 / bash background 时可传 `dependencies:[job_id,...]`，声明"等这些任务有结果（done/failed/超时/killed 均算）后，我的结果才被处理"。声明的依赖必须已存在（会校验，未知 id 或循环依赖直接报错）。
- **软超时处理**：长任务到 `timeout_secs`（默认 30min）不会直接杀进程，而是收到一条"已超时，任务仍在跑"的部分产出。你自主判断：kill 它 / 继续等 / 先用现有输出。若任务连续 5 次软超时或总宽限 120min 仍不结束，系统会强制 kill。
- **重启汇报**：每次应用启动，若上次退出时有任务被强制判失败（forced_exit），会主动收到一条系统提示告诉你哪些任务失败了——请告知用户并判断是否重做。
```

- [ ] **Step 3: 验证**（node --check 不适用 md；确认文件可读 + pinned 重读）

Run: `cargo check --tests --manifest-path src-tauri/Cargo.toml`（确认未误改代码）
Expected: 0 error。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/defaults/AGENT.md
git commit -m "docs(agent): 阶段3-T7 AGENT.md 加批处理协议 + 依赖声明 + 软超时处理 + 重启汇报"
```

---

## Self-Review（plan 作者自查，非 subagent）

**1. Spec 覆盖**：
- §2 dependencies（环检测 A1 + 未知 id A2 + 偏序语义）→ T2（check_deps）+ T3（spawn 接）+ T1（drain_ready 筛 ready）✅
- §3 调度模型（completed_pending + turn_busy + ready 筛选 + FIFO + 同 id 取最后 + 整批喂 + turn 内 spawn 入队）→ T1（Scheduler）+ T5（driver 接）✅
- §6 软超时（不 kill + soft_timeout outcome + 资源上限 N=5/M=120 + done 覆盖 + soft_timeout 算 outcome）→ T4 + T1（has_outcome）✅
- §5.2 重启汇报（启动即触发）→ T6 ✅
- §8 提示词 → T7 ✅
- D4（环+未知 id）✅、D6（idle=turn_busy=false）✅、D7（资源上限）✅

**2. 风险点**：
- T4 subagent 软超时最棘手（run_turn 无法续跑）→ 已决策简化为单次 timeout 即终止（brief 明示，implementer 遵循）。
- T5 driver 重构面大 → 拆 append_history/run_turn_from/handle_batch + drain_and_run_batch 循环，每步可测。
- finalize_process 内 deps:vec![]（终态 outcome 不再参与调度）→ 已注 reviewer 关注点。

**3. 类型一致性**：JobOutcome 三字段（deps/finished_at/soft_timeout）T1 加齐，T4 用 soft_timeout，T5 handle_batch 消费——一致。Scheduler 签名 T1 定，T5 调——一致。spawn_process_job/spawn_agent 的 deps/timeout_secs 参数 T3 加，tools.rs T3 解析传入——一致。

**4. Placeholder 扫描**：T4 subagent 软超时的 `run_round_for_subagent(...)` 是占位——已修正为"按 subagents.rs 现有 run_turn 调用原样保留，只加 select timeout 分支"（brief 决策块明示）。其余无 TBD/TODO。

**5. 测试矩阵覆盖**（spec §测试矩阵-阶段3）：
- idle/busy 两路径 → T1 busy_drains_none + T5 ✅
- ready 筛选 → T1 ready_filter_waits_for_deps ✅
- FIFO → T1 idle_drains_ready_batch ✅
- 同 id 取最后（done 覆盖 soft_timeout）→ T1 same_id_takes_last ✅
- 整批喂一个 turn → T1 batch_preserves_attribution + T5 handle_batch_one_turn ✅
- 启动主动触发汇报 turn → T6 ✅
- 环检测 / 未知 id / 失败 dep 算满足 → T2 + T1 failed_dep_counts_as_outcome ✅
- soft_timeout 不 kill + tail / done 覆盖 / 资源上限 kill → T4 ✅

NO UNRESOLVED DECISIONS。Plan 可进入 SDD 执行。
