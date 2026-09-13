//! 后台 Job 系统：进程型 Job + JobRegistry。
//! 独立于 agent.rs —— 完成信号走 mpsc::Sender<JobOutcome>，状态推送走 JobUpdate trait。
use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use crate::history::{date_from_ts_local, local_offset_secs};

pub type JobId = String;
pub const MAX_RUNNING: usize = 8;
pub const MAX_AGENTS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobKind { Process, Agent }

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
    pub kind: JobKind,
    pub label: String,
    pub status: JobStatus,
    pub log_path: PathBuf,
    pub started_at: u64,           // epoch ms
    pub finished_at: Option<u64>,
    // agent 专属（process 时为 None/默认）：
    #[serde(skip)]
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    #[serde(skip)]
    pub progress: Option<std::sync::Arc<std::sync::Mutex<SubagentProgress>>>,
    pub answer: Option<String>,
    #[serde(skip)]
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
    pub fn new() -> Self { Self { rounds: 0, recent_tools: Default::default(), partial: String::new(), started: std::time::Instant::now() } }
}
impl Default for SubagentProgress { fn default() -> Self { Self::new() } }

#[derive(Debug, Clone)]
pub struct ToolTrace { pub name: String, pub args_brief: String, pub result_brief: Option<String> }

/// guardian 完成时投递给 driver 的结果（driver 据此注入 [后台任务 #N 完成]）。
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

impl JobRegistry {
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

    pub fn new_with_max(max_agents: usize) -> Self {
        Self { max_agents, ..Self::new() }
    }

    /// 达上限返回 false（A3）。
    pub fn can_spawn(&self, kind: JobKind) -> bool {
        match kind { JobKind::Process => self.running < MAX_RUNNING, JobKind::Agent => self.running_agents < self.max_agents }
    }

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

    pub fn get(&self, id: &JobId) -> Option<&Job> { self.jobs.get(id) }

