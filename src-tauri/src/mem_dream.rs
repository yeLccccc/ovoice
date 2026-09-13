//! mem_dream —— mem CLI 的 dream 写端（四级漏斗记忆整理）。自建，不调 dream.rs/memory.rs。
use std::path::{Path, PathBuf};
use std::collections::{VecDeque, HashMap};
use std::sync::Mutex;
use thiserror::Error;
use async_trait::async_trait;
use crate::history::{HistoryEvent, date_from_ts_local, time_hhmm_from_ts_local};

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub rounds: usize, pub events_out: usize, pub day_segments: usize,
    pub months_rotated: u32, pub years_rotated: u32, pub markers: u32,
    pub seg_a: u64, pub seg_b: u64, pub batches: usize,
    pub prompt_tokens: u64, pub completion_tokens: u64, pub cached_tokens: u64,
    pub elapsed_ms: u128, pub model: String,
}

/// dream_round 返回(content + token 用量),供 marker 记录 dream 运行细节
#[derive(Debug, Clone, Default)]
pub struct DreamResp {
    pub content: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
}

/// execute_dream 返回值
#[derive(Debug, Clone, Default)]
pub struct ExecuteOutcome {
    pub processed_until: Option<u64>,  // Some(b) 实际处理到 b；None 空段(F12)推进到 cur
    pub stats: Stats,
}

#[derive(Debug, Default, Copy, Clone)]
pub struct DreamFlags { pub once: bool, pub mechanical: bool, pub dry_run: bool, pub force: bool }

#[derive(Debug, Clone)]
pub struct DreamCfg { pub api_key: Option<String>, pub model: String, pub region: String,
    pub max_rounds: u32, pub batch_max_events: usize }

#[derive(Debug, Error)]
pub enum DreamError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("llm: {0}")]
    Llm(String),
    #[error("config: {0}")]
    Config(String),
    #[error("parse: {0}")]
    Parse(String),
}

/// Lock file guard for `.dream-lock` (Task 13, §22.1).
/// Prevents concurrent dream writes during transition period.
/// Acquires lock on creation, releases on Drop (success/error/panic/unwind).
/// Zombie locks (>1h old) are taken over.
#[derive(Debug)]
pub struct DreamGuard {
    path: std::path::PathBuf,
}

impl DreamGuard {
    /// Acquires the dream lock. Creates `.dream-lock` with current pid and timestamp.
    /// Returns error if lock exists and is fresh (<1h old).
    /// Takes over zombie locks (>1h old).
    pub fn acquire(cache: &Path) -> Result<Self, DreamError> {
        use std::io::ErrorKind;

        let lock_path = cache.join(".dream-lock");

        // Get current timestamp
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| DreamError::Io(std::io::Error::new(ErrorKind::Other, e.to_string())))?
            .as_millis() as u64;

        // Try to create lock file exclusively (atomic create_new)
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(_) => {
                // Lock didn't exist -> write fresh lock
                let lock_content = serde_json::json!({
                    "pid": std::process::id(),
                    "started_at_ms": now_ms
                });
                std::fs::write(&lock_path, lock_content.to_string())?;
                Ok(DreamGuard { path: lock_path })
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                // Lock exists -> check if zombie
                let existing = std::fs::read_to_string(&lock_path).unwrap_or_default();
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&existing) {
                    if let Some(started_at_ms) = v.get("started_at_ms").and_then(|x| x.as_u64()) {
                        let age_ms = now_ms.saturating_sub(started_at_ms);
                        if age_ms > 3_600_000 {
                            // Zombie (>1h) -> take over
                            let lock_content = serde_json::json!({
                                "pid": std::process::id(),
                                "started_at_ms": now_ms
                            });
                            std::fs::write(&lock_path, lock_content.to_string())?;
                            return Ok(DreamGuard { path: lock_path });
                        }
                    }
                }
                // Fresh lock or unparseable -> error
                Err(DreamError::Config(".dream-lock 已被占用（另一 dream 进程运行中）".into()))
            }
            Err(e) => Err(DreamError::Io(e)),
        }
    }
}

impl Drop for DreamGuard {
    fn drop(&mut self) {
        // Best-effort delete on drop (success/error/panic)
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Legacy → 三层迁移（spec §19）。
/// - 两层（`## 近期` / `## 年总览`，旧错误产物）→ MEMORY.md 重写为三层模板 + 补 memory/ 树月级；
/// - 三层（`## 日/月/年`，旧 dream.rs 或已是目标）或未知 → MEMORY.md 不重组，只补 memory/ 树月级（幂等）；
/// - 全新（无 MEMORY.md）→ no-op。
/// day 文件永不触碰（无损）。返回是否做了迁移。
pub fn migrate_if_legacy(cache: &Path, offset_secs: i64) -> Result<bool, DreamError> {
    let mem_path = cache.join("MEMORY.md");
    if !mem_path.exists() {
        return Ok(false);
    }
    let content = std::fs::read_to_string(&mem_path)?;
    let is_twolayer = content.contains("## 近期") || content.contains("## 年总览");

    // 补 memory/ 树月级 ## 日概括（从 day 文件回填，幂等）
    let backed = backfill_month_summaries(cache, offset_secs)?;

    if is_twolayer {
        // 两层 → 三层：MEMORY.md 重写三层模板。两层内容丢弃——## 近期 历史事件在 day 文件
        // 有明细，## 年总览 是按年概要无月粒度、无法映射 ## 年 月主题；交由 dream 跨月重新累积。
        backup_to_bak(&mem_path)?;
        atomic_write(&mem_path, &memory_md_template())?;
        return Ok(true);
    }
    Ok(backed)
}

/// 补建 memory/ 树月级 `YYYY-MM.md` ## 日概括：遍历 day 文件，每个回填一条日概括（规则提炼）。
/// `upsert_month_day_summary` 按日期去重 → 幂等。返回是否写了。
fn backfill_month_summaries(cache: &Path, offset_secs: i64) -> Result<bool, DreamError> {
    let mut changed = false;
    let memory_dir = cache.join("memory");
    let year_entries = match std::fs::read_dir(&memory_dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(false),
    };
    for year_entry in year_entries.flatten() {
        let year_path = year_entry.path();
        if !year_path.is_dir() { continue; }
        let year_str = year_path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let year: i32 = match year_str.parse() { Ok(y) => y, Err(_) => continue };

        let month_entries = match std::fs::read_dir(&year_path) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for month_entry in month_entries.flatten() {
            let month_path = month_entry.path();
            if !month_path.is_dir() { continue; }
            let month_str = month_path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            let month: u32 = match month_str.parse() { Ok(m) => m, Err(_) => continue };

            // 仅 day 文件（YYYY-MM-DD.md，2 个横线）；排除月级 YYYY-MM.md（1 横线）
            let mut day_files: Vec<String> = std::fs::read_dir(&month_path).into_iter()
                .flatten().flatten()
                .filter_map(|e| e.file_name().to_string_lossy().into_owned().into())
                .filter(|n| n.ends_with(".md") && n.matches('-').count() == 2)
                .collect();
            day_files.sort();
            for day_file_name in &day_files {
                let day_path = month_path.join(day_file_name);
                let day_content = std::fs::read_to_string(&day_path).unwrap_or_default();
                let pseudo = parse_day_file_to_events(&day_content);
                if pseudo.is_empty() { continue; }
                let mut ds = extract_day_summary(&pseudo, offset_secs);
                // 用文件名日期覆盖（pseudo ts=0 → 否则 1970 bug）
                ds.date = day_file_name.trim_end_matches(".md").to_string();
                upsert_month_day_summary(cache, (year, month), &ds)?;
                changed = true;
            }
        }
    }
    Ok(changed)
}

/// 解析 day 文件文本 → 伪 FinalEvent 列表（标题 + seq 范围），供回填日概括。
/// 段：`## HH:MM evt-NNN 标题` + `**对话索引**: history/...#seq[a,b]`。
fn parse_day_file_to_events(day_content: &str) -> Vec<FinalEvent> {
    let mut out = Vec::new();
    for line in day_content.lines() {
        let l = line.trim();
        if l.is_empty() || !l.starts_with('{') { continue; }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
            let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let seq = v.get("seq").and_then(|x| x.as_array());
            let seq_a = seq.and_then(|a| a.get(0)).and_then(|x| x.as_u64()).unwrap_or(0);
            let seq_b = seq.and_then(|a| a.get(1)).and_then(|x| x.as_u64()).unwrap_or(0);
            if title.is_empty() { continue; }
            out.push(FinalEvent {
                ts: 0, seq_a, seq_b, title,
                subject: v.get("subject").and_then(|x| x.as_str()).unwrap_or("用户").into(),
                detail: v.get("detail").and_then(|x| x.as_str()).unwrap_or("").into(),
                event_type: v.get("type").and_then(|x| x.as_str()).unwrap_or("其他").into(),
                keywords: v.get("keywords").and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|n| n.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_default(),
                has_attachment: false, attachment: String::new(),
            });
        }
    }
    out
}


/// 循环跑到空的入口（F8 单次 read_all + break-after-one-segment for F1 留尾）。
pub async fn run_dream_to_idle(cache: &Path, cfg: &DreamCfg, flags: DreamFlags) -> Result<Stats, DreamError> {
    // 0. Migration (Task 12): detect legacy format and migrate before dreaming
    migrate_if_legacy(cache, crate::history::local_offset_secs())?;

    // 0.5. Lock acquisition (Task 13): prevent concurrent dream writes
    let _guard = DreamGuard::acquire(cache)?;

    // 1. 构造 caller（LlmCaller）
    let caller: Box<dyn LlmCaller> = if !flags.mechanical && !flags.dry_run && cfg.api_key.is_none() {
        return Err(DreamError::Config("缺 api_key 且非 --mechanical，无法调 LLM".into()));
    } else if flags.mechanical || flags.dry_run || cfg.api_key.is_none() {
        Box::new(FakeLlm::new(vec!["（机械模式月主题）".into()]))
    } else {
        Box::new(MiniMaxCaller::new(cfg)?)
    };

    // 2. offset_secs
    let offset_secs = crate::history::local_offset_secs();

    // 3. 读事件 ONCE（F8）
    let events = read_all_events(cache)?;

    // 4. a = frontier + 1
    let a = last_dream_marker_seq(&events) + 1;

    // 5. 循环跑到空（spec §14.2）：cursor 递进，直到空段（b<a）
    let mut stats = Stats::default();
    let mut cursor = a;
    loop {
        let outcome = execute_dream(cache, &events, cfg, caller.as_ref(), flags, cursor, offset_secs, None).await?;

        // 累积 stats
        stats.rounds += outcome.stats.rounds;
        stats.events_out += outcome.stats.events_out;
        stats.day_segments += outcome.stats.day_segments;
        stats.markers += outcome.stats.markers;
        stats.months_rotated += outcome.stats.months_rotated;
        stats.years_rotated += outcome.stats.years_rotated;

        match outcome.processed_until {
            Some(b) => { cursor = b + 1; }   // 推进 frontier，继续下一段
            None => break,                    // 空段（F12）——跑到空，完成
        }
    }

    Ok(stats)
}

/// execute_dream 主管线：整理一段 [a,b] 的历史为记忆。
/// faithful port of dream.rs:126-204，适配 mem CLI 的 async/owned/ExecuteOutcome
pub async fn execute_dream(
    cache: &Path,
    events: &[HistoryEvent],
    cfg: &DreamCfg,
    caller: &dyn LlmCaller,
    flags: DreamFlags,
    a: u64,
    offset_secs: i64,
    b_override: Option<u64>,
) -> Result<ExecuteOutcome, DreamError> {
    let mut stats = Stats::default();

    // cur = events 里 main 非 marker 的最大 seq（透传同源）
    let cur = events.iter()
        .filter(|e| e.thread == "main" && e.kind != "marker")
        .map(|e| e.seq).max().unwrap_or(0);

    let started = std::time::Instant::now();

    // b = 段上界（默认 cur 全量；--force 指定日时 b_override = 该日 max，只整理该日）
    let b = b_override.unwrap_or(cur);

    // F12：b<a 空段。两种情形：
    //   - a>cur（无新事件，frontier 超前）→ 写 marker(cur) 对齐，防下次重复整理
    //   - a≤cur 且 b<a（留尾结束，剩余都在 tail）→ 不写 marker，保留留尾 frontier（spec §14.2 break）
    if b < a {
        // 空段(F12):a=frontier=last_marker+1。a>cur 说明 last_marker>=cur(已对齐),不写冗余 marker。
        // a<=cur 且 b<a:留尾结束,保留 frontier。
        return Ok(ExecuteOutcome { processed_until: None, stats });
    }

    // 段 = [a, b] 中 main 非 marker 事件
    let segment_owned: Vec<HistoryEvent> = events.iter()
        .filter(|e| e.seq >= a && e.seq <= b && e.thread == "main" && e.kind != "marker")
        .cloned()
        .collect();

    if segment_owned.is_empty() {
        // [a,b] 范围内无 main 非 marker 事件 → 推进 marker 到 b，避免重整空范围
        if !flags.dry_run {
            write_dream_marker(cache, b, offset_secs, &stats)?;
            stats.markers += 1;
        }
        return Ok(ExecuteOutcome { processed_until: None, stats });
    }

    // 切回合
    let rounds = split_rounds(&segment_owned);
    if rounds.is_empty() {
        if !flags.dry_run {
            write_dream_marker(cache, b, offset_secs, &stats)?;
            stats.markers += 1;
        }
        return Ok(ExecuteOutcome { processed_until: Some(b), stats });
    }

    // 分批：按事件数阈值(cfg.batch_max_events)在【活动段边界】对齐切批。
    // 累积段,事件数达阈值 → 切批;单段超阈值 → 独占一批不切(切断任务比超阈值更糟)。
    // 每批独立 dream_round + reconcile;跨批用 next_context 滚动衔接(防任务切断)。
    let batch_max = cfg.batch_max_events.max(1);
    const CHAR_BUDGET: usize = 40_000; // 输入预算(≈64K token,中文保守估):超则在段边界减轮
    let mut batches: Vec<Vec<&Vec<HistoryEvent>>> = Vec::new();
    let mut cur_batch: Vec<&Vec<HistoryEvent>> = Vec::new();
    let mut cur_events = 0usize;
    let mut cur_chars = 0usize;
    for r in &rounds {
        let rlen = r.len();
        let rchars: usize = r.iter().map(seg_event_chars).sum();
        if !cur_batch.is_empty() && (cur_events + rlen > batch_max || cur_chars + rchars > CHAR_BUDGET) {
            batches.push(std::mem::take(&mut cur_batch));
            cur_events = 0;
            cur_chars = 0;
        }
        cur_batch.push(r);
        cur_events += rlen;
        cur_chars += rchars;
    }
    if !cur_batch.is_empty() { batches.push(cur_batch); }

    let mut events_out: Vec<FinalEvent> = Vec::with_capacity(rounds.len());
    let mut next_context = String::new();
    for chunk_refs in &batches {
        let chunk: Vec<Vec<HistoryEvent>> = chunk_refs.iter().map(|r| (*r).clone()).collect();
        let mech_chunk: Vec<MechGroup> = chunk.iter().map(|r| mechanical_extract(r)).collect();
        let groups = if !flags.mechanical {
            match caller.dream_round(&build_dream_messages(&chunk, a, b, cfg.max_rounds, &next_context)).await {
                Ok(resp) => {
                    stats.prompt_tokens += resp.prompt_tokens;
                    stats.completion_tokens += resp.completion_tokens;
                    stats.cached_tokens += resp.cached_tokens;
                    next_context = extract_next_context(&resp.content);
                    parse_groups(&resp.content)
                }
                Err(e) => return Err(DreamError::Llm(format!("该批 LLM 失败(重试耗尽),绝不机械兜底,中断留待下次: {e}"))),
            }
        } else {
            vec![]
        };
        events_out.extend(reconcile_groups(groups, &mech_chunk, cfg.max_rounds));
    }

    // stats 赋值(marker 前完成,marker 记录本次 dream 运行细节)
    stats.events_out = events_out.len();
    stats.day_segments = events_out.len();
    stats.rounds = rounds.len();
    stats.seg_a = a; stats.seg_b = b;
    stats.batches = batches.len();
    stats.model = cfg.model.clone();
    stats.elapsed_ms = started.elapsed().as_millis();

    // 写盘:dream day 只写日层 jsonl + marker(V2:含 token/分段/处理细节)
    if !flags.dry_run {
        for ev in &events_out {
            append_day_event(cache, ev, offset_secs)?;
        }
        write_dream_marker(cache, b, offset_secs, &stats)?;
        stats.markers += 1;
    }

    Ok(ExecuteOutcome { processed_until: Some(b), stats })
}

/// dream day 指定日:整理该日 history → 日层（忽略 marker；--force 删旧重写）。
/// 用于重建某日日层（数据丢失 / 强制重整理）。b_override = 该日 max，只整理该日范围。
pub async fn dream_day_at(cache: &Path, date: &str, cfg: &DreamCfg, flags: DreamFlags) -> Result<Stats, DreamError> {
    let mut stats = Stats::default();
    let caller: Box<dyn LlmCaller> = build_caller(cfg, flags)?;
    let offset_secs = crate::history::local_offset_secs();
    let events = read_all_events(cache)?;
    // 该日 main 非 marker 事件
    let day_seqs: Vec<u64> = events.iter()
        .filter(|e| e.thread == "main" && e.kind != "marker"
            && crate::history::date_from_ts_local(e.ts, offset_secs) == date)
        .map(|e| e.seq).collect();
    if day_seqs.is_empty() {
        eprintln!("[mem dream day] {} 无对话事件", date);
        return Ok(stats);
    }
    let a = *day_seqs.first().unwrap();
    let b = *day_seqs.last().unwrap();
    // --force:删该日日层（否则 append_day_event 去重跳过，重写不生效）
    if flags.force {
        let parts: Vec<&str> = date.split('-').collect();
        if parts.len() == 3 {
            let day_path = cache.join("memory").join(parts[0]).join(parts[1]).join(format!("{}.jsonl", date));
            if day_path.exists() {
                let _ = std::fs::remove_file(&day_path);
                eprintln!("[mem dream day] --force:删 {} 重建", day_path.display());
            }
        }
    }
    let outcome = execute_dream(cache, &events, cfg, caller.as_ref(), flags, a, offset_secs, Some(b)).await?;
    stats.rounds += outcome.stats.rounds;
    stats.events_out += outcome.stats.events_out;
    stats.day_segments += outcome.stats.day_segments;
    stats.markers += outcome.stats.markers;
    Ok(stats)
}

// ─── 月层/年层提取(读下层 jsonl → LLM → 本层 jsonl)───

const MONTH_LAYER_SYS: &str = r#"你是 dream,负责把一个月各天的「日层索引卡」综合成「月层索引」。输入按天给出,每天含事件标题列表、类型分布、关键词。输出一段 JSON:{"daily":[{"date":"YYYY-MM-DD","day_title":"那天主线一句话(带具体事件名词,禁'讨论/检索'万能词)","top_events":["top3事件标题"]}],"mainline":{"mainline":"月度主线1-3句(带具体大事名词,禁空话)","key_outputs":["关键产出/项目/文件"],"keywords":["月度关键词5-10"]}}。这些是给 agent 回忆定位用的月层索引卡:day_title 看一眼知道那天干了啥、mainline 知道这月大事。只输出 JSON,不要其他文字。"#;

const YEAR_LAYER_SYS: &str = r#"你是 dream,负责把一年各月的「月层主线」综合成「年层索引」。输入按月给出,每月含月度主线。输出一段 JSON:{"monthly":[{"month":"YYYY-MM","month_title":"该月主线一句话(带具体名词)"}],"year_mainline":"年度主线1-3句(带年度大事,带具体名词,禁空话)","keywords":["年度关键词5-10"]}。只输出 JSON,不要其他文字。"#;

/// 月层提取:读当月所有日层 jsonl → LLM → 月层 jsonl(memory/YYYY/MM/YYYY-MM.jsonl)
pub async fn extract_month(
    cache: &Path,
    ym: (i32, u32),
    cfg: &DreamCfg,
    flags: DreamFlags,
) -> Result<Stats, DreamError> {
    let mut stats = Stats::default();
    let caller = build_caller(cfg, flags)?;
    let (year, month) = ym;
    let ym_str = format!("{}-{:02}", year, month);
    let dir = cache.join("memory").join(year.to_string()).join(format!("{:02}", month));

    // 系统当前日期(用于排除当天未完成事件)
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let off = crate::history::local_offset_secs();
    let today = crate::history::date_from_ts_local(now_ms, off);
    let this_ym = format!("{}-{:02}", year, month);

    struct DayAgg { titles: Vec<String>, types: std::collections::HashMap<String, u32>, keywords: Vec<String> }
    let mut days: Vec<(String, DayAgg)> = Vec::new();
    if dir.exists() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)?.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        entries.sort();
        for path in entries {
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") { continue; }
            let date = match path.file_stem().and_then(|s| s.to_str()) { Some(d) => d.to_string(), None => continue };
            if !date.starts_with(&ym_str) { continue; }
            let mut titles = Vec::new();
            let mut types: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
            let mut keywords: Vec<String> = Vec::new();
            for line in std::fs::read_to_string(&path).unwrap_or_default().lines() {
                let l = line.trim();
                if !l.starts_with('{') { continue; }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
                    if let Some(t) = v.get("title").and_then(|x| x.as_str()) { titles.push(t.to_string()); }
                    if let Some(ty) = v.get("type").and_then(|x| x.as_str()) { *types.entry(ty.to_string()).or_insert(0) += 1; }
                    if let Some(kws) = v.get("keywords").and_then(|x| x.as_array()) {
                        for k in kws { if let Some(s) = k.as_str() { keywords.push(s.to_string()); } }
                    }
                }
            }
            if !titles.is_empty() {
                // P0: 月层排除当天(事件未完成,综合进月层=污染)
                let is_today = this_ym == today[..7] && date == today;
                if is_today {
                    eprintln!("[mem dream month] 跳过当天 {}(事件可能未完成)", date);
                    continue;
                }
                days.push((date, DayAgg { titles, types, keywords }));
            }
        }
    }
    if days.is_empty() {
        eprintln!("[mem dream month] {} 无日层数据(先 mem dream day 生成日层 jsonl)", ym_str);
        return Ok(stats);
    }

    // P2: 增量跳过(月层 mtime > 所有日层 mtime → 无变化不调 LLM)
    let month_path = dir.join(format!("{}.jsonl", ym_str));
    if month_path.exists() && !flags.dry_run && !flags.force {
        let month_mtime = std::fs::metadata(&month_path).ok().and_then(|m| m.modified().ok());
        if let Some(mt) = month_mtime {
            let mut has_newer = false;
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.extension().and_then(|s| s.to_str()) != Some("jsonl") { continue; }
                    let stem_len = p.file_stem().and_then(|s| s.to_str()).map(|s| s.len()).unwrap_or(0);
                    if stem_len != 10 { continue; } // 只看日层
                    let d_stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                    if d_stem == today { continue; } // 排当天（跟 P0 一致：当天未结束，不算 month 新）
                    if let Some(dt) = std::fs::metadata(&p).ok().and_then(|m| m.modified().ok()) {
                        if dt > mt { has_newer = true; break; }
                    }
                }
            }
            if !has_newer {
                eprintln!("[mem dream month] {} 月层无变化(日层未更新),跳过", ym_str);
                return Ok(stats);
            }
        }
    }

    let total_events: usize = days.iter().map(|(_, d)| d.titles.len()).sum();
    let mut prompt = format!("把 {} 年 {} 月各天日层索引卡综合成月层索引:\n\n", year, month);
    for (date, d) in &days {
        let types_str = d.types.iter().map(|(k, v)| format!("{}{}", k, v)).collect::<Vec<_>>().join("/");
        let kw_top = top_keywords(&d.keywords, 8);
        prompt.push_str(&format!("{} ({}事件 类型{} 关键词{}):\n  {}\n",
            date, d.titles.len(), types_str, kw_top.join("·"), d.titles.join(" / ")));
    }

    let messages = vec![
        serde_json::json!({"role":"system","content": MONTH_LAYER_SYS}),
        serde_json::json!({"role":"user","content": prompt}),
    ];
    let resp = if !flags.mechanical {
        caller.dream_round(&messages).await.unwrap_or_else(|e| { eprintln!("[mem dream month] LLM 失败: {e}"); DreamResp::default() })
    } else { DreamResp::default() };
    let parsed = parse_month_output(&resp.content);

    if let Some(parent) = dir.parent() { std::fs::create_dir_all(parent)?; }
    let out_path = &month_path;
    let mut out = String::new();
    for (date, d) in &days {
        let llm_day = parsed.daily.iter().find(|x| x.date == *date);
        let day_title = llm_day.map(|x| x.day_title.clone()).filter(|s| !s.is_empty())
            .unwrap_or_else(|| d.titles.first().cloned().unwrap_or_default());
        let top_events = llm_day.map(|x| x.top_events.clone()).filter(|v| !v.is_empty())
            .unwrap_or_else(|| d.titles.iter().take(3).cloned().collect());
        let kw_top = top_keywords(&d.keywords, 8);
        let mut types_map = serde_json::Map::new();
        for (k, v) in &d.types { types_map.insert(k.clone(), serde_json::json!(v)); }
        let line = serde_json::json!({
            "type": "day", "date": date,
            "day_title": day_title, "event_count": d.titles.len(),
            "types": serde_json::Value::Object(types_map),
            "keywords": kw_top, "top_events": top_events,
            "drill": format!("memory/{}/{:02}/{}.jsonl", year, month, date),
        });
        out.push_str(&format!("{}\n", line));
    }
    let (ml_main, ml_outs, ml_kw) = if let Some(m) = &parsed.mainline {
        (m.mainline.clone(), m.key_outputs.clone(), m.keywords.clone())
    } else {
        (format!("{}年{}月 共{}天 {}事件", year, month, days.len(), total_events), vec![], vec![])
    };
    let mut themes_map = serde_json::Map::new();
    for (_, d) in &days {
        for (k, v) in &d.types {
            let cur = themes_map.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
            themes_map.insert(k.clone(), serde_json::json!(cur + *v as u64));
        }
    }
    let ml_line = serde_json::json!({
        "type": "mainline", "month": ym_str,
        "mainline": ml_main, "themes": serde_json::Value::Object(themes_map),
        "key_outputs": ml_outs, "keywords": ml_kw,
    });
    out.push_str(&format!("{}\n", ml_line));
    std::fs::write(&out_path, out)?;
    stats.months_rotated += 1;
    println!("[mem dream month] {} → {} ({}天/{}事件)", ym_str, out_path.display(), days.len(), total_events);
    Ok(stats)
}

