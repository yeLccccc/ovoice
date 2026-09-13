# 用户提示词前缀（User Prompt Prefix）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让每条发给 LLM 的 user-role 消息 content 前拼一段可配置 prefix（落盘 `data["prefix"]`，重建时拼接），配置页可设置文本 + 启停。

**Architecture:** 落盘侧 `handle_event` 调纯函数 `maybe_snapshot_prefix` 给 user-role 事件 `data["prefix"]` 快照；取侧 `event_to_message` 三分支读 `data["prefix"]` 用 `with_prefix` 拼到 content 前。`build_messages` 签名不动，prefix 完全来自 history。config 加两字段，前端设置页加 textarea + checkbox。

**Tech Stack:** Rust（config / context / agent）+ Vanilla HTML/JS（无框架，无 JS 测试 runner）

## Global Constraints

- **TDD red-green**：每个后端 task 先写失败测试 → 跑 → 实现 → 跑过 → commit。
- **唯一自动测试**：`cargo test --manifest-path src-tauri/Cargo.toml --lib`。前端用 `node --check src/main.js` 验语法 + 手动验证功能。
- **不改 `build_messages` 签名**（`context.rs:89`）—— prefix 来自 history，不加参数。~15 个现有测试调用零改动。
- **分支**：`feat/user-prompt-prefix`（已从 master 开，spec 已 commit `e594fcb`）。
- **commit message** 末尾必须 `Co-Authored-By: Claude <noreply@anthropic.com>`。
- **`max_tool_iters=100` 不动**（CLAUDE.md 硬约束，与本功能无关，别误改）。
- **cargo check 零 warning**：每次改完 Rust 跑 `cargo check --manifest-path src-tauri/Cargo.toml` 确认无新 warning。
- **cwd**：Bash 工具是 git bash/POSIX sh；cargo 命令用 `--manifest-path` 避免 cd。

---

## Task 1: config 字段 + Default + 全字段测试同步

**Files:**
- Modify: `src-tauri/src/config.rs`（字段声明 :20 区 + Default :172 区 + `:297` 全字段 literal + 新测试）

**Interfaces:**
- Produces: `Config.user_prompt_prefix_enabled: bool`（默认 false）、`Config.user_prompt_prefix: String`（默认 ""）。后续 task 全靠这两个字段。

- [ ] **Step 1: 写失败测试**

在 `config.rs` 的 `mod tests`（文件末尾 `}` 前，挨着 `dream_fields_survive_json_roundtrip` 测试之后）加：

```rust
    #[test]
    fn user_prompt_prefix_disabled_and_empty_by_default() {
        let c = Config::default();
        assert!(!c.user_prompt_prefix_enabled, "默认关闭");
        assert_eq!(c.user_prompt_prefix, "", "默认空串");
    }

    #[test]
    fn user_prompt_prefix_roundtrip() {
        let c = Config {
            user_prompt_prefix_enabled: true,
            user_prompt_prefix: "请用英文回答".into(),
            ..Config::default()
        };
        let s = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&s).unwrap();
        assert!(back.user_prompt_prefix_enabled);
        assert_eq!(back.user_prompt_prefix, "请用英文回答");
    }

    #[test]
    fn user_prompt_prefix_missing_field_uses_default() {
        let back: Config = serde_json::from_str("{}").unwrap();
        assert!(!back.user_prompt_prefix_enabled);
        assert_eq!(back.user_prompt_prefix, "");
    }
```

- [ ] **Step 2: 跑测试验证编译失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib config::tests::user_prompt_prefix`
Expected: 编译错误 `no field `user_prompt_prefix_enabled` on type `Config``（字段还不存在）

- [ ] **Step 3: 加字段声明 + Default**

在 `config.rs` 字段声明区，紧跟 `system_prompt` 字段（`pub system_prompt: String,` 那行）之后加：

```rust
    // 用户提示词前缀开关：开启后每条 user-role 消息发给 LLM 前 content 前拼 user_prompt_prefix。
    #[serde(default)]
    pub user_prompt_prefix_enabled: bool,
    // 用户提示词前缀文本（典型用途：做事与思考逻辑；每条 user-role 输入前拼接，落盘 data["prefix"]）。
    #[serde(default)]
    pub user_prompt_prefix: String,
```

在 `Default` impl（`impl Default for Config`）里，紧跟 `system_prompt: d_sys(),` 那行之后加：

```rust
            user_prompt_prefix_enabled: false,
            user_prompt_prefix: String::new(),
```

- [ ] **Step 4: 跑新测试验证通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib config::tests::user_prompt_prefix`
Expected: 3 passed

