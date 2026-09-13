//! v2 记忆「日」层（第 1 期只做日；周/月/年晋级见第 2 期）。
//! dream 是 memory/ 唯一写者（mem 只读，零竞态）。事件段的「对话索引」seq[a,b] 由代码盖戳（P1 铁律③）。
//! 时间一律本地（date_from_ts_local / time_hhmm_from_ts_local 注入 offset），offset 由 dream 传 local_offset_secs()——
//! 必须与 history writer 同源（spawn_writer 也用同一 local_offset_secs），否则 memory 文件日期与 history jsonl
//! 文件名日期不一致，seq 指针 history/YYYY-MM-DD.jsonl#seq[a,b] 会指向错日期文件而断裂。
use crate::history::{date_from_ts_local, time_hhmm_from_ts_local};
use std::path::Path;

fn date_compact(ts: u64, offset_secs: i64) -> String {
    date_from_ts_local(ts, offset_secs).replace('-', "")
}

fn day_file_path(cache: &Path, ts: u64, offset_secs: i64) -> std::path::PathBuf {
    let d = date_from_ts_local(ts, offset_secs); // YYYY-MM-DD（本地）
    let (y, m) = match d.split('-').collect::<Vec<_>>().as_slice() {
        [y, m, _] => (y.to_string(), m.to_string()),
        _ => ("1970".into(), "01".into()),
    };
    cache.join("memory").join(y).join(m).join(format!("{d}.md"))
}

/// 数当天文件里已有多少事件段（## 开头行数）→ 下一个 NN。
fn day_event_count(path: &Path) -> u32 {
    std::fs::read_to_string(path).unwrap_or_default()
        .lines().filter(|l| l.starts_with("## ")).count() as u32
}

/// `day_event_count` 的公开包装：按 (cache, ts, offset) 定位 day 文件后数段数（F10 evt_no 同源用）。
pub fn day_event_count_pub(cache: &Path, ts: u64, offset_secs: i64) -> u32 {
    day_event_count(&day_file_path(cache, ts, offset_secs))
}

/// 追加一条「日」事件段到 memory/{Y}/{M}/{date}.md。seq_range 由代码盖戳（dream 传入观察到的范围）。
/// offset_secs = 本地偏移（dream 传 local_offset_secs()）；标题 HH:MM + 文件日期都用它，与 history writer 一致。
/// 返回该段的 evt_no（nn = 写前 day 段数 + 1）——dream 复用它调 append_memory_day，保证 day 文件
/// 与 MEMORY 日层的 evt-NNN 同源（F10：避免两轮循环计数时机错位）。
pub fn append_day_event(
    cache: &Path, ts: u64, offset_secs: i64, title: &str, detail: &str, subject: &str,
    seq_range: (u64, u64), attachment: Option<&str>,
) -> Result<u32, String> {
    let path = day_file_path(cache, ts, offset_secs);
    if let Some(p) = path.parent() { std::fs::create_dir_all(p).map_err(|e| e.to_string())?; }
    let nn = day_event_count(&path) + 1;
    let hhmm = time_hhmm_from_ts_local(ts, offset_secs);
    let date = date_from_ts_local(ts, offset_secs);
    let date_c = date_compact(ts, offset_secs);
    let mut seg = format!(
        "\n## {hhmm} evt-{date_c}-{nn:03} {title}\n**主语**: {subject}\n**详情**: {detail}\n**对话索引**: history/{date}.jsonl#seq[{a},{b}]\n",
        a = seq_range.0, b = seq_range.1
    );
    if let Some(att) = attachment {
        seg.push_str(&format!("**附件**: {att}\n"));
    }
    // append-only（dream 是当天唯一写者；跨天冻结旧文件）
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| e.to_string())?;
    f.write_all(seg.as_bytes()).map_err(|e| e.to_string())?;
    Ok(nn)
}

