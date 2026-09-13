//! mem CLI 核心逻辑（零 LLM 只读 drill）。文件树即索引：路径里 Y/M/D 是时间索引，
//! {date}.md 首行 ## 是标题，事件段「对话索引」是指针。三样都机械可读，无需 dream 写索引文件（§10.6）。
//!
//! 接口（对齐 dream-user-expectations §五）：
//! - ls <年/月/日>  列索引（紧凑 YYYY/YYYYMM/YYYYMMDD，也认横线）：年→各月+月主题；
//!                 月→各日+日概括；日→各事件+evt/seq。
//! - read <年/月/日> 读 dream 整理后记忆内容（年→月主题索引；月→月主题+日概括；日→事件段全文）。
//! - history/search 读原始对话 / 关键字搜。
use crate::history::HistoryEvent;
use std::path::Path;

fn split_date(date: &str) -> Option<(&str, &str)> {
    let mut it = date.split('-');
    Some((it.next()?, it.next()?))
}

/// mem ls [年/月/日]：列索引（紧凑 YYYY / YYYYMM / YYYYMMDD，也认横线 YYYY-MM-DD / YYYY-MM）。
/// - 无参 → 列年；2026 → 各月+月主题；202607 → 各日+日概括；20260731 → 各事件+evt/seq 索引。
pub fn ls(workspace: &Path, arg: Option<&str>) -> String {
    let mem = workspace.join("memory");
    let Some(arg) = arg else { return list_dirs(&mem); };
    let digits: String = arg.chars().filter(|c| c.is_ascii_digit()).collect();
    match digits.len() {
        4 => ls_year(workspace, &digits),
        6 => ls_month(workspace, &digits[..4], &digits[4..]),
        8 => ls_day(workspace, &digits[..4], &digits[4..6], &digits[6..]),
        _ => format!("日期格式应为 YYYY / YYYYMM / YYYYMMDD（也认横线），收到: {arg}"),
    }
}

/// 列数字名子目录（年/月目录用）。
fn list_dirs(dir: &Path) -> String {
    let mut names: Vec<String> = std::fs::read_dir(dir).map(|rd| {
        rd.filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_string_lossy().into_owned().into())
            .filter(|n| n.chars().all(|c| c.is_ascii_digit()))
            .collect()
    }).unwrap_or_default();
    names.sort();
    if names.is_empty() { format!("（{dir} 下无数据）", dir = dir.display()) } else { names.join("\n") }
}

/// ls YYYY：各月 + 月主题（有月主题用月主题，否则段数+首条标题）。
fn ls_year(workspace: &Path, year: &str) -> String {
    let dir = workspace.join("memory").join(year);
    let mut months: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| rd.filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_string_lossy().into_owned().into())
            .collect()).unwrap_or_default();
    months.sort();
    if months.is_empty() { return format!("（{year} 无记忆）"); }
    // 读年层 jsonl 的各月 month_title
    let mut month_titles: std::collections::HashMap<String, String> = Default::default();
    if let Ok(s) = std::fs::read_to_string(dir.join(format!("{year}.jsonl"))) {
        for line in s.lines() {
            let l = line.trim();
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
                if v.get("type").and_then(|x| x.as_str()) == Some("month") {
                    let m = v.get("month").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    let t = v.get("month_title").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    month_titles.insert(m, t);
                }
            }
        }
    }
    let mut out = format!("## {year}（各月）\n");
    for m in &months {
        let ym = format!("{year}-{m}");
        let count = count_events_in_month(&dir.join(m));
        let title = month_titles.get(&ym).cloned().unwrap_or_else(|| format!("{}事件", count));
        out.push_str(&format!("- {}: {}（{}事件）\n", ym, title, count));
    }
    out
}

/// 数某月目录下所有 day jsonl 的事件总行数。
fn count_events_in_month(dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                // 只数 day 文件(YYYY-MM-DD,stem 长 10),跳过月层(YYYY-MM,stem 长 7)
                let stem_len = p.file_stem().and_then(|s| s.to_str()).map(|s| s.len()).unwrap_or(0);
                if stem_len != 10 { continue; }
                if let Ok(s) = std::fs::read_to_string(&p) {
                    total += s.lines().filter(|l| l.trim().starts_with('{')).count() as u64;
                }
            }
        }
    }
    total
}

