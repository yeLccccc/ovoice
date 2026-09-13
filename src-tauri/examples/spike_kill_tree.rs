//! Spike (spec §11 预开发验证门 #2)：验证 `kill_tree` crate 能杀净 busybox-w32 sh 的 `&` 后代。
//!
//! 核心未知：busybox-w32 的 `&` 到底 fork 出独立 Windows 进程、还是 busybox 内部协程？
//! - 若 fork 独立进程 → kill_tree 必须遍历杀掉；杀 busypid 后留孤儿 sleep = FAIL（需 Job Object 兜底）。
//! - 若 in-process → 只有 1 个 busybox.exe，kill_tree 杀它即可（trivial）。
//!
//! 双重佐证：
//! 1. `kill_tree` 返回 `Vec<Output>` —— 它实际找到并杀了几个进程（target + descendants）。
//! 2. `tasklist` 计 busybox64u.exe 实例数 before/during/after —— 地面真值，确认无孤儿。
//!
//! Run（从 src-tauri/）: `cargo run --example spike_kill_tree`
//! 这是 throwaway 验证程序，非执行器本体。

use std::process::{Command, Stdio};
use std::time::Duration;

use kill_tree::{blocking::kill_tree_with_config, Config, Output};

/// 用 tasklist 计 live busybox64u.exe 进程数（按 image 名，地面真值）。
/// busybox 所有 applet（sh/sleep/...）共用同一 exe，所以计 image 名 = 计所有我们 spawn 的 busybox 进程。
fn count_busybox() -> usize {
    let out = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq busybox64u.exe", "/FO", "CSV", "/NH"])
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.lines().filter(|l| l.contains("busybox64u")).count()
        }
        Err(e) => {
            eprintln!("  [warn] tasklist failed: {e}");
            usize::MAX // 信号：不可信，别当成 0
        }
    }
}

fn main() {
    // busybox 跟 Cargo.toml 同目录的 binaries/（gitignored）。
    let bb = format!("{}/binaries/busybox64u.exe", env!("CARGO_MANIFEST_DIR"));
    println!("busybox: {bb}");
    assert!(
        std::path::Path::new(&bb).exists(),
        "busybox64u.exe 缺失：{bb}"
    );

    let before = count_busybox();
    println!("[1] busybox procs BEFORE spawn : {before}   (期望 0)");

    // 两个后台 sleep + 显式 wait：让 sh 活着、把两个 sleep 挂在自己名下当直接子进程。
    // 没 wait 的话非交互 sh 跑完脚本就退、& 后台 job 被托管走，就不是 sh 的直接后代了。
    let mut child = Command::new(&bb)
        .args(["sh", "-c", "sleep 30 & sleep 30 & echo spawned; wait"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn busybox sh 失败");
    let pid = child.id();
    println!("[2] spawned busybox sh pid={pid}");

    // 给两个 sleep fork 出来的时间。
    std::thread::sleep(Duration::from_secs(2));

    let during = count_busybox();
    println!(
        "[3] busybox procs DURING (子已 fork) : {during}   (期望 >=3：sh + 2×sleep)"
    );

    println!("[4] 调 kill_tree(pid={pid}) ...");
    // Windows 上 signal 被忽略（走 TerminateProcess），SIGKILL 仅写明意图。
    let cfg = Config {
        signal: String::from("SIGKILL"),
        ..Default::default()
    };
    let killed = match kill_tree_with_config(pid, &cfg) {
        Ok(outputs) => outputs,
        Err(e) => {
            println!("    kill_tree 报错: {e:?}");
            Vec::new()
        }
    };
    println!("    kill_tree 报告杀了 {} 个进程:", killed.len());
    for o in &killed {
        match o {
            Output::Killed {
                process_id,
                parent_process_id,
                name,
            } => println!("      ✓ Killed pid={process_id} parent={parent_process_id} name={name}"),
            Output::MaybeAlreadyTerminated { process_id, .. } => {
                println!("      ~ MaybeAlreadyTerminated pid={process_id}")
            }
        }
    }

    // 给 TerminateProcess 生效的时间。
    std::thread::sleep(Duration::from_secs(2));

    let after = count_busybox();
    println!("[5] busybox procs AFTER kill  : {after}   (期望 0 = 无孤儿)");

    let _ = child.wait(); // reap 句柄

    println!("\n========== VERDICT ==========");
    println!("before={before}  during={during}  after={after}  killed.len={}", killed.len());
    let forked_descendants = during.saturating_sub(1); // 减去 sh 父本身
    if after == 0 {
        println!("✅ PASS：无孤儿 busybox 进程，kill_tree 杀净了整树。");
        if forked_descendants >= 2 {
            println!("   busybox sh 确实 fork 出 {forked_descendants} 个独立后代进程，kill_tree 全杀了 → 真未知已 cleared。");
        } else {
            println!("   NOTE: during={during} 暗示 sh 没怎么 fork 独立进程（可能 in-process）；kill_tree 仍 trivially 足够。");
        }
    } else {
        println!("❌ FAIL：{after} 个孤儿 busybox 进程存活 → kill_tree 不够，需 Job Object 兜底。");
    }
}
