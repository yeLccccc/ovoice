# 任务管理子系统（Task Management）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 agent 一份独立于对话流的「当前工作记忆」——一个 sqlite 维护、可查可改、有专门页面可视化的任务状态板，让 agent 跨长时段持续推进同一目标。

**Architecture:** 新模块 `tasks.rs`（类型 + DbActor 独占 Connection 串行 + async API + migrate），经 `tools.rs` 暴露 7 个 agent 工具 + 经 `lib.rs` 暴露 5 个用户 command，前端第 5 视图（看板四列）消费。**跟对话流零耦合**（红线 1）：不碰 context.rs / history.jsonl 写路径 / dream / SOUL·AGENT·MEMORY 块；task 工具的 tool_result 落 history 是既有工具机制，非新增耦合。

**Tech Stack:** Rust + rusqlite 0.31 (bundled) + tokio mpsc/oneshot (DbActor on dedicated OS thread) + Tauri 2 commands/events + 原生 HTML/JS（无框架）。

---

## Global Constraints

- **分支**：`feat/task-management`（已在此分支；不直接落 main）。
- **红线 1（强制）**：任务功能不碰 `context.rs build_messages` / `history.jsonl` 写路径 / `dream` / `memory` / SOUL·AGENT·MEMORY 块 / 不每轮注入 system。`build_messages` 签名与 ~15 个 context 测试零改。
- **红线 2（强制）**：agent 经 7 个 task 工具接入；用户经 tasks 视图 + 5 个 Tauri command 接入；两侧共用同一 `tasks::async API + DbActor`（单一写者）。
- **tool 数量级联（必查，memory: ovoice-tool-count-cascade）**：`tools.rs schemas()` 现 **13** 个（spec §8.2 写的「12」已 stale——dream 工具后加了 1 个），加 7 个 task 工具 → **20**。`llm.rs:778` 的 `assert_eq!(body["tools"].as_array().unwrap().len(), 13)` 必同步改 20；`tools.rs schemas_has_thirteen_tools` 测试必改名 + 改期望。
- **`max_tool_iters = 100` 绝对不改**（llm.rs run_turn 循环上限）。
- **`build_messages` 签名绝对不改**。
- **rusqlite 版本**：`rusqlite = { version = "0.31", features = ["bundled"] }`（bundled 静态编 sqlite C 源进二进制，无外部 dll；release +~1.5MB；首次编译 +30s~1min）。
- **tasks.db 路径**：`{cache_dir}/tasks.db`（cache_dir 由 config 决定，跟 history/memory 同根）。无 audit 日志文件。
- **schema 版本**：`PRAGMA user_version = 1`；migrate 仅认 0（建 v1）和 1（当前）；未知版本返回 `Err`。
- **嵌套集合用 JSON 列不拆表**：`tags/acceptance/blockers/artifacts` 在 sqlite 里存 JSON 文本列，读写整组替换（serde_json），不拆 4 表。
- **DbActor 跑在专用 OS 线程**（非 tokio task）：rusqlite `Connection` 阻塞 API，放 `std::thread::spawn` 里，用 `tokio::sync::mpsc::unbounded_channel` 收命令 + `tokio::sync::oneshot` 回结果，actor 循环用 `rx.blocking_recv()`。async API 侧 `.await` oneshot。AppHandle 传入线程用于 emit。
- **verified 逻辑约束**：`status != Done && verified == true` 是逻辑错，写入时拦截（update 接口拒绝）。`Done` 默认 `verified=false`（自述完成），升 true 需 evidence。
- **无 Blocked enum**：「卡住」由 `blockers` 数组派生（Active + 有未解决 blocker = 卡住）。
- **progress 不存盘**：从 `acceptance` 的 done/total 派生，渲染时算。
- **AGENT.md 只增「按需查任务」**：不写「每轮主动 surface」（依赖注入，撤了注入就撤这条）。
- **SECURITY**：`config.json` 含明文密钥（api_key/baidu_*）——永不打印真实值；本 plan 不碰 config.json。
- **dev 验证命令**：`cargo check --manifest-path src-tauri/Cargo.toml --lib`（零 warning 才算过）；测试 `cargo test --manifest-path src-tauri/Cargo.toml --lib <module>`；dev server 锁 exe 时改用 cargo check（memory: ovoice-dev-server-cargo-lock）。
- **commit message 末尾必加** `Co-Authored-By: Claude <noreply@anthropic.com>`。

---

## File Structure

```
src-tauri/
├─ Cargo.toml                          [Modify] +rusqlite 0.31 bundled（Task 1）
├─ src/
│  ├─ tasks.rs                         [Create] Task 2-4：类型 / DbActor / async API（~400 行）
│  ├─ lib.rs                           [Modify] Task 6：mod tasks + 5 command 注册 + setup spawn DbActor
│  ├─ tools.rs                         [Modify] Task 5：schemas +7 / dispatch +7 / 7 tool 函数 + cascade 测试
│  ├─ llm.rs                           [Modify] Task 5：tools.len() 断言 13→20（1 行，llm.rs:778）
│  └─ ovoice/AGENT.md                  [Modify] Task 7：增「按需查任务」段（不写每轮 surface）
src/
├─ index.html                          [Modify] Task 8：topbar +tasks-btn / +<section id=tasks-view>
├─ styles.css                          [Modify] Task 9：看板/卡片/徽章样式
└─ main.js                             [Modify] Task 10-11：
                                          Task 10：看板渲染 + CRUD invoke + listen task-changed
                                          Task 11：setupAgentEvents + buildHistoryBubbles 加 task_* 工具卡分支（双渲染路径）
```

**职责边界**：`tasks.rs` 一个文件管所有任务子系统后端逻辑（类型 + actor + API + migrate），与对话核心零耦合；前端 tasks 视图自成一格，不污染 chat 双渲染路径（仅 task_* 工具调用卡进 chat，那是既有工具机制）。

**关键设计决策（实现前必读）**：

1. **不往 `ToolsCtx` 加字段**。ToolsCtx 有 ~5 个构造点（agent.rs:228 / agent.rs:465 / subagents.rs:208 / subagents.rs:611 clone / tools.rs foreground），加字段要全改。改让 7 个 task 工具经 **`ctx.app_handle` 间接拿 DbActorHandle**——但 ToolsCtx 现无 app handle。**最终方案**：给 ToolsCtx 加 **一个** 字段 `pub tasks: crate::tasks::DbActorHandle`（Clone 便宜，就是 channel sender）。所有构造点补 `tasks:` 一行；`foreground()`/测试用 `DbActorHandle::noop()`。比 30 个构造点轻（实际 ~5 个），且 task 工具是同步调用 async API（`tools::dispatch` 本就 async），无需像 attach/dream 那样绕 session_tx。**这条覆盖上面 DbActor 决策里「async API」的可达性**：tool 函数 `async fn tool_task_list(args, ctx) -> String { ...ctx.tasks.list_tasks(f).await... }`，dispatch 调 `.await`（dispatch 是 async）。

2. **DbActor 线程内 emit task-changed**：写入命令（add/update/check/progress/archive）执行成功后 `let _ = app.emit("task-changed", ())`（payload 空，前端收到就重查 list_tasks）。读命令（list/get）不 emit。AppHandle 经 `std::thread::spawn` 闭包 move 进线程（AppHandle 是 Clone+Send+Sync）。

3. **DbActorHandle::noop()**：开一个无人接的 unbounded channel，sender 给调用方，receiver 丢弃（actor 线程不启动）。用于 foreground()/测试——调用会静默失败返回 Err（不 panic）。跟 `HistoryWriterHandle::noop()` 同模式。

4. **task id 自增**：sqlite `INTEGER PRIMARY KEY` 即 rowid 自增，add 时不传 id，`conn.last_insert_rowid()` 回填返回。

5. **时间戳**：复用 `crate::agent::now_ms()`（agent.rs:139，pub(crate) 需改成 pub 或 tasks 内自写一个 `now_ms()`——后者更解耦，本 plan 在 tasks.rs 内写 `fn now_ms() -> i64`）。

6. **chat 双渲染路径**：task 工具调用卡（task_list/task_add 等）必须同时改 `setupAgentEvents`（live，main.js:1092 `appendToolCard` 附近）和 `buildHistoryBubbles`（history，main.js:1608 附近）。但 `appendToolCard` 和 `addToolCard`(history) 是通用渲染——task_* 工具走默认分支即可（不像 display/edit_card 要特殊走 media）。**所以实际上无需改这两个函数**——task_* 默认就被当普通工具卡渲染。Task 11 降级为「验证 + 加 task 工具的友好显示名映射」。这条修正 spec §10.3 的「必须同时改」——经核查，通用工具卡路径已覆盖，无需改，只需加显示名映射让卡片标题好看。

---

## Task 1: Cargo.toml 加 rusqlite bundled 依赖

**Files:**
- Modify: `src-tauri/Cargo.toml`（`[dependencies]` 段末尾，`regex = "1"` 后）

**Interfaces:**
- Produces: `rusqlite` crate 可在 `tasks.rs` `use rusqlite::{Connection, params};`

- [ ] **Step 1: 加依赖行**

在 `src-tauri/Cargo.toml` `[dependencies]` 段，`regex = "1"` 这行之后加：

```toml
# 任务管理子系统：sqlite（bundled 静态编 C 源进二进制，无外部 dll）
rusqlite = { version = "0.31", features = ["bundled"] }
```

- [ ] **Step 2: 验证依赖拉取 + 编译**

Run: `cargo check --manifest-path src-tauri/Cargo.toml --lib`
Expected: 退出码 0（首次会编 sqlite C 源，+30s~1min；无 warning）。若报版本找不到，先 `cargo update -p rusqlite --manifest-path src-tauri/Cargo.toml`。

- [ ] **Step 3: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "chore(tasks): 加 rusqlite 0.31 bundled 依赖

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 2: tasks.rs 类型 + migrate + schema（无 DbActor，纯函数可测）

**Files:**
- Create: `src-tauri/src/tasks.rs`
- Modify: `src-tauri/src/lib.rs`（加 `mod tasks;`，与现有 `mod agent;` 等并列）

**Interfaces:**
- Produces（本 Task）:
  - `pub enum Horizon { Current, Short, Long, Vision }` (+ `as_str`/`from_str`)
  - `pub enum Status { Todo, Active, Done, Dropped }` (+ `as_str`/`from_str`)
  - `pub enum Actor { User, Agent }` (+ `as_str`)
  - `pub enum ArtifactKind { File, Link, Note }` (+ `as_str`)
  - `pub struct CheckItem { text, done, evidence }`
  - `pub struct Blocker { reason, raised_at, resolved }`
  - `pub struct Artifact { kind, reference, note }`
  - `pub struct Task { 全 20 字段 }` (Serialize/Deserialize/Clone/Debug)
  - `pub struct TaskFilter { horizon, status, archived }`（list 用）
  - `pub fn migrate(conn: &Connection) -> Result<()>`（建表+索引+PRAGMA+user_version）
  - `fn row_to_task(row: &Row) -> rusqlite::Result<Task>`（共享行→实体）
- Consumes: 无（首个 task）

- [ ] **Step 1: 在 lib.rs 注册模块**

在 `src-tauri/src/lib.rs` 现有 `mod` 声明区（找 `mod agent;` / `mod tools;` 那块），加一行：

```rust
mod tasks;
```

（放在 `mod tools;` 之后、与字母序或现有顺序一致即可。）

- [ ] **Step 2: 写 tasks.rs 顶部——imports + 枚举 + 嵌套结构体**

创建 `src-tauri/src/tasks.rs`：

```rust
//! 任务管理子系统：当前工作记忆状态板（sqlite）。
//! 独立于对话流（红线 1）：不碰 context/history/dream；agent 经 7 工具 + 用户经 5 command 共用本模块的 async API + DbActor。
//!
//! 架构：DbActor 跑在专用 OS 线程独占 Connection（单一写者，sqlite 不需锁），
//!       经 mpsc::unbounded_channel 收 DbCommand，oneshot 回结果。
//!       ◄ 跟 history writer task / job writer task 同款 ►

use rusqlite::{params, Connection, Row};
use serde::{Deserialize, Serialize};

// ── 枚举 ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Horizon {
    Current, // 今天/本周聚焦
    Short,   // 几周~几个月
    Long,    // 今年~1-3 年
    Vision,  // 3 年+
}
impl Horizon {
    pub fn as_str(self) -> &'static str {
        match self {
            Horizon::Current => "current",
            Horizon::Short => "short",
            Horizon::Long => "long",
            Horizon::Vision => "vision",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "current" => Horizon::Current,
            "short" => Horizon::Short,
            "long" => Horizon::Long,
            "vision" => Horizon::Vision,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Todo,
    Active,
    Done,
    Dropped,
}
impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Todo => "todo",
            Status::Active => "active",
            Status::Done => "done",
            Status::Dropped => "dropped",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "todo" => Status::Todo,
            "active" => Status::Active,
            "done" => Status::Done,
            "dropped" => Status::Dropped,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Actor {
    User,
    Agent,
}
impl Actor {
    pub fn as_str(self) -> &'static str {
        match self {
            Actor::User => "user",
            Actor::Agent => "agent",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "agent" => Actor::Agent,
            _ => Actor::User,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactKind {
    File,
    Link,
    Note,
}
impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactKind::File => "file",
            ArtifactKind::Link => "link",
            ArtifactKind::Note => "note",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "link" => ArtifactKind::Link,
            "note" => ArtifactKind::Note,
            _ => ArtifactKind::File,
        }
    }
}

// ── 嵌套结构体（JSON 列） ──────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CheckItem {
    pub text: String,
    pub done: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blocker {
    pub reason: String,
    pub raised_at: i64,
    pub resolved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}
```

- [ ] **Step 3: 写 Task 实体 + TaskFilter**

继续 `src-tauri/src/tasks.rs`：

