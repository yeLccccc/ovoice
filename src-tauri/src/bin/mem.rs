//! mem —— 记忆深思 CLI（同 crate 第二个 [[bin]]）。零 LLM 只读 drill + dream 写端。
//! cache 定位（mem 只读 memory/+history/，本质是 cache 浏览器）：
//!   --cache 覆盖 > $OVOICE_CACHE > next-to-exe config(portable) > %APPDATA% config > %APPDATA%/cache 默认。
//!   （--workspace / $OVOICE_WORKSPACE 作 legacy 别名，等价指向 cache，兼容旧肌肉记忆。）
use ovoice_lib::{config, history, mem_cli, mem_dream};
use std::path::{Path, PathBuf};

/// dream 子命令的旗标解析。
fn parse_dream_flags(args: &[String]) -> mem_dream::DreamFlags {
    let mut flags = mem_dream::DreamFlags::default();
    for arg in args {
        match arg.as_str() {
            "--once" => flags.once = true,
            "--mechanical" => flags.mechanical = true,
            "--dry-run" => flags.dry_run = true,
            "--force" => flags.force = true,
            _ => {}
        }
    }
    flags
}

fn load_dream_cfg(cache: &Path) -> mem_dream::DreamCfg {
    // config.json 可能在 cache/（flat/portable/测试沙箱）或 cache/..（默认 %APPDATA% 布局：
    // config 在 cache 父目录）。先查 cache，再回退父目录，否则默认 dream 找不到 api_key。
    let cfg_dir = if cache.join("config.json").exists() {
        cache
    } else if cache.parent().map(|p| p.join("config.json").exists()).unwrap_or(false) {
        cache.parent().unwrap()
    } else {
        cache // 都没有 → load_from 返回默认 config（mechanical/dry_run 不需要 key）
    };
    let cfg = config::load_from(cfg_dir);
    mem_dream::DreamCfg {
        api_key: if cfg.api_key.is_empty() { None } else { Some(cfg.api_key.clone()) },
        model: cfg.llm_model.clone(),
        region: cfg.minimax_region.clone(),
        max_rounds: cfg.dream_merge_max_rounds as u32,
        batch_max_events: cfg.dream_batch_max_events as usize,
    }
}

/// dream 子命令的异步 runtime 入口。
fn block_on_dream(args: &[String], cache: &Path) -> i32 {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime 创建失败");
    let flags = parse_dream_flags(args);
    let cfg = load_dream_cfg(cache);
    let sub = args.get(2).map(|s| s.as_str()).unwrap_or("");
    // 第一个非 flag 位置参数（args[3..] 跳过 --force/--once 等）
    let pos = args.iter().skip(3).find(|a| !a.starts_with("--")).map(|s| s.as_str());
    // --force 必须带指定 date/ym/year，无参拒绝（避免误批量重综合）
    if flags.force && pos.is_none() {
        eprintln!("[mem dream] --force 必须指定 date/ym/year（如 day --force 2026-08-02 / month --force 2026-08 / year --force 2026）");
        return 1;
    }
    let result = match sub {
        "day" => match pos {
            Some(date) => rt.block_on(async { mem_dream::dream_day_at(cache, date, &cfg, flags).await }),
            None => rt.block_on(async { mem_dream::run_dream_to_idle(cache, &cfg, flags).await }),
        },
        "month" => match pos {
            Some(ym_str) => {
                let ym = parse_ym_arg(Some(ym_str));
                rt.block_on(async { mem_dream::extract_month(cache, ym, &cfg, flags).await })
            }
            None => rt.block_on(async { auto_complete_months(cache, &cfg, flags).await }),
        },
        "year" => match pos {
            Some(y_str) => match y_str.parse::<i32>().ok() {
                Some(year) => rt.block_on(async { mem_dream::extract_year(cache, year, &cfg, flags).await }),
                None => { eprintln!("[mem dream] year 非法: {y_str}"); return 1; }
            }
            None => rt.block_on(async { auto_complete_years(cache, &cfg, flags).await }),
        },
        _ => rt.block_on(async { mem_dream::run_dream_to_idle(cache, &cfg, flags).await }),
    };
    match result {
        Ok(stats) => { println!("{stats:?}"); 0 }
        Err(e) => { eprintln!("[mem dream] fatal: {e}"); 1 }
    }
}

