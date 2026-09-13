//! 跨平台 bash 执行器（spec §4）。
//!
//! 唯一命令构造点 [`build_command`]：把 `sh -c <command>` 拼成完整 tokio Command
//! （stdio 管道 + cwd + env 追加 + PATH 注入 + Windows CREATE_NO_WINDOW + Unix process_group）。
//! 前台入口 [`run_foreground`]：spawn + 超时 + kill_tree 杀整树 + 解码 + 截断。
//!
//! 设计取舍（spec §5/§6）：env 暂走「继承 + 追加」（D5 全量 scrub 延后——key 在 config.json
//! 不在 env、全清会缺 TEMP/SYSTEMROOT 砸 Windows 命令）；返回串保持「退出码 N\n...」
//! （D9 工具契约不变，结构化 BashResult D7 延后）。kill_tree 替前台 win_job（D6，跨平台）。

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::shell;
use crate::tools::{decode_output, path_with_exe_dir, truncate, BASH_MAX};

/// Windows：不弹黑色 cmd 闪窗（#002，spec D4；windows-rs 官方值 134217728，spike §11 #1 验过）。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 构造执行 `sh -c <command>` 的 tokio Command（唯一构造点，fg/bg 共用）。
///
/// - stdio 全管道化（调用方自行 wait/drain）。
/// - cwd = workspace。
/// - env：继承父进程 + 追加 `env_vars`（OVOICE_CACHE/WORKSPACE 等）+ `path_with_exe_dir`（exe 父目录前置 PATH，让 mem/busybox 可调）。
/// - Windows：`CREATE_NO_WINDOW`。
/// - Unix：`process_group(0)`（独立进程组，便于超时杀整组）。
pub fn build_command(
    command: &str,
    env_vars: &[(String, String)],
    cwd: &Path,
    kill_on_drop: bool,
) -> Command {
    let (prog, args) = shell::argv(command);
    let mut c = Command::new(&prog);
    c.args(&args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(kill_on_drop);
    for (k, v) in env_vars {
        c.env(k, v);
    }
    if let Some((k, v)) = path_with_exe_dir() {
        c.env(k, v);
    }

    #[cfg(windows)]
    {
        // tokio::process::Command 在 Windows 上有 inherent creation_flags（无需引 CommandExt trait）。
        c.creation_flags(CREATE_NO_WINDOW); // 不弹黑窗
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            c.process_group(0);
        }
    }
    c
}

/// 前台执行：spawn → 超时/中断（kill_tree 杀整树，含 sh 的 `&` 后代）→ 捕获 → 解码 → 截断。
///
/// `cancel`：当前 turn 的中断 token（None = 不支持中断，走原纯超时路径）。cancel 先于
/// 超时就绪时同样 kill_tree 杀树并返回「用户中断」串（interrupt_task 在 bash 执行期间
/// 点下时立即生效的关键——外层 llm.rs 的 select drop 工具 future 前本函数已先走此分支）。
///
/// 返回与旧 `tool_bash` 一致的 `「退出码 N\n<body>」`（D9 契约不变）。
pub async fn run_foreground(
    command: &str,
    env_vars: &[(String, String)],
    cwd: &Path,
    timeout_secs: u64,
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> String {
    eprintln!("[bash] spawn: sh -c {command:?}");
    let mut cmd = build_command(command, env_vars, cwd, true);
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[bash] spawn 失败: {e}");
            return format!("启动失败: {e}");
        }
    };
    let pid = child.id();

    // select 的 cancel 分支不能凭空构造 Elapsed（私有构造器），改为：cancel 就绪时直接
    // 杀树并 return「用户中断」串（与超时分支同款杀法）。timeout future 被 drop 时
    // kill_on_drop 已杀 sh 本身，kill_tree 补杀 `&` 后代——零孤儿不变。
    let result = if let Some(t) = &cancel {
        tokio::select! {
            r = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output()) => r,
            _ = t.cancelled() => {
                eprintln!("[bash] INTERRUPT; kill_tree pid={pid:?}");
                if let Some(pid) = pid {
                    let _ = kill_tree::blocking::kill_tree_with_config(
                        pid,
                        &kill_tree::Config::default(),
                    );
                }
                return "用户中断，已终止".to_string();
            }
        }
    } else {
        tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            child.wait_with_output(),
        )
        .await
    };

    match result {
        Ok(Ok(output)) => {
            let code = output.status.code().unwrap_or(-1);
            eprintln!(
                "[bash] ok code={code} stdout={}B stderr={}B",
                output.stdout.len(),
                output.stderr.len()
            );
            let mut combined = decode_output(&output.stdout);
            if !output.stderr.is_empty() {
                combined.push_str("\n[stderr]\n");
                combined.push_str(&decode_output(&output.stderr));
            }
            let body = truncate(combined.trim(), BASH_MAX, "\n…[已截断]");
            format!("退出码 {}\n{}", code, body)
        }
        Ok(Err(e)) => {
            eprintln!("[bash] wait 失败: {e}");
            format!("执行失败: {e}")
        }
        Err(_) => {
            // 超时：kill_on_drop 已杀 sh 本身；kill_tree 兜底杀 sh 的 `&` 后代（spike §11 #2 验过零孤儿）。
            eprintln!("[bash] TIMEOUT {timeout_secs}s; kill_tree pid={pid:?}");
            if let Some(pid) = pid {
                let _ = kill_tree::blocking::kill_tree_with_config(
                    pid,
                    &kill_tree::Config::default(),
                );
            }
            format!("超时（{}s），已终止", timeout_secs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // cancel 先到（远早于 sleep 完成/超时）→ 立即返回「用户中断」串，耗时秒级。
    #[tokio::test]
    async fn run_foreground_cancel_kills_immediately() {
        let token = tokio_util::sync::CancellationToken::new();
        let ws = std::env::temp_dir();
        let t0 = std::time::Instant::now();
        // sleep 300 + timeout 300：不 cancel 的话测试必然 300s 超时失败 → 红绿信号明确。
        // spawn 后 200ms cancel（模拟 bash 跑着时用户点中断）。
        let tok2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            tok2.cancel();
        });
        let res = run_foreground("sleep 300", &[], &ws, 300, Some(token.clone())).await;
        assert!(res.contains("用户中断"), "应走中断分支, got: {res}");
        assert!(t0.elapsed() < std::time::Duration::from_secs(10),
            "cancel 应立即生效, elapsed: {:?}", t0.elapsed());
    }

    // 无 token（legacy/测试路径）→ 原纯超时行为不变。
    #[tokio::test]
    async fn run_foreground_no_token_timeout_unchanged() {
        let ws = std::env::temp_dir();
        let res = run_foreground("sleep 5", &[], &ws, 1, None).await;
        assert!(res.contains("超时"), "无 token 走纯超时, got: {res}");
    }
}
