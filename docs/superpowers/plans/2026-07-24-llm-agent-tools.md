# ovoice Agent（工具调用 + write/read/bash + 思考分离）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 ovoice 升级为可调用 `write`/`read`/`bash` 的 agent：Rust 权威工具循环 + Tauri 事件实时推送 + `reasoning_split` 思考分离展示 + 全自动执行配非交互护栏。

**Architecture:** Rust 侧 `llm::run_loop<R,E>` 泛型循环（注入 `LlmRound` 单轮调用 + `Emitter` 事件），`tools.rs` 三工具，`chat` 命令用真实 `HttpRound`+`AppEmitter` 装配；前端监听三个事件实时渲染思考区/工具卡，`chat` resolve 后渲染最终 markdown。

**Tech Stack:** Rust + Tauri v2，reqwest，tokio，async-trait，encoding_rs，windows-sys（Job Object/GetACP）；前端 Vanilla JS（无打包器），现有 markdown-it/DOMPurify/hljs。

**Spec:** `docs/superpowers/specs/2026-07-24-llm-agent-tools-design.md`（已 review 修订）

## Global Constraints

- 平台 **Windows**，命令行 PowerShell；Bash 工具用 Git Bash（路径正斜杠）。
- 后端编译：`cargo build --manifest-path src-tauri/Cargo.toml`；测试：`cargo test --manifest-path src-tauri/Cargo.toml`。
- 前端无打包器：库以 vendor UMD `<script>` 注入；`main.js` 是 raw ES module。
- MiniMax OpenAI 兼容端点 `https://api.minimaxi.com/v1/chat/completions`，model `MiniMax-M3`，Bearer 鉴权。
- 工具**全自动执行**（不弹确认）；护栏：循环 12 轮、bash 30s 超时、read 截断 50KB、bash 截断 ~20KB。
- 工作目录默认 `app_data_dir/workspace`，相对路径在此解析，绝对路径放行。
- `chat()` 永不抛 Err：失败也回传 partial history（`ChatResponse.error`）。
- 安全：API key 仅 Rust 侧；`.env` gitignored。

## 文件结构 / 任务依赖

```
Task1 config.rs      (workspace_dir + resolve_workspace)        独立
Task2 tools.rs       (resolve_path + truncate)                   独立
Task6 llm.rs parse   (chat_once + LlmRound + parse_resp)         独立
Task3 tools.rs       (write + read)            依赖 Task2
Task4 tools.rs       (bash) + Cargo deps       依赖 Task2
Task5 tools.rs       (schemas + dispatch)      依赖 Task3,Task4
Task7 llm.rs loop    (run_loop + ChatResponse + Emitter) 依赖 Task5,Task6
Task8 lib.rs         (chat 命令 + AppEmitter + 注册 tools) 依赖 Task1,Task7
Task9 index.html+main.js (workspace 设置项)   依赖 Task1
Task10 main.js       (事件 + 三段气泡 + busy 闸) 依赖 Task8,Task9
Task11 styles.css    (思考区 + 工具卡)          独立（可并行）
Task12 冒烟验证      依赖 全部
```
可并行批：{Task1, Task2, Task6, Task11} 同时启动；之后 Task3/Task4 并行；Task5 之后 Task7；Task8/Task9 并行；Task10；Task12。

## 关键接口契约（跨任务共享，名字/类型必须一致）

```rust
// tools.rs
pub fn resolve_path(input: &str, workspace: &Path) -> PathBuf;
pub fn truncate(s: &str, max_bytes: usize, note: &str) -> String;
pub fn tool_write(args: &Value, workspace: &Path) -> String;
pub fn tool_read(args: &Value, workspace: &Path) -> String;
pub async fn tool_bash(args: &Value, workspace: &Path, timeout_secs: u64) -> String;
pub fn schemas() -> Vec<Value>;
pub async fn dispatch(name: &str, args: Value, workspace: &Path) -> String;

// llm.rs
pub struct ParsedResp { pub content: String, pub reasoning: String, pub tool_calls: Vec<Value>, pub assistant_message: Value }
#[async_trait] pub trait LlmRound: Send+Sync { async fn round(&self, messages:&[Value], cfg:&Config)->Result<ParsedResp,String>; }
pub struct HttpRound;
pub async fn chat_once(messages:&[Value], cfg:&Config)->Result<ParsedResp,String>;
#[async_trait] pub trait Emitter: Send+Sync { async fn thinking(&self,text:&str); async fn tool_call(&self,name:&str,args:&str); async fn tool_result(&self,name:&str,result:&str); }
#[derive(serde::Serialize, Clone)] pub struct ChatResponse { pub content:String, pub history:Vec<Value>, pub error:Option<String> }
pub async fn run_loop<R:LlmRound, E:Emitter>(round:R, emitter:E, cfg:&Config, messages:Vec<Value>, workspace:&Path)->ChatResponse;
```

---

### Task 1: config —— workspace_dir 字段 + 运行时默认解析

**Files:**
- Modify: `src-tauri/src/config.rs`

**Interfaces:**
- Produces: `Config.workspace_dir: String`（serde 默认 `""`）；`config::resolve_workspace(field:&str, app_data_dir:&Path)->PathBuf`；`config::load` 把空值解析为 `app_data_dir/workspace`。

- [ ] **Step 1: 写失败测试**（追加到 `config.rs` 的 `#[cfg(test)] mod tests`）

```rust
use std::path::{Path, PathBuf};

#[test]
fn resolve_workspace_empty_defaults() {
    let d = Path::new("C:/fake/appdata");
    assert_eq!(resolve_workspace("", d), d.join("workspace"));
    assert_eq!(resolve_workspace("   ", d), d.join("workspace"));
}
#[test]
fn resolve_workspace_absolute_passthrough() {
    let d = Path::new("C:/fake/appdata");
    assert_eq!(resolve_workspace("D:/proj", d), PathBuf::from("D:/proj"));
}
#[test]
fn resolve_workspace_relative_joined() {
    let d = Path::new("C:/fake/appdata");
    assert_eq!(resolve_workspace("myproj", d), d.join("myproj"));
}
```
并在 `roundtrip_preserves_all_fields` 测试里补一行：构造 cfg 时设 `workspace_dir:"D:/w".into()`，断言 `assert_eq!(back.workspace_dir, "D:/w");`。

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml config::tests`
Expected: 编译失败（`resolve_workspace` 未定义）。

- [ ] **Step 3: 实现**

在 `Config` 结构体加字段（紧随 `hold_gate_ms` 之后）：
```rust
    // Agent 工作目录（write/read/bash 相对路径根；空 → app_data_dir/workspace）
    #[serde(default)]
    pub workspace_dir: String,
