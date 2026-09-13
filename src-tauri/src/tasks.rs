//! 任务管理子系统：当前工作记忆状态板（sqlite）。
//! 独立于对话流（红线 1）：不碰 context/history/dream；agent 经 7 工具 + 用户经 5 command 共用本模块的 async API + DbActor。
//!
//! 架构：DbActor 跑在专用 OS 线程独占 Connection（单一写者，sqlite 不需锁），
//!       经 mpsc::unbounded_channel 收 DbCommand，oneshot 回结果。
//!       ◄ 跟 history writer task / job writer task 同款 ►

use rusqlite::{params, Connection, Row};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
// Manager 暂未用（Task 6 setup 拿 state 时才用）；AppHandle/Emitter 已在 spawn_db_actor 用。
#[allow(unused_imports)]
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{mpsc, oneshot};

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
    /// 用户侧升 verified（agent 不暴露此字段——防自述完成）。
    /// sql_update_explicit honor 它；status≠Done 时仍强制清 false。
    pub verified: Option<bool>,
    /// 用户侧整组替换 acceptance（agent 用 task_check 逐项；用户编辑弹层整组写）。
    pub acceptance: Option<Vec<CheckItem>>,
}

/// verified 逻辑约束：status≠Done && verified=true → 拒绝。
fn validate_status_verified(status: Status, verified: bool) -> Result<(), String> {
    if status != Status::Done && verified {
        return Err(format!(
            "逻辑错：status={:?}（非 Done）不能 verified=true",
            status
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
    // verified 跟随：patch.verified 优先（用户侧升），否则保 existing；非 Done 强制清 false（防逻辑错）
    let mut new_verified = patch.verified.unwrap_or(existing.verified);
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
    sql_update_explicit(conn, id, patch, existing, new_status, new_verified, completed_at, now)
}

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
    let acceptance = serde_json::to_string(&patch.acceptance.unwrap_or(existing.acceptance))
        .unwrap_or_else(|_| "[]".into());

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
    if affected == 0 {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    sql_get(conn, id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

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

// ── DbActor：专用 OS 线程独占 Connection ───────────────────

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

        // horizon=current = a + c（c 继承 sample_input 的 Current horizon）
        let cur = sql_list(
            &conn,
            &TaskFilter { horizon: Some(Horizon::Current), ..Default::default() },
        )
        .unwrap();
        assert_eq!(cur.len(), 2);
        assert!(cur.iter().any(|x| x.id == a.id));
        assert!(cur.iter().any(|x| x.id == c.id));

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
}
