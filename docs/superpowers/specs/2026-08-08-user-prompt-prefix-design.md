# 用户提示词前缀（User Prompt Prefix）设计

> 日期：2026-08-08 ｜ 分支：`feat/user-prompt-prefix` ｜ 状态：设计定稿，待实现

## 1. 背景与动机

用户希望在每条发给 LLM 的 **user-role 输入**前，自动拼接一段自定义指令前缀（典型用途：一段固定的「做事与思考逻辑」工作方法论），让 LLM 在每次 user 输入时都带上这套框架。

现状：
- `config.rs:20` 已有 `system_prompt` 字段（助手人设），但它语义是全局 system 块，不是「每条用户输入前的提醒」。
- 用户要的是 **per-message、拼到 user 消息 content 前**，且对所有 user-role 来源（输入框 / 后台回调 / 拖入 / 助手自请求图）都生效。
- ovoice 的核心铁律（`agent.rs:1-3`）：**history 是唯一真相之源，messages 是每轮从 history 重建的派生视图**。因此 prefix 必须落盘 history，否则 history 与实际发给 LLM 的内容不一致，重建会丢信息。

## 2. 目标 / 非目标

**目标：**
- 每条 user-role 消息发给 LLM 前，content 前拼一段可配置的 prefix。
- 覆盖所有 user-role 来源：用户输入框（含语音转写、定时器预设）、用户主动发图、后台 process 任务回调、子代理结果回灌、拖入文件 / ContextNote、助手经 attach 工具自请求的图。
- prefix 可在配置页设置文本 + 启停开关。
- prefix **落盘 history**（`data["prefix"]` 独立字段），忠实记录、可被 mem CLI drill 回看。
- `data["text"]` 仍存用户原文，前端气泡显示原文（不显示 prefix）。

**非目标（YAGNI）：**
- 不改 `system_prompt` 语义（人设仍走 system 块）。
- 不进子代理 session（`subagents.rs:415` 独立 system，不经 `build_messages`）。
- 不做 prefix 模板 / 变量替换 / 多套 prefix 切换。
- 不把 prefix 注入 system 块（语义不符：用户要的是「拼到用户对话前」）。

## 3. 数据结构

`HistoryEvent.data`（`history.rs:14-22` 的 `serde_json::Map`）新增**可选**字段：

| 字段 | 类型 | 落盘条件 |
|---|---|---|
| `data["prefix"]` | string | 仅 user-role 事件（`kind ∈ {user, subagent_result, external}`）且 `cfg.user_prompt_prefix_enabled && !prefix.trim().is_empty()` |

- `data["text"]` 保持原文，**不受 prefix 影响**。
- 旧 history 文件无此字段 → rebuild 时 `data.get("prefix")` 返回 `None` → 不拼 → **完全向后兼容**。

## 4. 落盘侧（handle_event，一处统一）

`agent.rs:61` 的 `handle_event` 在 `match` 构造完 `current`、赋 `current.seq` 之后（`agent.rs:99` 之后）、`history.append` 之前，统一插入：

```rust
// user-role 事件快照 prefix（cfg 在 handle_event scope：fn 签名收 cfg: &Config）
if matches!(current.kind.as_str(), "user" | "subagent_result" | "external")
    && cfg.user_prompt_prefix_enabled
    && !cfg.user_prompt_prefix.trim().is_empty()
{
    current.data.insert("prefix".into(), serde_json::json!(cfg.user_prompt_prefix));
}
```

**一处覆盖所有 5 个 `SessionEvent` 分支**（`agent.rs:72-95`）：
- `UserMessage` → kind=`user`
- `InjectAttachment` → kind=`user`（助手自请求图，复用用户管道，`agent.rs:89-91`）
- `JobDone(Agent)` → kind=`subagent_result`（`agent.rs:82-83`）
- `JobDone(Process)` → kind=`external`（`agent.rs:84`）
- `ContextNote` → kind=`external`（`agent.rs:78-79`）
- `Reset` → kind=`marker`（不匹配，不落 prefix ✓）
- `DreamCheck` → 不进 `handle_event`（`agent.rs:94` unreachable）

`cfg` 已在 `handle_event` 签名 scope（参数 `cfg: &Config`，`agent.rs:67`），无需额外传递。

## 5. 取侧（context.rs event_to_message）

`context.rs:111` 的 `event_to_message` 三个 user-role 分支读取 `data["prefix"]` 并拼接。新增 helper：

```rust
/// prefix 非空时拼到 text 前（中间换行）；空串原样返回。
fn with_prefix(prefix: &str, text: &str) -> String {
    if prefix.trim().is_empty() {
        text.to_string()
    } else {
        format!("{prefix}\n{text}")
    }
}
```

三个分支改动：

**`"user"` 分支（`context.rs:113-118`）** —— 在进 `user_message_with_attachments` 之前拼（此时 text 还是纯字符串，不碰多模态数组结构）：
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

