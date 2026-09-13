//! v2 上下文管理：history 是唯一真相之源（append-only jsonl）。
//! 单写不变量（P1 铁律①）：三个并发生产者（主 driver / 子代理 / dream marker）都经同一 writer task +
//! 同一 mpsc 落盘；writer 串行分配 seq、独占文件句柄、跨天切文件。并发 append 才不交错损坏行。
use crate::AttachmentRef;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// 一条 history 事件。seq 由 writer 盖戳（producer 发出时 seq=0）；ts=epoch ms（producer 盖）。
/// kind 决定 data 字段（见模块文档）。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HistoryEvent {
    pub seq: u64,
    pub ts: u64,
    pub thread: String, // "main" | "agent:{id}"
    pub kind: String,
    #[serde(flatten)]
    pub data: Map<String, Value>,
}

impl HistoryEvent {
    pub fn user(ts: u64, thread: &str, text: &str, attachments: &[AttachmentRef]) -> Self {
        let mut d = Map::new();
        d.insert("text".into(), json!(text));
        d.insert("attachments".into(), json!(attachments));
        Self { seq: 0, ts, thread: thread.into(), kind: "user".into(), data: d }
    }
    pub fn assistant(ts: u64, thread: &str, content: &str, thinking: &str, tool_calls: Vec<Value>) -> Self {
        let mut d = Map::new();
        d.insert("content".into(), json!(content));
        d.insert("thinking".into(), json!(thinking));
        d.insert("tool_calls".into(), Value::Array(tool_calls));
        Self { seq: 0, ts, thread: thread.into(), kind: "assistant".into(), data: d }
    }
    /// assistant() + 可选 usage（MiniMax 返回的 token 用量，整个 Value 原样进 data.usage）。
    /// 旧 assistant() 不动（context.rs/mem_cli.rs 测试零改）；run_turn 落盘切到本 helper。
    pub fn assistant_with_usage(
        ts: u64, thread: &str, content: &str, thinking: &str,
        tool_calls: Vec<Value>, usage: Option<Value>,
    ) -> Self {
        let mut ev = Self::assistant(ts, thread, content, thinking, tool_calls);
        if let Some(u) = usage {
            if !u.is_null() {
                ev.data.insert("usage".into(), u);
            }
        }
        ev
    }
    pub fn tool_result(ts: u64, thread: &str, name: &str, result: &str, call_id: &str) -> Self {
        let mut d = Map::new();
        d.insert("name".into(), json!(name));
        d.insert("result".into(), json!(result));
        d.insert("call_id".into(), json!(call_id));
        Self { seq: 0, ts, thread: thread.into(), kind: "tool_result".into(), data: d }
    }
    pub fn external(ts: u64, what: &str, path: &str, hash: Option<&str>) -> Self {
        let mut d = Map::new();
        d.insert("what".into(), json!(what));
        d.insert("path".into(), json!(path));
        if let Some(h) = hash { d.insert("hash".into(), json!(h)); }
        Self { seq: 0, ts, thread: "main".into(), kind: "external".into(), data: d }
    }
    /// 子代理结算事件。summary 落盘永远全文（护栏只在 build_messages 渲染层，见 context.rs）；
    /// ok=结算三态（完成/失败/取消均由 spawn 结算块判定）。存量 jsonl 事件缺 ok → 渲染按完成处理。
    pub fn subagent_result(ts: u64, agent_id: &str, summary: &str, refs: &str, ok: bool) -> Self {
        let mut d = Map::new();
        d.insert("agent_id".into(), json!(agent_id));
        d.insert("summary".into(), json!(summary));
        d.insert("ref".into(), json!(refs));
        d.insert("ok".into(), json!(ok));
        Self { seq: 0, ts, thread: "main".into(), kind: "subagent_result".into(), data: d }
    }
    pub fn edit(ts: u64, path: &str, sha_before: Option<&str>, sha_after: Option<&str>) -> Self {
        let mut d = Map::new();
        d.insert("path".into(), json!(path));
        if let Some(s) = sha_before { d.insert("sha_before".into(), json!(s)); }
        if let Some(s) = sha_after { d.insert("sha_after".into(), json!(s)); }
        Self { seq: 0, ts, thread: "main".into(), kind: "edit".into(), data: d }
    }
    pub fn marker(ts: u64, which: &str, until_seq: u64) -> Self {
        let mut d = Map::new();
        d.insert("marker".into(), json!(which)); // "dream" | "reset"
        d.insert("until_seq".into(), json!(until_seq));
        Self { seq: 0, ts, thread: "main".into(), kind: "marker".into(), data: d }
    }
}