- [ ] **Step 5: 同步改 `:297` 全字段测试（否则编译挂）**

`roundtrip_preserves_all_fields` 测试（`config.rs` 的 `fn roundtrip_preserves_all_fields()`）里有一个全字段 struct literal。在 `system_prompt: "hi".into(),` 那行之后加：

```rust
            user_prompt_prefix_enabled: true,
            user_prompt_prefix: "PREFIX".into(),
```

在同一测试的断言区（挨着其它 `assert_eq!`）加：

```rust
    assert!(back.user_prompt_prefix_enabled);
    assert_eq!(back.user_prompt_prefix, "PREFIX");
```

- [ ] **Step 6: 全量 config 测试 + cargo check**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib config::` && `cargo check --manifest-path src-tauri/Cargo.toml`
Expected: config 全测试 passed；check 零 warning 零 error。

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/config.rs
git commit -m "feat(config): 用户提示词前缀两字段（enabled + prefix）

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 2: 后端落盘 + 取侧（maybe_snapshot_prefix + with_prefix + event_to_message）

**Files:**
- Modify: `src-tauri/src/context.rs`（`with_prefix` helper + `event_to_message` 三分支 + 测试）
- Modify: `src-tauri/src/agent.rs`（`maybe_snapshot_prefix` 纯函数 + `handle_event` 调用 + 测试）

**Interfaces:**
- Consumes: `Config.user_prompt_prefix_enabled` / `Config.user_prompt_prefix`（Task 1）
- Produces:
  - `context::with_prefix(prefix: &str, text: &str) -> String`（私有 helper）
  - `agent::maybe_snapshot_prefix(current: &mut HistoryEvent, cfg: &Config) -> bool`（纯函数，落盘逻辑抽出便于单测；spec §4 内联 if 的等价提取）
  - 约定：user-role 事件 `data["prefix"]` 字段（string）

- [ ] **Step 1: 写取侧失败测试（context.rs）**

在 `context.rs` 的 `mod tests`（末尾 `}` 前）加：

```rust
    #[test]
    fn with_prefix_prepends_when_non_empty() {
        assert_eq!(with_prefix("P", "hi"), "P\nhi");
    }

    #[test]
    fn with_prefix_passthrough_when_empty() {
        assert_eq!(with_prefix("", "hi"), "hi");
        assert_eq!(with_prefix("   ", "hi"), "hi");
    }

    #[test]
    fn user_message_gets_prefix_from_data() {
        let mut ev = HistoryEvent::user(1000, "main", "原文", &[]);
        ev.seq = 0;
        ev.data.insert("prefix".into(), json!("PFX"));
        let m = event_to_message(&ev).unwrap();
        assert_eq!(m["content"], "PFX\n原文");
    }

    #[test]
    fn user_message_no_prefix_field_means_no_prepend() {
        let mut ev = HistoryEvent::user(1000, "main", "原文", &[]);
        ev.seq = 0;
        let m = event_to_message(&ev).unwrap();
        assert_eq!(m["content"], "原文", "无 prefix 字段 → 行为同前（回归保护）");
    }

    #[test]
    fn subagent_result_gets_prefix_from_data() {
        let mut ev = HistoryEvent::subagent_result(1000, "1", "完成", "thread=agent:1");
        ev.seq = 0;
        ev.data.insert("prefix".into(), json!("PFX"));
        let m = event_to_message(&ev).unwrap();
        assert_eq!(m["role"], "user");
        assert!(m["content"].as_str().unwrap().starts_with("PFX\n[子代理"),
            "subagent_result content 前应拼 prefix: {}", m["content"]);
    }

    #[test]
    fn external_gets_prefix_from_data() {
        let mut ev = HistoryEvent::external(1000, "拖入", "x.pdf", None);
        ev.seq = 0;
        ev.data.insert("prefix".into(), json!("PFX"));
        let m = event_to_message(&ev).unwrap();
        assert!(m["content"].as_str().unwrap().starts_with("PFX\n[外部]"),
            "external content 前应拼 prefix: {}", m["content"]);
    }
```

- [ ] **Step 2: 跑测试验证失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib context::tests::with_prefix context::tests::user_message_gets_prefix`
Expected: 编译错误 `cannot find function `with_prefix`` + 取侧测试不通过（当前 event_to_message 不读 prefix）

- [ ] **Step 3: 实现 `with_prefix` helper + 改 `event_to_message` 三分支**

在 `context.rs` 的 `fn event_to_message`（`context.rs:111`）之前加 helper：

