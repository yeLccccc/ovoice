# attach 工具实现计划（P-2026-005）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增 `attach(path, caption="")` 工具，让助手主动把图片/pdf/docx 纳入上下文——pdf/docx 当轮以 tool result 文本可见，图片下一轮以系统注入的带图 user 消息可见（复用用户拖图管道）。

**Architecture:** 单工具按 `file_kind` 分流。pdf/docx 走 `extract::doc_text` + 截断 → tool result 字符串（当轮）。image 走 `stage_one` 落盘 + 经 `ToolsCtx.session_tx` 发 `SessionEvent::InjectAttachment` 信号 → driver 在 turn N 结束后构造带图 user 消息（`HistoryEvent::user`）→ 触发 turn N+1（M3 在 user role 看见图）。driver `select` 循环零改动（`rx.recv()` 天然接收新 variant）；history 层零改动（复用 `kind=user`）；改动集中在 `handle_event`（加臂 + `triggers_turn`）+ `ToolsCtx`（加 `session_tx`）。

**Tech Stack:** Rust（tokio mpsc、serde_json）、Tauri 2（emit）、原生 JS 前端。

## Global Constraints

- **分支**：`feat/attach-file`（基于 master `a29c4d3`），严禁直接落 master；merge `--no-ff`。提交信息以 `Co-Authored-By: Claude <noreply@anthropic.com>` 结尾。
- **dev server 锁 exe**（[[ovoice-dev-server-cargo-lock]]）：验证只用 `cargo check --tests --manifest-path src-tauri/Cargo.toml` / `cargo test --lib --manifest-path src-tauri/Cargo.toml` / `node --check src/main.js`，**不** `cargo build/run`（debug）。
- **工具计数级联**（[[ovoice-tool-count-cascade]]）：工具数 7→8，两处断言必须同步——`tools.rs` 的 `schemas_has_seven_tools`（约 line 772）+ `llm.rs` 的 `build_body_has_tools_and_reasoning_split`（`tools.len()==7`，约 line 681）。
- **live vs history 双路径**（[[ovoice-live-vs-history-render-paths]]）：前端图卡渲染改动须同时覆盖 live（`setupAgentEvents`）与 history（`buildHistoryBubbles`）。
- **pinned 每轮重读**（[[ovoice-pinned-files-per-turn-reload]]）：改 `defaults/AGENT.md` 下轮即生效，不必重启 dev。
- **数据保留 D2**：`InjectAttachment` 落 history 为 `kind=user` 带 attachments，append-only 只增不删。
- **API 约束**：`image_url` 只在 user role 可靠；tool result（role:tool）带图不被识别 → 图片必须经系统注入 user 消息（下轮可见）；pdf/docx 文本走 tool result（当轮可见，文本在 role:tool 合法）。
- **不冒充用户**：系统注入的 user 文本必须中性（`[系统：助手通过 attach 请求纳入以下图片]`），不能写成指令。
- **工具名**：`attach`（JSON schema name），Rust 函数 `tool_attach`。
- **.doc 剔除**：旧 OLE 二进制，报错引导转 .docx；txt/md/csv 引导用 read。

---

## File Structure

| 文件 | 责任 | 本计划改动 |
|---|---|---|
| `src-tauri/src/tools.rs` | 工具定义 + dispatch + schema + ToolsCtx | 新 `tool_attach`；dispatch 加臂；schema 加 attach；`ToolsCtx` 加 `session_tx` 字段；工具数断言 7→8 |
| `src-tauri/src/llm.rs` | run_turn 工具循环 + 消息构造 | `cap_doc_text`/`DOC_TEXT_MAX` 设 `pub(crate)`（供 tool_attach 复用）；`build_body` 断言 7→8 |
| `src-tauri/src/agent.rs` | driver session 事件循环 | `SessionEvent::InjectAttachment` variant；`handle_event` 臂映射成 `HistoryEvent::user` + `triggers_turn`；`run_one` emit `injected-attachment`；`spawn_session` 给 ctx 注入 `session_tx` |
| `src-tauri/src/extract.rs` | pdf/docx 文本抽取 | **零改动**（`doc_text` 已就绪） |
| `src-tauri/src/history.rs` | history 事件 + 单写 writer | **零改动**（`HistoryEvent::user` 已支持 attachments） |
| `src-tauri/src/lib.rs` | Tauri 命令 + AppEmitter | **零改动**（emit 直接在 agent.rs 用 `TauriEmitter::emit`） |
| `src/defaults/AGENT.md` | agent 工具说明（pinned） | 加 `attach` 工具段（含 image 下轮见的行为告知） |
| `src/main.js` | 前端渲染 | `setupAgentEvents` 加 `listen("injected-attachment")`；`buildHistoryBubbles` 让 image attachment 渲染完整图卡 |
| `src/styles.css` | 样式 | **零改动**（复用 `.bubble-media`/`.media-card`） |

---

## Task 1: tool_attach 文本路径（pdf/docx）+ 拒绝路径 + schema + 工具数断言

**Files:**
- Modify: `src-tauri/src/tools.rs`（新 `tool_attach` + dispatch 臂 + schema + 断言测试）
- Modify: `src-tauri/src/llm.rs`（`cap_doc_text`/`DOC_TEXT_MAX` 设 `pub(crate)`；`build_body` 断言 7→8）
- Test: `src-tauri/src/tools.rs`（`#[cfg(test)] mod tests`）