/// epoch ms → YYYY-MM-DD（UTC；确定性，跨机一致；测试可精确断言）。memory.rs 复用。
pub fn date_from_ts(ts: u64) -> String {
    let days = (ts / 86_400_000) as i64; // 天数（自 1970-01-01）
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// epoch ms + 本地偏移(秒) → YYYY-MM-DD（本地）。memory 标题/文件路径 + history jsonl 选文件用它；
/// UTC 底层 date_from_ts 保留供测试确定性。偏移由调用方传 local_offset_secs()（dream / spawn_writer 入口）。
pub fn date_from_ts_local(ts: u64, offset_secs: i64) -> String {
    date_from_ts(saturating_add_offset_ms(ts, offset_secs))
}

/// epoch ms + 本地偏移(秒) → HH:MM（本地）。dream 写 memory 标题时间用它（用户本地可读）。
pub fn time_hhmm_from_ts_local(ts: u64, offset_secs: i64) -> String {
    let biased = saturating_add_offset_ms(ts, offset_secs);
    let secs = biased / 1000;
    let in_day = secs % 86400;
    let h = in_day / 3600;
    let m = (in_day % 3600) / 60;
    format!("{h:02}:{m:02}")
}

/// ts + 偏移，负值截到 0（避免 epoch 前下溢）。
fn saturating_add_offset_ms(ts: u64, offset_secs: i64) -> u64 {
    (ts as i64 + offset_secs * 1000).max(0) as u64
}

/// 本地时区相对 UTC 的偏移（秒）。CST 返 +28800。Windows 用 GetTimeZoneInformation（含夏令时 bias）；
/// 非 Windows 暂返 0（本项目 Windows 优先）。只读查询系统时区，不写注册表/系统配置（绿色）。
#[cfg(target_os = "windows")]
pub fn local_offset_secs() -> i64 {
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    unsafe {
        let mut tzi: TIME_ZONE_INFORMATION = std::mem::zeroed();
        let r = GetTimeZoneInformation(&mut tzi);
        // Win32 Bias 定义 = UTC − local（分钟）；本地相对 UTC = −Bias。
        let base = -(tzi.Bias as i64);
        let dyn_bias = match r {
            2 => -(tzi.DaylightBias as i64), // TIME_ZONE_ID_DAYLIGHT：夏令时生效
            1 => -(tzi.StandardBias as i64), // TIME_ZONE_ID_STANDARD
            _ => 0,
        };
        (base + dyn_bias) * 60
    }
}
#[cfg(not(target_os = "windows"))]
pub fn local_offset_secs() -> i64 { 0 }

/// Howard Hinnant civil_from_days：天数 → (年, 月, 日)，纯算术无外部依赖。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// Producer 句柄：clone 进 ToolsCtx，所有 append 经此投递到单写 task。
#[derive(Clone)]
pub struct HistoryWriterHandle {
    pub tx: mpsc::UnboundedSender<HistoryEvent>,
    current_seq: Arc<AtomicU64>,
}

impl HistoryWriterHandle {
    /// 不落盘的句柄（foreground/测试用：send 到无人接收的 unbounded channel，事件丢弃）。
    /// run_loop 过渡期 + 未接 history 单写的 ctx 构造点用它；run_turn 在它上面 append 是 no-op。
    pub fn noop() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel::<HistoryEvent>();
        Self { tx, current_seq: Arc::new(AtomicU64::new(0)) }
    }
    /// 投递一条事件（seq 由 writer 盖戳）。无界 channel：写盘慢时暂存，进程崩了才丢（§13.1）。
    pub fn append(&self, ev: HistoryEvent) {
        let _ = self.tx.send(ev);
    }
    /// 下一个将分配的 seq = 已落盘事件数。dream 在 dream-start 读它作 until_seq 边界（§7.1）。
    pub fn current_seq(&self) -> u64 {
        self.current_seq.load(Ordering::SeqCst)
    }
}