/// ls YYYYMM：各日 + 日概括（读月级 YYYY-MM.md ## 日概括 段，每日一行）。
fn ls_month(workspace: &Path, y: &str, m: &str) -> String {
    let ym = format!("{y}-{m}");
    let path = workspace.join("memory").join(y).join(m).join(format!("{ym}.jsonl"));
    let s = std::fs::read_to_string(&path).unwrap_or_default();
    if s.trim().is_empty() { return format!("（{ym} 无月级文件）"); }
    let mut out = format!("## {ym}（各天）\n");
    let mut n = 0;
    for line in s.lines() {
        let l = line.trim();
        if l.is_empty() || !l.starts_with('{') { continue; }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
            if v.get("type").and_then(|x| x.as_str()) == Some("day") {
                let date = v.get("date").and_then(|x| x.as_str()).unwrap_or("");
                let ec = v.get("event_count").and_then(|x| x.as_u64()).unwrap_or(0);
                let title = v.get("day_title").and_then(|x| x.as_str()).unwrap_or("");
                out.push_str(&format!("- {} [{}事件] {}\n", date, ec, title));
                n += 1;
            }
        }
    }
    if n == 0 { format!("（{ym} 无天卡）") } else { out }
}

/// ls YYYYMMDD：各事件段 + evt/seq 索引（读 day 文件事件段）。
fn ls_day(workspace: &Path, y: &str, m: &str, d: &str) -> String {
    let date = format!("{y}-{m}-{d}");
    let path = workspace.join("memory").join(y).join(m).join(format!("{date}.jsonl"));
    let s = std::fs::read_to_string(&path).unwrap_or_default();
    if s.trim().is_empty() { return format!("（{date} 无当天记忆）"); }
    let mut out = format!("## {date}（事件索引）\n");
    let mut n = 0;
    for line in s.lines() {
        let l = line.trim();
        if l.is_empty() || !l.starts_with('{') { continue; }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
            let get = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("");
            let seq = v.get("seq").and_then(|x| x.as_array()).map(|a| {
                let lo = a.get(0).and_then(|x| x.as_u64()).unwrap_or(0);
                let hi = a.get(1).and_then(|x| x.as_u64()).unwrap_or(0);
                format!("{}..{}", lo, hi)
            }).unwrap_or_default();
            out.push_str(&format!("- {} [{}] {} — seq[{}]\n", get("hhmm"), get("type"), get("title"), seq));
            n += 1;
        }
    }
    if n == 0 { format!("（{date} 无事件）") } else { out }
}

/// mem read <date>：打印当天事件段（含「对话索引」指针）。
pub fn read_day(workspace: &Path, date: &str) -> String {
    let (y, m) = match split_date(date) { Some(x) => x, None => return "日期格式应为 YYYY-MM-DD".into() };
    let path = workspace.join("memory").join(y).join(m).join(format!("{date}.jsonl"));
    let s = std::fs::read_to_string(&path).unwrap_or_default();
    if s.trim().is_empty() { return format!("（{date} 无当天记忆）"); }
    let mut out = String::new();
    for line in s.lines() {
        let l = line.trim();
        if l.is_empty() || !l.starts_with('{') { continue; }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
            let get = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("");
            let kw = v.get("keywords").and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|n| n.as_str()).collect::<Vec<_>>().join("·"))
                .unwrap_or_default();
            out.push_str(&format!("## {} {} [{}] {}\n详情: {}\n关键词: {}\n主语: {}\n对话索引: {}\n\n",
                get("hhmm"), get("evt"), get("type"), get("title"), get("detail"), kw, get("subject"), get("ref")));
        }
    }
    if out.is_empty() { format!("（{date} 无当天记忆）") } else { out }
}

