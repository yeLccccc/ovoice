# 聊天文档渲染与编辑 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 agent 能在对话气泡里展示 HTML/PDF/Word(.docx)/CSV/Markdown 文档，并编辑 markdown/text 文件（气泡内可编辑卡 + 存盘）。

**Architecture:** 加法式——新增两个 agent 工具（`display_doc` / `edit_file`）+ 一条前端命令（`write_file`），**不动**已工作且有测试的 media 代码。文档卡与媒体卡共用 `.bubble-media` 容器与 `.media-card` 玻璃样式；前端按 `kind` 分发渲染（html/pdf 沙箱 iframe、docx 用 docx-preview、csv 用 PapaParse、md 用现有 markdown-it、编辑卡 textarea+实时预览）。安全：所有脚本承载面走 `sandbox="allow-scripts"`（无 allow-same-origin）；写盘 scope 到 workspace+app_data。

**Tech Stack:** Tauri v2 + Rust（后端纯逻辑可离线测）；前端无打包器，vendor UMD（docx-preview / JSZip / PapaParse）；WebView2 内置 PDF 阅读器；现有 markdown-it + DOMPurify。

## Global Constraints

- **无打包器、无 npm。** 新前端库必须以单文件 UMD 进 `/vendor/`，经 `<script src="/vendor/*.min.js">` 暴露全局（`window.docx` / `window.JSZip` / `window.Papa`）。**禁止**引入 ESM-only 库（如 CodeMirror 6）。
- **`withGlobalTauri:true` + CSP=null。** `window.__TAURI__.core.invoke()` 暴露在主页面 → 任何不可信 HTML 必须走 `<iframe sandbox="allow-scripts">`（**绝不加** `allow-same-origin`；**绝不**用 Shadow DOM 当安全边界）。
- **路径 scope。** `write_file` 写入目标必须落在 workspace 或 app_data 下（`is_within_roots`，兼容新建文件的父目录 canonicalize）；防 `../` 越权。
- **全屏不搬节点。** 文档卡全屏用 `cloneNode` 复制 `.doc-body` 进覆层（搬 iframe 会重载、搬 docx 容器会脱挂）。
- **复用 `.media-card` 玻璃 token。** 文档/编辑卡复用 `.media-card` + `--glass-alpha` + `--text-secondary`，不另起配色。
- **限高。** 文档卡内容区高度一律 `min(<N>px, 62~70vh)`，500px 最小窗不溢出；文档卡**不**默认折叠（与媒体卡一致）。
- 后端测试用 `cargo test --manifest-path src-tauri/Cargo.toml --lib`；前端用 `node --check src/main.js`（项目无 JS 测试框架）。
- 分支 `feat/chat-documents`（off master @ 2a8fdf9）。

参考规格：`docs/superpowers/specs/2026-07-24-chat-documents-design.md`（已过 plan-design-review 9/10 CLEAR）。

---

## File Structure

- **`src-tauri/src/lib.rs`** — 新增 `DocKind` 枚举 + `doc_kind_from_ext`；新增 `write_file` 命令；`invoke_handler` 注册 `write_file`。`media://` 协议、`is_within_roots`、`mime_from_ext` 不动。
- **`src-tauri/src/tools.rs`** — 新增 `tool_display_doc` / `tool_edit_file` / `write_file_scoped`（纯逻辑，可测）；`dispatch` 加 2 条路由；`schemas()` 加 2 工具。`tool_display_media` / `tool_write` / `tool_read` 不动。
- **`src/index.html`** — `<head>` 加 3 条 vendor `<script>`。
- **`src/main.js`** — 新增 `renderDocCard` / `renderEditCard` / `renderDocx` / `renderCsv` / `fetchText` / `openDocFullscreen` / `toast` / `confirmDiscardDirtyEdits`；扩展 `llm-tool-call` / `llm-tool-result` 处理器与 reset 守卫。`renderMediaCard` / `openLightbox` 不动。
- **`src/styles.css`** — 复用 `.media-card`；新增 `.doc-body` / `.doc-iframe` / `.csv-table` / `.md-body` / `.edit-card` / `.doc-lightbox` / `.toast` + 响应式。
- **`docs/superpowers/smoke/2026-07-24-chat-documents.md`** — 真机 smoke 清单。

---

### Task 1: DocKind 分类 + 扩展 mime_from_ext（lib.rs）

**Files:**
- Modify: `src-tauri/src/lib.rs`（在 `MediaKind` 定义之后新增 `DocKind` + `doc_kind_from_ext`；在 `mime_from_ext` 的 match 末尾 `_` 之前补 html/pdf/csv/docx/md/txt 分支；在 `media_tests` 模块加测试）

**Interfaces:**
- Produces: `pub(crate) enum DocKind { Html, Pdf, Docx, Csv, Markdown, Text, Unsupported }`；`pub(crate) fn doc_kind_from_ext(path: &std::path::Path) -> DocKind`。T2/T3 消费 `crate::doc_kind_from_ext` / `crate::DocKind`。
- **为何顺带改 `mime_from_ext`：** `media://` 协议处理器（lib.rs 内）用 `mime_from_ext` 设响应 `Content-Type`；当前 html/pdf 落到 `application/octet-stream`，会让 **PDF 内置阅读器不激活**（Chromium 仅对 `application/pdf` 弹阅读器）且 **HTML iframe 不渲染**。本任务一并补齐（纯加分支，不动既有 image/video/audio 映射）。

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/lib.rs` 的 `#[cfg(test)] mod media_tests { ... }` 内追加：

```rust
    #[test]
    fn doc_kind_covers_each_ext() {
        use DocKind::*;
        assert!(matches!(doc_kind_from_ext(Path::new("a.html")), Html));
        assert!(matches!(doc_kind_from_ext(Path::new("a.HTM")), Html));
        assert!(matches!(doc_kind_from_ext(Path::new("a.pdf")), Pdf));
        assert!(matches!(doc_kind_from_ext(Path::new("a.docx")), Docx));
        assert!(matches!(doc_kind_from_ext(Path::new("a.csv")), Csv));
        assert!(matches!(doc_kind_from_ext(Path::new("a.md")), Markdown));
        assert!(matches!(doc_kind_from_ext(Path::new("a.markdown")), Markdown));
        assert!(matches!(doc_kind_from_ext(Path::new("a.txt")), Text));
        assert!(matches!(doc_kind_from_ext(Path::new("a.log")), Text));
    }
    #[test]
    fn doc_kind_doc_and_unknown_unsupported() {
        // .doc 旧二进制明确不支持（客户端无库）
        assert!(matches!(doc_kind_from_ext(Path::new("old.doc")), DocKind::Unsupported));
        assert!(matches!(doc_kind_from_ext(Path::new("x")), DocKind::Unsupported));
        assert!(matches!(doc_kind_from_ext(Path::new("a.exe")), DocKind::Unsupported));
    }
    #[test]
    fn mime_covers_doc_kinds() {
        // display_doc 的 html/pdf iframe 依赖正确 Content-Type（media:// 用 mime_from_ext）
        assert_eq!(mime_from_ext(Path::new("a.html")), "text/html");
        assert_eq!(mime_from_ext(Path::new("A.HTM")), "text/html");
        assert_eq!(mime_from_ext(Path::new("a.pdf")), "application/pdf");
        assert_eq!(mime_from_ext(Path::new("a.csv")), "text/csv");
        assert_eq!(mime_from_ext(Path::new("a.docx")), "application/vnd.openxmlformats-officedocument.wordprocessingml.document");
        assert_eq!(mime_from_ext(Path::new("a.md")), "text/markdown");
        assert_eq!(mime_from_ext(Path::new("a.txt")), "text/plain");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib doc`