/// 启动单写 task：独占 history/ 文件句柄，串行 recv → 盖 seq → 按 ts 选日期文件 → append 一行。
/// 跨天自动开新文件（append 模式，永不覆盖）。
///
/// **前置条件（C2 regression guard）：必须在 Tokio runtime 上下文里调用**——本函数内部
/// 用 `tokio::spawn` 拉起单写 task，而 `tokio::spawn` 需要 thread-local runtime guard。
/// app 中由 `agent::spawn_session` 在 `tauri::async_runtime::spawn(async move { ... })` 异步块
/// 首行调用（async_runtime::spawn 把任务挂到 Tauri 的 runtime，await 后即在 runtime 上）；
/// 单元测试用 `#[tokio::test]` 自带 runtime，故也可直接调。**禁止**从同步主线程裸调（panic：
/// "there is no reactor running"）。
pub fn spawn_writer(history_dir: PathBuf, offset_secs: i64) -> HistoryWriterHandle {
    let (tx, mut rx) = mpsc::unbounded_channel::<HistoryEvent>();
    let _ = std::fs::create_dir_all(&history_dir);
    // 跨重启续 seq：从既有 history 最大 seq +1 起。若重启后 seq 归 0，新事件会与旧事件/marker
    // 撞车（read_all 按 seq 排序错乱），且新事件 seq 落在旧 marker 之前会被重建窗口
    // （seq > last_marker）排除 → rebuild 只剩 system → MiniMax 400 "chat content is empty"。
    // 必须跨重启单调续 seq。
    let seed: u64 = read_all(&history_dir).iter().map(|e| e.seq).max().map(|m| m + 1).unwrap_or(0);
    let current_seq = Arc::new(AtomicU64::new(seed));
    let handle = HistoryWriterHandle { tx, current_seq: current_seq.clone() };
    tokio::spawn(async move {
        let mut open: Option<(String, std::fs::File)> = None; // (date, append handle)
        while let Some(mut ev) = rx.recv().await {
            let seq = current_seq.fetch_add(1, Ordering::SeqCst); // 0-based 单调
            ev.seq = seq;
            let date = date_from_ts_local(ev.ts, offset_secs);
            let need_new = match &open {
                Some((d, _)) => d != &date,
                None => true,
            };
            if need_new {
                match std::fs::OpenOptions::new()
                    .create(true).append(true)
                    .open(history_dir.join(format!("{date}.jsonl")))
                {
                    Ok(f) => open = Some((date.clone(), f)),
                    Err(e) => {
                        eprintln!("[history] 打开 {date}.jsonl 失败: {e}（丢 seq={seq}，上游无界 channel 已暂存其余）");
                        continue;
                    }
                }
            }
            if let Some((_, f)) = open.as_mut() {
                use std::io::Write;
                let line = serde_json::to_string(&ev).unwrap_or_default();
                if let Err(e) = writeln!(f, "{line}") {
                    // §13.1：写失败 warn + 继续读下一事件（不阻塞 producer、不死 writer）
                    eprintln!("[history] 写失败 seq={seq}: {e}");
                }
            }
        }
    });
    handle
}