/// 解析 YYYY-MM → (year, month);缺省取当前月。
fn parse_ym_arg(arg: Option<&str>) -> (i32, u32) {
    match arg {
        Some(s) => {
            let p: Vec<&str> = s.split('-').collect();
            (p.first().and_then(|x| x.parse().ok()).unwrap_or(1970),
             p.get(1).and_then(|x| x.parse().ok()).unwrap_or(1))
        }
        None => current_ym(),
    }
}

fn current_ym() -> (i32, u32) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let off = history::local_offset_secs();
    let date = history::date_from_ts_local(now, off);
    let p: Vec<&str> = date.split('-').collect();
    (p.first().and_then(|x| x.parse().ok()).unwrap_or(1970),
     p.get(1).and_then(|x| x.parse().ok()).unwrap_or(1))
}

#[allow(dead_code)]
fn current_year() -> i32 { current_ym().0 }

/// P1: 自动补齐所有「有日层但无月层」的月。
/// 扫 memory/ 下所有 YYYY/MM/YYYY-MM-DD.jsonl,找没有 YYYY-MM.jsonl 的月,逐个 extract_month。
async fn auto_complete_months(cache: &Path, cfg: &mem_dream::DreamCfg, flags: mem_dream::DreamFlags) -> Result<mem_dream::Stats, mem_dream::DreamError> {
    let mem = cache.join("memory");
    let mut missing: Vec<(i32, u32)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&mem) {
        let mut year_dirs: Vec<_> = rd.flatten().filter(|e| e.path().is_dir()).map(|e| e.path()).collect();
        year_dirs.sort();
        for ydir in year_dirs {
            let year: i32 = ydir.file_name().and_then(|s| s.to_str()).and_then(|s| s.parse().ok()).unwrap_or(0);
            if year == 0 { continue; }
            if let Ok(mrd) = std::fs::read_dir(&ydir) {
                let mut month_dirs: Vec<_> = mrd.flatten().filter(|e| e.path().is_dir()).map(|e| e.path()).collect();
                month_dirs.sort();
                for mdir in month_dirs {
                    let month: u32 = mdir.file_name().and_then(|s| s.to_str()).and_then(|s| s.parse().ok()).unwrap_or(0);
                    if month == 0 { continue; }
                    let ym = format!("{}-{:02}", year, month);
                    let month_jsonl = mdir.join(format!("{}.jsonl", ym));
                    // 有日层(stem=10)但无月层 jsonl → 缺
                    let has_day = std::fs::read_dir(&mdir).map(|rd| {
                        rd.flatten().any(|e| {
                            let p = e.path();
                            p.extension().and_then(|s| s.to_str()) == Some("jsonl") &&
                            p.file_stem().and_then(|s| s.to_str()).map(|s| s.len() == 10).unwrap_or(false)
                        })
                    }).unwrap_or(false);
                    if has_day && !month_jsonl.exists() {
                        missing.push((year, month));
                    }
                }
            }
        }
    }
    if missing.is_empty() {
        eprintln!("[mem dream month] 所有月份月层已生成,无需补齐");
        return Ok(mem_dream::Stats::default());
    }
    eprintln!("[mem dream month] 检测到 {} 个月缺月层,逐个补齐: {:?}", missing.len(),
        missing.iter().map(|(y, m)| format!("{}-{:02}", y, m)).collect::<Vec<_>>());
    let mut total = mem_dream::Stats::default();
    for (y, m) in &missing {
        let s = mem_dream::extract_month(cache, (*y, *m), cfg, flags).await?;
        total.months_rotated += s.months_rotated;
    }
    eprintln!("[mem dream month] 补齐完成: {} 个月", total.months_rotated);
    Ok(total)
}

