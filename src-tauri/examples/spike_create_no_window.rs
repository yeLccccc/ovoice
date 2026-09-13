//! Spike（spec §11 预开发验证门 #1）：权威判定 CREATE_NO_WINDOW 是否隐藏 busybox 控制台窗口。
//!
//! 前序结论：busybox 不强 AllocConsole；CREATE_NO_WINDOW 分配控制台（有 conhost 子），
//! DETACHED_PROCESS 不分配。MainWindowHandle 法不 discriminative（Win11 下 conhost 窗口
//! 不暴露为 MainWindowHandle）。
//!
//! 权威法（per-process、无系统噪声）：原始 user32 FFI ——
//! EnumWindows 遍历顶层窗口，筛 IsWindowVisible 且 class=="ConsoleWindowClass" 的，
//! GetWindowThreadProcessId 拿其所属 pid，对照我们 spawn 出来的 conhost 子 pid：
//!   - 该 pid 出现在可见 ConsoleWindowClass 窗口集合里 → **窗口可见**（黑窗会现）。
//!   - 不在 → 窗口隐藏 / 无窗。
//! Run（从 src-tauri/）：`cargo run --example spike_create_no_window`
//! 结果写 `target/spike_cnw_result.txt`。

#![windows_subsystem = "windows"]

use std::io::Read;
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use kill_tree::blocking::kill_tree_with_config;
use kill_tree::Config;

const CREATE_NO_WINDOW: u32 = 0x0800_0000; // windows-rs 官方值 134217728
const DETACHED_PROCESS: u32 = 0x0000_0008;
const RESULT_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/spike_cnw_result.txt");

#[link(name = "user32")]
extern "system" {
    fn EnumWindows(cb: Option<unsafe extern "system" fn(hwnd: isize, lparam: isize) -> i32>, lparam: isize) -> i32;
    fn IsWindowVisible(hwnd: isize) -> i32;
    fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
}

/// 收集所有「可见顶层窗口」（任意 class）的所属 pid 集合。
/// 不限 class —— 本机默认终端是 Windows Terminal，控制台窗口由 WindowsTerminal.exe 托管，
/// class 不是 ConsoleWindowClass，所以按 class 筛会漏。这里看「有没有新可见窗口出现」即可。
fn visible_window_pids() -> std::collections::HashSet<u32> {
    let mut set: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let ptr = &mut set as *mut std::collections::HashSet<u32> as isize;
    unsafe extern "system" fn cb(hwnd: isize, lparam: isize) -> i32 {
        let s: &mut std::collections::HashSet<u32> =
            &mut *(lparam as *mut std::collections::HashSet<u32>);
        if IsWindowVisible(hwnd) != 0 {
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            s.insert(pid);
        }
        1
    }
    unsafe { EnumWindows(Some(cb), ptr); }
    set
}

fn log(f: &mut std::fs::File, msg: &str) {
    let _ = writeln!(f, "{msg}");
    let _ = f.flush();
}

fn ps(cmd: &str, f: &mut std::fs::File) -> String {
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", cmd])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(e) => {
            log(f, &format!("[warn] powershell failed: {e}"));
            String::from("ERR")
        }
    }
}

/// busybox pid 下 conhost/OpenConsole 子进程的 pid（无则 None）。
fn conhost_child_pid(busybox_pid: u32, f: &mut std::fs::File) -> Option<u32> {
    let cmd = format!(
        "(Get-CimInstance Win32_Process -Filter \"ParentProcessId={busybox_pid}\" | \
         Where-Object {{ $_.Name -eq 'conhost.exe' -or $_.Name -eq 'OpenConsole.exe' }} | \
         Select-Object -First 1).ProcessId"
    );
    ps(&cmd, f).parse::<u32>().ok()
}