Expected: 编译失败（`DocKind` / `doc_kind_from_ext` 未定义；`mime_covers_doc_kinds` 里 pdf 仍返回 octet-stream）。

- [ ] **Step 3: 实现 DocKind + doc_kind_from_ext**

在 `src-tauri/src/lib.rs` 现有 `pub(crate) fn media_kind_from_ext` 函数之后插入：

```rust
/// 文档类型（display_doc/edit_file 用；与 MediaKind 分离，加法式不动 media 代码）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum DocKind { Html, Pdf, Docx, Csv, Markdown, Text, Unsupported }

/// 按扩展名判文档类型。.doc 旧二进制 → Unsupported（客户端不支持）。
pub(crate) fn doc_kind_from_ext(path: &std::path::Path) -> DocKind {
    use DocKind::*;
    match path.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("html") | Some("htm") => Html,
        Some("pdf") => Pdf,
        Some("docx") => Docx,
        Some("csv") => Csv,
        Some("md") | Some("markdown") => Markdown,
        Some("txt") | Some("text") | Some("log") => Text,
        _ => Unsupported,
    }
}
```

然后在 `src-tauri/src/lib.rs` 的 `mime_from_ext` 函数里，把现有 `_ => "application/octet-stream"` 这一行**之前**插入文档类 MIME 分支（纯加分支，不动既有 image/video/audio 映射）：

```rust
        Some("html") | Some("htm") => "text/html",
        Some("pdf") => "application/pdf",
        Some("csv") => "text/csv",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("md") | Some("markdown") => "text/markdown",
        Some("txt") | Some("text") | Some("log") => "text/plain",

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib doc`
Expected: PASS（3 tests：2× doc_kind + 1× mime_covers_doc_kinds）。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(doc): DocKind 分类 + doc_kind_from_ext + mime_from_ext 补文档类型"
```

---

### Task 2: tool_display_doc 工具（tools.rs）

**Files:**
- Modify: `src-tauri/src/tools.rs`（在 `tool_display_media` 之后新增 `tool_display_doc`；测试模块加测试）

**Interfaces:**
- Consumes: `crate::doc_kind_from_ext` / `crate::DocKind`（T1）；`resolve_path`（同文件）。
- Produces: `pub fn tool_display_doc(args: &Value, workspace: &Path) -> String`，返回 JSON 字符串：内联片段 `{"display":true,"html":<str>,"kind":"html","caption"}`；路径 `{"display":true,"path":abs,"kind":<html|pdf|docx|csv|markdown>,"caption"}`；失败 `{"display":false,"error":...}`。T6 前端解析此契约。

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/tools.rs` 的 `#[cfg(test)] mod tests` 内追加：

```rust
    #[test]
    fn display_doc_missing_args_errors() {
        let dir = tempfile::tempdir().unwrap();
        let r = tool_display_doc(&serde_json::json!({}), dir.path());
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["display"], false);
        assert!(v["error"].as_str().unwrap().contains("path"));
    }
    #[test]
    fn display_doc_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let r = tool_display_doc(&serde_json::json!({"path":"nope.pdf"}), dir.path());
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["display"], false);
        assert!(v["error"].as_str().unwrap().contains("文件不存在"));
    }
    #[test]
    fn display_doc_doc_unsupported_with_hint() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("old.doc"), b"x").unwrap();
        let r = tool_display_doc(&serde_json::json!({"path":"old.doc"}), dir.path());
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["display"], false);
        assert!(v["error"].as_str().unwrap().contains(".doc"));
    }
    #[test]
    fn display_doc_path_kinds() {
        let dir = tempfile::tempdir().unwrap();
        for (fname, kind) in [
            ("a.html", "html"), ("b.pdf", "pdf"), ("c.docx", "docx"),
            ("d.csv", "csv"), ("e.md", "markdown"),
        ] {
            std::fs::write(dir.path().join(fname), b"x").unwrap();
            let r = tool_display_doc(&serde_json::json!({"path":fname,"caption":"cap"}), dir.path());
            let v: serde_json::Value = serde_json::from_str(&r).unwrap();
            assert_eq!(v["display"], true, "{fname}");
            assert_eq!(v["kind"], kind, "{fname}");
            assert!(v["path"].as_str().unwrap().ends_with(fname), "{fname}");
            assert_eq!(v["caption"], "cap", "{fname}");
        }
    }
    #[test]
    fn display_doc_inline_html_fragment() {
        let dir = tempfile::tempdir().unwrap();
        let r = tool_display_doc(&serde_json::json!({"html":"<b>hi</b>"}), dir.path());
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["display"], true);
        assert_eq!(v["kind"], "html");
        assert_eq!(v["html"], "<b>hi</b>");
        assert!(v.get("path").is_none(), "内联片段不应带 path");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib display_doc`
Expected: 编译失败（`tool_display_doc` 未定义）。

- [ ] **Step 3: 实现 tool_display_doc**

在 `src-tauri/src/tools.rs` 的 `tool_display_media` 函数之后插入：