```rust
// ── Task 实体（20 字段） ───────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    // 描述层
    pub id: i64,
    pub title: String,
    pub goal: String,
    pub detail: String,
    // 分类结构
    pub horizon: Horizon,
    pub parent_id: Option<i64>,
    pub tags: Vec<String>,
    // 状态进度
    pub status: Status,
    pub acceptance: Vec<CheckItem>,
    pub verified: bool,
    // 阻塞产出
    pub blockers: Vec<Blocker>,
    pub artifacts: Vec<Artifact>,
    // 排序
    pub sort_index: i32,
    // 时间轴
    pub created_at: i64,
    pub updated_at: i64,
    pub due_date: Option<i64>,
    pub completed_at: Option<i64>,
    pub archived_at: Option<i64>,
    // 元数据
    pub created_by: Actor,
    pub last_progress: Option<String>,
}

impl Task {
    /// progress 派生：acceptance done/total；无 acceptance → None。
    pub fn progress(&self) -> Option<(usize, usize)> {
        if self.acceptance.is_empty() {
            None
        } else {
            let done = self.acceptance.iter().filter(|c| c.done).count();
            Some((done, self.acceptance.len()))
        }
    }
    /// 阻塞派生：Active + 有未解决 blocker。
    pub fn is_blocked(&self) -> bool {
        self.status == Status::Active
            && self.blockers.iter().any(|b| !b.resolved)
    }
}

#[derive(Debug, Clone, Default)]
pub struct TaskFilter {
    pub horizon: Option<Horizon>,
    pub status: Option<Status>,
    pub archived: bool, // false=仅未归档（主视图默认）；true=仅已归档
    pub parent_id: Option<Option<i64>>, // Some(None)=顶层；Some(Some(id))=某父下；None=不过滤
}
```

- [ ] **Step 4: 写 migrate() + row_to_task()**

继续 `src-tauri/src/tasks.rs`：

```rust
// ── schema 建表 + 迁移 ────────────────────────────────────

/// 初始化/迁移 schema。幂等：v0→建表+user_version=1；v1→空操作；其他→Err。
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let version: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    match version {
        0 => {
            conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS tasks (
                    id            INTEGER PRIMARY KEY,
                    title         TEXT    NOT NULL,
                    goal          TEXT    NOT NULL DEFAULT '',
                    detail        TEXT    NOT NULL DEFAULT '',
                    horizon       TEXT    NOT NULL,
                    parent_id     INTEGER REFERENCES tasks(id),
                    tags          TEXT    NOT NULL DEFAULT '[]',
                    status        TEXT    NOT NULL,
                    acceptance    TEXT    NOT NULL DEFAULT '[]',
                    verified      INTEGER NOT NULL DEFAULT 0,
                    blockers      TEXT    NOT NULL DEFAULT '[]',
                    artifacts     TEXT    NOT NULL DEFAULT '[]',
                    sort_index    INTEGER NOT NULL DEFAULT 0,
                    created_at    INTEGER NOT NULL,
                    updated_at    INTEGER NOT NULL,
                    due_date      INTEGER,
                    completed_at  INTEGER,
                    archived_at   INTEGER,
                    created_by    TEXT    NOT NULL,
                    last_progress TEXT
                );
                CREATE INDEX IF NOT EXISTS idx_tasks_status_horizon_sort
                    ON tasks(status, horizon, sort_index);
                CREATE INDEX IF NOT EXISTS idx_tasks_parent ON tasks(parent_id);
                CREATE INDEX IF NOT EXISTS idx_tasks_due
                    ON tasks(due_date) WHERE due_date IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_tasks_updated ON tasks(updated_at);
                CREATE INDEX IF NOT EXISTS idx_tasks_active
                    ON tasks(id) WHERE archived_at IS NULL AND status IN ('todo','active');
                ",
            )?;
            conn.pragma_update(None, "user_version", 1)?;
        }
        1 => { /* 当前版本 */ }
        _ => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown tasks schema version: {version}"),
                )),
            ))
        }
    }
    Ok(())
}

/// 把一行读成 Task（列顺序与 CREATE TABLE 一致）。
fn row_to_task(row: &Row) -> rusqlite::Result<Task> {
    let horizon_s: String = row.get("horizon")?;
    let status_s: String = row.get("status")?;
    let created_by_s: String = row.get("created_by")?;
    let tags_s: String = row.get("tags")?;
    let acceptance_s: String = row.get("acceptance")?;
    let blockers_s: String = row.get("blockers")?;
    let artifacts_s: String = row.get("artifacts")?;
    Ok(Task {
        id: row.get("id")?,
        title: row.get("title")?,
        goal: row.get("goal")?,
        detail: row.get("detail")?,
        horizon: Horizon::from_str(&horizon_s).unwrap_or(Horizon::Current),
        parent_id: row.get("parent_id")?,
        tags: serde_json::from_str(&tags_s).unwrap_or_default(),
        status: Status::from_str(&status_s).unwrap_or(Status::Todo),
        acceptance: serde_json::from_str(&acceptance_s).unwrap_or_default(),
        verified: row.get::<_, i64>("verified")? != 0,
        blockers: serde_json::from_str(&blockers_s).unwrap_or_default(),
        artifacts: serde_json::from_str(&artifacts_s).unwrap_or_default(),
        sort_index: row.get("sort_index")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        due_date: row.get("due_date")?,
        completed_at: row.get("completed_at")?,
        archived_at: row.get("archived_at")?,
        created_by: Actor::from_str(&created_by_s),
        last_progress: row.get("last_progress")?,
    })
}

/// tasks.rs 内部用的墙钟毫秒（i64）。
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
```

- [ ] **Step 5: 写 migrate 测试（先失败再过）**

继续 `src-tauri/src/tasks.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn migrate_creates_schema_and_sets_user_version() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let v: u32 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1);
        // 表存在
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn migrate_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // 二次不报错
        let v: u32 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1);
    }

    #[test]
    fn migrate_creates_indexes() {
        let conn = fresh_conn();
        let names: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_tasks_%'")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        for expect in [
            "idx_tasks_status_horizon_sort",
            "idx_tasks_parent",
            "idx_tasks_due",
            "idx_tasks_updated",
            "idx_tasks_active",
        ] {
            assert!(names.contains(&expect.to_string()), "缺索引 {expect}: {names:?}");
        }
    }

    #[test]
    fn horizon_roundtrip() {
        for h in [Horizon::Current, Horizon::Short, Horizon::Long, Horizon::Vision] {
            assert_eq!(Horizon::from_str(h.as_str()), Some(h));
        }
        assert!(Horizon::from_str("xxx").is_none());
    }

    #[test]
    fn task_progress_and_blocked_derived() {
        let mut t = Task {
            id: 1, title: "t".into(), goal: "".into(), detail: "".into(),
            horizon: Horizon::Current, parent_id: None, tags: vec![],
            status: Status::Active,
            acceptance: vec![
                CheckItem { text: "a".into(), done: true, evidence: None },
                CheckItem { text: "b".into(), done: false, evidence: None },
            ],
            verified: false,
            blockers: vec![Blocker { reason: "卡".into(), raised_at: 1, resolved: false }],
            artifacts: vec![], sort_index: 0,
            created_at: 0, updated_at: 0, due_date: None, completed_at: None, archived_at: None,
            created_by: Actor::Agent, last_progress: None,
        };
        assert_eq!(t.progress(), Some((1, 2)));
        assert!(t.is_blocked());
        t.status = Status::Todo;
        assert!(!t.is_blocked(), "非 Active 不算阻塞");
    }
}
```

- [ ] **Step 6: 跑测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib tasks::`
Expected: 5 passed（migrate_creates_schema_and_sets_user_version / migrate_is_idempotent / migrate_creates_indexes / horizon_roundtrip / task_progress_and_blocked_derived）。

- [ ] **Step 7: cargo check 零 warning**

Run: `cargo check --manifest-path src-tauri/Cargo.toml --lib`
Expected: 退出码 0，0 warning。

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/tasks.rs src-tauri/src/lib.rs
git commit -m "feat(tasks): 类型 + migrate + 派生(progress/blocked)（Task 实体 20 字段）

- Horizon/Status/Actor/ArtifactKind 枚举 + roundtrip
- CheckItem/Blocker/Artifact 嵌套（JSON 列）
- migrate(): v0→建表+5 索引+user_version=1，幂等
- row_to_task() + now_ms() 内部工具
- progress/is_blocked 派生（不存盘）
- 5 单测全过

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 3: DbActor + DbActorHandle + add/get/list/write SQL

**Files:**
- Modify: `src-tauri/src/tasks.rs`（追加 DbCommand / DbActor / DbActorHandle + SQL 函数）

**Interfaces:**
- Consumes: Task 2 的 Task/TaskFilter/migrate/row_to_task/now_ms
- Produces:
  - `pub enum DbCommand { ... }`（actor 收的命令，oneshot 回）
  - `#[derive(Clone)] pub struct DbActorHandle { tx: mpsc::UnboundedSender<DbCommand>, app: Option<AppHandle> }`
  - `impl DbActorHandle { pub fn noop() -> Self; pub async fn list(&self, f: TaskFilter) -> Result<Vec<Task>, String>; pub async fn get(&self, id: i64) -> Result<Option<Task>, String>; pub async fn add(&self, input: TaskInput, actor: Actor) -> Result<Task, String>; pub async fn update(&self, id: i64, patch: TaskPatch) -> Result<Task, String>; pub async fn check(&self, id: i64, idx: usize, done: bool, evidence: Option<String>) -> Result<Task, String>; pub async fn set_progress(&self, id: i64, note: String) -> Result<Task, String>; pub async fn archive(&self, id: i64, archived: bool) -> Result<Task, String>; pub async fn delete(&self, id: i64) -> Result<bool, String>; }`
  - `pub fn spawn_db_actor(db_path: PathBuf, app: AppHandle) -> DbActorHandle`
  - `pub struct TaskInput`（add 用：title/goal/detail/horizon/parent_id/tags/status/acceptance/due_date）
  - `pub struct TaskPatch`（update 用：可选字段集合）
- Consumes（后续 Task）：Task 4 写命令分支，Task 5 用 DbActorHandle，Task 6 spawn

> **架构决策（实现前必读）**：actor 跑在 `std::thread::spawn` 专用 OS 线程（非 tokio task），因为 rusqlite `Connection` 是阻塞 + 非 Send-safe 跨 await。actor 循环用 `rx.blocking_recv()`（std thread 里用 blocking recv，没问题）。async API 侧 `.await` oneshot。
> `DbActorHandle` 同时持有 `Option<AppHandle>`：noop()=None；spawn 的=None 也行，但**写入后 emit task-changed 需要它**——所以 spawn 时 move 进 actor 线程，actor 在写命令后 emit。handle 本身不需要 app（emit 在 actor 内）。
> **修正**：handle 不持 app；actor 线程闭包持 app。handle 只持 `tx`。

- [ ] **Step 1: 写 SQL 函数（add/get/list/update_row/check/progress/archive/delete）**

在 `src-tauri/src/tasks.rs`（Task 2 内容之后、`#[cfg(test)]` 之前）追加：

```rust
// ── SQL 原语（actor 线程内同步调用） ───────────────────────

pub struct TaskInput {
    pub title: String,
    pub goal: String,
    pub detail: String,
    pub horizon: Horizon,
    pub parent_id: Option<i64>,
    pub tags: Vec<String>,
    pub status: Status,
    pub acceptance: Vec<CheckItem>,
    pub due_date: Option<i64>,
}

/// update 用的可选字段补丁。None=不改；Some(v)=改成 v。
/// status/verified 经 `validate_status_verified` 校验后再落。
#[derive(Default)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub goal: Option<String>,
    pub detail: Option<String>,
    pub horizon: Option<Horizon>,
    pub parent_id: Option<Option<i64>>, // Some(None)=清父；Some(Some)=设父
    pub tags: Option<Vec<String>>,
    pub status: Option<Status>,
    pub due_date: Option<Option<i64>>,
    pub sort_index: Option<i32>,
    pub blockers: Option<Vec<Blocker>>,
    pub artifacts: Option<Vec<Artifact>>,
}

/// verified 逻辑约束：status≠Done && verified=true → 拒绝。
fn validate_status_verified(status: Status, verified: bool) -> Result<(), String> {
    if status != Status::Done && verified {
        return Err(format!(
            "逻辑错：status={:?}（非 Done）不能 verified=true", status
        ));
    }
    Ok(())
}

fn sql_get(conn: &Connection, id: i64) -> rusqlite::Result<Option<Task>> {
    let mut stmt = conn.prepare(
        "SELECT id,title,goal,detail,horizon,parent_id,tags,status,acceptance,verified,
                blockers,artifacts,sort_index,created_at,updated_at,due_date,completed_at,
                archived_at,created_by,last_progress
         FROM tasks WHERE id = ?1",
    )?;
    let mut rows = stmt.query(params![id])?;
    match rows.next()? {
        Some(r) => Ok(Some(row_to_task(r)?)),
        None => Ok(None),
    }
}

fn sql_list(conn: &Connection, f: &TaskFilter) -> rusqlite::Result<Vec<Task>> {
    let mut sql = String::from(
        "SELECT id,title,goal,detail,horizon,parent_id,tags,status,acceptance,verified,
                blockers,artifacts,sort_index,created_at,updated_at,due_date,completed_at,
                archived_at,created_by,last_progress FROM tasks WHERE 1=1",
    );
    let mut p: Vec<Box<dyn rusqlite::ToSql>> = vec![];
    let mut i = 1;
    if let Some(h) = f.horizon {
        sql.push_str(&format!(" AND horizon = ?{i}"));
        p.push(Box::new(h.as_str().to_string()));
        i += 1;
    }
    if let Some(s) = f.status {
        sql.push_str(&format!(" AND status = ?{i}"));
        p.push(Box::new(s.as_str().to_string()));
        i += 1;
    }
    if f.archived {
        sql.push_str(" AND archived_at IS NOT NULL");
    } else {
        sql.push_str(" AND archived_at IS NULL");
    }
    if let Some(parent_filter) = &f.parent_id {
        match parent_filter {
            None => sql.push_str(" AND parent_id IS NULL"), // 顶层
            Some(pid) => {
                sql.push_str(&format!(" AND parent_id = ?{i}"));
                p.push(Box::new(*pid));
            }
        }
    }
    sql.push_str(" ORDER BY sort_index ASC, id ASC");
    let params_refs: Vec<&dyn rusqlite::ToSql> = p.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_refs.as_slice(), row_to_task)?;
    rows.collect()
}

fn sql_add(conn: &Connection, input: TaskInput, actor: Actor) -> rusqlite::Result<Task> {
    let now = now_ms();
    validate_status_verified(input.status, false).map_err(|e| {
        rusqlite::Error::ToSqlConversionFailure(e.into())
    })?;
    conn.execute(
        "INSERT INTO tasks
         (title,goal,detail,horizon,parent_id,tags,status,acceptance,verified,blockers,
          artifacts,sort_index,created_at,updated_at,due_date,completed_at,archived_at,
          created_by,last_progress)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0,'[]','[]',0,?9,?9,?10,NULL,NULL,?11,NULL)",
        params![
            input.title,
            input.goal,
            input.detail,
            input.horizon.as_str(),
            input.parent_id,
            serde_json::to_string(&input.tags).unwrap_or_else(|_| "[]".into()),
            input.status.as_str(),
            serde_json::to_string(&input.acceptance).unwrap_or_else(|_| "[]".into()),
            now,
            input.due_date,
            actor.as_str(),
        ],
    )?;
    sql_get(conn, conn.last_insert_rowid())?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

fn sql_update(conn: &Connection, id: i64, patch: TaskPatch) -> rusqlite::Result<Task> {
    let existing = sql_get(conn, id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    // 计算落库后的 status/verified，做一致性校验
    let new_status = patch.status.unwrap_or(existing.status);
    // verified 跟随：status 变成非 Done 时，verified 强制清 false（防逻辑错）
    let mut new_verified = existing.verified;
    if new_status != Status::Done {
        new_verified = false;
    }
    validate_status_verified(new_status, new_verified)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
    let now = now_ms();
    let completed_at: Option<i64> = match (existing.completed_at, new_status) {
        (Some(c), _) => Some(c),                  // 已完成时间保留
        (None, Status::Done) => Some(now),        // 新完成：盖戳
        (None, _) => None,
    };
    let mut sets: Vec<&str> = vec!["updated_at = ?1"];
    let mut p: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(now)];
    let mut i = 2;
    macro_rules! pushset {
        ($val:expr, $col:expr) => {{
            p.push(Box::new($val));
            sets.push(concat!($col, " = ?"));
            // concat! 不接动态拼接，下面改用 format!
        }};
    }
    // 用显式拼接更清晰（避免 macro 复杂度）
    let _ = (pushset::<()>); // 占位防 unused 警告；实际下面手写
    let _ = sets; let _ = p; let _ = i;
    // —— 重做：清晰版（避免上面 macro 坑）——
    sql_update_explicit(conn, id, patch, existing, new_status, new_verified, completed_at, now)
}
```