**Interfaces:**
- Consumes: `crate::file_kind` / `crate::file_kind_str` / `crate::FileKind`（lib.rs，已存在）；`crate::extract::doc_text`（extract.rs:17，已存在）；`crate::llm::cap_doc_text`（本 task 设 pub(crate)）；`resolve_path`（tools.rs:9）
- Produces: `pub async fn tool_attach(args: &Value, ctx: &ToolsCtx) -> String`（Task 4 给 image 分支补 `ctx.session_tx` 用法；本 task image 臂先占位）；schema name `"attach"`；dispatch 臂 `"attach" => tool_attach(&args, ctx).await`

**范围说明**：本 task 只做 pdf/docx + 拒绝路径（不依赖信号通道，独立可测）。image 分支占位返回 `"image 路径尚未实现（Task 4）"`，Task 4 接通。

- [ ] **Step 1.1: 把 `cap_doc_text` / `DOC_TEXT_MAX` 设为 `pub(crate)`**

`src-tauri/src/llm.rs` 约第 47 行：
```rust
pub(crate) const DOC_TEXT_MAX: usize = 200 * 1024;
```
约第 50 行：
```rust
pub(crate) fn cap_doc_text(t: &str) -> String {
```
（仅去掉 `fn`/`const` 前的可见性：加 `pub(crate)`。函数体不变。）

- [ ] **Step 1.2: 写失败测试（tools.rs tests 段末尾追加）**

```rust
    // ─── tool_attach：pdf/docx 抽文本 + 拒绝路径 ───
    #[tokio::test]
    async fn attach_docx_extracts_text_into_result() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().to_path_buf();
        let p = crate::extract::build_minimal_docx(&["季度营收", "同比增长"]);
        // build_minimal_docx 给绝对路径；复制进 workspace 让 resolve_path 命中
        let dest = ws.join("report.docx");
        std::fs::copy(&p, &dest).unwrap();
        let _ = std::fs::remove_file(&p);
        let ctx = ToolsCtx::foreground(ws, "cn".into());
        let r = tool_attach(&serde_json::json!({"path":"report.docx"}), &ctx).await;
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["attached"], true, "docx 应成功: {r}");
        assert_eq!(v["kind"], "docx");
        assert!(v["text"].as_str().unwrap().contains("季度营收"), "应内联抽取文本: {r}");
        assert!(v["text"].as_str().unwrap().contains("同比增长"));
    }

    #[tokio::test]
    async fn attach_doc_rejects_with_docx_hint() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("old.doc"), b"ole-bytes").unwrap();
        let ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        let r = tool_attach(&serde_json::json!({"path":"old.doc"}), &ctx).await;
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["attached"], false);
        assert!(v["error"].as_str().unwrap().contains(".doc"), "应点名 .doc: {r}");
        assert!(v["error"].as_str().unwrap().contains("docx"), "应引导转 docx: {r}");
    }

    #[tokio::test]
    async fn attach_txt_rejects_pointing_to_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        let r = tool_attach(&serde_json::json!({"path":"a.txt"}), &ctx).await;
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["attached"], false);
        assert!(v["error"].as_str().unwrap().contains("read"), "txt 应引导用 read: {r}");
    }

    #[tokio::test]
    async fn attach_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        let r = tool_attach(&serde_json::json!({"path":"nope.pdf"}), &ctx).await;
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["attached"], false);
        assert!(v["error"].as_str().unwrap().contains("不存在"), "应报不存在: {r}");
    }

    #[tokio::test]
    async fn attach_missing_path_arg_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        let r = tool_attach(&serde_json::json!({}), &ctx).await;
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["attached"], false);
        assert!(v["error"].as_str().unwrap().contains("path"));
    }

    #[test]
    fn schemas_has_eight_tools() {
        let s = schemas();
        let names: Vec<&str> = s.iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["write", "read", "bash", "display", "edit_card", "edit", "subagent", "attach"]);
    }
```

- [ ] **Step 1.3: 跑测试确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml tool_attach schemas_has_eight`
Expected: 编译失败（`tool_attach` 未定义；`schemas_has_seven_tools` 仍 7 个会挂——本 step 同时改断言名，故旧测试需一并重命名，见 1.5）。

- [ ] **Step 1.4: 实现 `tool_attach`（pdf/docx + 拒绝，image 占位）**

在 `src-tauri/src/tools.rs`，紧跟 `tool_edit` 之后（约 line 268 后）插入：

```rust
/// attach：把 read 搞不定的图/pdf/docx 纳入助手视野。
/// - pdf/docx：抽文本截断 → tool result 文本（当轮可见）
/// - image：落盘 + 触发系统注入带图 user 消息（下轮可见）；本函数返回 note（不带图）
/// - .doc/txt 等：报错引导
/// 返回 JSON 字符串（同 tool_display/tool_edit_card 模式）。
pub async fn tool_attach(args: &Value, ctx: &ToolsCtx) -> String {
    let p = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return serde_json::json!({ "attached": false, "error": "缺少 path 参数" }).to_string(),
    };
    let path = resolve_path(p, &ctx.workspace);
    if !path.exists() {
        return serde_json::json!({ "attached": false, "error": format!("文件不存在: {}", path.display()) }).to_string();
    }
    let caption = args.get("caption").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let kind = crate::file_kind(&path);
    match kind {
        crate::FileKind::Image => {
            // Task 4 接通：stage_one + 发 InjectAttachment 信号
            serde_json::json!({ "attached": false, "error": "image 路径尚未实现（Task 4）" }).to_string()
        }
        crate::FileKind::Pdf | crate::FileKind::Docx => {
            let k = crate::file_kind_str(kind);
            match crate::extract::doc_text(&path, k) {
                Some(t) if !t.trim().is_empty() => {
                    let capped = crate::llm::cap_doc_text(&t);
                    serde_json::json!({ "attached": true, "kind": k, "text": capped }).to_string()
                }
                _ => serde_json::json!({
                    "attached": false, "kind": k,
                    "error": format!("{} 无法抽取文本（损坏/加密/无文本层），可用 read/display 兜底", path.display())
                }).to_string(),
            }
        }
        crate::FileKind::Text | crate::FileKind::Markdown | crate::FileKind::Csv => {
            serde_json::json!({ "attached": false, "error": "纯文本/代码/ csv 用 read，无需 attach" }).to_string()
        }
        _ => {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let hint = if ext == "doc" { "（.doc 旧二进制格式不支持，请转成 .docx 后再 attach）" } else { "" };
            serde_json::json!({ "attached": false, "error": format!("不支持的文件类型: .{ext}{hint}") }).to_string()
        }
    }
}
```

- [ ] **Step 1.5: dispatch 加 `"attach"` 臂 + schema 加 attach + 改工具数断言**

`src-tauri/src/tools.rs` dispatch（约 line 603-620），在 `"subagent"` 臂后、`other =>` 前加：
```rust
        "attach" => tool_attach(&args, ctx).await,
