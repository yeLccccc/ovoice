# 文档文本抽取 Implementation Plan

> **For agentic workers:** 本特性改动面小（1 新模块 + 1 处接线），controller 直接实现 + in-loop 验证（`cargo check --tests` / `cargo test --lib`），不派 blind subagent。

**Goal:** 让用户附件里的 PDF / DOCX 把文字抽出来内联给 MiniMax（M3 无原生文档输入，当前只给模型一句指针，看不到内容）。

**Architecture:** M3 ChatCompletion 只吃 text/image/video（无 file/document 通道）。复用已有「文本类在 `user_message_with_attachments` 构建消息时读 staged 文件、作 text part 内联」的套路：新增 `extract::doc_text(path, kind)` 纯逻辑抽取器，在 `llm.rs` 给 `pdf`/`docx` 加一个分支调它，抽到的文本作 text part。该用户消息只在发送时构建一次，抽出后作为 text part 存进历史，后续 tool loop 直接复用、不重复抽取（`expand_messages_for_send` 只重读 image/video_ref）。`AttachmentRef` / `StagedFile` 契约**不变**。

**Tech Stack:** 纯 Rust —— DOCX 用 `zip`+`quick-xml`（解 `word/document.xml` 走 `<w:t>`）；PDF 用 `pdf-extract`（无原生 dll，避开 Windows 打包坑）。

## Global Constraints

- 决策（已与用户确认）：**格式范围 = PDF + DOCX**；csv 已能内联不动；legacy `.doc`/`.ppt`/`.xlsx` 仍 Unsupported。
- 决策（已与用户确认）：**超长文本 = 截断+标记**，默认 `DOC_TEXT_MAX = 200KB`，加 `[…已截断…]` 标记。
- 纯 Rust 依赖，不得引入需随包的原生库（pdfium 等）。
- dev server 在跑、占 exe：用 `cargo check --tests --manifest-path src-tauri/Cargo.toml` 与 `cargo test --lib`，**不用 `cargo build`**（见 [[ovoice-dev-server-cargo-lock]]）。
- agent 工具数不变（仍 5），`llm.rs` tools.len()==5 断言不动（见 [[ovoice-tool-count-cascade]]）。
- 截断必须在 UTF-8 字符边界切（用 `char_indices`），不得 panic。
- 抽取失败（损坏/加密/非预期）必须优雅回落到现有的指针 note，不得让整条消息失败。

## File Structure

- **`src-tauri/src/extract.rs`**（新）：`doc_text(path, kind) -> Option<String>` + 内部 `docx_text`/`docx_xml_to_text`/`pdf_text`。纯逻辑、可离线测。
- **`src-tauri/src/lib.rs`**：`mod extract;` 声明（挂到既有 mod 区）。
- **`src-tauri/src/llm.rs`**：`user_message_with_attachments` 加 `pdf|docx` 分支 + `DOC_TEXT_MAX` 常量 + `cap_doc_text` 辅助。
- **`src-tauri/Cargo.toml`** / **`Cargo.lock`**：加 `zip`、`quick-xml`、`pdf-extract`。

## 任务

### Task 1: 抽取模块 `extract.rs`（含 hermetic 测试）

**接口（产给 T2）：**
```rust
pub fn doc_text(path: &Path, kind: &str) -> Option<String>
// kind ∈ {"pdf","docx"} 抽纯文本（不截断）；其余或失败返 None。
```

- 加依赖（`cargo add` 让 resolver 选最新兼容；手改 Cargo.toml 三行亦可）：`zip`、`quick-xml`、`pdf-extract`。
- `docx_text`：`zip::ZipArchive` 开文件 → `by_name("word/document.xml")` → 读到 String。
- `docx_xml_to_text`：`quick_xml::Reader` 走读；遇 `w:t` 的 Start/Empty 置 `in_t`、Text 累加 `unescape()`；遇 `w:p` 在非空时 push `\n`；`w:tab`→` `；EOF/end。trim 收尾。
- `pdf_text`：`pdf_extract::extract_text(path).ok()`（失败/损坏→None）。
- `lib.rs`：`mod extract;`。
- 测试（`#[cfg(test)]` 在 extract.rs 内）：
  - `docx_extracts_minimal_docx`：in-test 用 `zip::ZipWriter` 写一个只含 `word/document.xml`（`<w:p><w:t>Hello</w:t></w:p><w:p><w:t>World</w:t></w:p>`）的 zip 到 tempfile；断言 `doc_text(path,"docx")` 含 "Hello" 且含换行。
  - `doc_text_unknown_kind_returns_none`：`doc_text(path,"txt")` == None（路由）。
  - `doc_text_missing_file_returns_none`：不存在路径 → None。
  - PDF happy-path **不写单测**（in-test 造合法 PDF 易 flaky；交手测）。PDF 回落路径由 T2 的 llm 测试覆盖（kind="pdf" 指向非 PDF 文件 → 抽取失败 → 回落 note）。

### Task 2: 接线 `llm.rs`（含测试）

- 常量 `DOC_TEXT_MAX: usize = 200 * 1024;`
- `cap_doc_text(t: &str) -> String`：≤ 上限原样；否则在 ≤上限的字符边界切，附 `[…已截断，原文约 N 字，仅取前 M 字…]`。
- `user_message_with_attachments` 的 match 加分支（置于 `image`/`video` 之后、`is_inline_text_kind` 守卫之前）：
  ```rust
  "pdf" | "docx" => {
      match crate::extract::doc_text(std::path::Path::new(&a.staged_path), k) {
          Some(t) if !t.trim().is_empty() => {
              parts.push(json!({ "type": "text", "text": format!("[用户上传了 {} ({})]\n{}", a.staged_path, k, cap_doc_text(&t)) }));
          }
          _ => parts.push(json!({ "type": "text", "text": format!("[用户上传了 {} ({})，无法抽取文本，可用 read/display 处理]", a.staged_path, k) })),
      }
  }
  ```
  注意：`k` 在该分支来自 match 绑定（`a.kind.as_str()` 的 match），可直接用。
- 测试（llm.rs 现有 `#[cfg(test)]`）：
  - `cap_doc_text_under_limit_unchanged`：短串原样。
  - `cap_doc_text_over_limit_truncates_with_marker`：构造 250KB 串 → 长度 ≤ 上限+标记开销、含 "已截断"。
  - `user_message_docx_inlines_extracted_text`：复用 T1 的 docx tempfile 思路（或抽公共 helper）写个含已知文本的 docx，`aref(path,"docx")` → 断言 content 含已知文本 + `"(docx)"`。
  - `user_message_pdf_falls_back_when_extraction_fails`：`aref(一个非pdf文件路径,"pdf")` → 断言 content 含 "无法抽取文本"（无需真 PDF）。

## 验证

1. `cargo check --tests --manifest-path src-tauri/Cargo.toml` → 零 warning。
2. `cargo test --lib --manifest-path src-tauri/Cargo.toml` → 全绿（含新测试）。
3. **手测（需重启 dev server 拾取新依赖）**：拖/选一个 .docx 与 .pdf 进对话发送，让 agent 复述文档内容 → 应能说出内容（而非「我用 read 处理」）。再试一个超大 PDF → 模型收到截断标记仍能答前半。加密/损坏 PDF → 回落 note，不崩。