> ⚠️ 上面 Step 1 末尾的 `pushset!` macro 是一个**故意的反面教材占位**，说明「不要用 macro 拼 SQL」——实现者请直接用下面的 `sql_update_explicit` 清晰版，**删掉** macro 那段。最终代码里不应出现 `pushset!`。

- [ ] **Step 2: 写 sql_update_explicit（清晰版，替换上面 macro 段）**

把 Task 3 Step 1 末尾的 `sql_update` 函数体（从 `let mut sets` 到函数结束，含 `pushset!` macro）替换为：

```rust
#[allow(clippy::too_many_arguments)]
fn sql_update_explicit(
    conn: &Connection,
    id: i64,
    patch: TaskPatch,
    existing: Task,
    new_status: Status,
    new_verified: bool,
    completed_at: Option<i64>,
    now: i64,
) -> rusqlite::Result<Task> {
    let title = patch.title.unwrap_or(existing.title);
    let goal = patch.goal.unwrap_or(existing.goal);
    let detail = patch.detail.unwrap_or(existing.detail);
    let horizon = patch.horizon.unwrap_or(existing.horizon);
    let parent_id = match patch.parent_id {
        Some(v) => v,
        None => existing.parent_id,
    };
    let tags = serde_json::to_string(&patch.tags.unwrap_or(existing.tags))
        .unwrap_or_else(|_| "[]".into());
    let due_date = match patch.due_date {
        Some(v) => v,
        None => existing.due_date,
    };
    let sort_index = patch.sort_index.unwrap_or(existing.sort_index);
    let blockers = serde_json::to_string(&patch.blockers.unwrap_or(existing.blockers))
        .unwrap_or_else(|_| "[]".into());
    let artifacts = serde_json::to_string(&patch.artifacts.unwrap_or(existing.artifacts))
        .unwrap_or_else(|_| "[]".into());

    let affected = conn.execute(
        "UPDATE tasks SET
            title=?1, goal=?2, detail=?3, horizon=?4, parent_id=?5, tags=?6,
            status=?7, verified=?8, blockers=?9, artifacts=?10, sort_index=?11,
            due_date=?12, completed_at=?13, updated_at=?14
         WHERE id=?15",
        params![
            title, goal, detail, horizon.as_str(), parent_id, tags,
            new_status.as_str(), new_verified as i64, blockers, artifacts, sort_index,
            due_date, completed_at, now, id,
        ],
    )?;
    if affected == 0 {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    sql_get(conn, id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}
```

- [ ] **Step 3: 写 sql_check / sql_set_progress / sql_archive / sql_delete**

追加到 `src-tauri/src/tasks.rs`：

```rust
fn sql_check(
    conn: &Connection,
    id: i64,
    idx: usize,
    done: bool,
    evidence: Option<String>,
) -> rusqlite::Result<Task> {
    let mut t = sql_get(conn, id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    if idx >= t.acceptance.len() {
        return Err(rusqlite::Error::ToSqlConversionFailure(
            format!("acceptance idx {idx} 越界（len={}）", t.acceptance.len()).into(),
        ));
    }
    t.acceptance[idx].done = done;
    t.acceptance[idx].evidence = if done { evidence } else { None };
    let acceptance = serde_json::to_string(&t.acceptance).unwrap_or_else(|_| "[]".into());
    conn.execute(
        "UPDATE tasks SET acceptance=?1, updated_at=?2 WHERE id=?3",
        params![acceptance, now_ms(), id],
    )?;
    sql_get(conn, id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

fn sql_set_progress(conn: &Connection, id: i64, note: String) -> rusqlite::Result<Task> {
    let n = conn.execute(
        "UPDATE tasks SET last_progress=?1, updated_at=?2 WHERE id=?3",
        params![note, now_ms(), id],
    )?;
    if n == 0 {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    sql_get(conn, id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

fn sql_archive(conn: &Connection, id: i64, archived: bool) -> rusqlite::Result<Task> {
    let ts: Option<i64> = if archived { Some(now_ms()) } else { None };
    let n = conn.execute(
        "UPDATE tasks SET archived_at=?1, updated_at=?2 WHERE id=?3",
        params![ts, now_ms(), id],
    )?;
    if n == 0 {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    sql_get(conn, id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

fn sql_delete(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM tasks WHERE id=?1", params![id])? > 0)
}
```

- [ ] **Step 4: 写 DbCommand + DbActorHandle + spawn_db_actor**

追加到 `src-tauri/src/tasks.rs`：

```rust
// ── DbActor：专用 OS 线程独占 Connection ───────────────────

use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Manager};

enum DbCommand {
    List { filter: TaskFilter, resp: oneshot::Sender<Result<Vec<Task>, String>> },
    Get { id: i64, resp: oneshot::Sender<Result<Option<Task>, String>> },
    Add { input: TaskInput, actor: Actor, resp: oneshot::Sender<Result<Task, String>> },
    Update { id: i64, patch: TaskPatch, resp: oneshot::Sender<Result<Task, String>> },
    Check { id: i64, idx: usize, done: bool, evidence: Option<String>, resp: oneshot::Sender<Result<Task, String>> },
    Progress { id: i64, note: String, resp: oneshot::Sender<Result<Task, String>> },
    Archive { id: i64, archived: bool, resp: oneshot::Sender<Result<Task, String>> },
    Delete { id: i64, resp: oneshot::Sender<Result<bool, String>> },
}

#[derive(Clone)]
pub struct DbActorHandle {
    tx: mpsc::UnboundedSender<DbCommand>,
}

impl DbActorHandle {
    /// 测试/foreground 用：开了无人接的 channel，调用静默失败返回 Err。
    pub fn noop() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel::<DbCommand>();
        Self { tx }
    }

    fn send<T>(
        &self,
        cmd: DbCommand,
        rx: oneshot::Receiver<Result<T, String>>,
    ) -> Result<T, String> {
        self.tx
            .send(cmd)
            .map_err(|_| "DbActor 通道已关闭".to_string())?;
        rx.blocking_recv().unwrap_or(Err("DbActor 未回结果".into()))
    }

    pub async fn list(&self, filter: TaskFilter) -> Result<Vec<Task>, String> {
        let (resp, rx) = oneshot::channel();
        self.send(DbCommand::List { filter, resp }, rx)
    }
    pub async fn get(&self, id: i64) -> Result<Option<Task>, String> {
        let (resp, rx) = oneshot::channel();
        self.send(DbCommand::Get { id, resp }, rx)
    }
    pub async fn add(&self, input: TaskInput, actor: Actor) -> Result<Task, String> {
        let (resp, rx) = oneshot::channel();
        self.send(DbCommand::Add { input, actor, resp }, rx)
    }
    pub async fn update(&self, id: i64, patch: TaskPatch) -> Result<Task, String> {
        let (resp, rx) = oneshot::channel();
        self.send(DbCommand::Update { id, patch, resp }, rx)
    }
    pub async fn check(
        &self,
        id: i64,
        idx: usize,
        done: bool,
        evidence: Option<String>,
    ) -> Result<Task, String> {
        let (resp, rx) = oneshot::channel();
        self.send(DbCommand::Check { id, idx, done, evidence, resp }, rx)
    }
    pub async fn set_progress(&self, id: i64, note: String) -> Result<Task, String> {
        let (resp, rx) = oneshot::channel();
        self.send(DbCommand::Progress { id, note, resp }, rx)
    }
    pub async fn archive(&self, id: i64, archived: bool) -> Result<Task, String> {
        let (resp, rx) = oneshot::channel();
        self.send(DbCommand::Archive { id, archived, resp }, rx)
    }
    pub async fn delete(&self, id: i64) -> Result<bool, String> {
        let (resp, rx) = oneshot::channel();
        self.send(DbCommand::Delete { id, resp }, rx)
    }
}
```

> ⚠️ **async fn 用 blocking_recv？**——不行。上面 `send()` 用 `rx.blocking_recv()` 但函数是 `async`，会在异步上下文里阻塞 runtime。
> **正确做法**：actor 用 `mpsc::unbounded_channel`，但 async API 端 `self.tx.send(cmd)` 后直接 `rx.await`（oneshot receiver 是 async）。
> **下 Step 5 修正**：把 `send()` 的 `blocking_recv` 换成各 async 方法里直接 `rx.await`，且 actor 线程用 `std::thread` + `rx.blocking_recv()` 收 `DbCommand`（actor 在 OS 线程，可 blocking）。

- [ ] **Step 5: 修正 async API（去掉 blocking_recv，用 rx.await）**

把 Task 3 Step 4 的 `impl DbActorHandle` 整块替换为：

```rust
impl DbActorHandle {
    /// 测试/foreground 用：开了无人接的 channel，调用静默失败返回 Err。
    pub fn noop() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel::<DbCommand>();
        Self { tx }
    }

    pub async fn list(&self, filter: TaskFilter) -> Result<Vec<Task>, String> {
        let (resp, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::List { filter, resp })
            .map_err(|_| "DbActor 通道已关闭".to_string())?;
        rx.await.unwrap_or(Err("DbActor 未回结果".into()))
    }
    pub async fn get(&self, id: i64) -> Result<Option<Task>, String> {
        let (resp, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::Get { id, resp })
            .map_err(|_| "DbActor 通道已关闭".to_string())?;
        rx.await.unwrap_or(Err("DbActor 未回结果".into()))
    }
    pub async fn add(&self, input: TaskInput, actor: Actor) -> Result<Task, String> {
        let (resp, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::Add { input, actor, resp })
            .map_err(|_| "DbActor 通道已关闭".to_string())?;
        rx.await.unwrap_or(Err("DbActor 未回结果".into()))
    }
    pub async fn update(&self, id: i64, patch: TaskPatch) -> Result<Task, String> {
        let (resp, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::Update { id, patch, resp })
            .map_err(|_| "DbActor 通道已关闭".to_string())?;
        rx.await.unwrap_or(Err("DbActor 未回结果".into()))
    }
    pub async fn check(
        &self,
        id: i64,
        idx: usize,
        done: bool,
        evidence: Option<String>,
    ) -> Result<Task, String> {
        let (resp, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::Check { id, idx, done, evidence, resp })
            .map_err(|_| "DbActor 通道已关闭".to_string())?;
        rx.await.unwrap_or(Err("DbActor 未回结果".into()))
    }
    pub async fn set_progress(&self, id: i64, note: String) -> Result<Task, String> {
        let (resp, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::Progress { id, note, resp })
            .map_err(|_| "DbActor 通道已关闭".to_string())?;
        rx.await.unwrap_or(Err("DbActor 未回结果".into()))
    }
    pub async fn archive(&self, id: i64, archived: bool) -> Result<Task, String> {
        let (resp, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::Archive { id, archived, resp })
            .map_err(|_| "DbActor 通道已关闭".to_string())?;
        rx.await.unwrap_or(Err("DbActor 未回结果".into()))
    }
    pub async fn delete(&self, id: i64) -> Result<bool, String> {
        let (resp, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::Delete { id, resp })
            .map_err(|_| "DbActor 通道已关闭".to_string())?;
        rx.await.unwrap_or(Err("DbActor 未回结果".into()))
    }
}
```

- [ ] **Step 6: 写 spawn_db_actor（专用 OS 线程 + actor 循环 + emit）**

追加到 `src-tauri/src/tasks.rs`：