/// 「日」索引行（MEMORY.md「日」节）。evt_no 由调用方（dream）按当天序号给。
pub fn append_memory_day(cache: &Path, ts: u64, offset_secs: i64, evt_no: u32, title: &str) -> Result<String, String> {
    let path = cache.join("MEMORY.md");
    if !path.exists() { ensure_memory_skeleton(cache).map_err(|e| e.to_string())?; }
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    let hhmm = time_hhmm_from_ts_local(ts, offset_secs);
    let date_c = date_compact(ts, offset_secs);
    let line = format!("- {hhmm} evt-{date_c}-{evt_no:03} {title}\n");
    text = insert_in_section(&text, "日", &line);
    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    Ok(format!("已追加「日」索引行 → {}", path.display()))
}

/// 读当天 day 文件所有事件段的 title（去 `## HH:MM evt-NNN ` 前缀）。
pub fn day_titles(cache: &Path, ts: u64, offset_secs: i64) -> Vec<String> {
    let path = day_file_path(cache, ts, offset_secs);
    let body = std::fs::read_to_string(&path).unwrap_or_default();
    body.lines().filter_map(|l| {
        let rest = l.strip_prefix("## ")?;
        // "HH:MM evt-NNN title..." → splitn 3 取第三段（title，可含空格）
        let mut it = rest.splitn(3, ' ');
        it.next();
        it.next();
        it.next().map(|t| t.to_string())
    }).collect()
}

/// 重算当月「今日行」（汇总当天所有事件段 title），在 MEMORY.md 月节「### 当月」子节里 upsert。
pub fn upsert_current_month_today(cache: &Path, ts: u64, offset_secs: i64) -> Result<(), String> {
    let path = cache.join("MEMORY.md");
    if !path.exists() { ensure_memory_skeleton(cache).map_err(|e| e.to_string())?; }
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let today = date_from_ts_local(ts, offset_secs); // YYYY-MM-DD
    let titles = day_titles(cache, ts, offset_secs);
    let summary = if titles.is_empty() { String::from("（无事件）") } else { titles.join("；") };
    let new_line = format!("- {today}: {summary}");
    let new_text = upsert_line_in_subsection(&text, "月", "当月", &today, &new_line);
    std::fs::write(&path, new_text).map_err(|e| e.to_string())
}

/// 在 `## {section}` 节的 `### {subsection}` 子节里，按 key（行前缀 `- {key}`）upsert 一行。
fn upsert_line_in_subsection(text: &str, section: &str, subsection: &str, key: &str, new_line: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    let sec_header = format!("## {section}");
    let sec_start = match lines.iter().position(|l| l.trim_start() == sec_header) {
        Some(i) => i + 1,
        None => return lines.join("\n"),
    };
    let mut sec_end = sec_start;
    while sec_end < lines.len() && !lines[sec_end].trim_start().starts_with("## ") { sec_end += 1; }
    let sub_prefix = format!("### {subsection}");
    let sub_start = match (sec_start..sec_end).find(|&i| lines[i].trim_start().starts_with(&sub_prefix)) {
        Some(i) => i + 1,
        None => return lines.join("\n"),
    };
    let mut sub_end = sub_start;
    while sub_end < sec_end && !lines[sub_end].trim_start().starts_with("### ") { sub_end += 1; }
    let key_prefix = format!("- {key}");
    if let Some(i) = (sub_start..sub_end).find(|&i| lines[i].trim_start().starts_with(&key_prefix)) {
        lines[i] = new_line.to_string(); // 替换
    } else {
        let mut insert_at = sub_end;
        while insert_at > sub_start && lines[insert_at - 1].trim().is_empty() { insert_at -= 1; }
        lines.insert(insert_at, new_line.to_string()); // 追加
    }
    lines.join("\n")
}