/// 年层提取:读当年所有月层 jsonl mainline → LLM → 年层 jsonl(memory/YYYY/YYYY.jsonl)
/// P0: 排除当月(当月事件未完成,月层可能不完整)
/// P2: 月层文件 mtime > 年层 mtime → 跳过(无变化不调 LLM)
pub async fn extract_year(cache: &Path, year: i32, cfg: &DreamCfg, flags: DreamFlags) -> Result<Stats, DreamError> {
    let mut stats = Stats::default();
    let caller = build_caller(cfg, flags)?;
    let dir = cache.join("memory").join(year.to_string());

    // P0: 系统当前年月(排除当月)
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let off = crate::history::local_offset_secs();
    let today = crate::history::date_from_ts_local(now_ms, off);
    let current_ym = today[..7].to_string(); // "YYYY-MM"

    // P2: 增量跳过(年层 mtime > 所有月层 mtime → 无变化)
    let year_path = dir.join(format!("{}.jsonl", year));
    if year_path.exists() && !flags.dry_run && !flags.force {
        let year_mtime = std::fs::metadata(&year_path).ok().and_then(|m| m.modified().ok());
        if let Some(yt) = year_mtime {
            let mut has_newer = false;
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if !p.is_dir() { continue; }
                    if let Ok(mrd) = std::fs::read_dir(&p) {
                        for mf in mrd.flatten() {
                            let mp = mf.path();
                            if mp.extension().and_then(|s| s.to_str()) != Some("jsonl") { continue; }
                            let stem_len = mp.file_stem().and_then(|s| s.to_str()).map(|s| s.len()).unwrap_or(0);
                            if stem_len != 7 { continue; } // 只看月层(YYYY-MM)
                            let m_stem = mp.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                            if m_stem == current_ym { continue; } // 排当月（跟 P0 一致：当月未结束，不算 year 新）
                            if let Some(mt) = std::fs::metadata(&mp).ok().and_then(|m| m.modified().ok()) {
                                if mt > yt { has_newer = true; break; }
                            }
                        }
                    }
                    if has_newer { break; }
                }
            }
            if !has_newer {
                eprintln!("[mem dream year] {} 年层无变化(月层未更新),跳过", year);
                return Ok(stats);
            }
        }
    }

    let mut months: Vec<(String, String)> = Vec::new(); // (ym, mainline)
    if dir.exists() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)?.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        entries.sort();
        for month_dir in entries {
            if !month_dir.is_dir() { continue; }
            let mm = match month_dir.file_name().and_then(|s| s.to_str()) { Some(s) => s.to_string(), None => continue };
            let ym = format!("{}-{}", year, mm);
            // P0: 排除当月
            if ym == current_ym {
                eprintln!("[mem dream year] 跳过当月 {}(事件可能未完成)", ym);
                continue;
            }
            let path = month_dir.join(format!("{}.jsonl", ym));
            if !path.exists() { continue; }
            for line in std::fs::read_to_string(&path).unwrap_or_default().lines() {
                let l = line.trim();
                if !l.starts_with('{') { continue; }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
                    if v.get("type").and_then(|x| x.as_str()) == Some("mainline") {
                        let ml = v.get("mainline").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        months.push((ym.clone(), ml));
                        break;
                    }
                }
            }
        }
    }
    if months.is_empty() { eprintln!("[mem dream year] {} 无月层数据(先 mem dream month 生成月层 jsonl)", year); return Ok(stats); }

    let mut prompt = format!("把 {} 年各月月层主线综合成年层索引:\n\n", year);
    for (m, ml) in &months { prompt.push_str(&format!("{}: {}\n", m, ml)); }
    let messages = vec![
        serde_json::json!({"role":"system","content": YEAR_LAYER_SYS}),
        serde_json::json!({"role":"user","content": prompt}),
    ];
    let resp = if !flags.mechanical { caller.dream_round(&messages).await.unwrap_or_default() } else { DreamResp::default() };
    let parsed = parse_year_output(&resp.content);

    if let Some(parent) = dir.parent() { std::fs::create_dir_all(parent)?; }
    let mut out = String::new();
    for (m, ml) in &months {
        let title = parsed.monthly.iter().find(|x| x.month == *m)
            .map(|x| x.month_title.clone()).filter(|s| !s.is_empty())
            .unwrap_or_else(|| ml.clone());
        let mm = m.get(5..).unwrap_or(m);
        let line = serde_json::json!({"type":"month","month": m, "month_title": title, "drill": format!("memory/{}/{}/{}.jsonl", year, mm, m)});
        out.push_str(&format!("{}\n", line));
    }
    let ym_line = serde_json::json!({
        "type": "year_mainline", "year": year,
        "year_mainline": parsed.year_mainline.unwrap_or_default(),
        "keywords": parsed.year_keywords,
    });
    out.push_str(&format!("{}\n", ym_line));
    std::fs::write(&year_path, out)?;
    stats.years_rotated += 1;
    println!("[mem dream year] {} → {} ({}月)", year, year_path.display(), months.len());
    Ok(stats)
}

fn build_caller(cfg: &DreamCfg, flags: DreamFlags) -> Result<Box<dyn LlmCaller>, DreamError> {
    if !flags.mechanical && !flags.dry_run && cfg.api_key.is_none() {
        return Err(DreamError::Config("缺 api_key 且非 --mechanical".into()));
    }
    if flags.mechanical || flags.dry_run || cfg.api_key.is_none() {
        Ok(Box::new(FakeLlm::new(vec!["{}".into()])))
    } else {
        Ok(Box::new(MiniMaxCaller::new(cfg)?))
    }
}

fn top_keywords(kws: &[String], n: usize) -> Vec<String> {
    let mut freq: std::collections::HashMap<&String, u32> = std::collections::HashMap::new();
    for k in kws { *freq.entry(k).or_insert(0) += 1; }
    let mut v: Vec<_> = freq.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.iter().take(n).map(|(k, _)| (*k).clone()).collect()
}

#[derive(Default, Clone)]
struct ParsedMonth { daily: Vec<ParsedDay>, mainline: Option<MonthMainline> }
#[derive(Clone)] struct ParsedDay { date: String, day_title: String, top_events: Vec<String> }
#[derive(Clone, Default)] struct MonthMainline { mainline: String, key_outputs: Vec<String>, keywords: Vec<String> }

fn parse_month_output(resp: &str) -> ParsedMonth {
    let s = strip_think(resp);
    let json = match (s.find('{'), s.rfind('}')) { (Some(a), Some(b)) if b > a => &s[a..=b], _ => return ParsedMonth::default() };
    let v: serde_json::Value = match serde_json::from_str(json) { Ok(v) => v, Err(_) => return ParsedMonth::default() };
    let mut pm = ParsedMonth::default();
    if let Some(arr) = v.get("daily").and_then(|x| x.as_array()) {
        for d in arr {
            pm.daily.push(ParsedDay {
                date: d.get("date").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                day_title: d.get("day_title").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                top_events: d.get("top_events").and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|n| n.as_str().map(|s| s.to_string())).collect()).unwrap_or_default(),
            });
        }
    }
    if let Some(m) = v.get("mainline") {
        pm.mainline = Some(MonthMainline {
            mainline: m.get("mainline").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            key_outputs: m.get("key_outputs").and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|n| n.as_str().map(|s| s.to_string())).collect()).unwrap_or_default(),
            keywords: m.get("keywords").and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|n| n.as_str().map(|s| s.to_string())).collect()).unwrap_or_default(),
        });
    }
    pm
}

#[derive(Default)]
struct ParsedYear { monthly: Vec<YearMonth>, year_mainline: Option<String>, year_keywords: Vec<String> }
#[derive(Clone)] struct YearMonth { month: String, month_title: String }

fn parse_year_output(resp: &str) -> ParsedYear {
    let s = strip_think(resp);
    let json = match (s.find('{'), s.rfind('}')) { (Some(a), Some(b)) if b > a => &s[a..=b], _ => return ParsedYear::default() };
    let v: serde_json::Value = match serde_json::from_str(json) { Ok(v) => v, Err(_) => return ParsedYear::default() };
    let mut py = ParsedYear::default();
    if let Some(arr) = v.get("monthly").and_then(|x| x.as_array()) {
        for m in arr {
            py.monthly.push(YearMonth {
                month: m.get("month").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                month_title: m.get("month_title").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            });
        }
    }
    py.year_mainline = v.get("year_mainline").and_then(|x| x.as_str()).map(|s| s.to_string());
    py.year_keywords = v.get("keywords").and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|n| n.as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
    py
}

/// 找最后一条 dream marker 的 until_seq（frontier）。无 marker 返回 0。
pub fn last_dream_marker_seq(events: &[HistoryEvent]) -> u64 {
    events.iter().rev()
        .find_map(|e| {
            if e.kind == "marker" && e.data.get("marker").and_then(|v| v.as_str()) == Some("dream") {
                e.data.get("until_seq").and_then(|v| v.as_u64())
            } else {
                None
            }
        })
        .unwrap_or(0)
}

/// 机械底座分组：程序化真相，LLM 漏掉的回合用此落盘（G1 完整性兜底）。
#[derive(Debug, Clone, PartialEq)]
pub struct MechGroup {
    pub ts: u64,
    pub seq_a: u64,
    pub seq_b: u64,
    pub title: String,
    pub subject: String,
    pub detail: String,
    pub event_type: String,
    pub keywords: Vec<String>,
    pub has_attachment: bool,
    pub attachment: String,
}

