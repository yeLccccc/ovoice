# attach：助手主动把图 / pdf / docx 纳入上下文（P-2026-005）

- **日期**：2026-07-29（v2，重写）
- **提案**：`workspace/projects/ovoice-test/proposals/P-2026-005_助手主动添加图片附件能力.md`
- **关联**：P-2026-002（edit_card，已合并 master `a29c4d3`）
- **分支**：`feat/attach-file`（基于 master `a29c4d3`）

> **v2 变更**：v1（`c53752c`）用独立 `kind="attach"` 事件 + 让所有类型都走"注入 user 事件"。v2 按用户决定重写——**按文件类型分流两条路**，且图片复用"用户拖图"现成管道。同时撤回 v1 里越界的"自检次数上限"（那是 agent loop 的职责，不归工具）。

## 决定（用户拍板）

**一个工具 `attach(path, caption="")`，后端按文件类型分流**：

| 类型 | 工具单次调用做什么 | 助手何时看见 |
|---|---|---|
| **pdf / docx** | 抽文本 → tool result 直接带文本返回 | **当轮**（文本在 role:tool 合法） |
| **image** | 落盘 + 触发系统注入一条带图 user 消息 | **下一轮**（系统以 user 身份喂图） |
| **.doc** | 报错引导转 .docx | — |

**明确不归 attach 管**（agent loop / tool use loop 的职责）：
- 自检循环几轮停、token 预算、烧 token 防护 —— 这些是 `run_turn` 循环层的事，attach 只负责"单次调用把文件纳入视野"这一件事。

## 现状（缺口定位 + 范围判据）

### 用户拖图管道（已铺好，image 路径直接复用）

```
用户拖图 → stage_one(tools.rs:305) 落 attachments/<hash8>.<ext>   ← SHA256 去重
  → chat 带 AttachmentRef{staged_path,kind}(lib.rs:330) 落 history(kind=user)
  → 下一轮 build_messages(context.rs:75)
  → user_message_with_attachments(llm.rs:69) 图 → image_ref(llm.rs:76)
  → expand_messages_for_send(llm.rs:110) image_ref → base64 image_url
  → M3 在 user role 看见
```

### read 的边界 = attach 的范围判据

`tool_read`（tools.rs:156）遇二进制（含 `\0` 字节）直接拒"read 仅支持文本"：
- **read 管**：txt/md/csv/json/code（纯文本）
- **attach 管**：图片 + pdf + docx（二进制，read 搞不定）
- **attach 不管**：`.doc`（老 OLE 二进制，`doc_kind_from_ext` lib.rs:335 当前 Unsupported，MVP 剔除）

### display 的缺口 = image 路径的存在理由

`tool_display`（tools.rs:170）只返回 `{display:true,path,kind}` 给前端渲染——不 stage、不落 history。下一轮重建不出来 → 助手自己产/展示的图看不见。attach 的 image 路径补上这个缺口。

### API 约束（决定 image 必须走 user-role 注入）

- `image_url` **只在 user role 可靠工作**（实测：curl 直打 ovoice 端点，M3 精确描述测试图细节）
- **tool result（role:tool）带 image_url 不被识别**；ovoice 当前 tool result 就是纯字符串（context.rs:138），`expand` 也只展开 `role=="user"`（llm.rs:117）
- **推论**：
  - pdf/docx 抽的**文本**可以走 tool result（文本在 role:tool 合法）→ 当轮可见
  - **图片不能**走 tool result → 必须由后端注入一条 user-role 消息 → **下一轮** user role 可见

## 范围

**attach 接受**：
- `image`（png/jpg/webp/...）→ 落盘 + 触发系统注入带图 user 消息 → 下轮 M3 vision
- `pdf` / `docx` → `extract::doc_text`（extract.rs:17）抽文本截断 → tool result 文本 → 当轮可见

**明确排除**：
- txt/md/csv/json/code → read 覆盖
- `.doc`（老二进制）→ 报错"请转 .docx"
- 视频 → 留增量（video_ref 管道已就绪，MVP 不开放）
- `as_reference` 参考图池 / mmx 自动复用 → P-2026-006

## 设计

### 1. 工具定义（tools.rs，新工具 `attach`）

```
attach(path: str, caption: str = "")
```
- `path`：本地文件（mmx 产物 / 任意路径），workspace 相对或绝对，必填
- `caption`：可选文本说明（pdf/docx 不用；image 进系统注入消息的文本段 + 渲染到卡片）

**`tool_attach` 行为**：
1. `resolve_path` + 校验存在
2. `file_kind` 判定 → 只接受 Image / Pdf / Docx；其它（含 .doc）→ 错误引导
3. **按 kind 分流**（见 §2 / §3）