/// mem read <YYYY-MM>：打印月级文件（月主题 + 日概括）。
pub fn read_month(workspace: &Path, ym: &str) -> String {
    let (y, m) = match split_date(ym) { Some(x) => x, None => return "月份格式应为 YYYY-MM".into() };
    let path = workspace.join("memory").join(y).join(m).join(format!("{ym}.jsonl"));
    let s = std::fs::read_to_string(&path).unwrap_or_default();
    if s.trim().is_empty() { return format!("（{ym} 无月级文件）"); }
    let mut out = String::new();
    for line in s.lines() {
        let l = line.trim();
        if l.is_empty() || !l.starts_with('{') { continue; }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
            let get = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("");
            let kw = |v: &serde_json::Value| v.get("keywords").and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|n| n.as_str()).collect::<Vec<_>>().join("·")).unwrap_or_default();
            match get("type") {
                "day" => {
                    let ec = v.get("event_count").and_then(|x| x.as_u64()).unwrap_or(0);
                    out.push_str(&format!("## {} [{}事件] {}\n关键词: {}\n→ {}\n\n", get("date"), ec, get("day_title"), kw(&v), get("drill")));
                }
                "mainline" => {
                    let outs = v.get("key_outputs").and_then(|x| x.as_array())
                        .map(|a| a.iter().filter_map(|n| n.as_str()).collect::<Vec<_>>().join(" / ")).unwrap_or_default();
                    out.push_str(&format!("## 月度主线\n{}\n关键产出: {}\n关键词: {}\n", get("mainline"), outs, kw(&v)));
                }
                _ => {}
            }
        }
    }
    if out.is_empty() { format!("（{ym} 无月级文件）") } else { out }
}

/// mem read <YYYY>：打印年级文件（月主题索引）。
pub fn read_year(workspace: &Path, year: &str) -> String {
    if year.len() != 4 || !year.bytes().all(|b| b.is_ascii_digit()) {
        return "年份格式应为 YYYY".into();
    }
    let path = workspace.join("memory").join(year).join(format!("{year}.jsonl"));
    let s = std::fs::read_to_string(&path).unwrap_or_default();
    if s.trim().is_empty() { return format!("（{year} 无年级文件）"); }
    let mut out = String::new();
    for line in s.lines() {
        let l = line.trim();
        if l.is_empty() || !l.starts_with('{') { continue; }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
            let get = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("");
            match get("type") {
                "month" => out.push_str(&format!("## {} {}\n→ {}\n", get("month"), get("month_title"), get("drill"))),
                "year_mainline" => {
                    let kw = v.get("keywords").and_then(|x| x.as_array())
                        .map(|a| a.iter().filter_map(|n| n.as_str()).collect::<Vec<_>>().join("·")).unwrap_or_default();
                    out.push_str(&format!("## 年度主线\n{}\n关键词: {}\n", get("year_mainline"), kw));
                }
                _ => {}
            }
        }
    }
    if out.is_empty() { format!("（{year} 无年级文件）") } else { out }
}

