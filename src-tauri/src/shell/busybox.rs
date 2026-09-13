//! Windows shell：自带 busybox-w32（POSIX sh + coreutils，单 exe，零安装）。spec D1/D2/D8。
//!
//! 不吊死在 cmd：调 `busybox.exe sh -c "<cmd>"`，引号交给 sh（#001 根治）、
//! 输出原生 UTF-8（#003 chcp 可删，spec D3）、随包自带（绿色）。
//!
//! 二进制定位（D2/D8，复用 mem.exe 的 release 自带机制）：
//! 1. release：`busybox64u.exe` 随包，落在 ovoice.exe 同目录 → `current_exe()` 父目录。
//! 2. dev：`<CARGO_MANIFEST_DIR>/binaries/busybox64u.exe`（gitignored，下载 staged）。
//! 3. 兜底：交 PATH（`tools::path_with_exe_dir` 已把 exe 父目录前置进 PATH）。

use std::path::PathBuf;

pub fn argv(command: &str) -> (String, Vec<String>) {
    (
        resolve().to_string_lossy().into_owned(),
        vec!["sh".into(), "-c".into(), command.into()],
    )
}

/// 定位 busybox64u.exe：release 同目录 → dev manifest/binaries → 裸名交 PATH。
fn resolve() -> PathBuf {
    // 1. release：ovoice.exe 同目录
    if let Some(p) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("busybox64u.exe")))
    {
        if p.exists() {
            return p;
        }
    }
    // 2. dev：<manifest>/binaries（CARGO_MANIFEST_DIR 编译期烘焙；dev 指向 src-tauri）
    let dev = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("binaries")
        .join("busybox64u.exe");
    if dev.exists() {
        return dev;
    }
    // 3. 兜底：交 PATH
    PathBuf::from("busybox64u.exe")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_uses_sh_c() {
        let (prog, args) = argv("ls");
        assert!(prog.contains("busybox"), "program 应是 busybox 路径：{prog}");
        assert_eq!(args, vec!["sh", "-c", "ls"]);
    }
}