**工具数**：7→8，同步改 `tools.rs:772`（`schemas_has_seven_tools`）+ `llm.rs:681`（build_body 断言）（[[ovoice-tool-count-cascade]]）。

### 2. pdf / docx 路径（简单：tool result 带文本）

```
tool_attach:
  text = extract::doc_text(path)?         // extract.rs:17，pdf_text / docx_text
  text = truncate(text, DOC_TEXT_MAX)     // llm.rs:47，200KB
  return tool_result(text)                // 普通字符串 tool result，当轮可见
```

**文本在 role:tool 完全合法**，不需要注入 user 消息、不需要新事件 kind——和 read 的返回模式一致。history 层零改动（复用现成 tool_result 事件）。

### 3. image 路径（核心：系统注入带图 user 消息）

```
Turn N（assistant 自检）:
  assistant: tool_call(attach, path=render/x.png, caption="我生成的草图")
    ↓ tool_attach 执行：
       1. stage_one(tools.rs:305) 落 attachments/<hash8>.png     ← 和用户拖图同入口、同去重
       2. tool result 返回 {attached:true, kind:image,
                             note:"图片下一轮以附件纳入视野"}
          （不带图——绕开 tool-role-不带图限制）
       3. 登记"待注入附件"（AttachmentRef{staged_path, kind:image}）+ 触发系统注入
    ↓ assistant 收到 result，知道图下轮才见 → 结束当前 turn
       （"何时停"是 agent loop 的职责；AGENT.md 只告知"图片下轮见"这个事实）

Turn N+1（系统注入，无需用户打字）:
  系统构造一条 user 消息：
    text = "[系统：助手通过 attach 请求纳入以下图片]" + (caption ? "：{caption}" : "")
    attachments = [AttachmentRef{staged_path, kind:image}]
  → 落 history（kind=user 带 attachments，与用户拖图同种事件）
  → build_messages → user_message_with_attachments(llm.rs:69) → image_ref(llm.rs:76)
  → expand base64(llm.rs:110) → M3 在 user role 看见 ✅
```

**为什么这样咬合最紧**：image 路径**完全复用用户拖图管道**（stage_one → AttachmentRef → kind=user → user_message_with_attachments → image_ref → expand），零新增内容处理逻辑。系统注入的 user 消息和用户拖的图，对 M3 来说长得一模一样——只是触发者从"用户拖"变成"系统代表助手意图"。

### 4. 系统注入带图 user 消息（机制，attach 自建最小版）

ovoice 现有 turn 由"用户发话 / job done / subagent done"触发，**没有"工具结束后系统主动发一个带图 user turn"的入口**。attach 自建一个最小版，不依赖任何其他特性：

- **触发**：`tool_attach`（image 分支）执行后，通过 `ToolsCtx` / emitter 往 driver 发一个 `InjectAttachment(AttachmentRef, caption)` 信号
- **driver 收到**（agent.rs select 循环加一个分支）：当前 turn 结束后，构造带图 user 消息 → append history（kind=user）→ 发起一个新 turn
- **不冒充用户**：注入的 user 文本必须中性（"[系统：助手通过 attach 请求纳入以下图片]"），**不能**写成"请描述这张图"——那是借用户之口下指令，会污染对话语义。M3 要明白：图是助手自己要看，响应要接 assistant 上一轮的自检意图。

**live vs history 双路径**（[[ovoice-live-vs-history-render-paths]]）：系统注入的 user 消息带图，前端渲染复用用户附件图卡（`renderDropped` 那套）。live（setupAgentEvents）和 history（buildHistoryBubbles）都要正确渲染——但因为复用现成图卡代码，改动点在"系统注入的 user 消息也要走图卡渲染分支"。

### 5. .doc 处理

遇 `.doc`：tool result 返回错误 `".doc 旧二进制格式不支持，请转成 .docx 后再 attach"`。不做 .doc 文本提取（OLE 复合格式，Rust 生态支持弱，MVP 不含）。

### 6. AGENT.md（defaults/AGENT.md）

加 `attach` 段：
- **用途**：把 read 搞不定的图/pdf/docx 纳入助手视野（自检、对照、再生成）
- **与 read 区别**：read=纯文本；attach=图 + 二进制文档
- **与 display 区别**：display=给用户看（不进历史）；attach=助手也纳入视野
- **关键行为告知**：
  - pdf/docx：当轮就能在 tool result 里看到文本
  - **image：本轮看不到，系统下一轮以附件形式注入后才能看到**——调完 attach(image) 不要死等，正常结束当前回合，下轮系统会把图喂进来
- **自检循环控制**（几轮停、不满意如何处理）**不在此处规定**——那是 agent loop 的职责，AGENT.md 只描述 attach 工具本身的行为。

## 不做（YAGNI）