/// mem history <date> [--seq a..b]：读原始对话，人性化渲染，可按 seq 过滤。
/// mem pack:从三层 jsonl 真相源重拼 MEMORY.md(今日事件 + 当月/上月月层 + 年层)。
/// 无 LLM,纯渲染:读 jsonl → 拼 markdown → 原子写 MEMORY.md。
pub fn pack(workspace: &Path) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let off = crate::history::local_offset_secs();
    let today = crate::history::date_from_ts_local(now, off); // YYYY-MM-DD
    let parts: Vec<&str> = today.split('-').collect();
    let year: i32 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(1970);
    let month: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
    let (py, pm) = if month == 1 { (year - 1, 12u32) } else { (year, month - 1) };
    let ys = year.to_string();
    let ms = format!("{:02}", month);
    let pys = py.to_string();
    let pms = format!("{:02}", pm);

    let mem = workspace.join("memory");
    let day_jsonl = read_jsonl_vec(&mem.join(&ys).join(&ms).join(format!("{}.jsonl", today)));
    let cur_month = read_jsonl_vec(&mem.join(&ys).join(&ms).join(format!("{}-{}.jsonl", ys, ms)));
    let prev_month = read_jsonl_vec(&mem.join(&pys).join(&pms).join(format!("{}-{}.jsonl", pys, pms)));
    let year_jsonl = read_jsonl_vec(&mem.join(&ys).join(format!("{}.jsonl", ys)));

    let mut out = String::from("# 记忆（MEMORY.md）\n由 dream 维护，pack 从三层 jsonl 重拼。三层总览：日 / 月 / 年。\n\n");

    // ## 日:今日事件(从日层 jsonl,按 HH:MM 倒序)
    out.push_str("## 日\n");
    let mut day_lines: Vec<String> = Vec::new();
    for v in &day_jsonl {
        if let Some(l) = day_event_line(v) { day_lines.push(l); }
    }
    day_lines.sort_by(|a, b| b.cmp(a));
    if day_lines.is_empty() {
        out.push_str(&format!("（{} 无当天记忆，先 mem dream day）\n", today));
    } else {
        for l in &day_lines { out.push_str(l); out.push('\n'); }
    }
    out.push('\n');

    // ## 月:当月 + 上月(各从月层 jsonl)
    out.push_str("## 月\n");
    out.push_str(&format!("### 当月 {}-{}\n", ys, ms));
    out.push_str(&render_month_md(&cur_month));
    out.push_str(&format!("### 上月 {}-{}\n", pys, pms));
    out.push_str(&render_month_md(&prev_month));
    out.push('\n');

    // ## 年:年度主线 + 月卡(从年层 jsonl)
    out.push_str("## 年\n");
    out.push_str(&render_year_md(&year_jsonl));

    let path = workspace.join("MEMORY.md");
    let _ = atomic_write_str(&path, &out);
    format!("pack → {}（日 {} 事件 / 当月 {}-{} / 上月 {}-{} / 年 {}）",
        path.display(), day_lines.len(), ys, ms, pys, pms, ys)
}

fn read_jsonl_vec(path: &Path) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    if let Ok(s) = std::fs::read_to_string(path) {
        for line in s.lines() {
            let l = line.trim();
            if l.starts_with('{') {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) { out.push(v); }
            }
        }
    }
    out
}

fn day_event_line(v: &serde_json::Value) -> Option<String> {
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("");
    let (hhmm, evt, ty, title, detail) = (get("hhmm"), get("evt"), get("type"), get("title"), get("detail"));
    if hhmm.is_empty() && evt.is_empty() { return None; }
    let mut line = format!("- {} {} [{}] {}", hhmm, evt, ty, title);
    if !detail.is_empty() {
        line.push_str(&format!("\n  {}", detail));
    }
    Some(line)
}

fn render_month_md(jsonl: &[serde_json::Value]) -> String {
    let mut out = String::new();
    let mut day_lines: Vec<String> = Vec::new();
    for v in jsonl {
        match v.get("type").and_then(|x| x.as_str()).unwrap_or("") {
            "mainline" => {
                let ml = v.get("mainline").and_then(|x| x.as_str()).unwrap_or("");
                let outs = v.get("key_outputs").and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|n| n.as_str()).collect::<Vec<_>>().join(" / ")).unwrap_or_default();
                let kw = v.get("keywords").and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|n| n.as_str()).collect::<Vec<_>>().join("·")).unwrap_or_default();
                if outs.is_empty() {
                    out.push_str(&format!("月度主线: {}\n关键词: {}\n", ml, kw));
                } else {
                    out.push_str(&format!("月度主线: {}\n关键产出: {}\n关键词: {}\n", ml, outs, kw));
                }
            }
            "day" => {
                let date = v.get("date").and_then(|x| x.as_str()).unwrap_or("");
                let ec = v.get("event_count").and_then(|x| x.as_u64()).unwrap_or(0);
                let title = v.get("day_title").and_then(|x| x.as_str()).unwrap_or("");
                let mut line = format!("- {} [{}事件] {}", date, ec, title);
                if let Some(tops) = v.get("top_events").and_then(|x| x.as_array()) {
                    for t in tops {
                        if let Some(s) = t.as_str() { if !s.is_empty() { line.push_str(&format!("\n  · {}", s)); } }
                    }
                }
                day_lines.push(line);
            }
            _ => {}
        }
    }
    day_lines.sort_by(|a, b| b.cmp(a));
    for l in &day_lines { out.push_str(l); out.push('\n'); }
    if out.is_empty() { out = "（无月层数据）\n".into(); }
    out
}