```

`src-tauri/src/tools.rs` `schemas()`（约 line 486-599），在 `subagent` schema 后、闭括号前追加：
```rust
        serde_json::json!({
            "type":"function",
            "function":{
                "name":"attach",
                "description":"把 read 搞不定的文件纳入你的视野：pdf/Word(.docx) 抽取文本当轮即可看到；图片(png/jpg/webp 等)本轮看不到，系统下一轮以附件形式注入后你才能看到——调完 attach(图片) 不要死等，正常结束当前回合即可。.doc 旧格式不支持（转 .docx）；纯文本/代码用 read。",
                "parameters":{
                    "type":"object",
                    "properties":{
                        "path":{"type":"string","description":"文件路径，相对工作目录或绝对路径"},
                        "caption":{"type":"string","description":"可选；图片说明（下轮注入消息的标注 + 卡片说明）。pdf/docx 不用"}
                    },
                    "required":["path"]
                }
            }
        }),
```

`src-tauri/src/tools.rs` 把旧断言测试 `schemas_has_seven_tools`（约 line 772）整段替换为 `schemas_has_eight_tools`（即 Step 1.2 写的那个——删除旧的 seven 版本，避免两个并存）。

`src-tauri/src/llm.rs` `build_body_has_tools_and_reasoning_split`（约 line 681）：
```rust
        assert_eq!(body["tools"].as_array().unwrap().len(), 8);
```

- [ ] **Step 1.6: 跑测试确认通过**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml tool_attach schemas_has_eight build_body_has_tools`
Expected: PASS（attach 5 个 + schemas_has_eight + build_body_has_tools 全绿）。

- [ ] **Step 1.7: 全量 check + commit**

Run: `cargo check --tests --manifest-path src-tauri/Cargo.toml`
Expected: 零错误零 warning。

```bash
git add src-tauri/src/tools.rs src-tauri/src/llm.rs
git commit -m "$(cat <<'EOF'
feat(tools): attach 工具文本路径——pdf/docx 抽文本进 tool result + 拒绝路径 + 工具数 7→8

- tool_attach：pdf/docx 走 extract::doc_text + cap_doc_text 截断 → {attached,kind,text}；
  .doc 报错引导转 docx；txt/md/csv 引导用 read；不存在/缺参报错。image 臂占位（Task 4 接通）
- dispatch 加 "attach" 臂；schemas() 加 attach 段
- llm.rs cap_doc_text/DOC_TEXT_MAX 设 pub(crate)（tool_attach 复用，与 user_message_with_attachments 同语义）
- 工具数断言两处同步 7→8（tools.rs schemas_has_eight_tools + llm.rs build_body）

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: ToolsCtx 加 session_tx 字段（image 信号通道前置）

**Files:**
- Modify: `src-tauri/src/tools.rs`（`ToolsCtx` 加字段 + `foreground` 同步）
- Modify: `src-tauri/src/agent.rs`（`spawn_session` 给 ctx 注入 `tx.clone()`；测试 `ctx_with` 同步）

**Interfaces:**
- Consumes: `crate::agent::SessionEvent`（agent.rs，Task 3 才加 `InjectAttachment` variant；本 task 字段类型先用既有 `SessionEvent`，Task 3 加 variant 不破坏类型）
- Produces: `ToolsCtx.session_tx: mpsc::Sender<crate::agent::SessionEvent>`（Task 4 的 tool_attach image 分支用它发信号）

**范围说明**：纯接线 task（加字段 + 构造点同步），无新行为。测试以"编译通过 + 现有测试不挂"为验收。

- [ ] **Step 2.1: 给 ToolsCtx 加 session_tx 字段**

`src-tauri/src/tools.rs` `ToolsCtx` struct（约 line 36-53），在 `job_writer` 字段后加：
```rust
    /// driver session 事件通道。tool_attach(image) 经此发 InjectAttachment 信号给 driver
    /// （driver 在 turn N 结束后处理 → 构造带图 user 消息 → turn N+1）。
    /// foreground()/测试用无人接收的 channel；spawn_session 注入真实 driver tx。
    pub session_tx: mpsc::Sender<crate::agent::SessionEvent>,