/// P1: 自动补齐所有「有月层但无年层」的年。
async fn auto_complete_years(cache: &Path, cfg: &mem_dream::DreamCfg, flags: mem_dream::DreamFlags) -> Result<mem_dream::Stats, mem_dream::DreamError> {
    let mem = cache.join("memory");
    let mut missing: Vec<i32> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&mem) {
        let mut year_dirs: Vec<_> = rd.flatten().filter(|e| e.path().is_dir()).map(|e| e.path()).collect();
        year_dirs.sort();
        for ydir in year_dirs {
            let year: i32 = ydir.file_name().and_then(|s| s.to_str()).and_then(|s| s.parse().ok()).unwrap_or(0);
            if year == 0 { continue; }
            let year_jsonl = ydir.join(format!("{}.jsonl", year));
            // 有月层 jsonl(stem=7)但无年层 jsonl → 缺
            let has_month: bool = std::fs::read_dir(&ydir).map(|rd| {
                rd.flatten().filter(|e| e.path().is_dir()).any(|e| {
                    let mdir = e.path();
                    std::fs::read_dir(&mdir).map(|rd| {
                        rd.flatten().any(|f| {
                            let p = f.path();
                            p.extension().and_then(|s| s.to_str()) == Some("jsonl") &&
                            p.file_stem().and_then(|s| s.to_str()).map(|s| s.len() == 7).unwrap_or(false)
                        })
                    }).unwrap_or(false)
                })
            }).unwrap_or(false);
            if has_month && !year_jsonl.exists() {
                missing.push(year);
            }
        }
    }
    if missing.is_empty() {
        eprintln!("[mem dream year] 所有年份年层已生成,无需补齐");
        return Ok(mem_dream::Stats::default());
    }
    eprintln!("[mem dream year] 检测到 {} 个年缺年层,逐个补齐: {:?}", missing.len(), missing);
    let mut total = mem_dream::Stats::default();
    for y in &missing {
        let s = mem_dream::extract_year(cache, *y, cfg, flags).await?;
        total.years_rotated += s.years_rotated;
    }
    eprintln!("[mem dream year] 补齐完成: {} 个年", total.years_rotated);
    Ok(total)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cache = resolve_cache(&args);
    // dispatch 拿 args[1] 当子命令；先剥离 `--cache/--workspace <path>` 对，否则 `mem --cache X ls`
    // 会被当"未知命令: --cache"（bug #3a）。resolve_cache 仍用原 args 自行扫描该 flag。
    let clean = strip_loc_flag(&args);
    if clean.get(1).map(|s| s.as_str()) == Some("dream") {
        let code = block_on_dream(&clean, &cache);
        std::process::exit(code);
    }
    let (out, code) = dispatch(&clean, &cache);
    println!("{out}");
    std::process::exit(code);
}

/// 从 args 中剥离 `--cache <path>` / `--workspace <path>` 这对 flag，让 dispatch 看到的 args[1] 是子命令。
fn strip_loc_flag(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--cache" || a == "--workspace" { it.next(); continue; } // 跳过 flag + 其值
        out.push(a.clone());
    }
    out
}

fn dispatch(args: &[String], cache: &std::path::Path) -> (String, i32) {
    match args.get(1).map(|s| s.as_str()) {
        Some("ls") => {
            let arg = args.get(2).map(|s| s.as_str());
            match arg {
                Some(a) if !valid_ls_arg(a) => (format!("日期非法: {a}(应为 YYYY/YYYYMM/YYYYMMDD,月01-12 日01-31)"), 2),
                _ => (mem_cli::ls(cache, arg), 0),
            }
        }
        Some("read") => {
            let raw = args.get(2).map(|s| s.as_str()).unwrap_or("");
            let date_arg = normalize_date(raw); // 紧凑 YYYYMMDD/YYYYMM → 横线，跟 ls 对齐
            if !valid_read_arg(&date_arg) {
                (format!("日期非法: {raw}(应为 YYYY-MM-DD/YYYY-MM/YYYY,月01-12 日01-31)"), 2)
            } else {
                (read_route(cache, &date_arg), 0)
            }
        }
        Some("history") => (mem_cli::history(cache, args.get(2).map(|s| s.as_str()).unwrap_or(""), parse_seq_flag(args)), 0),
        Some("search") => (mem_cli::search(cache, args.get(2).map(|s| s.as_str()).unwrap_or(""), args.iter().any(|a| a == "--raw")), 0),
        Some("pack") => (mem_cli::pack(cache), 0),
        Some("--help") | Some("-h") | None => (usage(), 0),
        Some(other) => (format!("未知命令: {other}\n\n{}", usage()), 2),
    }
}