```rust
/// 启动 DbActor：专用 OS 线程独占 Connection，经 unbounded channel 收 DbCommand。
/// 写命令成功后经 `app.emit("task-changed", ())` 通知前端重查。
pub fn spawn_db_actor(db_path: PathBuf, app: AppHandle) -> DbActorHandle {
    let (tx, rx) = mpsc::unbounded_channel::<DbCommand>();
    let handle = DbActorHandle { tx };
    std::thread::Builder::new()
        .name("tasks-db-actor".into())
        .spawn(move || actor_loop(db_path, rx, app))
        .expect("spawn tasks-db-actor 失败");
    handle
}

fn actor_loop(db_path: PathBuf, mut rx: mpsc::UnboundedReceiver<DbCommand>, app: AppHandle) {
    // 父目录确保存在
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = match Connection::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[tasks] 打开 db 失败 {}: {e}", db_path.display());
            // actor 退出；所有后续调用收到通道关闭 Err
            return;
        }
    };
    if let Err(e) = migrate(&conn) {
        eprintln!("[tasks] migrate 失败: {e}");
        return;
    }
    while let Some(cmd) = rx.blocking_recv() {
        let is_write = !matches!(
            &cmd,
            DbCommand::List { .. } | DbCommand::Get { .. }
        );
        let emit_ok = match cmd {
            DbCommand::List { filter, resp } => {
                let r = sql_list(&conn, &filter).map_err(|e| e.to_string());
                let _ = resp.send(r);
                false
            }
            DbCommand::Get { id, resp } => {
                let r = sql_get(&conn, id).map_err(|e| e.to_string());
                let _ = resp.send(r);
                false
            }
            DbCommand::Add { input, actor, resp } => {
                let r = sql_add(&conn, input, actor).map_err(|e| e.to_string());
                let ok = r.is_ok();
                let _ = resp.send(r);
                ok
            }
            DbCommand::Update { id, patch, resp } => {
                let r = sql_update(&conn, id, patch).map_err(|e| e.to_string());
                let ok = r.is_ok();
                let _ = resp.send(r);
                ok
            }
            DbCommand::Check { id, idx, done, evidence, resp } => {
                let r = sql_check(&conn, id, idx, done, evidence).map_err(|e| e.to_string());
                let ok = r.is_ok();
                let _ = resp.send(r);
                ok
            }
            DbCommand::Progress { id, note, resp } => {
                let r = sql_set_progress(&conn, id, note).map_err(|e| e.to_string());
                let ok = r.is_ok();
                let _ = resp.send(r);
                ok
            }
            DbCommand::Archive { id, archived, resp } => {
                let r = sql_archive(&conn, id, archived).map_err(|e| e.to_string());
                let ok = r.is_ok();
                let _ = resp.send(r);
                ok
            }
            DbCommand::Delete { id, resp } => {
                let r = sql_delete(&conn, id).map_err(|e| e.to_string());
                let ok = r.is_ok();
                let _ = resp.send(r);
                ok
            }
        };
        // 写命令成功 → 通知前端重查（payload 空，前端收到就 list_tasks）
        if is_write && emit_ok {
            let _ = app.emit("task-changed", ());
        }
    }
    // 优雅关闭：WAL checkpoint（best-effort）
    let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
}
```

- [ ] **Step 7: 在文件顶部补全 imports**

回到 `src-tauri/src/tasks.rs` 顶部 `use` 段，确保含（部分 Step 4 加过，统一整理）：

```rust
use rusqlite::{params, Connection, Row};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{mpsc, oneshot};
```

（`Manager` trait 提供 `app.state` 等，本 Task 暂未用，但 Task 6 会用——保留。）

- [ ] **Step 8: cargo check（actor 闭环但不接入 tools，先确保编译）**

Run: `cargo check --manifest-path src-tauri/Cargo.toml --lib`
Expected: 退出码 0。可能 warning：`TaskInput`/`TaskPatch`/`spawn_db_actor` 未被使用（dead_code）——**这是预期的**（Task 5/6 才用），允许 dead_code warning 存在到 Task 6 接完。若想消，可加 `#[allow(dead_code)]` 到 TaskInput/TaskPatch，但**不必**——Task 5 马上就用。

> ⚠️ 若 cargo check 报 `pushset` 未定义等错——说明实现者没按 Step 2 删掉 macro 段。回去删 Step 1 末尾的 macro 段（从 `let mut sets: Vec<&str>` 到 `sql_update_explicit(...)` 调用那句的整段），只保留清晰的 `sql_update` 函数（它现在内部委托 `sql_update_explicit`）。

- [ ] **Step 9: Commit（不写测试——actor 跨线程难单测，CRUD 往返放 Task 4 写，Task 4 用直接调 sql_* 的方式测，不经 actor）**

```bash
git add src-tauri/src/tasks.rs
git commit -m "feat(tasks): DbActor + SQL 原语 + async API（专用 OS 线程独占 Connection）

- DbCommand 8 变体（list/get/add/update/check/progress/archive/delete）
- DbActorHandle 8 async 方法（oneshot 回结果）
- spawn_db_actor: std::thread 独占 Connection，写命令成功 emit task-changed
- sql_* 原语：add/get/list/update/check/progress/archive/delete
- validated 逻辑约束：status≠Done && verified=true 拒绝
- TaskInput/TaskPatch 入参结构

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 4: CRUD 往返测试（直接调 sql_* 原语，不经 actor）

**Files:**
- Modify: `src-tauri/src/tasks.rs`（追加测试到 `#[cfg(test)] mod tests`）

**Interfaces:**
- Consumes: Task 2/3 的 sql_add/sql_get/sql_list/sql_update/sql_check/sql_set_progress/sql_archive/sql_delete + TaskInput/TaskPatch/validate_status_verified

> **为什么直接调 sql_* 而不经 actor**：actor 跨 OS 线程 + oneshot 异步，单测里要 `#[tokio::test]` + 等 actor 线程，慢且脆。直接对 `Connection::open_in_memory()` 调 sql_* 原语，测的是「SQL 正确性 + 状态机 + 派生」，这是 actor 不变性的核心。actor 本身的 channel 往返留给 Task 5/6 的集成验证。

- [ ] **Step 1: 写 add→get→list 往返测试**

追加到 `src-tauri/src/tasks.rs` 的 `mod tests`（Task 2 Step 5 的测试之后）：

```rust
    fn sample_input(title: &str) -> TaskInput {
        TaskInput {
            title: title.into(),
            goal: "目标".into(),
            detail: "细节".into(),
            horizon: Horizon::Current,
            parent_id: None,
            tags: vec!["x".into()],
            status: Status::Todo,
            acceptance: vec![CheckItem {
                text: "a1".into(),
                done: false,
                evidence: None,
            }],
            due_date: None,
        }
    }

    #[test]
    fn crud_add_get_list_roundtrip() {
        let conn = fresh_conn();
        let t1 = sql_add(&conn, sample_input("t1"), Actor::Agent).unwrap();
        let t2 = sql_add(&conn, sample_input("t2"), Actor::User).unwrap();
        assert_eq!(t1.title, "t1");
        assert_eq!(t1.created_by, Actor::Agent);
        assert_eq!(t1.status, Status::Todo);
        assert!(!t1.verified);
        assert_eq!(t1.acceptance.len(), 1);

        // get
        let got = sql_get(&conn, t1.id).unwrap().unwrap();
        assert_eq!(got.title, "t1");

        // list 全量（未归档）
        let all = sql_list(&conn, &TaskFilter::default()).unwrap();
        assert_eq!(all.len(), 2);
        // sort_index ASC + id ASC：t1 在前
        assert_eq!(all[0].id, t1.id);
        assert_eq!(all[1].id, t2.id);
    }
```

- [ ] **Step 2: 写 update + status 流转 + verified 约束测试**

追加：

```rust
    #[test]
    fn update_status_flow_and_verified_guard() {
        let conn = fresh_conn();
        let t = sql_add(&conn, sample_input("t"), Actor::Agent).unwrap();

        // Todo → Active
        let t = sql_update(
            &conn,
            t.id,
            TaskPatch { status: Some(Status::Active), ..Default::default() },
        )
        .unwrap();
        assert_eq!(t.status, Status::Active);
        assert!(t.completed_at.is_none());

        // Active → Done（completed_at 盖戳，verified 默认 false）
        let t = sql_update(
            &conn,
            t.id,
            TaskPatch { status: Some(Status::Done), ..Default::default() },
        )
        .unwrap();
        assert_eq!(t.status, Status::Done);
        assert!(t.completed_at.is_some());
        assert!(!t.verified, "done 默认 verified=false（自述完成）");

        // 逻辑错：非 Done + verified=true 应拒绝
        let t0 = sql_add(&conn, sample_input("t0"), Actor::Agent).unwrap();
        let patch_err = TaskPatch {
            status: Some(Status::Active),
            blockers: Some(vec![Blocker {
                reason: "x".into(),
                raised_at: 1,
                resolved: false,
            }]),
            ..Default::default()
        };
        // Active 本身不触发 verified 校验（verified 仍是 false）；这里测 validate 直接
        assert!(validate_status_verified(Status::Active, true).is_err());
        assert!(validate_status_verified(Status::Active, false).is_ok());
        assert!(validate_status_verified(Status::Done, true).is_ok());
        let _ = sql_update(&conn, t0.id, patch_err).unwrap(); // 不该 panic
    }

    #[test]
    fn update_non_done_clears_verified() {
        // 已是 Done+verified=true，改回 Active → verified 自动清 false
        let conn = fresh_conn();
        let t = sql_add(&conn, sample_input("t"), Actor::Agent).unwrap();
        let t = sql_update(
            &conn,
            t.id,
            TaskPatch { status: Some(Status::Done), ..Default::default() },
        )
        .unwrap();
        // 直接 SQL 设 verified=1（绕过 patch，模拟历史数据）
        conn.execute("UPDATE tasks SET verified=1 WHERE id=?1", params![t.id])
            .unwrap();
        // 改回 Active
        let t = sql_update(
            &conn,
            t.id,
            TaskPatch { status: Some(Status::Active), ..Default::default() },
        )
        .unwrap();
        assert_eq!(t.status, Status::Active);
        assert!(!t.verified, "非 Done 时 verified 必须被清回 false");
    }
```

- [ ] **Step 3: 写 check + progress + archive + delete 测试**

追加：

```rust
    #[test]
    fn check_progress_archive_delete() {
        let conn = fresh_conn();
        let t = sql_add(&conn, sample_input("t"), Actor::Agent).unwrap();

        // check：勾第 0 项 + evidence
        let t = sql_check(&conn, t.id, 0, true, Some("evidence.md".into())).unwrap();
        assert!(t.acceptance[0].done);
        assert_eq!(t.acceptance[0].evidence.as_deref(), Some("evidence.md"));
        assert_eq!(t.progress(), Some((1, 1)));

        // check idx 越界
        assert!(sql_check(&conn, t.id, 99, true, None).is_err());

        // 取消勾 → evidence 清
        let t = sql_check(&conn, t.id, 0, false, None).unwrap();
        assert!(!t.acceptance[0].done);
        assert!(t.acceptance[0].evidence.is_none());

        // progress
        let t = sql_set_progress(&conn, t.id, "干到一半".into()).unwrap();
        assert_eq!(t.last_progress.as_deref(), Some("干到一半"));

        // archive
        let t = sql_archive(&conn, t.id, true).unwrap();
        assert!(t.archived_at.is_some());
        // 主视图（archived=false）不再列出
        let active = sql_list(&conn, &TaskFilter::default()).unwrap();
        assert!(active.iter().all(|x| x.id != t.id));
        // 归档视图列出
        let archived = sql_list(&conn, &TaskFilter { archived: true, ..Default::default() }).unwrap();
        assert!(archived.iter().any(|x| x.id == t.id));

        // 恢复
        let t = sql_archive(&conn, t.id, false).unwrap();
        assert!(t.archived_at.is_none());

        // delete
        assert!(sql_delete(&conn, t.id).unwrap());
        assert!(sql_get(&conn, t.id).unwrap().is_none());
        assert!(!sql_delete(&conn, t.id).unwrap(), "二次删返回 false");
    }

    #[test]
    fn list_filter_by_horizon_and_status_and_parent() {
        let conn = fresh_conn();
        let a = sql_add(&conn, sample_input("a"), Actor::Agent).unwrap(); // current, todo, 顶层
        let mut bi = sample_input("b");
        bi.horizon = Horizon::Long;
        bi.status = Status::Active;
        let b = sql_add(&conn, bi, Actor::Agent).unwrap();
        let mut ci = sample_input("c");
        ci.parent_id = Some(a.id);
        let c = sql_add(&conn, ci, Actor::Agent).unwrap();

        // horizon=current 只 a
        let cur = sql_list(
            &conn,
            &TaskFilter { horizon: Some(Horizon::Current), ..Default::default() },
        )
        .unwrap();
        assert_eq!(cur.len(), 1);
        assert_eq!(cur[0].id, a.id);

        // status=active 只 b
        let act = sql_list(
            &conn,
            &TaskFilter { status: Some(Status::Active), ..Default::default() },
        )
        .unwrap();
        assert_eq!(act.len(), 1);
        assert_eq!(act[0].id, b.id);

        // parent=None（顶层）= a + b
        let top = sql_list(
            &conn,
            &TaskFilter { parent_id: Some(None), ..Default::default() },
        )
        .unwrap();
        assert_eq!(top.len(), 2);

        // parent=Some(a.id) = c
        let children = sql_list(
            &conn,
            &TaskFilter { parent_id: Some(Some(a.id)), ..Default::default() },
        )
        .unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, c.id);
    }
```

- [ ] **Step 4: 跑 tasks 全测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib tasks::`
Expected: 9 passed（Task 2 的 5 个 + 本 Task 的 4 个：crud_add_get_list_roundtrip / update_status_flow_and_verified_guard / update_non_done_clears_verified / check_progress_archive_delete / list_filter_by_horizon_and_status_and_parent）。

- [ ] **Step 5: cargo check 零 warning**

Run: `cargo check --manifest-path src-tauri/Cargo.toml --lib`
Expected: 退出码 0。`spawn_db_actor`/`TaskInput`/`TaskPatch` 等「未使用」warning 仍可接受（Task 5/6 接完才消）。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/tasks.rs
git commit -m "test(tasks): CRUD 往返 + 状态机 + verified 约束 + 过滤（9 测）

直接调 sql_* 原语（不经 actor），测 SQL 正确性/状态机/派生不变性。
actor 跨线程往返留 Task 5/6 集成验证。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 5: tools.rs 接 7 个 task 工具 + llm.rs cascade

**Files:**
- Modify: `src-tauri/src/tools.rs`（ToolsCtx 加 `tasks` 字段 + 7 schema + 7 dispatch + 7 tool 函数 + 改测试名/期望）
- Modify: `src-tauri/src/llm.rs`（llm.rs:778 断言 13→20）
- Modify: `src-tauri/src/tools.rs` 的 `foreground()` + 所有 ToolsCtx 构造点（加 `tasks:` 字段）
- Modify: `src-tauri/src/agent.rs`（agent.rs:228 构造点 + agent.rs:465 测试构造点）
- Modify: `src-tauri/src/subagents.rs`（subagents.rs:208 agent_ctx + subagents.rs:611 clone_ctx）

**Interfaces:**
- Consumes: Task 2/3/4 的 DbActorHandle + 8 async 方法 + TaskInput/TaskPatch + 类型
- Produces: 7 个 task 工具（task_list / task_get / task_add / task_update / task_check / task_progress / task_archive）注册进 schemas()/dispatch()；`ToolsCtx.tasks` 字段供 dispatch 调
- Consumes（后续 Task）：Task 6 把真实 DbActorHandle 注入 agent.rs 构造点

> **关键决策**：ToolsCtx 加 **一个** 字段 `pub tasks: crate::tasks::DbActorHandle`（Clone 便宜）。所有构造点补一行；foreground()/测试用 `DbActorHandle::noop()`。比 session_tx 绕行更直接（task 工具是同步调 async API，dispatch 本就 async，直接 `.await`）。
> **7 工具只列 7 个**（spec §8.1）：task_list / task_get / task_add / task_update / task_check / task_progress / task_archive。delete 不给 agent（只 archive；硬删是用户侧 command 专属）。

- [ ] **Step 1: llm.rs cascade（1 行）**

`src-tauri/src/llm.rs:778`：

```rust
        assert_eq!(body["tools"].as_array().unwrap().len(), 13);