```

- [ ] **Step 2.2: foreground() 同步构造**

`src-tauri/src/tools.rs` `ToolsCtx::foreground`（约 line 58-72），在函数体首行加一个无人接收的 channel，并在 struct 字面量末尾加字段：
```rust
    pub fn foreground(workspace: std::path::PathBuf, minimax_region: String) -> Self {
        let (tx, _rx) = mpsc::channel(8);
        let (stx, _srx) = mpsc::channel::<crate::agent::SessionEvent>(8);
        Self {
            cache: workspace.clone(),
            workspace,
            jobs: Arc::new(std::sync::Mutex::new(jobs::JobRegistry::new())),
            job_done_tx: tx,
            job_update: Arc::new(jobs::NoopJobUpdate),
            minimax_region,
            allow_background: true,
            subagent_stream: std::sync::Arc::new(crate::subagents::NoopSubagentStream),
            history: crate::history::HistoryWriterHandle::noop(),
            job_writer: crate::jobs::JobWriterHandle::noop(),
            session_tx: stx,
        }
    }
```

- [ ] **Step 2.3: spawn_session 给 ctx 注入真实 tx（注意 async move 捕获）**

`tx` 在 `spawn_session` 顶部定义（`let (tx, mut rx) = tokio::sync::mpsc::channel::<SessionEvent>(CHANNEL_CAP);`，约 line 124），函数末尾 `SessionHandle { tx }`（约 line 201）**还要用它**。主 driver 的 `tauri::async_runtime::spawn(async move { ... })` 块（约 line 160）若直接写 `tx.clone()`，`async move` 会把 `tx` 整个 move 进块 → 末尾 `SessionHandle { tx }` 编译失败（error: use of moved value `tx`）。

**正确做法**：在主 driver spawn 块**外**（ticker spawn 块之后、主 driver spawn 之前，约 line 159）先 clone 一个独立变量，块内 ctx 构造消费它：

`src-tauri/src/agent.rs` 主 driver spawn 之前加一行：
```rust
    let session_tx_for_ctx = tx.clone(); // 给主 driver ctx 用；tx 本身留给 SessionHandle { tx }
```

主 driver spawn 块内 ctx 构造（约 line 177-188），在 `job_writer` 字段后加：
```rust
            session_tx: session_tx_for_ctx,
```

（验证：`cargo check --tests` 必须通过——若报 `use of moved value: tx` 在 `SessionHandle { tx }`，说明误把 clone 写进 async move 块了，回到"块外 clone"。）

- [ ] **Step 2.4: agent.rs 测试 ctx_with 同步**

`src-tauri/src/agent.rs` tests 段 `ctx_with`（约 line 358-367），加 session_tx：
```rust
    fn ctx_with(ws: std::path::PathBuf, h: crate::history::HistoryWriterHandle) -> ToolsCtx {
        let (_tx, _rx) = tokio::sync::mpsc::channel(8);
        let (stx, _srx) = tokio::sync::mpsc::channel(8);
        ToolsCtx {
            workspace: ws.clone(), cache: ws, jobs: Arc::new(Mutex::new(JobRegistry::new())) as SharedRegistry,
            job_done_tx: _tx, job_update: Arc::new(NoopJobUpdate), minimax_region: "cn".into(),
            allow_background: true, subagent_stream: Arc::new(crate::subagents::NoopSubagentStream),
            history: h,
            job_writer: crate::jobs::JobWriterHandle::noop(),
            session_tx: stx,
        }
    }
```

- [ ] **Step 2.5: 跑全量测试确认不挂**

Run: `cargo check --tests --manifest-path src-tauri/Cargo.toml && cargo test --lib --manifest-path src-tauri/Cargo.toml`
Expected: 编译通过（ToolsCtx 所有构造点都已同步）；现有测试全绿（无新行为）。

- [ ] **Step 2.6: commit**

```bash
git add src-tauri/src/tools.rs src-tauri/src/agent.rs
git commit -m "$(cat <<'EOF'
feat(tools): ToolsCtx 加 session_tx 字段——tool_attach(image) 信号通道前置

纯接线：ToolsCtx 加 session_tx: Sender<SessionEvent>；foreground/ctx_with 用无人接收 channel；
spawn_session 注入真实 driver tx.clone()。无新行为，为 Task 4 image 分支发 InjectAttachment 信号铺路。

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: SessionEvent::InjectAttachment + handle_event 映射 + emit

**Files:**
- Modify: `src-tauri/src/agent.rs`（SessionEvent variant + handle_event 臂 + triggers_turn + run_one emit + inject_text helper）
- Test: `src-tauri/src/agent.rs`（tests 段）

**Interfaces:**
- Consumes: `crate::AttachmentRef`（lib.rs:330）；`crate::history::HistoryEvent::user`（history.rs:25）；`TauriEmitter::emit`（agent.rs 顶部已 import）
- Produces: `SessionEvent::InjectAttachment { attachments: Vec<crate::AttachmentRef>, caption: String }`；driver 收到后映射成 `HistoryEvent::user(now, "main", &inject_text(caption), attachments)` 并触发 turn；前端事件 `"injected-attachment"` payload `{staged_path, kind, caption}`

**范围说明**：本 task 建立"系统注入带图 user 消息"的 driver 侧机制（落 history + 触发 turn + 告前端）。Task 4 的 tool_attach image 分支会发这个信号。driver `select` 循环结构零改动。

- [ ] **Step 3.1: 写失败测试（agent.rs tests 段追加）**