/// read 按日期段数路由(合法性已由 valid_read_arg 校验)。
fn read_route(cache: &std::path::Path, date_arg: &str) -> String {
    let parts: Vec<&str> = date_arg.split('-').collect();
    match parts.as_slice() {
        [y, m, d] if y.len() == 4 && m.len() == 2 && d.len() == 2 => mem_cli::read_day(cache, date_arg),
        [y, m] if y.len() == 4 && m.len() == 2 => mem_cli::read_month(cache, date_arg),
        [y] if y.len() == 4 && y.bytes().all(|b| b.is_ascii_digit()) => mem_cli::read_year(cache, date_arg),
        _ => format!("日期格式应为 YYYY-MM-DD / YYYY-MM / YYYY"),
    }
}

/// 紧凑日期 normalize 成横线:YYYYMMDD→YYYY-MM-DD、YYYYMM→YYYY-MM;4 位年或非数字原样返回。
/// 让 read 路由复用横线 split 逻辑,跟 ls 的 digit 数位校验对齐(认紧凑)。
fn normalize_date(arg: &str) -> String {
    let digits: String = arg.chars().filter(|c| c.is_ascii_digit()).collect();
    match digits.len() {
        8 => format!("{}-{}-{}", &digits[0..4], &digits[4..6], &digits[6..8]),
        6 => format!("{}-{}", &digits[0..4], &digits[4..6]),
        _ => arg.to_string(),
    }
}

/// read 日期校验:横线 YYYY-MM-DD / YYYY-MM / YYYY,数值合法(月01-12 日01-31)。
fn valid_read_arg(arg: &str) -> bool {
    let parts: Vec<&str> = arg.split('-').collect();
    match parts.as_slice() {
        [y] if y.len() == 4 && y.bytes().all(|b| b.is_ascii_digit()) => true,
        [y, m] if y.len() == 4 && m.len() == 2 => matches!(m.parse::<u32>(), Ok(1..=12)),
        [y, m, d] if y.len() == 4 && m.len() == 2 && d.len() == 2 =>
            matches!(m.parse::<u32>(), Ok(1..=12)) && matches!(d.parse::<u32>(), Ok(1..=31)),
        _ => false,
    }
}

/// ls 日期校验:compact YYYYMMDD/YYYYMM/YYYY(也认横线),数值合法。
fn valid_ls_arg(arg: &str) -> bool {
    let digits: String = arg.chars().filter(|c| c.is_ascii_digit()).collect();
    match digits.len() {
        4 => true,
        6 => matches!(digits[4..].parse::<u32>(), Ok(1..=12)),
        8 => matches!(digits[4..6].parse::<u32>(), Ok(1..=12)) && matches!(digits[6..].parse::<u32>(), Ok(1..=31)),
        _ => false,
    }
}

fn usage() -> String {
    "mem —— 记忆深思（零 LLM 只读 drill）\n\n用法:\n  mem ls [年/月/日]                 列索引：2026→各月+月主题；202607→各日+日概括；20260731→各事件+seq\n  mem read <年/月/日>               读整理后记忆：2026→年主题；2026-07→月主题+日概括；2026-07-30→事件段\n  mem history <date> [--seq a..b]    读原始对话（可按 seq 过滤）\n  mem search <query> [--raw]         关键字 grep（默认 memory/，--raw 扩到 history）\n\ncache 定位: --cache > $OVOICE_CACHE > (--workspace/$OVOICE_WORKSPACE 别名) > next-to-exe config > %APPDATA% config > %APPDATA%/cache".into()
}

fn parse_seq_flag(args: &[String]) -> Option<(u64, u64)> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--seq" {
            if let Some(v) = it.next() {
                let (s, e) = v.split_once("..").or_else(|| v.split_once(','))?;
                return Some((s.parse().ok()?, e.parse().ok()?));
            }
        }
    }
    None
}