```rust
/// 展示本地文档（html/pdf/docx/csv/markdown）：内联 html 片段优先；否则按 path 分类。
/// 返回结构化 JSON 供前端渲染文档卡（契约同 tool_display_media 的 display/error 模式）。
pub fn tool_display_doc(args: &Value, workspace: &Path) -> String {
    // 内联 html 片段（不落盘）：图表/小工具
    if let Some(html) = args.get("html").and_then(|v| v.as_str()) {
        let caption = args.get("caption").and_then(|v| v.as_str()).unwrap_or("");
        return serde_json::json!({ "display": true, "html": html, "kind": "html", "caption": caption }).to_string();
    }
    let p = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return serde_json::json!({ "display": false, "error": "缺少 path 或 html 参数" }).to_string(),
    };
    let path = resolve_path(p, workspace);
    if !path.exists() {
        return serde_json::json!({ "display": false, "error": format!("文件不存在: {}", path.display()) }).to_string();
    }
    let kind = crate::doc_kind_from_ext(&path);
    let kind_str = match kind {
        crate::DocKind::Html => "html",
        crate::DocKind::Pdf => "pdf",
        crate::DocKind::Docx => "docx",
        crate::DocKind::Csv => "csv",
        crate::DocKind::Markdown => "markdown",
        crate::DocKind::Text | crate::DocKind::Unsupported => {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let hint = if matches!(kind, crate::DocKind::Unsupported) && ext == "doc" {
                "（.doc 旧格式不支持，请在系统查看器打开）"
            } else if matches!(kind, crate::DocKind::Text) {
                "（可用 edit_file 编辑）"
            } else { "" };
            return serde_json::json!({ "display": false, "error": format!("不支持的文档类型: .{ext}{hint}") }).to_string();
        }
    };
    let abs = path.canonicalize().unwrap_or(path).to_string_lossy().to_string();
    let caption = args.get("caption").and_then(|v| v.as_str()).unwrap_or("");
    serde_json::json!({ "display": true, "path": abs, "kind": kind_str, "caption": caption }).to_string()
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib display_doc`
Expected: PASS（5 tests）。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/tools.rs
git commit -m "feat(doc): tool_display_doc（html 片段 + path→html/pdf/docx/csv/markdown）"
```

---

### Task 3: tool_edit_file 工具（tools.rs）

**Files:**
- Modify: `src-tauri/src/tools.rs`（在 `tool_display_doc` 之后新增 `tool_edit_file`；测试模块加测试）

**Interfaces:**
- Consumes: `crate::doc_kind_from_ext` / `crate::DocKind`（T1）；`resolve_path`（同文件）。
- Produces: `pub fn tool_edit_file(args: &Value, workspace: &Path) -> String`，返回 `{"edit":true,"path":abs,"kind":<markdown|text>,"content":<str>,"caption"?}` 或 `{"edit":false,"error":...}`。T7 前端解析此契约。

- [ ] **Step 1: 写失败测试**

在 tools.rs 测试模块追加：

```rust
    #[test]
    fn edit_file_missing_path_errors() {
        let r = tool_edit_file(&serde_json::json!({}), Path::new("."));
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["edit"], false);
        assert!(v["error"].as_str().unwrap().contains("path"));
    }
    #[test]
    fn edit_file_non_editable_kind_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.pdf"), b"%PDF").unwrap();
        let r = tool_edit_file(&serde_json::json!({"path":"a.pdf"}), dir.path());
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["edit"], false);
        assert!(v["error"].as_str().unwrap().contains("markdown/text"));
    }
    #[test]
    fn edit_file_reads_disk_when_no_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("n.md"), "# 标题\n正文").unwrap();
        let r = tool_edit_file(&serde_json::json!({"path":"n.md"}), dir.path());
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["edit"], true);
        assert_eq!(v["kind"], "markdown");
        assert_eq!(v["content"], "# 标题\n正文");
    }
    #[test]
    fn edit_file_pregills_given_content_for_new_file() {
        let dir = tempfile::tempdir().unwrap();
        // 文件不存在但给了 content（新建场景）
        let r = tool_edit_file(&serde_json::json!({"path":"new.md","content":"初始"}), dir.path());
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["edit"], true);
        assert_eq!(v["content"], "初始");
    }
    #[test]
    fn edit_file_text_kind() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "plain").unwrap();
        let r = tool_edit_file(&serde_json::json!({"path":"a.txt"}), dir.path());
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["edit"], true);
        assert_eq!(v["kind"], "text");
        assert_eq!(v["content"], "plain");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib edit_file`
Expected: 编译失败（`tool_edit_file` 未定义）。

- [ ] **Step 3: 实现 tool_edit_file**

在 tools.rs 的 `tool_display_doc` 之后插入：

```rust
/// 为用户打开可编辑卡片编辑 markdown/text：content 给定则预填（可新建），缺省读磁盘。
/// 返回 {"edit":true,"path","kind","content","caption"?} 或 {"edit":false,"error"}。
pub fn tool_edit_file(args: &Value, workspace: &Path) -> String {
    let p = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return serde_json::json!({ "edit": false, "error": "缺少 path 参数" }).to_string(),
    };
    let path = resolve_path(p, workspace);
    let kind_str = match crate::doc_kind_from_ext(&path) {
        crate::DocKind::Markdown => "markdown",
        crate::DocKind::Text => "text",
        _ => return serde_json::json!({ "edit": false, "error": format!("仅支持编辑 markdown/text: {}", path.display()) }).to_string(),
    };
    let content = match args.get("content").and_then(|v| v.as_str()) {
        Some(c) => c.to_string(),
        None => match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => return serde_json::json!({ "edit": false, "error": format!("读取失败（文件不存在或非 UTF-8）: {}", path.display()) }).to_string(),
        },
    };
    let abs = path.canonicalize().unwrap_or(path).to_string_lossy().to_string();
    let caption = args.get("caption").and_then(|v| v.as_str()).unwrap_or("");
    serde_json::json!({ "edit": true, "path": abs, "kind": kind_str, "content": content, "caption": caption }).to_string()
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib edit_file`
Expected: PASS（5 tests）。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/tools.rs
git commit -m "feat(doc): tool_edit_file（md/txt 可编辑卡，content 预填或读盘）"
```

---

### Task 4: dispatch 路由 + schemas（tools.rs）

**Files:**
- Modify: `src-tauri/src/tools.rs`（`dispatch` 加 2 路由；`schemas()` 加 2 工具；更新 `schemas_has_four_tools` 测试）

**Interfaces:**
- Consumes: `tool_display_doc` / `tool_edit_file`（T2/T3）。
- Produces: `dispatch("display_doc"|"edit_file", ...)` 可用；`schemas()` 返回 6 工具（write/read/bash/display_media/display_doc/edit_file）。llm.rs 的 `tools: tools::schemas()` 自动带上新工具。

- [ ] **Step 1: 更新计数测试（先改测试再实现，TDD）**

在 tools.rs 找到现有测试：

```rust
    #[test]
    fn schemas_has_four_tools() {
        let s = schemas();
        let names: Vec<&str> = s.iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["write", "read", "bash", "display_media"]);
    }
```

替换为：

```rust
    #[test]
    fn schemas_has_six_tools() {
        let s = schemas();
        let names: Vec<&str> = s.iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["write", "read", "bash", "display_media", "display_doc", "edit_file"]);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib schemas_has_six`
Expected: FAIL（`schemas()` 仍返 4 工具，断言 6 失败）。

- [ ] **Step 3a: 在 `dispatch` 加路由**

在 tools.rs 的 `pub async fn dispatch` 内，`"display_media" => ...` 行之后加：

```rust
        "display_doc" => tool_display_doc(&args, &ctx.workspace),
        "edit_file" => tool_edit_file(&args, &ctx.workspace),
```

- [ ] **Step 3b: 在 `schemas()` 加 2 工具**

在 tools.rs 的 `pub fn schemas()` 内，`display_media` 那条 `serde_json::json!({...})` 之后、`vec!` 结束之前追加：