fn render_year_md(jsonl: &[serde_json::Value]) -> String {
    let mut out = String::new();
    let mut month_lines: Vec<String> = Vec::new();
    for v in jsonl {
        match v.get("type").and_then(|x| x.as_str()).unwrap_or("") {
            "year_mainline" => {
                let ml = v.get("year_mainline").and_then(|x| x.as_str()).unwrap_or("");
                out.push_str(&format!("年度主线: {}\n", ml));
            }
            "month" => {
                let m = v.get("month").and_then(|x| x.as_str()).unwrap_or("");
                let title = v.get("month_title").and_then(|x| x.as_str()).unwrap_or("");
                month_lines.push(format!("- {} {}", m, title));
            }
            _ => {}
        }
    }
    month_lines.sort_by(|a, b| b.cmp(a));
    for l in &month_lines { out.push_str(l); out.push('\n'); }
    if out.is_empty() { out = "（无年层数据）\n".into(); }
    out
}

fn atomic_write_str(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn history(workspace: &Path, date: &str, seq_range: Option<(u64, u64)>) -> String {
    // 兼容 compact(20260731) 和横线(2026-07-31) 两种日期格式:
    // compact → 转横线(YYYYMMDD → YYYY-MM-DD)
    let date_norm = if date.len() == 8 && date.chars().all(|c| c.is_ascii_digit()) {
        format!("{}-{}-{}", &date[..4], &date[4..6], &date[6..8])
    } else {
        date.to_string()
    };
    let path = workspace.join("history").join(format!("{date_norm}.jsonl"));
    let text = match std::fs::read_to_string(&path) { Ok(s) => s, Err(_) => return format!("（{date} 无原始对话 history）") };
    let mut out = String::new();
    for line in text.lines() {
        if line.trim().is_empty() { continue; }
        let Ok(ev) = serde_json::from_str::<HistoryEvent>(line) else { continue };
        if let Some((a, b)) = seq_range { if ev.seq < a || ev.seq > b { continue; } }
        out.push_str(&humanize(&ev));
        out.push('\n');
    }
    if out.is_empty() { format!("（{date} 的 seq 范围内无事件）") } else { out }
}

fn humanize(e: &HistoryEvent) -> String {
    let s = e.data.get("content").and_then(|v| v.as_str()).or_else(|| e.data.get("text").and_then(|v| v.as_str())).unwrap_or("");
    match e.kind.as_str() {
        "user" => format!("[seq{} 用户] {s}", e.seq),
        "assistant" => {
            let has_tc = e.data.get("tool_calls").and_then(|v| v.as_array()).map(|a| !a.is_empty()).unwrap_or(false);
            if has_tc { format!("[seq{} 助手] (调用工具)", e.seq) } else { format!("[seq{} 助手] {s}", e.seq) }
        }
        "tool_result" => format!("[seq{} 工具结果:{}] {}",
            e.seq, e.data.get("name").and_then(|v| v.as_str()).unwrap_or(""),
            e.data.get("result").and_then(|v| v.as_str()).unwrap_or("")),
        "external" => format!("[seq{} 外部] {}", e.seq, e.data.get("what").and_then(|v| v.as_str()).unwrap_or("")),
        "subagent_result" => format!("[seq{} 子代理#{}完成] {}",
            e.seq, e.data.get("agent_id").and_then(|v| v.as_u64()).unwrap_or(0),
            e.data.get("summary").and_then(|v| v.as_str()).unwrap_or("")),
        "marker" => format!("[seq{} marker {} until_seq={}] (隐藏)", e.seq,
            e.data.get("marker").and_then(|v| v.as_str()).unwrap_or(""),
            e.data.get("until_seq").and_then(|v| v.as_u64()).unwrap_or(0)),
        _ => format!("[seq{} {}]", e.seq, e.kind),
    }
}

/// mem search <query> [--raw]：关键字 grep（默认 memory/，--raw 扩到 history）。
pub fn search(workspace: &Path, query: &str, raw: bool) -> String {
    if query.trim().is_empty() { return "（请提供搜索关键词）".into(); }
    let mut hits = Vec::new();
    let mem = workspace.join("memory");
    grep_dir(&mem, query, &mut hits);
    if raw {
        let hist = workspace.join("history");
        grep_dir(&hist, query, &mut hits);
    }
    if hits.is_empty() { format!("（未找到 {query:?}）") } else { hits.join("\n") }
}

fn grep_dir(dir: &Path, query: &str, out: &mut Vec<String>) {
    let query_lc = query.to_lowercase();
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() { grep_dir(&p, query, out); continue; }
        let is_jsonl = p.extension().and_then(|s| s.to_str()) == Some("jsonl");
        // memory/ 下的月层/年层 jsonl(YYYY-MM / YYYY)跳过,只搜日层(YYYY-MM-DD)避免上层概要重复噪音
        let stem_len = p.file_stem().and_then(|s| s.to_str()).map(|s| s.len()).unwrap_or(0);
        let is_index_jsonl = is_jsonl && (stem_len == 4 || stem_len == 7); // YYYY or YYYY-MM
        if is_index_jsonl { continue; }
        if let Ok(text) = std::fs::read_to_string(&p) {
            for (i, line) in text.lines().enumerate() {
                // jsonl 行:语义匹配(case-insensitive);非 jsonl(如 MEMORY.md):字面 contains(case-insensitive)
                let hit = if is_jsonl && line.starts_with('{') {
                    jsonl_line_matches(line, &query_lc)
                } else {
                    line.to_lowercase().contains(&query_lc)
                };
                if hit {
                    out.push(format!("{}:{}: {}", p.display(), i + 1, line.trim()));
                }
            }
        }
    }
}