```
`Default` impl 里加 `workspace_dir: String::new(),`。

加纯函数（模块顶层，`save` 函数之后）：
```rust
/// 解析工作目录：空 → app_data_dir/workspace；绝对路径原样；相对路径拼到 app_data_dir 下。
pub fn resolve_workspace(field: &str, app_data_dir: &Path) -> std::path::PathBuf {
    let trimmed = field.trim();
    if trimmed.is_empty() {
        return app_data_dir.join("workspace");
    }
    let p = std::path::PathBuf::from(trimmed);
    if p.is_absolute() { p } else { app_data_dir.join(trimmed) }
}
```

改 `load`，让它在返回前解析 `workspace_dir`：
```rust
pub fn load(app: &AppHandle) -> Config {
    let dir = match app.path().app_data_dir() {
        Ok(d) => d,
        Err(_) => return Config::default(),
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("config.json");
    let mut cfg = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str::<Config>(&s).unwrap_or_else(|_| Config::default()),
        Err(_) => Config::default(),
    };
    cfg.workspace_dir = resolve_workspace(&cfg.workspace_dir, &dir).to_string_lossy().to_string();
    cfg
}
```
（`path(app)` 仍保留给 `save` 用。）

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml config::tests`
Expected: PASS（含 roundtrip 的 workspace_dir 断言）。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/config.rs
git commit -m "feat(config): workspace_dir 字段 + 运行时默认解析"
```

---

### Task 2: tools —— 路径解析 + 输出截断（新模块骨架）

**Files:**
- Create: `src-tauri/src/tools.rs`
- Modify: `src-tauri/src/lib.rs`（加 `pub mod tools;`）

**Interfaces:**
- Produces: `tools::resolve_path`、`tools::truncate`。

- [ ] **Step 1: 写失败测试**（新建 `tools.rs`，先只放测试）

```rust
use serde_json::Value;
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_path_absolute() {
        let ws = Path::new("C:/ws");
        assert_eq!(resolve_path("D:/x.txt", ws), PathBuf::from("D:/x.txt"));
    }
    #[test]
    fn resolve_path_relative() {
        let ws = Path::new("C:/ws");
        assert_eq!(resolve_path("a/b.txt", ws), ws.join("a/b.txt"));
    }
    #[test]
    fn resolve_path_trims() {
        let ws = Path::new("C:/ws");
        assert_eq!(resolve_path("  a.txt  ", ws), ws.join("a.txt"));
    }
    #[test]
    fn truncate_short_unchanged() {
        assert_eq!(truncate("abc", 100, "..."), "abc");
    }
    #[test]
    fn truncate_over_cuts_at_boundary() {
        let s = "abcdefghij";
        let out = truncate(s, 5, "|");
        assert!(out.starts_with("abcde") || out.starts_with("abcd")); // 在 char 边界
        assert!(out.ends_with('|'));
    }
    #[test]
    fn truncate_multibyte_boundary() {
        let s = "中文测试"; // 每字 3 字节
        let out = truncate(s, 4, "|");
        assert_eq!(out, "中|"); // 3 字节一个汉字，4 处非边界回退到 3
    }
}
```

- [ ] **Step 2: 在 lib.rs 注册模块**

`src-tauri/src/lib.rs` 的模块声明区（`pub mod llm;` 同级）加：
```rust
pub mod tools;
```

- [ ] **Step 3: 运行测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml tools::tests`
Expected: 编译失败（`resolve_path`/`truncate` 未定义）。

- [ ] **Step 4: 实现**（在 `tools.rs` 测试 mod 之前）

```rust
use serde_json::Value;
use std::path::{Path, PathBuf};

/// 相对路径基于 workspace 解析；绝对路径原样返回；自动 trim。
pub fn resolve_path(input: &str, workspace: &Path) -> PathBuf {
    let p = PathBuf::from(input.trim());
    if p.is_absolute() { p } else { workspace.join(p) }
}

/// 按字节上限截断（在 UTF-8 char 边界切），附 note 标记。
pub fn truncate(s: &str, max_bytes: usize, note: &str) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str(note);
    out
}
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml tools::tests`
Expected: PASS。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/tools.rs src-tauri/src/lib.rs
git commit -m "feat(tools): 路径解析 + 输出截断（新模块）"
```

---

### Task 3: tools —— write + read

**Files:**
- Modify: `src-tauri/src/tools.rs`
- Modify: `src-tauri/Cargo.toml`（dev-dep 加 `tempfile`）

**Interfaces:**
- Consumes: `resolve_path`、`truncate`（Task 2）。
- Produces: `tool_write`、`tool_read`。

- [ ] **Step 1: Cargo 加 tempfile dev-dep**

`src-tauri/Cargo.toml` 的 `[dev-dependencies]`：
```toml
[dev-dependencies]
hound = "3.5"
tempfile = "3"
```

- [ ] **Step 2: 写失败测试**（追加到 `tools.rs` 的 `tests` mod）

```rust
    #[test]
    fn write_then_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let args = serde_json::json!({ "path":"sub/a.txt", "content":"hello 世界" });
        let w = tool_write(&args, ws);
        assert!(w.contains("已写入"));
        let r = tool_read(&serde_json::json!({"path":"sub/a.txt"}), ws);
        assert_eq!(r, "hello 世界");
    }
    #[test]
    fn write_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        tool_write(&serde_json::json!({"path":"a/b/c.txt","content":"x"}), ws);
        assert_eq!(tool_read(&serde_json::json!({"path":"a/b/c.txt"}), ws), "x");
    }
    #[test]
    fn read_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let r = tool_read(&serde_json::json!({"path":"nope.txt"}), dir.path());
        assert!(r.contains("读取失败"));
    }
    #[test]
    fn read_binary_detected() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::write(ws.join("bin.dat"), [0u8, 1, 0, 2]).unwrap();
        let r = tool_read(&serde_json::json!({"path":"bin.dat"}), ws);
        assert!(r.contains("二进制"));
    }
    #[test]
    fn write_missing_args_errors() {
        let r = tool_write(&serde_json::json!({"path":"a"}), Path::new("."));
        assert!(r.contains("缺少 content"));
    }
```

- [ ] **Step 3: 运行测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml tools::tests`
Expected: 编译失败（`tool_write`/`tool_read` 未定义）。

