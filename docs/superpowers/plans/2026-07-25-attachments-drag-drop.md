# 聊天附件 + 拖放加载 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让用户能把本地文件带进 ovoice 对话：附件按钮 / 拖到输入框 → 作为附件随消息发给 MiniMax-M3（图片视频走多模态、文本内联、二进制走 agent 工具）；拖到消息区 → 立刻本地渲染/编辑 + 静默上下文（不触发模型）。

**Architecture:** Rust 主导。前端只 stage + 预览 + 拖放路由；密钥、请求构造、base64、消息历史都在 Rust。附件复制进 `app_data/workspace/attachments/`（去重）。user 多模态消息在历史里存**路径引用**（`image_ref`/`video_ref`），`build_body` 发送时才展开成 `image_url`/`video_url` base64 + 每请求媒体预算守卫（≤58MB）。静默上下文走 `SessionEvent::ContextNote`（driver 私有持有 `messages`，命令只能发事件）。

**Tech Stack:** Rust + Tauri v2（`withGlobalTauri:true`，`csp:null`，自定义 `media://` 协议）；vanilla JS（无打包器，`/vendor/*.min.js` UMD）；`base64 = "0.22"`（已 dep）；新增 `sha2 = "0.10"`（去重）。

## Global Constraints

- **无打包器、UMD-only**：前端 deps 全是 `/vendor/*.min.js`；`main.js` 裸 module；无 JS 测试框架 → 前端用 `node --check src/main.js` + 手测。
- **base64 0.22 API**：用 `use base64::Engine; base64::engine::general_purpose::STANDARD.encode(bytes)`（`base64::encode` 已移除）。参考实现：`lib.rs:113-116`、`lib.rs:199`。
- **agent.rs 事件驱动零锁 session**：`messages` 由 driver task 私有持有（`agent.rs:96-110`）；命令只有 `mpsc::Sender`（`agent.rs:69-72`）。**任何要改消息历史的命令都必须发 `SessionEvent`，不能直接改 `messages`**。
- **build_body 每轮重发全量 messages**（`agent.rs:56→66`）：多模态 part 每轮都上传；必须存引用、发送时展开 + 预算守卫。
- **dev server 占 exe**：`npm run tauri dev` 跑时 `cargo build` 失败，用 `cargo check --manifest-path src-tauri/Cargo.toml --tests` 验证（见 [[ovoice-dev-server-cargo-lock]]）。
- **工具计数级联**：本功能**不增减 agent 工具**（write/read/bash/display/edit_file 不变）→ `tools::schemas()` 仍 5 个、`llm.rs:424` 断言仍 `== 5`，**无级联**。若误改 `schemas()` 必须同步该断言 + `tools.rs` `schemas_has_five_tools` 测试（见 [[ovoice-tool-count-cascade]]）。
- **类型判定复用**：`lib.rs::media_kind_from_ext`(210) / `doc_kind_from_ext`(224) / `mime_from_ext`(246) / `is_within_roots`(278) 不改；本计划新增 `file_kind()` 包装前两者（DRY）。
- **提交规范**：中文 commit message，末尾 `Co-Authored-By: Claude <noreply@anthropic.com>`。本分支 `feat/attachments-drag-drop`（off master @ de10921）。每个任务结束 commit。

---

## File Structure

| 文件 | 职责 | 本计划改动 |
|---|---|---|
| `src-tauri/src/lib.rs` | 命令 + 类型判定 + `media://` 协议 | 新增 `FileKind`/`file_kind()`/`file_kind_str()`、`StagedFile`/`AttachmentRef` 结构、`stage_attachments`/`append_context_note` 命令、`chat` 加 attachments 参；注册到 invoke_handler |
| `src-tauri/src/tools.rs` | 工具纯逻辑 | 新增 `stage_one()` 纯逻辑（复制+去重+尺寸+kind+文本提取）+ 测试；`tool_display` 改用 `file_kind`（DRY） |
| `src-tauri/src/agent.rs` | 常驻 session driver | `SessionEvent::UserMessage` 加 `attachments`；新增 `ContextNote` 变体；`handle_event` 用 llm helper 组消息；`window_messages` 折叠旧媒体引用 |
| `src-tauri/src/llm.rs` | 请求构造 | 新增 `user_message_with_attachments()` + `expand_messages_for_send()`（含预算守卫）；`build_body` 改返 `Result` + 调展开；`HttpRound` 处理 Err；测试 |
| `src-tauri/src/config.rs` | 配置 | 新增 `max_attachment_mb`（默认 30）+ 测试 |
| `src-tauri/Cargo.toml` | 依赖 | 加 `sha2 = "0.10"` |
| `src/main.js` | 前端逻辑 | 附件按钮、预览条、发送带附件、两 dropzone、拖放状态机、本地渲染入口、a11y、空消息区提示 |
| `src/index.html` | 前端结构 | 附件按钮 + 预览条容器；设置页「附件大小上限」输入 |

---

## Task 1: 基础设施 — config 字段 + file_kind DRY + 类型定义

**Files:**
- Modify: `src-tauri/src/config.rs`（加 `max_attachment_mb`）
- Modify: `src-tauri/src/lib.rs`（加 `FileKind`/`file_kind()`/`file_kind_str()` + `StagedFile`/`AttachmentRef`）
- Modify: `src-tauri/src/tools.rs`（`tool_display` 改用 `file_kind`）
- Test: 同上各文件 `#[cfg(test)]`

**Interfaces:**
- Produces: `config::Config::max_attachment_mb: u64`（默认 30）；`crate::FileKind`（Image/Video/Audio/Html/Pdf/Docx/Csv/Markdown/Text/Unsupported）；`crate::file_kind(&Path) -> FileKind`；`crate::file_kind_str(FileKind) -> &'static str`；`crate::StagedFile{staged_path,kind,original_name,size,text?}`（Serialize）；`crate::AttachmentRef{staged_path,kind}`（Deserialize）。
- Consumes: 既有 `media_kind_from_ext`/`doc_kind_from_ext`。

- [ ] **Step 1: 加 config 字段（先写失败测试）**

在 `src-tauri/src/config.rs` 的 `Config` struct 里（`minimax_region` 字段后、`bg_enabled` 前）加：

```rust
    // 单个附件大小上限（MB）：图片恒≤10（MiniMax 硬限制）；其余类型受此值约束，超限拒绝
    #[serde(default = "d_max_attachment_mb")]
    pub max_attachment_mb: u64,
```

在默认函数区（`d_voice_action` 后）加：

```rust
fn d_max_attachment_mb() -> u64 { 30 }
```

在 `impl Default for Config`（`minimax_region: d_region(),` 后）加：

```rust
            max_attachment_mb: d_max_attachment_mb(),
```

在 `roundtrip_preserves_all_fields` 测试的 `Config { ... }` 字面量加 `max_attachment_mb: 50,`，并在断言区加 `assert_eq!(back.max_attachment_mb, 50);`。在 `missing_fields_use_defaults` 断言区加 `assert_eq!(back.max_attachment_mb, 30);`。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib config::tests`
Expected: FAIL（字段不存在，编译错）

- [ ] **Step 3: 实现 config 字段**

按 Step 1 的代码块实际写入 `config.rs`（字段 + default fn + Default impl）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib config::tests`
Expected: PASS

- [ ] **Step 5: 加 FileKind + file_kind + file_kind_str（先写失败测试）**

在 `src-tauri/src/lib.rs` 的 `DocKind`/`doc_kind_from_ext` 之后、`mime_from_ext` 之前加 `FileKind` 枚举与函数，并在 `media_tests` 模块加测试：

```rust
/// 统一文件类型（media+doc 合并）：display 与 stage_attachments 共用，新增扩展名只改这里。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum FileKind { Image, Video, Audio, Html, Pdf, Docx, Csv, Markdown, Text, Unsupported }

/// 按扩展名判统一类型：先 media（图/视/音），未命中再 doc。
pub(crate) fn file_kind(path: &std::path::Path) -> FileKind {
    match media_kind_from_ext(path) {
        MediaKind::Image => FileKind::Image,
        MediaKind::Video => FileKind::Video,
        MediaKind::Audio => FileKind::Audio,
        MediaKind::Unsupported => match doc_kind_from_ext(path) {
            DocKind::Html => FileKind::Html,
            DocKind::Pdf => FileKind::Pdf,
            DocKind::Docx => FileKind::Docx,
            DocKind::Csv => FileKind::Csv,
            DocKind::Markdown => FileKind::Markdown,
            DocKind::Text => FileKind::Text,
            DocKind::Unsupported => FileKind::Unsupported,
        },
    }
}

/// FileKind → 前端/JSON 契约字符串（与 tool_display 既有 kind 字面量一致）。
pub(crate) fn file_kind_str(k: FileKind) -> &'static str {
    match k {
        FileKind::Image => "image", FileKind::Video => "video", FileKind::Audio => "audio",
        FileKind::Html => "html", FileKind::Pdf => "pdf", FileKind::Docx => "docx",
        FileKind::Csv => "csv", FileKind::Markdown => "markdown", FileKind::Text => "text",
        FileKind::Unsupported => "unsupported",
    }
}
```