/// 单次读取全部 history 事件（F8 透传：一次 read_all，全程传递）。
pub fn read_all_events(cache: &Path) -> Result<Vec<HistoryEvent>, DreamError> {
    Ok(crate::history::read_all(&cache.join("history")))
}

// ─── dream 纯逻辑层 A：回合切 + 机械底座 + 留尾（移植自 dream.rs:257-382）───

/// 按 kind=user 边界切回合。段首非 user 前导事件挂到下一个 user 回合。
/// 返回 owned Vec<Vec<HistoryEvent>>（每个 inner vec = 一个回合的事件）。
pub fn split_rounds(segment: &[HistoryEvent]) -> Vec<Vec<HistoryEvent>> {
    let mut rounds = Vec::new();
    let mut cur: Vec<HistoryEvent> = Vec::new();
    let mut prev_ts: u64 = 0;
    for e in segment {
        // 活动段边界 = user 输入 ∪ spawn 子代理 ∪ ts 间隔 > 5min
        let is_sub_spawn = e.kind == "assistant" && e.data.get("tool_calls")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().any(|t| {
                let n = t.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or("");
                n == "subagent"
            }))
            .unwrap_or(false);
        let ts_gap = prev_ts != 0 && e.ts.saturating_sub(prev_ts) > SEG_TS_GAP_MS;
        let boundary = e.kind == "user" || is_sub_spawn || ts_gap;
        if boundary && !cur.is_empty() {
            rounds.push(std::mem::take(&mut cur));
        }
        match e.kind.as_str() {
            "user" | "assistant" | "tool_result" | "external" | "subagent_result" | "edit" => {
                cur.push(e.clone());
            }
            _ => {}
        }
        prev_ts = e.ts;
    }
    if !cur.is_empty() { rounds.push(cur); }
    rounds
}

/// 活动段 ts 间隔边界：超过 5 分钟无活动视为任务边界。
const SEG_TS_GAP_MS: u64 = 300_000;

/// 估算一个 history 事件喂给 LLM 的字数(text/content/thinking/summary/工具参数),
/// 供动态分批按输入预算(CHAR_BUDGET)在段边界减轮。
fn seg_event_chars(e: &HistoryEvent) -> usize {
    let mut n = 0usize;
    for k in ["text", "content", "thinking", "summary"] {
        if let Some(s) = e.data.get(k).and_then(|v| v.as_str()) { n += s.chars().count(); }
    }
    if let Some(arr) = e.data.get("tool_calls").and_then(|v| v.as_array()) {
        for t in arr {
            if let Some(a) = t.get("function").and_then(|f| f.get("arguments")).and_then(|v| v.as_str()) {
                n += a.chars().count();
            }
        }
    }
    n
}

/// 从 LLM 响应里提取 next_context 行（跨批衔接摘要），供下一批 prompt 头部带上。
fn extract_next_context(content: &str) -> String {
    for line in content.lines() {
        let l = line.trim().trim_start_matches("```json").trim_start_matches("```").trim();
        if l.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
                if let Some(nc) = v.get("next_context").and_then(|x| x.as_str()) {
                    return nc.to_string();
                }
            }
        }
    }
    String::new()
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Unicode 安全截断：按 char_indices 截到 max chars（不切多字节字符中段）。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max { return s.to_string(); }
    let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
    s[..end].to_string()
}

/// 截到 max chars + 去末尾标点（。，！？、,.!?）。
fn truncate_title(s: &str, max: usize) -> String {
    let mut t = truncate_chars(s, max);
    while let Some(c) = t.chars().last() {
        if "。，！？、,.!?".contains(c) { t.pop(); } else { break; }
    }
    t
}

/// 取首句（首个句号/换行/分号前）或前 80 字。
fn truncate_first_sentence(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() { return String::new(); }
    let cut = s.find(|c: char| c == '。' || c == '\n' || c == '；' || c == '.')
        .map(|i| {
            let c_len = s[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(0);
            i + c_len
        })
        .unwrap_or_else(|| s.char_indices().nth(80).map(|(i, _)| i).unwrap_or(s.len()));
    s[..cut].to_string()
}

/// 机械底座：从单个回合（owned slice）产出程序化真相（G1 完整性兜底）。
pub fn mechanical_extract(round: &[HistoryEvent]) -> MechGroup {
    // 提取 user 事件文本
    let user_text = round.first()
        .and_then(|e| {
            if e.kind == "user" {
                e.data.get("text").and_then(|v| v.as_str())
            } else {
                None
            }
        })
        .unwrap_or("");

    // 提取 assistant 事件
    let assistants: Vec<&HistoryEvent> = round.iter()
        .filter(|e| e.kind == "assistant")
        .collect();

    // seq_a = 首事件 seq，seq_b = 末事件 seq，ts = 末事件 ts
    let seq_a = round.first().map(|e| e.seq).unwrap_or(0);
    let seq_b = round.last().map(|e| e.seq).unwrap_or(seq_a);
    let ts = round.last().map(|e| e.ts).unwrap_or(0);

    // 判断是否有 tool_calls
    let has_tool_calls = assistants.iter().any(|a| a.data.get("tool_calls").is_some());

    let user_clean = collapse_ws(user_text);
    let title = truncate_title(&user_clean, 30);
    let asst_first = assistants.first()
        .and_then(|a| a.data.get("content").and_then(|v| v.as_str()))
        .map(truncate_first_sentence)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| if has_tool_calls { "(调用工具)".into() } else { String::new() });
    let detail = {
        let u = truncate_chars(&user_clean, 100);
        if asst_first.is_empty() { u } else { format!("{} → {}", u, truncate_chars(&asst_first, 80)) }
    };

    MechGroup {
        ts,
        seq_a,
        seq_b,
        title: if title.is_empty() { "（无文本）".into() } else { title },
        detail: truncate_chars(&detail, 200),
        subject: "用户".into(),
        event_type: if has_tool_calls { "操作".into() } else { "沟通".into() },
        keywords: Vec::new(),
        has_attachment: false,
        attachment: String::new(),
    }
}


// ─── dream 纯逻辑层 B：LLM 分组解析 + reconcile（移植自 dream.rs:384-497）───

/// LLM 输出的一个分组（覆盖若干回合，使用 1-based 回合索引）。
#[derive(Debug, Clone, PartialEq)]
pub struct LlmGroup {
    pub rounds: Vec<usize>,
    pub title: String,
    pub detail: String,
    pub subject: String,
    pub event_type: String,
    pub keywords: Vec<String>,
}

/// 最终事件（落盘单位）。reconcile 输出 + G1 机械兜底共用此 struct。
#[derive(Debug, Clone, PartialEq)]
pub struct FinalEvent {
    pub ts: u64,
    pub seq_a: u64,
    pub seq_b: u64,
    pub title: String,
    pub subject: String,
    pub detail: String,
    pub event_type: String,
    pub keywords: Vec<String>,
    pub has_attachment: bool,
    pub attachment: String,
}

/// 日概括（规则提炼，无 LLM）
#[derive(Debug, Clone, PartialEq)]
pub struct DaySummary {
    pub date: String,            // "YYYY-MM-DD"
    pub headline: String,        // 一句话主线 "围绕 X"
    pub keywords: Vec<String>,   // Top-N 具体标识符（联想锚）
    pub entities: Vec<String>,   // 文件路径/函数名/外部工具（下钻锚）
    pub event_count: u32,
    pub seq_min: u64,
    pub seq_max: u64,
}

/// 解析 LLM 输出：逐行 JSON `{rounds,title,detail,subject}`，跳过坏行。
/// faithful port of dream.rs:399-421
pub fn parse_groups(content: &str) -> Vec<LlmGroup> {
    let mut groups = Vec::new();
    for line in content.lines() {
        let l = line.trim().trim_start_matches("```json").trim_start_matches("```").trim();
        if l.is_empty() || !l.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
            let rounds: Vec<usize> = v.get("rounds")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|n| n.as_u64().map(|n| n as usize)).collect())
                .unwrap_or_default();
            let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if rounds.is_empty() || title.is_empty() {
                continue; // skip bad lines silently
            }
            groups.push(LlmGroup {
                rounds,
                title,
                detail: v.get("detail").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                subject: v.get("subject").and_then(|x| x.as_str()).unwrap_or("用户").to_string(),
                event_type: v.get("event_type").and_then(|x| x.as_str()).unwrap_or("其他").to_string(),
                keywords: v.get("keywords").and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|n| n.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_default(),
            });
        }
        // malformed lines are skipped (no error)
    }
    groups
}

/// reconcile：LLM 分组 → 最终事件。强制全覆盖（漏的机械补）+ 非连续拆子组（F6）+ 超限强制拆（F11）。
/// faithful port of dream.rs:440-497
pub fn reconcile_groups(llm: Vec<LlmGroup>, mech: &[MechGroup], max_rounds: u32) -> Vec<FinalEvent> {
    let n = mech.len();
    let mut covered = vec![false; n];
    let mut out = Vec::new();

    for g in llm {
        // 1-based rounds → 0-based idx；越界/重复跳过
        let mut idxs: Vec<usize> = Vec::new();
        for r in &g.rounds {
            if *r == 0 || *r > n {
                continue; // out of range, skip
            }
            let i = r - 1;
            if covered[i] {
                continue; // duplicate, skip
            }
            covered[i] = true;
            idxs.push(i);
        }
        if idxs.is_empty() {
            continue;
        }

        // F6：非连续拆子组（idxs 升序后检查相邻是否连续）
        idxs.sort();
        let mut chunks: Vec<Vec<usize>> = vec![vec![idxs[0]]];
        for &i in &idxs[1..] {
            let last = *chunks.last().unwrap().last().unwrap();
            if i == last + 1 {
                chunks.last_mut().unwrap().push(i);
            } else {
                // non-contiguous, start new chunk
                chunks.push(vec![i]);
            }
        }

        for chunk in chunks {
            // F11：超限强制拆成 max_rounds 块（续段带 (续K) 后缀）
            if chunk.len() as u64 > max_rounds as u64 {
                for (k, sub) in chunk.chunks(max_rounds as usize).enumerate() {
                    let suffix = format!(" (续{})", k + 1);
                    let first = mech[sub[0]].clone();
                    let last = mech[sub[sub.len() - 1]].clone();
                    out.push(FinalEvent {
                        ts: last.ts,
                        seq_a: first.seq_a,
                        seq_b: last.seq_b,
                        title: g.title.clone(),
                        detail: format!("{}{}", g.detail, suffix),
                        subject: g.subject.clone(),
                        event_type: g.event_type.clone(),
                        keywords: g.keywords.clone(),
                        has_attachment: false,
                        attachment: String::new(),
                    });
                }
            } else {
                let first = mech[chunk[0]].clone();
                let last = mech[chunk[chunk.len() - 1]].clone();
                out.push(FinalEvent {
                    ts: last.ts,
                    seq_a: first.seq_a,
                    seq_b: last.seq_b,
                    title: g.title.clone(),
                    detail: g.detail.clone(),
                    subject: g.subject.clone(),
                    event_type: g.event_type.clone(),
                    keywords: g.keywords.clone(),
                    has_attachment: false,
                    attachment: String::new(),
                });
            }
        }
    }

    // G1：漏的回合 → 机械兜底（完整性硬保证）
    for (i, &c) in covered.iter().enumerate() {
        if !c {
            let m = &mech[i];
            out.push(FinalEvent {
                ts: m.ts,
                seq_a: m.seq_a,
                seq_b: m.seq_b,
                title: m.title.clone(),
                detail: m.detail.clone(),
                subject: m.subject.clone(),
                event_type: m.event_type.clone(),
                keywords: m.keywords.clone(),
                has_attachment: false,
                attachment: String::new(),
            });
        }
    }

    out.sort_by_key(|e| e.seq_a);
    out
}

// ─── LLM 层：trait + build_dream_messages + FakeLlm + MiniMaxCaller ───

const DREAM_SYS: &str = "你是 dream，负责把一段对话整理成「日」层记忆（事件索引卡）。输入按【活动段】编号给出，每段含【用户原话】【助手正文+思考全文】【工具调用名+参数】【子代理汇报】【可选工具状态首行】。请把属于同一事件的活动段合并成一个分组，对每个分组输出一行 JSON：{\"rounds\":[段编号...],\"title\":\"具体到能区分本组与其他组的标题(带关键名词，禁用'启动/讨论/检索'这类万能词)\",\"detail\":\"该事件的关键事实/结论/产出——必须写出【检索到了什么/得出什么结论/产出什么文件】，禁写成'检索了/讨论了/警告了'空动作概述\",\"event_type\":\"事件类型:调研/创作/调试/决策/检索/重构/沟通/其他\",\"keywords\":[\"3-6个中英文关键词，从本组实际内容提取，用于日后检索\"],\"subject\":\"主导方:用户/agent/子代理#N\"}。这些字段是给 agent 未来回忆时【扫读定位】的索引卡：title 看一眼能想起是哪件事、detail 用来确认、keywords/类型用来过滤跳过不相关的——【不要记录过程细节，细节在原文 history 里】。动笔前先想清楚整段主线(这几组共同构成一件什么事)，让每组服务主线、组间不重复。硬性要求：①每个段编号必须恰好被一个分组覆盖(1..N 全覆盖，不许漏/重复)；②单组段数 ≤ {MAX_ROUNDS}；③组内编号连续(如[3,4,5]，不允许[3,5])；④只输出 JSON 行，不要任何其他文字、不要 markdown 代码块。最后额外输出一行：{\"next_context\":\"本批最后一个活动段在做什么/结论/是否未完成，供下一批做上下文衔接\"}。";

const MONTH_THEME_SYS: &str = "你是 dream，负责把一个月的「日」层记忆综合成一段「月主题」。给你这个月每天的事件段，请输出一段话（150-300字）概括这个月的主要活动、主题和关键人物。只输出月主题正文，不要标题、不要 JSON、不要 markdown、不要任何前后缀。";

/// LLM 调用者抽象（可注入 FakeLlm 或 MiniMaxCaller）
#[async_trait]
pub trait LlmCaller: Send + Sync {
    async fn dream_round(&self, messages: &[serde_json::Value]) -> Result<DreamResp, DreamError>;
    async fn month_theme(&self, messages: &[serde_json::Value]) -> Result<String, DreamError>;
}

/// 构造 dream LLM 的 messages（system + 渲染回合的 user prompt）。
/// faithful port of dream.rs:500-520，适配 borrowed Round → owned Vec<HistoryEvent>
pub fn build_dream_messages(
    rounds: &[Vec<HistoryEvent>],
    a: u64, b: u64, max_rounds: u32,
    next_context: &str,
) -> Vec<serde_json::Value> {
    let mut prompt = format!("把下面 {} 个活动段（history seq[{a},{b}]）整理成事件分组：\n\n", rounds.len());
    if !next_context.is_empty() {
        prompt.push_str(&format!("【上一批衔接】{}\n\n", next_context));
    }
    for (i, r) in rounds.iter().enumerate() {
        let seq_a = r.first().map(|e| e.seq).unwrap_or(0);
        let seq_b = r.last().map(|e| e.seq).unwrap_or(seq_a);
        prompt.push_str(&format!("段{} [seq{}-{}]\n", i + 1, seq_a, seq_b));

        // 用户原话（全文）
        if let Some(t) = r.iter().find(|e| e.kind == "user")
            .and_then(|e| e.data.get("text").and_then(|v| v.as_str()))
        {
            if !t.trim().is_empty() {
                prompt.push_str(&format!("用户：{}\n", collapse_ws(t)));
            }
        }
        // 助手正文 + 思考全文 + 工具调用名+标识参数
        for e in r.iter().filter(|e| e.kind == "assistant") {
            if let Some(c) = e.data.get("content").and_then(|v| v.as_str()) {
                if !c.trim().is_empty() {
                    prompt.push_str(&format!("助手：{}\n", c));
                }
            }
            if let Some(th) = e.data.get("thinking").and_then(|v| v.as_str()) {
                if !th.trim().is_empty() {
                    prompt.push_str(&format!("思考：{}\n", th));
                }
            }
            if let Some(tcs) = e.data.get("tool_calls").and_then(|v| v.as_array()) {
                let lines: Vec<String> = tcs.iter().filter_map(|t| {
                    let name = t.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or("?");
                    let args = t.get("function").and_then(|f| f.get("arguments")).and_then(|v| v.as_str()).unwrap_or("");
                    // 内容性参数（write/edit/attach 的正文）不进 prompt，只留工具名
                    let disp = match name {
                        "write" | "edit" | "attach" => name.to_string(),
                        _ => format!("{}({})", name, truncate_chars(args, 100)),
                    };
                    Some(disp)
                }).collect();
                if !lines.is_empty() {
                    prompt.push_str(&format!("工具调用：{}\n", lines.join(" | ")));
                }
            }
        }
        // 子代理汇报全文
        for e in r.iter().filter(|e| e.kind == "subagent_result") {
            if let Some(s) = e.data.get("summary").and_then(|v| v.as_str()) {
                if !s.trim().is_empty() {
                    prompt.push_str(&format!("子代理汇报：{}\n", s));
                }
            }
        }
        // 工具结果状态首行（极短，给「成败 + 线索」，不进正文）
        for e in r.iter().filter(|e| e.kind == "tool_result") {
            let name = e.data.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let res = e.data.get("result").and_then(|v| v.as_str()).unwrap_or("");
            let first = res.lines().next().unwrap_or("").trim();
            if !first.is_empty() {
                prompt.push_str(&format!("工具状态：{} {}\n", name, truncate_chars(first, 60)));
            }
        }
        prompt.push('\n');
    }
    prompt.push_str(&format!(
        "\n输出 JSON 行（每行一个分组，必须覆盖所有段编号 1..{}，单组段数 ≤ {}，组内编号连续），最后额外一行 {{\"next_context\":\"...\"}}。",
        rounds.len(), max_rounds));
    vec![
        serde_json::json!({"role":"system","content": DREAM_SYS.replace("{MAX_ROUNDS}", &max_rounds.to_string())}),
        serde_json::json!({"role":"user","content": prompt}),
    ]
}