**`"subagent_result"` 分支（`context.rs:151-156`）**：
```rust
"subagent_result" => {
    let prefix = e.data.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
    let id = e.data.get("agent_id").and_then(|v| v.as_u64()).unwrap_or(0);
    let summary = e.data.get("summary").and_then(|v| v.as_str()).unwrap_or("");
    let refs = e.data.get("ref").and_then(|v| v.as_str()).unwrap_or("");
    Some(json!({ "role": "user", "content": with_prefix(prefix, &format!("[子代理 #{id} 完成] {summary} [完整对话: {refs}]")) }))
}
```

**`"external"` 分支（`context.rs:157-167`）**：
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

**`build_messages` 签名不动**（`context.rs:89`），prefix 完全来自 history，~15 个现有测试调用零改动。

`"tool_result"`（role=tool）、`"assistant"`、`"error"` 不拼 prefix（不是 user role）。

## 6. config 字段（config.rs）

新增两字段，挨着 `system_prompt`（`config.rs:20`）声明：

```rust
// 用户提示词前缀开关：开启后每条 user-role 消息发给 LLM 前 content 前拼 user_prompt_prefix。
#[serde(default)]
pub user_prompt_prefix_enabled: bool,
// 用户提示词前缀文本（典型用途：做事与思考逻辑；每条 user-role 输入前拼接，落盘 data["prefix"]）。
#[serde(default)]
pub user_prompt_prefix: String,
```

- `Default` impl（`config.rs:167-208`）补：`user_prompt_prefix_enabled: false, user_prompt_prefix: String::new(),`
- `#[serde(default)]` per-field → 旧 `config.json` 缺这俩字段走默认 → **向后兼容**。

⚠ **编译强制陷阱**：`config.rs:297` `roundtrip_preserves_all_fields` 是全字段 struct literal，加字段不同步改 → 编译失败。必须同步：
- struct literal 补两字段（`:298-335` 区）
- 加断言 `assert!(!back.user_prompt_prefix_enabled);` + `assert_eq!(back.user_prompt_prefix, "");`

## 7. 前端

**`index.html`**（`settings-scroll` 内，`:88` 区，挨着 system_prompt 的 form-group）加一个 form-group：
- `<textarea id="cfg-user-prefix">`（prefix 文本）
- `<input type="checkbox" id="cfg-user-prefix-enabled">`（启停开关）

**`main.js`**：
- load（`:425` 区，挨着 `F.systemPrompt.value = cfg.system_prompt`）：补 `F.userPrefix.value = cfg.user_prompt_prefix || "";` + `F.userPrefixEnabled.checked = !!cfg.user_prompt_prefix_enabled;`
- save（`:463` 区，挨着 `system_prompt: F.systemPrompt.value`）：补 `user_prompt_prefix: F.userPrefix.value,` + `user_prompt_prefix_enabled: F.userPrefixEnabled.checked,`
- 字段映射对象 `F`（`:92` 区「配置表单字段」）补两个元素引用。

**进表单即自动避开 `main.js:491` 的 save 回传坑**（非表单字段才会被清默认；prefix 进表单 → save 时正常提交）。

**save 路径决策**：复用现有 `save_config`（`lib.rs:248` / 注册 `lib.rs:917`），**不新增命令**。代价：`main.js:1038-1039` 保存后触发 `reset_session`（清当前对话 history）。理由：
- prefix 落盘是 per-message 快照，功能正确性不依赖 reset（reset 后新对话 user 消息会用新 prefix 快照）。
- settings 页统一一个保存按钮，体验一致。
- reset 副作用仅 UX 层（改 prefix 会清当前对话），UI 文案提示「保存会清空当前对话」即可。
- 新增独立 save 命令绕开 reset 是后续优化，YAGNI。

## 8. 影响面

**受影响需回归（2 处，低风险）：**
- `build_messages` 重建路径：`event_to_message` 行为变了（user-role 分支多拼 prefix）。现有 context 测试应全绿（测试事件无 `data["prefix"]` → 不拼 → 行为同前）。
- 前端 save 链：`save_config` 字段集合 +2。

**不受影响（零改动零回归）：**
- **dream 提取**：`mem_dream.rs:913` / `:1165` 读 `e.data.get("text")`（原文），不碰 `data["prefix"]` → MEMORY.md 不含 prefix。
- **子代理 session**：`subagents.rs:415` 独立 system，不经 `build_messages`。
- **M3 缓存**：稳态中性（见 §9）。
- **history 读路径**：`read_all` / `read_tail` 不碰 prefix 字段。
- **TTS / ASR / 热键 / 定时器 / dream 触发**：与 message 构造无关。
- **`build_messages` 签名**：不动 → ~15 个测试调用零改动。

## 9. 缓存分析（M3 被动缓存）

M3 缓存 = prompt 序列化后从前缀连续 token 匹配，命中部分按缓存价计费。