```rust
    #[tokio::test]
    async fn injectattachment_appended_as_user_with_image_and_runs_turn() {
        let (dir, h, hd) = ws_setup();
        let round: Arc<dyn LlmRound> = Arc::new(StopRound);
        let emit = FakeEmitter { content: Mutex::new(String::new()) };
        let x = ctx_with(dir.path().to_path_buf(), h.clone());
        let c = cfg();
        let aref = crate::AttachmentRef { staged_path: "/tmp/fake.png".into(), kind: "image".into() };
        let resp = handle_event(
            &SessionEvent::InjectAttachment { attachments: vec![aref.clone()], caption: "草图".into() },
            &h, &hd, dir.path(), &x, &c, round, &emit).await;
        assert!(resp.is_some(), "InjectAttachment 应触发 turn");
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let evs = crate::history::read_all(&hd);
        // 落 kind=user 带 attachments（复用用户拖图事件类型）
        let u = evs.iter().find(|e| e.kind == "user").expect("应落 user 事件");
        let atts = u.data["attachments"].as_array().unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0]["kind"], "image");
        assert_eq!(atts[0]["staged_path"], "/tmp/fake.png");
        // 文本中性、含 caption、不冒充用户下指令
        let text = u.data["text"].as_str().unwrap();
        assert!(text.contains("系统"), "注入文本须中性标注系统: {text}");
        assert!(text.contains("草图"), "含 caption: {text}");
        assert!(!text.contains("请描述"), "不能冒充用户下指令: {text}");
    }

    #[test]
    fn inject_text_neutral_with_and_without_caption() {
        assert!(inject_text("").contains("系统"));
        assert!(!inject_text("").contains("请"));
        let with = inject_text("我的草图");
        assert!(with.contains("系统") && with.contains("我的草图"));
    }
```

- [ ] **Step 3.2: 跑测试确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml injectattachment inject_text`
Expected: 编译失败（`SessionEvent::InjectAttachment` 未定义；`inject_text` 未定义）。

- [ ] **Step 3.3: 加 SessionEvent variant**

`src-tauri/src/agent.rs` `SessionEvent` enum（约 line 10-18），在 `DreamCheck` 后加：
```rust
    /// 助手经 attach 工具请求纳入图片：driver 构造一条带图 user 消息（下轮 M3 vision）。
    /// 不冒充用户：文本中性（inject_text）；history 存 kind=user（复用用户拖图管道）。
    InjectAttachment { attachments: Vec<crate::AttachmentRef>, caption: String },
```

- [ ] **Step 3.4: 加 inject_text helper**

`src-tauri/src/agent.rs`，紧跟 `jobdone_body` 函数后（约 line 40 后）加：
```rust
/// 系统注入 user 消息的中性文本（不冒充用户下指令）。
/// M3 须明白：图是助手自己要看，响应接 assistant 上一轮的自检意图——故文本只陈述事实，不下命令。
fn inject_text(caption: &str) -> String {
    if caption.trim().is_empty() {
        "[系统：助手通过 attach 请求纳入以下图片]".to_string()
    } else {
        format!("[系统：助手通过 attach 请求纳入以下图片：{caption}]")
    }
}
```

- [ ] **Step 3.5: handle_event 加 InjectAttachment 臂**

`src-tauri/src/agent.rs` `handle_event` 的 `match event`（约 line 59-77），在 `SessionEvent::Reset` 臂前加：
```rust
        SessionEvent::InjectAttachment { attachments, caption } => {
            crate::history::HistoryEvent::user(now, "main", &inject_text(caption), attachments)
        }
```

- [ ] **Step 3.6: triggers_turn 加 InjectAttachment**

`src-tauri/src/agent.rs` `handle_event` 约第 87 行：
```rust
    let triggers_turn = matches!(event,
        SessionEvent::UserMessage { .. } | SessionEvent::JobDone(_) | SessionEvent::InjectAttachment { .. });
```

- [ ] **Step 3.7: run_one 在处理 InjectAttachment 时 emit 前端事件**

`src-tauri/src/agent.rs` `run_one`（约 line 210-262），在 `let emit = crate::AppEmitter { app: app.clone() };`（约 line 229）后、`handle_event` 调用（约 line 245）前，插入（注意须在 DreamCheck early-return 之后，故放 line 229 后即可——DreamCheck 在 line 225 已 return）：
```rust
    // 系统注入带图 user 消息：落 history 前先 emit 前端图卡（live 路径；history 路径由 buildHistoryBubbles 重建）
    if let SessionEvent::InjectAttachment { attachments, caption } = e {
        if let Some(a) = attachments.first() {
            let _ = TauriEmitter::emit(app, "injected-attachment", serde_json::json!({
                "staged_path": a.staged_path, "kind": a.kind, "caption": caption,
            }));
        }
    }
```

注：`run_one` 现有 `matches!(e, SessionEvent::UserMessage { .. } | SessionEvent::ContextNote { .. })`（line 221 note_activity）与 `matches!(e, ... | SessionEvent::JobDone(_))`（line 259 dispatch_dream）** deliberately 不含 InjectAttachment**——系统注入不算用户活动（不推迟 dream），MVP 也不在注入后额外 dispatch_dream。

- [ ] **Step 3.8: 跑测试确认通过**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml injectattachment inject_text`
Expected: PASS。

- [ ] **Step 3.9: 全量 check + commit**

Run: `cargo check --tests --manifest-path src-tauri/Cargo.toml`
Expected: 零错误零 warning。