```rust
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"display_doc",
                "description":"在对话中展示本地文档：HTML/PDF/Word(.docx)/CSV/Markdown。path 指文件；或用 html 给内联 HTML 片段（图表/小工具，不落盘）。.doc 旧格式不支持。",
                "parameters":{
                    "type":"object",
                    "properties":{
                        "path":{"type":"string","description":"文档文件路径，相对工作目录或绝对路径"},
                        "html":{"type":"string","description":"内联 HTML 片段（与 path 二选一）"},
                        "caption":{"type":"string","description":"可选；卡片下方说明文字"}
                    }
                }
            }
        }),
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"edit_file",
                "description":"为用户打开可编辑卡片编辑 markdown/text 文件（左编辑右预览，用户点保存存盘）。content 缺省从磁盘读；给了则预填（可新建文件）。仅 .md/.txt。",
                "parameters":{
                    "type":"object",
                    "properties":{
                        "path":{"type":"string","description":"要编辑的文件路径，相对工作目录或绝对路径"},
                        "content":{"type":"string","description":"可选；预填内容（缺省读磁盘）"},
                        "caption":{"type":"string","description":"可选；说明文字"}
                    },
                    "required":["path"]
                }
            }
        }),
```

- [ ] **Step 4: 跑测试确认通过 + 全量**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib`
Expected: PASS（含 `schemas_has_six_tools` + `dispatch_routes_write` 等既有测试）。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/tools.rs
git commit -m "feat(doc): dispatch 路由 + schemas 加 display_doc/edit_file（4→6 工具）"
```

---

### Task 5: write_file 命令（tools.rs 纯逻辑 + lib.rs 命令）

**Files:**
- Modify: `src-tauri/src/tools.rs`（新增纯逻辑 `write_file_scoped` + 测试）
- Modify: `src-tauri/src/lib.rs`（新增 `#[tauri::command] write_file` + `invoke_handler` 注册）

**Interfaces:**
- Consumes: `crate::is_within_roots`（lib.rs，pub(crate)）；`resolve_path`（tools.rs，pub）。
- Produces: `pub fn write_file_scoped(target: &Path, content: &str, roots: &[PathBuf]) -> Result<String, String>`（可测）；Tauri 命令 `write_file(path, content, app)`。T7 前端 `invoke("write_file", {path, content})`。

- [ ] **Step 1: 写失败测试（纯逻辑）**

在 tools.rs 测试模块追加：

```rust
    #[test]
    fn write_file_scoped_inside_workspace_ok() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        let target = ws.join("sub/a.md");
        let r = write_file_scoped(&target, "# hi\n你好", &[ws.clone()]);
        assert!(r.is_ok(), "{:?}", r);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "# hi\n你好");
    }
    #[test]
    fn write_file_scoped_creates_new_file_and_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        let target = ws.join("nested/deep/x.txt"); // 不存在
        let r = write_file_scoped(&target, "new", &[ws.clone()]);
        assert!(r.is_ok(), "{:?}", r);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    }
    #[test]
    fn write_file_scoped_outside_workspace_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        let target = outside.path().join("evil.txt");
        let r = write_file_scoped(&target, "x", &[ws]);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("不在允许范围"));
        assert!(!target.exists(), "越权写不应落盘");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib write_file_scoped`
Expected: 编译失败（`write_file_scoped` 未定义）。

- [ ] **Step 3: 实现 write_file_scoped（tools.rs）**

在 tools.rs 的 `tool_write` 函数之后插入：

```rust
/// 用户编辑存盘：scope 校验（兼容新建文件——用最近存在的祖先 canonicalize）后写盘。
/// scope 不在 roots 内 → Err；自动建父目录。纯逻辑，可离线测。
pub fn write_file_scoped(target: &Path, content: &str, roots: &[std::path::PathBuf]) -> Result<String, String> {
    // canonicalize 目标；不存在则用父目录（必存在）的 canonicalize + 文件名拼接
    let canon_existing = target.canonicalize().ok();
    let anchor = canon_existing.clone().unwrap_or_else(|| {
        target.parent().and_then(|par| par.canonicalize().ok()).unwrap_or_default()
    });
    if !crate::is_within_roots(&anchor, roots) {
        return Err("路径不在允许范围内".into());
    }
    let dest = canon_existing.unwrap_or_else(|| anchor.join(target.file_name().unwrap_or_default()));
    if let Some(parent) = dest.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Err(format!("创建目录失败: {e}"));
        }
    }
    std::fs::write(&dest, content.as_bytes()).map_err(|e| format!("写入失败: {e}"))?;
    Ok(format!("已写入 {}（{} 字节）", dest.display(), content.len()))
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib write_file_scoped`
Expected: PASS（3 tests）。

- [ ] **Step 5: 加 write_file 命令 + 注册（lib.rs）**

在 `src-tauri/src/lib.rs` 的 `save_config` 命令之后插入：

```rust
/// 用户编辑存盘（前端→磁盘）：scope 到 workspace+app_data；复用 write_file_scoped 纯逻辑。
#[tauri::command]
fn write_file(path: String, content: String, app: AppHandle) -> Result<String, String> {
    let cfg = config::load(&app);
    let ws = std::path::PathBuf::from(&cfg.workspace_dir);
    let appdata = app.path().app_data_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let target = tools::resolve_path(&path, &ws);
    tools::write_file_scoped(&target, &content, &[ws, appdata])
}
```

然后在 `.invoke_handler(tauri::generate_handler![ ... ])` 的列表里加 `write_file`（紧跟 `save_config` 之后）。

- [ ] **Step 6: 全量构建确认无 warning**

Run: `cargo build --manifest-path src-tauri/Cargo.toml 2>&1 | tail -5`
Expected: `Finished`，零 warning。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/tools.rs src-tauri/src/lib.rs
git commit -m "feat(doc): write_file 命令 + write_file_scoped（scoped workspace+app_data，兼容新建）"
```

---

### Task 6: vendor 引入 + renderDocCard + 处理器接线（前端）

**Files:**
- Create: `src/vendor/docx-preview.min.js`、`src/vendor/jszip.min.js`、`src/vendor/papaparse.min.js`（手动下载 UMD 产物）
- Modify: `src/index.html`（`<head>` 加 3 条 `<script>`）
- Modify: `src/main.js`（新增渲染函数 + 扩展 `llm-tool-call`/`llm-tool-result` 处理器）

**Interfaces:**
- Consumes: T2 契约（`{display,path|html,kind,caption}`）；现有 `convertFileSrc` / `invoke` / `listen` / `renderMarkdown` / `.bubble-media` 容器 / `.media-card` 样式 / `openLightbox` 模式。
- Produces: `renderDocCard(wrap, j)` 把文档卡 append 到 `.bubble-media`；`llm-tool-call`/`llm-tool-result` 识别 `display_doc`。

- [ ] **Step 1: 放 vendor UMD 文件**

下载以下 UMD 产物到 `src/vendor/`（与现有 markdown-it/purify/highlight 同目录）：
- `jszip.min.js`（https://github.com/Stuk/jszip/dist/jszip.min.js）
- `docx-preview.min.js`（https://unpkg.com/docx-preview/dist/docx-preview.min.js，依赖 JSZip，需先加载）
- `papaparse.min.js`（https://unpkg.com/papaparse/papaparse.min.js）

加载后暴露全局：`window.JSZip` / `window.docx`（`docx.renderAsync`）/ `window.Papa`。

- [ ] **Step 2: index.html 引入**

在 `src/index.html` 的 `<head>` 内、现有 `highlight.min.js` 那行之后（`<script type="module" src="/main.js" defer>` 之前）加：

```html
    <script src="/vendor/jszip.min.js"></script>
    <script src="/vendor/docx-preview.min.js"></script>
    <script src="/vendor/papaparse.min.js"></script>