```rust
/// prefix 非空时拼到 text 前（中间换行）；空串原样返回。供 user-role 消息 content 前拼接。
fn with_prefix(prefix: &str, text: &str) -> String {
    if prefix.trim().is_empty() {
        text.to_string()
    } else {
        format!("{prefix}\n{text}")
    }
}
```

改 `event_to_message` 的 `"user"` 分支（原 `context.rs:113-118`）为：

```rust
        "user" => {
            let text = e.data.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let prefix = e.data.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
            let text = with_prefix(prefix, text);
            let atts: Vec<AttachmentRef> = e.data.get("attachments")
                .and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
            Some(crate::llm::user_message_with_attachments(&text, &atts))
        }
```

改 `"subagent_result"` 分支（原 `context.rs:151-156`）为：

```rust
        "subagent_result" => {
            let prefix = e.data.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
            let id = e.data.get("agent_id").and_then(|v| v.as_u64()).unwrap_or(0);
            let summary = e.data.get("summary").and_then(|v| v.as_str()).unwrap_or("");
            let refs = e.data.get("ref").and_then(|v| v.as_str()).unwrap_or("");
            Some(json!({ "role": "user", "content": with_prefix(prefix, &format!("[子代理 #{id} 完成] {summary} [完整对话: {refs}]")) }))
        }
```

改 `"external"` 分支（原 `context.rs:157-167`）为：

```rust
        "external" => {
            let prefix = e.data.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
            let what = e.data.get("what").and_then(|v| v.as_str()).unwrap_or("");
            let path = e.data.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let content = if path.is_empty() {
                format!("[外部] {what}")
            } else {
                format!("[外部] {what} {path}")
            };
            Some(json!({ "role": "user", "content": with_prefix(prefix, &content) }))
        }
```

- [ ] **Step 4: 跑取侧测试验证通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib context::tests::with_prefix context::tests::user_message_gets_prefix context::tests::subagent_result_gets_prefix context::tests::external_gets_prefix context::tests::user_message_no_prefix_field`
Expected: 全 passed（含回归保护 `user_message_no_prefix_field_means_no_prepend` + 现有 context 测试不破）

- [ ] **Step 5: 写落盘失败测试（agent.rs）**

在 `agent.rs` 的 `mod tests`（文件末尾，挨着现有测试）加。注意 `maybe_snapshot_prefix` 接 `&mut HistoryEvent` + `&Config`：

```rust
    #[test]
    fn snapshot_prefix_on_user_when_enabled() {
        let cfg = crate::config::Config {
            user_prompt_prefix_enabled: true,
            user_prompt_prefix: "PFX".into(),
            ..crate::config::Config::default()
        };
        let mut ev = crate::history::HistoryEvent::user(1000, "main", "hi", &[]);
        assert!(maybe_snapshot_prefix(&mut ev, &cfg), "enabled+非空 user 事件应落 prefix");
        assert_eq!(ev.data.get("prefix").and_then(|v| v.as_str()), Some("PFX"));
    }

    #[test]
    fn no_snapshot_when_disabled() {
        let cfg = crate::config::Config::default(); // enabled=false
        let mut ev = crate::history::HistoryEvent::user(1000, "main", "hi", &[]);
        assert!(!maybe_snapshot_prefix(&mut ev, &cfg));
        assert!(ev.data.get("prefix").is_none(), "disabled 不落 prefix");
    }

    #[test]
    fn no_snapshot_when_prefix_empty() {
        let cfg = crate::config::Config {
            user_prompt_prefix_enabled: true,
            user_prompt_prefix: "   ".into(),
            ..crate::config::Config::default()
        };
        let mut ev = crate::history::HistoryEvent::user(1000, "main", "hi", &[]);
        assert!(!maybe_snapshot_prefix(&mut ev, &cfg), "prefix 空白不落");
        assert!(ev.data.get("prefix").is_none());
    }

    #[test]
    fn no_snapshot_on_marker() {
        let cfg = crate::config::Config {
            user_prompt_prefix_enabled: true,
            user_prompt_prefix: "PFX".into(),
            ..crate::config::Config::default()
        };
        let mut ev = crate::history::HistoryEvent::marker(1000, "reset", 5);
        assert!(!maybe_snapshot_prefix(&mut ev, &cfg), "marker 事件不落 prefix");
        assert!(ev.data.get("prefix").is_none());
    }

    #[test]
    fn snapshot_on_all_user_role_kinds() {
        let cfg = crate::config::Config {
            user_prompt_prefix_enabled: true,
            user_prompt_prefix: "PFX".into(),
            ..crate::config::Config::default()
        };
        let cases = [
            crate::history::HistoryEvent::user(1000, "main", "u", &[]),
            crate::history::HistoryEvent::subagent_result(1000, "1", "s", "r"),
            crate::history::HistoryEvent::external(1000, "拖", "f", None),
        ];
        for mut ev in cases {
            let kind = ev.kind.clone();
            assert!(maybe_snapshot_prefix(&mut ev, &cfg), "kind={kind} 应落 prefix");
            assert_eq!(ev.data.get("prefix").and_then(|v| v.as_str()), Some("PFX"));
        }
    }