```bash
git add src-tauri/src/agent.rs
git commit -m "$(cat <<'EOF'
feat(agent): SessionEvent::InjectAttachment——系统注入带图 user 消息机制

- SessionEvent 加 InjectAttachment{attachments,caption} variant
- handle_event 臂映射成 HistoryEvent::user（复用用户拖图管道，history 零改动）+ triggers_turn
- inject_text：中性标注文本（不冒充用户下指令）
- run_one：处理时 emit injected-attachment 给前端（live 图卡渲染）
- driver select 循环零改动（rx.recv() 天然接收新 variant）
- note_activity/dispatch_dream deliberately 不含 InjectAttachment（系统注入非用户活动）

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: tool_attach image 分支（stage_one + 发信号 + 返回 note）

**Files:**
- Modify: `src-tauri/src/tools.rs`（`tool_attach` image 臂替换占位）
- Test: `src-tauri/src/tools.rs`（tests 段）

**Interfaces:**
- Consumes: `stage_one`（tools.rs:305，已存在）；`ctx.session_tx`（Task 2 加）；`crate::AttachmentRef`（lib.rs:330）；`crate::agent::SessionEvent::InjectAttachment`（Task 3 加）；`ctx.cache`（attachments 目录基）
- Produces: image 分支返回 `{attached:true, kind:"image", staged_path, note}`（不带图——绕开 tool-role 不带图限制），并经 `ctx.session_tx` 发一个 `InjectAttachment` 信号

**范围说明**：接通 image 路径。依赖 Task 2（session_tx）+ Task 3（InjectAttachment variant）。

- [ ] **Step 4.1: 写失败测试（tools.rs tests 段追加）**

```rust
    #[tokio::test]
    async fn attach_image_stages_and_sends_inject_signal() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().to_path_buf();
        // 造一张真图（stage_one 走 IMG_MAX_BYTES 校验，小文件 OK）
        std::fs::write(ws.join("pic.png"), b"png-bytes").unwrap();
        // ctx 用带可观测 rx 的 session_tx（foreground 用无人接收 channel，测不到信号 → 手工建）
        let (stx, mut srx) = tokio::sync::mpsc::channel::<crate::agent::SessionEvent>(8);
        let mut ctx = ToolsCtx::foreground(ws.clone(), "cn".into());
        ctx.cache = ws.clone(); // 让 attachments 落 tempdir 内可断言
        ctx.session_tx = stx;
        let r = tool_attach(&serde_json::json!({"path":"pic.png","caption":"草图"}), &ctx).await;
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["attached"], true, "image 应成功: {r}");
        assert_eq!(v["kind"], "image");
        assert!(v["staged_path"].as_str().unwrap().ends_with(".png"));
        assert!(v["note"].as_str().unwrap().contains("下一轮"), "note 须说明下轮可见: {r}");
        // 信号已发：driver 应收到 InjectAttachment
        match srx.recv().await.unwrap() {
            crate::agent::SessionEvent::InjectAttachment { attachments, caption } => {
                assert_eq!(caption, "草图");
                assert_eq!(attachments.len(), 1);
                assert_eq!(attachments[0].kind, "image");
                // staged_path 真实落盘
                assert!(std::fs::metadata(&attachments[0].staged_path).is_ok());
            }
            other => panic!("应为 InjectAttachment，得到 {other:?}"),
        }
    }
```

- [ ] **Step 4.2: 跑测试确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml attach_image`
Expected: FAIL（`attached==false`，image 臂仍是占位）。

- [ ] **Step 4.3: 实现 image 分支**

`src-tauri/src/tools.rs` `tool_attach` 的 `crate::FileKind::Image =>` 臂（Task 1 占位处）替换为：
```rust
        crate::FileKind::Image => {
            let dir = ctx.cache.join("attachments");
            let staged = stage_one(&path, &dir, 0); // max_mb=0：image 走 stage_one 内部 IMG_MAX_BYTES（10MB）
            if !staged.ok {
                return serde_json::json!({
                    "attached": false,
                    "error": staged.error.unwrap_or_else(|| "落盘失败".into())
                }).to_string();
            }
            let aref = crate::AttachmentRef {
                staged_path: staged.staged_path.clone(), kind: "image".into(),
            };
            // 发信号给 driver：下轮以 user 消息注入此图（当轮 tool result 不带图——绕开 tool-role 限制）
            let _ = ctx.session_tx.send(crate::agent::SessionEvent::InjectAttachment {
                attachments: vec![aref], caption: caption.clone(),
            }).await;
            serde_json::json!({
                "attached": true, "kind": "image", "staged_path": staged.staged_path,
                "note": "图片下一轮以附件纳入视野（本轮不可见）；正常结束当前回合即可，下轮系统会把图喂进来"
            }).to_string()
        }
```

- [ ] **Step 4.4: 跑测试确认通过**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml attach_image`
Expected: PASS。

- [ ] **Step 4.5: 全量 check + commit**

Run: `cargo check --tests --manifest-path src-tauri/Cargo.toml && cargo test --lib --manifest-path src-tauri/Cargo.toml`
Expected: 零 warning；全绿（含 Task 1/2/3 回归）。

```bash
git add src-tauri/src/tools.rs
git commit -m "$(cat <<'EOF'
feat(tools): attach image 分支——stage_one 落盘 + 发 InjectAttachment 信号

image 臂：stage_one 落 attachments/<hash8>.png（复用用户拖图入口+去重）→ 构造 AttachmentRef →
经 ctx.session_tx 发 InjectAttachment 信号（driver 下轮构造带图 user 消息）→ 返回 {attached,kind,staged_path,note}
（不带图，绕开 tool-role 不带图限制）。note 明示"本轮不可见、下轮见"。

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: 前端 live + history 图卡渲染

**Files:**
- Modify: `src/main.js`（`setupAgentEvents` 加 `listen("injected-attachment")`；`buildHistoryBubbles` 让 image attachment 渲染完整图卡）
- Test: `node --check src/main.js` + 手测