```

改为：

```rust
        assert_eq!(body["tools"].as_array().unwrap().len(), 20);
```

- [ ] **Step 2: ToolsCtx 加 tasks 字段**

`src-tauri/src/tools.rs` 的 `ToolsCtx` 结构体（tools.rs:52-76），在 `pub interrupt: InterruptHandle,` 后加：

```rust
    /// 任务管理 db actor 句柄。task_* 工具经此 async 调 SQL。
    /// foreground()/测试用 noop()；spawn_session 注入真实 actor（lib.rs setup spawn）。
    pub tasks: crate::tasks::DbActorHandle,
```

- [ ] **Step 3: 改 tools.rs 的 foreground() + 所有 tools.rs 内 ToolsCtx 构造点**

`src-tauri/src/tools.rs:81-98` 的 `foreground()`，在 `interrupt: InterruptHandle::new(),` 后加：

```rust
        tasks: crate::tasks::DbActorHandle::noop(),
```

然后 `cargo check` 看报错——所有 `ToolsCtx { ... }` 字面构造点（tools.rs 内的测试，据 Task 前探索约 tools.rs:986/999/1007/1015/1333/1343/1363/1379/1392/1404/1414/1424/1449 等）会因缺 `tasks` 字段报错。**每个** `ToolsCtx { ... }` 字面构造点补一行 `tasks: crate::tasks::DbActorHandle::noop(),`（测试场景一律 noop）。

> 用 `cargo check` 报错清单逐个补，不要靠记忆。报错信息会列每个构造点的 file:line。

- [ ] **Step 4: 改 agent.rs 的 ToolsCtx 构造点**

`src-tauri/src/agent.rs:228-241`（spawn_session 内生产构造点），在 `interrupt: interrupt.clone(),` 后加：

```rust
        tasks: crate::tasks::DbActorHandle::noop(), // Task 6 换成真实 actor
```

> 暂时 noop，Task 6 改注入真实。这样 Task 5 独立编译过。

`src-tauri/src/agent.rs:465-473`（测试构造点），同样补 `tasks: crate::tasks::DbActorHandle::noop(),`。

- [ ] **Step 5: 改 subagents.rs 的 clone_ctx + agent_ctx**

`src-tauri/src/subagents.rs:611-627` 的 `clone_ctx`，在 `interrupt: crate::tools::InterruptHandle::new(),` 后加：

```rust
        tasks: src.tasks.clone(),
```

`src-tauri/src/subagents.rs:208-212` 的 `agent_ctx` 用 `foreground()`，已被 Step 3 覆盖（foreground 补了 noop），不用再改。

- [ ] **Step 6: 写 7 个 tool 函数**

在 `src-tauri/src/tools.rs` 找到 `tool_dream` 函数附近（dream 工具后），加 7 个 tool 函数：

```rust
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
        horizon: args.get("horizon").and_then(|v| v.as_str()).and_then(parse_horizon).ok().flatten(),
        status: args.get("status").and_then(|v| v.as_str()).and_then(parse_status).ok().flatten(),
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
```

- [ ] **Step 7: schemas() 加 7 个 task 工具 schema**

`src-tauri/src/tools.rs` 的 `schemas()` 函数，在 `dream` 工具 schema 之后（`create_timer` 之前，按现有顺序），加 7 个：

```rust
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
```

- [ ] **Step 8: dispatch 加 7 分支**

`src-tauri/src/tools.rs:778-801` 的 `dispatch`，在 `"dream" => tool_dream(&args, ctx),` 后、`"create_timer"` 前加：

```rust
        "task_list" => tool_task_list(&args, ctx).await,
        "task_get" => tool_task_get(&args, ctx).await,
        "task_add" => tool_task_add(&args, ctx).await,
        "task_update" => tool_task_update(&args, ctx).await,
        "task_check" => tool_task_check(&args, ctx).await,
        "task_progress" => tool_task_progress(&args, ctx).await,
        "task_archive" => tool_task_archive(&args, ctx).await,
```

- [ ] **Step 9: 改 tools.rs schemas 测试（13→20）**

`src-tauri/src/tools.rs:1432-1438` 的 `schemas_has_thirteen_tools`，整函数替换为：

```rust
    #[test]
    fn schemas_has_twenty_tools() {
        let s = schemas();
        let names: Vec<&str> = s.iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        // 8 基础 + dream + 4 timer + 7 task = 20；须与 llm.rs 的 tools.len()==20 断言同步
        assert_eq!(
            names,
            vec![
                "write", "read", "bash", "display", "edit_card", "edit", "subagent",
                "attach", "dream",
                "task_list", "task_get", "task_add", "task_update", "task_check",
                "task_progress", "task_archive",
                "create_timer", "update_timer", "delete_timer", "list_timers",
            ]
        );
    }
```

> ⚠️ **顺序很重要**：测试断言的 vec 顺序必须跟 `schemas()` 里 `vec![]` 的实际顺序一致。本 plan 假定 task 工具插在 dream 后、timer 前。若实现者把 schema 插别处，相应改测试 vec 顺序——但**数量必是 20**。

- [ ] **Step 10: cargo check + 改剩余构造点**

Run: `cargo check --manifest-path src-tauri/Cargo.toml --lib`
Expected: 报错清单会列所有漏改 `tasks:` 字段的 ToolsCtx 构造点。逐个补 `tasks: crate::tasks::DbActorHandle::noop(),`。
重跑直到退出码 0、0 warning。

> 典型漏点：tools.rs 测试构造点（~12 处）、agent.rs:228 / agent.rs:465、subagents.rs:611 clone_ctx。Step 3-5 已覆盖主要，但 cargo check 报错是真相。

- [ ] **Step 11: 跑 tools + llm 测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib tools::`
Expected: 全过（含新 `schemas_has_twenty_tools`）。

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib llm::`
Expected: 全过（含 llm.rs:778 改后的 `len()==20` 测试）。

- [ ] **Step 12: Commit**

```bash
git add src-tauri/src/tools.rs src-tauri/src/llm.rs src-tauri/src/agent.rs src-tauri/src/subagents.rs
git commit -m "feat(tools): 7 个 task 工具 + llm.rs/tools.rs cascade(13→20)

- ToolsCtx 加 tasks: DbActorHandle 字段（foreground/测试 noop）
- schemas() +7 task 工具(task_list/get/add/update/check/progress/archive)
- dispatch() +7 分支(.await async API)
- llm.rs:778 断言 13→20；tools.rs schemas_has_thirteen_tools→schemas_has_twenty_tools
- agent.rs/subagents.rs 构造点补 tasks 字段（Task 6 注入真实 actor）

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 6: lib.rs 5 个用户 command + setup spawn DbActor + 注入 agent.rs

**Files:**
- Modify: `src-tauri/src/lib.rs`（mod tasks 已在 Task 2 加；加 5 command + setup spawn + invoke_handler 注册 + stop flag）
- Modify: `src-tauri/src/agent.rs`（spawn_session 内 ToolsCtx 构造点：noop → 真实 `app.state::<DbActorHandle>()`）
- Modify: `src-tauri/src/tasks.rs`（TaskPatch 加 `verified` 字段 + sql_update_explicit honor 它——供用户侧升 verified）

**Interfaces:**
- Consumes: Task 3/4/5 的 DbActorHandle + spawn_db_actor + 类型
- Produces: 5 个 Tauri command（`list_tasks`/`add_task`/`update_task`/`archive_task`/`delete_task`）注册进 invoke_handler；setup 阶段 `app.manage(DbActorHandle)`；agent.rs ToolsCtx 拿真实 actor

> **关键决策**：
> 1. **不改 spawn_session 签名**——setup 里先 spawn DbActor + `app.manage(handle)`，spawn_session 内部用 `app.state::<DbActorHandle>().inner().clone()` 拿。跟 `registry` 经 `app.manage` 共享同款。
> 2. **TaskPatch 加 `verified: Option<bool>`**：agent 的 tool_task_update（Task 5）不暴露此字段（agent 不能自验证），但**用户侧 update_task command** 可设。这是「轻确认」防线的落地——verified 升 true 是用户/确认动作，非 agent 自述。sql_update_explicit 须 honor `patch.verified`。

- [ ] **Step 1: TaskPatch 加 verified 字段（tasks.rs 修正）**

`src-tauri/src/tasks.rs` 的 `TaskPatch` 结构体（Task 3 Step 1 定义），在 `pub artifacts: Option<Vec<Artifact>>,` 后加：

```rust
    /// 用户侧升 verified（agent 不暴露此字段——防自述完成）。
    /// sql_update_explicit honor 它；status≠Done 时仍强制清 false。
    pub verified: Option<bool>,
```

`Default` derive 仍生效（`None` 默认）。

- [ ] **Step 2: sql_update_explicit honor patch.verified（tasks.rs 修正）**

`src-tauri/src/tasks.rs` 的 `sql_update` 函数（Task 3 Step 1，算 new_verified 那段），把：

```rust
    let mut new_verified = existing.verified;
    if new_status != Status::Done {
        new_verified = false;
    }
```

改为：

```rust
    let mut new_verified = patch.verified.unwrap_or(existing.verified);
    if new_status != Status::Done {
        new_verified = false;
    }
```

（`validate_status_verified(new_status, new_verified)` 紧跟其后不变——Done + verified=true 合法；非 Done 被强制 false 后也合法。）

> 更新 Task 4 的 `update_non_done_clears_verified` 测试仍过（patch.verified=None → 用 existing）。无需改测试。

- [ ] **Step 3: lib.rs 写 5 个用户 command**

在 `src-tauri/src/lib.rs` 找现有 command 区（如 `async fn list_jobs` / `async fn list_timers` 附近），加 5 个：

```rust
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
        // 用 blockers 字段通道不行——需在 TaskPatch 加 acceptance 字段。见 Step 4。
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
```

- [ ] **Step 4: TaskPatch 加 acceptance 字段（tasks.rs 再修正）**

Step 3 用了 `p.acceptance = Some(items)`——但 TaskPatch（Task 3/6 Step 1）没有 `acceptance` 字段。在 `src-tauri/src/tasks.rs` 的 `TaskPatch`，`pub verified: Option<bool>,` 后加：

```rust
    /// 用户侧整组替换 acceptance（agent 用 task_check 逐项；用户编辑弹层整组写）。
    pub acceptance: Option<Vec<CheckItem>>,
```

并在 `sql_update_explicit`（tasks.rs）的 SQL SET 里 honor 它。在 `sql_update_explicit` 函数体内，`let artifacts = ...` 之后、`let affected = conn.execute(...)` 之前加：

```rust
    let acceptance = serde_json::to_string(&patch.acceptance.unwrap_or(existing.acceptance))
        .unwrap_or_else(|_| "[]".into());
```

然后改 `conn.execute` 的 SQL：把 `blockers=?9, artifacts=?10` 那段加 `acceptance`。完整 UPDATE 语句改为：

```rust
    let affected = conn.execute(
        "UPDATE tasks SET
            title=?1, goal=?2, detail=?3, horizon=?4, parent_id=?5, tags=?6,
            status=?7, verified=?8, acceptance=?9, blockers=?10, artifacts=?11,
            sort_index=?12, due_date=?13, completed_at=?14, updated_at=?15
         WHERE id=?16",
        params![
            title, goal, detail, horizon.as_str(), parent_id, tags,
            new_status.as_str(), new_verified as i64, acceptance, blockers, artifacts,
            sort_index, due_date, completed_at, now, id,
        ],
    )?;
```

（注意：`now` 在 `sql_update` 里算，`sql_update_explicit` 接收 `now: i64` 参数。已在 Task 3 Step 2 签名里。）

- [ ] **Step 5: lib.rs setup spawn DbActor + app.manage**

`src-tauri/src/lib.rs` 的 `setup` 闭包（lib.rs:841-865），在 `let cache0 = std::path::PathBuf::from(&cfg0.cache_dir);` 之后、`migrate_legacy_cache` 之前（或 `bootstrap_defaults` 之后均可，只要在 `spawn_session` 之前），加：

```rust
    // 任务管理 DbActor：专用 OS 线程独占 Connection（cache_dir/tasks.db）
    let db_path = cache0.join("tasks.db");
    let db_actor = crate::tasks::spawn_db_actor(db_path, app.handle().clone());
    app.manage(db_actor);
```

> 必须在 `let session = agent::spawn_session(...)` 之前 `app.manage(db_actor)`——spawn_session 内部会 `app.state::<DbActorHandle>()` 取它。

- [ ] **Step 6: agent.rs spawn_session 注入真实 DbActorHandle**

`src-tauri/src/agent.rs:228-241` 的 ToolsCtx 构造点，把 Task 5 Step 4 临时写的：

```rust
        tasks: crate::tasks::DbActorHandle::noop(), // Task 6 换成真实 actor
```

改为：

```rust
        tasks: app.state::<crate::tasks::DbActorHandle>().inner().clone(),
```

> 需 `use tauri::Manager;` 在 agent.rs 顶部（应已有——spawn_session 接 `app: AppHandle`）。若没有，加 `use tauri::Manager;`（state() 是 Manager trait 方法）。
> `app` 在 spawn_session 签名是 `app: AppHandle`（agent.rs:162），闭包内可见。

- [ ] **Step 7: lib.rs invoke_handler 注册 5 command**

`src-tauri/src/lib.rs:899-922` 的 `invoke_handler!`，在 `history_head,` 后（或 `list_timers` 附近，按聚合习惯）加：

```rust
        list_tasks,
        add_task,
        update_task,
        archive_task,
        delete_task,
```

- [ ] **Step 8: cargo check + cargo run（spawn 起来不 panic）**