```

- [ ] **Step 3: 在 main.js 加渲染辅助函数**

在 `src/main.js` 的 `renderMediaCard` 函数之后插入以下函数：

```js
// ===== 文档卡（display_doc）：html/pdf 沙箱 iframe / docx / csv / markdown =====
function labelFor(kind) {
  return ({ html: "HTML", pdf: "PDF", docx: "Word", csv: "CSV", markdown: "Markdown" })[kind] || "文档";
}
async function fetchText(path) {
  const r = await fetch(convertFileSrc(path, "media"));
  return await r.text();
}
function showDocError(body, msg) {
  body.innerHTML = "";
  const e = document.createElement("div");
  e.className = "media-error"; e.textContent = msg;
  body.appendChild(e);
}
// docx：fetch arrayBuffer → docx-preview 渲染进容器
async function renderDocx(body, path) {
  const r = await fetch(convertFileSrc(path, "media"));
  const buf = await r.arrayBuffer();
  const wrap = document.createElement("div");
  wrap.className = "docx-container";
  body.appendChild(wrap);
  await window.docx.renderAsync(buf, wrap, null, { className: "docx-body" });
}
// csv：fetch 文本 → PapaParse → <table>（首 1000 行 + 显示更多）
async function renderCsv(body, path) {
  const text = await fetchText(path);
  const parsed = window.Papa.parse(text, { skipEmptyLines: true });
  const rows = parsed.data || [];
  if (!rows.length) { showDocError(body, "CSV 为空"); return; }
  const region = document.createElement("div");
  region.className = "csv-wrap"; region.setAttribute("role", "region"); region.setAttribute("aria-label", "CSV 表格");
  const table = document.createElement("table"); table.className = "csv-table";
  const headRow = rows[0];
  const thead = document.createElement("thead"); const tr = document.createElement("tr");
  headRow.forEach((c) => { const th = document.createElement("th"); th.scope = "col"; th.textContent = c == null ? "" : String(c); tr.appendChild(th); });
  thead.appendChild(tr); table.appendChild(thead);
  const tbody = document.createElement("tbody");
  table.appendChild(tbody);
  const RENDER = 1000;
  let drawn = 1; // 跳过表头
  function draw(n) {
    const end = Math.min(drawn + n, rows.length);
    for (let i = drawn; i < end; i++) {
      const r = document.createElement("tr");
      (rows[i] || []).forEach((c) => { const td = document.createElement("td"); td.textContent = c == null ? "" : String(c); r.appendChild(td); });
      tbody.appendChild(r);
    }
    drawn = end;
  }
  draw(RENDER - 1);
  region.appendChild(table);
  body.appendChild(region);
  if (drawn < rows.length) {
    const more = document.createElement("button");
    more.type = "button"; more.className = "link-btn csv-more";
    more.textContent = `显示更多（共 ${rows.length} 行）`;
    more.addEventListener("click", () => {
      draw(RENDER);
      if (drawn >= rows.length) more.remove();
      else more.textContent = `显示更多（共 ${rows.length} 行，已显示 ${drawn}）`;
    });
    body.appendChild(more);
  }
}
// 文档卡全屏：clone .doc-body 进覆层（不搬原节点，避免 iframe 重载/docx 脱挂）
function openDocFullscreen(card) {
  const last = document.activeElement;
  const box = document.createElement("div");
  box.className = "media-lightbox doc-lightbox";
  box.setAttribute("role", "dialog"); box.setAttribute("aria-modal", "true");
  const inner = document.createElement("div"); inner.className = "doc-lightbox-inner";
  inner.appendChild(card.querySelector(".doc-body").cloneNode(true));
  box.appendChild(inner); box.tabIndex = -1;
  const close = () => { box.remove(); document.removeEventListener("keydown", onKey); last?.focus?.(); };
  const onKey = (e) => { if (e.key === "Escape") close(); };
  box.addEventListener("click", (e) => { if (e.target === box) close(); });
  document.addEventListener("keydown", onKey);
  document.body.appendChild(box); box.focus();
}
// toast（保存反馈）
function toast(msg, isError) {
  const t = document.createElement("div");
  t.className = "toast" + (isError ? " toast-error" : "");
  t.textContent = msg;
  document.body.appendChild(t);
  requestAnimationFrame(() => t.classList.add("show"));
  setTimeout(() => { t.classList.remove("show"); setTimeout(() => t.remove(), 200); }, 2400);
}
// 文档卡公共：系统打开 + 全屏 按钮（复用既有 .doc-actions，避免与 appendEditButton 重复建条）
function appendDocActions(card, j) {
  let bar = card.querySelector(".doc-actions");
  if (!bar) { bar = document.createElement("div"); bar.className = "doc-actions"; card.appendChild(bar); }
  if (j.path) {
    const open = document.createElement("button");
    open.type = "button"; open.className = "link-btn";
    open.textContent = "在系统查看器打开";
    open.addEventListener("click", () => invoke("plugin:opener|open_path", { path: j.path }).catch(() => {}));
    bar.appendChild(open);
  }
  const fs = document.createElement("button");
  fs.type = "button"; fs.className = "link-btn";
  fs.textContent = "全屏";
  fs.addEventListener("click", () => openDocFullscreen(card));
  bar.appendChild(fs);
}
async function renderDocCard(wrap, j) {
  const tools = wrap.querySelector(".bubble-media");
  const card = document.createElement("div");
  card.className = "media-card doc-card";
  const filename = j.path ? String(j.path).split(/[\\/]/).pop() : (j.caption || "片段");
  const title = `${labelFor(j.kind)}：${filename}`;
  card.setAttribute("aria-label", title);
  const body = document.createElement("div"); body.className = "doc-body";
  card.appendChild(body);
  if (j.caption) { const c = document.createElement("div"); c.className = "media-caption"; c.textContent = j.caption; card.appendChild(c); }
  tools.appendChild(card); scrollBottom();
  try {
    if (j.kind === "html") {
      const ifr = document.createElement("iframe");
      ifr.setAttribute("sandbox", "allow-scripts");
      ifr.setAttribute("referrerpolicy", "no-referrer");
      ifr.className = "doc-iframe"; ifr.title = title;
      if (j.path) ifr.src = convertFileSrc(j.path, "media"); else ifr.srcdoc = j.html || "";
      body.appendChild(ifr);
    } else if (j.kind === "pdf") {
      const ifr = document.createElement("iframe");
      ifr.setAttribute("sandbox", "allow-scripts");
      ifr.className = "doc-iframe"; ifr.title = title;
      ifr.src = convertFileSrc(j.path, "media");
      body.appendChild(ifr);
    } else if (j.kind === "docx") {
      await renderDocx(body, j.path);
    } else if (j.kind === "csv") {
      await renderCsv(body, j.path);
    } else if (j.kind === "markdown") {
      const md = await fetchText(j.path);
      const d = document.createElement("div"); d.className = "md-body md";
      d.innerHTML = renderMarkdown(md);
      body.appendChild(d);
      appendEditButton(card, { path: j.path, kind: "markdown", content: md, wrap });
    }
    card.classList.add("doc-ready");
  } catch (e) {
    showDocError(body, `加载失败：${(e && e.message) || e}`);
  }
  appendDocActions(card, j);
}
// renderEditCard 占位：Task 7 实现；此处先给 stub 以便 appendEditButton 编译通过
function appendEditButton(card, ctx) { /* Task 7 填充 */ }
```

> 注：`appendEditButton` 在本任务留 stub（空函数），T7 替换为真实实现。这是有意的跨任务占位，不是计划漏洞——T7 必须实现它。

- [ ] **Step 4: 扩展 llm-tool-call 占位处理器**

在 `src/main.js` 的 `setupAgentEvents` 内，把 `llm-tool-call` 监听里：

```js
    if (p.name === "display_media") {
```

改为：

```js
    if (p.name === "display_media" || p.name === "display_doc" || p.name === "edit_file") {
```

并把占位卡文案从 `准备中… ${fname}` 改为区分编辑：

```js
        const fname = String(p.args || "").replace(/.*"path"\s*:\s*"([^"]*)".*/, "$1").split(/[\\/]/).pop();
        ph.textContent = `${p.name === "edit_file" ? "编辑中" : "准备中"}… ${fname || ""}`;
```

- [ ] **Step 5: 扩展 llm-tool-result 渲染分发**

把 `llm-tool-result` 监听里的 `if (p.name === "display_media") { ... return; }` 整块替换为：

```js
    if (p.name === "display_media" || p.name === "display_doc" || p.name === "edit_file") {
      if (!activeAssistantWrap) return;
      const tools = activeAssistantWrap.querySelector(".bubble-media");
      const ph = tools?.querySelector(".media-loading");
      if (ph) ph.remove();
      let j; try { j = JSON.parse(p.result || "{}"); } catch { j = {}; }
      const ok = j.display !== undefined ? j.display : j.edit;
      if (p.name === "display_media") {
        ok ? renderMediaCard(activeAssistantWrap, j) : appendDocError(tools, j.error);
      } else if (p.name === "display_doc") {
        ok ? renderDocCard(activeAssistantWrap, j) : appendDocError(tools, j.error);
      } else { // edit_file
        ok ? renderEditCard(activeAssistantWrap, j) : appendDocError(tools, j.error);
      }
      return;
    }
```

并在文件顶部辅助函数区（`renderMediaCard` 附近）加一个小工具：

```js
function appendDocError(tools, msg) {
  const err = document.createElement("div");
  err.className = "media-card media-error";
  err.textContent = msg || "展示失败";
  tools?.appendChild(err); scrollBottom();
}
```

> `renderEditCard` 在本任务尚未实现——Step 5 引用了它。为让 `node --check` 通过，先在 Step 3 的函数区加一个临时 stub `async function renderEditCard(wrap, j) { appendDocError(wrap.querySelector(".bubble-media"), "编辑卡即将就绪"); }`，T7 替换为真实实现。

- [ ] **Step 6: 语法检查**

Run: `node --check src/main.js`
Expected: 无输出（语法 OK）。

- [ ] **Step 7: 提交**

```bash
git add src/index.html src/main.js src/vendor/jszip.min.js src/vendor/docx-preview.min.js src/vendor/papaparse.min.js
git commit -m "feat(doc): vendor 引入 + renderDocCard（html/pdf/docx/csv/md）+ 处理器接线"
```

---

### Task 7: renderEditCard + 编辑按钮 + 未保存守卫（前端）

**Files:**
- Modify: `src/main.js`（实现 `renderEditCard` + `appendEditButton`；reset 入口加 dirty 守卫）

**Interfaces:**
- Consumes: T3 契约（`{edit,path,kind,content,caption}`）；`write_file` 命令（T5）；`renderMarkdown`。
- Produces: 编辑卡（textarea + 实时预览 + 保存）；查看卡上的"编辑"按钮切编辑态；reset 前确认未保存改动。

- [ ] **Step 1: 替换 renderEditCard stub 为真实实现**

把 T6 Step 5 加的临时 `renderEditCard` stub 整体替换为：

```js
// 编辑卡：左 textarea + 右实时预览 + 保存→write_file；dirty 守卫
function renderEditCard(wrap, j) {
  const tools = wrap.querySelector(".bubble-media");
  const card = document.createElement("div");
  card.className = "media-card edit-card";
  const fname = String(j.path).split(/[\\/]/).pop();
  card.setAttribute("aria-label", `编辑 ${fname}`);
  const head = document.createElement("div"); head.className = "edit-head";
  head.textContent = `编辑：${fname}`;
  card.appendChild(head);
  const split = document.createElement("div"); split.className = "edit-split";
  const ta = document.createElement("textarea");
  ta.className = "edit-textarea"; ta.value = j.content || ""; ta.setAttribute("aria-label", "编辑内容");
  const prev = document.createElement("div"); prev.className = "edit-preview";
  split.appendChild(ta); split.appendChild(prev);
  card.appendChild(split);
  const actions = document.createElement("div"); actions.className = "edit-actions";
  const save = document.createElement("button");
  save.type = "button"; save.className = "primary-btn"; save.textContent = "保存";
  actions.appendChild(save);
  card.appendChild(actions);
  tools.appendChild(card); scrollBottom();
  let dirty = false; let timer = null;
  const refresh = () => {
    if (j.kind === "markdown") prev.innerHTML = renderMarkdown(ta.value);
    else { prev.innerHTML = ""; const pre = document.createElement("pre"); pre.textContent = ta.value; prev.appendChild(pre); }
  };
  ta.addEventListener("input", () => {
    dirty = true; card.dataset.dirty = "1";
    clearTimeout(timer); timer = setTimeout(refresh, 200);
  });
  refresh();
  save.addEventListener("click", async () => {
    save.disabled = true;
    try {
      const msg = await invoke("write_file", { path: j.path, content: ta.value });
      dirty = false; delete card.dataset.dirty;
      toast(msg);
    } catch (e) { toast("保存失败：" + (e || ""), true); }
    finally { save.disabled = false; }
  });
}
```

- [ ] **Step 2: 实现 appendEditButton（替换 T6 stub）**

把 T6 Step 3 加的 `appendEditButton` stub 整体替换为：

```js
// 查看卡（markdown/text）右上"编辑"按钮：切编辑态（同 wrap 内新建编辑卡，预填当前内容）
function appendEditButton(card, ctx) {
  const edit = document.createElement("button");
  edit.type = "button"; edit.className = "link-btn edit-btn";
  edit.textContent = "编辑";
  edit.addEventListener("click", () => {
    card.remove();
    renderEditCard(ctx.wrap || activeAssistantWrap, { edit: true, path: ctx.path, kind: ctx.kind, content: ctx.content });
  });
  const bar = card.querySelector(".doc-actions") || (() => { const b = document.createElement("div"); b.className = "doc-actions"; card.appendChild(b); return b; })();
  bar.appendChild(edit);
}
```

- [ ] **Step 3: 加 dirty 守卫并接入 reset 入口**

在辅助函数区（`toast` 附近）加：

```js
// 未保存保护（轻量）：有 dirty 编辑卡时 reset 前确认
function confirmDiscardDirtyEdits() {
  const dirty = document.querySelectorAll('.edit-card[data-dirty="1"]');
  if (dirty.length && !confirm("有未保存的编辑改动，放弃？")) return false;
  return true;
}
```

找到调用 `await invoke("reset_session")` 的那个函数（重置/清空对话的入口，约 main.js 内 `invoke("reset_session")` 处），在该 `invoke` **之前**加守卫：

```js
  if (!confirmDiscardDirtyEdits()) return;
```

（即在触发后端 reset 前拦截，避免清掉未保存编辑。）

- [ ] **Step 4: 语法检查**

Run: `node --check src/main.js`
Expected: 无输出（语法 OK）。

- [ ] **Step 5: 提交**

```bash
git add src/main.js
git commit -m "feat(doc): renderEditCard（textarea+预览+保存）+ 编辑按钮切态 + dirty 守卫"
```

---

### Task 8: 样式（styles.css）

**Files:**
- Modify: `src/styles.css`（在现有 `.media-lightbox` 块之后追加文档/编辑卡样式 + toast + 响应式）

**Interfaces:**
- Consumes: 既有 `--glass-alpha` / `--text-secondary` / `--text-tertiary` / `--error` / `.media-card` / `.media-lightbox` / `.primary-btn` / `.link-btn`。
- Produces: `.doc-card` / `.doc-body` / `.doc-iframe` / `.docx-container` / `.csv-wrap` / `.csv-table` / `.md-body` / `.doc-actions` / `.edit-card` / `.edit-split` / `.edit-textarea` / `.edit-preview` / `.edit-head` / `.edit-actions` / `.doc-lightbox` / `.toast` + `@media(max-width:640px)`。

- [ ] **Step 1: 追加样式块**

在 `src/styles.css` 末尾（`.media-error-note` 那行之后）追加：

```css
/* ===== 文档卡 / 编辑卡（复用 .media-card 玻璃底）===== */
.doc-card { /* 仅作语义钩子，视觉同 .media-card */ }
.doc-body {
  min-height: 120px;
  max-height: min(480px, 62vh);
  overflow: auto;
  border-radius: 10px;
  background: rgba(255, 255, 255, 0.55);
}
.doc-card .doc-body:has(iframe.doc-iframe) { background: transparent; padding: 0; }
.doc-iframe {
  width: 100%;
  height: min(480px, 62vh);
  border: 0;
  border-radius: 10px;
  display: block;
  background: #fff;
}
/* pdf 卡放大一点 */
.doc-card[data-kind="pdf"] .doc-iframe,
.media-card:has(iframe[src$=".pdf"]) .doc-iframe { height: min(520px, 70vh); }
.doc-actions { display: flex; gap: 10px; flex-wrap: wrap; align-self: flex-start; }
.edit-btn { font-weight: 600; }

/* docx：docx-preview 输出继承文字色 */
.docx-container { color: var(--text); background: #fff; border-radius: 8px; padding: 12px; }
.docx-container .docx-body { color: #1d1d1f; }

/* CSV 单色玻璃表 */
.csv-wrap { overflow-x: auto; max-height: inherit; }
.csv-table { border-collapse: collapse; width: 100%; font-size: 13px; color: var(--text); }
.csv-table th, .csv-table td { padding: 5px 8px; border-bottom: 1px solid var(--separator); white-space: nowrap; }
.csv-table thead th {
  position: sticky; top: 0; background: rgba(0, 0, 0, 0.06); text-align: left; font-weight: 600;
}
.csv-table tbody tr:nth-child(odd) { background: rgba(0, 0, 0, 0.02); }
.csv-more { margin-top: 6px; align-self: flex-start; }

/* markdown 查看体 */
.md-body.md { font-size: 14px; line-height: 1.6; color: var(--text); }
.md-body.md :first-child { margin-top: 0; }

/* 编辑卡 */
.edit-card { /* 同 .media-card */ }
.edit-head { font-size: 13px; color: var(--text-secondary); }
.edit-split { display: flex; gap: 8px; }
.edit-textarea {
  flex: 1 1 0; min-height: 180px; resize: vertical;
  font-family: ui-monospace, "SF Mono", Menlo, Consolas, monospace; font-size: 13px;
  border: 1px solid var(--separator); border-radius: 8px; padding: 8px 10px;
  background: rgba(255, 255, 255, 0.9); color: var(--text);
}
.edit-preview {
  flex: 1 1 0; min-height: 180px; overflow: auto;
  padding: 8px 10px; border: 1px solid var(--separator); border-radius: 8px;
  background: rgba(255, 255, 255, 0.55); font-size: 13px;
}
.edit-preview pre { white-space: pre-wrap; word-break: break-word; margin: 0; font-family: ui-monospace, Menlo, Consolas, monospace; }
.edit-actions { display: flex; justify-content: flex-end; }

/* 文档卡全屏覆层（复用 .media-lightbox 黑底）*/
.doc-lightbox { padding: 24px; }
.doc-lightbox-inner {
  max-width: 92vw; max-height: 92vh; overflow: auto;
  background: #fff; border-radius: 10px; padding: 16px;
}
.doc-lightbox-inner .doc-body { max-height: none; }

/* toast */
.toast {
  position: fixed; left: 50%; bottom: 28px; transform: translate(-50%, 20px);
  background: rgba(29, 29, 31, 0.92); color: #fff; font-size: 13px;
  padding: 8px 14px; border-radius: 10px; z-index: 10000;
  opacity: 0; transition: opacity 0.2s ease, transform 0.2s ease;
  max-width: 80vw; word-break: break-all;
}
.toast.show { opacity: 1; transform: translate(-50%, 0); }
.toast.toast-error { background: var(--error); }

/* 响应式：窄窗编辑卡分栏纵向堆叠 */
@media (max-width: 640px) {
  .edit-split { flex-direction: column; }
}
```

- [ ] **Step 2: 全量构建 + 前端语法**

Run: `cargo build --manifest-path src-tauri/Cargo.toml 2>&1 | tail -3`
Expected: `Finished`，零 warning。

Run: `node --check src/main.js`
Expected: 无输出。

- [ ] **Step 3: 提交**

```bash
git add src/styles.css
git commit -m "feat(doc): 文档/编辑卡样式（玻璃复用 + 限高 + CSV 表 + 全屏覆层 + toast + 窄窗堆叠）"
```

---

### Task 9: smoke 文档 + 真机手测

**Files:**
- Create: `docs/superpowers/smoke/2026-07-24-chat-documents.md`

- [ ] **Step 1: 全量自动化回归**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib 2>&1 | tail -5`
Expected: 全 PASS（tools 新增 display_doc/edit_file/write_file_scoped + lib DocKind 测试；既有测试不受影响）。

Run: `cargo build --manifest-path src-tauri/Cargo.toml 2>&1 | tail -3`
Expected: `Finished`，零 warning。

Run: `node --check src/main.js`
Expected: 无输出。

- [ ] **Step 2: 写 smoke 文档**

创建 `docs/superpowers/smoke/2026-07-24-chat-documents.md`：

````markdown
# chat-documents 冒烟记录

> 日期：2026-07-24 · 分支 `feat/chat-documents`（off master @ 2a8fdf9）
> 自动化：cargo test --lib 全 PASS + 零 warning 构建 + node --check OK。
> 真机 smoke：`taskkill //F //IM ovoice.exe` → `pnpm tauri dev`，按下表执行。

## 1. 自动化（已跑）

| 检查 | 命令 | 结果 |
|---|---|---|
| 单元/回归 | `cargo test --manifest-path src-tauri/Cargo.toml --lib` | 全 PASS |
| 全量构建 | `cargo build --manifest-path src-tauri/Cargo.toml` | Finished，零 warning |
| 前端语法 | `node --check src/main.js` | OK |

## 2. 真机 smoke（人工，回填）

| # | 操作 | 预期 | 实测 |
|---|---|---|---|
| 1 | agent `display_doc({path:"a.html"})` | 沙箱 iframe 渲染；脚本跑；限高内滚 | ☐ |
| 2 | 该 HTML 含 `parent.__TAURI__` 探针 | 探针拿不到 invoke（opaque origin 阻断） | ☐ |
| 3 | agent `display_doc({html:"<b>内联</b>"})` | srcdoc iframe 渲染片段 | ☐ |
| 4 | agent `display_doc({path:"r.pdf"})` | 内置阅读器；缩放/搜索；`min(520px,70vh)` | ☐ |
| 5 | agent `display_doc({path:"d.docx"})` | docx-preview 表格/图/分页 | ☐ |
| 6 | agent `display_doc({path:"old.doc"})` | 提示卡「不支持 .doc」+ 系统打开 | ☐ |
| 7 | agent `display_doc({path:"data.csv"})` | 表格；首 1000 行；"显示更多（共 N 行）" | ☐ |
| 8 | agent `edit_file({path:"n.md"})` | 编辑卡 textarea+预览；预填磁盘内容 | ☐ |
| 9 | 改后点"保存" | toast「已写入 …」；磁盘内容一致 | ☐ |
| 10 | 编辑后不保存，点清空对话 | confirm「有未保存…放弃？」 | ☐ |
| 11 | markdown 查看卡点"编辑" | 切编辑态，预填当前内容 | ☐ |
| 12 | 任意文档卡点"全屏" | 覆层全宽展示；Esc/点遮罩关；焦点归还 | ☐ |
| 13 | 窗口收到 <640px 宽 | 编辑卡 textarea/预览纵向堆叠 | ☐ |
| 14 | write_file 越权路径（前端构造） | Err「路径不在允许范围内」；不落盘 | ☐ |

## 3. 已知限制

- `.doc` 旧二进制不支持（提示卡 + 系统打开）。
- CSV 仅首 1000 行 + 显示更多（>10k 行虚拟化留 v2）。
- 跨域沙箱 iframe 无法自适应高度 → 限高 + 全屏放大。
- docx 全屏走 `cloneNode`（静态 DOM 克隆，不重跑脚本）。
````

- [ ] **Step 3: 提交**

```bash
git add docs/superpowers/smoke/2026-07-24-chat-documents.md
git commit -m "docs(smoke): chat-documents 冒烟清单（自动化 PASS + 真机手测表）"
```

---

## Self-Review（写完后自查）

**1. Spec 覆盖：** 逐条对照 spec——
- HTML 文件/片段 → T6 renderDocCard（src/srcdoc + sandbox allow-scripts）✅
- PDF 内置阅读器 → T6 ✅
- docx docx-preview → T6 renderDocx ✅
- CSV PapaParse + 首 1000 行 + 显示更多 → T6 renderCsv ✅
- markdown 查看 → T6 ✅
- 编辑卡 textarea+预览+保存 → T7 ✅
- write_file scoped → T5 ✅
- display_doc/edit_file 工具 + dispatch/schemas → T2/T3/T4 ✅
- 交互状态表（loading/empty/error）→ T6（fetch/parse 失败 showDocError；空 CSV「CSV 为空」）✅
- a11y（iframe title/label/th scope/焦点）→ T6/T8 ✅
- 响应式限高 + 窄窗堆叠 → T8 ✅
- 全屏不搬节点（cloneNode）→ T6 openDocFullscreen ✅
- dirty 轻量保护 → T7 ✅
- 复用 .media-card token → T8 ✅
- vendor UMD → T6 ✅
- .doc 提示卡 → T2（error with hint）✅

**2. 占位扫描：** T6 的 `appendEditButton`/`renderEditCard` 是**跨任务有意的临时 stub**，T7 必须替换为真实实现（已显式标注，非计划漏洞）。除此之外无 TBD/TODO/"适当处理错误"等。✅

**3. 类型/命名一致性：** `renderDocCard`/`renderEditCard`/`write_file_scoped`/`doc_kind_from_ext`/`DocKind` 在定义（T1-T7）与消费处签名一致；前端契约字段（`display`/`edit`/`path`/`html`/`kind`/`content`/`caption`/`error`）前后端对齐。✅

**4. 执行前预扫（pre-flight）修正：**
- **Important — MIME 缺口：** `mime_from_ext` 原只覆盖 image/video/audio，html/pdf 落到 `application/octet-stream`，会让 **PDF 内置阅读器不激活** + **HTML iframe 不渲染**。已在 **T1** 一并补 html/htm→`text/html`、pdf→`application/pdf`、csv/docx/md/txt 各自 MIME（含 `mime_covers_doc_kinds` 测试）。`media://` 协议处理器用 `mime_from_ext` 设 `Content-Type`，故扩展即生效。
- **Minor — 重复操作条：** markdown 分支 `appendEditButton`（T7）在 `appendDocActions`（T6）之前跑且各自建 `.doc-actions` 条。已让 `appendDocActions` 复用既有条（与 `appendEditButton` 同款 reuse-or-create），保证每卡单条。

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-24-chat-documents.md`. Two execution options:

**1. Subagent-Driven (recommended)** - 每 task 派一个新 subagent，task 间评审，快迭代。

**2. Inline Execution** - 本 session 内按 executing-plans 批量执行 + 检查点。

Which approach?