/// 在 MEMORY.md 的某个「## {section}」节（到下一个 ## 之前）末尾插入 line。
fn insert_in_section(text: &str, section: &str, line: &str) -> String {
    let header = format!("## {section}");
    let mut lines = text.lines().collect::<Vec<_>>();
    let start = match lines.iter().position(|l| l.trim_start() == header) {
        Some(i) => i + 1,
        None => { lines.push(""); lines.push(header.as_str()); lines.push(line); return lines.join("\n"); }
    };
    // 找下一个 ## 的位置（节末尾）
    let mut end = start;
    while end < lines.len() && !lines[end].trim_start().starts_with("## ") { end += 1; }
    // 跳过节末尾空行，把新行插在内容尾部
    let mut insert_at = end;
    while insert_at > start && lines[insert_at - 1].trim().is_empty() { insert_at -= 1; }
    lines.insert(insert_at, line.trim_end_matches('\n'));
    lines.join("\n")
}

/// 建 MEMORY.md 三层骨架（日/月/年，月节带 ### 当月 / ### 上月 子标题）；已存在则不动。
/// 若是旧四层（含 ## 周）或缺月节子标题 → 幂等迁移到三层。
pub fn ensure_memory_skeleton(cache: &Path) -> std::io::Result<()> {
    let path = cache.join("MEMORY.md");
    if !path.exists() {
        let skeleton = r#"# 记忆索引（MEMORY.md）

由 dream 自动维护。三层：日（今天 per-event）/月（当月逐日 + 上月冻结）/年（每月一段月主题）。

## 日

## 月
### 当月

### 上月

## 年
"#;
        return std::fs::write(&path, skeleton);
    }
    // 迁移：旧四层（含 ## 周）或月节缺子标题 → 升级为三层
    let text = std::fs::read_to_string(&path)?;
    if text.contains("\n## 周") || !text.contains("### 当月") {
        let migrated = migrate_to_three_tiers(&text);
        std::fs::write(&path, migrated)?;
    }
    Ok(())
}

fn migrate_to_three_tiers(text: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    // 删 ## 周 节（从 ## 周 到下一个 ## 之前）
    if let Some(start) = lines.iter().position(|l| l.trim_start() == "## 周") {
        let mut end = start + 1;
        while end < lines.len() && !lines[end].trim_start().starts_with("## ") { end += 1; }
        lines.drain(start..end);
    }
    // 确保月节有 ### 当月 / ### 上月 子标题
    if let Some(mstart) = lines.iter().position(|l| l.trim_start() == "## 月") {
        let mut mend = mstart + 1;
        while mend < lines.len() && !lines[mend].trim_start().starts_with("## ") { mend += 1; }
        let has_current = (mstart + 1..mend).any(|i| lines[i].trim_start().starts_with("### 当月"));
        if !has_current {
            lines.insert(mstart + 1, "### 上月".to_string());
            lines.insert(mstart + 1, "### 当月".to_string());
        }
    }
    lines.join("\n")
}