**Interfaces:**
- Consumes: 后端 `injected-attachment` 事件（payload `{staged_path, kind, caption}`，Task 3 emit）；`renderMediaCard(wrap, {display,path,kind,caption})`（main.js:936）；`buildUserBubble(text, attachments)`（main.js:209）；`list`/`bottomLoader`/`scrollBottom`（模块级）
- Produces: live 路径收到 `injected-attachment` → 插入带系统标注的 user 气泡 + 完整图卡；history 路径 user 事件的 image attachment → 完整图卡（改进：用户拖图的历史重载也从 chip 升级为完整图卡）

**范围说明**：本 task 让"系统注入的图"对用户可见（spec 验收 #4）。live 走新事件；history 走改 `buildHistoryBubbles` 的 user 臂。**行为变化**：history 路径所有 image kind 的 user attachment 都会渲染完整图卡（含用户自己拖的图——从 chip 升级为完整图，这是改进，非退化）。

**实现前必读**：动手前先 `Read src/main.js` 定位 `setupAgentEvents`（约 line 1607）、`buildHistoryBubbles` 的 user 臂（约 line 1419）、`renderMediaCard`（约 line 936）、确认 `list`/`bottomLoader`/`scrollBottom` 的作用域。行号会随改动漂移，以函数名为准。

- [ ] **Step 5.1: 在 setupAgentEvents 加 injected-attachment listener**

`src/main.js` `setupAgentEvents` 内现有 `listen(...)` 群里（约 line 1607+，与 `chat-turn-start` 等 listener 同段），追加：
```javascript
  await listen("injected-attachment", (e) => {
    const p = e.payload || {};
    const caption = p.caption || "";
    const text = caption
      ? `[系统：助手纳入图片：${caption}]`
      : "[系统：助手纳入图片]";
    const wrap = document.createElement("div");
    wrap.className = "bubble user";
    const body = document.createElement("div");
    body.className = "bubble-text";
    body.textContent = text;
    wrap.appendChild(body);
    if (p.staged_path) {
      const media = document.createElement("div");
      media.className = "bubble-media";
      wrap.appendChild(media);
      renderMediaCard(wrap, { display: true, path: p.staged_path, kind: "image", caption });
    }
    if (typeof bottomLoader !== "undefined" && list && bottomLoader) {
      list.insertBefore(wrap, bottomLoader);
    } else if (list) {
      list.appendChild(wrap);
    }
    if (typeof scrollBottom === "function") scrollBottom();
  });
```

注：`renderMediaCard` 内部已 `scrollBottom()`（见 Explore 报告 main.js:936），重复调用无害。

- [ ] **Step 5.2: 改 buildHistoryBubbles 让 image attachment 渲染完整图卡**

`src/main.js` `buildHistoryBubbles` 的 user 臂（约 line 1419-1423，原 `groups.push({ node: buildUserBubble(...), ... })`），替换为：
```javascript
  if (k === "user") {
    asst = null;
    const atts = evField(ev, "attachments") || [];
    const wrap = buildUserBubble(evField(ev, "text") || "", atts);
    // image attachment 额外渲染完整图卡（用户拖图 + 系统注入统一；让图对用户可见）
    const imgs = atts.filter((a) => a && a.kind === "image");
    if (imgs.length) {
      const media = document.createElement("div");
      media.className = "bubble-media";
      wrap.appendChild(media);
      for (const a of imgs) {
        renderMediaCard(wrap, { display: true, path: a.staged_path, kind: "image", caption: "" });
      }
    }
    groups.push({ node: wrap, seqStart: ev.seq, seqEnd: ev.seq });
  }
```

（保留原 `buildUserBubble` 调用产出的 attach-strip chip；在其后追加完整图卡。两者并存：chip 标注 + 完整图预览。）

- [ ] **Step 5.3: 语法校验**

Run: `node --check src/main.js`
Expected: 无输出（语法 OK）。

- [ ] **Step 5.4: commit（前端改动；手测留到全分支完工后 release exe 验）**

```bash
git add src/main.js
git commit -m "$(cat <<'EOF'
feat(ui): 系统注入图卡渲染——live injected-attachment + history image 完整图卡

- setupAgentEvents 加 listen("injected-attachment")：后端注入时插 user 气泡（系统标注文本）+ renderMediaCard 完整图
- buildHistoryBubbles：user 事件的 image attachment 追加 renderMediaCard（让历史重载也能看见图；
  用户拖图的历史渲染一并从 chip 升级为 chip+完整图，改进非退化）
- 复用现成 .bubble-media/.media-card，styles.css 零改动

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: AGENT.md 加 attach 工具段

**Files:**
- Modify: `src-tauri/defaults/AGENT.md`（pinned，每轮重读，改完下轮即生效）

**范围说明**：告知 agent attach 的用途、与 read/display 的区别、image 下轮见的关键行为。无单测（pinned 文件，行为靠 agent 运行时遵循）。

**实现前必读**：先 `Read src-tauri/defaults/AGENT.md` 找到工具说明区（read/bash/display 等段附近），把下面内容插入与之并列的位置。照该文件既有风格（标题层级、措辞密度）写。

- [ ] **Step 6.1: Read AGENT.md 定位插入点**

Run: 读 `src-tauri/defaults/AGENT.md`，找到现有工具说明（read/write/bash/display/edit/edit_card/subagent）的段落布局。

- [ ] **Step 6.2: 插入 attach 段**

在工具说明区追加（措辞随既有风格调整）：
```markdown
## attach(path, caption="")