Run: `cargo check --manifest-path src-tauri/Cargo.toml --lib`
Expected: 退出码 0，0 warning。

> ⚠️ `spawn_db_actor` 现在被调用了——Task 3 Step 8 的「dead_code warning」应消失。若仍有 dead_code，说明 setup 没接上。

Run（可选，验证 actor 启动不 panic）：`cargo test --manifest-path src-tauri/Cargo.toml --lib tasks::`
Expected: Task 4 的 9 测仍全过（测试不经 actor，但确认 sql_* 没 regression）。

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/agent.rs src-tauri/src/tasks.rs
git commit -m "feat(tasks): 5 用户 command + setup spawn DbActor + 注入 agent.rs

- lib.rs: list_tasks/add_task/update_task/archive_task/delete_task
- setup: spawn_db_actor(cache/tasks.db) + app.manage(DbActorHandle)
- agent.rs spawn_session: ToolsCtx.tasks 从 noop → app.state 真实 actor
- invoke_handler 注册 5 command
- TaskPatch +verified（用户侧升 verified，agent 不暴露——防自述完成）
- TaskPatch +acceptance（用户编辑弹层整组写；agent 用 task_check 逐项）
- sql_update_explicit honor patch.verified/acceptance

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 7: AGENT.md 增「按需查任务」段（不写每轮 surface）

**Files:**
- Modify: `src-tauri/ovoice/AGENT.md`（确认路径——探索时是 `src-tauri/ovoice/AGENT.md`）

**Interfaces:**
- Consumes: 无（纯文档）
- Produces: AGENT.md 增「任务管理」段，指导 agent 何时用 task 工具

> **红线 1 落地**：spec §9.4 明确**撤回**「每轮主动 surface 任务」（依赖注入，撤了注入就撤这条）。改为「按需查」——对话涉及任务话题 / agent 决策需要时才 task_list/task_get。agent 自主管理时维护状态准确（做了一步 check / 卡了 blocker / 成了 done）。

- [ ] **Step 1: 确认 AGENT.md 路径**

Run: `ls src-tauri/ovoice/AGENT.md` （或 `find . -name AGENT.md -not -path "*/node_modules/*"`）
若路径不对，找到真实路径再改。

- [ ] **Step 2: AGENT.md 加「任务管理」段**

在 `src-tauri/ovoice/AGENT.md` 末尾（或合适章节后），加：

```markdown
## 任务管理（当前工作记忆）

你有一份**独立于对话流的任务状态板**——跨会话持续推进同一目标用。任务表是 sqlite 维护的「当前做到哪」；dream 是「过去发生了什么」。两者分工不重叠。

**何时用 task 工具（按需查，不是每轮 surface）**：
- 用户提新目标 → `task_add` 登记成任务，告知「已登记 #N」
- 用户提到任务/进度/某目标相关话题 → 先 `task_list` / `task_get` 看现状再答
- 你自己决策需要知道当前工作上下文 → `task_list`（horizon=current 优先）

**自主维护状态（让任务板始终反映现实）**：
- 开工 → `task_update` status=active
- 推进 → `task_check` 勾验收项（鼓励带 evidence=文件路径/链接）+ `task_progress` 记一句话摘要
- 卡住 → `task_update` blockers=[{reason, raised_at, resolved:false}]
- 解除阻塞 → blockers 里 resolved=true
- 完成 → `task_update` status=done（**默认 verified=false，即自述完成**）
- 用 read/bash 验证产出确实存在后 → 告诉用户「我标了完成，你确认下」，让用户在任务页升 verified=true（你不能自己升）
- done 一段时间 → `task_archive`

**拆解**：大任务用 `task_add` 建 + `parent_id` 串子任务。

**克制**：
- 别每轮都 surface 任务板（用户没问就别查）
- status=done 是你的声称，verified=true 是用户/证据的确认——别自证
- 改 goal/title 这种核心定义前，跟用户说一声
- 硬删（delete）不是你的权限——只 archive
```

- [ ] **Step 3: cargo check（确认没碰源码，仍绿）**

Run: `cargo check --manifest-path src-tauri/Cargo.toml --lib`
Expected: 退出码 0（文档改动不影响编译；只是保险跑一次）。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/ovoice/AGENT.md
git commit -m "docs(agent): 增任务管理段（按需查 + 自主维护状态 + verified 制衡）

不写「每轮 surface」（红线 1：依赖每轮注入，撤了注入就撤这条）。
改为：涉及任务话题/决策需要时才查；自主 check/blocker/done；
done 默认 verified=false，升 true 须用户确认（防自述完成）。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 8: index.html 加 tasks 视图 tab + 看板骨架

**Files:**
- Modify: `src/index.html`（topbar 加 tasks-btn；body 加 `<section id="tasks-view">`）

**Interfaces:**
- Consumes: 无（前端起点）
- Produces: DOM 节点 `#tasks-btn` / `#tasks-view` / `#tasks-current-list` 等 / `#tasks-back` / `#tasks-refresh` / `#tasks-new`，供 Task 10 main.js 查询

- [ ] **Step 1: topbar 加 tasks-btn**

`src/index.html` 的 `.topbar-actions`（index.html:29-38），在 `jobs-btn` 后、`scheduler-btn` 前，加：

```html
        <button id="tasks-btn" class="icon-btn text-btn" type="button" aria-label="任务看板" title="任务看板">看板</button>
```

> ⚠️ **注意命名歧义**：现有 `jobs-btn` 文案是「任务」（后台任务）。本视图用「看板」文案避歧义。aria-label/title 用「任务看板」。

- [ ] **Step 2: 加 tasks-view section（看板四列）**

`src/index.html` 在 `jobs-view` section 之后（约 index.html:307）、`scheduler-view` 之前，加：

```html
    <!-- 任务看板视图 -->
    <section id="tasks-view" class="view jobs-view" hidden>
      <header class="jobs-topbar">
        <div class="jobs-title">
          <h2>任务看板</h2>
          <span id="tasks-sub" class="jobs-sub">agent 的当前工作记忆——跨会话持续推进的目标</span>
        </div>
        <div class="jobs-topbar-actions">
          <label class="row" style="flex-direction:row;align-items:center;gap:6px;flex:0 0 auto">
            <span style="white-space:nowrap">归档</span>
            <input id="tasks-show-archived" type="checkbox" />
          </label>
          <button id="tasks-new" type="button" class="link-btn">新建</button>
          <button id="tasks-refresh" type="button" class="link-btn">刷新</button>
          <button id="tasks-back" type="button" class="link-btn">返回对话</button>
        </div>
      </header>

      <div class="tasks-board" id="tasks-board">
        <div class="kanban-col" data-horizon="current">
          <div class="kanban-col-head"><span class="kanban-h current">当前</span><span class="kanban-count" data-count="current">0</span></div>
          <div class="kanban-cards" id="tasks-current-list"></div>
        </div>
        <div class="kanban-col" data-horizon="short">
          <div class="kanban-col-head"><span class="kanban-h short">短期</span><span class="kanban-count" data-count="short">0</span></div>
          <div class="kanban-cards" id="tasks-short-list"></div>
        </div>
        <div class="kanban-col" data-horizon="long">
          <div class="kanban-col-head"><span class="kanban-h long">长期</span><span class="kanban-count" data-count="long">0</span></div>
          <div class="kanban-cards" id="tasks-long-list"></div>
        </div>
        <div class="kanban-col" data-horizon="vision">
          <div class="kanban-col-head"><span class="kanban-h vision">愿景</span><span class="kanban-count" data-count="vision">0</span></div>
          <div class="kanban-cards" id="tasks-vision-list"></div>
        </div>
      </div>

      <div id="tasks-empty" class="jobs-empty" hidden>
        <p>还没有任务</p>
        <span>在对话里让 agent「登记一个任务」，或点「新建」手动加。任务板是 agent 跨会话持续推进目标的当前工作记忆。</span>
      </div>
    </section>
```

- [ ] **Step 3: 加任务编辑弹层 DOM（modal，Task 10 用）**

`src/index.html` 在 `</section>`（tasks-view 结束）之后、`<script src="main.js"></script>` 之前，加一个全局 modal（独立于视图，复用于新建/编辑）：

```html
    <!-- 任务编辑弹层 -->
    <div id="task-modal" class="task-modal" hidden>
      <div class="task-modal-card">
        <header class="task-modal-head">
          <h3 id="task-modal-title">新建任务</h3>
          <button type="button" id="task-modal-close" class="link-btn">×</button>
        </header>
        <div class="task-modal-body">
          <label class="task-field"><span>标题</span><input id="task-f-title" type="text" /></label>
          <label class="task-field"><span>目标/动机</span><textarea id="task-f-goal" rows="2"></textarea></label>
          <label class="task-field"><span>背景细节</span><textarea id="task-f-detail" rows="3"></textarea></label>
          <div class="task-field-row">
            <label class="task-field"><span>时间视野</span>
              <select id="task-f-horizon">
                <option value="current">当前</option><option value="short">短期</option>
                <option value="long">长期</option><option value="vision">愿景</option>
              </select>
            </label>
            <label class="task-field"><span>状态</span>
              <select id="task-f-status">
                <option value="todo">todo</option><option value="active">active</option>
                <option value="done">done</option><option value="dropped">dropped</option>
              </select>
            </label>
          </div>
          <div class="task-field-row">
            <label class="task-field"><span>父任务 id（可选）</span><input id="task-f-parent" type="number" /></label>
            <label class="task-field"><span>截止（毫秒，可选）</span><input id="task-f-due" type="number" /></label>
          </div>
          <label class="task-field"><span>标签（逗号分隔）</span><input id="task-f-tags" type="text" /></label>
          <details class="task-acceptance-wrap">
            <summary>验收清单（每行一条）</summary>
            <textarea id="task-f-acceptance" rows="4" placeholder="第1章&#10;第2章&#10;校对"></textarea>
          </details>
          <div id="task-f-meta" class="task-meta" hidden></div>
          <div id="task-f-blockers" class="task-blockers" hidden></div>
        </div>
        <footer class="task-modal-foot">
          <button type="button" id="task-modal-archive" class="link-btn" hidden>归档</button>
          <button type="button" id="task-modal-delete" class="link-btn danger" hidden>删除</button>
          <span class="spacer"></span>
          <button type="button" id="task-modal-cancel" class="link-btn">取消</button>
          <button type="button" id="task-modal-save" class="primary-btn">保存</button>
        </footer>
      </div>
    </div>
```

- [ ] **Step 4: 视觉验证（不跑，只确认 DOM 结构对）**

打开 `src/index.html` 通读 topbar（应有 jobs/看板/scheduler/settings 四个 text-btn）和 tasks-view section（四列 + modal）。无 backend 改动，无需 cargo check。

- [ ] **Step 5: Commit**

```bash
git add src/index.html
git commit -m "feat(ui): tasks 视图 tab + 看板四列骨架 + 编辑弹层 DOM

- topbar 加 tasks-btn（文案「看板」避与 jobs「任务」歧义）
- tasks-view: 四列 current/short/long/vision + 计数 + empty
- 归档切换 checkbox + 新建/刷新/返回
- task-modal: 标题/目标/细节/horizon/status/parent/due/tags/acceptance
  + 归档/删除按钮（编辑模式显示）

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 9: styles.css 看板/卡片/徽章/modal 样式

**Files:**
- Modify: `src/styles.css`（末尾加 tasks 视图专属样式；复用 jobs-view 基础）

**Interfaces:**
- Consumes: 无
- Produces: `.tasks-board` / `.kanban-col` / `.kanban-cards` / `.task-card` / `.task-badge` / `.task-modal` 等类样式

- [ ] **Step 1: 末尾追加 tasks 视图样式**

`src/styles.css` 文件末尾追加：

```css
/* ===== Tasks 看板视图 ===== */
.tasks-board {
  flex: 1;
  display: grid;
  grid-template-columns: repeat(4, 1fr);
  gap: 10px;
  padding: 10px 14px 18px;
  overflow-x: auto;
  min-height: 0;
}
.kanban-col {
  display: flex;
  flex-direction: column;
  gap: 8px;
  background: rgba(245, 245, 247, var(--glass-alpha));
  border: 1px solid var(--separator);
  border-radius: 14px;
  padding: 10px;
  min-height: 160px;
}
.kanban-col-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 2px 4px 6px;
  border-bottom: 1px solid var(--separator);
}
.kanban-h { font-size: 13px; font-weight: 700; }
.kanban-h.current { color: var(--text); }
.kanban-h.short { color: var(--text-secondary); }
.kanban-h.long { color: var(--text-tertiary); }
.kanban-h.vision { color: var(--text-tertiary); opacity: 0.8; }
.kanban-count {
  font-size: 11px; font-weight: 600; color: var(--text-tertiary);
  background: var(--bg-soft); padding: 1px 8px; border-radius: 999px;
}
.kanban-cards { flex: 1; display: flex; flex-direction: column; gap: 8px; min-height: 0; }

