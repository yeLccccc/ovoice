//! 定时任务:per-task 精确 timer(每任务独立 sleep 到自己下次触发点)。
//!
//! 投递后的一切(进 history / 跑一轮 / 回复 / 归档)全复用主 agent 现有链路
//! (经 SessionEvent::UserMessage 触发,跟用户发消息同入口)。
//!
//! 为什么不用全局 ticker(初版踩的坑):一个 60s 轮询会把所有任务的触发精度掐到 60s,
//! interval < 60s 的任务(如 30s)永远跑不出自己的间隔。dream 那样做没问题(它触发条件是
//! idle/token,分钟级够),定时任务的 interval 是用户精确指定的,必须每任务独立计时。
//!
//! 配置同步:scheduler.json 被 command(界面)或工具(agent)写后,10s watcher diff 同步
//! (新增 spawn / 删除·禁用·改了 abort+重建);command 写完顺带立即 reload 即时生效。
use crate::agent::SessionEvent;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 一个定时任务。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimerTask {
    pub task_id: String,
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    pub mode: String,                       // "interval" | "schedule"
    #[serde(default = "d_count")]
    pub count: String,                      // "once" | "forever"
    #[serde(default)]
    pub interval: Option<IntervalSpec>,
    #[serde(default)]
    pub schedule: Option<ScheduleSpec>,
    pub action: String,
    #[serde(default)]
    pub context_inject: Vec<String>,
    #[serde(default)]
    pub last_fired_ts: u64,
    #[serde(default)]
    pub last_fired_date: String,
}