/// 把一批 pid 解析成进程名（诊断新窗口归属：conhost / WindowsTerminal / 别的）。
fn pid_names(pids: &[u32], f: &mut std::fs::File) -> Vec<String> {
    if pids.is_empty() {
        return vec![];
    }
    let list = pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
    let cmd = format!(
        "Get-Process -Id {list} -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Name"
    );
    let s = ps(&cmd, f);
    s.lines().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

fn main() {
    let bb = concat!(env!("CARGO_MANIFEST_DIR"), "/binaries/busybox64u.exe");
    let mut f = std::fs::File::create(RESULT_PATH).expect("create result file");
    log(&mut f, &format!("busybox: {bb}"));
    log(&mut f, "parent subsystem = windows（无控制台，模拟 release ovoice）");
    log(&mut f, "CREATE_NO_WINDOW=0x08000000  DETACHED_PROCESS=0x00000008");
    assert!(std::path::Path::new(bb).exists(), "busybox64u.exe 缺失");

    // 基线：当前系统可见顶层窗口数（context，看噪声）
    let base_vis = visible_window_pids();
    log(&mut f, &format!("基线 可见顶层窗口 pid 数 = {}", base_vis.len()));

    // ===================== Part 1：可见性（spawn 前后「可见顶层窗口 pid 集」delta）=====================
    // 看 spawn 是否让系统多出可见顶层窗口（不分 class——WT 默认下控制台窗口非 ConsoleWindowClass）。
    log(&mut f, "\n############ PART 1: 可见性（spawn 前后可见顶层窗口 pid 集 delta）############");
    let mut results: Vec<(&'static str, Vec<u32>, u32, Option<u32>)> = Vec::new();
    // (label, 新可见窗口 pid 集, busybox pid, conhost 子 pid)
    for (label, flags) in [("A no-flag", 0u32), ("B CREATE_NO_WINDOW", CREATE_NO_WINDOW)] {
        log(&mut f, &format!("\n[{label}] flags=0x{flags:08X}"));
        let before = visible_window_pids();
        log(&mut f, &format!("[{label}] spawn 前可见窗口 pid 数 = {}", before.len()));
        let mut c = Command::new(bb);
        c.args(["sh", "-c", "sleep 6"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null());
        if flags != 0 {
            c.creation_flags(flags);
        }
        let pid = match c.spawn() {
            Ok(ch) => ch.id(),
            Err(e) => {
                log(&mut f, &format!("[{label}] spawn FAILED: {e}"));
                continue;
            }
        };
        log(&mut f, &format!("[{label}] spawned busybox pid={pid}"));
        std::thread::sleep(Duration::from_millis(2500));
        let during = visible_window_pids();
        let new_pids: Vec<u32> = during.difference(&before).copied().collect();
        let names = pid_names(&new_pids, &mut f);
        let conhost = conhost_child_pid(pid, &mut f);
        log(&mut f, &format!("[{label}] busybox conhost 子 pid = {conhost:?}"));
        log(&mut f, &format!("[{label}] spawn 后新增可见窗口 pid = {new_pids:?} → 进程名 = {names:?}"));
        results.push((label, new_pids, pid, conhost));
        let _ = kill_tree_with_config(pid, &Config::default());
        std::thread::sleep(Duration::from_millis(1500));
    }
    // 「我们的进程树冒出了可见窗口」= 新可见窗口含 busybox pid 或其 conhost 子 pid。
    let own_window = |r: &(&'static str, Vec<u32>, u32, Option<u32>)| {
        r.1.contains(&r.2) || r.3.map(|c| r.1.contains(&c)).unwrap_or(false)
    };
    let a = results.iter().find(|r| r.0 == "A no-flag").map(|r| own_window(r)).unwrap_or(false);
    let b = results
        .iter()
        .find(|r| r.0 == "B CREATE_NO_WINDOW")
        .map(|r| own_window(r))
        .unwrap_or(false);

    // ===================== Part 2：DETACHED_PROCESS 输出捕获（备选修法）=====================
    log(&mut f, "\n############ PART 2: DETACHED_PROCESS 输出捕获 ############");
    let mut c = Command::new(bb);
    c.args(["sh", "-c", "echo MARKER_42; echo $((6*7))"])
        .creation_flags(DETACHED_PROCESS)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => {
            log(&mut f, &format!("[P2] spawn FAILED: {e}"));
            return;
        }
    };
    log(&mut f, &format!("[P2] spawned busybox pid={} under DETACHED_PROCESS", child.id()));
    let mut buf = String::new();
    let _ = child.stdout.as_mut().map(|s| s.read_to_string(&mut buf));
    let status = child.wait();
    log(&mut f, &format!("[P2] exit = {status:?}"));
    log(&mut f, &format!("[P2] stdout = {buf:?}"));
    let capture_ok = buf.contains("MARKER_42") && buf.contains("42");
    log(&mut f, &format!("[P2] 输出捕获正确? {capture_ok}"));

    // ===================== Part 3：mmx（#002 真触发命令）在 DETACHED 下跑通 + 捕获 =====================
    log(&mut f, "\n############ PART 3: mmx auth status under DETACHED_PROCESS ############");
    let mut c3 = Command::new(bb);
    c3.args(["sh", "-c", "mmx auth status"])
        .creation_flags(DETACHED_PROCESS)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child3 = match c3.spawn() {
        Ok(ch) => ch,
        Err(e) => {
            log(&mut f, &format!("[P3] spawn FAILED: {e}"));
            return;
        }
    };
    log(&mut f, &format!("[P3] spawned busybox pid={} under DETACHED_PROCESS", child3.id()));
    let mut buf3 = String::new();
    let _ = child3.stdout.as_mut().map(|s| s.read_to_string(&mut buf3));
    let status3 = child3.wait();
    log(&mut f, &format!("[P3] exit = {status3:?}"));
    // 只记前几行（含可能 key，截断防泄漏）
    let head3: Vec<&str> = buf3.lines().take(4).collect();
    log(&mut f, &format!("[P3] stdout 前 4 行 = {head3:?}"));
    let mmx_ok = buf3.contains("api-key") || buf3.contains("\"method\"");
    log(&mut f, &format!("[P3] mmx 正常返回 auth JSON? {mmx_ok}"));

    // ===================== VERDICT =====================
    log(&mut f, "\n========== VERDICT ==========");
    log(&mut f, &format!("A(no-flag)          控制台窗口可见? {a}"));
    log(&mut f, &format!("B(CREATE_NO_WINDOW) 控制台窗口可见? {b}"));
    log(&mut f, &format!("DETACHED 输出捕获可用? {capture_ok}"));
    if a && !b {
        log(&mut f, "✅ PASS：no-flag 时窗口可见、CREATE_NO_WINDOW 时窗口隐藏（conhost 在但无可见窗）。");
        log(&mut f, "   → CREATE_NO_WINDOW 对 busybox-w32 生效，#002 修复成立，spec D4 不变。");
    } else if a && b {
        log(&mut f, "❌ CREATE_NO_WINDOW 下窗口仍可见 → CNW 未隐藏 busybox 控制台窗口。");
        if capture_ok {
            log(&mut f, "   DETACHED_PROCESS 输出捕获可用 → #002 改用 DETACHED_PROCESS（无控制台=必无窗）。spec D4 需改 DETACHED。");
        } else {
            log(&mut f, "   且 DETACHED 输出捕获异常 → 需更深方案。");
        }
    } else {
        log(&mut f, "⚠️ AMBIGUOUS：no-flag 时也未观察到可见窗口（可能 conhost 窗口归属另类，或本机默认终端=WT 影响窗口归属）。");
        log(&mut f, "   建议：直接采用 DETACHED_PROCESS（无控制台=必无窗）+ 输出捕获，绕开可见性不确定。");
    }
    log(&mut f, "DONE");
}