fn resolve_cache(args: &[String]) -> PathBuf {
    // 1. --cache 覆盖（--workspace 作 legacy 别名）
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--cache" || a == "--workspace" {
            if let Some(p) = it.next() { return PathBuf::from(p); }
        }
    }
    // 2. $OVOICE_CACHE（$OVOICE_WORKSPACE 作 legacy 别名）—— agent bash spawn 时注入 OVOICE_CACHE
    if let Some(p) = std::env::var_os("OVOICE_CACHE").or_else(|| std::env::var_os("OVOICE_WORKSPACE")) {
        return PathBuf::from(p);
    }
    // 3. next-to-exe config（portable 优先；读 config 原始 cache_dir 字段）
    if let Some(parent) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf())) {
        if let Some(c) = resolve_from_config(&parent) { return c; }
    }
    // 4. %APPDATA% fallback（读同档 config 的 cache_dir）
    if let Some(ad) = appdata_dir() {
        if let Some(c) = resolve_from_config(&ad) { return c; }
    }
    // 5. 默认 %APPDATA%/com.ovoice.app/cache（cache 不进 Documents）
    appdata_dir().map(|ad| ad.join("cache")).unwrap_or_else(|| config::default_workspace_dir())
}

fn appdata_dir() -> Option<PathBuf> {
    let id = "com.ovoice.app";
    #[cfg(target_os = "windows")]
    { let base = std::env::var_os("APPDATA")?; Some(PathBuf::from(base).join(id)) }
    #[cfg(target_os = "macos")]
    { let home = std::env::var_os("HOME")?; Some(PathBuf::from(home).join("Library/Application Support").join(id)) }
    #[cfg(all(unix, not(target_os = "macos")))]
    { let home = std::env::var_os("HOME")?; Some(PathBuf::from(home).join(".local/share").join(id)) }
}