- [ ] **Step 4: 实现**（`tools.rs`，`truncate` 函数之后）

```rust
const READ_MAX: usize = 50 * 1024;

pub fn tool_write(args: &Value, workspace: &Path) -> String {
    let path = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => resolve_path(p, workspace),
        None => return "write 缺少 path 参数".into(),
    };
    let content = match args.get("content").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return "write 缺少 content 参数".into(),
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return format!("创建目录失败 {}: {}", parent.display(), e);
        }
    }
    match std::fs::write(&path, content.as_bytes()) {
        Ok(_) => format!("已写入 {}（{} 字节）", path.display(), content.len()),
        Err(e) => format!("写入失败 {}: {}", path.display(), e),
    }
}

pub fn tool_read(args: &Value, workspace: &Path) -> String {
    let path = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => resolve_path(p, workspace),
        None => return "read 缺少 path 参数".into(),
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => return format!("读取失败 {}: {}", path.display(), e),
    };
    let total = bytes.len();
    if bytes.contains(&0u8) {
        return format!("{} 是二进制文件（{} 字节），read 仅支持文本", path.display(), total);
    }
    let text = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return format!("{} 非 UTF-8 文本（{} 字节）", path.display(), total),
    };
    if total > READ_MAX {
        truncate(&text, READ_MAX, &format!("\n…[已截断，共 {} 字节]", total))
    } else {
        text
    }
}
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml tools::tests`
Expected: PASS。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/tools.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat(tools): write/read 文件工具"
```

---

### Task 4: tools —— bash（chcp 65001 + encoding 兜底 + 超时 + Job Object 杀进程树）

**Files:**
- Modify: `src-tauri/src/tools.rs`
- Modify: `src-tauri/Cargo.toml`（加 `encoding_rs`，windows-sys 加 feature）

**Interfaces:**
- Consumes: `truncate`（Task 2）。
- Produces: `tool_bash(args, workspace, timeout_secs)`（async）。

- [ ] **Step 1: Cargo 依赖**

`Cargo.toml` `[dependencies]`：
```toml
encoding_rs = "0.8"
```
改 windows-sys 行（加 JobObjects/Threading/Globalization feature）：
```toml
windows-sys = { version = "0.59", features = ["Win32_UI_Input_KeyboardAndMouse", "Win32_System_JobObjects", "Win32_System_Threading", "Win32_Globalization"] }
```

- [ ] **Step 2: 写失败测试**（追加到 `tools.rs` 的 `tests` mod）

```rust
    #[tokio::test]
    async fn bash_echo() {
        let dir = tempfile::tempdir().unwrap();
        let out = tool_bash(&serde_json::json!({"command":"echo hello"}), dir.path(), 10).await;
        assert!(out.contains("hello"), "got: {out}");
        assert!(out.contains("退出码 0"), "got: {out}");
    }
    #[tokio::test]
    async fn bash_timeout_kills() {
        let dir = tempfile::tempdir().unwrap();
        // ping -n 60 持续约 60s，2s 超时
        let out = tool_bash(&serde_json::json!({"command":"ping -n 60 127.0.0.1"}), dir.path(), 2).await;
        assert!(out.contains("超时"), "got: {out}");
    }
    #[tokio::test]
    async fn bash_missing_command() {
        let out = tool_bash(&serde_json::json!({}), Path::new("."), 5).await;
        assert!(out.contains("缺少 command"));
    }
```

- [ ] **Step 3: 运行测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml tools::tests`
Expected: 编译失败（`tool_bash` 未定义 + 缺 encoding_rs/windows-sys feature）。

- [ ] **Step 4: 实现**（`tools.rs`，`tool_read` 之后）

```rust
use std::time::Duration;

const BASH_MAX: usize = 20 * 1024;

#[cfg(windows)]
mod win_job {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        JOB_OBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, SetInformationJobObject,
        PROCESS_SET_QUOTA, PROCESS_TERMINATE};

    /// Windows Job Object：句柄关闭时杀掉整棵进程树（KILL_ON_JOB_CLOSE）。
    pub struct Job(HANDLE);
    impl Job {
        pub fn create() -> Option<Self> {
            unsafe {
                let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if h == 0 { return None; }
                let mut info: JOB_OBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    h, JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOB_OBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 { CloseHandle(h); return None; }
                Some(Job(h))
            }
        }
        pub fn assign_pid(&self, pid: u32) {
            unsafe {
                let h = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
                if h != 0 { AssignProcessToJobObject(self.0, h); CloseHandle(h); }
            }
        }
    }
    impl Drop for Job {
        fn drop(&mut self) { unsafe { CloseHandle(self.0); } }
    }
}

/// 解码 cmd 输出：chcp 65001 后应为 UTF-8；失败按系统 OEM 码页兜底（中文 locale 多为 GBK）。
fn decode_output(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let cp = unsafe { windows_sys::Win32::Globalization::GetACP() };
    let enc = encoding_rs::encoding_from_windows_code_page(cp).unwrap_or(encoding_rs::GB18030);
    enc.decode(bytes).0.to_string()
}

pub async fn tool_bash(args: &Value, workspace: &Path, timeout_secs: u64) -> String {
    let command = match args.get("command").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return "bash 缺少 command 参数".into(),
    };
    let full = format!("chcp 65001>nul && {}", command);
    let mut cmd = tokio::process::Command::new("cmd");
    cmd.arg("/C").arg(&full)
        .current_dir(workspace)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return format!("启动失败: {}", e),
    };

    // 入 Job Object：超时时杀整棵进程树（含 cmd /C 启动的孙进程）。
    #[cfg(windows)]
    let _job = {
        let job = win_job::Job::create();
        if let Some(j) = &job {
            if let Some(pid) = child.id() {
                j.assign_pid(pid as u32);
            }
        }
        job
    };

    let result = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        child.wait_with_output(),
    ).await;

    match result {
        Ok(Ok(output)) => {
            let mut combined = decode_output(&output.stdout);
            if !output.stderr.is_empty() {
                combined.push_str("\n[stderr]\n");
                combined.push_str(&decode_output(&output.stderr));
            }
            let code = output.status.code().unwrap_or(-1);
            let body = truncate(combined.trim(), BASH_MAX, "\n…[已截断]");
            format!("退出码 {}\n{}", code, body)
        }
        Ok(Err(e)) => format!("执行失败: {}", e),
        Err(_) => {
            // 超时：child 已随超时 future 丢弃 + kill_on_drop 终止；_job 离开作用域时杀整棵树。
            format!("超时（{}s），已终止", timeout_secs)
        }
    }
}
```