在 `media_tests` 加：

```rust
    #[test]
    fn file_kind_covers_all_categories() {
        use FileKind::*;
        assert_eq!(file_kind(Path::new("a.png")), Image);
        assert_eq!(file_kind(Path::new("b.mp4")), Video);
        assert_eq!(file_kind(Path::new("c.mp3")), Audio);
        assert_eq!(file_kind(Path::new("d.html")), Html);
        assert_eq!(file_kind(Path::new("e.pdf")), Pdf);
        assert_eq!(file_kind(Path::new("f.docx")), Docx);
        assert_eq!(file_kind(Path::new("g.csv")), Csv);
        assert_eq!(file_kind(Path::new("h.md")), Markdown);
        assert_eq!(file_kind(Path::new("i.py")), Text);
        assert_eq!(file_kind(Path::new("x.exe")), Unsupported);
    }
```

- [ ] **Step 6: 跑测试确认失败（函数未定义）**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib media_tests::file_kind_covers_all_categories`
Expected: FAIL（`file_kind` 未定义）

- [ ] **Step 7: 实现 FileKind 三件套**

按 Step 5 代码块写入 `lib.rs`。

- [ ] **Step 8: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib media_tests`
Expected: PASS（含新测试 + 既有 media/doc 测试不回归）

- [ ] **Step 9: tool_display 改用 file_kind（DRY，回归保护）**

在 `src-tauri/src/tools.rs` 把 `tool_display` 里的两步 match（`tools.rs:163-180`）替换为：

```rust
    let kind_enum = crate::file_kind(&path);
    if kind_enum == crate::FileKind::Unsupported {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let hint = if ext == "doc" { "（.doc 旧格式不支持，请在系统查看器打开）" } else { "" };
        return serde_json::json!({ "display": false, "error": format!("不支持的文件类型: .{ext}{hint}") }).to_string();
    }
    let kind_str = crate::file_kind_str(kind_enum);
```

（删掉原 `let kind_str = match crate::media_kind_from_ext ...` 整块；保留其后 `let abs = ...` 与返回。）

- [ ] **Step 10: 跑 display 测试确认不回归**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib tools::tests::display`
Expected: PASS（`display_auto_routes_by_ext` / `display_doc_unsupported_with_hint` / `display_media_kind_from_ext` 全过）

- [ ] **Step 11: 加 StagedFile / AttachmentRef 结构**

在 `src-tauri/src/lib.rs`（`FileKind` 区附近）加：

```rust
/// stage 后的附件结果（返前端：预览条 / 本地渲染用）。
#[derive(serde::Serialize)]
pub(crate) struct StagedFile {
    pub ok: bool,
    pub staged_path: String,      // 绝对路径（attachments/<ts>_<名>）
    pub kind: String,             // file_kind_str
    pub original_name: String,
    pub size: u64,
    pub text: Option<String>,     // 文本类且 ≤50KB 才填
    pub error: Option<String>,    // ok=false 时填
}

/// 发送时随消息带的附件引用（前端 → chat 命令）。
#[derive(serde::Deserialize, Clone)]
pub struct AttachmentRef {
    pub staged_path: String,
    pub kind: String,
}
```

- [ ] **Step 12: cargo check 全量零 warning**

Run: `cargo check --manifest-path src-tauri/Cargo.toml --tests`
Expected: 编译通过、零 warning（StagedFile/AttachmentRef 此刻未被使用，可能告警 unused → 暂加 `#[allow(dead_code)]` 于两结构上，Task 2/4 用到后再删）

- [ ] **Step 13: Commit**

```bash
git add src-tauri/src/config.rs src-tauri/src/lib.rs src-tauri/src/tools.rs
git commit -m "feat(attachments): config max_attachment_mb + file_kind DRY + StagedFile/AttachmentRef 类型

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 2: stage_attachments — 复制进工作区 + 去重 + 尺寸 + kind + 文本提取

**Files:**
- Modify: `src-tauri/Cargo.toml`（加 `sha2`）
- Modify: `src-tauri/src/tools.rs`（加 `stage_one` 纯逻辑 + 测试）
- Modify: `src-tauri/src/lib.rs`（加 `stage_attachments` 命令 + 注册）
- Test: `tools.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `crate::file_kind`/`file_kind_str`、`crate::is_within_roots`、`crate::StagedFile`、`std::fs`、`sha2`。
- Produces: `tools::stage_one(src: &Path, attachments_dir: &Path, max_mb: u64) -> StagedFile`（纯逻辑，可离线测）；Tauri 命令 `stage_attachments(paths: Vec<String>, app) -> Vec<StagedFile>`。

- [ ] **Step 1: 加 sha2 依赖**

在 `src-tauri/Cargo.toml` 的 `[dependencies]` 区（`base64 = "0.22"` 旁）加：

```toml
sha2 = "0.10"
```

Run: `cargo check --manifest-path src-tauri/Cargo.toml`
Expected: 拉取 sha2，编译通过。

- [ ] **Step 2: 写 stage_one 失败测试**

在 `src-tauri/src/tools.rs` 的 `mod tests` 加：

```rust
    use crate::{FileKind, file_kind, file_kind_str};

    fn stage_ok(args: serde_json::Value, ws: &Path, max_mb: u64) -> crate::StagedFile {
        let p = args["path"].as_str().unwrap();
        let src = resolve_path(p, ws);
        let dir = ws.join("attachments");
        crate::tools::stage_one(&src, &dir, max_mb)
    }

    #[test]
    fn stage_one_copies_and_detects_kind() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::write(ws.join("cat.png"), b"png-bytes").unwrap();
        let s = stage_ok(serde_json::json!({"path":"cat.png"}), ws, 30);
        assert!(s.ok);
        assert_eq!(s.kind, "image");
        assert_eq!(s.original_name, "cat.png");
        assert_eq!(s.size, 9);
        assert!(s.staged_path.ends_with("cat.png"));
        assert!(s.text.is_none(), "图片不提文本");
        // 副本确实落盘
        assert!(std::fs::metadata(&s.staged_path).is_ok());
    }

    #[test]
    fn stage_one_text_under_50kb_inlined() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::write(ws.join("a.txt"), "hello world").unwrap();
        let s = stage_ok(serde_json::json!({"path":"a.txt"}), ws, 30);
        assert!(s.ok);
        assert_eq!(s.kind, "text");
        assert_eq!(s.text.as_deref(), Some("hello world"));
    }

    #[test]
    fn stage_one_text_over_50kb_not_inlined() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let big = "x".repeat(60 * 1024);
        std::fs::write(ws.join("big.txt"), &big).unwrap();
        let s = stage_ok(serde_json::json!({"path":"big.txt"}), ws, 30);
        assert!(s.ok);
        assert!(s.text.is_none(), "超 50KB 不内联");
    }

    #[test]
    fn stage_one_oversize_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::write(ws.join("big.pdf"), vec![0u8; 31 * 1024 * 1024]).unwrap();
        let s = stage_ok(serde_json::json!({"path":"big.pdf"}), ws, 30);
        assert!(!s.ok);
        assert!(s.error.as_deref().unwrap().contains("过大"));
    }

    #[test]
    fn stage_one_dedup_reuses_same_hash() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::write(ws.join("a.png"), b"identical").unwrap();
        std::fs::write(ws.join("b.png"), b"identical").unwrap();
        let s1 = stage_ok(serde_json::json!({"path":"a.png"}), ws, 30);
        let s2 = stage_ok(serde_json::json!({"path":"b.png"}), ws, 30);
        assert_eq!(s1.staged_path, s2.staged_path, "同内容应复用同一副本");
    }

    #[test]
    fn stage_one_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let s = stage_ok(serde_json::json!({"path":"nope.png"}), dir.path(), 30);
        assert!(!s.ok);
        assert!(s.error.as_deref().unwrap().contains("不存在"));
    }
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib tools::tests::stage_one`
Expected: FAIL（`stage_one` 未定义）

- [ ] **Step 4: 实现 stage_one**

在 `src-tauri/src/tools.rs`（`tool_edit_file` 之后）加：