/// 读 history/ 全部事件，按 seq 升序。逐行解析，跳过坏行（§13.1）。
pub fn read_all(history_dir: &Path) -> Vec<HistoryEvent> {
    let mut evs = Vec::new();
    let mut files: Vec<PathBuf> = match std::fs::read_dir(history_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .collect(),
        Err(_) => return evs,
    };
    files.sort();
    for f in files {
        let text = match std::fs::read_to_string(&f) { Ok(s) => s, Err(_) => continue };
        for line in text.lines() {
            if line.trim().is_empty() { continue; }
            match serde_json::from_str::<HistoryEvent>(line) {
                Ok(ev) => evs.push(ev),
                Err(e) => eprintln!("[history] 跳过坏行 {f:?}: {e}"),
            }
        }
    }
    evs.sort_by_key(|e| e.seq);
    evs
}

/// display 滑动窗口：最近 limit 条可见事件（main+非marker），before_seq 之前（向上翻）。升序返回。
pub fn visible_tail(evs: &[HistoryEvent], limit: u64, before_seq: Option<u64>) -> Vec<HistoryEvent> {
    let mut v: Vec<&HistoryEvent> = evs.iter().filter(|e| e.thread == "main" && e.kind != "marker").collect();
    v.sort_by_key(|e| e.seq);
    let cut = match before_seq { Some(s) => v.iter().take_while(|e| e.seq < s).count(), None => v.len() };
    let start = cut.saturating_sub(limit as usize);
    v[start..cut].iter().map(|e| (*e).clone()).collect()
}
/// display 滑动窗口：after_seq 之后的 limit 条可见事件（向下翻）。升序返回。
pub fn visible_head(evs: &[HistoryEvent], limit: u64, after_seq: Option<u64>) -> Vec<HistoryEvent> {
    let mut v: Vec<&HistoryEvent> = evs.iter().filter(|e| e.thread == "main" && e.kind != "marker").collect();
    v.sort_by_key(|e| e.seq);
    let start = match after_seq { Some(s) => v.iter().position(|e| e.seq > s).unwrap_or(v.len()), None => 0 };
    let end = (start + limit as usize).min(v.len());
    v[start..end].iter().map(|e| (*e).clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn flush() { tokio::time::sleep(Duration::from_millis(120)).await; }

    #[test]
    fn date_from_ts_epoch() {
        assert_eq!(date_from_ts(0), "1970-01-01");
    }
    #[test]
    fn date_from_ts_known() {
        // 2026-07-26 00:00:00 UTC = 1785024000 s = 1785024000000 ms
        assert_eq!(date_from_ts(1_785_024_000_000), "2026-07-26");
    }
    #[test]
    fn date_from_ts_local_shifts_day_at_boundary() {
        // UTC 底层不变；+8h 偏移让 UTC 23:00 跨天到次日 07:00
        let utc_2300 = 1_785_024_000_000 + 23 * 3600_000; // 2026-07-26 23:00 UTC
        assert_eq!(date_from_ts(utc_2300), "2026-07-26", "UTC 底层不变");
        assert_eq!(date_from_ts_local(utc_2300, 0), "2026-07-26", "+0 等价 UTC");
        assert_eq!(date_from_ts_local(utc_2300, 28800), "2026-07-27", "+8h 跨天到次日");
    }
    #[test]
    fn time_hhmm_from_ts_local_shifts_hour() {
        // UTC 00:00 + 8h = 08:00（本地）
        assert_eq!(time_hhmm_from_ts_local(1_785_024_000_000, 0), "00:00");
        assert_eq!(time_hhmm_from_ts_local(1_785_024_000_000, 28800), "08:00");
    }
    #[test]
    fn local_offset_secs_in_plausible_range() {
        let o = local_offset_secs();
        assert!((-43200..=50400).contains(&o), "本地偏移应在 -12h..+14h 内: {o}");
    }

    #[test]
    fn event_user_roundtrip() {
        let ev = HistoryEvent::user(1000, "main", "你好", &[]);
        let line = serde_json::to_string(&ev).unwrap();
        let back: HistoryEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(back.kind, "user");
        assert_eq!(back.thread, "main");
        assert_eq!(back.data["text"], serde_json::json!("你好"));
    }
    #[test]
    fn event_marker_roundtrip() {
        let ev = HistoryEvent::marker(2000, "dream", 42);
        let line = serde_json::to_string(&ev).unwrap();
        let back: HistoryEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(back.kind, "marker");
        assert_eq!(back.data["marker"], serde_json::json!("dream"));
        assert_eq!(back.data["until_seq"], serde_json::json!(42));
    }

    #[tokio::test]
    async fn writer_appends_monotonic_seq() {
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"), 0);
        h.append(HistoryEvent::user(1000, "main", "a", &[]));
        h.append(HistoryEvent::user(2000, "main", "b", &[]));
        h.append(HistoryEvent::user(3000, "main", "c", &[]));
        flush().await;
        let evs = read_all(&dir.path().join("history"));
        assert_eq!(evs.len(), 3);
        assert_eq!(evs[0].seq, 0);
        assert_eq!(evs[1].seq, 1);
        assert_eq!(evs[2].seq, 2);
        assert_eq!(h.current_seq(), 3);
    }

    #[tokio::test]
    async fn writer_writes_dated_file_and_switches_day() {
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"), 0);
        // 同一天两条
        h.append(HistoryEvent::user(1_785_024_000_000, "main", "d1a", &[]));
        h.append(HistoryEvent::user(1_785_024_001_000, "main", "d1b", &[]));
        // 次日一条
        h.append(HistoryEvent::user(1_785_024_000_000 + 86_400_000, "main", "d2", &[]));
        flush().await;
        assert!(dir.path().join("history/2026-07-26.jsonl").exists());
        assert!(dir.path().join("history/2026-07-27.jsonl").exists());
        let evs = read_all(&dir.path().join("history"));
        assert_eq!(evs.len(), 3, "跨天合并读取");
    }

    #[tokio::test]
    async fn writer_append_only_grows_not_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"), 0);
        h.append(HistoryEvent::user(1000, "main", "x", &[]));
        flush().await;
        assert_eq!(read_all(&dir.path().join("history")).len(), 1);
        h.append(HistoryEvent::user(2000, "main", "y", &[]));
        flush().await;
        assert_eq!(read_all(&dir.path().join("history")).len(), 2, "append-only：文件应增长非覆盖");
    }

    #[tokio::test]
    async fn writer_multi_producer_seq_monotonic_no_corrupt() {
        // P1 铁律①：多生产者并发 → seq 单调连续 + 无交错坏行
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"), 0);
        let mut handles = vec![];
        for t in 0..4u64 {
            let h2 = h.clone();
            handles.push(tokio::spawn(async move {
                for i in 0..25u64 {
                    h2.append(HistoryEvent::user(1000 + t * 100 + i, "main", &format!("t{t}-{i}"), &[]));
                }
            }));
        }
        for hd in handles { let _ = hd.await; }
        flush().await;
        let evs = read_all(&dir.path().join("history"));
        assert_eq!(evs.len(), 100, "4×25 全部落盘");
        let seqs: Vec<u64> = evs.iter().map(|e| e.seq).collect();
        let mut sorted = seqs.clone(); sorted.sort();
        assert_eq!(sorted, (0..100u64).collect::<Vec<_>>(), "seq 应 0..99 无缺无重");
        // 每行都可解析（read_all 已跳坏行；再验文件直读无垃圾）
        let raw = std::fs::read_to_string(dir.path().join("history").join(format!("{}.jsonl", date_from_ts(1000)))).unwrap();
        for line in raw.lines() {
            assert!(serde_json::from_str::<HistoryEvent>(line).is_ok(), "坏行: {line}");
        }
    }

    #[tokio::test]
    async fn read_all_skips_bad_lines() {
        let dir = tempfile::tempdir().unwrap();
        let hist = dir.path().join("history");
        std::fs::create_dir_all(&hist).unwrap();
        let f = hist.join(format!("{}.jsonl", date_from_ts(1000)));
        let good1 = serde_json::to_string(&HistoryEvent::user(1000, "main", "a", &[])).unwrap();
        let good2 = serde_json::to_string(&HistoryEvent::user(2000, "main", "b", &[])).unwrap();
        std::fs::write(&f, format!("{good1}\nTHIS IS GARBAGE\n{good2}\n")).unwrap();
        let evs = read_all(&hist);
        assert_eq!(evs.len(), 2, "坏行应跳过");
    }

    #[tokio::test]
    async fn spawn_writer_seeds_seq_from_existing_history() {
        // 重启续 seq：进程退出再起，新 writer 必须从既有 max seq+1 续编，不能归 0
        // （归 0 会与旧事件/marker 撞车，致 read_all 排序错乱 + 重建窗口排除新事件 → 空消息 400）
        let dir = tempfile::tempdir().unwrap();
        let hd = dir.path().join("history");
        // session 1：写 3 条 → seq 0,1,2
        let h1 = spawn_writer(hd.clone(), 0);
        h1.append(HistoryEvent::user(1000, "main", "a", &[]));
        h1.append(HistoryEvent::user(2000, "main", "b", &[]));
        h1.append(HistoryEvent::user(3000, "main", "c", &[]));
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        drop(h1); // 模拟进程退出：唯一 tx drop → writer task 的 rx 关闭 → task 结束
        // session 2：新 writer 应续 seq（新事件得 seq 3），不归 0
        let h2 = spawn_writer(hd.clone(), 0);
        assert_eq!(h2.current_seq(), 3, "新 writer 应 seed 到 max+1=3");
        h2.append(HistoryEvent::user(4000, "main", "d", &[]));
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let seqs: Vec<u64> = read_all(&hd).iter().map(|e| e.seq).collect();
        let mut sorted = seqs.clone(); sorted.sort();
        assert_eq!(sorted, vec![0, 1, 2, 3], "重启后续 seq，无撞车：{seqs:?}");
    }

    fn ev(seq: u64, kind: &str, thread: &str) -> HistoryEvent {
        let mut e = HistoryEvent::user(seq * 1000, thread, &format!("u{seq}"), &[]); e.seq = seq;
        e.kind = kind.into(); e
    }
    #[test]
    fn visible_tail_returns_newest_before_cursor() {
        let evs = vec![ev(0,"user","main"), ev(1,"assistant","main"), ev(2,"user","main"),
            ev(3,"marker","main"), ev(4,"user","main"), ev(5,"assistant","agent:1")];
        // limit=2, before_seq=None → 最近 2 条可见（main+非marker）：seq 2 与 4（3 是 marker 隐藏，5 是 agent:N 隐藏）
        let t = visible_tail(&evs, 2, None);
        let seqs: Vec<u64> = t.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![2, 4], "最近 2 条可见事件（升序），marker/agent:N 跳过");
    }
    #[test]
    fn visible_tail_before_cursor_paginates_older() {
        let evs = (0..10).map(|i| ev(i, "user", "main")).collect::<Vec<_>>();
        let t = visible_tail(&evs, 3, Some(7)); // 7 之前的 3 条可见 → 4,5,6
        let seqs: Vec<u64> = t.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![4, 5, 6]);
    }
    #[test]
    fn visible_head_returns_oldest_after_cursor() {
        let evs = (0..10).map(|i| ev(i, "user", "main")).collect::<Vec<_>>();
        let h = visible_head(&evs, 3, Some(4)); // 4 之后的 3 条 → 5,6,7
        let seqs: Vec<u64> = h.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![5, 6, 7]);
    }
}