/// 从 dir/config.json 读原始 cache_dir 字段：空 → None（跳下一档）；非空 → resolve 成绝对
/// （相对拼 dir，绝对原样）。用 load_raw 而非 load_from——后者把空重写成默认、无法判空（bug #3 根因）。
fn resolve_from_config(dir: &std::path::Path) -> Option<PathBuf> {
    let raw = config::load_raw(dir).cache_dir;
    let trimmed = raw.trim();
    if trimmed.is_empty() { return None; }
    Some(config::resolve_cache_dir(trimmed, dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_cache_flag_overrides() {
        let args = vec!["mem".to_string(), "--cache".into(), "/tmp/c".into(), "ls".into()];
        assert_eq!(resolve_cache(&args), PathBuf::from("/tmp/c"));
    }
    #[test]
    fn resolve_cache_flag_anywhere() {
        let args = vec!["mem".to_string(), "history".into(), "2026-07-26".into(), "--cache".into(), "C:/c".into()];
        assert_eq!(resolve_cache(&args), PathBuf::from("C:/c"));
    }
    #[test]
    fn resolve_workspace_flag_is_legacy_alias() {
        // --workspace 作 legacy 别名，等价指向 cache（兼容旧用法）
        let args = vec!["mem".to_string(), "--workspace".into(), "/tmp/leg".into(), "ls".into()];
        assert_eq!(resolve_cache(&args), PathBuf::from("/tmp/leg"));
    }
    #[test]
    fn parse_seq_dotdot() {
        assert_eq!(parse_seq_flag(&["mem".into(),"history".into(),"d".into(),"--seq".into(),"3..9".into()]), Some((3,9)));
    }
    #[test]
    fn parse_seq_comma() {
        assert_eq!(parse_seq_flag(&["mem".into(),"history".into(),"d".into(),"--seq".into(),"3,9".into()]), Some((3,9)));
    }
    #[test]
    fn parse_seq_none_when_absent() {
        assert_eq!(parse_seq_flag(&["mem".into(),"history".into(),"d".into()]), None);
    }
    #[test]
    fn dispatch_routes_read() {
        // dispatch 不依赖 env（cache 由调用方解析）
        let dir = tempfile::tempdir().unwrap();
        let (out, _) = dispatch(&["mem".into(), "read".into(), "2099-01-01".into()], dir.path());
        assert!(out.contains("2099-01-01") || out.contains("无"));
    }

    #[test]
    fn strip_loc_flag_removes_pair() {
        let a = vec!["mem".to_string(), "--cache".into(), "/x".into(), "ls".into(), "2026".into()];
        assert_eq!(strip_loc_flag(&a), vec!["mem".to_string(), "ls".into(), "2026".into()]);
    }
    #[test]
    fn strip_loc_flag_also_strips_legacy_workspace() {
        let a = vec!["mem".to_string(), "--workspace".into(), "/x".into(), "ls".into()];
        assert_eq!(strip_loc_flag(&a), vec!["mem".to_string(), "ls".into()]);
    }
    #[test]
    fn strip_loc_flag_preserves_when_absent() {
        let a = vec!["mem".to_string(), "ls".into()];
        assert_eq!(strip_loc_flag(&a), a);
    }
    #[test]
    fn dispatch_with_leading_cache_flag_not_unknown() {
        // 回归（bug #3a）：mem --cache X ls 不再报"未知命令"
        let dir = tempfile::tempdir().unwrap();
        let clean = strip_loc_flag(&["mem".into(), "--cache".into(), "/x".into(), "ls".into()]);
        let (out, _) = dispatch(&clean, dir.path());
        assert!(!out.contains("未知命令"), "leading --cache 不该报未知命令: {out}");
    }
    #[test]
    fn resolve_from_config_none_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), "{}").unwrap();
        assert_eq!(resolve_from_config(dir.path()), None, "空 cache_dir 应跳档（不当默认用）");
    }
    #[test]
    fn resolve_from_config_some_when_cache_set_absolute() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), r#"{"cache_dir":"D:/mycache"}"#).unwrap();
        assert_eq!(resolve_from_config(dir.path()), Some(PathBuf::from("D:/mycache")));
    }
    #[test]
    fn dispatch_ls_routes_compact() {
        // ls 紧凑路由：20260731 → ls_day（需 day 文件）
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("memory").join("2026").join("07").join("2026-07-31.md");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "## 10:00 evt-20260731-001 测试\n**对话索引**: history/2026-07-31.jsonl#seq[1,2]\n").unwrap();
        let (out, _) = dispatch(&["mem".into(), "ls".into(), "20260731".into()], dir.path());
        assert!(out.contains("测试"), "ls 紧凑日期路由到 ls_day: {out}");
    }
    #[test]
    fn dispatch_index_removed_is_unknown() {
        // index 已删 → 走"未知命令"
        let dir = tempfile::tempdir().unwrap();
        let (out, _) = dispatch(&["mem".into(), "index".into()], dir.path());
        assert!(out.contains("未知命令"), "index 已删: {out}");
    }

    // ─── read 按粒度路由测试（Task 11） ───

    #[test]
    fn dispatch_read_routes_by_granularity() {
        let dir = tempfile::tempdir().unwrap();
        // 准备 day 文件（供 read_day 测试）
        // 路径结构：memory/YYYY/MM/YYYY-MM-DD.md（不是 memory/YYYY/MM/DD/YYYY-MM-DD.md）
        let day_path = dir.path().join("memory").join("2026").join("07").join("2026-07-30.md");
        std::fs::create_dir_all(day_path.parent().unwrap()).unwrap();
        std::fs::write(&day_path, "## 12:00 evt-001 今日事\n").unwrap();

        // YYYY-MM-DD → read_day
        let (out_day, _) = dispatch(&["mem".into(), "read".into(), "2026-07-30".into()], dir.path());
        assert!(out_day.contains("今日事"), "YYYY-MM-DD 应路由到 read_day: {out_day}");

        // YYYY-MM → read_month（文件不存在会友好提示）
        let (out_month, _) = dispatch(&["mem".into(), "read".into(), "2026-07".into()], dir.path());
        assert!(out_month.contains("无") || out_month.contains("2026-07"), "YYYY-MM 应路由到 read_month: {out_month}");

        // YYYY → read_year
        let (out_year, _) = dispatch(&["mem".into(), "read".into(), "2026".into()], dir.path());
        assert!(out_year.contains("无") || out_year.contains("2026"), "YYYY 应路由到 read_year: {out_year}");
    }

    #[test]
    fn load_dream_cfg_falls_back_to_parent_config() {
        // 真实布局：config.json 在 cache 父目录，cache/ 只有 memory/history
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"),
            serde_json::json!({"api_key":"test-key-123","llm_model":"M","minimax_region":"cn",
                             "dream_merge_max_rounds":5}).to_string()).unwrap();
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let cfg = load_dream_cfg(&cache);
        assert_eq!(cfg.api_key.as_deref(), Some("test-key-123"), "应从父目录读到 api_key");
        assert_eq!(cfg.model, "M");
    }
}