/// 读 memory/.dream-meta.json 的 current_month（YYYY-MM）。不存在/解析失败 → None。
pub fn read_dream_meta(cache: &Path) -> Option<String> {
    let text = std::fs::read_to_string(cache.join("memory").join(".dream-meta.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("current_month").and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// 写 memory/.dream-meta.json 的 current_month（YYYY-MM）。目录不存在则创建。
pub fn write_dream_meta(cache: &Path, ym: &str) -> Result<(), String> {
    let dir = cache.join("memory");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let body = format!(r#"{{"current_month":"{}"}}"#, ym);
    std::fs::write(dir.join(".dream-meta.json"), body).map_err(|e| e.to_string())
}

/// 由时间戳得 YYYY-MM（用 date_from_ts_local 取前 7 字符）。无 chrono。
pub fn ym_from_ts(ts: u64, offset_secs: i64) -> String {
    date_from_ts_local(ts, offset_secs)[..7].to_string()
}

/// 跨月轮转：当月 {completed_ym} → 上月（冻结），旧上月退出，新建空当月 {new_ym}。
pub fn rotate_month(cache: &Path, completed_ym: &str, new_ym: &str) -> Result<(), String> {
    let path = cache.join("MEMORY.md");
    if !path.exists() { ensure_memory_skeleton(cache).map_err(|e| e.to_string())?; }
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let new_text = rotate_month_text(&text, completed_ym, new_ym);
    std::fs::write(&path, new_text).map_err(|e| e.to_string())
}

fn rotate_month_text(text: &str, completed_ym: &str, new_ym: &str) -> String {
    let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    let sec_start = match lines.iter().position(|l| l.trim_start() == "## 月") {
        Some(i) => i,
        None => return lines.join("\n"),
    };
    let mut sec_end = sec_start + 1;
    while sec_end < lines.len() && !lines[sec_end].trim_start().starts_with("## ") { sec_end += 1; }
    // 取当月内容（连带其逐日行），删整个旧月节（含旧上月），重建为「上月冻结 + 空当月」
    let cur_body = extract_subsection_body(&lines, sec_start + 1, sec_end, "当月");
    let mut new_section: Vec<String> = vec![String::from("## 月")];
    new_section.push(format!("### 上月 {completed_ym}（冻结）"));
    new_section.extend(cur_body.into_iter().filter(|l| !l.is_empty()));
    new_section.push(String::new());
    new_section.push(format!("### 当月 {new_ym}"));
    new_section.push(String::new()); // 空行分隔下一节（## 年），避免当月与年紧贴
    let mut out: Vec<String> = lines[..sec_start].to_vec();
    out.extend(new_section);
    out.extend(lines[sec_end..].to_vec());
    out.join("\n")
}

fn extract_subsection_body(lines: &[String], from: usize, to: usize, subsection: &str) -> Vec<String> {
    let prefix = format!("### {subsection}");
    let start = match (from..to).find(|&i| lines[i].trim_start().starts_with(&prefix)) {
        Some(i) => i + 1,
        None => return Vec::new(),
    };
    let mut end = start;
    while end < to && !lines[end].trim_start().starts_with("### ") { end += 1; }
    lines[start..end].to_vec()
}

/// 年节追加一条月主题行（`- {ym}: {theme}`）。
pub fn append_year_theme(cache: &Path, ym: &str, theme: &str) -> Result<(), String> {
    let path = cache.join("MEMORY.md");
    if !path.exists() { ensure_memory_skeleton(cache).map_err(|e| e.to_string())?; }
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    let line = format!("- {ym}: {theme}");
    text = insert_in_section(&text, "年", &line);
    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    Ok(())
}

/// 收集某月（YYYY-MM）所有 day 文件的全部事件段文本，按文件名（日期）排序，作月主题 LLM 原料。
pub fn collect_month_segments(cache: &Path, ym: &str) -> String {
    let (y, m) = match ym.split_once('-') { Some(x) => x, None => return String::new() };
    let dir = cache.join("memory").join(y).join(m);
    let mut files: Vec<(String, String)> = std::fs::read_dir(&dir).map(|rd| {
        rd.filter_map(|e| e.ok()).filter_map(|e| {
            let p = e.path();
            let name = p.file_name()?.to_string_lossy().into_owned();
            if !name.ends_with(".md") { return None; }
            let body = std::fs::read_to_string(&p).ok()?;
            Some((name, body))
        }).collect()
    }).unwrap_or_default();
    files.sort();
    let mut out = String::new();
    for (name, body) in &files {
        out.push_str(&format!("## {name}\n"));
        out.push_str(body);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> tempfile::TempDir { tempfile::tempdir().unwrap() }
    fn ts_on(_d: &str) -> u64 {
        // 2026-07-26 当天 12:30 UTC（date_from_ts==2026-07-26）。offset=0 时 HH:MM=12:30。
        1_785_024_000_000 + 12 * 3600_000 + 30 * 60_000
    }

    #[test]
    fn append_day_event_writes_segment_with_code_stamped_seq() {
        let w = ws();
        let r = append_day_event(w.path(), ts_on("2026-07-26"), 0, "重构 foo.rs 完成",
            "拆成 3 模块，改 12 处", "agent（子代理#7）", (3, 9), Some("a1b2.png"));
        assert!(r.is_ok(), "{:?}", r);
        let path = w.path().join("memory/2026/07/2026-07-26.md");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("重构 foo.rs 完成"), "标题: {body}");
        assert!(body.contains("拆成 3 模块"), "详情: {body}");
        assert!(body.contains("agent（子代理#7）"), "主语: {body}");
        // P1 铁律③：seq 指针代码盖戳，必须精确
        assert!(body.contains("对话索引**: history/2026-07-26.jsonl#seq[3,9]"), "seq 指针盖戳: {body}");
        assert!(body.contains("a1b2.png"));
    }

    #[test]
    fn append_day_event_correct_path_and_time() {
        let w = ws();
        append_day_event(w.path(), ts_on("2026-07-26"), 0, "T", "D", "S", (0,1), None).unwrap();
        let path = w.path().join("memory/2026/07/2026-07-26.md");
        assert!(path.exists(), "应落 memory/{{Y}}/{{M}}/{{date}}.md");
        let body = std::fs::read_to_string(path).unwrap();
        assert!(body.contains("12:30"), "offset=0 等价 UTC，HH:MM=12:30: {body}");
        assert!(body.contains("evt-20260726-001"), "事件编号: {body}");
    }

    #[test]
    fn append_day_event_local_offset_crosses_day() {
        // UTC 2026-07-26 20:00 + 8h 偏移 = 本地 2026-07-27 04:00 → 文件落次日，标题 04:00，seq 指针指次日 jsonl
        let w = ws();
        let ts = 1_785_024_000_000 + 20 * 3600_000; // UTC 20:00
        append_day_event(w.path(), ts, 28800, "夜聊", "d", "s", (0,1), None).unwrap();
        let same_day = w.path().join("memory/2026/07/2026-07-26.md");
        let next_day = w.path().join("memory/2026/07/2026-07-27.md");
        assert!(!same_day.exists(), "+8h 应跨天，不当日落当天");
        assert!(next_day.exists(), "+8h 应跨天落次日文件: {}", next_day.display());
        let body = std::fs::read_to_string(next_day).unwrap();
        assert!(body.contains("04:00"), "本地 04:00: {body}");
        assert!(body.contains("evt-20260727-001"), "次日事件编号: {body}");
        assert!(body.contains("history/2026-07-27.jsonl#seq[0,1]"), "seq 指针指次日 jsonl（与文件日期一致）: {body}");
    }

    #[test]
    fn append_day_event_increments_nn() {
        let w = ws();
        append_day_event(w.path(), ts_on("2026-07-26"), 0, "第一件", "d", "s", (0,1), None).unwrap();
        append_day_event(w.path(), ts_on("2026-07-26"), 0, "第二件", "d", "s", (2,3), None).unwrap();
        let body = std::fs::read_to_string(w.path().join("memory/2026/07/2026-07-26.md")).unwrap();
        assert!(body.contains("evt-20260726-001"));
        assert!(body.contains("evt-20260726-002"));
    }

    #[test]
    fn ensure_memory_skeleton_creates_three_tiers_no_week() {
        let w = ws();
        ensure_memory_skeleton(w.path()).unwrap();
        let body = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        assert!(body.contains("## 日"), "含日节: {body}");
        assert!(body.contains("## 月"), "含月节: {body}");
        assert!(body.contains("## 年"), "含年节: {body}");
        assert!(!body.contains("## 周"), "去周: {body}");
        assert!(body.contains("### 当月"), "月节带当月子标题: {body}");
        assert!(body.contains("### 上月"), "月节带上月子标题: {body}");
    }

    #[test]
    fn ensure_memory_skeleton_migrates_old_four_tier() {
        // 旧四层 MEMORY.md（含 ## 周，月节无子标题）→ 迁移：删周 + 补当月/上月子标题
        let w = ws();
        let old = "# 记忆索引\n\n## 日\n- 旧日行\n\n## 周\n- 旧周行\n\n## 月\n- 旧月行\n\n## 年\n";
        std::fs::write(w.path().join("MEMORY.md"), old).unwrap();
        ensure_memory_skeleton(w.path()).unwrap();
        let body = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        assert!(!body.contains("## 周"), "迁移后无周节: {body}");
        assert!(!body.contains("旧周行"), "周节内容被删: {body}");
        assert!(body.contains("### 当月"), "补上当月子标题: {body}");
        assert!(body.contains("### 上月"), "补上上月子标题: {body}");
        assert!(body.contains("旧月行"), "月节原内容保留: {body}");
    }

    #[test]
    fn ensure_memory_skeleton_idempotent_three_tier() {
        let w = ws();
        ensure_memory_skeleton(w.path()).unwrap();
        let once = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        ensure_memory_skeleton(w.path()).unwrap(); // 再调不动
        let twice = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        assert_eq!(once, twice, "已是三层应幂等不动");
    }

    #[test]
    fn read_dream_meta_none_when_absent() {
        let w = ws();
        assert_eq!(read_dream_meta(w.path()), None);
    }

    #[test]
    fn write_then_read_dream_meta_roundtrip() {
        let w = ws();
        write_dream_meta(w.path(), "2026-07").unwrap();
        assert_eq!(read_dream_meta(w.path()), Some("2026-07".to_string()));
        // 文件落在 memory/.dream-meta.json
        assert!(w.path().join("memory").join(".dream-meta.json").exists());
    }

    #[test]
    fn ym_from_ts_returns_yyyy_mm() {
        // ts_on(2026-07-26) + offset=0 → date 2026-07-26 → ym 2026-07
        assert_eq!(ym_from_ts(ts_on("2026-07-26"), 0), "2026-07");
    }

    #[test]
    fn append_memory_day_adds_line_under_day_section() {
        let w = ws();
        ensure_memory_skeleton(w.path()).unwrap();
        append_memory_day(w.path(), ts_on("2026-07-26"), 0, 1, "重构 foo.rs 完成").unwrap();
        let body = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        // 行落在「## 日」与「## 月」之间（v2 三层骨架去周，月节紧邻日节）
        let day_start = body.find("## 日").unwrap();
        let month_start = body.find("## 月").unwrap();
        let day_section = &body[day_start..month_start];
        assert!(day_section.contains("12:30"), "「日」节应含时间: {day_section}");
        assert!(day_section.contains("重构 foo.rs 完成"));
        assert!(day_section.contains("evt-20260726-001") || day_section.contains("evt-001") || day_section.contains("evt-1"),
            "「日」索引行: {day_section}");
    }

    fn write_day_with_segments(ws_dir: &std::path::Path, date: &str, titles: &[&str]) {
        // date="2026-07-26" → memory/2026/07/2026-07-26.md，每标题写一段
        let (y, m) = (date.split('-').next().unwrap(), date.split('-').nth(1).unwrap());
        let p = ws_dir.join("memory").join(y).join(m).join(format!("{date}.md"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut body = String::new();
        for (i, t) in titles.iter().enumerate() {
            body.push_str(&format!("## 12:0{i} evt-{}-{i:03} {t}\n**详情**: d\n**主语**: 用户\n", date.replace('-', "")));
        }
        std::fs::write(&p, body).unwrap();
    }

    #[test]
    fn day_titles_reads_all_segment_titles() {
        let w = ws();
        write_day_with_segments(w.path(), "2026-07-26", &["第一件", "第二件"]);
        let titles = day_titles(w.path(), ts_on("2026-07-26"), 0);
        assert_eq!(titles, vec!["第一件".to_string(), "第二件".to_string()]);
    }

    #[test]
    fn upsert_creates_today_line_when_absent() {
        let w = ws();
        ensure_memory_skeleton(w.path()).unwrap();
        write_day_with_segments(w.path(), "2026-07-26", &["重构完成", "聊了三件事"]);
        upsert_current_month_today(w.path(), ts_on("2026-07-26"), 0).unwrap();
        let body = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        let cur_start = body.find("### 当月").unwrap();
        let cur_section = &body[cur_start..];
        assert!(cur_section.contains("- 2026-07-26: 重构完成；聊了三件事"),
            "当月今日行应汇总当天 title: {cur_section}");
    }

    #[test]
    fn upsert_replaces_today_line_on_second_dream_same_day() {
        // 同一天 dream 两次（第二次多了事件段）→ 今日行更新，不重复
        let w = ws();
        ensure_memory_skeleton(w.path()).unwrap();
        write_day_with_segments(w.path(), "2026-07-26", &["A"]);
        upsert_current_month_today(w.path(), ts_on("2026-07-26"), 0).unwrap();
        // 第二次 dream 又写了一段
        write_day_with_segments(w.path(), "2026-07-26", &["A", "B"]);
        upsert_current_month_today(w.path(), ts_on("2026-07-26"), 0).unwrap();
        let body = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        let count = body.matches("- 2026-07-26:").count();
        assert_eq!(count, 1, "今日行只一行（upsert 替换不重复）: {body}");
        assert!(body.contains("- 2026-07-26: A；B"), "今日行含最新汇总: {body}");
    }

    #[test]
    fn upsert_does_not_touch_other_sections() {
        let w = ws();
        ensure_memory_skeleton(w.path()).unwrap();
        // 先在日节 / 年节放内容
        let mut text = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        text = text.replace("## 日\n\n## 月", "## 日\n- 不该被动\n\n## 月");
        text = text.replace("## 年\n", "## 年\n- 2026-01: 旧年主题\n");
        std::fs::write(w.path().join("MEMORY.md"), text).unwrap();
        write_day_with_segments(w.path(), "2026-07-26", &["X"]);
        upsert_current_month_today(w.path(), ts_on("2026-07-26"), 0).unwrap();
        let body = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        assert!(body.contains("- 不该被动"), "日节不动: {body}");
        assert!(body.contains("- 2026-01: 旧年主题"), "年节不动: {body}");
        assert!(body.contains("- 2026-07-26: X"), "当月今日行写入: {body}");
    }

    fn three_tier_with_current(ws_dir: &std::path::Path, current_ym: &str, current_lines: &[&str]) -> String {
        let mut body = format!("# 记忆索引\n\n## 日\n\n## 月\n### 当月 {current_ym}\n");
        for l in current_lines { body.push_str(&format!("{l}\n")); }
        body.push_str("\n### 上月\n\n## 年\n");
        std::fs::write(ws_dir.join("MEMORY.md"), &body).unwrap();
        body
    }

    #[test]
    fn rotate_moves_current_to_previous_and_creates_empty_current() {
        let w = ws();
        three_tier_with_current(w.path(), "2026-07", &["- 2026-07-01: 七月A", "- 2026-07-15: 七月B"]);
        rotate_month(w.path(), "2026-07", "2026-08").unwrap();
        let body = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        assert!(body.contains("### 上月 2026-07（冻结）"), "旧当月→上月冻结: {body}");
        assert!(body.contains("七月A") && body.contains("七月B"), "旧当月内容带到上月: {body}");
        assert!(body.contains("### 当月 2026-08"), "新建空当月: {body}");
    }

    #[test]
    fn rotate_drops_old_previous() {
        // 已有旧上月（2026-05）+ 当月（2026-06）→ 轮转到 2026-07 时，旧上月 2026-05 应退出
        let w = ws();
        let body = "# 记忆索引\n\n## 日\n\n## 月\n### 当月 2026-06\n- 2026-06-01: 六月\n\n### 上月 2026-05（冻结）\n- 2026-05-01: 五月旧\n\n## 年\n";
        std::fs::write(w.path().join("MEMORY.md"), body).unwrap();
        rotate_month(w.path(), "2026-06", "2026-07").unwrap();
        let after = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        assert!(!after.contains("五月旧"), "旧上月内容退出 2 月窗口: {after}");
        assert!(!after.contains("2026-05"), "旧上月标识退出: {after}");
        assert!(after.contains("六月"), "刚完成月（六月）→ 上月: {after}");
    }

    #[test]
    fn rotate_leaves_blank_line_before_next_section() {
        // cosmetic：rotate 后新当月与下一节（## 年）间应有空行分隔，不紧贴
        let w = ws();
        three_tier_with_current(w.path(), "2026-07", &["- 2026-07-01: 七月A"]);
        rotate_month(w.path(), "2026-07", "2026-08").unwrap();
        let body = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        let cur = body.find("### 当月 2026-08").unwrap();
        let year = body.find("## 年").unwrap();
        let between = &body[cur..year];
        assert!(between.contains("\n\n"), "新当月与 ## 年 间应有空行分隔: {between}");
    }

    #[test]
    fn append_year_theme_adds_line_under_year() {
        let w = ws();
        ensure_memory_skeleton(w.path()).unwrap();
        append_year_theme(w.path(), "2026-06", "六月主要做了X和Y").unwrap();
        let body = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        let year_start = body.find("## 年").unwrap();
        let year_section = &body[year_start..];
        assert!(year_section.contains("- 2026-06: 六月主要做了X和Y"), "年节追加月主题: {year_section}");
    }

    #[test]
    fn append_year_theme_multiple_months_accumulate() {
        let w = ws();
        ensure_memory_skeleton(w.path()).unwrap();
        append_year_theme(w.path(), "2026-05", "五月主题").unwrap();
        append_year_theme(w.path(), "2026-06", "六月主题").unwrap();
        let body = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        assert!(body.contains("五月主题") && body.contains("六月主题"), "多月主题累加: {body}");
    }

    #[test]
    fn collect_month_segments_reads_all_days_sorted() {
        let w = ws();
        // 2026-06 月两天，各含事件段
        for (d, title) in [("2026-06-15", "六月A"), ("2026-06-01", "六月B")] {
            let p = w.path().join("memory/2026/06").join(format!("{d}.md"));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, format!("## 10:00 evt-{}-001 {title}\n**详情**: d\n**主语**: 用户\n", d.replace('-', ""))).unwrap();
        }
        let segs = collect_month_segments(w.path(), "2026-06");
        assert!(segs.contains("六月A") && segs.contains("六月B"), "含两天事件段: {segs}");
        // 按文件名排序：2026-06-01 在 2026-06-15 前
        assert!(segs.find("六月B").unwrap() < segs.find("六月A").unwrap(), "按日期排序: {segs}");
    }

    #[test]
    fn collect_month_segments_empty_when_no_dir() {
        let w = ws();
        assert_eq!(collect_month_segments(w.path(), "2099-01"), "");
    }

    // ─── Task 3: day_event_count_pub 测试（F10 evt_no 同源）───

    fn write_day(cache: &std::path::Path, date: &str, body: &str) {
        let (y, m) = {
            let v: Vec<&str> = date.split('-').collect();
            (v[0], v[1])
        };
        let p = cache.join("memory").join(y).join(m).join(format!("{date}.md"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
    }

    #[test]
    fn day_event_count_pub_counts_segments() {
        let dir = tempfile::tempdir().unwrap();
        // offset=0 → ts=0 对应 1970-01-01
        write_day(dir.path(), "1970-01-01",
            "## 00:00 evt-19700101-001 A\n**主语**: 用户\n**详情**: d\n**对话索引**: x\n## 00:01 evt-19700101-002 B\n**主语**: 用户\n**详情**: d\n**对话索引**: x\n");
        let n = day_event_count_pub(dir.path(), 0, 0);
        assert_eq!(n, 2, "应数到 2 个 ## 段");
    }

    #[test]
    fn day_event_count_pub_zero_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let n = day_event_count_pub(dir.path(), 0, 0);
        assert_eq!(n, 0, "文件不存在 → 0");
    }
}
