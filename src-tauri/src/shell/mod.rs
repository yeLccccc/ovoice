//! 跨平台 shell 选择（spec §4 / D1）。
//!
//! 一套 POSIX sh 通吃三平台：
//! - Windows → 自带 busybox-w32（`busybox.exe sh -c "<cmd>"`），不依赖系统 cmd。
//! - macOS/Linux → 系统 `/bin/sh -c "<cmd>"`。
//!
//! 对外只暴露 [`argv`]：返回执行 `sh -c <command>` 的 `(program, args)`，
//! 由 [`crate::bash`] 拼成完整 Command（stdio/env/cwd/CREATE_NO_WINDOW/process_group）。

#[cfg(windows)]
mod busybox;
#[cfg(unix)]
mod sh;

/// 返回执行 `sh -c <command>` 的 `(program, args)`。
///
/// - Windows：`(<busybox.exe 路径>, ["sh", "-c", command])`
/// - Unix：`("/bin/sh", ["-c", command])`
///
/// `command` 永远作为**单个 arg** 原样传给 sh（不分词）——这是脱离 cmd、根治 #001 引号地狱的关键。
pub fn argv(command: &str) -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        busybox::argv(command)
    }
    #[cfg(unix)]
    {
        sh::argv(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// command 必须作为最后一个 arg 原样传入（不被 shell/Command 分词）。
    #[test]
    fn argv_passes_command_as_single_unsplit_arg() {
        let cmd = "echo 'a \"b\" c'; ls x y";
        let (prog, args) = argv(cmd);
        assert!(!prog.is_empty(), "program 路径非空");
        assert_eq!(args.last().expect("至少一个 arg"), cmd, "最后 arg = 原始 command 原样");
    }

    /// Windows 下应是 `sh -c <cmd>`（3 arg）；Unix 下 `-c <cmd>`（2 arg）。
    #[test]
    fn argv_shape() {
        let (_, args) = argv("echo hi");
        #[cfg(windows)]
        assert_eq!(args, &["sh".to_string(), "-c".to_string(), "echo hi".to_string()]);
        #[cfg(unix)]
        assert_eq!(args, &["-c".to_string(), "echo hi".to_string()]);
    }
}