> 说明：`_job` 用 `#[cfg(windows)]` 绑定，在非 windows 平台编译跳过（本项目仅 windows）。`child.id()` 在 move 进 `wait_with_output` 之前调用。

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml tools::tests`
Expected: PASS（echo 即时返回；ping 在 2s 超时；空参报错）。注意 ping 超时测试约等 2s。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/tools.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat(tools): bash 工具（chcp65001+encoding兜底+超时+JobObject杀进程树）"
```

---

### Task 5: tools —— 工具 schemas + dispatch

**Files:**
- Modify: `src-tauri/src/tools.rs`

**Interfaces:**
- Consumes: `tool_write`/`tool_read`/`tool_bash`（Task 3/4）。
- Produces: `schemas()`、`dispatch(name, args, workspace)`。

- [ ] **Step 1: 写失败测试**（追加到 `tools.rs` 的 `tests` mod）

```rust
    #[tokio::test]
    async fn dispatch_routes_write() {
        let dir = tempfile::tempdir().unwrap();
        let r = dispatch("write", serde_json::json!({"path":"a.txt","content":"x"}), dir.path()).await;
        assert!(r.contains("已写入"));
    }
    #[tokio::test]
    async fn dispatch_unknown_tool() {
        let r = dispatch("nope", serde_json::json!({}), Path::new(".")).await;
        assert!(r.contains("未知工具"));
    }
    #[test]
    fn schemas_has_three_tools() {
        let s = schemas();
        let names: Vec<&str> = s.iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["write", "read", "bash"]);
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml tools::tests`
Expected: 编译失败（`dispatch`/`schemas` 未定义）。

- [ ] **Step 3: 实现**（`tools.rs`，文件末尾）