```

- [ ] **Step 6: 跑测试验证失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib agent::tests::snapshot_prefix agent::tests::no_snapshot`
Expected: 编译错误 `cannot find function `maybe_snapshot_prefix``

- [ ] **Step 7: 实现 `maybe_snapshot_prefix` + `handle_event` 调用**

在 `agent.rs`（`handle_event` 函数之前，挨着 `inject_text` / `jobdone_body` 那些 helper 区）加纯函数：

```rust
/// 用户提示词前缀快照落盘：仅 user-role 事件（user/subagent_result/external）且 enabled 且 prefix 非空时，
/// 把当前 config 的 prefix 文本快照写进 data["prefix"]。返回是否写入。
/// 纯函数（不碰 history writer / IO），便于单测；handle_event 在 append 前调用。
pub fn maybe_snapshot_prefix(current: &mut crate::history::HistoryEvent, cfg: &Config) -> bool {
    if matches!(current.kind.as_str(), "user" | "subagent_result" | "external")
        && cfg.user_prompt_prefix_enabled
        && !cfg.user_prompt_prefix.trim().is_empty()
    {
        current.data.insert("prefix".into(), serde_json::json!(cfg.user_prompt_prefix));
        true
    } else {
        false
    }
}
```

在 `handle_event` 里，找到 `current.seq = history.current_seq();`（`agent.rs:99`）那行，**紧跟其后**加一行调用：

```rust
    current.seq = history.current_seq();
    maybe_snapshot_prefix(&mut current, cfg);
```

- [ ] **Step 8: 全量测试 + cargo check**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib` && `cargo check --manifest-path src-tauri/Cargo.toml`
Expected: 全 passed（含 Task 1 的 config 测试 + 本 task 新测试 + 现有 context/agent 回归全绿）；check 零 warning。
**重点确认**：现有 `context::tests` 的 ~15 个 `build_messages` 测试全绿（签名没动、测试事件无 prefix 字段 → 不拼 → 行为同前）。

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/context.rs src-tauri/src/agent.rs
git commit -m "feat(context,agent): prefix 落盘 data[\"prefix\"] + event_to_message 取侧拼接

- with_prefix helper + event_to_message 三分支（user/subagent_result/external）读 data[\"prefix\"] 拼
- maybe_snapshot_prefix 纯函数：handle_event append 前给 user-role 事件快照 prefix
- build_messages 签名不动；dream 读 data.text 不受影响

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 3: 前端配置页（textarea + checkbox + load/save）

**Files:**
- Modify: `src/index.html`（「对话 (LLM)」group 内，system_prompt label 后）
- Modify: `src/main.js`（`F` 对象 :113 区 + `fillForm` + `readForm` :445 区）

**Interfaces:**
- Consumes: `save_config` / `get_config` 既有命令（字段集合 +2：`user_prompt_prefix_enabled` / `user_prompt_prefix`）。无新命令。

- [ ] **Step 1: index.html 加 form-group**

找到「对话 (LLM)」group 里 system_prompt 的 label（`<textarea id="cfg-system-prompt" rows="3"></textarea>` 所在 label），在其 `</label>` 之后、group 的 `</div>` 之前加：

```html
              <label class="row">
                <span>用户提示词前缀 <input id="cfg-user-prefix-enabled" type="checkbox" style="margin-left:6px;vertical-align:middle"> 启用</span>
              </label>
              <label class="row row-col">
                <span>前缀文本（每条用户输入发给助手前拼接；典型：做事与思考逻辑）</span>
                <textarea id="cfg-user-prefix" rows="4" spellcheck="false"></textarea>
              </label>