```rust
use sha2::{Sha256, Digest};

pub const TEXT_INLINE_MAX: usize = 50 * 1024;
const IMG_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// 把一个文件复制进 attachments 目录（按内容 hash 去重），判 kind、尺寸校验、文本≤50KB 提取。
/// 纯逻辑（不碰 AppHandle），可离线测。图片恒 ≤10MB；其余 ≤ max_mb；超限 ok=false。
pub fn stage_one(src: &Path, attachments_dir: &Path, max_mb: u64) -> crate::StagedFile {
    let meta = match std::fs::metadata(src) {
        Ok(m) => m,
        Err(_) => return err_staged(&src.to_string_lossy(), format!("文件不存在: {}", src.display())),
    };
    if !meta.is_file() {
        return err_staged(&src.to_string_lossy(), "暂不支持文件夹".into());
    }
    let size = meta.len();
    let kind = crate::file_kind(src);
    if kind == crate::FileKind::Unsupported {
        return err_staged(&src.to_string_lossy(), format!("不支持的文件类型: {}", src.display()));
    }
    let limit = if kind == crate::FileKind::Image { IMG_MAX_BYTES } else { max_mb * 1024 * 1024 };
    if size > limit {
        return err_staged(
            &src.to_string_lossy(),
            format!("文件过大 {:.1} MB，上限 {:.0} MB", size as f64 / 1048576.0, limit as f64 / 1048576.0),
        );
    }
    let bytes = match std::fs::read(src) {
        Ok(b) => b,
        Err(e) => return err_staged(&src.to_string_lossy(), format!("读取失败: {e}")),
    };
    // 去重：sha256 内容寻址。文件名 = <hash 前缀>_<原名>；同 hash 复用。
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let hash = hasher.finalize();
    let hash_hex: String = hash.iter().take(8).map(|b| format!("{:02x}", b)).collect();
    let orig_name = src.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
    let dest_name = format!("{hash_hex}_{orig_name}");
    let dest = attachments_dir.join(&dest_name);
    if !dest.exists() {
        if let Err(e) = std::fs::create_dir_all(attachments_dir) {
            return err_staged(&src.to_string_lossy(), format!("创建目录失败: {e}"));
        }
        if let Err(e) = std::fs::write(&dest, &bytes) {
            return err_staged(&src.to_string_lossy(), format!("复制失败: {e}"));
        }
    }
    let abs = dest.canonicalize().unwrap_or(dest).to_string_lossy().to_string();
    let text = if matches!(kind, crate::FileKind::Text | crate::FileKind::Markdown | crate::FileKind::Csv)
        && size as usize <= TEXT_INLINE_MAX
    {
        String::from_utf8(bytes.clone()).ok()
    } else {
        None
    };
    crate::StagedFile {
        ok: true, staged_path: abs, kind: crate::file_kind_str(kind).to_string(),
        original_name: orig_name, size, text, error: None,
    }
}

fn err_staged(path: &str, error: String) -> crate::StagedFile {
    crate::StagedFile { ok: false, staged_path: path.into(), kind: String::new(),
        original_name: String::new(), size: 0, text: None, error: Some(error) }
}
```

注：`use sha2::{Sha256, Digest};` 放文件顶部 use 区（与既有 `use` 合并，勿重复）；`TEXT_INLINE_MAX`/`IMG_MAX_BYTES` 放 `READ_MAX` 旁。

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib tools::tests::stage_one`
Expected: 5 个全 PASS

- [ ] **Step 6: 加 stage_attachments 命令**

在 `src-tauri/src/lib.rs`（`write_file` 命令后）加：

```rust
/// 把用户选/拖的文件复制进工作区 attachments（去重 + 尺寸校验 + kind + 文本提取）。
/// 逐个处理、逐个返回（一个超大不连累其它）。
#[tauri::command]
fn stage_attachments(paths: Vec<String>, app: AppHandle) -> Vec<crate::StagedFile> {
    let cfg = config::load(&app);
    let ws = std::path::PathBuf::from(&cfg.workspace_dir);
    let dir = ws.join("attachments");
    paths.iter().map(|p| {
        let src = tools::resolve_path(p, &ws);
        tools::stage_one(&src, &dir, cfg.max_attachment_mb)
    }).collect()
}
```

在 `invoke_handler`（`lib.rs:492` 的 `generate_handler!`）加 `stage_attachments,`（列在 `write_file,` 后）。

- [ ] **Step 7: cargo check + 测试不回归**

Run: `cargo check --manifest-path src-tauri/Cargo.toml --tests`
Expected: 通过、零 warning（删掉 Task 1 给 StagedFile 加的 `#[allow(dead_code)]`）

- [ ] **Step 8: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/src/tools.rs src-tauri/src/lib.rs
git commit -m "feat(attachments): stage_attachments 命令（复制+sha去重+尺寸+kind+文本提取）

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 3: 多模态消息构造 — 引用存储 + 发送时展开 + 预算守卫