fn d_count() -> String { "forever".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntervalSpec {
    pub value: u64,
    pub unit: String,                       // "sec"|"min"|"hour"|"day"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleSpec {
    pub time: String,                       // "HH:MM"(本地)
    pub repeat: String,                     // "daily"|"weekly"|"monthly"|"yearly"
    #[serde(default)]
    pub weekdays: Vec<u8>,                  // weekly:0=周日..6=周六
    #[serde(default)]
    pub day: u8,
    #[serde(default)]
    pub month: u8,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SchedulerConfig {
    #[serde(default)]
    pub global_enabled: bool,
    #[serde(default)]
    pub tasks: Vec<TimerTask>,
}

pub fn path_in(dir: &Path) -> PathBuf { dir.join("scheduler.json") }

pub fn load_from(dir: &Path) -> SchedulerConfig {
    let path = path_in(dir);
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str::<SchedulerConfig>(&s).unwrap_or_default(),
        Err(_) => SchedulerConfig::default(),
    }
}

/// 原子写(先 .tmp 再 rename):watcher/command/工具并发写,中途崩溃不能留半截 json。
pub fn save_to(dir: &Path, cfg: &SchedulerConfig) -> Result<(), String> {
    let _ = std::fs::create_dir_all(dir);
    let path = path_in(dir);
    let json = serde_json::to_string_pretty(cfg).map_err(|e| format!("序列化 scheduler 失败: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("写 scheduler 失败: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename scheduler 失败: {e}"))?;
    Ok(())
}

pub fn gen_task_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("timer-{:x}-{:x}", now.as_secs(), now.subsec_nanos())
}

pub fn interval_secs(spec: &IntervalSpec) -> u64 {
    let mul = match spec.unit.as_str() {
        "sec" => 1u64,
        "min" => 60,
        "hour" => 3600,
        "day" => 86_400,
        _ => 1,
    };
    spec.value.saturating_mul(mul)
}

/// 本地时间分解 → (date "YYYY-MM-DD", hhmm "HH:MM", weekday 0=周日, day, month)。
fn local_parts(ts_ms: u64, off_secs: i64) -> (String, String, u8, u8, u8) {
    let off_ms = (off_secs as i64).saturating_mul(1000) as u64;
    let biased = ts_ms.wrapping_add(off_ms);
    let date = crate::history::date_from_ts(biased);
    let hhmm = crate::history::time_hhmm_from_ts_local(ts_ms, off_secs);
    let day: u8 = date.get(8..10).and_then(|s| s.parse().ok()).unwrap_or(0);
    let month: u8 = date.get(5..7).and_then(|s| s.parse().ok()).unwrap_or(0);
    let days = (biased / 86_400_000) as i64;
    let weekday = ((days.rem_euclid(7) + 4) % 7) as u8; // 1970-01-01=周四=4;0=周日..6=周六
    (date, hhmm, weekday, day, month)
}

/// 投递提示词:构造 source=scheduler 的 UserMessage,经 tx 进主 agent 驱动循环。
pub async fn fire(task: &TimerTask, tx: &tokio::sync::mpsc::Sender<SessionEvent>) {
    // text 自带来源前缀:LLM 推理和用户气泡都直接可见(知道这是定时触发,非实时用户指令)。
    // source=scheduler 另写进 history data,供机器查询/触发历史过滤,与前缀互补。
    let mut text = format!("[定时任务「{}」] {}", task.name, task.action);
    if !task.context_inject.is_empty() {
        text.push_str("\n[可参考] ");
        text.push_str(&task.context_inject.join("; "));
    }
    let _ = tx
        .send(SessionEvent::UserMessage {
            text,
            attachments: vec![],
            source: "scheduler".into(),
        })
        .await;
}

/// task 下次触发需 sleep 的时长。interval=固定间隔;schedule=到下次匹配时点。
fn next_sleep(task: &TimerTask, now_ms: u64, off_secs: i64) -> Duration {
    match task.mode.as_str() {
        "interval" => {
            let secs = task.interval.as_ref().map(interval_secs).unwrap_or(60);
            Duration::from_secs(secs.max(1))
        }
        "schedule" => match &task.schedule {
            Some(s) => Duration::from_millis(
                next_schedule_ms(s, now_ms, off_secs).saturating_sub(now_ms).max(60_000),
            ),
            None => Duration::from_secs(3600),
        },
        _ => Duration::from_secs(3600),
    }
}

/// schedule 下次匹配时点 ts:从 now+1min 逐分钟扫,第一个满足 time+repeat 的返回。
fn next_schedule_ms(spec: &ScheduleSpec, now: u64, off: i64) -> u64 {
    let mut t = (now / 60_000 + 1) * 60_000; // 下一分钟整
    for _ in 0..(366 * 24 * 60 + 10) {
        let (_, hhmm, weekday, day, month) = local_parts(t, off);
        if hhmm == spec.time {
            let ok = match spec.repeat.as_str() {
                "daily" => true,
                "weekly" => spec.weekdays.iter().any(|w| *w == weekday),
                "monthly" => spec.day == day,
                "yearly" => spec.month == month && spec.day == day,
                _ => false,
            };
            if ok { return t; }
        }
        t += 60_000;
    }
    now + 86_400_000 // 兜底(一年内无匹配,不该发生)
}

/// 配置快照(排除 last_fired 运行时状态):用于 diff 判断任务"配置"是否变更。
/// 必须不含 last_fired_ts/date —— 否则每次 fire 更新它们,watcher 会误判"任务改了"
/// → abort 当前 timer + 从头 sleep,把触发间隔从 interval 变成 interval+10s,精度垮。
fn config_snapshot(t: &TimerTask) -> String {
    serde_json::json!({
        "name": t.name, "enabled": t.enabled, "mode": t.mode, "count": t.count,
        "interval": t.interval, "schedule": t.schedule, "action": t.action,
        "context_inject": t.context_inject,
    })
    .to_string()
}

/// per-task 精确 timer 调度器:每个启用任务一个独立 tokio task,sleep 到自己触发点。
pub struct SchedulerRunner {
    /// task_id -> (timer 句柄, 创建时 task 的 json 快照用于 diff)
    handles: Mutex<HashMap<String, (tauri::async_runtime::JoinHandle<()>, String)>>,
    tx: tokio::sync::mpsc::Sender<SessionEvent>,
    cache_dir: PathBuf,
}

impl SchedulerRunner {
    pub fn new(tx: tokio::sync::mpsc::Sender<SessionEvent>, cache_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self { handles: Mutex::new(HashMap::new()), tx, cache_dir })
    }

    /// 启动:初始 reload + 10s watcher(同步 scheduler.json 的配置变更)。
    pub fn start(self: &Arc<Self>) {
        self.reload();
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                me.reload();
            }
        });
    }

    /// diff 同步:删除/禁用/全局关/快照变了的 abort;新增的 spawn。未变的保留(继续 sleep 不打断)。
    /// diff 同步:删除/禁用/全局关/快照变了的 abort;新增的 spawn。未变的保留(继续 sleep 不打断)。
    pub fn reload(self: &Arc<Self>) {
        let cfg = load_from(&self.cache_dir);
        let live: HashMap<String, String> = cfg
            .tasks
            .iter()
            .filter(|t| t.enabled && cfg.global_enabled)
            .map(|t| (t.task_id.clone(), config_snapshot(t)))
            .collect();
        // 1. abort 不再有效的(删除/禁用/全局关/改了)
        let dead: Vec<String> = {
            let handles = self.handles.lock().unwrap();
            handles
                .keys()
                .filter_map(|id| match live.get(id) {
                    None => Some(id.clone()), // 不在 live
                    Some(snap) => handles
                        .get(id)
                        .map(|h| h.1.clone())
                        .map_or(Some(id.clone()), |cur| {
                            if cur != *snap { Some(id.clone()) } else { None }
                        }),
                })
                .collect()
        };
        for id in dead {
            if let Some((h, _)) = self.handles.lock().unwrap().remove(&id) {
                h.abort();
            }
        }
        // 2. spawn 新增(live 里但还没 timer)
        for (id, snap) in &live {
            if !self.handles.lock().unwrap().contains_key(id) {
                if let Some(t) = cfg.tasks.iter().find(|t| &t.task_id == id) {
                    self.spawn_task_timer(t.clone(), snap.clone());
                }
            }
        }
    }

    fn spawn_task_timer(self: &Arc<Self>, task: TimerTask, snap: String) {
        let id = task.task_id.clone();
        let me = self.clone();
        let handle = tauri::async_runtime::spawn(async move {
            loop {
                let now = now_ms();
                let off = crate::history::local_offset_secs();
                let dur = next_sleep(&task, now, off);
                tokio::time::sleep(dur).await;
                // fire 前 reload 确认还活着(可能已被 watcher/命令 abort,但兜底自检)
                let cfg = load_from(&me.cache_dir);
                if !cfg.global_enabled { return; }
                let cur = match cfg.tasks.iter().find(|t| t.task_id == task.task_id) {
                    Some(t) => t.clone(),
                    None => return,
                };
                if !cur.enabled { return; }
                fire(&cur, &me.tx).await;
                // 更新 last_fired + once 停用 + save
                let n = now_ms();
                let mut cfg2 = load_from(&me.cache_dir);
                if let Some(t) = cfg2.tasks.iter_mut().find(|t| t.task_id == task.task_id) {
                    t.last_fired_ts = n;
                    t.last_fired_date = local_parts(n, off).0;
                    if t.count == "once" { t.enabled = false; }
                }
                let _ = save_to(&me.cache_dir, &cfg2);
                if cur.count == "once" { return; }
                // forever 继续 loop(下次 sleep 用 task 快照;若用户改了 interval,watcher 会 abort 重建)
            }
        });
        self.handles.lock().unwrap().insert(id, (handle, snap));
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_secs_units() {
        assert_eq!(interval_secs(&IntervalSpec { value: 30, unit: "sec".into() }), 30);
        assert_eq!(interval_secs(&IntervalSpec { value: 5, unit: "min".into() }), 300);
        assert_eq!(interval_secs(&IntervalSpec { value: 2, unit: "hour".into() }), 7200);
        assert_eq!(interval_secs(&IntervalSpec { value: 1, unit: "day".into() }), 86_400);
        assert_eq!(interval_secs(&IntervalSpec { value: 9, unit: "unknown".into() }), 9);
    }

    fn mk(mode: &str) -> TimerTask {
        TimerTask {
            task_id: "t".into(), name: "n".into(), enabled: true,
            mode: mode.into(), count: "forever".into(),
            interval: None, schedule: None, action: "a".into(),
            context_inject: vec![], last_fired_ts: 0, last_fired_date: String::new(),
        }
    }

    #[test]
    fn next_sleep_interval_30s() {
        // 30s interval → sleep 正好 30s(不被任何 ticker 粒度掐)
        let mut t = mk("interval");
        t.interval = Some(IntervalSpec { value: 30, unit: "sec".into() });
        assert_eq!(next_sleep(&t, 1_000_000, 0), Duration::from_secs(30));
    }

    #[test]
    fn next_sleep_interval_zero_floor_to_1s() {
        let mut t = mk("interval");
        t.interval = Some(IntervalSpec { value: 0, unit: "sec".into() });
        assert_eq!(next_sleep(&t, 0, 0), Duration::from_secs(1)); // 防 0 死循环
    }

    #[test]
    fn next_schedule_daily_today_remaining() {
        // now=09:00(UTC,off=0),daily 09:05 → 下次是今天 09:05(5 分钟后)
        let now = (20637 * 86_400 + 9 * 3600) * 1000; // 当日 09:00:00
        let spec = ScheduleSpec { time: "09:05".into(), repeat: "daily".into(), weekdays: vec![], day: 0, month: 0 };
        let target = next_schedule_ms(&spec, now, 0);
        assert_eq!(target, (20637 * 86_400 + 9 * 3600 + 5 * 60) * 1000);
    }

    #[test]
    fn next_schedule_daily_already_passed_today() {
        // now=09:10,daily 09:05 → 今天已过,下次明天 09:05
        let now = (20637 * 86_400 + 9 * 3600 + 10 * 60) * 1000; // 09:10
        let spec = ScheduleSpec { time: "09:05".into(), repeat: "daily".into(), weekdays: vec![], day: 0, month: 0 };
        let target = next_schedule_ms(&spec, now, 0);
        assert_eq!(target, ((20637 + 1) * 86_400 + 9 * 3600 + 5 * 60) * 1000); // 明天 09:05
    }

    #[test]
    fn next_schedule_weekday_filter() {
        // daily 09:05 但 repeat=weekly weekday=[1](周一),now=2026-08-03(周一)09:00 → 今天
        let now = (20637 * 86_400 + 9 * 3600) * 1000; // 2026-08-03 周一 09:00
        let spec = ScheduleSpec { time: "09:05".into(), repeat: "weekly".into(), weekdays: vec![1], day: 0, month: 0 };
        let target = next_schedule_ms(&spec, now, 0);
        // 2026-08-03 是周一(weekday=1)?验证:见 date_from_ts;若不是周一则跳到下个周一
        let (_, _, wd, _, _) = local_parts(target, 0);
        assert_eq!(wd, 1, "target 应落在周一");
    }

    #[test]
    fn config_snapshot_excludes_last_fired() {
        // last_fired 是运行时状态,变更不应让 watcher 误判"配置改了"→ 否则 fire 后被打断重置
        let mut t = mk("interval");
        t.interval = Some(IntervalSpec { value: 30, unit: "sec".into() });
        t.last_fired_ts = 100;
        let s1 = config_snapshot(&t);
        t.last_fired_ts = 999_999_999;
        let s2 = config_snapshot(&t);
        assert_eq!(s1, s2, "last_fired 变化不应改变 config snapshot");
        assert!(!s1.contains("last_fired"), "snapshot 不应含 last_fired");
    }

    #[test]
    fn config_snapshot_detects_interval_change() {
        let mut t = mk("interval");
        t.interval = Some(IntervalSpec { value: 30, unit: "sec".into() });
        let s1 = config_snapshot(&t);
        t.interval = Some(IntervalSpec { value: 60, unit: "sec".into() });
        let s2 = config_snapshot(&t);
        assert_ne!(s1, s2, "interval 变了 snapshot 应不同 → watcher 会 abort 重建");
    }

    #[test]
    fn gen_task_id_shape() {
        let id = gen_task_id();
        assert!(id.starts_with("timer-"));
        assert!(id.len() > "timer-".len() + 2);
    }

    #[test]
    fn scheduler_config_default_empty() {
        let c = SchedulerConfig::default();
        assert!(!c.global_enabled);
        assert!(c.tasks.is_empty());
    }

    #[test]
    fn scheduler_json_roundtrip() {
        let mut c = SchedulerConfig::default();
        c.global_enabled = true;
        let mut t = mk("interval");
        t.interval = Some(IntervalSpec { value: 5, unit: "min".into() });
        t.last_fired_ts = 123;
        c.tasks.push(t);
        let s = serde_json::to_string(&c).unwrap();
        let back: SchedulerConfig = serde_json::from_str(&s).unwrap();
        assert!(back.global_enabled);
        assert_eq!(back.tasks.len(), 1);
        assert_eq!(back.tasks[0].last_fired_ts, 123);
    }
}