把 **read 搞不定**的文件纳入你的视野——用于自检、对照、再生成。

- **pdf / Word(.docx)**：抽取文本，**当轮**就能在 tool result 里看到全文（截断到 200KB）。
- **图片(png/jpg/webp/...)**：**本轮看不到**——系统会在**下一轮**以附件形式把图注入进来（user 角色），那时你才能用视觉看它。调完 `attach(图片)` **不要死等**，正常结束当前回合即可，下轮系统自动喂图。
- **.doc** 旧二进制不支持（转 .docx）；纯文本/代码/配置用 `read`（read 已覆盖）。

与其它工具的区别：
- `read` = 纯文本（txt/md/csv/code）；`attach` = 图 + pdf/docx（二进制，read 搞不定）。
- `display` = 给**用户**看（不进历史，你下一轮看不见）；`attach` = **你**也纳入视野（进历史）。

自检循环控制（几轮停、不满意如何处理）不归 attach 管——那是你的判断职责。
```

- [ ] **Step 6.3: commit**

```bash
git add src-tauri/defaults/AGENT.md
git commit -m "$(cat <<'EOF'
docs(agent): AGENT.md 加 attach 工具段——用途/read+display 区别/image 下轮见

pinned 文件，每轮重读，下轮即生效。重点告知：pdf/docx 当轮可见、image 下轮见（调完别死等）。

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

## 验证（全分支完工后）

1. **编译 + 全量单测**（dev server 锁 exe，用 check/test）：
   ```bash
   cargo check --tests --manifest-path src-tauri/Cargo.toml
   cargo test --lib --manifest-path src-tauri/Cargo.toml
   node --check src/main.js
   ```
   预期：零 warning；tools/llm/agent 测试全绿（含本计划新增的 attach/injectattachment/inject_text 测试 + 回归）。

2. **TDD 覆盖清单**（每 task 已写，完工后回归一遍）：
   - `tool_attach` 分流：docx→{attached,kind,text}；.doc→引导 docx；txt→引导 read；不存在/缺参→报错；image→落盘+发信号+note（Task 1/4）
   - `handle_event(InjectAttachment)`：落 kind=user 带 attachments（复用用户拖图事件）+ 触发 turn + 中性文本不冒充用户（Task 3）
   - `inject_text`：中性、含/不含 caption（Task 3）
   - 工具数断言 = 8（tools.rs + llm.rs 两处，Task 1）
   - 回归：现有用户拖图管道不受影响（image 注入走同一管道；run_turn 工具循环、driver 事件路由未改结构）

3. **手测**（release exe，dev server 锁 debug exe 所以前端改动靠 release 重嵌验证）：
   - `cargo build --release --manifest-path src-tauri/Cargo.toml`（确认前端 main.js 经 `generate_context!` 重嵌）。
   - agent `attach` 一张 mmx 产物图 → **下一轮**助手能描述图内容（证明系统注入 user 消息 + M3 在 user role 看见）；当轮 tool result 含 note"下一轮见"。
   - agent `attach` 一个 pdf → **当轮** tool result 含文本，助手能引用。
   - agent `attach` 一个 .doc → 报错引导转 docx。
   - 历史重载（重启 / 切会话回看）：系统注入的图卡正常渲染（live/history 双路径）。

## 约束（执行时遵守）

- 本计划所有 Rust 改动用 `cargo check --tests` / `cargo test --lib` 验，**不** `cargo build/run`（debug exe 被 dev server 锁）。
- 工具数断言两处必须同步（[[ovoice-tool-count-cascade]]）。
- live vs history 双路径（[[ovoice-live-vs-history-render-paths]]）：Task 5 两处渲染都要改。
- 改 AGENT.md 下轮即生效，不必重启 dev（[[ovoice-pinned-files-per-turn-reload]]）。
- 数据保留 D2：InjectAttachment 落 history 为 kind=user 带 attachments，append-only。
- 每个 task 一个 commit（TDD 循环：写失败测试→跑红→实现→跑绿→commit），commit message 以 `Co-Authored-By: Claude <noreply@anthropic.com>` 结尾。

## 附：关键时序（image 路径，文字说明）

```
Turn N（assistant 自检）:
  assistant: tool_call(attach, path=render/x.png, caption="草图")
    ↓ llm.rs run_turn 工具循环调 tools::dispatch("attach", ...)
    ↓ tool_attach(image 臂):
       1. stage_one → attachments/<hash8>.png（复用用户拖图入口 + 去重）
       2. ctx.session_tx.send(InjectAttachment{attachments:[aref], caption})  ← 信号进 driver rx buffer（bounded 64）
       3. return {attached:true, kind:image, staged_path, note:"下一轮见"}     ← 当轮 tool result（不带图）
    ↓ run_turn 把 tool_result 落 history（role:tool 纯文本，合法）
    ↓ assistant 收到 result → 结束 turn N（Stop）

Turn N 结束 → driver run_one 返回 → select 回顶 → rx.recv() 拿到 InjectAttachment:
  run_one:
    1. emit "injected-attachment"（前端 live 插图卡）
    2. handle_event(InjectAttachment):
       - 落 HistoryEvent::user(now, "main", "[系统：助手通过 attach 请求纳入以下图片：草图]", [aref])
       - triggers_turn = true
       - build_messages → user_message_with_attachments → image_ref（llm.rs:76）
       - run_turn N+1 → expand_messages_for_send → image_ref base64 image_url（llm.rs:110，role=user）
       - M3 在 user role 看见图 ✅ → assistant 基于图继续
```