**Files:**
- Modify: `src-tauri/src/llm.rs`（`build_body` 改 `Result` + 展开逻辑 + 预算守卫 + helper + 测试）
- Modify: `src-tauri/src/tools.rs`（无；仅类型依赖）
- Test: `llm.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `crate::AttachmentRef`、`crate::mime_from_ext`、`base64`。
- Produces: `llm::user_message_with_attachments(text: &str, attachments: &[AttachmentRef]) -> Value`（历史里存的形态：`content: [{type:"text"},{type:"image_ref"|"video_ref",path},...]`，**不内联 base64**）；`llm::expand_messages_for_send(messages: &[Value]) -> Result<Vec<Value>, String>`（把 image_ref/video_ref 现场读盘 base64 展开、文本内联 part 原样、二进制 note part 原样；预算 >58MB 返 Err）；`build_body -> Result<Value, String>`。

**消息形态约定（Task 4 依赖）：**
- 纯文本用户消息：`{"role":"user","content":"字符串"}`（不变，回归零改动）。
- 带附件用户消息：`{"role":"user","content":[ {"type":"text","text":"..."}, {"type":"image_ref","path":"C:/..."}, {"type":"video_ref","path":"..."}, {"type":"text","text":"[用户上传了 <path>，可用 read/display 处理]"} ]}`。其中：
  - 图片 → `image_ref`；视频 → `video_ref`；
  - 文本类（kind text/markdown/csv 且 stage 时已内联）→ 直接 `{"type":"text","text":<内容>}`；
  - 二进制（pdf/docx/audio/...）→ `{"type":"text","text":"[用户上传了 <path>，可用 read/display 处理]"}`，path 为 staged 绝对路径。

- [ ] **Step 1: 写 user_message_with_attachments 失败测试**

在 `src-tauri/src/llm.rs` 的 `mod tests` 加：

```rust
    use crate::AttachmentRef;

    fn aref(path: &str, kind: &str) -> AttachmentRef {
        AttachmentRef { staged_path: path.into(), kind: kind.into() }
    }

    #[test]
    fn user_message_image_ref_no_inline_base64() {
        let m = user_message_with_attachments("看图", &[aref("C:/a.png", "image")]);
        assert_eq!(m["role"], "user");
        let content = m["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "看图");
        assert_eq!(content[1]["type"], "image_ref");
        assert_eq!(content[1]["path"], "C:/a.png");
        assert!(content[1].get("url").is_none(), "历史里不内联 base64");
    }

    #[test]
    fn user_message_binary_becomes_agent_note() {
        let m = user_message_with_attachments("", &[aref("C:/a.pdf", "pdf")]);
        let note = m["content"].as_array().unwrap().iter()
            .find(|p| p["type"] == "text").unwrap();
        assert!(note["text"].as_str().unwrap().contains("a.pdf"));
        assert!(note["text"].as_str().unwrap().contains("read/display"));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib llm::tests::user_message`
Expected: FAIL（函数未定义）

- [ ] **Step 3: 实现 user_message_with_attachments**

在 `src-tauri/src/llm.rs`（`build_body` 之后）加：

```rust
use crate::AttachmentRef;

/// 文本类 kind：stage 时已内联，直接作为 text part（不再存 ref）。
fn is_inline_text_kind(kind: &str) -> bool {
    matches!(kind, "text" | "markdown" | "csv")
}

/// 组 user 消息：历史里存引用（image_ref/video_ref）或内联文本 part（文本类）/ agent note（二进制）。
/// **不内联 base64** —— build_body 发送时才展开（省内存、控每轮成本）。
pub fn user_message_with_attachments(text: &str, attachments: &[AttachmentRef]) -> Value {
    let mut parts: Vec<Value> = Vec::new();
    if !text.is_empty() {
        parts.push(json!({ "type": "text", "text": text }));
    }
    for a in attachments {
        match a.kind.as_str() {
            "image" => parts.push(json!({ "type": "image_ref", "path": a.staged_path })),
            "video" => parts.push(json!({ "type": "video_ref", "path": a.staged_path })),
            k if is_inline_text_kind(k) => {
                // 文本内容已由 stage 提取；这里读 staged 副本（≤50KB）
                let body = std::fs::read_to_string(&a.staged_path).unwrap_or_default();
                parts.push(json!({ "type": "text", "text": format!("[用户上传了 {}]\n{}", a.staged_path, body) }));
            }
            _ => parts.push(json!({ "type": "text", "text": format!("[用户上传了 {}，可用 read/display 处理]", a.staged_path) })),
        }
    }
    if parts.is_empty() {
        json!({ "role": "user", "content": text })
    } else if parts.len() == 1 && parts[0]["type"] == "text" && text == parts[0]["text"].as_str().unwrap_or("\0") {
        json!({ "role": "user", "content": text })
    } else {
        json!({ "role": "user", "content": parts })
    }
}
```

注：末尾的「单 text part 且就是原文」分支保持纯文本消息为字符串（回归）。顶部 `use crate::AttachmentRef;` 合并到文件 use 区。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib llm::tests::user_message`
Expected: PASS

- [ ] **Step 5: 写 expand_messages_for_send 失败测试（含预算守卫）**

`mod tests` 加：

```rust
    #[test]
    fn expand_plain_string_messages_unchanged() {
        let msgs = vec![json!({"role":"user","content":"hi"})];
        let out = expand_messages_for_send(&msgs).unwrap();
        assert_eq!(out[0]["content"], "hi"); // 纯文本原样，不展开
    }

    #[test]
    fn expand_image_ref_to_data_url() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("cat.png");
        std::fs::write(&p, b"png").unwrap();
        let abs = p.canonicalize().unwrap().to_string_lossy().to_string();
        let msgs = vec![json!({"role":"user","content":[
            {"type":"text","text":"看"},
            {"type":"image_ref","path": abs}
        ]})];
        let out = expand_messages_for_send(&msgs).unwrap();
        let parts = out[0]["content"].as_array().unwrap();
        assert_eq!(parts[1]["type"], "image_url");
        let url = parts[1]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"), "{url}");
    }

    #[test]
    fn expand_budget_guard_rejects_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.png");
        // 59MB 超过 58MB 预算
        std::fs::write(&p, vec![0u8; 59 * 1024 * 1024]).unwrap();
        let abs = p.canonicalize().unwrap().to_string_lossy().to_string();
        let msgs = vec![json!({"role":"user","content":[{"type":"image_ref","path": abs}]})];
        let err = expand_messages_for_send(&msgs).err().unwrap();
        assert!(err.contains("过大"), "{err}");
    }
```

- [ ] **Step 6: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib llm::tests::expand`
Expected: FAIL（函数未定义）

- [ ] **Step 7: 实现 expand_messages_for_send**

`llm.rs` 加：

```rust
use base64::Engine;
const REQUEST_BUDGET_BYTES: u64 = 58 * 1024 * 1024; // 留余量 < 64MB

/// 发送前展开：image_ref/video_ref → 现场读盘 base64 成 image_url/video_url part；
/// text part 原样。累加 base64 体积，超 REQUEST_BUDGET_BYTES 返 Err（build_body → chat-error）。
pub fn expand_messages_for_send(messages: &[Value]) -> Result<Vec<Value>, String> {
    let mut total: u64 = 0;
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    for m in messages {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let content = m.get("content");
        // 仅展开 content 为数组、且含 image_ref/video_ref 的消息；其余原样克隆
        let needs_expand = role == "user"
            && content.and_then(|c| c.as_array()).map(|arr| arr.iter().any(|p|
                p.get("type").and_then(|t| t.as_str()).map(|s| s == "image_ref" || s == "video_ref").unwrap_or(false)
            )).unwrap_or(false);
        if !needs_expand {
            out.push(m.clone());
            continue;
        }
        let arr = content.unwrap().as_array().unwrap();
        let mut new_parts: Vec<Value> = Vec::with_capacity(arr.len());
        for p in arr {
            let t = p.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if t == "image_ref" || t == "video_ref" {
                let path = std::path::PathBuf::from(p["path"].as_str().unwrap_or(""));
                let bytes = std::fs::read(&path).map_err(|e| format!("附件文件已丢失，请重新添加: {}: {e}", path.display()))?;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                total = total.saturating_add(b64.len() as u64);
                if total > REQUEST_BUDGET_BYTES {
                    return Err("历史媒体过大（超 58MB 上限），请开新会话或移除旧图/视频".into());
                }
                let mime = crate::mime_from_ext(&path);
                let url = format!("data:{mime};base64,{b64}");
                let key = if t == "image_ref" { "image_url" } else { "video_url" };
                new_parts.push(json!({ "type": key, key: { "url": url } }));
            } else {
                new_parts.push(p.clone());
            }
        }
        let mut nm = m.clone();
        nm["content"] = Value::Array(new_parts);
        out.push(nm);
    }
    Ok(out)
}
```

- [ ] **Step 8: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib llm::tests::expand`
Expected: 3 个 PASS

- [ ] **Step 9: build_body 改 Result + 接入展开（含回归）**

把 `llm.rs:23-33` 的 `build_body` 改为：

```rust
pub fn build_body(messages: &[Value], cfg: &Config) -> Result<Value, String> {
    let expanded = expand_messages_for_send(messages)?;
    Ok(json!({
        "model": cfg.llm_model,
        "messages": expanded,
        "tools": tools::schemas(),
        "tool_choice": "auto",
        "reasoning_split": true,
        "stream": true,
        "stream_options": { "include_usage": true },
    }))
}
```

把 `HttpRound::round`（`llm.rs:44-62`）里的 `let body = build_body(messages, cfg);` 改为 `let body = build_body(messages, cfg)?;`（Err 自动透传成 `round` 的 Err → `run_turn` emit error）。

更新既有测试 `build_body_has_tools_and_reasoning_split`（`llm.rs:418`）：

```rust
    #[test]
    fn build_body_has_tools_and_reasoning_split() {
        let cfg = Config::default();
        let body = build_body(&[serde_json::json!({"role":"user","content":"hi"})], &cfg).unwrap();
        assert_eq!(body["reasoning_split"], true);
        assert_eq!(body["stream"], true);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"].as_array().unwrap().len(), 5);
    }
```

- [ ] **Step 10: 跑 llm 全量测试不回归**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib llm::tests`
Expected: 全 PASS（含既有 SSE / loop 测试 —— 它们用纯文本消息，expand 原样，不回归）

- [ ] **Step 11: cargo check 全量**

Run: `cargo check --manifest-path src-tauri/Cargo.toml --tests`
Expected: 通过、零 warning

- [ ] **Step 12: Commit**

```bash
git add src-tauri/src/llm.rs
git commit -m "feat(attachments): 多模态消息构造（引用存储+发送展开+58MB 预算守卫）

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 4: Session 接线 — UserMessage 带 attachments + ContextNote 事件 + chat/append_context_note 命令 + 媒体感知窗口化

**Files:**
- Modify: `src-tauri/src/agent.rs`（`SessionEvent` 加 attachments + ContextNote；`handle_event` 组消息；`window_messages` 折叠旧媒体）
- Modify: `src-tauri/src/lib.rs`（`chat` 加 attachments 参；新增 `append_context_note` 命令；注册）
- Test: `agent.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `llm::user_message_with_attachments`、`crate::AttachmentRef`。
- Produces: `SessionEvent::UserMessage { text, attachments }`、`SessionEvent::ContextNote { text }`；命令 `chat(text, attachments: Vec<AttachmentRef>)`、`append_context_note(text)`。

- [ ] **Step 1: 写 ContextNote 失败测试（push 但不跑 turn）**

`agent.rs` 的 `mod tests` 加（复用既有 `ScriptedRound`/`FakeEmitter`/`ctx()`/`cfg()`）：

```rust
    #[tokio::test]
    async fn context_note_pushes_without_running_turn() {
        let round = ScriptedRound { steps: Mutex::new(vec![]) }; // 无步骤：若误跑 turn 会拿"完成"
        let emit = FakeEmitter { content: Mutex::new(String::new()) };
        let mut msgs = vec![json!({"role":"system","content":"你是助手"})];
        let c = cfg(); let x = ctx();
        let res = handle_event(&SessionEvent::ContextNote { text: "[上下文] 拖入了 a.pdf".into() },
            &mut msgs, &x, &c, &round, &emit).await;
        assert!(res.is_none(), "ContextNote 不跑 turn（应返 None，仿 Reset）");
        assert!(msgs.iter().any(|m| m["role"]=="user"
            && m["content"].as_str().unwrap_or("").contains("a.pdf")));
        assert!(emit.content.lock().unwrap().is_empty(), "不应产生任何正文流");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib agent::tests::context_note`
Expected: FAIL（`ContextNote` 变体不存在）

- [ ] **Step 3: 扩 SessionEvent + handle_event**

把 `agent.rs:11-15` 的 `SessionEvent` 改为：

```rust
#[derive(Debug, Clone)]
pub enum SessionEvent {
    UserMessage { text: String, attachments: Vec<crate::AttachmentRef> },
    ContextNote { text: String },
    JobDone(JobOutcome),
    Reset,
}
```

把 `handle_event`（`agent.rs:50-67`）改为：

```rust
pub async fn handle_event<R: LlmRound, E: Emitter>(
    event: &SessionEvent, messages: &mut Vec<Value>, ctx: &ToolsCtx,
    cfg: &Config, round: &R, emitter: &E,
) -> Option<ChatResponse> {
    match event {
        SessionEvent::UserMessage { text, attachments } => {
            let m = if attachments.is_empty() {
                json!({ "role": "user", "content": text })
            } else {
                crate::llm::user_message_with_attachments(text, attachments)
            };
            messages.push(m);
        }
        SessionEvent::ContextNote { text } => {
            messages.push(json!({ "role": "user", "content": text }));
            window_messages(messages);
            return None; // 静默上下文：入历史但不跑 turn（仿 Reset）
        }
        SessionEvent::JobDone(o) => inject_jobdone_message(messages, o),
        SessionEvent::Reset => {
            messages.clear();
            messages.push(json!({ "role": "system", "content": cfg.system_prompt }));
            return None;
        }
    }
    window_messages(messages);
    Some(llm::run_turn(round, emitter, cfg, messages, ctx).await)
}
```

- [ ] **Step 4: 跑测试确认通过 + 既有 UserMessage 测试更新**

既有 `usermessage_then_stop_runs_turn` 用 `SessionEvent::UserMessage { text: "hi".into() }` —— 现在缺 `attachments` 字段，编译错。改为：

```rust
        let res = handle_event(&SessionEvent::UserMessage { text: "hi".into(), attachments: vec![] }, &mut msgs, &x, &c, &round, &emit).await;
```

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib agent::tests`
Expected: 全 PASS（含新 context_note + 更新后的 usermessage + reset + window 测试）

- [ ] **Step 5: window_messages 折叠旧媒体引用（ARCH-4）+ 测试**

把 `agent.rs:28-46` 的 `window_messages` 在折叠 tool 结果的循环里，**额外**折叠旧媒体引用消息。在 `mod tests` 加：

```rust
    #[test]
    fn window_folds_old_image_refs() {
        let mut msgs: Vec<Value> = vec![json!({"role":"system","content":"s"})];
        // 50 条带 image_ref 的 user 消息 + 夹杂普通消息，撑过 40 条阈值
        for i in 0..50 {
            msgs.push(json!({"role":"user","content":[{"type":"image_ref","path":format!("C:/{i}.png")}]}));
        }
        window_messages(&mut msgs);
        // 被折叠的旧 image_ref 消息：content 应不再是含 image_ref 的数组（防每轮重发 base64）
        let still_ref = msgs.iter().filter(|m|
            m["content"].as_array().map(|a| a.iter().any(|p| p["type"]=="image_ref")).unwrap_or(false)
        ).count();
        assert!(still_ref <= 10, "旧 image_ref 应被折叠，剩余 {still_ref}");
    }
```

实现：在 `window_messages` 的 `for m in messages.iter_mut().skip(1)` 循环里，把折叠条件从「只 tool」扩为「tool 或 含媒体引用的 user 消息」：

```rust
pub fn window_messages(messages: &mut Vec<Value>) {
    const MAX_KEEP: usize = 40;
    if messages.len() <= MAX_KEEP { return; }
    let drop_n = messages.len() - MAX_KEEP;
    let mut folded = 0;
    for m in messages.iter_mut().skip(1) {
        if folded >= drop_n { break; }
        let role = m.get("role").and_then(|v| v.as_str());
        let is_tool = role == Some("tool");
        let has_media_ref = m.get("content").and_then(|c| c.as_array())
            .map(|a| a.iter().any(|p| matches!(p["type"].as_str(), Some("image_ref"|"video_ref"))))
            .unwrap_or(false);
        if is_tool {
            let id = m.get("tool_call_id").cloned();
            let mut folded_msg = json!({ "role": "tool", "content": "[旧工具结果已省略]" });
            if let Some(idv) = id { folded_msg["tool_call_id"] = idv; }
            *m = folded_msg;
            folded += 1;
        } else if has_media_ref && role == Some("user") {
            *m = json!({ "role": "user", "content": "[历史中含图片/视频的旧消息已省略]" });
            folded += 1;
        }
    }
}
```

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib agent::tests::window`
Expected: 3 个 window 测试全 PASS（含新 `window_folds_old_image_refs` + 既有 tool 折叠两个不回归）

- [ ] **Step 6: chat 命令加 attachments 参 + append_context_note 命令**

把 `lib.rs:60-65` 的 `chat` 改为：

```rust
#[tauri::command]
async fn chat(text: String, attachments: Vec<crate::AttachmentRef>, app: AppHandle) -> Result<(), String> {
    let tx = app.state::<agent::SessionHandle>().inner().tx.clone();
    tx.send(agent::SessionEvent::UserMessage { text, attachments }).await
        .map_err(|_| "session 已关闭".to_string())
}
```

在 `reset_session` 命令后加：

```rust
/// 静默上下文：往 session 历史追加一条 user 消息但**不触发** turn（右区 drop 用）。
/// driver 私有持有 messages，故命令只能发 ContextNote 事件（见 ARCH-2）。
#[tauri::command]
async fn append_context_note(text: String, app: AppHandle) -> Result<(), String> {
    let tx = app.state::<agent::SessionHandle>().inner().tx.clone();
    tx.send(agent::SessionEvent::ContextNote { text }).await
        .map_err(|_| "session 已关闭".to_string())
}
```

在 `invoke_handler` 的 `generate_handler!`（`lib.rs:492`）加 `append_context_note,`（列在 `reset_session,` 后）。

- [ ] **Step 7: cargo check + 全量测试**

Run: `cargo check --manifest-path src-tauri/Cargo.toml --tests && cargo test --manifest-path src-tauri/Cargo.toml --lib`
Expected: 编译通过、零 warning、全测试 PASS

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/agent.rs src-tauri/src/lib.rs
git commit -m "feat(attachments): SessionEvent 接线（UserMessage 带 attachments + ContextNote）+ 媒体感知窗口化

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 5: 前端 — 附件按钮 + 预览条 + 发送带附件 + 用户气泡附件条带

**Files:**
- Modify: `src/index.html`（composer 加 attach 按钮 + 预览条容器；设置页加「附件大小上限」输入）
- Modify: `src/main.js`（attach 按钮、预览条渲染、submit 带 attachments、addBubble 支持附件条带）
- Verify: `node --check src/main.js` + 手测

**Interfaces:**
- Consumes: `invoke("stage_attachments", {paths})`、`invoke("chat", {text, attachments})`；`AttachmentRef = {staged_path, kind}`。
- Produces: 全局 `let pendingAttachments = []`（待发附件 refs）；`renderAttachmentBar()`；`addBubble(role, text, opts)`。

- [ ] **Step 1: index.html — composer 加 attach 按钮 + 预览条**

在 `src/index.html` 的 `#chat-form`（约 56 行）内，`<button id="mic-btn">` 之后、`<textarea>` 之前加附件按钮，并在 `<form>` 开头加预览条容器。改为：

```html
        <form id="chat-form" class="composer">
          <div id="attach-bar" class="attach-bar" hidden></div>
          <button id="mic-btn" class="mic-btn" type="button" aria-label="语音输入" title="语音输入">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <rect x="9" y="3" width="6" height="11" rx="3" />
              <path d="M5 11a7 7 0 0014 0M12 18v3" />
            </svg>
          </button>
          <button id="attach-btn" class="attach-btn" type="button" aria-label="添加附件" title="添加附件">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <path d="M21.44 11.05l-9.19 9.19a5 5 0 01-7.07-7.07l9.19-9.19a3.5 3.5 0 014.95 4.95l-9.2 9.19a2 2 0 01-2.83-2.83l8.49-8.48" />
            </svg>
          </button>
          <input id="attach-input" type="file" multiple hidden />
          <textarea id="chat-input" placeholder="输入消息，Enter 发送 / Shift+Enter 换行…" rows="1" autofocus></textarea>
          <button type="submit" id="send-btn" aria-label="发送" title="发送">
            <svg viewBox="0 0 24 24" fill="none" aria-hidden="true">
              <path d="M12 19V5M6 11l6-6 6 6" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" />
            </svg>
          </button>
        </form>
```

在设置页「Agent 工作目录」group 之后加一个 group（供 `max_attachment_mb` 可调）：

```html
            <div class="group">
              <div class="group-title">附件</div>
              <label class="row">
                <span>单个附件大小上限（MB；图片恒≤10）</span>
                <input id="cfg-max-attachment-mb" type="number" min="1" max="100" step="1" />
              </label>
            </div>
```

- [ ] **Step 2: main.js — 状态 + 元素 + F 字段**

在 `src/main.js` 顶部状态区（`let lastConfig = null;` 后，约 42 行）加：

```js
let pendingAttachments = []; // [{staged_path, kind, original_name, size}] 待发附件
```

在元素引用区（`const sendBtn = ...` 后）加：

```js
const attachBtn = document.getElementById("attach-btn");
const attachInput = document.getElementById("attach-input");
const attachBar = document.getElementById("attach-bar");
```

在 `F` 对象（约 62-92 行）末尾加：

```js
  maxAttachmentMb: document.getElementById("cfg-max-attachment-mb"),
```

- [ ] **Step 3: main.js — fillForm / readForm 接 max_attachment_mb**

`fillForm`（约 246 行）末尾（`glassOpacityVal.textContent` 后）加：

```js
  F.maxAttachmentMb.value = cfg.max_attachment_mb ?? 30;
```

`readForm`（约 275 行）返回对象里加（与其它字段并列）：

```js
    max_attachment_mb: Number(F.maxAttachmentMb.value) || 30,
```

- [ ] **Step 4: main.js — 预览条渲染 + attach 按钮交互**

在 `addBubble` 函数（约 121 行）**之前**加预览条渲染与附件添加逻辑：

```js
const ATTACH_ICON = { image:"🖼", video:"🎬", audio:"🎵", html:"🌐", pdf:"📄",
  docx:"📝", csv:"📊", markdown:"📑", text:"📃", unsupported:"❓" };

// 把 staged 结果并入待发列表并刷新预览条。
function addStaged(items) {
  for (const it of items) {
    if (!it.ok) { toast((it.error || "添加失败") + (it.original_name ? `：${it.original_name}` : ""), true); continue; }
    pendingAttachments.push(it);
  }
  renderAttachmentBar();
}

function renderAttachmentBar() {
  attachBar.innerHTML = "";
  if (pendingAttachments.length === 0) { attachBar.hidden = true; return; }
  attachBar.hidden = false;
  for (let i = 0; i < pendingAttachments.length; i++) {
    const a = pendingAttachments[i];
    const chip = document.createElement("span");
    chip.className = "attach-chip";
    chip.setAttribute("role", "listitem");
    chip.setAttribute("aria-label", `${a.original_name}，${a.kind}，按删除键移除`);
    chip.tabIndex = 0;
    const mb = (a.size / 1048576).toFixed(a.size > 1048576 ? 1 : 2);
    chip.innerHTML = `<span class="attach-ico">${ATTACH_ICON[a.kind] || "📎"}</span>`
      + `<span class="attach-name"></span><span class="attach-meta">${mb} MB</span>`;
    chip.querySelector(".attach-name").textContent = a.original_name;
    const x = document.createElement("button");
    x.type = "button"; x.className = "attach-x"; x.setAttribute("aria-label", `移除 ${a.original_name}`);
    x.textContent = "✕";
    const idx = i;
    const remove = () => { pendingAttachments.splice(idx, 1); renderAttachmentBar(); };
    x.addEventListener("click", remove);
    chip.addEventListener("keydown", (e) => {
      if (e.key === "Backspace" || e.key === "Delete") { e.preventDefault(); remove(); }
    });
    chip.appendChild(x);
    attachBar.appendChild(chip);
  }
}

// 文件选择 → stage → 入栏
async function stageFiles(filePaths) {
  if (!filePaths || filePaths.length === 0) return;
  try {
    const items = await invoke("stage_attachments", { paths: filePaths });
    addStaged(items);
  } catch (e) { toast("添加附件失败：" + e, true); }
}

if (attachBtn) {
  attachBtn.addEventListener("click", () => attachInput.click());
  attachInput.addEventListener("change", () => {
    const paths = Array.from(attachInput.files || []).map(f => f.path).filter(Boolean);
    stageFiles(paths);
    attachInput.value = "";
  });
}
```

注：`f.path` 是 Tauri 文件输入特有的文件绝对路径字段。Tauri v2 下**文件输入** `.path` 不受 `dragDropEnabled` 影响（本功能 dragDrop 保持默认 true，故 native 模式 `.path` 可用），实测应在手测确认；若个别版本不可用，回落用 Tauri `dialog` 插件 `open({multiple:true})` 取路径（项目已集成 `tauri-plugin-dialog` 2.7.2）。

- [ ] **Step 5: main.js — addBubble 支持附件条带 + submit 带 attachments 发送**

把 `addBubble`（约 121-133 行）改为支持 `opts.attachments`：

```js
function addBubble(role, text, opts) {
  opts = opts || {};
  const wrap = document.createElement("div");
  wrap.className = "bubble " + role;
  const body = document.createElement("div");
  body.className = "bubble-text";
  body.textContent = text;
  wrap.appendChild(body);
  // 附件作为底部条带（文字为主、文件为辅）
  if (opts.attachments && opts.attachments.length) {
    const strip = document.createElement("div");
    strip.className = "attach-strip";
    for (const a of opts.attachments) {
      const chip = document.createElement("span");
      chip.className = "attach-chip";
      chip.innerHTML = `<span class="attach-ico">${ATTACH_ICON[a.kind] || "📎"}</span><span class="attach-name"></span>`;
      chip.querySelector(".attach-name").textContent = a.original_name;
      strip.appendChild(chip);
    }
    wrap.appendChild(strip);
  }
  list.appendChild(wrap);
  scrollBottom();
  return wrap;
}
```

把 submit handler（约 578-596 行）改为带 attachments：

```js
form.addEventListener("submit", async (e) => {
  e.preventDefault();
  if (micState !== "idle" || chatBusy) return;
  const text = input.value.trim();
  const atts = pendingAttachments.slice();
  if (!text && atts.length === 0) return;
  input.value = "";
  input.style.height = "auto";
  chatBusy = true;
  sendBtn.disabled = true;
  addBubble("user", text, { attachments: atts });
  pendingAttachments = [];
  renderAttachmentBar();
  try {
    await invoke("chat", { text, attachments: atts.map(a => ({ staged_path: a.staged_path, kind: a.kind })) });
  } catch (err) {
    addBubble("assistant", "发送失败: " + err);
    chatBusy = false;
    sendBtn.disabled = false;
  }
});
```

- [ ] **Step 6: node --check**

Run: `node --check src/main.js`
Expected: 无输出（语法 OK）

- [ ] **Step 7: 手测（dev server，右键 Reload 刷前端）**

启动 `npm run tauri dev`，在窗口里：
- 点 📎 → 选 1 张图 + 1 个 txt → 预览条出现 2 个 chip（图标+名+大小）；
- chip 上按 Backspace → 移除；✕ 点击 → 移除；
- 输入文字 → 发送 → 用户气泡文字下方出现附件条带；
- 选一个 >30MB 文件 → toast「文件过大…」不入栏。

- [ ] **Step 8: Commit**

```bash
git add src/index.html src/main.js
git commit -m "feat(attachments): 附件按钮 + 预览条 + 发送带 attachments + 用户气泡附件条带

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 6: 前端 — 双 dropzone + 拖放状态机 + 本地渲染入口 + a11y + 空消息区提示

> ### ⚠️ Task 6 Pre-Flight 修订（2026-07-25）—— Step 2 整体替换为 Native Tauri 拖放
>
> 原 Step 2 的 HTML5 `bindDropzone`（dragenter/dragover/drop）在 Tauri v2 **不可用**：默认 `dragDropEnabled:true` 下 webview 的 HTML5 drop 事件根本不触发（OS 级拖放被 Tauri 拦截走 `WindowEvent::DragDrop`）；而若关掉 `dragDropEnabled` 又会丢失 `f.path`（浏览器安全限制，Tauri 仅在 native 模式注入 path）。二者不可兼得（见 [Tauri v2 config](https://v2.tauri.app/reference/config/)、[issue #14373](https://github.com/tauri-apps/tauri/issues/14373)）。
>
> **已定方案（用户选）：Native DnD + 落点路由** —— 保留真实 `.path`（复制原文件、零字节重传、与 ARCH-1 媒体引用一致）。dragDropEnabled 维持默认，**不改 tauri.conf.json**。Tauri 2.11.5。下方「Step 2 替换版」为准；原 Step 2 的 HTML5 bindDropzone/setupDropzones 代码**作废，勿实现**。
>
> **Step 2 替换版 —— lib.rs（Rust 转发 native 拖放）**：在 `tauri::Builder::default()` 链（`lib.rs:404` 起，`.setup`/`.invoke_handler` 之间）加：
>
> ```rust
> .on_window_event(|window, event| {
>     if let tauri::WindowEvent::DragDrop(drag) = event {
>         match drag {
>             tauri::DragDropEvent::Enter { position, .. }
>             | tauri::DragDropEvent::Over { position } => {
>                 let _ = window.emit("dnd-hover", json!({ "x": position.x, "y": position.y }));
>             }
>             tauri::DragDropEvent::Leave => {
>                 let _ = window.emit("dnd-hover", Value::Null);
>             }
>             tauri::DragDropEvent::Drop { paths, position } => {
>                 let ps: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
>                 let _ = window.emit("dnd-drop", json!({ "paths": ps, "x": position.x, "y": position.y }));
>             }
>             _ => {}
>         }
>     }
> })
> ```
>
> 需 `use tauri::Emitter;`（v2 `emit` 在 trait 上）；`json!`/`Value` 若 lib.rs 顶部无则 `use serde_json::{json, Value};`。`DragDropEvent` 变体名/字段以编译器报错为准微调（Enter 带.paths，Over 不带）。
>
> **Step 2 替换版 —— main.js（前端按落点路由）**：复用既有 `listen`（`main.js:7`）；`elementFromPoint` + `devicePixelRatio` 缩放（native 给的是物理像素）。文件末尾事件绑定区加：
>
> ```js
> // 高亮光标所在拖放区（Over/Enter 实时；Leave/Drop 清除）
> function highlightDropZone(x, y) {
>   form.classList.remove("dropzone-attach");
>   list.classList.remove("dropzone-render");
>   if (x == null) return;
>   const el = document.elementFromPoint(x, y);
>   if (form.contains(el)) form.classList.add("dropzone-attach");
>   else if (list.contains(el)) list.classList.add("dropzone-render");
> }
>
> // 消息区 drop：立刻本地渲染 + 静默上下文（不发送）
> async function renderDropped(paths) {
>   let items;
>   try { items = await invoke("stage_attachments", { paths }); }
>   catch (e) { toast("打开失败：" + e, true); return; }
>   for (const it of items) {
>     if (!it.ok) { toast((it.error || "打开失败") + (it.original_name ? `：${it.original_name}` : ""), true); continue; }
>     const wrap = addFileBubble(it);
>     renderStagedInto(wrap, it);
>     invoke("append_context_note", { text: `[上下文] 用户将 ${it.staged_path}（${it.kind}）拖入渲染区查看/编辑` })
>       .catch(() => {});
>   }
> }
>
> function setupDropzones() {
>   const dpr = window.devicePixelRatio || 1;
>   listen("dnd-hover", ({ payload }) => {
>     if (!payload) { highlightDropZone(null, null); return; }
>     highlightDropZone(payload.x / dpr, payload.y / dpr);
>   });
>   listen("dnd-drop", ({ payload }) => {
>     highlightDropZone(null, null);
>     const el = document.elementFromPoint(payload.x / dpr, payload.y / dpr);
>     const paths = payload.paths || [];
>     if (!paths.length) return;
>     if (form.contains(el)) stageFiles(paths);          // 附件：复用 Task 5 stageFiles
>     else if (list.contains(el)) renderDropped(paths);  // 本地渲染 + 静默上下文
>   });
>   form.setAttribute("role", "group");
>   form.setAttribute("aria-label", "输入框区：拖放文件作为附件发送");
>   list.setAttribute("role", "region");
>   list.setAttribute("aria-label", "消息区：拖放文件本地查看/编辑");
> }
> ```
>
> 在初始化区（调用 `setupAgentEvents()` 的地方）加 `setupDropzones();`。**Step 4 的 CSS（`form.dropzone-attach, #messages.dropzone-render { outline/background }`）不变** —— 现由 `highlightDropZone` 在 native Over 期间 toggle 这两个 class，原 class 选择器照常生效。

**Files:**
- Modify: `src/main.js`（dropzone 监听、dragover 计数器防抖、本地渲染入口 `addFileBubble`、空消息区提示）
- Modify: `src/styles.css`（dropzone 高亮、attach-bar/chip、本地气泡「本地」标签）—— 仅最小必要样式
- Verify: `node --check src/main.js` + 手测

**Interfaces:**
- Consumes: `invoke("stage_attachments")`、`invoke("append_context_note", {text})`、`renderMediaCard`/`renderDocCard`/`renderEditCard`、`convertFileSrc`。
- Produces: `setupDropzones()`；`addFileBubble(staged)`（复用 addBubble 骨架 + `.bubble-media` 容器 + 「本地」标签）。

- [ ] **Step 1: main.js — 本地渲染气泡 + 渲染入口**

在 `renderMediaCard`（约 642 行）**之前**加：

```js
// 右区 drop：用户主动打开的文件 —— 新建一个带 .bubble-media 容器的用户侧气泡，
// 复用 renderMediaCard/renderDocCard/renderEditCard（契约不变），加「本地」标签与 agent 气泡区分（D1）。
function addFileBubble(staged) {
  const wrap = document.createElement("div");
  wrap.className = "bubble user file-bubble";
  const tag = document.createElement("div");
  tag.className = "local-tag"; tag.textContent = "本地";
  wrap.appendChild(tag);
  const media = document.createElement("div");
  media.className = "bubble-media";
  wrap.appendChild(media);
  list.appendChild(wrap);
  scrollBottom();
  return wrap;
}

// 按 staged.kind 把卡片渲染进给定 wrap（复用既有渲染函数）。
function renderStagedInto(wrap, staged) {
  const kind = staged.kind;
  const j = { display: true, path: staged.staged_path, kind, caption: staged.original_name };
  if (kind === "image" || kind === "video" || kind === "audio") {
    renderMediaCard(wrap, j);
  } else if (kind === "markdown" || kind === "text") {
    // 文本/markdown：可编辑卡（编辑存到 staged 副本，D2 非破坏性）
    const content = staged.text != null ? staged.text : "";
    renderEditCard(wrap, { edit: true, path: staged.staged_path, kind, content, caption: staged.original_name });
  } else {
    renderDocCard(wrap, j);
  }
}
```

注：`renderDocCard`/`renderEditCard`/`renderMediaCard` 既有签名不动；它们 `wrap.querySelector(".bubble-media")` —— `addFileBubble` 已提供该容器。

- [ ] **Step 2: ~~main.js — 拖放状态机（计数器防抖）~~ [❌ 作废，见上方「Task 6 Pre-Flight 修订」Step 2 替换版 —— Native Tauri 拖放；下方 HTML5 bindDropzone/setupDropzones 代码勿实现]**

在 `setupAgentEvents`（约 965 行）**之后**或文件末尾事件绑定区加：

```js
// dropzone 计数器防抖：dragenter +1 / dragleave -1，归 0 取消高亮（避免穿越子元素闪烁）。
function bindDropzone(el, highlightClass, onFiles) {
  let depth = 0;
  el.addEventListener("dragenter", (e) => {
    if (!e.dataTransfer || !Array.from(e.dataTransfer.types || []).includes("Files")) return;
    e.preventDefault();
    depth++; el.classList.add(highlightClass);
  });
  el.addEventListener("dragover", (e) => {
    if (!e.dataTransfer || !Array.from(e.dataTransfer.types || []).includes("Files")) return;
    e.preventDefault(); e.dataTransfer.dropEffect = "copy";
  });
  el.addEventListener("dragleave", (e) => {
    if (!Array.from(e.dataTransfer?.types || []).includes("Files")) return;
    depth = Math.max(0, depth - 1);
    if (depth === 0) el.classList.remove(highlightClass);
  });
  el.addEventListener("drop", async (e) => {
    if (!e.dataTransfer) return;
    e.preventDefault(); depth = 0; el.classList.remove(highlightClass);
    const paths = Array.from(e.dataTransfer.files || []).map(f => f.path).filter(Boolean);
    if (paths.length) await onFiles(paths);
  });
}

function setupDropzones() {
  // 输入框区 = 加附件随消息发
  bindDropzone(form, "dropzone-attach", async (paths) => {
    await stageFiles(paths); // 复用 Task 5 的 stageFiles（入待发栏）
  });
  // 消息区 = 立刻本地渲染/编辑 + 静默上下文（不发送）
  bindDropzone(list, "dropzone-render", async (paths) => {
    let items;
    try { items = await invoke("stage_attachments", { paths }); }
    catch (e) { toast("打开失败：" + e, true); return; }
    for (const it of items) {
      if (!it.ok) { toast((it.error || "打开失败") + (it.original_name ? `：${it.original_name}` : ""), true); continue; }
      const wrap = addFileBubble(it);
      renderStagedInto(wrap, it);
      // 静默上下文：告知 agent，但不触发模型（D3 不渲染可见气泡）
      invoke("append_context_note", { text: `[上下文] 用户将 ${it.staged_path}（${it.kind}）拖入渲染区查看/编辑` })
        .catch(() => {});
    }
  });
}
```

在初始化区（调用 `setupAgentEvents()` 的地方，文件末尾）加 `setupDropzones();`。给两个区加无障碍标签：

```js
form.setAttribute("role", "group");
form.setAttribute("aria-label", "输入框区：拖放文件作为附件发送");
list.setAttribute("role", "region");
list.setAttribute("aria-label", "消息区：拖放文件本地查看/编辑");
```

- [ ] **Step 3: main.js — 空消息区拖放提示（发现性）**

在 `setupDropzones` 里，依据 `list` 是否为空显示/隐藏提示。加一个轻量渲染：

```js
function renderEmptyHint() {
  const id = "empty-drop-hint";
  let hint = document.getElementById(id);
  const empty = list.children.length === 0;
  if (empty) {
    if (!hint) {
      hint = document.createElement("div");
      hint.id = id; hint.className = "empty-drop-hint";
      hint.innerHTML = "拖放文件到此查看，或拖到输入框作为附件发送";
      list.parentElement.insertBefore(hint, list);
    }
  } else if (hint) {
    hint.remove();
  }
}
```

在 `chat-reset` 监听（约 1053 行）的 `list.innerHTML = "";` 后、`addBubble("assistant","（已重置）")` 前调用 `renderEmptyHint()`；并在首条消息出现后隐藏 —— 在 `chat-turn-start` 监听里 `list.appendChild(wrap)` 后加 `renderEmptyHint()`。在 `setupDropzones` 末尾也调一次 `renderEmptyHint()`。

- [ ] **Step 4: styles.css — 最小必要样式**

在 `src/styles.css` 末尾加（复用既有面板/气泡 token；不引入新设计语言）：

```css
/* 附件预览条 + chip */
.attach-bar { display: flex; flex-wrap: wrap; gap: 6px; padding: 6px 8px 0; }
.attach-bar[hidden] { display: none; }
.attach-chip { display: inline-flex; align-items: center; gap: 6px; padding: 4px 8px;
  border-radius: 14px; background: var(--glass-bg, rgba(255,255,255,0.08));
  border: 1px solid var(--glass-border, rgba(255,255,255,0.14)); font-size: 12px; }
.attach-chip .attach-name { max-width: 140px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.attach-chip .attach-meta { opacity: 0.6; }
.attach-chip .attach-x { background: none; border: none; color: inherit; cursor: pointer;
  opacity: 0.6; padding: 0 2px; line-height: 1; }
.attach-chip .attach-x:hover { opacity: 1; }
.attach-btn { display: flex; align-items: center; justify-content: center; background: none;
  border: none; color: var(--glass-fg, currentColor); cursor: pointer; opacity: 0.7; padding: 0 6px; }
.attach-btn:hover { opacity: 1; }
/* 已发送用户气泡内的附件条带（无删除按钮） */
.attach-strip { display: flex; flex-wrap: wrap; gap: 6px; margin-top: 6px; opacity: 0.85; }

/* dropzone 高亮（对比度满足 4.5:1：实线 + 不透明底） */
form.dropzone-attach, #messages.dropzone-render { outline: 2px solid #4aa3ff; outline-offset: -4px;
  background: rgba(74,163,255,0.10); }

/* 右区本地渲染气泡 + 本地标签（与 agent 气泡区分，D1） */
.file-bubble .local-tag { display: inline-block; font-size: 11px; opacity: 0.7;
  padding: 1px 6px; border-radius: 8px; background: rgba(255,255,255,0.10); margin-bottom: 4px; }

/* 空消息区拖放提示 */
.empty-drop-hint { text-align: center; opacity: 0.4; padding: 24px; font-size: 13px; pointer-events: none; }
```

- [ ] **Step 5: node --check**

Run: `node --check src/main.js`
Expected: 无输出（语法 OK）

- [ ] **Step 6: 手测（dev server，右键 Reload）**

- 从资源管理器拖一张图到**输入框区** → 输入框高亮 → 松手 → 入待发栏 chip；输入文字发送 → 模型描述图片内容（验证 image_url 多模态）；
- 拖一个 pdf 到**消息区** → 消息区高亮 → 松手 → 立刻出现「本地」标签 + pdf 卡（pdf.js 渲染），**不**触发模型；再发一条文字消息 → agent 回复体现它知道你看过 pdf（静默上下文生效）；
- 拖一个 .md 到消息区 → 编辑卡打开（textarea + 预览），改后保存 → 落到 staged 副本（D2，原文件不动）；
- 鼠标在 dropzone 内子元素间移动 → 高亮**不闪烁**（计数器防抖）；
- 拖一个文件夹到消息区 → toast「暂不支持文件夹」；
- chatBusy（agent 回复中）时拖文件到消息区 → 仍能本地渲染，上下文 note 在当前轮结束后入历史（不污染在途 messages）；
- 窗口拉窄 → 附件条换行（不撑爆 composer）；
- Tab 到 📎 → Enter 打开选择器；聚焦 chip → Backspace 删除。

- [ ] **Step 7: Commit**

```bash
git add src/main.js src/styles.css
git commit -m "feat(attachments): 双 dropzone（输入框=附件/消息区=本地渲染）+ dragleave 防抖 + 本地气泡 + a11y + 空消息区提示

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Self-Review（写完后自检，已修订进上文）

**1. Spec coverage：**
- 附件按钮（Task 5 ✓）、拖到输入框=附件（Task 6 ✓）、拖到消息区=本地渲染+静默上下文（Task 6 ✓）。
- 混合+文本内联附件语义（Task 3 `user_message_with_attachments` ✓）、复制进工作区（Task 2 ✓）、按拖放目标分区（Task 6 ✓）、超限拒绝（Task 2 ✓）。
- design-review：信息架构（T5 附件条带在文字下方 ✓）、交互状态表（T5 chip 转圈/标红、T6 高亮/提示）、拖放状态机（T6 dragleave 计数器 ✓）、发现性（T6 空消息区提示 ✓）、a11y（T5 chip ARIA+键盘、T6 dropzone role ✓）、响应式（T6 attach-bar flex-wrap ✓）、D1 本地标签（T6 ✓）、D2 编辑存副本（T6 renderEditCard 用 staged_path ✓）、D3 history-only（T4 ContextNote 不跑 turn + T6 不渲染可见气泡 ✓）、D4 忙时排队（T4 channel 自动 ✓）、D5 空态不可见（T5 renderAttachmentBar ✓）。
- eng-review：ARCH-1 引用存储+发送展开+预算守卫（Task 3 ✓）、ARCH-2 ContextNote 是 SessionEvent（Task 4 ✓）、ARCH-3 去重（Task 2 sha ✓）、ARCH-4 媒体感知窗口（Task 4 ✓）、CQ-1 file_kind DRY（Task 1 ✓）、CQ-2 base64 0.22（Task 3 用 Engine API ✓）、CQ-3 复用 addBubble 骨架（Task 6 addFileBubble ✓）、F1 预算守卫（Task 3 ✓）、F2 文件丢失（Task 3 expand 读盘 Err ✓）、F3 文件夹拒收（Task 2 stage_one ✓）。

**2. Placeholder scan：** 无 TBD/TODO 占位；所有 step 含完整代码或精确命令。

**3. Type consistency：** `AttachmentRef{staged_path,kind}`（Task 1 定义）→ Task 3/4/5/6 一致使用；`StagedFile{ok,staged_path,kind,original_name,size,text,error}`（Task 1）→ Task 2 产出 / Task 5-6 消费一致；`file_kind`/`file_kind_str`（Task 1）→ Task 2 stage_one 一致；`SessionEvent::UserMessage{text,attachments}`（Task 4）与 lib.rs chat 一致；`build_body -> Result`（Task 3）与 HttpRound + 测试一致。

---

## 执行选择

Plan complete and saved to `docs/superpowers/plans/2026-07-25-attachments-drag-drop.md`. Two execution options:

1. **Subagent-Driven (recommended)** — 每个 Task 派一个全新 subagent 实现，任务间 review（spec 合规 + 代码质量），最后一轮整支 review。快、上下文不污染。
2. **Inline Execution** — 本会话内按 executing-plans 批量执行，带 checkpoint review。

选哪个？
