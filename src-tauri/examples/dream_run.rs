//! dream 手动干跑测试工具（**不改原始数据**）。
//!
//! 把 `%APPDATA%/com.ovoice.app/cache/{memory,history,MEMORY.md}` 拷贝到
//! `%TEMP%/ovoice-dream-dryrun`，对拷贝跑 `execute_dream`（真实 HttpRound / MiniMax M3），
//! 打印日/月/年三层产物 diff + 新 dream marker。
//!
//! 用法：
//!   cargo run --example dream_run                        # a=最后marker+1（通常已追平→空段，测 F12）
//!   cargo run --example dream_run -- --a 4170            # 重提取 seq[a..cur]（测真实 LLM 提取）
//!   cargo run --example dream_run -- --a 4170 --tail 0   # 不留尾（提取到 cur）
//!   cargo run --example dream_run -- --dry               # 只打印段/配置，不调 LLM
//!
//! dream 是写端、mem 是纯读端；本工具只动 temp 拷贝，原始 cache 不变。

use ovoice_lib::config;
use ovoice_lib::dream;
use ovoice_lib::history;
use ovoice_lib::llm::{Emitter, HttpRound, LlmRound};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 静默 emitter：Emitter trait 全方法有默认空实现，空 impl 即可。
struct SilentEmitter;
#[async_trait]
impl Emitter for SilentEmitter {}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_dir_all(&from, &to)?;
        } else if ft.is_file() {
            let _ = std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// 递归收集 root 下所有 *.md，key = 相对 root 的路径（/ 分隔）。
fn collect_md(root: &Path) -> HashMap<String, String> {
    let mut m = HashMap::new();
    fn walk(root: &Path, base: &Path, m: &mut HashMap<String, String>) {
        if let Ok(rd) = std::fs::read_dir(root) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, base, m);
                } else if p.extension().and_then(|x| x.to_str()) == Some("md") {
                    let rel = p.strip_prefix(base).unwrap_or(&p).to_string_lossy().replace('\\', "/");
                    m.insert(rel, std::fs::read_to_string(&p).unwrap_or_default());
                }
            }
        }
    }
    walk(root, root, &mut m);
    m
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app_data: PathBuf = std::env::var_os("APPDATA").ok_or("APPDATA 未设置")?.into();
    let app_data = app_data.join("com.ovoice.app");
    let src_cache = app_data.join("cache");
    if !src_cache.exists() {
        return Err(format!("源 cache 不存在: {}", src_cache.display()).into());
    }

    // 命令行
    let args: Vec<String> = std::env::args().collect();
    let mut a_arg: Option<u64> = None;
    let mut tail_override: Option<u64> = None;
    let mut dry = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--a" => { a_arg = args.get(i + 1).and_then(|s| s.parse().ok()); i += 2; }
            "--tail" => { tail_override = args.get(i + 1).and_then(|s| s.parse().ok()); i += 2; }
            "--dry" => { dry = true; i += 1; }
            _ => i += 1,
        }
    }

    // 1. 拷贝（不动原始）
    let dst = std::env::temp_dir().join("ovoice-dream-dryrun");
    println!("📁 拷贝\n  src: {}\n  dst: {}", src_cache.display(), dst.display());
    if dst.exists() { std::fs::remove_dir_all(&dst)?; }
    copy_dir_all(&src_cache.join("memory"), &dst.join("memory"))?;
    copy_dir_all(&src_cache.join("history"), &dst.join("history"))?;
    if src_cache.join("MEMORY.md").exists() {
        std::fs::copy(&src_cache.join("MEMORY.md"), &dst.join("MEMORY.md"))?;
    }

    // 2. config（拿 api_key/llm_model/dream_*）
    let mut cfg = config::load_raw(&app_data);
    if let Some(t) = tail_override { cfg.dream_tail_rounds = t; }

    // 3. events / cur / a
    let history_dir = dst.join("history");
    let events = history::read_all(&history_dir);
    let offset = history::local_offset_secs();
    let cur = events.iter()
        .filter(|e| e.thread == "main" && e.kind != "marker")
        .map(|e| e.seq).max().unwrap_or(0);
    let last_marker = events.iter()
        .filter(|e| e.kind == "marker" && e.data.get("marker").and_then(|v| v.as_str()) == Some("dream"))
        .filter_map(|e| e.data.get("until_seq").and_then(|v| v.as_u64()))
        .max();
    let a = a_arg.unwrap_or_else(|| last_marker.map(|m| m + 1).unwrap_or(1));

    let seg_user = events.iter().filter(|e| e.seq >= a && e.thread == "main" && e.kind == "user").count();
    let seg_total = events.iter()
        .filter(|e| e.seq >= a && e.seq <= cur && e.thread == "main" && e.kind != "marker")
        .count();

    println!("\n=== 配置 / 段 ===");
    println!("events 总数: {}  cur(main 非 marker 的 max seq) = {}", events.len(), cur);
    println!("最后 dream marker until_seq = {:?}", last_marker);
    println!("a = {}{}", a, if a_arg.is_some() { "（命令行）" } else { "（最后 marker + 1）" });
    println!("dream_tail_rounds={}  dream_cap_turns={}  dream_merge_max_rounds={}",
        cfg.dream_tail_rounds, cfg.dream_cap_turns, cfg.dream_merge_max_rounds);
    println!("llm_model={}  api_key={}", cfg.llm_model,
        if cfg.api_key.trim().is_empty() { "空（回落 MINIMAX_API_KEY）" } else { "已配" });
    println!("段 [a={} .. cur={}]: {} 事件，含 {} 个 user 回合", a, cur, seg_total, seg_user);

    if seg_user == 0 {
        println!("\n⚠ 段内无 user（a>cur 或全被 tail 留尾）→ execute_dream 走 F12 空段：仅写 marker(until_seq=cur) 推进，不提取、不写 day。");
    }

    // 4. snapshot 跑前
    let mem_before = std::fs::read_to_string(dst.join("MEMORY.md")).unwrap_or_default();
    let day_before = collect_md(&dst.join("memory"));

    if dry {
        println!("\n--dry：跳过 LLM 调用，结束。");
        return Ok(());
    }

    // 5. execute_dream（真实 HttpRound / MiniMax M3）
    let writer = history::spawn_writer(history_dir.clone(), offset);
    let round: Arc<dyn LlmRound> = Arc::new(HttpRound);
    let emit = SilentEmitter;
    print!("\n⏳ 调 execute_dream（真实 MiniMax M3）…");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let t0 = std::time::Instant::now();
    let r = dream::execute_dream(round, &cfg, &events, &writer, &history_dir, &dst, a, &emit).await;
    drop(writer); // 关 channel → 后台 writer drain
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let dt = t0.elapsed();
    let b = match r {
        Ok(b) => { println!(" Ok(b={b})  耗时 {:.2}s", dt.as_secs_f64()); b }
        Err(e) => { println!(" ❌ Err: {e}"); return Ok(()); }
    };

    // 6. 产物 diff
    println!("\n=== 产物 diff（dst: {}）===", dst.display());

    // 6a. 新 marker（按 seq 比对：events 跑前没有的）
    let events2 = history::read_all(&history_dir);
    let old_seqs: HashSet<u64> = events.iter().map(|e| e.seq).collect();
    let new_markers: Vec<_> = events2.iter()
        .filter(|e| e.kind == "marker" && e.data.get("marker").and_then(|v| v.as_str()) == Some("dream"))
        .filter(|e| !old_seqs.contains(&e.seq))
        .collect();
    if new_markers.is_empty() {
        println!("✚ 无新 dream marker（IO 失败 propagate 或段空未写？看上方 [dream] stats）");
    } else {
        for m in &new_markers {
            let until = m.data.get("until_seq").and_then(|v| v.as_u64());
            println!("✚ 新 dream marker: seq={} until_seq={:?}（返回 b={}）", m.seq, until, b);
        }
    }

    // 6b. MEMORY.md diff
    let mem_after = std::fs::read_to_string(dst.join("MEMORY.md")).unwrap_or_default();
    if mem_before == mem_after {
        println!("\n📝 MEMORY.md：未变");
    } else {
        let before: HashSet<&str> = mem_before.lines().collect();
        println!("\n📝 MEMORY.md 变化（行 {} → {}）：", mem_before.lines().count(), mem_after.lines().count());
        for line in mem_after.lines() {
            if !before.contains(line) { println!("  + {line}"); }
        }
    }

    // 6c. day 文件新段
    let day_after = collect_md(&dst.join("memory"));
    let mut any = false;
    for (key, after) in &day_after {
        let before = day_before.get(key).cloned().unwrap_or_default();
        if before == *after { continue; }
        let bset: HashSet<&str> = before.lines().collect();
        let new_segs: Vec<&str> = after.lines()
            .filter(|l| l.starts_with("## ") && !bset.contains(*l))
            .collect();
        if !new_segs.is_empty() {
            any = true;
            println!("\n📅 memory/{key} 新增 {} 段：", new_segs.len());
            for s in new_segs { println!("  + {s}"); }
        }
    }
    if !any { println!("\n📅 day 文件：无新段（段空 / 机械兜底未触发 / LLM 失败？看 [dream] stats）"); }

    println!("\n原始 cache 未动；拷贝产物在 {}", dst.display());
    Ok(())
}