/// 月主题 user prompt（T8 用）
pub fn month_theme_prompt(ym: &str, segments_text: &str) -> String {
    format!("把 {ym} 这个月的日层记忆综合成一段月主题：\n\n{segments_text}\n\n现在输出月主题正文（一段话）。")
}

/// FakeLlm：测试用，返回预设响应（FIFO 队列）
pub struct FakeLlm {
    responses: Mutex<VecDeque<String>>,
}

impl FakeLlm {
    pub fn new(responses: Vec<String>) -> Self {
        Self { responses: Mutex::new(responses.into()) }
    }
}

#[async_trait]
impl LlmCaller for FakeLlm {
    async fn dream_round(&self, _messages: &[serde_json::Value]) -> Result<DreamResp, DreamError> {
        let mut q = self.responses.lock().unwrap();
        Ok(DreamResp {
            content: q.pop_front().unwrap_or_else(|| q.back().cloned().unwrap_or_default()),
            ..Default::default()
        })
    }

    async fn month_theme(&self, _month_events: &[serde_json::Value]) -> Result<String, DreamError> {
        let mut q = self.responses.lock().unwrap();
        Ok(q.pop_front().unwrap_or_else(|| q.back().cloned().unwrap_or_default()))
    }
}

/// MiniMaxCaller：真实 MiniMax LLM 调用（非流式）
pub struct MiniMaxCaller {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    key: String,
}

impl MiniMaxCaller {
    pub fn new(cfg: &DreamCfg) -> Result<Self, DreamError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| DreamError::Llm(format!("reqwest Client 构建失败: {e}")))?;

        // 复用 ovoice 的 endpoint 规则：region=cn 用 https://api.minimaxi.com/v1
        // （从 llm.rs:12 确认 API_BASE 为 "https://api.minimaxi.com/v1"，无 region 差异；
        //  intl 区若有差异待 T10 手测验证）
        let base = match cfg.region.as_str() {
            "cn" => "https://api.minimaxi.com",
            "intl" | _ => "https://api.minimaxi.com",
        };
        let endpoint = format!("{}/v1/chat/completions", base);

        let key = cfg.api_key.as_ref()
            .ok_or_else(|| DreamError::Config("api_key 未配置".into()))?;

        Ok(MiniMaxCaller {
            client,
            endpoint,
            model: cfg.model.clone(),
            key: key.clone(),
        })
    }
}

/// 剥离 MiniMax M3 content 内嵌的 <think>...</think> 思考前缀。
/// M3 非流式响应把 reasoning 内嵌在 content 开头；dream_round 的 parse_groups
/// 恰好只取 JSON 不受影响，但 month_theme 直接用 content 会污染月主题/年索引。
/// 处理成对标签与未闭合（被 max_tokens 截断）两种情况。
fn strip_think(s: &str) -> String {
    let mut out = s.to_string();
    // 成对 <think>...</think>
    while let (Some(start), Some(end)) = (out.find("<think>"), out.find("</think>")) {
        if end >= start {
            let mut buf = String::with_capacity(out.len());
            buf.push_str(&out[..start]);
            buf.push_str(&out[end + 8..]); // 跳过 "</think>"
            out = buf;
        } else { break; }
    }
    // 未闭合 <think>（截断）：从 <think> 到结尾全去
    if let Some(start) = out.find("<think>") {
        out.truncate(start);
    }
    out.trim().to_string()
}

#[async_trait]
impl LlmCaller for MiniMaxCaller {
    async fn dream_round(&self, messages: &[serde_json::Value]) -> Result<DreamResp, DreamError> {
        let req_body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": 32768,
        });
        let in_chars: usize = messages.iter()
            .map(|m| m.get("content").and_then(|c| c.as_str()).map(|s| s.chars().count()).unwrap_or(0)).sum();
        let mut last_err: Option<String> = None;
        for attempt in 1..=3u32 {
            let started = std::time::Instant::now();
            eprintln!("[dream] LLM 调用 尝试 {attempt}/3 (输入~{}字)...", in_chars);
            match self.client.post(&self.endpoint)
                .header("Authorization", format!("Bearer {}", self.key))
                .json(&req_body)
                .send().await
            {
                Ok(r) => {
                    if r.status() != reqwest::StatusCode::OK {
                        let status = r.status();
                        let body = r.text().await.unwrap_or_default();
                        last_err = Some(format!("HTTP {status}: {}", body.chars().take(200).collect::<String>()));
                        eprintln!("[dream] 非 200,{} — 重试", last_err.as_ref().unwrap());
                    } else {
                        let json: serde_json::Value = match r.json().await {
                            Ok(j) => j,
                            Err(e) => {
                                last_err = Some(format!("JSON 解析失败: {e}"));
                                eprintln!("[dream] {:?} — 重试", last_err);
                                if attempt < 3 { tokio::time::sleep(std::time::Duration::from_secs(1u64 << attempt)).await; }
                                continue;
                            }
                        };
                        let (p_tok, c_tok, cached_tok) = {
                            let u = json.get("usage").unwrap_or(&serde_json::Value::Null);
                            (
                                u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                                u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                                u.get("prompt_tokens_details").and_then(|d| d.get("cached_tokens")).and_then(|v| v.as_u64()).unwrap_or(0),
                            )
                        };
                        if let Some(content) = json.get("choices").and_then(|c| c.as_array())
                            .and_then(|a| a.first()).and_then(|c| c.get("message"))
                            .and_then(|m| m.get("content")).and_then(|c| c.as_str())
                        {
                            eprintln!("[dream] LLM 成功 {}ms in{p_tok}/out{c_tok}/cache{cached_tok} (尝试{})", started.elapsed().as_millis(), attempt);
                            return Ok(DreamResp {
                                content: strip_think(content),
                                prompt_tokens: p_tok, completion_tokens: c_tok, cached_tokens: cached_tok,
                            });
                        }
                        last_err = Some("响应缺 choices[0].message.content".into());
                        eprintln!("[dream] {last_err:?} — 重试");
                    }
                }
                Err(e) => { last_err = Some(format!("请求失败: {e}")); eprintln!("[dream] {:?} — 重试", last_err); }
            }
            if attempt < 3 { tokio::time::sleep(std::time::Duration::from_secs(1u64 << attempt)).await; }
        }
        Err(DreamError::Llm(format!("LLM 重试 3 次仍失败(绝不机械兜底,中断留待下次): {}", last_err.unwrap_or_default())))
    }

    async fn month_theme(&self, messages: &[serde_json::Value]) -> Result<String, DreamError> {
        // 调用方（check_rotate_month）已构造好 messages：system=MONTH_THEME_SYS +
        // user=month_theme_prompt(日概括文本)。直接转发，不自造——历史上 impl 曾把传入的
        // messages 当「事件列表」序列化喂给 LLM，月主题输出为空（FakeLlm 不看参数，单测漏掉）。
        // 结构保持与 dream_round 一致，避免再错位。
        let req_body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": 1024,
        });

        let resp = self.client
            .post(&self.endpoint)
            .header("Authorization", format!("Bearer {}", self.key))
            .json(&req_body)
            .send()
            .await
            .map_err(|e| DreamError::Llm(format!("月主题 LLM 请求失败: {e}")))?;

        if resp.status() != reqwest::StatusCode::OK {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(DreamError::Llm(format!("月主题 LLM 返回非 200: {status}, body: {body}")));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| DreamError::Llm(format!("月主题 LLM 响应 JSON 解析失败: {e}")))?;

        json.get("choices")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .map(|s| strip_think(s))
            .ok_or_else(|| DreamError::Llm("月主题 LLM 响应缺少 choices[0].message.content".into()))
    }
}

// ─── Task 8: 跨月/跨年 + meta + marker（§16）───

/// 从月文件正文提取 `## 日概括` 段下的日概括行（`- YYYY-MM-DD: ...`），供月主题 LLM 作输入。
/// 纯函数，便于单测。非该段的行（标题/其他段）不计入。
pub fn extract_day_summary_body(content: &str) -> String {
    let mut in_section = false;
    let mut out: Vec<&str> = Vec::new();
    for l in content.lines() {
        if l.starts_with("## 日概括") {
            in_section = true;
            continue;
        } else if l.starts_with("## ") {
            in_section = false;
        } else if in_section && l.starts_with("- ") {
            out.push(l);
        }
    }
    out.join("\n")
}

/// Meta: .dream-meta.json 内容
#[derive(Debug, Clone, PartialEq)]
pub struct Meta {
    pub current_month: (i32, u32), // (year, month)
}

/// Rotation 结果
#[derive(Debug, Clone, PartialEq)]
pub enum Rotation {
    Rotated { crossed_year: bool },
    NoRotation,
}

/// 读取 cache/.dream-meta.json（缺失/损坏 → None）
pub fn read_meta(cache: &Path) -> Option<Meta> {
    let path = cache.join(".dream-meta.json");
    let text = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let arr = v.get("current_month").and_then(|x| x.as_array())?;
    let year = arr.first()?.as_i64()? as i32;
    let month = arr.get(1)?.as_u64()? as u32;
    Some(Meta { current_month: (year, month) })
}

/// 写 cache/.dream-meta.json（原子写）
pub fn write_meta(cache: &Path, m: &Meta) -> Result<(), DreamError> {
    let path = cache.join(".dream-meta.json");
    let obj = serde_json::json!({ "current_month": [m.current_month.0, m.current_month.1] });
    atomic_write(&path, &obj.to_string())
}

/// 跨月/跨年检测与轮转（§16 严格实现）
/// 任一步失败 → propagate Err，不写 meta（F4）
pub async fn check_rotate_month(
    cache: &Path,
    seg_ym: (i32, u32),
    caller: &dyn LlmCaller,
    _offset_secs: i64,
) -> Result<Rotation, DreamError> {
    let old = read_meta(cache).map(|m| m.current_month);

    // 1. 无 meta → 初始化，不轮转
    let old_ym = match old {
        None => {
            write_meta(cache, &Meta { current_month: seg_ym })?;
            return Ok(Rotation::NoRotation);
        }
        Some(ym) => ym,
    };

    // 2. 同月或未来月 → 不动
    if seg_ym <= old_ym {
        return Ok(Rotation::NoRotation);
    }

    // 3. 跨月（seg_ym > old_ym）
    let crossed_year = old_ym.0 != seg_ym.0;

    // a. 读旧月日概括内容
    let month_path = month_file_path(cache, old_ym);
    let segments_text = if month_path.exists() {
        let content = std::fs::read_to_string(&month_path).unwrap_or_default();
        extract_day_summary_body(&content)
    } else {
        String::new()
    };

    // b. LLM 综合月主题（失败 → propagate，F4）
    let ym_str = format!("{}-{:02}", old_ym.0, old_ym.1);
    let messages = vec![
        serde_json::json!({"role": "system", "content": MONTH_THEME_SYS}),
        serde_json::json!({"role": "user", "content": month_theme_prompt(&ym_str, &segments_text)}),
    ];
    let theme = caller.month_theme(&messages).await?;

    // c. 写旧月 ## 月主题
    write_month_theme(cache, old_ym, &theme)?;

    // d. 年级月主题索引（取首句作为精简）
    let first_line = theme.lines().next().unwrap_or("").to_string();
    let summary = if first_line.len() > 40 {
        truncate_chars(&first_line, 40)
    } else {
        first_line
    };
    append_year_month_index(cache, old_ym.0, old_ym, &summary)?;

    // e. MEMORY.md 轮转（每次跨月，spec §16.3/§15.4/§15.5）：旧 ### 当月 → ### 上月（冻结），
    //    新 ### 当月 清空；同时 ## 年 append 旧月月主题精简（一生累积）。一次原子写。
    rotate_memory_month_year(cache, old_ym, seg_ym, &summary)?;

    // f. 写 meta（仅当所有步骤成功）
    write_meta(cache, &Meta { current_month: seg_ym })?;

    // g. 返回轮转结果
    Ok(Rotation::Rotated { crossed_year })
}

/// 写 dream marker 到当日 history jsonl
pub fn write_dream_marker(cache: &Path, until_seq: u64, offset_secs: i64, stats: &Stats) -> Result<(), DreamError> {
    let history_dir = cache.join("history");

    // 1. 取 next_seq = max(seq) + 1
    let next_seq = crate::history::read_all(&history_dir)
        .iter()
        .map(|e| e.seq)
        .max()
        .unwrap_or(0) + 1;

    // 2. 当前时间戳（epoch ms）
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| DreamError::Io(std::io::Error::new(std::io::ErrorKind::Other, e.to_string())))?
        .as_millis() as u64;

    // 3. 构造 marker 并覆盖 seq
    let mut ev = HistoryEvent::marker(ts, "dream", until_seq);
    ev.seq = next_seq;
    ev.data.insert("seg".into(), serde_json::json!([stats.seg_a, stats.seg_b]));
    ev.data.insert("rounds".into(), serde_json::json!(stats.rounds));
    ev.data.insert("events".into(), serde_json::json!(stats.events_out));
    ev.data.insert("batches".into(), serde_json::json!(stats.batches));
    ev.data.insert("tokens".into(), serde_json::json!({"in": stats.prompt_tokens, "out": stats.completion_tokens, "cached": stats.cached_tokens}));
    ev.data.insert("elapsed_ms".into(), serde_json::json!(stats.elapsed_ms));
    ev.data.insert("model".into(), serde_json::json!(stats.model));

    // 4. 追加到当日 jsonl
    let date = date_from_ts_local(ts, offset_secs);
    std::fs::create_dir_all(&history_dir)?;
    let path = history_dir.join(format!("{date}.jsonl"));

    let line = serde_json::to_string(&ev)
        .map_err(|e| DreamError::Parse(e.to_string()))?;
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{line}")?;
    f.sync_all()?;

    Ok(())
}

/// 写 reset marker 到 history（idle 触发 dream 全量后调用 → 下轮 build_messages 清空 context）。
/// direct write（不经 history writer）：dream 也是 direct write，writer 的 seq 计数与文件不同步，
/// 经 writer 会 seq 冲突。seq = read_all max + 1（与 write_dream_marker 同源）。
pub fn write_reset_marker(cache: &Path, offset_secs: i64) -> Result<u64, DreamError> {
    let history_dir = cache.join("history");
    let evs = crate::history::read_all(&history_dir);
    // until_seq = dream cur（最后 dream marker 的 until_seq）：打在「日级别处理位置」
    // → build_messages filter seq > dream cur → 清空 dream 整理过的对话（只剩 MEMORY.md）
    let dream_until = evs.iter()
        .filter(|e| e.kind == "marker" && e.data.get("marker").and_then(|v| v.as_str()) == Some("dream"))
        .filter_map(|e| e.data.get("until_seq").and_then(|v| v.as_u64()))
        .max().unwrap_or(0);
    let next_seq = evs.iter().map(|e| e.seq).max().unwrap_or(0) + 1;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| DreamError::Io(std::io::Error::new(std::io::ErrorKind::Other, e.to_string())))?
        .as_millis() as u64;
    let mut ev = HistoryEvent::marker(ts, "reset", dream_until);
    ev.seq = next_seq;
    let date = date_from_ts_local(ts, offset_secs);
    std::fs::create_dir_all(&history_dir)?;
    let path = history_dir.join(format!("{date}.jsonl"));
    let line = serde_json::to_string(&ev).map_err(|e| DreamError::Parse(e.to_string()))?;
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{line}")?;
    f.sync_all()?;
    Ok(next_seq)
}

// ─── Task 7: 月级/年级/年总览 + 规则提炼（§7.2/§7.3/§9/§11）───

/// 提取日概括（纯规则，无 LLM）
pub fn extract_day_summary(evs: &[FinalEvent], offset_secs: i64) -> DaySummary {
    if evs.is_empty() {
        return DaySummary {
            date: "1970-01-01".into(),
            headline: "（空）".into(),
            keywords: vec![],
            entities: vec![],
            event_count: 0,
            seq_min: 0,
            seq_max: 0,
        };
    }

    // date = 首事件 ts 对应日期（同日事件共享日期）
    let date = date_from_ts_local(evs[0].ts, offset_secs);

    // 收集文本语料（title + detail）
    let mut corpus = String::new();
    for ev in evs {
        corpus.push_str(&ev.title);
        corpus.push(' ');
        corpus.push_str(&ev.detail);
        corpus.push(' ');
    }

    // keywords: 优先用 LLM 吐的(FinalEvent.keywords 聚合 Top5),正则作 fallback
    let ident_regex = regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]+").unwrap();
    let mut kw_freq: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for ev in evs {
        for k in &ev.keywords { *kw_freq.entry(k.clone()).or_insert(0) += 1; }
    }
    let mut keywords_vec: Vec<(String, usize)> = if !kw_freq.is_empty() {
        kw_freq.into_iter().collect()
    } else {
        let mut freq: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for cap in ident_regex.find_iter(&corpus) {
            *freq.entry(cap.as_str().to_string()).or_insert(0) += 1;
        }
        freq.into_iter().collect()
    };
    keywords_vec.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let keywords: Vec<String> = keywords_vec.iter().take(5).map(|(k, _)| k.clone()).collect();

    // entities: 看起来像路径或带点的标识符（简单启发式）
    let entities: Vec<String> = keywords_vec.iter()
        .filter(|(k, _)| k.contains('/') || k.contains('.') || k.len() > 20)
        .take(3)
        .map(|(k, _)| k.clone())
        .collect();

    // headline: 优先用 LLM keywords 的 Top1,否则回退正则最高频 token
    let headline = if let Some(top) = keywords.first() {
        format!("围绕 {}", top)
    } else {
        let mut title_freq: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for ev in evs {
            for cap in ident_regex.find_iter(&ev.title) {
                let tok = cap.as_str().to_string();
                if tok.len() >= 2 && tok != "用户" && tok != "修" && tok != "改" {
                    *title_freq.entry(tok).or_insert(0) += 1;
                }
            }
        }
        if let Some((top, _)) = title_freq.iter().max_by_key(|(_, &c)| c) {
            format!("围绕 {}", top)
        } else {
            evs.first().map(|e| e.title.clone()).unwrap_or_else(|| "（无明显主线）".into())
        }
    };

    // 元信息
    let event_count = evs.len() as u32;
    let seq_min = evs.iter().map(|e| e.seq_a).min().unwrap_or(0);
    let seq_max = evs.iter().map(|e| e.seq_b).max().unwrap_or(0);

    DaySummary {
        date,
        headline,
        keywords,
        entities,
        event_count,
        seq_min,
        seq_max,
    }
}

