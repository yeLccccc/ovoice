//! Unix shell：系统 `/bin/sh`（POSIX sh，macOS/Linux 自带）。spec D1。

pub fn argv(command: &str) -> (String, Vec<String>) {
    ("/bin/sh".into(), vec!["-c".into(), command.into()])
}