- **稳态（prefix 不变）命中率不受影响**：
  - `system` 块（SYS+SOUL+AGENT+MEMORY，prefix 不在此）每轮稳定 → 永远命中。
  - 历史 N-1 条 user 里的 PREFIX 是稳定前缀的一部分 → 连续命中。
  - 只有最后一条新 user 的 `PREFIX+正文` 全 miss（这本就是新增内容，无 prefix 也 miss）。
  - 净效果：命中率与无 prefix 一致，甚至微正（PREFIX 稳定文本抬高命中占比）。
- **代价**：prompt 体积随轮数线性增长（每条 user 多带一份 PREFIX）。历史部分走缓存价（约 1/4），便宜；dream 在 300k token 触发封顶，保证 N 有上限。PREFIX 若 ~300 字，N≈150 轮时每轮多付的主要是缓存价累积，量级 <5% context。
- **prefix 变化**：从第一条新 PREFIX 消息开始断裂 → 该点之后 miss（改 prefix 的必然一次性代价）。
- **dream 后 rebuild**：`system` 块因 MEMORY.md 更新而变 → system miss（dream 本身代价，非 prefix 引入）；cut 之后消息含 PREFIX 快照作新缓存基底，无额外问题。

## 10. 边界语义

- **enabled 关闭**：老消息已有 `data["prefix"]` 快照 → rebuild 仍拼（忠实历史）；新消息 enabled=false → 不落 prefix → 不拼。
- **prefix 文本变化**：老消息保留旧快照、新消息带新快照，混合 rebuild 各读自己的 → 合法。
- **dream**：读 `data["text"]` 不碰 prefix，提取出的 MEMORY.md 不含 prefix（正确：prefix 是元指令不是对话内容）。
- **空 prefix / disabled**：不落 `data["prefix"]`，rebuild 时 `None` → 不拼 → 系统回到现状。
- **助手自请求图（InjectAttachment）**：kind=user，会落 prefix（用户明确要「都拼」）。
- **向后兼容**：旧 history jsonl 无 `data["prefix"]` → rebuild 不拼 → 旧二进制读新 history 也安全（新字段是 data map 的额外 key，不影响反序列化）。

## 11. 测试策略

**config.rs：**
- `user_prompt_prefix_disabled_and_empty_by_default`：`Config::default()` 两字段为 false / ""。
- `user_prompt_prefix_roundtrip`：设值后 serialize→deserialize 保真。
- `missing_fields_use_defaults`（`:361` 现有）自然覆盖（`{}` → 默认）。
- ⚠ 同步改 `roundtrip_preserves_all_fields`（`:297`）的 struct literal + 断言。

**agent.rs（handle_event 落盘）：**
- `prefix_snapshotted_on_user_event_when_enabled`：UserMessage + enabled + 非空 → `data["prefix"]` 落盘。
- `prefix_not_snapshotted_when_disabled`：enabled=false → 无 `data["prefix"]`。
- `prefix_not_snapshotted_on_reset_marker`：Reset 事件 → 无 `data["prefix"]`（marker 不匹配）。
- `prefix_snapshotted_on_all_user_role_kinds`：subagent_result / external / InjectAttachment 各落一份。

**context.rs（event_to_message 取）：**
- `with_prefix_prepends_when_non_empty` / `with_prefix_passthrough_when_empty`：helper 单测。
- `user_message_gets_prefix` / `subagent_result_gets_prefix` / `external_gets_prefix`：三分支带 `data["prefix"]` → content 前拼。
- `no_prefix_field_means_no_prepend`：无 `data["prefix"]` → 行为同前（回归保护）。
- `empty_prefix_string_means_no_prepend`：`data["prefix"]=""` → 不拼。

**回归：** 现有 `context.rs` ~15 个 `build_messages` 测试 + `agent.rs` handle_event 测试全绿（签名未变，无 prefix 字段 → 不拼）。

## 12. 风险评估

**风险等级：低。**
- `build_messages` 签名零变更 → 不会大面积破测试。
- 落盘是加字段（`serde(default)`）→ 旧 history / 旧 config.json 不破。
- 取侧是 `event_to_message` 内部加逻辑 → 隔离，不扩散。
- 最大已知坑 = `main.js:491` save 回传（进表单即避）+ `config.rs:297` 全字段 literal（漏改硬编译错，好抓）。
- **完全可逆**：`enabled=false` + 不落 prefix 字段 → 系统回到现状。

## 13. 改动清单

| 文件 | 改动 | 量级 |
|---|---|---|
| `src-tauri/src/config.rs` | +2 字段 + Default + 改 `:297` 测试 + 新字段测试 | ~25 行 |
| `src-tauri/src/agent.rs` | `handle_event :99` 后 +5 行插 `data["prefix"]` + 落盘测试 | ~30 行 |
| `src-tauri/src/context.rs` | `with_prefix` helper + 三分支读 prefix + 取侧测试 | ~40 行 |
| `src/index.html` | settings-scroll +form-group（textarea + checkbox） | ~10 行 |
| `src/main.js` | `F` 映射 + load（`:425`）+ save（`:463`） | ~6 行 |

产品码 ~50 行 + 测试 ~50 行 + 前端 ~25 行 ≈ **125 行**。2 个 task，半天级。