// ─── ls_day / ls_year 辅助：见 count_events_in_month（jsonl 行数）───

/// jsonl 行语义匹配:解析 JSON,检查 title/detail/keywords 等字段是否包含 query(case-insensitive)。
/// query 预期已是 lowercase,字段值也 to_lowercase 后比较。
fn jsonl_line_matches(line: &str, query_lc: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return false; };
    for key in &["title", "detail", "type", "evt"] {
        if let Some(s) = v.get(*key).and_then(|x| x.as_str()) {
            if s.to_lowercase().contains(query_lc) { return true; }
        }
    }
    for key in &["keywords", "top_events", "key_outputs"] {
        if let Some(arr) = v.get(*key).and_then(|x| x.as_array()) {
            for el in arr {
                if let Some(s) = el.as_str() {
                    if s.to_lowercase().contains(query_lc) { return true; }
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryEvent;

    fn ws() -> tempfile::TempDir { tempfile::tempdir().unwrap() }

    fn make_day(ws: &std::path::Path, date: &str, body: &str) {
        // date="2026-07-26" → memory/2026/07/2026-07-26.md
        let (y, m) = (date.split('-').next().unwrap(), date.split('-').nth(1).unwrap());
        let p = ws.join("memory").join(y).join(m).join(format!("{date}.md"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
    }
    fn make_hist(ws: &std::path::Path, date: &str, events: &[HistoryEvent]) {
        let p = ws.join("history").join(format!("{date}.jsonl"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut s = String::new();
        for e in events { s.push_str(&serde_json::to_string(e).unwrap()); s.push('\n'); }
        std::fs::write(&p, s).unwrap();
    }
    fn ev(seq: u64, kind: &str, text: &str) -> HistoryEvent {
        let mut e = match kind {
            "user" => HistoryEvent::user(1000 + seq * 1000, "main", text, &[]),
            "assistant" => HistoryEvent::assistant(1000 + seq * 1000, "main", text, "", vec![]),
            "tool_result" => HistoryEvent::tool_result(1000 + seq * 1000, "main", "bash", text, "c"),
            _ => HistoryEvent::user(0, "main", text, &[]),
        };
        e.seq = seq; e
    }

    // ─── ls 紧凑路由测试 ───

    #[test]
    fn ls_no_arg_lists_years() {
        let w = ws();
        make_day(w.path(), "2026-07-26", "## 12:30 evt-20260726-001 标题A\n");
        make_day(w.path(), "2025-03-01", "## 09:00 evt-20250301-001 标题B\n");
        let out = ls(w.path(), None);
        assert!(out.contains("2026") && out.contains("2025"), "无参列年: {out}");
    }

    /// 造 dream 格式 day 文件内容：segs = [((hhmm, subject, title, detail), (a, b))]。
    fn day_body(date_compact: &str, segs: &[((&str, &str, &str, &str), (u64, u64))]) -> String {
        let date = format!("{}-{}-{}", &date_compact[..4], &date_compact[4..6], &date_compact[6..8]);
        let mut body = String::new();
        for (i, ((hhmm, subj, title, detail), (a, b))) in segs.iter().enumerate() {
            body.push_str(&format!(
                "\n## {hhmm} evt-{date_compact}-{:03} {title}\n**主语**: {subj}\n**详情**: {detail}\n**对话索引**: history/{date}.jsonl#seq[{a},{b}]\n",
                i + 1
            ));
        }
        body
    }

    #[test]
    fn ls_year_lists_months_with_theme() {
        let w = ws();
        make_day(w.path(), "2025-06-15", &day_body("20250615", &[(("10:00", "用户", "做X", "d"), (0, 1))]));
        std::fs::write(w.path().join("MEMORY.md"), "# 记忆\n\n## 年\n- 2025-06: 六月主要做X和Y\n").unwrap();
        let out = ls(w.path(), Some("2025"));
        assert!(out.contains("## 2025（各月概览）"), "标题: {out}");
        assert!(out.contains("- 2025-06: 六月主要做X和Y（月主题 · 1 段）"), "有月主题用月主题: {out}");
    }

    #[test]
    fn ls_year_lists_months_without_theme() {
        let w = ws();
        make_day(w.path(), "2025-09-05", &day_body("20250905", &[(("10:00", "用户", "九月首事", "d"), (0, 1))]));
        let out = ls(w.path(), Some("2025"));
        assert!(out.contains("- 2025-09: 1 段（首条: 九月首事）"), "无月主题显示段数+首条: {out}");
    }

    #[test]
    fn ls_month_lists_day_summaries() {
        let w = ws();
        // 月级文件含 ## 日概括
        let p = w.path().join("memory").join("2026").join("07").join("2026-07.md");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "# 2026-07\n\n## 日概括\n- 2026-07-30: 围绕 dream | drag | 5事件 [seq 1..9]\n- 2026-07-29: 围绕 mem | 3事件 [seq 0..5]\n").unwrap();
        let out = ls(w.path(), Some("202607"));
        assert!(out.contains("## 2026-07（日概括）"), "标题: {out}");
        assert!(out.contains("- 2026-07-30: 围绕 dream"), "列日概括: {out}");
        assert!(out.contains("- 2026-07-29: 围绕 mem"), "列日概括: {out}");
    }

    #[test]
    fn ls_month_dash_alias_eq_compact() {
        let w = ws();
        let p = w.path().join("memory").join("2026").join("07").join("2026-07.md");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "# 2026-07\n\n## 日概括\n- 2026-07-30: x\n").unwrap();
        // 横线 2026-07 等价紧凑 202607
        assert_eq!(ls(w.path(), Some("2026-07")), ls(w.path(), Some("202607")));
    }

    #[test]
    fn ls_day_lists_event_segments() {
        let w = ws();
        make_day(w.path(), "2025-06-15", &day_body("20250615", &[
            (("10:00", "用户", "讨论了 attachments", "拖放+native DnD"), (0, 3)),
            (("14:00", "agent", "实现了 drag-drop", "on_window_event"), (4, 7)),
        ]));
        let out = ls(w.path(), Some("20250615"));
        assert!(out.contains("## 2025-06-15（事件段索引）"), "标题: {out}");
        assert!(out.contains("- 10:00 [用户] 讨论了 attachments — 拖放+native DnD → 2025-06-15#seq[0,3]"), "第一段: {out}");
        assert!(out.contains("- 14:00 [agent] 实现了 drag-drop — on_window_event → 2025-06-15#seq[4,7]"), "第二段: {out}");
    }

    #[test]
    fn ls_bad_format_hint() {
        let w = ws();
        let out = ls(w.path(), Some("foo"));
        assert!(out.contains("日期格式应为"), "坏格式给提示: {out}");
    }

    #[test]
    fn ls_dash_date_routes_like_compact() {
        let w = ws();
        make_day(w.path(), "2025-06-15", &day_body("20250615", &[(("10:00", "用户", "做X", "d"), (0, 1))]));
        // 横线 2025-06-15 等价紧凑 20250615
        assert_eq!(ls(w.path(), Some("2025-06-15")), ls(w.path(), Some("20250615")));
    }

    // ─── read 测试 ───

    #[test]
    fn read_day_prints_segments_with_pointer() {
        let w = ws();
        make_day(w.path(), "2026-07-26", "## 12:30 evt-001 重构\n**对话索引**: history/2026-07-26.jsonl#seq[3,9]\n");
        let out = read_day(w.path(), "2026-07-26");
        assert!(out.contains("重构"));
        assert!(out.contains("seq[3,9]"), "应含对话索引指针: {out}");
    }
    #[test]
    fn read_day_missing_says_none() {
        let w = ws();
        assert!(read_day(w.path(), "2099-01-01").contains("无") || read_day(w.path(), "2099-01-01").is_empty());
    }

    #[test]
    fn history_humanizes_and_filters_seq() {
        let w = ws();
        make_hist(w.path(), "2026-07-26", &[
            ev(1, "user", "你好"), ev(2, "assistant", "嗨"), ev(3, "user", "再做X"), ev(4, "assistant", "好"),
        ]);
        let all = history(w.path(), "2026-07-26", None);
        assert!(all.contains("你好") && all.contains("嗨"), "人性化全量: {all}");
        let sub = history(w.path(), "2026-07-26", Some((2, 3)));
        assert!(sub.contains("嗨") && sub.contains("再做X"), "[2,3] 段: {sub}");
        assert!(!sub.contains("你好"), "seq 1 应被过滤掉: {sub}");
    }

    #[test]
    fn search_finds_in_memory_default() {
        let w = ws();
        make_day(w.path(), "2026-07-26", "## 标题\n**详情**: 关键词foobar在此\n");
        assert!(search(w.path(), "foobar", false).contains("foobar"), "默认搜 memory/");
    }
    #[test]
    fn search_raw_extends_to_history() {
        let w = ws();
        make_hist(w.path(), "2026-07-26", &[ev(1, "user", "secret_value_xyz")]);
        assert!(search(w.path(), "secret_value_xyz", false).contains("未找到"), "默认只搜 memory/");
        assert!(search(w.path(), "secret_value_xyz", true).contains("secret_value_xyz"), "--raw 应扩到 history");
    }

    #[test]
    fn humanize_tool_result_shows_result_content() {
        let e = ev(5, "tool_result", "命令输出 xyz");
        let out = humanize(&e);
        assert!(out.contains("命令输出 xyz"), "tool_result 应显示 result 内容: {out}");
    }

    #[test]
    fn read_month_returns_content_or_missing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("memory").join("2026").join("07").join("2026-07.md");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "# 2026-07\n\n## 月主题\n七月围绕 dream\n").unwrap();
        assert!(read_month(dir.path(), "2026-07").contains("七月围绕 dream"));
        assert!(read_month(dir.path(), "2025-05").contains("无") || read_month(dir.path(), "2025-05").contains("2025-05"));
    }

    #[test]
    fn read_year_returns_content_or_missing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("memory").join("2026").join("2026.md");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "# 2026 月主题索引\n- 2026-07: dream\n").unwrap();
        assert!(read_year(dir.path(), "2026").contains("月主题索引"));
        assert!(read_year(dir.path(), "2099").contains("无") || read_year(dir.path(), "2099").contains("2099"));
    }
}