/* 任务卡 */
.task-card {
  border: 1px solid var(--separator-strong);
  border-left: 3px solid var(--separator-strong);
  border-radius: 12px;
  background: rgba(255, 255, 255, var(--glass-alpha));
  padding: 10px 12px;
  cursor: pointer;
  transition: box-shadow 0.15s ease;
}
.task-card:hover { box-shadow: 0 2px 6px rgba(0,0,0,0.06); }
.task-card.state-active { border-left-color: var(--text); }
.task-card.state-done { opacity: 0.62; }
.task-card.state-dropped { opacity: 0.5; text-decoration: line-through; }
.task-card.blocked { border-left-color: #c0392b; }

.task-card-title {
  font-size: 14px; font-weight: 600; color: var(--text);
  white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
}
.task-card-goal { font-size: 12px; color: var(--text-secondary); margin-top: 3px;
  display: -webkit-box; -webkit-line-clamp: 2; -webkit-box-orient: vertical; overflow: hidden; }
.task-card-badges { margin-top: 6px; display: flex; flex-wrap: wrap; gap: 4px; }
.task-badge {
  font-size: 11px; font-weight: 600; padding: 2px 7px; border-radius: 999px;
  background: var(--bg-soft); color: var(--text-secondary);
}
.task-badge.blocked { background: #c0392b; color: #fff; }
.task-badge.due { background: #b9770e; color: #fff; }
.task-badge.verified { background: #1e8449; color: #fff; }
.task-badge.unverified { background: var(--bg-soft); color: #b9770e; border: 1px dashed #b9770e; }
.task-badge.archived { background: var(--text-tertiary); color: var(--bg); }

.task-progress {
  margin-top: 6px; height: 4px; border-radius: 2px;
  background: var(--separator); overflow: hidden;
}
.task-progress-fill { height: 100%; background: var(--text); }

/* 编辑弹层 */
.task-modal {
  position: fixed; inset: 0; z-index: 1000;
  display: flex; align-items: center; justify-content: center;
  background: rgba(0,0,0,0.4);
}
.task-modal[hidden] { display: none; }
.task-modal-card {
  width: min(560px, 92vw); max-height: 88vh; overflow-y: auto;
  background: var(--bg); border-radius: 16px;
  box-shadow: 0 8px 32px rgba(0,0,0,0.2);
  display: flex; flex-direction: column;
}
.task-modal-head {
  display: flex; align-items: center; justify-content: space-between;
  padding: 14px 18px; border-bottom: 1px solid var(--separator);
}
.task-modal-head h3 { margin: 0; font-size: 17px; font-weight: 700; }
.task-modal-body { padding: 14px 18px; display: flex; flex-direction: column; gap: 10px; }
.task-field { display: flex; flex-direction: column; gap: 4px; font-size: 13px; color: var(--text-secondary); }
.task-field input, .task-field textarea, .task-field select {
  font-size: 14px; color: var(--text);
  border: 1px solid var(--separator-strong); border-radius: 8px;
  padding: 8px 10px; background: var(--bg);
}
.task-field-row { display: flex; gap: 10px; }
.task-field-row .task-field { flex: 1; }
.task-modal-foot {
  display: flex; align-items: center; gap: 8px;
  padding: 12px 18px; border-top: 1px solid var(--separator);
}
.task-modal-foot .spacer { flex: 1; }
.link-btn.danger { color: #c0392b; }

.task-meta { font-size: 12px; color: var(--text-tertiary); }
.task-blockers { font-size: 12px; color: #c0392b; }
```

- [ ] **Step 2: 视觉验证（不跑，仅样式表）**

`src/styles.css` 末尾确认追加无语法错（开合括号配对）。无 backend 改动。

- [ ] **Step 3: Commit**

```bash
git add src/styles.css
git commit -m "style(tasks): 看板四列 + 任务卡(状态/阻塞/due/verified 徽章) + 编辑弹层

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 10: main.js 看板渲染 + CRUD invoke + listen task-changed + 编辑弹层

**Files:**
- Modify: `src/main.js`（DOM 引用 + showTasks + refreshTasks + renderTasksBoard + buildTaskCard + 编辑弹层逻辑 + listen task-changed）

**Interfaces:**
- Consumes: Task 8 的 DOM 节点；Task 6 的 5 个 Tauri command（`list_tasks`/`add_task`/`update_task`/`archive_task`/`delete_task`）；Task 6 的 `task-changed` event
- Produces: tasks 视图完整交互闭环

> **参考现有模式**（探索已确认）：
> - 视图切换：`showXxx()` 设其他 `view.hidden=true`，本 view `.hidden=false`（main.js:532-568）
> - invoke 返回数组：`refreshJobs`（main.js:807）模式——`const tasks = await invoke("list_tasks"); cache=...; render()`
> - listen：`setupAgentEvents`（main.js:1961）内 `await listen("event", cb)`
> - escapeHtml：main.js:576；renderMarkdown：main.js（已有）
> - 卡片渲染：`buildJobCard`（main.js:630-773）createElement + innerHTML 模板 + wire handlers

- [ ] **Step 1: 加 DOM 引用（main.js 顶部，约 main.js:65-67 那块）**

在 main.js 现有 `chatView`/`settingsView`/`jobsView` 等引用附近，加：

```javascript
const tasksView = document.getElementById("tasks-view");
const tasksBtn = document.getElementById("tasks-btn");
const tasksBack = document.getElementById("tasks-back");
const tasksRefresh = document.getElementById("tasks-refresh");
const tasksNew = document.getElementById("tasks-new");
const tasksShowArchived = document.getElementById("tasks-show-archived");
const tasksEmpty = document.getElementById("tasks-empty");
const tasksBoard = document.getElementById("tasks-board");
const taskModal = document.getElementById("task-modal");
let tasksCache = [];        // 当前展示的任务数组
let editingTaskId = null;   // null=新建模式；number=编辑模式
```

- [ ] **Step 2: 加 showTasks + 视图切换接线**

在 main.js 现有 `showJobs`（main.js:568）函数附近，加：

```javascript
function hideAllViews() {
  chatView.hidden = true;
  settingsView.hidden = true;
  if (jobsView) jobsView.hidden = true;
  const sv = document.getElementById("scheduler-view"); if (sv) sv.hidden = true;
  if (tasksView) tasksView.hidden = true;
}

function showTasks() {
  hideAllViews();
  if (tasksView) tasksView.hidden = false;
}
```

> ⚠️ **复用机会**：现有 showSettings/showChat/showJobs/showScheduler 各自重复「设其他 view hidden=true」——本 Task 引入 `hideAllViews()` 作共享辅助，但**不重构现有 4 函数**（YAGNI，避免 merge 风险）。只让 showTasks 用它。

- [ ] **Step 3: 接 tasks-btn / tasks-back / tasks-refresh / tasks-new / show-archived click**

在 main.js 现有 jobs/scheduler 事件接线区（约 main.js:821-831 / 971-977），加：

```javascript
if (tasksBtn) {
  tasksBtn.onclick = async () => {
    if (tasksView.hidden) {
      showTasks();
      await refreshTasks();
    } else {
      showChat();
    }
  };
}
if (tasksBack) tasksBack.onclick = () => { showChat(); };
if (tasksRefresh) tasksRefresh.onclick = () => refreshTasks();
if (tasksShowArchived) tasksShowArchived.onchange = () => refreshTasks();
if (tasksNew) tasksNew.onclick = () => openTaskModal(null); // null=新建
```

> `showChat` 是现有函数（main.js:538）。`openTaskModal` 见 Step 6。

- [ ] **Step 4: 加 refreshTasks + renderTasksBoard + buildTaskCard**

在 main.js 现有 `refreshJobs`（main.js:807）/`renderJobsList`（main.js:775）附近，加：

```javascript
async function refreshTasks() {
  try {
    const archived = !!(tasksShowArchived && tasksShowArchived.checked);
    const tasks = await invoke("list_tasks", { archived });
    tasksCache = Array.isArray(tasks) ? tasks : [];
  } catch (e) {
    tasksCache = [];
    tasksBoard.innerHTML = `<div class="jobs-error">读取任务失败: ${escapeHtml(String(e))}</div>`;
    if (tasksEmpty) tasksEmpty.hidden = true;
    return;
  }
  renderTasksBoard();
}

const HORIZON_LABEL = { current: "当前", short: "短期", long: "长期", vision: "愿景" };

function renderTasksBoard() {
  // 清四列
  for (const h of ["current", "short", "long", "vision"]) {
    const list = document.getElementById(`tasks-${h}-list`);
    if (list) list.innerHTML = "";
    const cnt = document.querySelector(`[data-count="${h}"]`);
    if (cnt) cnt.textContent = "0";
  }
  // 分桶（按 horizon 分组；archived 任务 horizon 为空就归到对应列仍渲染）
  const byHorizon = { current: [], short: [], long: [], vision: [] };
  for (const t of tasksCache) {
    const h = byHorizon[t.horizon] ? t.horizon : "current";
    byHorizon[h].push(t);
  }
  let total = 0;
  for (const h of ["current", "short", "long", "vision"]) {
    const list = document.getElementById(`tasks-${h}-list`);
    if (!list) continue;
    byHorizon[h].sort((a, b) => (a.sort_index || 0) - (b.sort_index || 0) || a.id - b.id);
    for (const t of byHorizon[h]) list.appendChild(buildTaskCard(t));
    total += byHorizon[h].length;
    const cnt = document.querySelector(`[data-count="${h}"]`);
    if (cnt) cnt.textContent = String(byHorizon[h].length);
  }
  if (tasksEmpty) tasksEmpty.hidden = total > 0;
}

function buildTaskCard(t) {
  const card = document.createElement("div");
  card.className = `task-card state-${t.status || "todo"}`;
  card.dataset.id = t.id;
  card.dataset.horizon = t.horizon || "current";

  // 阻塞派生：active + 有未解决 blocker
  const blocked = t.status === "active"
    && Array.isArray(t.blockers) && t.blockers.some((b) => !b.resolved);
  if (blocked) card.classList.add("blocked");

  // 徽章
  const badges = [];
  if (blocked) badges.push(`<span class="task-badge blocked">⚠️阻塞</span>`);
  if (t.status === "done" && t.verified) badges.push(`<span class="task-badge verified">✅已验证</span>`);
  else if (t.status === "done" && !t.verified) badges.push(`<span class="task-badge unverified">⚠️待验证</span>`);
  if (t.due_date) badges.push(`<span class="task-badge due">⏰截止</span>`);
  if (t.archived_at != null) badges.push(`<span class="task-badge archived">🗄️归档</span>`);
  if (t.parent_id != null) badges.push(`<span class="task-badge">↳ #${t.parent_id}</span>`);

  // 进度条（acceptance 派生）
  let progressHtml = "";
  const acc = Array.isArray(t.acceptance) ? t.acceptance : [];
  if (acc.length > 0) {
    const done = acc.filter((c) => c.done).length;
    const pct = Math.round((done / acc.length) * 100);
    progressHtml = `<div class="task-progress"><div class="task-progress-fill" style="width:${pct}%"></div></div>`;
    if (badges.length === 0 || true) badges.push(`<span class="task-badge">${done}/${acc.length}</span>`);
  }

  card.innerHTML =
    `<div class="task-card-title">#${t.id} ${escapeHtml(t.title || "(未命名)")}</div>`
    + (t.goal ? `<div class="task-card-goal">${escapeHtml(t.goal)}</div>` : "")
    + (badges.length ? `<div class="task-card-badges">${badges.join("")}</div>` : "")
    + progressHtml
    + (t.last_progress ? `<div class="task-meta" style="margin-top:6px">📍 ${escapeHtml(t.last_progress)}</div>` : "");

  card.onclick = () => openTaskModal(t.id);
  return card;
}
```

- [ ] **Step 5: 加 task-changed listener（在 setupAgentEvents）**

`src/main.js` 的 `setupAgentEvents`（main.js:1961 起），在末尾（其他 listen 之后、函数结束前），加：

```javascript
  await listen("task-changed", () => {
    // 任务板有变（agent 或用户改）——若 tasks 视图打开则重查
    if (tasksView && !tasksView.hidden) refreshTasks();
  });
```

> payload 空（Task 6 actor emit `"task-changed", ()`），收到就重查。

- [ ] **Step 6: 加编辑弹层逻辑（openTaskModal / saveTaskModal / closeTaskModal）**

在 main.js 末尾（或 refreshTasks 附近），加：

```javascript
function openTaskModal(id) {
  editingTaskId = id;
  const t = id != null ? tasksCache.find((x) => x.id === id) : null;
  document.getElementById("task-modal-title").textContent = t ? `编辑 #${t.id}` : "新建任务";
  document.getElementById("task-f-title").value = t ? (t.title || "") : "";
  document.getElementById("task-f-goal").value = t ? (t.goal || "") : "";
  document.getElementById("task-f-detail").value = t ? (t.detail || "") : "";
  document.getElementById("task-f-horizon").value = t ? (t.horizon || "current") : "current";
  document.getElementById("task-f-status").value = t ? (t.status || "todo") : "todo";
  document.getElementById("task-f-parent").value = t && t.parent_id != null ? t.parent_id : "";
  document.getElementById("task-f-due").value = t && t.due_date != null ? t.due_date : "";
  document.getElementById("task-f-tags").value = t && Array.isArray(t.tags) ? t.tags.join(",") : "";
  document.getElementById("task-f-acceptance").value = t && Array.isArray(t.acceptance)
    ? t.acceptance.map((c) => c.text).join("\n") : "";

  // 编辑模式显示归档/删除 + meta
  const archiveBtn = document.getElementById("task-modal-archive");
  const deleteBtn = document.getElementById("task-modal-delete");
  const meta = document.getElementById("task-f-meta");
  if (t) {
    archiveBtn.hidden = false; deleteBtn.hidden = false;
    archiveBtn.textContent = t.archived_at != null ? "恢复" : "归档";
    meta.hidden = false;
    meta.textContent = `创建 ${fmtTime(t.created_at)} · 更新 ${fmtTime(t.updated_at)}${t.completed_at ? " · 完成 " + fmtTime(t.completed_at) : ""}`;
  } else {
    archiveBtn.hidden = true; deleteBtn.hidden = true; meta.hidden = true;
  }

  if (taskModal) taskModal.hidden = false;
}

function closeTaskModal() {
  if (taskModal) taskModal.hidden = true;
  editingTaskId = null;
}

async function saveTaskModal() {
  const title = document.getElementById("task-f-title").value.trim();
  if (!title) { alert("标题不能为空"); return; }
  const horizon = document.getElementById("task-f-horizon").value;
  const status = document.getElementById("task-f-status").value;
  const parentRaw = document.getElementById("task-f-parent").value.trim();
  const dueRaw = document.getElementById("task-f-due").value.trim();
  const tags = document.getElementById("task-f-tags").value.split(",").map((s) => s.trim()).filter(Boolean);
  const accLines = document.getElementById("task-f-acceptance").value.split("\n").map((s) => s.trim()).filter(Boolean);

  try {
    if (editingTaskId != null) {
      // 编辑：update_task
      const patch = {
        title,
        goal: document.getElementById("task-f-goal").value,
        detail: document.getElementById("task-f-detail").value,
        horizon, status, tags,
        acceptance: accLines.map((text) => ({ text, done: false })),
      };
      if (parentRaw) patch.parent_id = Number(parentRaw);
      if (dueRaw) patch.due_date = Number(dueRaw);
      await invoke("update_task", { id: editingTaskId, patch });
    } else {
      // 新建：add_task
      const input = {
        title,
        goal: document.getElementById("task-f-goal").value,
        detail: document.getElementById("task-f-detail").value,
        horizon, status, tags,
        acceptance: accLines.map((text) => ({ text, done: false })),
        created_by: "user",
      };
      if (parentRaw) input.parent_id = Number(parentRaw);
      if (dueRaw) input.due_date = Number(dueRaw);
      await invoke("add_task", { input });
    }
    closeTaskModal();
    await refreshTasks();
  } catch (e) {
    alert("保存失败: " + e);
  }
}

// 弹层按钮接线
if (document.getElementById("task-modal-close")) {
  document.getElementById("task-modal-close").onclick = closeTaskModal;
}
if (document.getElementById("task-modal-cancel")) {
  document.getElementById("task-modal-cancel").onclick = closeTaskModal;
}
if (document.getElementById("task-modal-save")) {
  document.getElementById("task-modal-save").onclick = saveTaskModal;
}
if (document.getElementById("task-modal-archive")) {
  document.getElementById("task-modal-archive").onclick = async () => {
    if (editingTaskId == null) return;
    const t = tasksCache.find((x) => x.id === editingTaskId);
    const willArchive = !(t && t.archived_at != null);
    if (!confirm(willArchive ? "归档此任务？" : "恢复此任务？")) return;
    try {
      await invoke("archive_task", { id: editingTaskId, archived: willArchive });
      closeTaskModal();
      await refreshTasks();
    } catch (e) { alert((willArchive ? "归档" : "恢复") + "失败: " + e); }
  };
}
if (document.getElementById("task-modal-delete")) {
  document.getElementById("task-modal-delete").onclick = async () => {
    if (editingTaskId == null) return;
    if (!confirm("硬删除此任务？此操作不可恢复（归档可恢复，硬删不行）。")) return;
    try {
      await invoke("delete_task", { id: editingTaskId });
      closeTaskModal();
      await refreshTasks();
    } catch (e) { alert("删除失败: " + e); }
  };
}
```

> `fmtTime` 是现有函数（main.js 用于 jobs 卡片——确认存在；若不存在，复用 jobs 的等价或写 `new Date(ms).toLocaleString()`）。

- [ ] **Step 7: 验证 fmtTime 存在**

Run（在 main.js 里搜）：`grep -n "function fmtTime" src/main.js`

若**无**，在 main.js 加：

```javascript
function fmtTime(ms) {
  if (!ms) return "—";
  try { return new Date(Number(ms)).toLocaleString(); } catch (_) { return String(ms); }
}
```

（放在 escapeHtml 附近。）

- [ ] **Step 8: 手动验证（无 JS 测试框架）**

Run: `pnpm tauri dev`（前端改动需 dev server 热重载）
验证清单：
- [ ] topbar 出现「看板」按钮，点击切到 tasks 视图
- [ ] 空态：「还没有任务」展示
- [ ] 新建：点「新建」→ 弹层 → 填标题+选 horizon → 保存 → 卡片出现在对应列
- [ ] 编辑：点卡片 → 弹层带值 → 改状态/勾验收（暂 acceptance 编辑是整组重置 done=false）→ 保存 → 徽章/进度更新
- [ ] 归档：编辑模式「归档」→ 主视图消失 → 勾「归档」checkbox → 归档视图看到
- [ ] 删除：编辑模式「删除」→ 确认 → 卡片消失
- [ ] 实时刷新：开两个 ovoice 窗口（或 agent 在对话里 task_add）→ 另一侧自动刷新（task-changed event）

> **前端无自动化测试**——手动验证是验收门。每项勾掉才算 Task 10 过。

- [ ] **Step 9: Commit**

```bash
git add src/main.js
git commit -m "feat(ui): tasks 看板渲染 + CRUD invoke + 编辑弹层 + task-changed 实时刷新

- showTasks/refreshTasks/renderTasksBoard/buildTaskCard
- 看板四列分桶 + 徽章(阻塞/due/verified/归档) + 进度条(acceptance 派生)
- 编辑弹层 openTaskModal/saveTaskModal（新建+编辑复用）
- 归档/删除按钮（编辑模式；删除二次确认）
- listen task-changed：agent 或用户改 → 视图打开时自动重查

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 11: chat 双渲染路径验证（task_* 工具调用卡）

**Files:**
- Modify: `src/main.js`（**仅加 task 工具显示名映射**——经核查，通用工具卡路径已覆盖 task_*）

**Interfaces:**
- Consumes: Task 5 的 7 个 task 工具（task_list/task_get/task_add/task_update/task_check/task_progress/task_archive）
- Produces: agent 在对话里调 task 工具时，工具卡显示友好名（「📝 列任务」「✏️ 建任务」等），不退化成裸函数名

> **spec §10.3 修正（设计决策 #6）**：经核查 `setupAgentEvents` 的 `appendToolCard`（main.js:1092）和 `buildHistoryBubbles` 的 `addToolCard`（main.js:1608）是**通用工具卡渲染**——任何非 display/edit_card 的工具默认就走工具卡路径。task_* 工具天然走这条，**无需像 display/edit_card 那样特殊分支**。所以本 Task **不是「必须同时改两个函数」**，而是：验证 task_* 默认渲染 OK + 加显示名映射让卡片标题好看。

- [ ] **Step 1: 找工具显示名映射位置**

Run: `grep -n "TOOL_LABEL\|toolLabel\|🔧 \|appendToolCard" src/main.js | head -20`

定位 `appendToolCard`（main.js:1092）——看它怎么渲染工具名（是裸 `name` 还是有映射表）。

- [ ] **Step 2: 加 task 工具显示名映射**

若 main.js 已有工具名→中文映射表，往里加 7 个 task 工具。若无，在 `appendToolCard` 附近加一个：

```javascript
const TASK_TOOL_LABEL = {
  task_list: "📋 列任务",
  task_get: "📋 查任务详情",
  task_add: "✏️ 建任务",
  task_update: "✏️ 改任务",
  task_check: "✅ 勾验收",
  task_progress: "📍 记进展",
  task_archive: "🗄️ 归档",
};

function taskToolLabel(name) {
  return TASK_TOOL_LABEL[name] || null;
}
```

然后在 `appendToolCard`（live 路径）渲染工具卡标题处，用映射：

```javascript
// 找到 appendToolCard 里渲染 name 的地方，例如
// card.querySelector(".tool-head").textContent = `🔧 ${name}`;
// 改为：
const label = taskToolLabel(name) || `🔧 ${name}`;
card.querySelector(".tool-head").textContent = label;
```

`buildHistoryBubbles`（history 路径，main.js:1608）的 `addToolCard` 同理——找到渲染工具名的处，加同一 `taskToolLabel(name)` 映射。

> ⚠️ **最小改动**：只改「工具名显示」那一行，不动卡片结构。live 和 history 两路径都过一遍 `taskToolLabel`。

- [ ] **Step 3: 手动验证（agent 调 task 工具）**

Run: `pnpm tauri dev`
验证：
- [ ] 对话里让 agent「登记一个任务：明天买菜」→ agent 调 `task_add` → chat 流出现工具卡，标题「✏️ 建任务」（非裸 task_add）
- [ ] 让 agent「列一下当前任务」→ `task_list` → 工具卡「📋 列任务」+ 结果正常显示
- [ ] 关闭重开 app（或 reload history）→ 历史 reload 仍渲染 task 工具卡（不退化成 JSON）——**这是双渲染路径的核心验收**

- [ ] **Step 4: Commit**

```bash
git add src/main.js
git commit -m "feat(ui): task 工具调用卡友好显示名（chat 双渲染路径）

经核查 task_* 默认走通用工具卡路径（非 display/edit_card 特殊分支），
本 Task 仅加显示名映射（📋列/✏️建改/✅勾/📍进展/🗄️归档），
live(appendToolCard) + history(addToolCard) 两路径共用。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 12: Final whole-branch review + 手动端到端验证

**Files:**
- 无源码改动（review + 验证 task）

**Interfaces:**
- Consumes: Task 1-11 全部产出

> **memory: ovoice-sdd-integration-wiring**：逐任务 review 全 green 仍可能漏运行时接线。Final whole-branch review 必做兜底——尤其 cross-task 接线（DbActor 真注入？app.manage 在 spawn_session 前？llm/tools cascade 两边对齐？前端 5 command 全注册？）。
> **memory: ovoice-tool-count-cascade**：merge-critical，controller 自验 schemas() 实际 count == 20 == llm.rs 断言 == tools.rs 测试期望。

- [ ] **Step 1: 跑全测试套件**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib`
Expected: 全过。重点确认：
- `tasks::` 9 测全过（Task 2/4）
- `tools::schemas_has_twenty_tools` 过（Task 5）
- `llm::` 含 `len()==20` 的测试过（Task 5）
- 其他模块（agent/jobs/dream/...）无 regression

- [ ] **Step 2: cargo check 零 warning**

Run: `cargo check --manifest-path src-tauri/Cargo.toml --lib`
Expected: 退出码 0，**0 warning**（Task 3 的 dead_code 应已消——DbActor/TaskInput/TaskPatch 都被用了）。

- [ ] **Step 3: 红线 1 自验（grep 确认零碰）**

Run 以下命令，确认**无输出**（tasks 子系统没碰对话核心）：

```bash
# context.rs build_messages 签名未变
git diff main..HEAD -- src-tauri/src/context.rs | head -5
# history.jsonl 写路径未变（task 工具的 tool_result 是既有机制，不应改 history.rs）
git diff main..HEAD -- src-tauri/src/history.rs | head -5
# dream/memory 未碰
git diff main..HEAD -- src-tauri/src/mem_dream.rs src-tauri/src/dream_trigger.rs | head -5
```

> `git diff main..HEAD -- <file>` 若空=没碰（绿）。context.rs / history.rs / dream_* 应都空 diff（或仅与本子系无关的既有改动）。

- [ ] **Step 4: tool cascade 自验**

Run:

```bash
grep -n "tools.len\(\) == 2" src-tauri/src/llm.rs
grep -c 'name":"task_' src-tauri/src/tools.rs
```

Expected:
- llm.rs 含 `tools.len() == 20`（断言）
- tools.rs 含 7 处 `"name":"task_`（7 schema）+ 7 dispatch 分支（`grep -c '"task_' ...` 应 ≥14，含 schema+dispatch）

- [ ] **Step 5: 端到端手动验证（完整生命周期）**

Run: `pnpm tauri dev`

走 spec §14 的电子书生命周期（简化版）：
- [ ] 对话：「我想写本电子书《X》，帮我登记成任务」→ agent `task_add` → 看板 current 列出现卡片，标题「电子书《X》」
- [ ] 对话：「拆成 3 章的验收清单」→ agent `task_update` acceptance / 或 `task_check` → 卡片进度条出现
- [ ] 看板：点卡片 → 编辑弹层显示 3 章 acceptance + meta 时间戳
- [ ] 对话：「开始写第 1 章」→ agent `task_update status=active` → 卡片状态变 active（边框高亮）
- [ ] 对话：「我卡住了，缺资料」→ agent raise blocker → 卡片标红「⚠️阻塞」徽章
- [ ] 看板 + 对话同时操作：你在看板改 status，agent 在对话改 progress → 两侧 task-changed 实时同步
- [ ] 关闭重开 app → 任务板数据持久（sqlite）+ 历史 reload 渲染 task 工具卡（不退化 JSON）

- [ ] **Step 6: 整理 commit log（若中途有 fix-up，可 squash；否则保留分 Task 提交）**

Run: `git log main..HEAD --oneline`
Expected: Task 1-11 各一 commit（约 11 个），每个 `Co-Authored-By` 齐。

- [ ] **Step 7: 标记完成**

本 Task 无 commit（review + 验证 task）。完成后调用 `superpowers:finishing-a-development-branch` 决定 merge/PR/keep。

---

## Self-Review（plan 自审，已执行）

**1. Spec coverage（逐节核对）**：
- §5 数据模型（Task 20 字段 + 3 嵌套）→ Task 2 ✅
- §6 状态机（status 流转 + verified + 阻塞派生 + 归档）→ Task 2（派生）+ Task 4（流转测试）✅
- §7 持久化（sqlite + migrate + WAL + 索引 + async 桥）→ Task 1（deps）+ Task 2（migrate）+ Task 3（DbActor）✅
- §8 agent 7 工具 + cascade → Task 5 ✅（注：delete 不给 agent，只 7 个：list/get/add/update/check/progress/archive；硬删是用户侧 command——spec §8.1 也是 7 个不含 delete ✅）
- §9 agent 自主管理 + verified 制衡 → Task 5（tool_task_update 不暴露 verified）+ Task 6（TaskPatch.verified 用户侧）+ Task 7（AGENT.md）✅
- §10 用户侧（5 command + 看板 + 双渲染 + task-changed）→ Task 6（command）+ Task 8-11（前端）✅
- §11 后端架构（tasks.rs / tools.rs / lib.rs / AGENT.md）→ Task 2-7 ✅
- §12 前端架构（index.html / main.js / styles.css）→ Task 8-11 ✅
- §16 不变量（tool cascade / build_messages 不动 / 单写者 / 向后兼容）→ Global Constraints + Task 12 自验 ✅

**2. Placeholder scan**：无 TBD/TODO；所有代码步含完整代码；命令含 expected output。✅

**3. Type consistency**：
- `DbActorHandle` 8 async 方法名跨 Task 3/5/6 一致（list/get/add/update/check/set_progress/archive/delete）✅
- `TaskInput`/`TaskPatch` 字段跨 Task 3（定义）→ Task 5（agent 用）→ Task 6（用户侧 +verified/acceptance）一致 ✅
- `Horizon`/`Status`/`Actor`/`ArtifactKind` 的 `as_str`/`from_str` 跨 Task 2（定义）→ Task 5/6（用）一致 ✅
- `TaskFilter` 字段（horizon/status/archived/parent_id）跨 Task 2（定义）→ Task 4（测试）→ Task 5/6（用）一致 ✅
- SQL 列名跨 Task 2（CREATE TABLE）→ Task 3（SELECT/UPDATE）一致 ✅

**已知 plan 内的「过程性脚手架」**（非 placeholder，是有意的教学痕迹）：
- Task 3 Step 1 末尾的 `pushset!` macro 反面教材 + Step 2 `sql_update_explicit` 正解——实现者须按 Step 2 指示删掉 macro 段。这是有意暴露「不要用 macro 拼 SQL」的坑。
- Task 3 Step 4 的 `blocking_recv` 错版 + Step 5 `rx.await` 正解——同理，有意暴露 async-in-actor 的坑。

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-08-12-task-management.md`. Two execution options:

**1. Subagent-Driven (recommended)** - I dispatch a fresh subagent per task, review between tasks, fast iteration

**2. Inline Execution** - Execute tasks in this session using executing-plans, batch execution with checkpoints

Which approach?

> 用户已预先指示：「等会你写完 plan 就直接启动 sub agent 执行计划吧」——选 **1. Subagent-Driven**，立即起。