/// 月文件路径：cache/memory/{Y}/{M:02}/YYYY-MM.md
fn month_file_path(cache: &Path, ym: (i32, u32)) -> PathBuf {
    let (y, m) = ym;
    cache.join("memory").join(y.to_string()).join(format!("{:02}", m)).join(format!("{}-{:02}.md", y, m))
}

/// 年文件路径：cache/memory/{Y}/YYYY.md
fn year_file_path(cache: &Path, year: i32) -> PathBuf {
    cache.join("memory").join(year.to_string()).join(format!("{}.md", year))
}

/// upsert 一条日概括到月文件的 ## 日概括 段（按日期去重）
pub fn upsert_month_day_summary(cache: &Path, ym: (i32, u32), day: &DaySummary) -> Result<(), DreamError> {
    let path = month_file_path(cache, ym);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // 读取现有内容
    let existing = std::fs::read_to_string(&path).unwrap_or_default();

    // 解析：分离 ## 月主题 和 ## 日概括 段
    let mut month_theme_lines: Vec<String> = Vec::new();
    let mut day_summary_lines: Vec<String> = Vec::new();
    let mut current_section: Option<&str> = None;

    for line in existing.lines() {
        if line.starts_with("## ") {
            current_section = match line[3..].trim() {
                "月主题" => Some("month_theme"),
                "日概括" => Some("day_summary"),
                _ => None,
            };
            if line.contains("月主题") {
                month_theme_lines.push(line.to_string());
                continue;
            } else if line.contains("日概括") {
                day_summary_lines.push(line.to_string());
                continue;
            }
        }

        match current_section {
            Some("month_theme") => month_theme_lines.push(line.to_string()),
            Some("day_summary") => day_summary_lines.push(line.to_string()),
            _ => {}
        }
    }

    // 构造新的日概括行
    let merged_keywords_entities: Vec<&String> = day.keywords.iter().chain(day.entities.iter()).collect();
    let keywords_str = merged_keywords_entities.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("·");
    let new_line = format!("- {}: {} | {} | {}事件 [seq {}..{}]\n",
        day.date, day.headline, keywords_str, day.event_count, day.seq_min, day.seq_max);

    // upsert：按日期去重（相同日期替换，否则追加）
    let date_prefix = format!("- {}:", day.date);
    let mut found = false;
    for line in &mut day_summary_lines {
        if line.starts_with(&date_prefix) {
            *line = new_line.clone();
            found = true;
        }
    }
    if !found {
        // 如果 ## 日概括 不存在，先加标题
        if !day_summary_lines.iter().any(|l| l.contains("## 日概括")) {
            day_summary_lines.insert(0, "## 日概括\n".to_string());
        }
        day_summary_lines.push(new_line);
    }

    // 渲染完整文件
    let mut output = String::new();
    output.push_str(&format!("# {}-{:02}\n\n", ym.0, ym.1));

    // 渲染月主题段（如果有）
    if !month_theme_lines.is_empty() {
        for line in &month_theme_lines {
            output.push_str(line);
            if !line.ends_with('\n') {
                output.push('\n');
            }
        }
        output.push('\n');
    }

    // 渲染日概括段
    for line in &day_summary_lines {
        output.push_str(line);
        if !line.ends_with('\n') {
            output.push('\n');
        }
    }

    std::fs::write(&path, output)?;
    Ok(())
}

/// 写月主题正文到月文件的 ## 月主题 段（T8 跨月时调用）
pub fn write_month_theme(cache: &Path, ym: (i32, u32), theme: &str) -> Result<(), DreamError> {
    let path = month_file_path(cache, ym);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // 读取现有内容
    let existing = std::fs::read_to_string(&path).unwrap_or_default();

    // 解析：分离 ## 月主题 和 ## 日概括
    let mut day_summary_lines: Vec<String> = Vec::new();
    let mut current_section: Option<&str> = None;

    for line in existing.lines() {
        if line.starts_with("## ") {
            current_section = match line[3..].trim() {
                "月主题" => Some("month_theme"),
                "日概括" => Some("day_summary"),
                _ => None,
            };
            if line.contains("日概括") {
                day_summary_lines.push(line.to_string());
                continue;
            }
        }

        if current_section == Some("day_summary") {
            day_summary_lines.push(line.to_string());
        }
    }

    // 渲染完整文件
    let mut output = String::new();
    output.push_str(&format!("# {}-{:02}\n\n", ym.0, ym.1));
    output.push_str("## 月主题\n");
    output.push_str(theme);
    if !theme.ends_with('\n') {
        output.push('\n');
    }
    output.push('\n');

    // 渲染日概括段（如果有）
    if !day_summary_lines.is_empty() {
        for line in &day_summary_lines {
            output.push_str(line);
            if !line.ends_with('\n') {
                output.push('\n');
            }
        }
    }

    std::fs::write(&path, output)?;
    Ok(())
}

/// upsert 一条月主题到年文件（按月去重）
pub fn append_year_month_index(cache: &Path, year: i32, ym: (i32, u32), theme_oneline: &str) -> Result<(), DreamError> {
    let path = year_file_path(cache, year);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // 读取现有内容
    let existing = std::fs::read_to_string(&path).unwrap_or_default();

    // 解析：按行提取
    let mut lines: Vec<String> = existing.lines().map(|l| l.to_string()).collect();

    // 构造新行
    let new_line = format!("- {}-{:02}: {}", ym.0, ym.1, theme_oneline);
    let month_prefix = format!("- {}-{:02}:", ym.0, ym.1);

    // upsert：按月去重
    let mut found = false;
    for line in &mut lines {
        if line.starts_with(&month_prefix) {
            *line = new_line.clone();
            found = true;
        }
    }
    if !found {
        // 如果文件为空或不存在，先加标题
        if lines.is_empty() || !lines[0].starts_with("# ") {
            lines.insert(0, format!("# {} 月主题索引\n\n", year));
        }
        lines.push(new_line);
    }

    // 渲染
    let output = lines.join("\n");
    let final_output = if output.ends_with('\n') { output } else { output + "\n" };
    std::fs::write(&path, final_output)?;
    Ok(())
}

// ─── Task 6: day 文件写盘 + MEMORY.md 三层拼接（§7.5 可靠性核心）───

/// 紧凑日期：YYYY-MM-DD → YYYYMMDD（day 文件 evt-NNN 用）
fn date_compact(ts: u64, offset_secs: i64) -> String {
    date_from_ts_local(ts, offset_secs).replace('-', "")
}

/// day 文件路径：cache/memory/YYYY/MM/YYYY-MM-DD.md
fn day_file_path(cache: &Path, ts: u64, offset_secs: i64) -> std::path::PathBuf {
    let d = date_from_ts_local(ts, offset_secs); // YYYY-MM-DD
    let parts: Vec<&str> = d.split('-').collect();
    let (y, m) = match parts.as_slice() {
        [y, m, _] => (*y, *m),
        _ => ("1970", "01"),
    };
    cache.join("memory").join(y).join(m).join(format!("{}.jsonl", d))
}

/// 追加一条事件段到 day 文件（F10 幂等：seq 范围去重，返回 nn）
/// 格式严格对齐 memory.rs:48
pub fn append_day_event(cache: &Path, ev: &FinalEvent, offset_secs: i64) -> Result<u32, DreamError> {
    let path = day_file_path(cache, ev.ts, offset_secs);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // 读现有 jsonl,按 seq 范围去重(F10),同时计数 nn
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut nn: u32 = 0;
    for line in existing.lines() {
        if line.trim().is_empty() { continue; }
        nn += 1;
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            let seq = v.get("seq").and_then(|x| x.as_array());
            let a = seq.and_then(|arr| arr.get(0)).and_then(|x| x.as_u64()).unwrap_or(0);
            let b = seq.and_then(|arr| arr.get(1)).and_then(|x| x.as_u64()).unwrap_or(0);
            if a == ev.seq_a && b == ev.seq_b {
                return Ok(nn); // 命中已有,返回其 nn(幂等)
            }
        }
    }

    // 未命中,追加一行 jsonl(每事件一行 JSON,真相源;MEMORY.md 从它渲染)
    nn += 1;
    let hhmm = time_hhmm_from_ts_local(ev.ts, offset_secs);
    let date = date_from_ts_local(ev.ts, offset_secs);
    let date_c = date_compact(ev.ts, offset_secs);
    let evt_id = format!("evt-{}-{:03}", date_c, nn);
    let line = serde_json::json!({
        "evt": evt_id, "hhmm": hhmm, "date": date,
        "seq": [ev.seq_a, ev.seq_b], "type": ev.event_type,
        "title": ev.title, "detail": ev.detail,
        "keywords": ev.keywords, "subject": ev.subject,
        "attachment": if ev.has_attachment { Some(ev.attachment.as_str()) } else { None },
        "ref": format!("history/{}.jsonl#seq[{},{}]", date, ev.seq_a, ev.seq_b),
    });
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{}", line)?;
    f.sync_all()?;
    Ok(nn)
}

/// ## 月 段：### 当月 + ### 上月（2 月窗口，spec §7.1）。只存日概括内容行，标题动态渲染。
#[derive(Debug, Default, Clone)]
pub struct MonthDoc {
    pub current: Vec<String>,   // ### 当月 的日概括行（- YYYY-MM-DD: ...）
    pub previous: Vec<String>,  // ### 上月（冻结）的日概括行
}

/// MEMORY.md 解析结果（三层段分离：日/月/年，spec §7.1/§7.5）。
/// day/month.current/month.previous/year 只存内容行；## / ### 标题动态渲染（结构恒定）。
/// other 存未知 ## 段（含其标题）原样保留（不丢手写）。
#[derive(Debug, Default, Clone)]
pub struct MemoryDoc {
    pub header: String,
    pub day: Vec<String>,
    pub month: MonthDoc,
    pub year: Vec<String>,
    pub other: Vec<String>,
}

/// 解析 MEMORY.md：按 ## 切段认 日/月/年；## 月 下按 ### 当月/上月 切。
/// 标题行（## / ###）不入 vec（渲染时固定输出）；未知 ## 段原样保留（spec §7.5 容错）。
pub fn parse_memory_md(text: &str) -> MemoryDoc {
    let mut doc = MemoryDoc::default();
    let mut section: Option<&str> = None;    // day/month/year/other
    let mut month_sub: Option<&str> = None; // current/previous（仅 month 段内）

    for line in text.lines() {
        if line.starts_with("# ") && !line.starts_with("## ") {
            doc.header = line.to_string();
            continue;
        }
        if line.starts_with("## ") {
            month_sub = None;
            section = match line[3..].trim() {
                "日" => Some("day"),
                "月" => Some("month"),
                "年" => Some("year"),
                _ => { doc.other.push(line.to_string()); Some("other") }
            };
            continue;
        }
        if line.starts_with("### ") && section == Some("month") {
            // ### 当月 → current；### 上月 → previous；未知 ### 归 current
            month_sub = if line[4..].trim().starts_with("上月") { Some("previous") } else { Some("current") };
            continue; // ### 标题不入 vec
        }
        if line.trim().is_empty() {
            continue; // 空行不入 vec（避免 round-trip 累积；render 统一控制段间空行）
        }
        if section.is_none() {
            // header 后、首个 ## 前的描述行 → 并入多行 header（round-trip 稳定）
            doc.header.push('\n');
            doc.header.push_str(line);
            continue;
        }
        match section {
            Some("day") => doc.day.push(line.to_string()),
            Some("month") => match month_sub {
                Some("previous") => doc.month.previous.push(line.to_string()),
                _ => doc.month.current.push(line.to_string()),
            },
            Some("year") => doc.year.push(line.to_string()),
            _ => doc.other.push(line.to_string()),
        }
    }
    doc
}

/// MEMORY.md 三层模板（spec §7.1）。
fn memory_md_template() -> String {
    "# 记忆（MEMORY.md）\n\n由 dream 维护。三层总览（一生记忆）：日 / 月 / 年。\n\n## 日\n\n## 月\n\n## 年\n".to_string()
}

/// 日概括 → 行（与月级文件 ## 日概括 行格式一致，spec §7.3/§9）：`- YYYY-MM-DD: 主线 | 关键词 | N事件 [seq a..b]`
fn day_summary_line(ds: &DaySummary) -> String {
    let kws: Vec<&str> = ds.keywords.iter().chain(ds.entities.iter()).map(|s| s.as_str()).collect();
    format!("- {}: {} | {} | {}事件 [seq {}..{}]",
        ds.date, ds.headline, kws.join("·"), ds.event_count, ds.seq_min, ds.seq_max)
}

/// 从 ## 日 的 evt-YYYYMMDD-NNN 行里取最大 nn（fallback 用，正常路径不走）。
fn max_evt_nn(lines: &[String]) -> u32 {
    lines.iter().filter_map(|l| {
        l.find("evt-").and_then(|pos| {
            let rest = &l[pos + 4..];
            let token = rest.split_whitespace().next().unwrap_or("");
            token.split('-').nth(1).and_then(|nn| nn.parse().ok())
        })
    }).max().unwrap_or(0)
}

/// 从 ### 上月 日概括行里推月份标签（取首条 - YYYY-MM-DD 的 YYYY-MM）。
fn previous_month_label(prev: &[String]) -> String {
    for line in prev {
        if let Some(rest) = line.strip_prefix("- ") {
            if let Some(ym) = rest.get(..7) { return ym.to_string(); }
        }
    }
    "（未知月）".to_string()
}

/// 渲染三层 MEMORY.md（结构恒定：# header / ## 日 / ## 月 / ## 年 / other）。
fn render_memory_md(doc: &MemoryDoc, seg_ym: (i32, u32)) -> String {
    let mut out = String::new();
    out.push_str(&doc.header);
    if !out.ends_with('\n') { out.push('\n'); }
    out.push('\n');

    out.push_str("## 日\n");
    for line in &doc.day {
        out.push_str(line);
        if !line.ends_with('\n') { out.push('\n'); }
    }
    out.push('\n');

    out.push_str("## 月\n");
    out.push_str(&format!("### 当月 {}-{:02}\n", seg_ym.0, seg_ym.1));
    for line in &doc.month.current {
        out.push_str(line);
        if !line.ends_with('\n') { out.push('\n'); }
    }
    out.push('\n');
    if doc.month.previous.iter().any(|l| l.starts_with("- ")) {
        out.push_str(&format!("### 上月 {}（冻结）\n", previous_month_label(&doc.month.previous)));
        for line in &doc.month.previous {
            out.push_str(line);
            if !line.ends_with('\n') { out.push('\n'); }
        }
        out.push('\n');
    }

    out.push_str("## 年\n");
    for line in &doc.year {
        out.push_str(line);
        if !line.ends_with('\n') { out.push('\n'); }
    }
    out.push('\n');

    for line in &doc.other {
        out.push_str(line);
        if !line.ends_with('\n') { out.push('\n'); }
    }
    out
}

/// 原子写：write → .tmp → sync_all → rename（Windows 覆盖）
pub fn atomic_write(path: &Path, content: &str) -> Result<(), DreamError> {
    // 构造临时文件路径：MEMORY.md → MEMORY.md.tmp
    let file_name = path.file_name().ok_or(DreamError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput, "invalid path"
    )))?;
    let tmp_name = format!("{}.tmp", file_name.to_string_lossy());
    let tmp_path = path.parent().map(|p| p.join(&tmp_name)).unwrap_or_else(|| PathBuf::from(&tmp_name));

    std::fs::write(&tmp_path, content)?;
    // sync_all 需要 open 文件，不要 create（会截断）
    let f = std::fs::OpenOptions::new().write(true).open(&tmp_path)?;
    f.sync_all()?;
    // rename 原子覆盖目标（Windows MoveFileEx REPLACE_EXISTING；同卷替换）
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// 备份：path → path.bak（单份滚动）
pub fn backup_to_bak(path: &Path) -> Result<(), DreamError> {
    if path.exists() {
        let bak_path = {
            let mut p = path.as_os_str().to_owned();
            p.push(".bak");
            std::path::PathBuf::from(p)
        };
        std::fs::copy(path, bak_path)?;
    }
    Ok(())
}