```

- [ ] **Step 2: main.js 加 F 对象引用**

`F` 对象（`const F = { ... };`，结尾是 `maxToolIters: document.getElementById("cfg-max-tool-iters"),` 然后 `};`）。把：

```js
  maxToolIters: document.getElementById("cfg-max-tool-iters"),
};
```

改为：

```js
  maxToolIters: document.getElementById("cfg-max-tool-iters"),
  userPrefix: document.getElementById("cfg-user-prefix"),
  userPrefixEnabled: document.getElementById("cfg-user-prefix-enabled"),
};
```

- [ ] **Step 3: main.js 加 fillForm 映射（load）**

`fillForm` 函数里找到 `F.systemPrompt.value = cfg.system_prompt || "";` 那行，紧跟其后加：

```js
  F.userPrefix.value = cfg.user_prompt_prefix || "";
  F.userPrefixEnabled.checked = !!cfg.user_prompt_prefix_enabled;
```

- [ ] **Step 4: main.js 加 readForm 映射（save）**

`readForm` 函数 return 的对象里找到 `system_prompt: F.systemPrompt.value,` 那行，紧跟其后加：

```js
    user_prompt_prefix: F.userPrefix.value,
    user_prompt_prefix_enabled: !!F.userPrefixEnabled.checked,
```

- [ ] **Step 5: 语法检查 + cargo build**

Run: `node --check src/main.js`
Expected: 无输出（语法 OK）

Run: `cargo build --manifest-path src-tauri/Cargo.toml` （前端 src/ 经 generate_context! 编译进 binary）
Expected: 编译成功零 warning。

- [ ] **Step 6: Commit**

```bash
git add src/index.html src/main.js
git commit -m "feat(ui): 设置页加用户提示词前缀（textarea + 启停 checkbox）

进 config-form 表单 → 自动避开 save 回传坑（main.js readForm 显式 return 两字段）

Co-Authored-By: Claude <noreply@anthropic.com>"
```

- [ ] **Step 7: 手动验证（用户侧，在 tauri dev 或 build.ps1 出的 portable 上）**

验证清单：
1. 设置页「对话 (LLM)」group 出现「用户提示词前缀 启用」checkbox + textarea。
2. 勾选启用，填入 `TEST_PREFIX`，保存（注意：保存会触发 reset_session 清空当前对话——这是现有 save 行为，UI 文案若需可后补）。
3. 发一条用户消息，确认助手收到的 context 里该 user 消息 content 前带 `TEST_PREFIX\n`（可通过 mem CLI drill history jsonl 看 `data["prefix"]` 字段，或观察助手行为是否符合 prefix 指令）。
4. 发第二、三轮，确认每条 user 消息都带 prefix。
5. 拖一个文件进来（external 事件），drill 确认也带 `data["prefix"]`。
6. 关闭启用，保存，新发消息 drill 确认无 `data["prefix"]`（老消息保留）。
7. 前端气泡显示原文（不含 prefix）。

---

## Self-Review（plan 写完后自查）

**1. Spec 覆盖：**
- spec §3 数据结构 `data["prefix"]` → Task 2 Step 7 `maybe_snapshot_prefix` ✓
- spec §4 落盘侧 handle_event → Task 2 Step 7 ✓
- spec §5 取侧 event_to_message 三分支 + with_prefix → Task 2 Step 3 ✓
- spec §6 config 字段 → Task 1 ✓
- spec §7 前端表单 + save 复用 → Task 3 ✓
- spec §11 测试策略 → Task 1（config 3 测试）+ Task 2（with_prefix 2 + 取侧 4 + 落盘 5）✓

**2. 占位扫描：** 无 TBD/TODO；每步都有 complete code 或 exact 命令。前端 HTML/JS 给完整片段 + 内容锚点（`F.systemPrompt` / `cfg-system-prompt`）。

**3. 类型一致性：**
- `Config.user_prompt_prefix_enabled`（bool）/ `user_prompt_prefix`（String）—— Task 1 声明，Task 2 `maybe_snapshot_prefix` 读，Task 3 `readForm` 写，名字全文一致 ✓
- `with_prefix(prefix: &str, text: &str) -> String` —— Task 2 Step 3 定义、Step 1 测试调用，签名一致 ✓
- `maybe_snapshot_prefix(current: &mut HistoryEvent, cfg: &Config) -> bool` —— Step 7 定义、Step 5 测试调用、handle_event 调用，签名一致 ✓
- `data["prefix"]` —— 落盘（Task 2 Step 7 insert）与取侧（Task 2 Step 3 get）字段名一致 ✓

**4. 风险复核：**
- `config.rs:297` 全字段 literal 同步 → Task 1 Step 5 显式处理 ✓
- `build_messages` 签名不动 → 全文未改 `context.rs:89` ✓
- dream 不受影响 → 不改 `mem_dream.rs`，Task 2 Step 8 回归确认 ✓

无问题，plan 可执行。