```rust
/// 三工具的 JSON Schema（传给模型 tools 字段）。description 详尽以引导正确调用。
pub fn schemas() -> Vec<Value> {
    vec![
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"write",
                "description":"把文本写入文件（UTF-8，覆盖已有文件，自动建父目录）。Windows 桌面环境；相对路径基于工作目录。",
                "parameters":{
                    "type":"object",
                    "properties":{
                        "path":{"type":"string","description":"文件路径，相对路径基于工作目录，绝对路径也可"},
                        "content":{"type":"string","description":"要写入的文本"}
                    },
                    "required":["path","content"]
                }
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"read",
                "description":"读取文本文件内容（UTF-8；二进制或超 50KB 会标注）。相对路径基于工作目录。",
                "parameters":{
                    "type":"object",
                    "properties":{
                        "path":{"type":"string","description":"文件路径"}
                    },
                    "required":["path"]
                }
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"bash",
                "description":"在 Windows cmd 中执行命令（工作目录=workspace；30s 超时；输出 UTF-8）。避免破坏性命令。",
                "parameters":{
                    "type":"object",
                    "properties":{
                        "command":{"type":"string","description":"要执行的 shell 命令（经 cmd /C 运行）"}
                    },
                    "required":["command"]
                }
            }
        }),
    ]
}

/// 按工具名分发执行；未知工具返回错误字符串（不 panic）。
pub async fn dispatch(name: &str, args: Value, workspace: &Path) -> String {
    match name {
        "write" => tool_write(&args, workspace),
        "read" => tool_read(&args, workspace),
        "bash" => tool_bash(&args, workspace, 30).await,
        other => format!("未知工具: {}", other),
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml tools::tests`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/tools.rs
git commit -m "feat(tools): schemas + dispatch 分发"
```

---

### Task 6: llm —— chat_once 单轮 + LlmRound trait + 响应解析

**Files:**
- Modify: `src-tauri/src/llm.rs`

**Interfaces:**
- Produces: `ParsedResp`、`LlmRound`、`HttpRound`、`chat_once`、`parse_resp`、`build_body`。

- [ ] **Step 1: 写失败测试**（追加到 `llm.rs` 的 `tests` mod）

```rust
    #[test]
    fn parse_resp_splits_content_and_reasoning() {
        let v = serde_json::json!({
            "choices":[{"message":{
                "content":"答案是 42",
                "reasoning_details":[{"text":"先想"},{"text":"再想"}],
                "reasoning_content":"先想再想"
            }}]
        });
        let p = parse_resp(&v).unwrap();
        assert_eq!(p.content, "答案是 42");
        assert_eq!(p.reasoning, "先想再想");
        assert!(p.tool_calls.is_empty());
    }
    #[test]
    fn parse_resp_tool_calls_nulls_content_in_echo() {
        let v = serde_json::json!({
            "choices":[{"message":{
                "content":"忽略我",
                "tool_calls":[{"id":"c1","type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}}]
            }}]
        });
        let p = parse_resp(&v).unwrap();
        assert_eq!(p.tool_calls.len(), 1);
        // 回传给模型的 assistant 消息：tool_calls 在场时 content 必须 null
        assert_eq!(p.assistant_message["content"], serde_json::Value::Null);
        assert_eq!(p.assistant_message["tool_calls"][0]["id"], "c1");
    }
    #[test]
    fn build_body_has_tools_and_reasoning_split() {
        let cfg = Config::default();
        let body = build_body(&[serde_json::json!({"role":"user","content":"hi"})], &cfg);
        assert_eq!(body["reasoning_split"], true);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"].as_array().unwrap().len(), 3);
    }
```
`llm.rs` 顶部需要 `use crate::config::Config;`、`use crate::tools;`（已有 config import；补 tools）。

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml llm::tests`
Expected: 编译失败（`parse_resp`/`build_body`/`ParsedResp` 未定义）。

- [ ] **Step 3: 实现**（`llm.rs`，在现有 `api_key` 之后、`chat` 之前插入结构体与 trait；改造 `chat` 留到 Task 8）

```rust
use crate::tools;
use async_trait::async_trait;

/// 单轮解析结果。assistant_message 是要原样回传给模型的消息对象。
#[derive(Debug, Clone)]
pub struct ParsedResp {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<Value>,
    pub assistant_message: Value,
}

/// 解析 OpenAI 兼容响应（reasoning_split=true 形态）。
pub fn parse_resp(v: &Value) -> Result<ParsedResp, String> {
    let msg = &v["choices"][0]["message"];
    if msg.is_null() {
        return Err(format!("响应缺少 message: {}", truncate(&v.to_string(), 400)));
    }
    let content = msg["content"].as_str().unwrap_or("").to_string();
    let reasoning = msg["reasoning_details"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|d| d["text"].as_str()).collect::<Vec<_>>().join(""))
        .filter(|s| !s.is_empty())
        .or_else(|| msg["reasoning_content"].as_str().map(String::from))
        .unwrap_or_default();
    let tool_calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
    let mut echo = msg.clone();
    if !tool_calls.is_empty() {
        echo["content"] = Value::Null; // H5：tool_calls 在场时 content 置 null
    }
    Ok(ParsedResp { content, reasoning, tool_calls, assistant_message: echo })
}

/// 构造请求体（含 tools + reasoning_split；省略 thinking 即默认开启）。
pub fn build_body(messages: &[Value], cfg: &Config) -> Value {
    json!({
        "model": cfg.llm_model,
        "messages": messages,
        "tools": tools::schemas(),
        "tool_choice": "auto",
        "reasoning_split": true,
    })
}

/// 单轮 LLM 调用（HTTP）。返回解析后的 ParsedResp。
pub async fn chat_once(messages: &[Value], cfg: &Config) -> Result<ParsedResp, String> {
    let key = api_key(cfg)?;
    let client = reqwest::Client::new();
    let body = build_body(messages, cfg);
    let resp = client
        .post(format!("{}/chat/completions", API_BASE))
        .bearer_auth(&key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("LLM 请求失败: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("读取响应失败: {e}"))?;
    if !status.is_success() {
        return Err(format!("LLM HTTP {status}: {}", truncate(&text, 600)));
    }
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| format!("解析响应失败: {e} | body: {}", truncate(&text, 600)))?;
    parse_resp(&v)
}

/// 可注入的单轮调用抽象（Task 7 循环测试用 FakeRound）。
#[async_trait]
pub trait LlmRound: Send + Sync {
    async fn round(&self, messages: &[Value], cfg: &Config) -> Result<ParsedResp, String>;
}

pub struct HttpRound;
#[async_trait]
impl LlmRound for HttpRound {
    async fn round(&self, messages: &[Value], cfg: &Config) -> Result<ParsedResp, String> {
        chat_once(messages, cfg).await
    }
}
```

> 此时旧的 `pub async fn chat(...)` 仍存在（Task 8 会替换它）。本任务暂保留它以保持编译通过；若与新增 `chat_once` 同名冲突则无（名字不同）。`strip_think`/`truncate` 保留。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml llm::tests`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/llm.rs
git commit -m "feat(llm): chat_once 单轮 + LlmRound 注入 + reasoning_split 解析"
```

---

### Task 7: llm —— run_loop 循环 + ChatResponse + Emitter

**Files:**
- Modify: `src-tauri/src/llm.rs`

**Interfaces:**
- Consumes: `ParsedResp`/`LlmRound`（Task 6）、`tools::dispatch`（Task 5）。
- Produces: `ChatResponse`、`Emitter`、`run_loop`。

- [ ] **Step 1: 写失败测试**（追加到 `llm.rs` 的 `tests` mod）

```rust
    use std::sync::Mutex;
    use async_trait::async_trait;
    use std::collections::VecDeque;

    // 一个按脚本返回的假单轮 + 录音 emitter
    struct FakeRound(Mutex<VecDeque<Result<ParsedResp, String>>>);
    #[async_trait]
    impl LlmRound for FakeRound {
        async fn round(&self, _m: &[Value], _cfg: &Config) -> Result<ParsedResp, String> {
            self.0.lock().unwrap().pop_front().unwrap_or_else(|| Ok(ParsedResp {
                content: "默认最终".into(), reasoning: String::new(),
                tool_calls: vec![], assistant_message: json!({"role":"assistant","content":"默认最终"}),
            }))
        }
    }
    #[derive(Default)]
    struct RecEmitter { thinking: Mutex<Vec<String>>, calls: Mutex<Vec<(String,String,String)>> }
    #[async_trait]
    impl Emitter for RecEmitter {
        async fn thinking(&self, t: &str) { self.thinking.lock().unwrap().push(t.into()); }
        async fn tool_call(&self, n: &str, a: &str) { self.calls.lock().unwrap().push((n.into(),a.into(),"call".into())); }
        async fn tool_result(&self, n: &str, r: &str) { self.calls.lock().unwrap().push((n.into(),r.into(),"result".into())); }
    }

    fn tool_round(name:&str, args:&str, id:&str) -> ParsedResp {
        ParsedResp {
            content: String::new(),
            reasoning: "想一下".into(),
            tool_calls: vec![json!({"id":id,"type":"function","function":{"name":name,"arguments":args}})],
            assistant_message: json!({"role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":args}}]}),
        }
    }

    #[tokio::test]
    async fn loop_one_tool_then_final() {
        // 用 bash echo（真实执行）验证 dispatch 也被调用
        let round = FakeRound(Mutex::new(VecDeque::from(vec![
            Ok(tool_round("bash", r#"{"command":"echo hi"}"#, "c1")),
            Ok(ParsedResp { content:"完成".into(), reasoning:String::new(), tool_calls:vec![], assistant_message:json!({"role":"assistant","content":"完成"}) }),
        ])));
        let em = RecEmitter::default();
        let cfg = Config::default();
        let ws = std::env::temp_dir();
        let res = run_loop(round, em, &cfg, vec![json!({"role":"user","content":"跑下"})], &ws).await;
        assert_eq!(res.content, "完成");
        assert!(res.error.is_none());
        // history: assistant(tool_calls) + tool(result) + assistant(final)
        assert_eq!(res.history.len(), 3);
        assert_eq!(res.history[0]["tool_calls"][0]["id"], "c1");
        assert_eq!(res.history[1]["role"], "tool");
        assert!(res.history[1]["content"].as_str().unwrap().contains("hi")); // echo hi 的输出
        assert_eq!(res.history[2]["content"], "完成");
    }

    #[tokio::test]
    async fn loop_mid_fail_keeps_partial_history() {
        let round = FakeRound(Mutex::new(VecDeque::from(vec![
            Ok(tool_round("bash", r#"{"command":"echo a"}"#, "c1")),
            Err("LLM HTTP 500: boom".into()),
        ])));
        let em = RecEmitter::default();
        let cfg = Config::default();
        let ws = std::env::temp_dir();
        let res = run_loop(round, em, &cfg, vec![], &ws).await;
        assert_eq!(res.error.as_deref(), Some("LLM HTTP 500: boom"));
        assert_eq!(res.content, "");
        // partial history 保留：1 个 assistant(tool_calls) + 1 个 tool(result)
        assert_eq!(res.history.len(), 2);
    }

    #[tokio::test]
    async fn loop_cap_at_12() {
        // 每轮都返回工具调用，永不终止 → 12 轮兜底
        let mut steps = VecDeque::new();
        for _ in 0..13 { steps.push_back(Ok(tool_round("bash", r#"{"command":"echo z"}"#, "c"))); }
        let round = FakeRound(Mutex::new(steps));
        let em = RecEmitter::default();
        let cfg = Config::default();
        let ws = std::env::temp_dir();
        let res = run_loop(round, em, &cfg, vec![], &ws).await;
        assert!(res.content.contains("工具调用上限"), "got: {}", res.content);
        // 12 轮 × (assistant + tool) = 24 条，加 1 条兜底 assistant = 25
        assert_eq!(res.history.len(), 25);
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml llm::tests`
Expected: 编译失败（`ChatResponse`/`Emitter`/`run_loop` 未定义）。

- [ ] **Step 3: 实现**（`llm.rs`，`HttpRound` 之后）

```rust
use std::path::Path;

/// chat 命令的统一返回（永不抛 Err：失败也带 partial history + error）。
#[derive(serde::Serialize, Clone, Debug)]
pub struct ChatResponse {
    pub content: String,
    pub history: Vec<Value>,
    pub error: Option<String>,
}

/// 事件出口抽象（真实 AppEmitter 在 lib.rs；测试用录音实现）。
#[async_trait]
pub trait Emitter: Send + Sync {
    async fn thinking(&self, text: &str);
    async fn tool_call(&self, name: &str, args: &str);
    async fn tool_result(&self, name: &str, result: &str);
}

const MAX_ITERS: usize = 12;

/// 工具循环：注入 round + emitter，messages 为前端历史的副本（不回写调用方）。
pub async fn run_loop<R: LlmRound, E: Emitter>(
    round: R,
    emitter: E,
    cfg: &Config,
    mut messages: Vec<Value>,
    workspace: &Path,
) -> ChatResponse {
    let mut history: Vec<Value> = vec![];
    let mut last_content = String::new();

    for _ in 0..MAX_ITERS {
        let resp = match round.round(&messages, cfg).await {
            Ok(r) => r,
            Err(e) => {
                // H4：中途失败也回传已积累的 history
                return ChatResponse { content: String::new(), history, error: Some(e) };
            }
        };
        last_content = resp.content.clone();
        emitter.thinking(&resp.reasoning).await;

        if !resp.tool_calls.is_empty() {
            messages.push(resp.assistant_message.clone());
            history.push(resp.assistant_message);

            for call in resp.tool_calls {
                let name = call["function"]["name"].as_str().unwrap_or("").to_string();
                let args_str = call["function"]["arguments"].as_str().unwrap_or("{}").to_string();
                emitter.tool_call(&name, &args_str).await;

                let result = match serde_json::from_str::<Value>(&args_str) {
                    Err(e) => format!("参数解析失败: {e}"), // M1 坏 JSON 不崩
                    Ok(args) => tools::dispatch(&name, args, workspace).await,
                };
                emitter.tool_result(&name, &result).await;

                let m = json!({ "role": "tool", "tool_call_id": call["id"], "content": result });
                messages.push(m.clone());
                history.push(m);
            }
            continue;
        } else {
            let final_msg = json!({ "role": "assistant", "content": last_content });
            messages.push(final_msg.clone());
            history.push(final_msg);
            return ChatResponse { content: last_content, history, error: None };
        }
    }

    // M4 兜底：达上限未出最终文本。
    let content = if last_content.trim().is_empty() {
        "（已达工具调用上限，未产生最终答复）".to_string()
    } else {
        format!("{last_content}\n\n_（已达工具调用上限）_")
    };
    history.push(json!({ "role": "assistant", "content": &content }));
    ChatResponse { content, history, error: None }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml llm::tests`
Expected: PASS（含三个循环测试；cap 测试约等 12×echo 时间，秒级）。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/llm.rs
git commit -m "feat(llm): run_loop 工具循环 + ChatResponse + Emitter 注入"
```

---

### Task 8: lib.rs —— 装配 chat 命令 + AppEmitter + 注册 tools

**Files:**
- Modify: `src-tauri/src/lib.rs`

**Interfaces:**
- Consumes: `llm::run_loop`/`HttpRound`/`Emitter`/`ChatResponse`（Task 7）、`config::load`（Task 1）。

- [ ] **Step 1: 改 chat 命令 + 加 AppEmitter**

把 lib.rs 顶部的旧 `ChatResponse` 结构体（`struct ChatResponse { content: String }`）**删除**，改用 `llm::ChatResponse`。

替换 `chat` 命令为：
```rust
use async_trait::async_trait;
use tauri::Emitter;

/// AppEmitter：把循环内事件通过 Tauri emit 推给前端。
struct AppEmitter {
    app: AppHandle,
}
#[async_trait]
impl llm::Emitter for AppEmitter {
    async fn thinking(&self, text: &str) {
        let _ = self.app.emit("llm-thinking", text);
    }
    async fn tool_call(&self, name: &str, args: &str) {
        let _ = self.app.emit("llm-tool-call", json!({ "name": name, "args": args }));
    }
    async fn tool_result(&self, name: &str, result: &str) {
        let _ = self.app.emit("llm-tool-result", json!({ "name": name, "result": result }));
    }
}

/// 与 LLM 多轮对话（含工具循环）。返回 ChatResponse（失败也带 partial history）。
#[tauri::command]
async fn chat(messages: Vec<Value>, app: AppHandle) -> llm::ChatResponse {
    let cfg = config::load(&app);
    let workspace = std::path::PathBuf::from(&cfg.workspace_dir);
    let _ = std::fs::create_dir_all(&workspace); // 确保工作目录存在
    llm::run_loop(llm::HttpRound, AppEmitter { app: app.clone() }, &cfg, messages, &workspace).await
}
```
> `Value` 已在 lib.rs `use serde_json::Value;`。`json!` 宏需要 `use serde_json::json;`（若未引入则在文件顶部加）。

- [ ] **Step 2: 确认模块注册**

lib.rs 模块声明区已有 `pub mod llm;`（Task 2 加了 `pub mod tools;`）。无需再改。

- [ ] **Step 3: 编译 + 全量测试**

Run: `cargo build --manifest-path src-tauri/Cargo.toml`
Expected: 编译成功（opener 等已编译过，增量快）。

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: 全部 PASS（config + tools + llm）。

- [ ] **Step 4: 提交**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(lib): chat 命令装配工具循环 + AppEmitter 事件推送"
```

---

### Task 9: 设置页 + main.js —— workspace_dir 配置项

**Files:**
- Modify: `src/index.html`
- Modify: `src/main.js`

**Interfaces:**
- Consumes: `Config.workspace_dir`（Task 1）。

- [ ] **Step 1: index.html 加设置项**

在「对话 (LLM)」group 之后插入新 group（`cfg-system-prompt` 所在 group 之后）：
```html
            <div class="group">
              <div class="group-title">Agent 工作目录</div>
              <label class="row row-col">
                <span>工作目录（write/read/bash 相对路径根）</span>
                <input id="cfg-workspace" type="text" spellcheck="false" placeholder="留空 = 默认 app_data/workspace" />
              </label>
            </div>
```

- [ ] **Step 2: main.js 加字段映射**

`F` 对象加一项：
```js
  workspace: document.getElementById("cfg-workspace"),
```
`fillForm` 末尾加：
```js
  F.workspace.value = cfg.workspace_dir || "";
```
`readForm` 返回对象加：
```js
    workspace_dir: F.workspace.value.trim(),
```

> 注：`load` 返回的是**已解析的绝对路径**，所以首次打开设置页会显示解析后的默认路径（如 `…/ovoice/workspace`）；清空保存则下次 load 回落默认。功能正确，仅展示如此。

- [ ] **Step 3: 提交**

```bash
git add src/index.html src/main.js
git commit -m "feat(ui): 设置页加 Agent 工作目录项"
```

---

### Task 10: main.js —— 事件监听 + 三段气泡 + chat-busy 闸 + history/error 处理

**Files:**
- Modify: `src/main.js`

**Interfaces:**
- Consumes: 事件 `llm-thinking`/`llm-tool-call`/`llm-tool-result`、`ChatResponse{content,history,error}`（Task 8）。

- [ ] **Step 1: 加 chat-busy 闸 + 当前气泡引用**

`main.js` 顶部全局变量区（`let messages = ...` 附近）加：
```js
let chatBusy = false;        // 工具循环进行中：阻止语音重入（M3）
let activeAssistantWrap = null; // 事件路由目标
```

- [ ] **Step 2: 改造提交逻辑（三段气泡 + 事件 + history/error）**

替换 `form.addEventListener("submit", ...)` 的整个回调体为：
```js
form.addEventListener("submit", async (e) => {
  e.preventDefault();
  if (micState !== "idle" || chatBusy) return; // 录音/识别/上一轮 chat 进行中不发送
  const text = input.value.trim();
  if (!text) return;
  input.value = "";
  input.style.height = "auto";
  chatBusy = true;
  sendBtn.disabled = true;

  messages.push({ role: "user", content: text });
  addBubble("user", text);

  // 三段式 assistant 气泡（reasoning / tools / text）
  const aiWrap = document.createElement("div");
  aiWrap.className = "bubble assistant";
  const reasoningEl = document.createElement("details");
  reasoningEl.className = "bubble-reasoning";
  reasoningEl.hidden = true;
  reasoningEl.innerHTML = '<summary>思考过程</summary><div class="reasoning-body"></div>';
  const toolsEl = document.createElement("div");
  toolsEl.className = "bubble-tools";
  const aiText = document.createElement("div");
  aiText.className = "bubble-text";
  aiText.innerHTML = '<span class="typing"><i></i><i></i><i></i></span>';
  aiWrap.append(reasoningEl, toolsEl, aiText);
  list.appendChild(aiWrap);
  scrollBottom();
  activeAssistantWrap = aiWrap;

  try {
    const res = await invoke("chat", { messages });
    // L1：用 history 替换旧的「单条 assistant push」——含工具中间消息保上下文
    messages.push(...res.history);
    if (res.error) {
      aiText.textContent = res.error;
      aiWrap.classList.add("error");
    } else {
      aiText.classList.add("md");
      aiText.innerHTML = renderMarkdown(res.content);
      enhanceMarkdown(aiText);
      attachSpeak(aiWrap);
    }
  } catch (err) {
    aiText.textContent = err;
    aiWrap.classList.add("error");
  } finally {
    activeAssistantWrap = null;
    chatBusy = false;
    sendBtn.disabled = false;
    input.focus();
    scrollBottom();
  }
});
```

- [ ] **Step 3: 注册三个事件监听（init 内，与 setupVoiceEvents 并列）**

在 `loadConfig().then(() => { ... })` 里，`bindLinkOpener(list);` 之后加 `setupAgentEvents();`，并新增函数：
```js
function appendReasoning(text) {
  if (!text || !activeAssistantWrap) return;
  const body = activeAssistantWrap.querySelector(".reasoning-body");
  if (!body) return;
  const r = activeAssistantWrap.querySelector(".bubble-reasoning");
  r.hidden = false;
  body.textContent += text;
}
function appendToolCard(name, args) {
  if (!activeAssistantWrap) return;
  const tools = activeAssistantWrap.querySelector(".bubble-tools");
  const card = document.createElement("div");
  card.className = "tool-card";
  card.dataset.name = name;
  card.innerHTML = `<div class="tool-head">🔧 ${name}</div><pre class="tool-args"></pre><pre class="tool-result"></pre>`;
  card.querySelector(".tool-args").textContent = args;
  tools.appendChild(card);
  scrollBottom();
}
function fillToolResult(name, result) {
  if (!activeAssistantWrap) return;
  const cards = activeAssistantWrap.querySelectorAll(`.tool-card[data-name="${name}"]`);
  const card = cards[cards.length - 1]; // 多次同名取最后一个
  if (!card) return;
  card.querySelector(".tool-result").textContent = result;
  scrollBottom();
}

async function setupAgentEvents() {
  await listen("llm-thinking", (e) => appendReasoning(typeof e.payload === "string" ? e.payload : (e.payload && e.payload.text) || ""));
  await listen("llm-tool-call", (e) => {
    const p = e.payload || {};
    appendToolCard(p.name || "?", p.args || "");
  });
  await listen("llm-tool-result", (e) => {
    const p = e.payload || {};
    fillToolResult(p.name || "?", p.result || "");
  });
}
```

- [ ] **Step 4: 让语音也认 chat-busy 闸（M3）**

`voice-result` 监听里，自动发送前加守卫。找到 `form.requestSubmit(); // 自动发送` 那行，改为：
```js
    if (chatBusy) return; // chat 进行中，丢弃本次语音结果避免重入
    form.requestSubmit(); // 自动发送
```

- [ ] **Step 5: 提交**

```bash
git add src/main.js
git commit -m "feat(ui): agent 事件监听 + 三段气泡 + chat-busy 防重入"
```

---

### Task 11: styles.css —— 思考区 + 工具卡

**Files:**
- Modify: `src/styles.css`

- [ ] **Step 1: 追加样式**（文件末尾）

```css
/* ===== Agent：思考区 + 工具卡（仅 assistant 气泡内）===== */
.bubble.assistant .bubble-reasoning {
  margin: 0 0 6px;
  font-size: 13px;
  color: var(--text-secondary);
  background: rgba(0, 0, 0, 0.04);
  border-radius: 8px;
  padding: 4px 8px;
}
.bubble.assistant .bubble-reasoning summary {
  cursor: pointer;
  user-select: none;
  font-size: 12px;
  color: var(--text-tertiary);
}
.bubble.assistant .bubble-reasoning .reasoning-body {
  margin-top: 4px;
  white-space: pre-wrap;
  word-break: break-word;
  line-height: 1.45;
}

.bubble.assistant .bubble-tools {
  display: flex;
  flex-direction: column;
  gap: 6px;
  margin: 0 0 6px;
}
.bubble.assistant .tool-card {
  background: #1d1d1f;
  color: #e6e6eb;
  border-radius: 8px;
  padding: 6px 9px;
  font-size: 12.5px;
}
.bubble.assistant .tool-card .tool-head {
  font-weight: 600;
  font-size: 12px;
  opacity: 0.85;
  margin-bottom: 3px;
}
.bubble.assistant .tool-card pre {
  margin: 0;
  white-space: pre-wrap;
  word-break: break-word;
  font-family: "SF Mono", "JetBrains Mono", Consolas, monospace;
  line-height: 1.4;
}
.bubble.assistant .tool-card .tool-args {
  color: #b8b8c0;
}
.bubble.assistant .tool-card .tool-result {
  margin-top: 4px;
  padding-top: 4px;
  border-top: 1px solid rgba(255, 255, 255, 0.12);
  color: #d6d6de;
  max-height: 240px;
  overflow-y: auto;
}

/* 三段气泡里 .bubble-text 不再继承 pre-wrap（markdown 自管换行） */
.bubble.assistant .bubble-text:not(.md) { white-space: pre-wrap; }
```

- [ ] **Step 2: 提交**

```bash
git add src/styles.css
git commit -m "style(ui): 思考区 + 工具卡样式"
```

---

### Task 12: 冒烟验证（端到端）

**Files:** 无（仅运行验证）

- [ ] **Step 1: 全量编译 + 测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: 全部 PASS。

Run: `cargo build --manifest-path src-tauri/Cargo.toml`
Expected: 编译成功。

- [ ] **Step 2: 启动应用**

Run（PowerShell，由用户执行）: `pnpm tauri dev`
Expected: 窗口启动，顶栏显示「MiniMax-M3 · …」，无 panic。

- [ ] **Step 3: 验证三工具 + 思考展示**

依次发消息：
1. 「读一下你自己的设置说明，简短总结」→ 期望看到思考区（可展开）、一张 read 工具卡（含结果）、最终 markdown 答复。
2. 「在 workspace 里写一个 hello.txt，内容是你好」→ 期望 write 工具卡，最终答复确认。
3. 「跑一下 `dir`」→ 期望 bash 工具卡，输出含目录列表（中文不乱码验证 H1）。
4. 「读 a.txt 再把它的内容写进 b.txt」→ **两轮工具调用**，验证 H5 回传形状与多轮历史（不报 400）。

- [ ] **Step 4: 验证护栏**

5. 让模型循环调用（如「反复读再写 20 次」）→ 期望达 12 轮后出现「（已达工具调用上限）」提示，气泡不为空（M4）。

- [ ] **Step 5: 验证 speak 卫生**

点任一 agent 答复的「朗读」→ 期望只朗读 `.bubble-text` 最终答案，不读思考/工具卡。

- [ ] **Step 6: 最终提交（如有验证中发现的小修）**

```bash
git add -A
git commit -m "chore: agent 冒烟验证通过"
```

---

## Self-Review（写完计划后自查）

**1. Spec 覆盖：**
- 工具循环归属 → Task 7 run_loop ✓
- reasoning_split 思考分离 → Task 6 parse_resp + Task 10 appendReasoning ✓
- write/read/bash → Task 3/4/5 ✓
- 可配置工作目录 + 默认 → Task 1 + Task 9 ✓
- 全自动执行 → 无确认 UI（计划全程无确认步骤）✓
- 护栏（12 轮/30s/50KB/20KB）→ Task 7 MAX_ITERS、Task 4 timeout、Task 3 READ_MAX、Task 4 BASH_MAX ✓
- H1 编码 → Task 4 chcp 65001 + decode_output ✓
- H2 workspace 运行时默认 → Task 1 resolve_workspace ✓
- H3 进程树清理 → Task 4 win_job Job Object ✓
- H4 失败保历史 → Task 7 run_loop Err 分支 + loop_mid_fail_keeps_partial_history ✓
- H5 回传形状 → Task 6 parse_resp content=null + Task 12 两轮冒烟 ✓
- M1 坏 JSON → Task 7 from_str Err 分支 ✓
- M2 循环单测 → Task 7 三个测试 + FakeRound ✓
- M3 chat-busy 闸 → Task 10 chatBusy + voice-result 守卫 ✓
- M4 空内容兜底 → Task 7 cap 分支 ✓
- M5 async dispatch → Task 4/5/7 均 async/await ✓
- L1 push history → Task 10 `messages.push(...res.history)` ✓
- L2 去 thinking:adaptive → Task 6 build_body（无 thinking）✓
- L3 description 充分 → Task 5 schemas ✓
- speak 卫生 → Task 10 三段气泡 + Task 12 step5 ✓

**2. 占位符扫描：** 无 TBD/TODO；每步含真实代码与命令。

**3. 类型一致性：** `ParsedResp`/`ChatResponse`/`Emitter`/`LlmRound`/`dispatch`/`schemas` 跨任务签名一致；`resolve_workspace`/`resolve_path` 名字统一。