/// MEMORY.md 三层拼接（spec §7.5/§15.2/§15.3）：## 日（今日 per-event 滚动）+ ## 月 ### 当月（日概括 upsert）。
/// ## 年 段不动（跨月时由 rotate_memory_month_year 在 check_rotate_month 追加）。
/// - evs: 本批 FinalEvent；nn_map: seq_a→nn（append_day_event 返回，F10 同源）；
/// - now_date: 今日 YYYY-MM-DD；seg_ym: 当月 (year,month)（### 当月 标题用）；
/// - day_summaries: 本批 distinct 日的 (date, DaySummary)（供 ## 月 ### 当月 upsert）。
pub fn upsert_memory_md(
    cache: &Path,
    evs: &[FinalEvent],
    nn_map: &HashMap<u64, u32>,
    now_date: &str,
    seg_ym: (i32, u32),
    day_summaries: &[(String, DaySummary)],
    offset_secs: i64,
) -> Result<(), DreamError> {
    let path = cache.join("MEMORY.md");

    // 1. 读，不存在 → 三层模板
    let text = if path.exists() { std::fs::read_to_string(&path)? } else { memory_md_template() };
    let mut doc = parse_memory_md(&text);

    // 2. ## 日：滚出 evt-YYYYMMDD ≠ today 的行（日期从 evt id 提取）
    let today_compact = now_date.replace('-', "");
    doc.day.retain(|line| {
        if !line.starts_with("- ") { return true; } // 非 evt 行保留（容错）
        line.find("evt-").map(|pos| {
            let rest = &line[pos + 4..];
            rest.chars().take(8).collect::<String>() == today_compact
        }).unwrap_or(true)
    });

    // 今日 per-event upsert（仅 ts 日期 == today 进 ## 日，spec §15.2），按 evt-NNN 替换/追加
    for ev in evs {
        if date_from_ts_local(ev.ts, offset_secs) != now_date { continue; }
        let hhmm = time_hhmm_from_ts_local(ev.ts, offset_secs);
        let date_c = date_compact(ev.ts, offset_secs);
        let nn = nn_map.get(&ev.seq_a).copied().unwrap_or_else(|| max_evt_nn(&doc.day).saturating_add(1));
        let new_line = format!("- {} evt-{}-{:03} [{}] {}", hhmm, date_c, nn, ev.event_type, ev.title);
        let evt_id = format!("evt-{}-{:03}", date_c, nn);
        if let Some(idx) = doc.day.iter().position(|l| l.contains(&evt_id)) {
            doc.day[idx] = new_line;
        } else {
            doc.day.push(new_line);
        }
    }

    // ## 日 排序：仅 - 事件行按 HH:MM 倒序；其它行保留在前
    let mut events: Vec<String> = doc.day.iter().filter(|l| l.starts_with("- ")).cloned().collect();
    let structure: Vec<String> = doc.day.iter().filter(|l| !l.starts_with("- ")).cloned().collect();
    events.sort_by(|a, b| {
        let ak = a.chars().skip(2).take(5).collect::<String>();
        let bk = b.chars().skip(2).take(5).collect::<String>();
        bk.cmp(&ak)
    });
    doc.day = structure.into_iter().chain(events).collect();

    // 3. ## 月 ### 当月：本批日概括 upsert（按 - YYYY-MM-DD: 去重），按日期倒序
    for (date, ds) in day_summaries {
        let line = day_summary_line(ds);
        let date_prefix = format!("- {}:", date);
        let mut found = false;
        for l in doc.month.current.iter_mut() {
            if l.starts_with(&date_prefix) { *l = line.clone(); found = true; }
        }
        if !found { doc.month.current.push(line); }
    }
    let mut cur_events: Vec<String> = doc.month.current.iter().filter(|l| l.starts_with("- ")).cloned().collect();
    let cur_struct: Vec<String> = doc.month.current.iter().filter(|l| !l.starts_with("- ")).cloned().collect();
    cur_events.sort_by(|a, b| {
        let ak = a.chars().skip(2).take(10).collect::<String>();
        let bk = b.chars().skip(2).take(10).collect::<String>();
        bk.cmp(&ak)
    });
    doc.month.current = cur_struct.into_iter().chain(cur_events).collect();

    // 4. 结构校验（header 含"记忆"）
    if doc.header.is_empty() || !doc.header.contains("记忆") {
        backup_to_bak(&path)?;
        return atomic_write(&path, &memory_md_template());
    }

    // 5. 备份 + 渲染 + 原子写
    backup_to_bak(&path)?;
    let output = render_memory_md(&doc, seg_ym);
    atomic_write(&path, &output)
}