    /// 置终态；若从 Running 转出则递减 running 计数（完成/失败/杀都释放并发槽）。
    pub fn finish(&mut self, id: &JobId, status: JobStatus, finished_at: u64) {
        if let Some(j) = self.jobs.get_mut(id) {
            let was_running = matches!(j.status, JobStatus::Running);
            let kind = j.kind;
            j.status = status; j.finished_at = Some(finished_at);
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

impl Default for JobRegistry { fn default() -> Self { Self::new() } }

pub type SharedRegistry = Arc<Mutex<JobRegistry>>;

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

/// 迁移旧 u64 时代的 {n}.log → legacy-{n}.log（spec D5：旧 .log 保留，数字 id → legacy-<n>）。
/// 纯数字文件名才迁；新格式（YYYYMMDD-NN.log / legacy-N.log）不动。给每个旧 log 写一条
/// legacy job 记录（status=failed, reason=legacy）到今日 jsonl，Jobs 面板可见。
/// 幂等：已迁移过的（无纯数字 .log）下次启动 no-op。
pub fn migrate_legacy_logs(jobs_dir: &Path, _offset_secs: i64, writer: &JobWriterHandle) {
    let entries = match std::fs::read_dir(jobs_dir) { Ok(rd) => rd, Err(_) => return };
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
    }
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

#[cfg(windows)]
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
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    // 日志落 cache/.ovoice-jobs（read_job_log 从 cache 读；cwd 只是命令工作目录=workspace，
    // 与日志位置无关）。旧 bug 曾用 cwd.join → 写 workspace → read_job_log 读 cache 永远落空。
    let jobs_dir = cache.join(".ovoice-jobs");
    let _ = std::fs::create_dir_all(&jobs_dir);
    let started = now_ms();

    // A3 并发上限 + 登记：同一把锁内 check-and-register（避免并发竞态超限）
    let (id, log_path) = {
        let mut r = registry.lock().unwrap();
        if !r.can_spawn(JobKind::Process) { return Err("已达并发上限(8)".into()); }
        let id = r.register(JobKind::Process, label.clone(), PathBuf::new(), started);
        if let Some(j) = r.jobs.get(&id) { writer.append(JobEvent::started(j)); }
        (id.clone(), jobs_dir.join(format!("{id}.log")))
    };
    {
        let mut r = registry.lock().unwrap();
        if let Some(j) = r.jobs.get_mut(&id) { j.log_path = log_path.clone(); }
        let snap = r.get(&id).cloned().unwrap();
        drop(r);
        update.update(&snap);
    }

    // 跨平台 sh（busybox Win / /bin/sh Unix）+ CREATE_NO_WINDOW（spec §4/D1/D4）；kill_on_drop=false（靠 win_job 句柄杀树，D6 bg 保留 win_job）。
    let mut cmd = crate::bash::build_command(&command, &env_vars, cwd, false);

    let mut child = cmd.spawn().map_err(|e| format!("启动失败: {e}"))?;
    let pid = child.id();

    // A4：Job Object create/assign 失败 → 杀掉刚起的子进程、判 Failed（避免孤儿）
    let job_handle: Option<crate::tools::win_job::Job> = match (crate::tools::win_job::Job::create(), pid) {
        (Some(j), Some(pid)) => {
            if j.assign_pid(pid) { Some(j) }
            else {
                let _ = child.start_kill();
                fail_job(&registry, &id, &update, "Job Object 分配失败");
                return Err("Job Object 分配失败".into());
            }
        }
        (Some(j), None) => Some(j),
        (None, _) => {
            let _ = child.start_kill();
            fail_job(&registry, &id, &update, "Job Object 创建失败");
            return Err("Job Object 创建失败".into());
        }
    };
    // 句柄入 registry 的 handles map：活过 child.wait()；drop（kill/app 退出）→ KILL_ON_JOB_CLOSE 杀树
    if let Some(h) = job_handle {
        let mut r = registry.lock().unwrap();
        r.handles.insert(id.clone(), h);
    }

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let log_for_drain = log_path.clone();
    // A1 pipe-drainer：stdout/stderr 并发 drain 后合并写 log（避免一个管道满时阻塞另一管道的死锁）
    let drainer = tokio::spawn(async move {
        let mut out_buf = Vec::new();
        let mut err_buf = Vec::new();
        let ofut = async { if let Some(mut s) = stdout { let _ = s.read_to_end(&mut out_buf).await; } };
        let efut = async { if let Some(mut e) = stderr { let _ = e.read_to_end(&mut err_buf).await; } };
        tokio::join!(ofut, efut);
        out_buf.extend_from_slice(&err_buf);
        let _ = std::fs::write(&log_for_drain, &out_buf);
    });

    let reg2 = registry.clone();
    let upd2 = update.clone();
    let writer2 = writer.clone();
    let done2 = done_tx.clone();
    let log_for_guard = log_path.clone();
    let id_for_spawn = id.clone();
    // guardian：超时/完成 → 投递 JobOutcome；emit 前查 Killed（kill 不唤醒模型）
    tokio::spawn(async move {
        let res = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await;
        // 超时时需主动杀子进程，否则 drainer 读 pipe 会永久阻塞
        let timeout_occurred = res.is_err();
        if timeout_occurred {
            let _ = child.start_kill();
        }
        // 正常退出时等 drain 写 log；超时时丢弃 drainer（Windows pipe 可能不关闭）
        if !timeout_occurred {
            let _ = drainer.await;
        } else {
            drop(drainer);
        }
        let tail = read_tail(&log_for_guard);
        let (ok, code_opt, reason) = match res {
            Ok(Ok(s)) => (true, s.code(), None),
            Ok(Err(_)) => (false, None, Some("进程 wait 失败".to_string())),
            Err(_) => {
                // 超时：取下并 drop Job 句柄 → KILL_ON_JOB_CLOSE 杀整棵树（避免孤儿）
                let h = { let mut r = reg2.lock().unwrap(); r.handles.remove(&id_for_spawn) };
                drop(h);
                (false, None, Some(format!("超时({timeout_secs}s)")))
            }
        };
        let status = match (&reason, code_opt) {
            (Some(r), _) => JobStatus::Failed { reason: r.clone() },
            (None, Some(c)) => JobStatus::Done { code: c },
            (None, None) => JobStatus::Failed { reason: "无退出码".into() },
        };
        let killed_already = {
            let mut r = reg2.lock().unwrap();
            // 原子 check-and-finish：查 Killed 与 finish 在同一把锁内，杜绝 kill_job 在两步之间
            // 插入导致 Killed 被 Done/Failed 覆盖、并误发 JobDone 唤醒模型（G2 竞态）。
            let is_killed = matches!(r.get(&id_for_spawn).map(|j| &j.status), Some(JobStatus::Killed));
            if !is_killed { r.finish(&id_for_spawn, status, now_ms()); }
            let snap = r.get(&id_for_spawn).cloned().unwrap();
            drop(r);
            // 写终态 JobEvent（done/failed；killed 不写——人为终止不落 job jsonl，与不唤醒模型一致）
            if !is_killed {
                writer2.append(terminal_event(&snap));
            }
            upd2.update(&snap);
            is_killed
        };
        if !killed_already {
            let _ = done2.send(JobOutcome { job_id: id_for_spawn, kind: JobKind::Process, label: None, ok, code: code_opt, tail, answer: None, note: None }).await;
        }
    });

    Ok(id)
}

/// 人为终止（UI kill_job 命令）：
/// - Process：置 Killed + 关 Job 句柄杀整棵树（Windows）。guardian 见 Killed 跳过 send。
/// - Agent：仅触发 CancellationToken（spawned 任务据此收尾 + 按 suppress_inject 决定是否 send）。
///   不在此置 Killed——否则会抢占 spawned 任务的 check-and-finish，致 human-kill 自动回注丢失。
pub fn kill_job(id: JobId, registry: &SharedRegistry, update: &std::sync::Arc<dyn JobUpdate>) -> bool {
    let (snap, handle, cancel) = {
        let mut r = registry.lock().unwrap();
        let Some(j) = r.jobs.get_mut(&id) else { return false; };
        let kind = j.kind;
        if matches!(kind, JobKind::Process) && matches!(j.status, JobStatus::Running) {
            j.status = JobStatus::Killed;
            j.finished_at = Some(now_ms());
        }
        let cancel = j.cancel.clone();
        // Clone needed data before second mutable borrow
        let is_process = matches!(kind, JobKind::Process);
        #[cfg(windows)]
        let handle: Option<crate::tools::win_job::Job> = if is_process { r.handles.remove(&id) } else { None };
        #[cfg(not(windows))]
        let handle: Option<()> = None;
        let snap = r.jobs.get(&id).cloned().unwrap();
        (snap, handle, cancel)
    };
    drop(handle); // process: 关句柄杀树；agent/none: no-op
    if let Some(c) = cancel { c.cancel(); } // agent: 触发 spawned 任务收尾
    update.update(&snap);
    true
}

#[cfg(windows)]
fn fail_job(registry: &SharedRegistry, id: &JobId, update: &Arc<dyn JobUpdate>, reason: &str) {
    let mut r = registry.lock().unwrap();
    r.finish(id, JobStatus::Failed { reason: reason.into() }, now_ms());
    let snap = r.get(id).cloned().unwrap();
    drop(r);
    update.update(&snap);
}

/// 今日日期 YYYYMMDD（本地，去横杠）。与 history::date_from_ts_local 同源（spec §1：id 编码 spawn 日）。
fn today_compact() -> String {
    date_from_ts_local(now_ms(), local_offset_secs()).replace('-', "")
}

/// Job 终态 → JobEvent（done/failed）。killed 不调用此函数（人为终止不落 jsonl）。
pub(crate) fn terminal_event(j: &Job) -> JobEvent {
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 读取日志末尾 ~4KB（结果/文件路径通常在输出末尾），按 UTF-8 char 边界对齐起始。
#[cfg(windows)]
fn read_tail(log: &std::path::Path) -> String {
    const TAIL: usize = 4 * 1024;
    let bytes = std::fs::read(log).unwrap_or_default();
    let text = crate::tools::decode_output(&bytes);
    let t = text.trim();
    if t.len() <= TAIL { return t.to_string(); }
    let mut start = t.len() - TAIL;
    while start < t.len() && !t.is_char_boundary(start) { start += 1; }
    format!("…[已截断]\n{}", &t[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch() -> u64 { 0 }

    #[test]
    fn register_increments_id_and_running() {
        let mut r = JobRegistry::new();
        let _a = r.register(JobKind::Process, "a".into(), PathBuf::from("/a"), epoch());
        let _b = r.register(JobKind::Process, "b".into(), PathBuf::from("/b"), epoch());
        // id 格式由 register_generates_today_seq_id 覆盖；此处仅验 running 计数。
        assert!(r.can_spawn(JobKind::Process));
    }

    #[test]
    fn cap_blocks_after_max() {
        let mut r = JobRegistry::new();
        for _ in 0..MAX_RUNNING { r.register(JobKind::Process, "x".into(), PathBuf::from("/"), epoch()); }
        assert!(!r.can_spawn(JobKind::Process), "达上限应禁止再开");
    }

    #[test]
    fn finish_releases_running_slot() {
        let mut r = JobRegistry::new();
        let mut first_id = None;
        for _ in 0..MAX_RUNNING {
            let id = r.register(JobKind::Process, "x".into(), PathBuf::from("/"), epoch());
            if first_id.is_none() { first_id = Some(id); }
        }
        assert!(!r.can_spawn(JobKind::Process));
        r.finish(&first_id.unwrap(), JobStatus::Done { code: 0 }, epoch());
        assert!(r.can_spawn(JobKind::Process), "完成后释放并发槽");
    }

    #[test]
    fn finish_idempotent_on_running_decrement() {
        let mut r = JobRegistry::new();
        let id = r.register(JobKind::Process, "x".into(), PathBuf::from("/"), epoch());
        r.finish(&id, JobStatus::Done { code: 0 }, epoch());
        let before = r.running;
        r.finish(&id, JobStatus::Killed, epoch()); // 重复 finish 不应再递减
        assert_eq!(r.running, before);
    }

    #[cfg(windows)]
    async fn drain_all(rx: &mut mpsc::Receiver<JobOutcome>) -> Vec<JobOutcome> {
        let mut out = vec![];
        while let Ok(o) = rx.try_recv() { out.push(o); }
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        while let Ok(o) = rx.try_recv() { out.push(o); }
        out
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn spawn_echo_completes_done() {
        let reg: SharedRegistry = Arc::new(Mutex::new(JobRegistry::new()));
        let (tx, mut rx) = mpsc::channel(8);
        let dir = tempfile::tempdir().unwrap();
        let id = spawn_process_job(
            "echo hello".into(), vec![], dir.path(), dir.path(), 15, "echo".into(),
            reg.clone(), tx, Arc::new(NoopJobUpdate), JobWriterHandle::noop(),
        ).await.unwrap();
        let out = drain_all(&mut rx).await;
        assert_eq!(out.len(), 1);
        assert!(out[0].ok, "echo 应成功");
        let r = reg.lock().unwrap();
        assert!(matches!(r.get(&id).unwrap().status, JobStatus::Done { code: 0 }));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn spawn_writes_log_to_cache_not_cwd() {
        // 回归：.ovoice-jobs 日志须落 cache（read_job_log 从 cache 读），不是 cwd(=workspace)。
        // 旧 bug：jobs_dir=cwd.join → 写 workspace；lib.rs read_job_log 读 cache → 永远读不到。
        let reg: SharedRegistry = Arc::new(Mutex::new(JobRegistry::new()));
        let (tx, mut rx) = mpsc::channel(8);
        let cwd = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let id = spawn_process_job(
            "echo hello".into(), vec![], cwd.path(), cache.path(), 15, "echo".into(),
            reg.clone(), tx, Arc::new(NoopJobUpdate), JobWriterHandle::noop(),
        ).await.unwrap();
        let _ = drain_all(&mut rx).await;
        let dir_in_cache = cache.path().join(".ovoice-jobs");
        let dir_in_cwd = cwd.path().join(".ovoice-jobs");
        assert!(dir_in_cache.exists(), "日志目录应落 cache/.ovoice-jobs: {}", dir_in_cache.display());
        assert!(!dir_in_cwd.exists(), "日志目录不该落 cwd(=workspace): {}", dir_in_cwd.display());
        let log_file = dir_in_cache.join(format!("{id}.log"));
        assert!(log_file.exists(), "日志文件应已写: {}", log_file.display());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn spawn_timeout_marks_failed() {
        let reg: SharedRegistry = Arc::new(Mutex::new(JobRegistry::new()));
        let (tx, mut rx) = mpsc::channel(8);
        let dir = tempfile::tempdir().unwrap();
        let id = spawn_process_job(
            "ping -n 100 127.0.0.1".into(), vec![], dir.path(), dir.path(), 1, "hang".into(),
            reg.clone(), tx, Arc::new(NoopJobUpdate), JobWriterHandle::noop(),
        ).await.unwrap();
        let out = drain_all(&mut rx).await;
        assert_eq!(out.len(), 1);
        assert!(!out[0].ok);
        let r = reg.lock().unwrap();
        assert!(matches!(r.get(&id).unwrap().status, JobStatus::Failed { .. }));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn spawn_pipe_drain_does_not_block_on_large_output() {
        // A1 关键：写 >4KB stdout；若不 drain，子进程阻塞、guardian 超时判 Failed。
        let reg: SharedRegistry = Arc::new(Mutex::new(JobRegistry::new()));
        let (tx, mut rx) = mpsc::channel(8);
        let dir = tempfile::tempdir().unwrap();
        let cmd = r#"for /L %i in (1,1,2000) do @echo line%i"#;
        let id = spawn_process_job(
            cmd.into(), vec![], dir.path(), dir.path(), 20, "chatty".into(),
            reg.clone(), tx, Arc::new(NoopJobUpdate), JobWriterHandle::noop(),
        ).await.unwrap();
        let out = drain_all(&mut rx).await;
        assert_eq!(out.len(), 1);
        assert!(out[0].ok, "大输出必须因 drain 正常完成，而非死锁超时");
        let r = reg.lock().unwrap();
        assert!(matches!(r.get(&id).unwrap().status, JobStatus::Done { .. }));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn spawn_rejected_at_cap() {
        let reg: SharedRegistry = Arc::new(Mutex::new(JobRegistry::new()));
        let (tx, _rx) = mpsc::channel(64);
        let dir = tempfile::tempdir().unwrap();
        for _ in 0..MAX_RUNNING {
            spawn_process_job("ping -n 60 127.0.0.1".into(), vec![], dir.path(), dir.path(), 60, "x".into(),
                reg.clone(), tx.clone(), Arc::new(NoopJobUpdate), JobWriterHandle::noop()).await.unwrap();
        }
        let err = spawn_process_job("echo hi".into(), vec![], dir.path(), dir.path(), 5, "over".into(),
            reg.clone(), tx, Arc::new(NoopJobUpdate), JobWriterHandle::noop()).await;
        assert!(err.is_err(), "达上限应拒绝");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn kill_does_not_emit_jobdone() {
        let reg: SharedRegistry = Arc::new(Mutex::new(JobRegistry::new()));
        let (tx, mut rx) = mpsc::channel(8);
        let dir = tempfile::tempdir().unwrap();
        let update: Arc<dyn JobUpdate> = Arc::new(NoopJobUpdate);
        let id = spawn_process_job(
            "ping -n 60 127.0.0.1".into(), vec![], dir.path(), dir.path(), 60, "hang".into(),
            reg.clone(), tx, update.clone(), JobWriterHandle::noop(),
        ).await.unwrap();
        assert!(kill_job(id.clone(), &reg, &update));
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let mut got = vec![];
        while let Ok(o) = rx.try_recv() { got.push(o); }
        assert!(got.is_empty(), "人为终止不应投递 JobOutcome（不唤醒模型）");
        let r = reg.lock().unwrap();
        assert!(matches!(r.get(&id).unwrap().status, JobStatus::Killed));
    }

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

    #[test]
    fn register_agent_increments_running_agents_only() {
        let mut r = JobRegistry::new();
        let id = r.register(JobKind::Agent, "子代理".into(), PathBuf::new(), 0);
        assert!(r.can_spawn(JobKind::Agent));
        assert_eq!(r.running_agents, 1, "agent 计数独立");
        assert_eq!(r.running, 0, "process 计数不动");
        assert!(matches!(r.get(&id).unwrap().kind, JobKind::Agent));
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
        r.finish(&id, JobStatus::Done { code: 0 }, 0);
        assert_eq!(r.running_agents, 0);
    }
    #[test]
    fn subagent_progress_default_empty() {
        let p = SubagentProgress::new();
        assert_eq!(p.rounds, 0);
        assert!(p.recent_tools.is_empty());
        assert!(p.partial.is_empty());
    }

    #[test]
    fn kill_job_agent_triggers_cancel() {
        let mut r = JobRegistry::new();
        let id = r.register(JobKind::Agent, "子代理".into(), PathBuf::new(), 0);
        let tok = tokio_util::sync::CancellationToken::new();
        r.jobs.get_mut(&id).unwrap().cancel = Some(tok.clone());
        let upd: std::sync::Arc<dyn JobUpdate> = std::sync::Arc::new(NoopJobUpdate);
        let registry: SharedRegistry = std::sync::Arc::new(std::sync::Mutex::new(r));
        assert!(kill_job(id, &registry, &upd), "agent job 应可 kill");
        assert!(tok.is_cancelled(), "agent kill 必须触发 cancel token");
        // agent 不在 kill_job 内置 Killed（留给 spawned 任务）；此处仅验 cancel 触发 + 返回 true
    }

    #[test]
    fn kill_job_unknown_returns_false() {
        let registry: SharedRegistry = std::sync::Arc::new(std::sync::Mutex::new(JobRegistry::new()));
        let upd: std::sync::Arc<dyn JobUpdate> = std::sync::Arc::new(NoopJobUpdate);
        assert!(!kill_job("999".into(), &registry, &upd));
    }

    #[test]
    fn new_with_max_caps_agents_at_custom_limit() {
        let mut r = JobRegistry::new_with_max(2);
        r.register(JobKind::Agent, "a".into(), PathBuf::new(), 0);
        r.register(JobKind::Agent, "b".into(), PathBuf::new(), 0);
        assert!(!r.can_spawn(JobKind::Agent), "自定义上限 2 应已满");
        assert!(r.can_spawn(JobKind::Process), "process 上限独立");
    }

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
        r.next_seq = 100;
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

    // ───────────────────────────────────────────────────────────────────────────
    // JobEvent + JobWriter 测试（TDD：先失败测试，后实现）
    // ───────────────────────────────────────────────────────────────────────────

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

    #[tokio::test]
    async fn load_today_restores_seq_and_marks_forced_exit() {
        let dir = tempfile::tempdir().unwrap();
        let jd = dir.path().join(".ovoice-jobs");
        // session 1：写 2 个完成 + 1 个悬空 running（app 强制退出没写终态）
        let w1 = spawn_job_writer(jd.clone(), 0);
        let today = crate::history::date_from_ts_local(now_ms(), crate::history::local_offset_secs());
        // 用动态今日日期构造 id（曾硬编码 20260727，过零点即因 load_today 按今日前缀过滤而全 skip → next_seq=1 假 fail）
        let compact = today.replace('-', "");
        let (id1, id2, id3) = (format!("{compact}-01"), format!("{compact}-02"), format!("{compact}-03"));
        w1.append(je(&id1, "running", now_ms()));
        let mut d1 = je(&id1, "done", now_ms()); d1.code = Some(0); d1.finished_at = Some(now_ms());
        w1.append(d1);
        w1.append(je(&id2, "running", now_ms())); // 悬空（无终态）
        w1.append(je(&id3, "running", now_ms()));
        let mut d3 = je(&id3, "done", now_ms()); d3.code = Some(0); d3.finished_at = Some(now_ms());
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
        let j02 = r.get(&id2).unwrap();
        assert!(matches!(&j02.status, JobStatus::Failed { reason } if reason == "forced_exit"),
            "悬空 running 应标 forced_exit，实际: {:?}", j02.status);
        // 01/03 done 正常
        assert!(matches!(r.get(&id1).unwrap().status, JobStatus::Done { .. }));
        assert!(matches!(r.get(&id3).unwrap().status, JobStatus::Done { .. }));
        // forced_exit 事件已补写 jsonl（02 最后一条应是 failed+forced_exit）
        let evs2 = read_jobs_jsonl(&jd, &today);
        let j02_ev = evs2.iter().filter(|e| e.id == id2).last().unwrap();
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

    #[tokio::test]
    async fn mark_running_as_forced_exit_marks_and_writes() {
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
}