- **自检循环上限 / token 预算 / 烧 token 防护** → agent loop（`run_turn`）的职责，不归工具
- `as_reference` 参考图池 / mmx 生图自动复用 → P-2026-006
- 视频附件（video_ref 管道已就绪，MVP 不开放）
- `.doc`（老二进制，剔除）
- txt/md/csv（read 覆盖）
- 图片客户端 resize（`stage_one` 已有 `IMG_MAX_BYTES` 兜底，超了报错引导；M3 对常见尺寸够用，等真遇拒收再加）

## 验收映射

| 提案验收 | MVP |
|---|---|
| #1 助手能主动 attach | ✅ attach |
| #2 挂入后助手能引用图内容 | ✅（image 系统注入 user 消息 + M3 vision） |
| #3 mmx 参考图自动复用 | ❌ P-006 |
| #4 用户视角每次 attach 都看到图 | ✅（image 复用图卡渲染；pdf/docx 复用文本 tool result） |

## 改动清单

| 文件 | 改动 |
|---|---|
| `tools.rs` | 新 `tool_attach` + schema（按 kind 分流）；工具数断言 7→8（line 772） |
| `llm.rs` | build_body 工具数断言 7→8（line 681） |
| `agent.rs` | driver select 循环加 `InjectAttachment` 分支：收信号 → 构造带图 user 消息 → append history → 触发新 turn |
| `lib.rs` | `ToolsCtx` / emitter 加发 `InjectAttachment` 信号的通道；系统注入 user 消息的 emit 接线 |
| `history.rs` | **零改动或极小**：image 复用 `kind=user`（带 attachments），pdf/docx 复用 `tool_result`——不引入新 kind（v1 的 `kind="attach"` 作废） |
| `src-tauri/defaults/AGENT.md` | attach 工具段（含 image 下轮见的行为告知） |
| `src/main.js` | 系统注入的 user 消息走图卡渲染分支（复用 `renderDropped`）；pdf/docx tool result 走文本渲染 |
| `src/styles.css` | 若需要：attach 相关卡片刻意区分（可选，复用现成图卡/doc 卡样式则零改动） |

**相比 v1 的简化**：history 层从"新 kind + helper + event_to_message 分支"收窄到"零改动或极小"；image/pdf 都复用现成事件类型。

## 验证

1. `cargo check --tests` + `cargo test --lib`（context/llm/tools/agent tests）+ `node --check src/main.js`（dev server 锁 exe，[[ovoice-dev-server-cargo-lock]]）
2. TDD：
   - `tool_attach` 分流：image → 落盘 + 登记注入 + result 不带图；pdf/docx → result 含抽文本；.doc/txt → 拒绝
   - driver `InjectAttachment` 分支：收信号 → append kind=user 带 attachments → 触发 turn
   - 工具数断言 = 8
   - 回归：现有用户拖图管道不受影响（image 注入走同一管道）
3. 手测（dev server / release）：
   - agent `attach` 一张 mmx 产物图 → 下一轮助手能描述图内容（证明系统注入 user 消息 + M3 看见）
   - agent `attach` 一个 pdf → **当轮** tool result 含文本，助手能引用
   - agent `attach` 一个 .doc → 报错引导转 docx
   - 历史重载：系统注入的 user 图卡正常渲染（live/history 双路径）

## 约束

- 分支 `feat/attach-file`（基于 master `a29c4d3`），严禁直接落 master；merge `--no-ff`。
- dev server 锁 exe → `cargo check --tests` / `cargo test --lib` / `node --check`。
- 数据保留（D2）：系统注入的 user 消息 append-only 落 history.jsonl（kind=user 带 attachments）。
- 工具数断言两处同步（[[ovoice-tool-count-cascade]]）。
- live vs history 双路径（[[ovoice-live-vs-history-render-paths]]）：系统注入 user 消息的图卡渲染两路径都要覆盖。
- 提交信息以 `Co-Authored-By: Claude <noreply@anthropic.com>` 结尾。

## 附：v1 → v2 决策记录

| 维度 | v1（c53752c） | v2（本文件） |
|---|---|---|
| 机制 | 所有类型都走独立 `kind="attach"` 事件，event_to_message 翻译成 user 消息 | **按类型分流**：pdf/docx=tool result 文本（当轮）；image=系统注入 user 消息（下轮） |
| 图片可见时机 | 下一轮 | 下一轮（同——API 限制不可避免） |
| pdf/docx 可见时机 | 下一轮（走注入） | **当轮**（tool result 文本，更直接） |
| history 改动 | 新 kind + helper + event_to_message 分支 | 零改动或极小（复用 user/tool_result） |
| 自检上限 | v1 含"最近 N=6 退化" + 隐含循环控制 | **删除**——agent loop 的职责，不归工具 |
| .doc | Unsupported（剔除） | Unsupported（剔除，不变） |