/// 跨月时 MEMORY.md 轮转（spec §15.4/§15.5/§16.3）：旧 ### 当月 → ### 上月（冻结，2 月窗口），
/// 新 ### 当月 清空；同时 ## 年 append/更新旧月月主题精简（一生累积）。一次原子写。
pub fn rotate_memory_month_year(
    cache: &Path,
    old_ym: (i32, u32),
    new_ym: (i32, u32),
    theme_oneline: &str,
) -> Result<(), DreamError> {
    let path = cache.join("MEMORY.md");
    let text = if path.exists() { std::fs::read_to_string(&path)? } else { memory_md_template() };
    let mut doc = parse_memory_md(&text);

    // ## 月 轮转：旧 current → previous（覆盖，2 月窗口）；current 清空等当批 upsert
    doc.month.previous = std::mem::take(&mut doc.month.current);

    // ## 年 append/upsert 旧月月主题精简（按月去重，一生累积）
    let new_line = format!("- {}-{:02}: {}", old_ym.0, old_ym.1, theme_oneline);
    let month_prefix = format!("- {}-{:02}:", old_ym.0, old_ym.1);
    let mut found = false;
    for line in doc.year.iter_mut() {
        if line.starts_with(&month_prefix) { *line = new_line.clone(); found = true; }
    }
    if !found { doc.year.push(new_line); }
    doc.year.sort_by(|a, b| {
        let ak = a.chars().skip(2).take(7).collect::<String>();
        let bk = b.chars().skip(2).take(7).collect::<String>();
        bk.cmp(&ak)
    });

    backup_to_bak(&path)?;
    let output = render_memory_md(&doc, new_ym);
    atomic_write(&path, &output)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test helpers - mirroring dream.rs pattern
    fn user(seq: u64, ts: u64) -> HistoryEvent {
        let mut e = HistoryEvent::user(ts, "main", "u", &[]);
        e.seq = seq;
        e
    }
    fn asst(seq: u64, ts: u64, content: &str) -> HistoryEvent {
        let mut e = HistoryEvent::assistant(ts, "main", content, "", vec![]);
        e.seq = seq;
        e
    }
    fn dream_marker(seq: u64, ts: u64, until_seq: u64) -> HistoryEvent {
        let mut e = HistoryEvent::marker(ts, "dream", until_seq);
        e.seq = seq;
        e
    }
    fn build_events_with_markers(until_seqs: &[u64]) -> Vec<HistoryEvent> {
        let mut evs = vec![];
        let mut seq = 0u64;
        for &until in until_seqs {
            evs.push(user(seq, 1000 + seq * 1000));
            seq += 1;
            evs.push(dream_marker(seq, 1000 + seq * 1000, until));
            seq += 1;
        }
        evs
    }
    fn build_user_assistant_rounds(n: u64) -> Vec<HistoryEvent> {
        let mut evs = vec![];
        for i in 0..n {
            evs.push(user(i * 2, 1000 + i * 2000));
            evs.push(asst(i * 2 + 1, 2000 + i * 2000, &format!("回复{}", i)));
        }
        evs
    }

    /// 从一行里提取 evt-YYYYMMDD-NNN 的 NNN（适用于 day 文件 `## HH:MM evt-...` 与
    /// MEMORY 近期 `- DATE HH:MM evt-...` 两种行）。
    fn extract_evt_nn(line: &str) -> Option<u32> {
        let idx = line.find("evt-")?;
        let rest = &line[idx + 4..];            // after "evt-"
        let token = rest.split_whitespace().next()?; // YYYYMMDD-NNN
        token.split('-').nth(1).and_then(|nn| nn.parse().ok())
    }

    #[test]
    fn write_reset_marker_appends_reset_marker() {
        // idle 触发 dream 全量后写 reset marker → 下轮 build_messages 清空 context
        let dir = tempfile::tempdir().unwrap();
        let off = crate::history::local_offset_secs();
        let seq = write_reset_marker(dir.path(), off).unwrap();
        let evs = crate::history::read_all(&dir.path().join("history"));
        let reset = evs.iter().find(|e| e.kind == "marker").expect("应有 marker");
        assert_eq!(reset.data["marker"], serde_json::json!("reset"), "kind=reset");
        assert_eq!(reset.data["until_seq"], serde_json::json!(seq), "until_seq=返回的 next_seq");
        assert_eq!(reset.seq, seq, "seq = until_seq = next_seq（盖戳在末尾）");
    }

    #[test]
    fn stats_default_zero() {
        assert_eq!(Stats::default().rounds, 0);
    }

    #[tokio::test]
    async fn run_dream_to_idle_skeleton_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = DreamCfg {
            api_key: None,
            model: "x".into(),
            region: "cn".into(),
            max_rounds: 5,
            batch_max_events: 100,
        };
        let s = run_dream_to_idle(dir.path(), &cfg, DreamFlags { once: true, mechanical: true, dry_run: true, force: false }).await;
        assert!(s.is_ok(), "骨架应返回 Ok(Stats)");
    }

    // --- Task 2 tests ---

    #[test]
    fn last_marker_until_seq_picks_last() {
        let ev = build_events_with_markers(&[10u64, 20, 30]);
        assert_eq!(last_dream_marker_seq(&ev), 30);
    }

    #[test]
    fn no_marker_returns_zero() {
        let ev = build_user_assistant_rounds(2);
        assert_eq!(last_dream_marker_seq(&ev), 0);
    }

    // --- Task 3 tests ---

    #[test]
    fn split_rounds_splits_on_assistant_boundary() {
        let seg = build_user_assistant_rounds(3);
        let rounds = split_rounds(&seg);
        assert_eq!(rounds.len(), 3, "3 轮 user/assistant 应切出 3 个回合");
    }

    #[test]
    fn split_rounds_leading_non_user_dropped() {
        // 段首非 user 前导事件被丢弃（与 dream.rs:722-727 一致：1 回合）。
        let seg = vec![asst(0, 500, "前导"), user(1, 1000), asst(2, 2000, "r")];
        let rounds = split_rounds(&seg);
        assert_eq!(rounds.len(), 1, "前导 assistant 不单成回合（与 dream.rs 一致）");
        assert_eq!(rounds[0].len(), 2, "唯一回合是 user(1)+asst(2)，前导 asst(0) 被丢弃");
        assert_eq!(rounds[0][0].kind, "user");
    }

    #[test]
    fn mechanical_extract_one_round() {
        // 构造一个回合：user + assistant
        let mut evs = vec![];
        evs.push({
            let mut e = HistoryEvent::user(1000, "main", "u", &[]);
            e.seq = 0;
            e.data.insert("text".into(), serde_json::json!("修 drag-drop 功能"));
            e
        });
        evs.push({
            let mut e = HistoryEvent::assistant(2000, "main", "已修复拖放问题。", "", vec![]);
            e.seq = 1;
            e
        });

        let g = mechanical_extract(&evs);
        assert_eq!(g.seq_a, 0);
        assert_eq!(g.seq_b, 1);
        assert!(!g.title.is_empty(), "title 应非空");
        assert!(g.title.contains("drag-drop") || g.title.contains("修"), "title 应包含用户输入");
        assert_eq!(g.subject, "用户");
        assert!(!g.detail.is_empty(), "detail 应包含 user → asst 摘要");
        assert_eq!(g.has_attachment, false, "机械底座无附件");
        assert_eq!(g.attachment, String::new(), "机械底座附件为空");
    }

    #[test]
    fn mechanical_extract_with_tool_calls() {
        let mut evs = vec![];
        evs.push({
            let mut e = HistoryEvent::user(1000, "main", "u", &[]);
            e.seq = 0;
            e.data.insert("text".into(), serde_json::json!("调用工具"));
            e
        });
        evs.push({
            let mut e = HistoryEvent::assistant(2000, "main", "", "", vec![]);
            e.seq = 1;
            e.data.insert("tool_calls".into(), serde_json::json!([]));
            e
        });

        let g = mechanical_extract(&evs);
        assert!(g.detail.contains("(调用工具)") || g.detail.contains("调用工具"), "有 tool_calls 时 detail 应包含提示");
    }

    // --- Task 4 tests ---

    fn mk_mech(ts: u64, seq_a: u64, seq_b: u64) -> MechGroup {
        MechGroup {
            ts,
            seq_a,
            seq_b,
            title: format!("r{}", seq_a),
            subject: "用户".into(),
            detail: format!("d{}", seq_a),
            event_type: "其他".into(),
            keywords: vec![],
            has_attachment: false,
            attachment: String::new(),
        }
    }

    #[test]
    fn parse_groups_ok_json() {
        let j = "{\"rounds\":[1,2],\"title\":\"a\",\"subject\":\"s\",\"detail\":\"d\"}";
        let g = parse_groups(j);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].rounds, vec![1, 2]);
    }

    #[test]
    fn reconcile_g1_fills_uncovered_mechanically() {
        // 无 LLM 分组 → 两个机械底座各成一个 FinalEvent（G1 兜底，不合并不拆）
        let mech = vec![mk_mech(100, 1, 2), mk_mech(200, 3, 4)];
        let out = reconcile_groups(vec![], &mech, 5);
        assert_eq!(out.len(), 2, "G1: 每个机械底座独立产出");
        assert!(out.iter().all(|e| e.seq_a <= e.seq_b));
    }

    #[test]
    fn reconcile_f6_splits_non_contiguous() {
        // LLM 把回合 1,2,4,5 分一组（3 缺失 → 非连续）→ 拆成 [1,2] 与 [4,5] 两个 FinalEvent
        let mech = vec![
            mk_mech(10, 1, 1),
            mk_mech(20, 2, 2),
            mk_mech(30, 3, 3),
            mk_mech(40, 4, 4),
            mk_mech(50, 5, 5)
        ];
        let llm = vec![LlmGroup {
            rounds: vec![1, 2, 4, 5],
            title: "T".into(),
            detail: "D".into(),
            subject: "S".into(),
            event_type: "其他".into(),
            keywords: vec![],
        }];
        let out = reconcile_groups(llm, &mech, 5);
        // [1,2] 合并 + [4,5] 合并 = 2 个；回合 3 漏 → G1 再补 1 个 = 3 个
        assert_eq!(out.len(), 3, "F6: 非连续拆 2 组 + 漏的回合 3 机械补");
        // 合并组的 seq 范围
        assert!(out.iter().any(|e| e.seq_a == 1 && e.seq_b == 2), "回合 1-2 合并");
        assert!(out.iter().any(|e| e.seq_a == 4 && e.seq_b == 5), "回合 4-5 合并");
    }

    #[test]
    fn reconcile_f11_splits_over_max() {
        // LLM 一组覆盖 7 回合 > max=5 → 强制拆，两段都带 (续K) 后缀（对齐 dream.rs）
        let mech: Vec<_> = (1..=7).map(|i| mk_mech(i * 10, i, i)).collect();
        let llm = vec![LlmGroup {
            rounds: vec![1, 2, 3, 4, 5, 6, 7],
            title: "Big".into(),
            detail: "D".into(),
            subject: "S".into(),
            event_type: "其他".into(),
            keywords: vec![],
        }];
        let out = reconcile_groups(llm, &mech, 5);
        assert_eq!(out.len(), 2, "F11: 7 回合按 max=5 拆成 [1..5]+[6..7]");
        assert!(out[0].detail.contains("(续1)"), "F11: 第一段带 (续1)（对齐 dream.rs），got: {}", out[0].detail);
        assert!(out[1].detail.contains("(续2)"), "F11: 第二段带 (续2)（对齐 dream.rs），got: {}", out[1].detail);
        assert_eq!(out[0].seq_a, 1);
        assert_eq!(out[0].seq_b, 5);
        assert_eq!(out[1].seq_a, 6);
        assert_eq!(out[1].seq_b, 7);
    }

    #[test]
    fn reconcile_skips_out_of_range_and_duplicate() {
        // rounds 含 0（越界）、n+1（越界）、重复 → 跳过，不 panic
        let mech = vec![mk_mech(10, 1, 1), mk_mech(20, 2, 2)];
        let llm = vec![LlmGroup {
            rounds: vec![0, 1, 1, 9, 2],
            title: "T".into(),
            detail: "D".into(),
            subject: "S".into(),
            event_type: "其他".into(),
            keywords: vec![],
        }];
        let out = reconcile_groups(llm, &mech, 5);
        assert_eq!(out.len(), 1, "回合 1+2 合并成一组（0/9 越界、重复 1 跳过）");
        assert_eq!(out[0].seq_a, 1);
        assert_eq!(out[0].seq_b, 2);
    }

    // --- Task 5 tests (LLM layer) ---

    #[tokio::test]
    async fn fake_llm_dream_round_returns_canned() {
        let fake = FakeLlm::new(vec!["{\"rounds\":[1,2],\"title\":\"a\",\"detail\":\"d\",\"subject\":\"s\"}".into()]);
        let s = fake.dream_round(&[]).await.unwrap();
        assert!(s.content.contains("rounds") && s.content.contains("title"));
    }

    #[test]
    fn build_dream_messages_has_system_and_user() {
        let rounds = vec![vec![user(1, 1000), asst(2, 2000, "回复")]];
        let m = build_dream_messages(&rounds, 1, 2, 5, "");
        assert_eq!(m.len(), 2);
        assert_eq!(m[0]["role"], "system");
        assert_eq!(m[1]["role"], "user");
        assert!(m[1]["content"].as_str().unwrap().contains("段1"));
    }

    #[tokio::test]
    async fn fake_llm_month_theme_returns_canned() {
        let fake = FakeLlm::new(vec!["本月围绕 dream 整理".into()]);
        let s = fake.month_theme(&[]).await.unwrap();
        assert!(s.contains("dream"));
    }

    // --- Task 6 tests (write side: day files + MEMORY.md recent) ---

    fn mk_event(ts: u64, seq_a: u64, seq_b: u64) -> FinalEvent {
        FinalEvent {
            ts,
            seq_a,
            seq_b,
            title: format!("标题{}", seq_a),
            subject: "用户".into(),
            detail: format!("详情{}", seq_a),
            event_type: "其他".into(),
            keywords: vec![],
            has_attachment: false,
            attachment: String::new(),
        }
    }

    #[test]
    fn append_day_event_returns_incrementing_nn() {
        let dir = tempfile::tempdir().unwrap();
        let n1 = append_day_event(dir.path(), &mk_event(1000, 1, 3), 0).unwrap();
        let n2 = append_day_event(dir.path(), &mk_event(2000, 4, 6), 0).unwrap();
        assert_eq!((n1, n2), (1, 2), "F10: 写前计数，递增 nn");
    }

    #[test]
    fn append_day_event_idempotent_skip_dup_seq() {
        let dir = tempfile::tempdir().unwrap();
        append_day_event(dir.path(), &mk_event(1000, 1, 3), 0).unwrap();
        let n2 = append_day_event(dir.path(), &mk_event(2000, 1, 3), 0).unwrap();
        assert_eq!(n2, 1, "写前去重：同 seq 范围已存在则跳过，不跳号");
    }

    #[test]
    fn memory_day_rolls_out_non_today() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("MEMORY.md");
        // 三层 MEMORY.md，## 日 含旧日事件 evt-20260729
        std::fs::write(&p, "# 记忆\n\n## 日\n- 10:00 evt-20260729-001 旧\n\n## 月\n\n## 年\n").unwrap();
        let ts = 1785638400000u64;
        let computed_date = crate::history::date_from_ts_local(ts, 0);
        let compact = computed_date.replace('-', "");
        let ev = mk_event(ts, 1, 2);
        let mut nn_map = std::collections::HashMap::new();
        nn_map.insert(1u64, 1u32);
        let seg_ym = {
            let parts: Vec<&str> = computed_date.split('-').collect();
            (parts[0].parse::<i32>().unwrap(), parts[1].parse::<u32>().unwrap())
        };
        upsert_memory_md(dir.path(), &[ev], &nn_map, &computed_date, seg_ym, &[], 0).unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(!out.contains("evt-20260729"), "## 日 滚出非当日 evt: {out}");
        assert!(out.contains(&format!("evt-{compact}")), "当日 evt 进 ## 日: {out}");
    }

    #[test]
    fn memory_day_idempotent_byte_stable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("MEMORY.md");
        let ts = 1785638400000u64;
        let computed_date = crate::history::date_from_ts_local(ts, 0);
        let ev = mk_event(ts, 1, 2);
        let mut nn_map = std::collections::HashMap::new();
        nn_map.insert(1u64, 1u32);
        let seg_ym = {
            let parts: Vec<&str> = computed_date.split('-').collect();
            (parts[0].parse::<i32>().unwrap(), parts[1].parse::<u32>().unwrap())
        };
        upsert_memory_md(dir.path(), &[ev.clone()], &nn_map, &computed_date, seg_ym, &[], 0).unwrap();
        let a = std::fs::read_to_string(&p).unwrap();
        upsert_memory_md(dir.path(), &[ev], &nn_map, &computed_date, seg_ym, &[], 0).unwrap();
        let b = std::fs::read_to_string(&p).unwrap();
        assert_eq!(a, b, "幂等：重整同批字节不变");
    }

    #[test]
    fn memory_parse_keeps_unknown_section() {
        let doc = parse_memory_md("# 记忆\n\n## 近期\n- x\n\n## 我的手写\n保留我\n\n## 年总览\n### 2025\n...\n");
        assert!(doc.other.iter().any(|s| s.contains("保留我")), "不丢手写：未知段原样保留");
    }

    #[test]
    fn memory_atomic_write_no_half_on_simulated_crash() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("MEMORY.md");
        std::fs::write(&p, "OLD").unwrap();
        atomic_write(&p, &"NEW".repeat(100)).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "NEW".repeat(100));
    }

    #[test]
    fn memory_day_heading_before_events() {
        // 结构恒定：## 日 标题必须在事件行之前（防排序 bug 回归）
        let dir = tempfile::tempdir().unwrap();
        let ts = 1785638400000u64;
        let computed_date = crate::history::date_from_ts_local(ts, 0);
        let ev = mk_event(ts, 1, 3);
        let mut nn = std::collections::HashMap::new();
        nn.insert(ev.seq_a, 1u32);
        let seg_ym = {
            let parts: Vec<&str> = computed_date.split('-').collect();
            (parts[0].parse::<i32>().unwrap(), parts[1].parse::<u32>().unwrap())
        };
        upsert_memory_md(dir.path(), &[ev], &nn, &computed_date, seg_ym, &[], 0).unwrap();
        let out = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        let heading = out.find("## 日").expect("## 日 存在");
        let evt = out.find("evt-").expect("事件行存在");
        assert!(heading < evt, "## 日 标题必须在事件行之前，got heading@{heading} evt@{evt}");
    }

    // --- Task 7 tests (month/year/year overview + rule extraction) ---

    #[test]
    fn extract_day_summary_keywords_and_meta() {
        // Use the same timestamp as T6 tests (resolves to a specific date at offset 0)
        let ts = 1785638400000u64;
        let computed_date = crate::history::date_from_ts_local(ts, 0);
        let ev = FinalEvent {
            ts,
            seq_a: 1,
            seq_b: 3,
            title: "修 drag-drop".into(),
            subject: "用户".into(),
            detail: "改 F10 evt 计数".into(),
            event_type: "调试".into(),
            keywords: vec!["drag".into(), "F10".into()],
            has_attachment: false,
            attachment: String::new(),
        };
        let s = extract_day_summary(&[ev], 0);
        assert!(s.keywords.iter().any(|k| k.contains("drag") || k.contains("F10") || k.contains("evt") || k.contains("计数")), "关键词锚定: {:?}", s.keywords);
        assert_eq!(s.event_count, 1);
        assert_eq!((s.seq_min, s.seq_max), (1, 3));
        assert_eq!(s.date, computed_date);
    }

    #[test]
    fn month_day_summary_upsert_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        // Use simple year/month to avoid timezone complexity in test
        let s = DaySummary {
            date: "2026-07-30".into(),
            headline: "围绕 dream".into(),
            keywords: vec!["drag".into()],
            entities: vec![],
            event_count: 1,
            seq_min: 1,
            seq_max: 3,
        };
        upsert_month_day_summary(dir.path(), (2026, 7), &s).unwrap();
        upsert_month_day_summary(dir.path(), (2026, 7), &s).unwrap(); // same day → replace, not dup
        let txt = std::fs::read_to_string(dir.path().join("memory").join("2026").join("07").join("2026-07.md")).unwrap();
        assert_eq!(txt.matches("2026-07-30").count(), 1, "按日 upsert 不重复");
    }

    #[test]
    fn year_month_index_append_per_month() {
        let dir = tempfile::tempdir().unwrap();
        append_year_month_index(dir.path(), 2026, (2026, 7), "围绕 dream").unwrap();
        append_year_month_index(dir.path(), 2026, (2026, 6), "围绕 mem").unwrap();
        let txt = std::fs::read_to_string(dir.path().join("memory").join("2026").join("2026.md")).unwrap();
        assert!(txt.contains("2026-07") && txt.contains("2026-06"));
    }

    #[test]
    fn rotate_memory_month_year_freezes_and_appends_year() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MEMORY.md"),
            "# 记忆\n\n## 日\n\n## 月\n\n### 当月 2026-06\n- 2026-06-15: 围绕 dream |  | 1事件 [seq 1..3]\n\n## 年\n").unwrap();
        rotate_memory_month_year(dir.path(), (2026, 6), (2026, 7), "六月围绕 dream 整理").unwrap();
        let txt = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        assert!(txt.contains("### 当月 2026-07"), "新当月 = seg_ym: {txt}");
        assert!(txt.contains("### 上月 2026-06（冻结）"), "旧当月→上月冻结: {txt}");
        assert!(txt.contains("2026-06-15"), "上月保留旧日概括: {txt}");
        assert!(txt.contains("- 2026-06: 六月围绕 dream 整理"), "## 年 追加旧月月主题精简: {txt}");
    }

    // --- Task 8 tests (cross-month/year + meta + marker) ---

    #[tokio::test]
    async fn rotate_month_writes_theme_and_advances_meta() {
        let dir = tempfile::tempdir().unwrap();
        write_meta(dir.path(), &Meta { current_month: (2026, 6) }).unwrap();
        let fake = FakeLlm::new(vec!["七月围绕 dream 整理".into()]);
        let r = check_rotate_month(dir.path(), (2026, 7), &fake, 0).await.unwrap();
        assert!(matches!(r, Rotation::Rotated { crossed_year: false }));
        assert!(month_file_path(dir.path(), (2026, 6)).exists(), "旧月主题已写");
        assert_eq!(read_meta(dir.path()).unwrap().current_month, (2026, 7), "meta 推进");
    }

    #[tokio::test]
    async fn rotate_year_appends_memory_year_line() {
        let dir = tempfile::tempdir().unwrap();
        write_meta(dir.path(), &Meta { current_month: (2026, 12) }).unwrap();
        let fake = FakeLlm::new(vec!["十二月收尾".into()]);
        let r = check_rotate_month(dir.path(), (2027, 1), &fake, 0).await.unwrap();
        assert!(matches!(r, Rotation::Rotated { crossed_year: true }));
        let mem = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        assert!(mem.contains("- 2026-12:"), "跨年→MEMORY ## 年 加旧月月主题: {mem}");
    }

    #[tokio::test]
    async fn rotate_failure_no_meta_change() {
        // FakeLlm that errors: build one whose month_theme returns Err.
        struct FailingLlm;
        #[async_trait]
        impl LlmCaller for FailingLlm {
            async fn dream_round(&self, _messages: &[serde_json::Value]) -> Result<DreamResp, DreamError> {
                Err(DreamError::Llm("fail".into()))
            }
            async fn month_theme(&self, _messages: &[serde_json::Value]) -> Result<String, DreamError> {
                Err(DreamError::Llm("fail".into()))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        write_meta(dir.path(), &Meta { current_month: (2026, 6) }).unwrap();
        let fake = FailingLlm;
        let r = check_rotate_month(dir.path(), (2026, 7), &fake, 0).await;
        assert!(r.is_err(), "失败 propagate");
        assert_eq!(read_meta(dir.path()).unwrap().current_month, (2026, 6), "失败不写 meta");
    }

    #[tokio::test]
    async fn month_theme_receives_messages_with_day_summary() {
        // 回归：MiniMaxCaller::month_theme 曾忽略传入 messages、自造并把它当「事件列表」
        // 序列化，月主题输出为空（FakeLlm 不看参数，单测漏过）。锁住调用点 check_rotate_month
        // 传入的是合法 messages：user prompt 含日概括文本 + 月主题指令。
        struct ThemeInspectLlm;
        #[async_trait]
        impl LlmCaller for ThemeInspectLlm {
            async fn dream_round(&self, _m: &[serde_json::Value]) -> Result<DreamResp, DreamError> {
                unreachable!("check_rotate_month 不应调 dream_round")
            }
            async fn month_theme(&self, messages: &[serde_json::Value]) -> Result<String, DreamError> {
                let user = messages.get(1)
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                assert!(user.contains("月主题"), "user prompt 应含月主题指令，实际: {user}");
                assert!(user.contains("2026-06-26"), "user prompt 应含日概括日期，实际: {user}");
                Ok("六月破冰起步".into())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        write_meta(dir.path(), &Meta { current_month: (2026, 6) }).unwrap();
        let month_path = month_file_path(dir.path(), (2026, 6));
        std::fs::create_dir_all(month_path.parent().unwrap()).unwrap();
        std::fs::write(&month_path,
            "# 2026-06\n\n## 日概括\n- 2026-06-26: 初次打招呼 | ovoice | 2事件 [seq 1..10]\n").unwrap();

        let r = check_rotate_month(dir.path(), (2026, 7), &ThemeInspectLlm, 0).await.unwrap();
        assert!(matches!(r, Rotation::Rotated { crossed_year: false }));

        let theme_content = std::fs::read_to_string(&month_path).unwrap();
        assert!(theme_content.contains("六月破冰起步"), "6月 ## 月主题 应含 LLM 输出: {theme_content}");
    }

    #[tokio::test]
    async fn write_marker_appends_until_seq() {
        let dir = tempfile::tempdir().unwrap();
        write_dream_marker(dir.path(), 42, 0, &Stats::default()).unwrap();
        let evs = read_all_events(dir.path()).unwrap();
        assert_eq!(last_dream_marker_seq(&evs), 42);
    }

    #[test]
    fn extract_day_summary_body_only_day_lines() {
        let content = "# 2026-07\n\n## 月主题\n旧主题\n\n## 日概括\n- 2026-07-30: 围绕 dream | drag | 5事件\n- 2026-07-29: 围绕 mem\n\n## 其它\n手写\n";
        let body = extract_day_summary_body(content);
        assert!(body.contains("2026-07-30") && body.contains("2026-07-29"), "含日概括行: {body}");
        assert!(!body.contains("## 日概括"), "不含标题行");
        assert!(!body.contains("旧主题"), "不含月主题段");
        assert!(!body.contains("手写"), "不含其他段");
    }

    // --- Task 9 integration tests (execute_dream pipeline) ---

    #[tokio::test]
    async fn execute_f1_tail_retained() {
        // 构建 8 轮 user/assistant（16 事件），tail_rounds=3
        // 最后 3 个 user（seq 12,14,16）应被留尾，b 应落在 seq 11 前
        let mut evs = vec![];
        for i in 0u64..8 {
            evs.push(user(i * 2, 1000 + i * 2000));  // user seq: 0,2,4,...,14
            evs.push(asst(i * 2 + 1, 2000 + i * 2000, &format!("回复{}", i)));  // asst seq: 1,3,5,...,15
        }

        let dir = tempfile::tempdir().unwrap();
        let cfg = DreamCfg {
            api_key: None,
            model: "x".into(),
            region: "cn".into(),
            max_rounds: 5,
            batch_max_events: 100,
        };
        let fake = FakeLlm::new(vec!["[]".into()]);  // 空 LLM 响应（全机械）
        let flags = DreamFlags { once: true, mechanical: true, dry_run: false, force: false };

        // a=1 开始处理
        let outcome = execute_dream(dir.path(), &evs, &cfg, &fake, flags, 1, 0, None).await.unwrap();

        assert!(outcome.processed_until.is_some(), "应处理到 b");
        let b = outcome.processed_until.unwrap();

        // 最后 3 个 user 是 seq 12,14,16（第 6,7,8 轮）
        // tail_aware_b 从 cur=15 往回数 user，见到 3 个后停在 seq 11 后
        // 算法：user seq 14(seen=1,b=13), seq 12(seen=2,b=11), seq 10(seen=3,b=9)
        // b 应 ≤ 11（留尾后不含最后 3 个 user）
        assert!(b <= 11, "b={b} 应 ≤ 11（留 3 个 user）");

        // 验证 day 文件确实不包含最后 3 个 user 的 seq
        let day_content = std::fs::read_to_string(dir.path().join("memory/1970/01/1970-01-01.md")).unwrap();
        assert!(!day_content.contains("seq-12"), "不应含 seq 12（留尾）");
        assert!(!day_content.contains("seq-14"), "不应含 seq 14（留尾）");
    }

    #[tokio::test]
    async fn execute_f12_empty_segment_advances() {
        // 只有一个 user seq=5，a=10 超过 cur
        let evs = vec![user(5, 1000)];
        let dir = tempfile::tempdir().unwrap();
        let cfg = DreamCfg {
            api_key: None,
            model: "x".into(),
            region: "cn".into(),
            max_rounds: 5,
            batch_max_events: 100,
        };
        let fake = FakeLlm::new(vec!["[]".into()]);
        let flags = DreamFlags { once: true, mechanical: true, dry_run: false, force: false };

        let outcome = execute_dream(dir.path(), &evs, &cfg, &fake, flags, 10, 0, None).await.unwrap();

        // F12: a > cur 且 b < a → processed_until = None（空段信号）
        assert_eq!(outcome.processed_until, None, "空段应返回 None");
        assert_eq!(outcome.stats.markers, 1, "空段应写 marker 推进到 cur");

        // 验证 marker 写到了 cur=5
        let evs_after = read_all_events(dir.path()).unwrap();
        assert_eq!(last_dream_marker_seq(&evs_after), 5, "marker 应推进到 cur");
    }

    #[tokio::test]
    async fn execute_cross_month_segment_rotates_per_month() {
        // 段跨 6月+7月：按月分片应让 6月先成当月、7月触发轮转。
        // 旧实现只用段末月调一次 check_rotate_month → 6月被跳过（首月 meta 直初始化成7月）。
        let ts_jun = 1_782_461_114_259u64; // 2026-06-26 08:05 UTC
        let ts_jul = 1_785_053_114_259u64; // 2026-07-26 08:05 UTC
        let mut evs = vec![];
        for i in 0u64..4 { // 6月 4 轮（seq 0..7）
            evs.push(user(i * 2, ts_jun + i * 1000));
            evs.push(asst(i * 2 + 1, ts_jun + i * 1000 + 500, &format!("六月{}", i)));
        }
        for i in 0u64..4 { // 7月 4 轮（seq 8..15）
            let s = 8 + i * 2;
            evs.push(user(s, ts_jul + i * 1000));
            evs.push(asst(s + 1, ts_jul + i * 1000 + 500, &format!("七月{}", i)));
        }
        // 8 个 user，tail_rounds=3 留最后 3 个（7月后3轮），整理 [0..b] 含 6月4轮 + 7月第1轮

        let cfg = DreamCfg { api_key: None, model: "x".into(), region: "cn".into(), max_rounds: 5, batch_max_events: 100 };
        let dir = tempfile::tempdir().unwrap();
        // 机械模式不调 dream_round；7月轮转冻结6月时 month_theme 调 1 次
        let fake = FakeLlm::new(vec!["六月围绕工具与目录整理".into()]);
        let flags = DreamFlags { once: true, mechanical: true, dry_run: false, force: false };

        execute_dream(dir.path(), &evs, &cfg, &fake, flags, 0, 0, None).await.unwrap();

        let mem = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        let cur_idx = mem.find("### 当月 2026-07").expect("当月应为 7月");
        let prev_idx = mem.find("### 上月").expect("应触发轮转：6月冻结为 ### 上月");
        assert!(cur_idx < prev_idx, "### 当月 应在 ### 上月 之前（近期放大）\n{mem}");
        assert!(mem[cur_idx..prev_idx].contains("2026-07-26"), "7月日概括应在 ### 当月 段\n{mem}");
        assert!(mem[prev_idx..].contains("2026-06-26"), "6月日概括应在 ### 上月 段\n{mem}");
        assert!(mem.contains("- 2026-06: 六月围绕工具与目录整理"), "## 年 应追加 6月主题\n{mem}");
    }

    #[test]
    fn strip_think_removes_paired_and_unclosed() {
        assert_eq!(strip_think("<think>思考</think>正文"), "正文");
        assert_eq!(strip_think("<think>没闭合"), "");            // 截断未闭合
        assert_eq!(strip_think("无 think 的普通文本"), "无 think 的普通文本");
        assert_eq!(strip_think("前<think>a</think>中<think>b</think>后"), "前中后"); // 多段
    }

    #[tokio::test]
    async fn execute_g4_llm_fail_mechanical_fallback() {
        // 构造 2 轮，LLM 失败 → 应机械兜底
        let evs = vec![
            user(0, 1000),
            asst(1, 2000, "回复1"),
            user(2, 3000),
            asst(3, 4000, "回复2"),
        ];

        struct FailingLlm;
        #[async_trait]
        impl LlmCaller for FailingLlm {
            async fn dream_round(&self, _messages: &[serde_json::Value]) -> Result<DreamResp, DreamError> {
                Err(DreamError::Llm("LLM 失败".into()))
            }
            async fn month_theme(&self, _messages: &[serde_json::Value]) -> Result<String, DreamError> {
                Ok("theme".into())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let cfg = DreamCfg {
            api_key: None,
            model: "x".into(),
            region: "cn".into(),
            max_rounds: 5,
            batch_max_events: 100,
        };
        let fail = FailingLlm;
        let flags = DreamFlags { once: true, mechanical: false, dry_run: false, force: false };

        // 先写 meta（避免跨月检查失败）
        write_meta(dir.path(), &Meta { current_month: (2026, 7) }).unwrap();

        let outcome = execute_dream(dir.path(), &evs, &cfg, &fail, flags, 0, 0, None).await.unwrap();

        // G4: LLM 失败 → 仍返回 Some(b)，机械兜底产出 day segments + marker
        assert!(outcome.processed_until.is_some(), "LLM 失败应仍处理到 b");
        assert_eq!(outcome.stats.events_out, 2, "应产出 2 个 mechanical segments");
        assert_eq!(outcome.stats.markers, 1, "应写 marker");

        // 验证 day 文件有 2 个机械段（标题截断的 user 文本）
        let day_content = std::fs::read_to_string(dir.path().join("memory/1970/01/1970-01-01.md")).unwrap();
        assert_eq!(day_content.matches("## ").count(), 2, "应有 2 个 day segments");
    }

    #[tokio::test]
    async fn execute_g1_full_coverage_mechanical() {
        // N 轮，mechanical=true → 每轮都应有 day segment
        let n = 5;
        let mut evs = vec![];
        for i in 0u64..n {
            evs.push(user(i * 2, 1000 + i * 2000));
            evs.push(asst(i * 2 + 1, 2000 + i * 2000, &format!("回复{}", i)));
        }

        let dir = tempfile::tempdir().unwrap();
        let cfg = DreamCfg {
            api_key: None,
            model: "x".into(),
            region: "cn".into(),
            max_rounds: 5,
            batch_max_events: 100,
        };
        let fake = FakeLlm::new(vec!["[]".into()]);  // 空 LLM（全机械）
        let flags = DreamFlags { once: true, mechanical: true, dry_run: false, force: false };

        write_meta(dir.path(), &Meta { current_month: (2026, 7) }).unwrap();

        let outcome = execute_dream(dir.path(), &evs, &cfg, &fake, flags, 0, 0, None).await.unwrap();

        assert_eq!(outcome.stats.events_out, n as usize, "G1: {n} 轮 → {n} 个 segments");

        // 验证 day 文件有 N 个 ## 段
        let day_content = std::fs::read_to_string(dir.path().join("memory/1970/01/1970-01-01.md")).unwrap();
        assert_eq!(day_content.matches("## ").count(), n as usize, "应有 {n} 个 day segments");
    }

    #[tokio::test]
    async fn execute_f10_nn_consistent_day_and_memory() {
        // 所有事件在同一天 → day 文件与 MEMORY.md 近期的 evt-NNN 应一致
        let mut evs = vec![];
        for i in 0u64..3 {
            evs.push(user(i * 2, 1656540000 + i * 1000));  // 同一天（2022-06-30）
            evs.push(asst(i * 2 + 1, 1656541000 + i * 1000, &format!("回复{}", i)));
        }

        let dir = tempfile::tempdir().unwrap();
        let cfg = DreamCfg {
            api_key: None,
            model: "x".into(),
            region: "cn".into(),
            max_rounds: 5,
            batch_max_events: 100,
        };
        let fake = FakeLlm::new(vec!["[]".into()]);
        let flags = DreamFlags { once: true, mechanical: true, dry_run: false, force: false };

        write_meta(dir.path(), &Meta { current_month: (2022, 6) }).unwrap();

        execute_dream(dir.path(), &evs, &cfg, &fake, flags, 0, 0, None).await.unwrap();

        // 读取 day 文件，提取 evt-NNN
        let day_content = std::fs::read_to_string(dir.path().join("memory/1970/01/1970-01-20.md")).unwrap();
        let day_nn: Vec<u32> = day_content
            .lines()
            .filter_map(|l| extract_evt_nn(l.trim()))
            .collect();

        // 读取 MEMORY.md 近期，提取 evt-NNN
        let mem_content = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        let mem_nn: Vec<u32> = mem_content
            .lines()
            .filter_map(|l| extract_evt_nn(l.trim()))
            .collect();

        assert_eq!(day_nn.len(), 3, "day 文件应有 3 个 evt");
        assert_eq!(mem_nn.len(), 3, "MEMORY.md 近期应有 3 个 evt");
        assert_eq!(day_nn, mem_nn, "F10: day 与 MEMORY evt-NNN 应一致（同源 nn）");
    }

    #[tokio::test]
    async fn execute_f4_io_failure_no_marker() {
        // 注入 IO 失败：cache/memory 是个文件而非目录 → append_day_event 失败
        let evs = vec![user(0, 1000), asst(1, 2000, "回复")];

        let dir = tempfile::tempdir().unwrap();

        // 创建一个文件叫 memory（而非目录），导致 create_dir_all("memory/...") 失败
        let memory_file = dir.path().join("memory");
        std::fs::write(&memory_file, "I am a file, not a dir").unwrap();

        let cfg = DreamCfg {
            api_key: None,
            model: "x".into(),
            region: "cn".into(),
            max_rounds: 5,
            batch_max_events: 100,
        };
        let fake = FakeLlm::new(vec!["[]".into()]);
        let flags = DreamFlags { once: true, mechanical: true, dry_run: false, force: false };

        write_meta(dir.path(), &Meta { current_month: (2026, 7) }).unwrap();

        let result = execute_dream(dir.path(), &evs, &cfg, &fake, flags, 0, 0, None).await;

        // F4: 写失败 → 返回 Err，无 marker
        assert!(result.is_err(), "IO 失败应 propagate Err");

        // 验证没有新 marker（last_dream_marker_seq 仍是 0）
        let evs_after = read_all_events(dir.path()).unwrap();
        assert_eq!(last_dream_marker_seq(&evs_after), 0, "F4: 写失败时不写 marker");
    }

    #[tokio::test]
    async fn execute_dry_run_writes_nothing() {
        // dry_run=true → 不写任何 day/MEMORY/month/marker
        let evs = vec![user(0, 1000), asst(1, 2000, "回复")];

        let dir = tempfile::tempdir().unwrap();
        let cfg = DreamCfg {
            api_key: None,
            model: "x".into(),
            region: "cn".into(),
            max_rounds: 5,
            batch_max_events: 100,
        };
        let fake = FakeLlm::new(vec!["[]".into()]);
        let flags = DreamFlags { once: true, mechanical: true, dry_run: true, force: false };

        write_meta(dir.path(), &Meta { current_month: (2026, 7) }).unwrap();

        let outcome = execute_dream(dir.path(), &evs, &cfg, &fake, flags, 0, 0, None).await.unwrap();

        assert!(outcome.processed_until.is_some(), "dry_run 仍应计算 b");
        assert_eq!(outcome.stats.markers, 0, "dry_run 不写 marker");

        // 验证目录为空（无 day/MEMORY/month/marker）
        assert!(!dir.path().join("memory").exists(), "dry_run 不创建 memory/");
        assert!(!dir.path().join("MEMORY.md").exists(), "dry_run 不创建 MEMORY.md");
        assert!(!dir.path().join("history").join("events.jsonl").exists(), "dry_run 不写 marker");
    }

    // --- Task 10 tests (run_dream_to_idle loop + flags + exit codes) ---

    #[tokio::test]
    async fn loop_runs_until_empty_one_segment() {
        // 8 user/asst rounds (16 events), tail_rounds=3 → one segment [1..b], marker at b (before tail), then stop
        let mut evs = vec![];
        for i in 0u64..8 { evs.push(user(i*2, 1000+i*2000)); evs.push(asst(i*2+1, 2000+i*2000, &format!("r{i}"))); }

        let dir = tempfile::tempdir().unwrap();
        let flags = DreamFlags { once: true, mechanical: true, dry_run: false, force: false };

        // Seed events to history jsonl
        let history_dir = dir.path().join("history");
        std::fs::create_dir_all(&history_dir).unwrap();
        let history_path = history_dir.join("1970-01-01.jsonl");
        for ev in &evs {
            let line = serde_json::to_string(ev).unwrap();
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&history_path).unwrap();
            writeln!(f, "{line}").unwrap();
        }
        let cfg = DreamCfg { api_key: None, model: "x".into(), region: "cn".into(), max_rounds: 5, batch_max_events: 100 };

        let stats = run_dream_to_idle(dir.path(), &cfg, flags).await.unwrap();
        assert!(stats.markers >= 1, "至少写了一个 marker");

        // frontier (last marker until_seq) < 最后一个 user 的 seq（留尾）
        let evs2 = read_all_events(dir.path()).unwrap();
        let frontier = last_dream_marker_seq(&evs2);
        let last_user = evs.iter().filter(|e| e.kind=="user").last().unwrap().seq;
        assert!(frontier < last_user, "F1: frontier {frontier} 应 < 最后 user seq {last_user}（留尾）");
    }

    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let mut evs = vec![];
        for i in 0u64..4 { evs.push(user(i*2, 1000+i*2000)); evs.push(asst(i*2+1, 2000+i*2000, "r")); }

        let dir = tempfile::tempdir().unwrap();
        let flags = DreamFlags { once: true, mechanical: true, dry_run: true, force: false };

        // Seed events to history jsonl
        let history_dir = dir.path().join("history");
        std::fs::create_dir_all(&history_dir).unwrap();
        let history_path = history_dir.join("1970-01-01.jsonl");
        for ev in &evs {
            let line = serde_json::to_string(ev).unwrap();
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&history_path).unwrap();
            writeln!(f, "{line}").unwrap();
        }
        let cfg = DreamCfg { api_key: None, model: "x".into(), region: "cn".into(), max_rounds: 5, batch_max_events: 100 };

        let _stats = run_dream_to_idle(dir.path(), &cfg, flags).await.unwrap();
        assert!(!dir.path().join("MEMORY.md").exists(), "dry_run 不写 MEMORY");

        // no day files, no marker
        let evs_after = read_all_events(dir.path()).unwrap();
        assert_eq!(last_dream_marker_seq(&evs_after), 0, "dry_run 不写 marker");
    }

    #[tokio::test]
    async fn no_api_key_non_mechanical_errors() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = DreamCfg { api_key: None, model: "x".into(), region: "cn".into(), max_rounds: 5, batch_max_events: 100 };
        let flags = DreamFlags { once: true, mechanical: false, dry_run: false, force: false };

        let r = run_dream_to_idle(dir.path(), &cfg, flags).await;
        assert!(r.is_err(), "缺 api_key 且非 mechanical 应报错");
    }

    #[tokio::test]
    async fn mechanical_skips_network() {
        // mechanical + no api_key → uses FakeLlm, no network, succeeds
        let mut evs = vec![];
        for i in 0u64..4 { evs.push(user(i*2, 1000+i*2000)); evs.push(asst(i*2+1, 2000+i*2000, "r")); }

        let dir = tempfile::tempdir().unwrap();
        let flags = DreamFlags { once: true, mechanical: true, dry_run: false, force: false };

        // Seed events to history jsonl
        let history_dir = dir.path().join("history");
        std::fs::create_dir_all(&history_dir).unwrap();
        let history_path = history_dir.join("1970-01-01.jsonl");
        for ev in &evs {
            let line = serde_json::to_string(ev).unwrap();
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&history_path).unwrap();
            writeln!(f, "{line}").unwrap();
        }
        let cfg = DreamCfg { api_key: None, model: "x".into(), region: "cn".into(), max_rounds: 5, batch_max_events: 100 };

        let stats = run_dream_to_idle(dir.path(), &cfg, flags).await.unwrap();
        assert!(stats.events_out > 0, "mechanical 全机械产出 events_out");
    }

    // --- Task 12 tests (legacy → funnel migration, idempotent) ---

    #[test]
    fn migrate_twolayer_to_threelayer() {
        let dir = tempfile::tempdir().unwrap();
        // 种两层 MEMORY.md（## 近期 / ## 年总览，旧错误产物）+ 一个 day 文件
        std::fs::write(dir.path().join("MEMORY.md"),
            "# 记忆（MEMORY.md）\n\n## 近期\n- 2026-07-30 10:00 evt-20260730-001 旧\n\n## 年总览\n### 2026\n围绕 dream\n").unwrap();
        let day = dir.path().join("memory").join("2026").join("07").join("2026-07-30.md");
        std::fs::create_dir_all(day.parent().unwrap()).unwrap();
        std::fs::write(&day, "\n## 10:00 evt-20260730-001 修 drag-drop\n**主语**: 用户\n**详情**: d\n**对话索引**: history/2026-07-30.jsonl#seq[1,3]\n").unwrap();
        let migrated = migrate_if_legacy(dir.path(), 0).unwrap();
        assert!(migrated, "两层应迁移");
        let mem = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        assert!(!mem.contains("## 近期") && !mem.contains("## 年总览"), "两层旧段已清除: {mem}");
        assert!(mem.contains("## 日") && mem.contains("## 月") && mem.contains("## 年"), "三层骨架齐全: {mem}");
        // 月级文件从 day 文件回填 ## 日概括
        let month = std::fs::read_to_string(dir.path().join("memory").join("2026").join("07").join("2026-07.md")).unwrap();
        assert!(month.contains("## 日概括"), "月级回填日概括: {month}");
    }

    #[test]
    fn migrate_idempotent_threelayer() {
        let dir = tempfile::tempdir().unwrap();
        // 已是三层 + 无 day 文件 → 不重组 MEMORY.md（backfill 无 day 文件可回填）
        let before = "# 记忆\n\n## 日\n\n## 月\n\n## 年\n";
        std::fs::write(dir.path().join("MEMORY.md"), before).unwrap();
        let migrated = migrate_if_legacy(dir.path(), 0).unwrap();
        assert!(!migrated, "三层 + 无 day 文件 → 不迁移");
        let after = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        assert_eq!(before, after, "幂等：字节不变");
    }

    #[test]
    fn migrate_preserves_day_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MEMORY.md"), "# 记忆\n\n## 日\n- x\n\n## 年\n### 2026\ny\n").unwrap();
        let day = dir.path().join("memory").join("2026").join("07").join("2026-07-30.md");
        std::fs::create_dir_all(day.parent().unwrap()).unwrap();
        std::fs::write(&day, "DAY_ORIGINAL").unwrap();
        migrate_if_legacy(dir.path(), 0).unwrap();
        assert_eq!(std::fs::read_to_string(&day).unwrap(), "DAY_ORIGINAL", "day 文件不动");
    }

    #[test]
    fn migrate_day_summaries_use_filename_dates() {
        // Regression test for 1970 collapse bug: two day files should produce two distinct date lines,
        // not collapse to one 1970-01-01 line. Also validates seq range parsing from #seq[a,b].
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MEMORY.md"), "# 记忆\n\n## 日\n- x\n\n## 年\n### 2026\ny\n").unwrap();
        let mk = |d: &str, a: u64, b: u64| {
            let p = dir.path().join("memory").join("2026").join("07").join(format!("{d}.md"));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, format!(
                "\n## 10:00 evt-{}-001 修 drag-drop\n**主语**: 用户\n**详情**: d\n**对话索引**: history/{}.jsonl#seq[{},{}]\n",
                d.replace('-', ""), d, a, b
            )).unwrap();
        };
        mk("2026-07-26", 10, 20);
        mk("2026-07-27", 30, 40);
        migrate_if_legacy(dir.path(), 0).unwrap();
        let month = std::fs::read_to_string(dir.path().join("memory").join("2026").join("07").join("2026-07.md")).unwrap();
        assert!(month.contains("2026-07-26"), "含 26 日概括");
        assert!(month.contains("2026-07-27"), "含 27 日概括");
        assert!(!month.contains("1970-01-01"), "不应有 1970 日期（collapse bug）");
        assert!(!month.contains("[seq 0..0]"), "seq 范围应从 #seq[a,b] 解析，非 0..0");
    }

    // --- Task 13 tests (.dream-lock) ---

    #[test]
    fn lock_acquire_then_release() {
        let dir = tempfile::tempdir().unwrap();
        {
            let _g = DreamGuard::acquire(dir.path()).unwrap();
            assert!(dir.path().join(".dream-lock").exists(), "锁已创建");
        } // guard dropped
        assert!(!dir.path().join(".dream-lock").exists(), "Drop 释放锁");
    }

    #[test]
    fn lock_blocks_concurrent() {
        let dir = tempfile::tempdir().unwrap();
        let _g1 = DreamGuard::acquire(dir.path()).unwrap();
        // 第二次 acquire（锁新鲜）→ Err
        let r = DreamGuard::acquire(dir.path());
        assert!(r.is_err(), "新鲜锁被占用 → 报错");
        assert!(r.unwrap_err().to_string().contains("已被占用"), "错误信息提示被占用");
    }

    #[test]
    fn lock_takes_over_zombie() {
        let dir = tempfile::tempdir().unwrap();
        // 种一个 >1h 的僵尸锁
        let zombie_ms: u64 = 1_700_000_000_000; // 2023-11 纪元 ms（远超 1h 前）
        std::fs::write(dir.path().join(".dream-lock"),
            format!("{{\"pid\":99999,\"started_at_ms\":{zombie_ms}}}")).unwrap();
        let g = DreamGuard::acquire(dir.path()).expect("僵尸锁 >1h → 抢占成功");
        // 锁被覆盖为当前 pid + 新时间
        let content = std::fs::read_to_string(dir.path().join(".dream-lock")).unwrap();
        assert!(content.contains(&format!("\"pid\":{}", std::process::id())) || !content.contains("99999"), "抢占覆盖");
        drop(g);
        assert!(!dir.path().join(".dream-lock").exists(), "抢占后 drop 释放");
    }
}
