# 聊天附件 + 拖放加载 — 设计规格

> 日期：2026-07-25 · 状态：已通过 brainstorming，待 writing-plans
> 前序：`feat/chat-media`（媒体卡 + `media://` 协议）、`feat/chat-documents`（`display`/`edit_file` 工具 + 渲染/编辑气泡 renderMediaCard/renderDocCard/renderEditCard）均已并入 master。本规格在此基础上给**用户**加上文件入口：附件按钮 + 拖放加载。

## 背景：MiniMax-M3 附件能力调研（官方文档核实）

ovoice 用 `MiniMax-M3`（`config.rs` 默认），原生多模态。OpenAI 兼容 Chat Completions 接口（`llm.rs::build_body` 走的就是这个形状）支持三类内容块：`text` / `image_url` / `video_url`。

| 类型 | 能否喂给模型 | 处理方式与硬限制 |
|---|---|---|
| 图片 jpg/png/gif/webp | ✅ | `image_url`，base64 或 URL，**≤10MB**；`detail` low/默认/high |
| 视频 mp4/avi/mov/mkv | ✅ | `video_url`，base64/URL **≤50MB**；请求体总上限 **64MB** |
| 音频 mp3/wav/m4a… | ❌ | 文档明说「audio input is not currently supported」 |
| pdf/docx/csv/doc | ❌ | 无对应内容块，不能作为附件直接读 |
| markdown/txt/代码 | ❌ 内容块 | 但本身是文本，可**内联**进消息 |

**关键结论：** 真正能让模型「看见」的附件只有图片和视频。其余只能内联成文本或走 agent 工具（给路径，让它 `read`/`display`/`edit_file`）。MiniMax 的 `/v1/files/upload` 接口 `purpose` 只有 `voice_clone`/`prompt_audio`/`t2a_async_input`（全是语音/TTS），**不是用来给聊天挂文档的** —— 故本设计不使用 Files API。

## 目标

给用户提供三种文件入口，把文件带进对话：

1. **附件按钮**（composer 里 📎）：选文件 → 作为附件随下一条消息发给模型。
2. **拖到输入框区**（`#chat-form`）：等价附件按钮 —— 加附件随消息发。
3. **拖到消息区**（`#messages`）：立刻在渲染/编辑气泡里**本地**打开（预览/播放/编辑），不经过模型；同时往对话历史追加一句静默上下文，让 agent 在**下一次**真正发消息时知道这件事（本次不触发模型）。

附件按类型分流到三种模型侧处理（见「已锁定决策 §1」）。

## 非目标（明确排除）

- **大文件**：超过尺寸上限（图片 >10MB / 其余 >30MB）一律**拒绝**，不做 Files API / `mm_file://` 上传链路（用户明确选择拒绝而非上传）。
- 音频转写喂模型：MiniMax 不支持音频输入；音频附件仅本地播放，模型听不到（已知限制，不解决）。
- `.doc`/`.xlsx` 等旧二进制格式的本地渲染（沿用 `display` 工具既有行为：命中时给提示卡）。
- 附件的跨设备同步 / 云存储（纯本地工作区副本）。
- 附件随消息历史的二进制持久化（历史只存 staged 路径引用，副本留在工作区）。

## 现状约束（设计前提）

- **消息体当前是纯文本**：`llm.rs::build_body`（约 22–33 行）发 `{model, messages, tools, tool_choice, reasoning_split, stream, stream_options}`，`messages` 全是 `{role, content:"字符串"}`。无任何多模态。
- **agent 已能处理任意文件类型**：`tools.rs` 的 `write`/`read`/`bash`/`display`/`edit_file`，`resolve_path`（tools.rs:9–12）按 workspace 解析相对/绝对路径。
- **类型判定可复用**：`lib.rs::media_kind_from_ext`（208–217）→ Image/Video/Audio/Unsupported；`doc_kind_from_ext`（220–243）→ Html/Pdf/Docx/Csv/Markdown/Text/Unsupported。**不改这两个函数。**
- **`media://` 协议有 roots 校验**：`lib.rs` 注册（408–475），`is_within_roots`（278–281）要求路径在 workspace/media_roots 内才服务。**任意拖入的文件必须先进工作区（复制）才能经 `media://` 渲染** —— 这是「复制进工作区」决策的技术原因。
- **前端无打包器、UMD-only**：`/vendor/*.min.js` 经典脚本（markdown-it/purify/highlight/jszip/docx-preview/papaparse）。`main.js` 裸 module，`withGlobalTauri:true`，`window.__TAURI__.core.invoke()` 可用。无 JS 测试框架，前端用 `node --check`。
- **渲染/编辑气泡已存在**：`renderMediaCard`（main.js:642–681，契约 `{display,path,kind,caption}`）、`renderDocCard`（824–881）、`renderEditCard`（883–932，契约 `{edit,path,kind,content,caption}`，保存走 `write_file`）。目前这些只由 **tool-result 事件**触发（main.js:983–1002 路由 `display`/`edit_file`）。本设计新增一条**用户直接触发**的渲染入口，复用同一组渲染函数。
- **composer 结构**（index.html:55–68）：`#chat-form` 内 `[mic-btn][textarea][send-btn]`。附件按钮自然落在 mic-btn 旁或 send 前；附件预览条落在 textarea 上方。
- **现有文件对话框模式**：main.js:1064–1089 用 `invoke("plugin:dialog|open", {options})` 选目录/图片。附件选择复用同一模式。

## 已锁定的决策（brainstorming 结论）

1. **附件语义（非图片视频的文件怎么发）= 混合 + 文本内联**
   - 图片/视频 → 多模态 `image_url`/`video_url` 内容块（base64），模型真看到。
   - 文本类（txt/md/csv/json/xml/代码 等 `DocKind::Text`/`Markdown`）→ stage 时读出文字，内联成 `text` part（≤50KB；超过的不内联，改走 agent 路径）。
   - 二进制（pdf/docx/audio/doc/其他 unsupported）→ stage 后，用户消息里追加一句 `"[用户上传了 <path>，可用 read/display 处理]"`，agent 自行决定调用哪个工具。

2. **右区（消息区拖放）= 纯本地渲染/编辑 + 静默上下文，不触发模型**
   - 拖入立刻在气泡里本地打开（复用 renderMediaCard/renderDocCard/renderEditCard），无模型往返。
   - 同时往 `agent.rs` 消息历史追加一条 user-role 上下文消息（如 `"[上下文] 用户将 <path> 拖入渲染区查看/编辑"`），**不调 `chat()`**。下次用户真正发消息时这条上下文已在历史里。

3. **文件落地 = 复制进工作区**
   - `stage_attachments` 把原文件复制到 `app_data/workspace/attachments/<unix_ms>_<原名>`（时间戳防冲突）。
   - 之后多模态读字节 / agent 拿路径 / 本地 `media://` 渲染，全基于这份副本。自包含、原文件移动/删除不影响、天然落在 roots 内可渲染。

4. **拖放分区 = 按拖放目标**（非左右半屏）
   - 丢到 `#chat-form`（输入框区）→ 加附件随消息发。
   - 丢到 `#messages`（消息区）→ 本地渲染/编辑。
   - 附件按钮提供非拖放入口（等价输入框区）。

5. **尺寸策略 = 超限拒绝**
   - 图片 ≤ 10MB（MiniMax 硬限制）。
   - 其余所有类型 ≤ `max_attachment_mb`（默认 30，进 config 可调）。视频选 30 是因 base64 膨胀 ~33% 后 ~41MB，给 64MB 请求体 + 文本/历史留余量。
   - 超限 → 拒绝，前端提示「文件过大 X MB，上限 Y MB」，不进 stage。
   - （文本内联 50KB 上限不拒绝，只决定内联 vs agent 路径。）

## 架构

**Rust 主导，前端只负责 stage 调用 + 预览 + 拖放路由。** API key/密钥、请求构造、文件访问、base64 编码、消息历史类型都在 Rust 侧；前端不碰密钥、不自己 base64 大媒体。

```
用户拖放/选文件
   │
   ├─ 输入框区 / 附件按钮 ──→ stage_attachments(原路径[]) ──→ 返回 StagedFile[]
   │                          （Rust：复制进 attachments/ + 判 kind + 文本提取）
   │                             │
   │                             └─ 前端：附件预览条（chip/缩略图，可删）
   │                                    用户写文字 + 发送
   │                                     │
   │                                     └─ chat(text, attachment_refs[]) ──→ Rust 构造多模态 user 消息
   │                                          │
   │                                          ├─ 图片 → image_url(base64) part
   │                                          ├─ 视频 → video_url(base64) part
   │                                          ├─ 文本类 → text part（内联）
   │                                          └─ 二进制 → text part「[用户上传 X，可用 read/display]」
   │
   └─ 消息区 ──→ stage_attachments(原路径) ──→ 前端直接 renderMediaCard/renderDocCard/renderEditCard
                                          （本地，无模型）+ agent.rs 追加静默上下文（不触发）
```

## 组件清单

### Rust 侧

- **`stage_attachments(paths: Vec<String>) -> Result<Vec<StagedFile>, String>`**（`lib.rs` 新命令）
  - 对每个路径：存在校验 → 尺寸校验（图片 10MB / 其余 `max_attachment_mb`，超限整批报错或逐个跳过——见「错误处理」）→ 复制到 `attachments/<unix_ms>_<名>` → `canonicalize` → 判 kind（`media_kind_from_ext`→`doc_kind_from_ext`）→ 文本类且 ≤50KB 读内容 → 返回 `{staged_path(绝对), kind, original_name, size, text?}`。
  - 复制大文件耗时 → 命令本身 `async`，前端转圈。
- **消息内容类型升级**（`agent.rs` + `llm.rs`）
  - user 消息 content：`String` → `serde_json::Value`（字符串或 parts 数组）。assistant/tool 消息不动（带 `tool_calls` 的保持原样）。
  - `build_body` 序列化时字符串原样、数组展开。
- **`chat` 命令签名扩展**：`chat(text, attachments: Option<Vec<AttachmentRef>>)` —— `AttachmentRef = {staged_path, kind}`。命令把这些 ref 组进 `SessionEvent::UserMessage`（user 消息 content 存 `text` + `*_ref` part，**不内联 base64**）；`build_body` 发送时才按 kind 展开（见工程审查修订 ARCH-1）。无附件时走原纯文本路径（零回归）。
- **静默上下文命令 `append_context_note(text: String)`**（`lib.rs` 命令 → 经 channel 发 `SessionEvent::ContextNote{文本}`；driver 处理时 push `{role:"user",content:text}` 并**不跑 turn**，仿 `Reset`）。详见工程审查修订 ARCH-2 —— 本架构下 `messages` 由 driver 私有持有（`agent.rs:96-110`），命令不能直接改历史，只能发事件。
- **config**：新增 `max_attachment_mb`（`#[serde(default="d_max_attachment_mb")]`，默认 30）。同步 config.rs 的 Default impl + 两个 roundtrip/missing 测试。
- **`media_roots` 已含 workspace**（既有），staged 副本天然可经 `media://` 渲染，无需改 roots。

### 前端侧（`index.html` + `main.js`）

- **附件按钮** `#attach-btn`（composer 内，mic-btn 旁）：点击 → `plugin:dialog|open`（多选，过滤器按支持类型）→ `stage_attachments` → 入附件栏。
- **附件预览条**：textarea 上方，每个附件一个 chip（图标+名+大小+✕删除）。仅暂存「待发」附件（发送后清空）。
- **两个 dropzone**：
  - `#chat-form`（含附件栏）监听 dragover/drop → `stage_attachments` → 入附件栏（等价附件按钮）。
  - `#messages` 监听 dragover/drop → `stage_attachments` → 直接渲染 + `append_context_note`（不发送）。
  - dragover 时给目标区高亮（边框/底色），防误判。
- **本地渲染入口**（新）：右区 drop 拿到 `StagedFile` 后，**在 `#messages` 末尾追加一个新的气泡**（用户侧/文件气泡，不含文字），按 kind 调对应渲染函数把卡渲染进这个新气泡（不复用 `activeAssistantWrap`，因为这是用户主动操作、没有 assistant 轮次上下文）。复用 `renderMediaCard`/`renderDocCard`/`renderEditCard`，契约不变（构造 `{display:true,path,kind,caption}` 或 `{edit:true,...}` 喂进去）。
- **附件随消息发送**：submit handler 把附件栏的 refs 连同 text 一起 `invoke("chat", {text, attachments})`；用户气泡里展示附件预览（缩略图/chip）。

## 数据流（类型 × 分区，完整表）

| 文件类型 | 输入框区 / 附件按钮（随消息发） | 消息区（本地） |
|---|---|---|
| 图片 | stage → 用户气泡预览 → 发送时 `image_url`(base64) part | 媒体卡（`media://` 预览 + lightbox） |
| 视频 | stage → 预览 → `video_url`(base64) part | 媒体卡（`media://` 播放） |
| 文本类(txt/md/csv/json/代码) ≤50KB | stage → 内联 `text` part | 文档卡/编辑卡（md 渲染 / txt 可编辑存盘） |
| 文本类 >50KB | stage → `[用户上传 X，可用 read]` agent 路径 | 同上（本地仍可渲染/编辑） |
| pdf/docx/csv | stage → `[用户上传 X，可用 read/display]` agent 路径 | 文档卡（pdf.js / docx-preview / papaparse） |
| 音频 | stage → `[用户上传 X]` agent 路径（模型听不了） | 音频卡（仅播放） |
| doc/xlsx/unsupported | stage → `[用户上传 X]` agent 路径 | 提示卡（沿用 display unsupported 提示） |

## 技术关键点：消息内容类型变更（最硬的一处）

当前 `agent.rs` 的消息历史是 `Vec<Value>`，每条 `{role, content:"string"}`。多模态要求 user 消息支持 `content: [parts]`。

**做法：** content 字段从「一定是字符串」放宽为「字符串或数组」。具体：
- 构造多模态 user 消息时，`content` 直接放 `[{type:"text",text:...}, {type:"image_url",image_url:{url:"data:image/png;base64,..."}}, ...]`。
- 其余所有消息（system / assistant 含 tool_calls / tool 结果）content 仍是字符串。
- `build_body` 把整个 `messages` 原样 `json!` 序列化即可（Value 已能承载两种形态）—— 关键是**构造处**别再把 content 强制成 String。
- **回归防护：** 无附件的 `chat(text)` 走原路径构造 `{role:"user", content:text_string}`，行为与今天逐字节一致；只有传了 `attachments` 才走数组路径。

base64 编码：`Cargo.toml:26` 已有 `base64 = "0.22"`。0.22 须 `use base64::Engine; base64::engine::general_purpose::STANDARD.encode(bytes)`（`base64::encode` 已移除）。读 staged 文件字节 → 编码 → 拼 `data:image/png;base64,...`（mime 由 `mime_from_ext` 定）。**历史里存路径引用、发送时再展开 + 预算守卫**，见工程审查修订 ARCH-1。

## 错误处理

- **文件不存在 / 无读权限**：`stage_attachments` 逐个跳过并返回失败项 `{ok:false, error}`，前端在该 chip 上标红 + 提示，不阻塞其余。
- **超尺寸**：同上逐个跳过，error 文案带「过大 X MB / 上限 Y MB」。**不整批失败**（多选时一个超大不该连累其它）。
- **不支持的类型**（如 .exe）：仍 stage（复制成功），渲染时按 unsupported 给提示卡；发给模型时走 agent 路径（模型 `read` 会自己报不支持）。不在 stage 层拒绝（只按尺寸拒）。
- **stage 复制失败**（磁盘满/路径非法）：该项 `{ok:false}`，前端标红。
- **拖放非文件**（如拖文本）：drop 事件检查 `dataTransfer.files`，空则忽略。
- **发送时附件已被用户删光**：退化为纯文本发送。

## 测试

### Rust 纯逻辑（可离线测，沿用 tempfile 模式）
- `stage_attachments`：复制成功 + staged 路径在 workspace 内 + `<ts>_<名>` 防冲突（同名两次得到不同路径）；尺寸校验（图片 >10MB 拒、其他 >max 拒、恰等于边界通过）；kind 判定覆盖图片/视频/音频/pdf/docx/csv/md/txt/unsupported；文本类 ≤50KB 读出 `text`、>50KB 不读。
- 多模态消息构造：四种 attachment_ref 各自产出正确的 content parts（image_url/video_url/text-inline/agent-note）；无 attachments 时 content 仍是纯字符串（回归）。
- `append_context_note`：历史多一条 user 消息、内容正确、session 计数/状态不触发请求。
- config：`max_attachment_mb` default 30 + roundtrip + missing-fields 用默认。

### 前端
- `node --check src/main.js`。
- 手测（dev server，右键 Reload 刷前端）：附件按钮选各类型 → 预览条 → 发送 → 用户气泡带预览 + 模型正确接收（图片能描述内容、视频能描述、文本内联能引用、二进制 agent 会 read）；两区拖放高亮与分流正确；右区 drop 立刻渲染 + 下次发消息 agent 知道；超尺寸拒绝提示；chip 删除。**追加：** 键盘可达性（📎 Tab/Enter、chip ✕ Enter、聚焦 chip Backspace 删除）；dragleave 计数器防抖（鼠标穿越子元素不闪烁）；窄窗口附件条换行；chatBusy 时右区 drop 的上下文 note 队列到轮结束。

## 前端交互细化（design-review 2026-07-25 补充）

> 由 gstack plan-design-review（7 维度 0-10 打分）产出，补齐原 spec 的前端 UX 细节。前端完备度 **4/10 → 目标 8+/10**。D1–D5 暂按推荐落定，可推翻。

### 信息架构（Pass 1：5 → 8）
- composer 层级：**文本输入 > 待发附件条 > 动作按钮**（mic/attach/send）。
- 待发附件条紧贴 textarea 上方（与它将随附发送的输入视觉成组）。
- 已发送用户气泡：附件作为**底部条带**渲染在文字下方（文字为主、文件为辅），不内联在文字中间。

### 交互状态表（Pass 2：4 → 8）
| 功能 | loading | empty | error | success | partial |
|---|---|---|---|---|---|
| 附件 stage | 每 chip 转圈 | 附件条空态不可见（D5） | 单项标红 + 文案（过大/无权限/复制失败） | chip 入栏 | 红绿混排；仍有 chip staging 时发送键禁用 |
| 拖放 | — | 空消息区显示拖放提示（见发现性） | 超尺寸 toast | drop 后短暂高亮确认 | — |
| 右区本地渲染 | stage 转圈 | 同上 | 不支持类型 → 提示卡 | 卡片入新气泡 | — |
| 多模态发送 | — | — | base64/网络失败 → 回退 agent 路径并提示 | 模型正常接收 | — |

### 拖放状态机（含 dragleave 抖动修复）
- 用**计数器**：dragenter +1 高亮、dragleave -1（归 0 取消高亮），避免鼠标穿越子元素时的高亮闪烁（经典 dragleave bug）。或在 dragleave/drop 检查 `e.relatedTarget` 是否仍属该区。
- `dragover` 必须 `e.preventDefault()` 才能触发 `drop`。
- 高亮样式对比度 ≥ 4.5:1。

### 发现性（Pass 3：5 → 8）
- **空消息区**（无对话时）显示淡提示：「拖放文件到此查看，或拖到输入框作为附件发送」；有消息后隐藏。
- 📎 按钮是拖放的**键盘等价路径**（无障碍必需）。
- 静默上下文（D3）不渲染可见气泡，避免对话流被系统化文字污染。

### 无障碍 a11y（Pass 6：2 → 8）
- 📎 按钮：Tab 可达，Enter 打开选择器。
- chip：`aria-label="截图.png，图片，按删除键移除"`；✕ 可聚焦，Enter/Space 删除；聚焦 chip 上 Backspace/Delete 删除。
- dropzone：`role`/`aria-label`（输入框区 =「附件」、消息区 =「本地查看」）。
- 高亮对比度 ≥ 4.5:1。
- 静默上下文 history-only（D3），无可见气泡故无额外 a11y 公告。

### 响应式（Pass 6）
- 窗口变窄：附件条**换行**（最多 N 行后内滚），textarea 最后压缩，动作按钮固定宽度不缩。
- 全屏：附件预览 / 渲染卡片复用既有 lightbox 全屏放大，不另做。

### 设计系统对齐（Pass 5：4 → 7，项目无 DESIGN.md）
- 📎 按钮：复用 composer 按钮样式（mic-btn 同款），不新增按钮风格。
- chip：复用面板/气泡背景 + 边框 token；类型图标用卡片已有的图标集。
- dropzone 高亮：复用 accent 色。local-render 气泡见 D1。
- **TODO（不阻塞本任务）**：项目尚无 DESIGN.md，建议后续 `/design-consultation` 沉淀玻璃面板/气泡/accent 的隐式 token。

### 决策落定（D1–D5，按推荐，可推翻）
- **D1 本地渲染气泡 vs agent 气泡**：用户发起的渲染气泡带小「本地」标签（或对齐/透明度微调），与 agent 工具结果气泡区分。避免「我打开的」和「agent 给我看的」混淆。
- **D2 编辑落点**：非破坏性 —— 编辑只改 staged 副本；编辑卡清楚显示保存路径，并提供「另存到原位置」动作（覆盖原文件需二次确认）。原文件默认不动。
- **D3 静默上下文**：history-only，**不渲染可见气泡**（渲染卡是可见产物，note 是静默上下文）。
- **D4 忙时 drop（chatBusy）**：允许 stage + 本地渲染；上下文 note **队列**到当前轮结束后再追加，不污染在途 messages 数组。
- **D5 附件条空态**：首个附件前不可见（无常驻提示）；发现性靠 📎 + 空消息区提示。

## 实现任务骨架（供 writing-plans 参考）

- **T1 config + 类型基础设施**：`max_attachment_mb` 入 config（含测试）；定义 `StagedFile`/`AttachmentRef` 结构。
- **T2 `stage_attachments` 命令**：复制 + 校验 + kind + 文本提取 + 注册到 invoke_handler；纯逻辑函数可单测。
- **T3 消息内容类型升级 + 多模态构造**：`agent.rs` 放宽 content、`chat` 接收 attachments、`llm.rs` 按 kind 造 part；回归测试（无附件 = 原行为）。
- **T4 `append_context_note`**：静默上下文追加，不触发 chat。
- **T5 前端附件按钮 + 预览条 + 发送**：composer UI + chat 调用带 attachments + 用户气泡预览。
- **T6 前端双 dropzone + 本地渲染入口 + a11y**：`#chat-form`/`#messages` 拖放分流（dragleave 计数器防抖 + dragover preventDefault + 高亮 ≥4.5:1）；右区直接渲染复用现有渲染函数 + 调 `append_context_note`（chatBusy 时队列，D4）；local-render 气泡带「本地」标签（D1）；chip/dropzone/📎 的 ARIA + 键盘操作（Pass 6）；附件条换行规则（响应式）。

每个任务自包含、可独立测，后端纯逻辑优先 TDD，前端 node --check + 手测。

## 工程审查修订（plan-eng-review 2026-07-25）

> 基于代码核实：session 是事件驱动零锁（`agent.rs:96-110`，`messages` 由 driver 私有持有，命令只有 `mpsc::Sender`）；`build_body` 每轮重发全量 `messages`（`agent.rs:56→66`）；`base64 = "0.22"` 已在 `Cargo.toml:26`。6 项发现按推荐落定，1 处设计被修正（ARCH-2）。D-ENG1 = 方案 A，D-ENG2 = 接受。

### ARCH-1（P1）：多轮媒体重发 + 请求体预算守卫（D-ENG1 = A）
- **历史存路径引用，发送时再展开 base64**：user 多模态消息在 `messages` 里存 `[{type:"text",...},{type:"image_ref",path},{type:"video_ref",path}]`（轻量引用，不把 41MB 字符串常驻内存）；`build_body` 序列化时把 `*_ref` 现场读 staged 文件 → base64 → 换成 `image_url`/`video_url` part。每轮仍上传字节（无状态多模态不可避免），但内存有界。
- **每请求媒体预算守卫**：`build_body` 展开后若总请求体估算 > ~58MB → 不发送，emit error「历史媒体过大，请开新会话或移除旧图」，避免 MiniMax 400。
- 取代原文「技术关键点」里 base64 一次性内联的隐含假设。

### ARCH-2（P1，修正）：静默上下文必须是 SessionEvent，不是直接改历史
- 原文「`append_context_note` 命令直接改 agent.rs 历史」在本架构不可能（`messages` 由 driver 私有持有）。
- **改为**：新增 `SessionEvent::ContextNote { text }` 变体；driver 处理时 push `{role:"user",content:text}` 并 `return None`（不跑 turn，仿 `Reset` `agent.rs:59-63`）。Tauri 命令 `append_context_note(text)` 仅 `tx.send(ContextNote)`。
- **D4 简化**：chatBusy 时排队由 channel 自动保证（bounded cap 64，`agent.rs:74`），无需额外队列逻辑。

### ARCH-3（P2）：attachments/ 去重 + 清理
- `stage_attachments` 复制前算内容 hash（sha256 或前 64KB 采样），同 hash 已存在则复用不重抄。
- 旧副本 GC 列为 TODO（不阻塞本任务；首期靠去重控增长）。

### ARCH-4（P2）：窗口化要感知媒体
- `window_messages`（`agent.rs:28-46`）当前只折叠旧 tool 结果。新增：超窗时把旧 `image_ref`/`video_ref` user 消息折叠为短文本 `[旧图：name]`（非 tool 消息，不影响 tool_call 配对），配合 ARCH-1 预算守卫双重控成本。

### CQ-1（P2，DRY）：抽 `file_kind(path) -> FileKind`
- `display`（tools.rs）与 `stage_attachments` 都做 media_kind→doc_kind 两步判定。抽 `pub fn file_kind()` 共用，新增扩展名只改一处。

### CQ-3（P2）：本地渲染气泡复用 `addBubble` 骨架
- 右区 drop 的无文字卡片气泡复用现有 `addBubble(role, text)`（main.js）骨架（空 text + 卡片子节点），不另起 DOM；先确认 helper 容忍空 text + 卡片子节点。

### 测试补缺（接入现有 FakeRound/FakeEmitter 基建，`agent.rs:139-178`）
- `SessionEvent::ContextNote` 事件：push 消息但不跑 turn（仿 `reset_clears_and_reinjects_system`，`agent.rs:204`）。
- `build_body` 多轮回归：历史带 `image_ref`，第 N 轮正确展开为 `image_url` part；无附件仍纯字符串（回归，护住现有 tools.len 断言）。
- 预算守卫：构造接近 58MB 的请求体 → 断言拒收 + error。
- stage 边界：图片恰 10MB / 其他恰 max；同名两次得不同路径；文本恰 50KB 内联 vs 超出不读。
- 文件夹 drop：目录路径 → 拒收 + 提示（F3）。

### 失败模式处理
- **F1（critical → 已闭）**：64MB 中途撞顶 → 由 ARCH-1 预算守卫拦下 + clear error。
- **F2（medium）**：staged 文件被删后再发/渲染 → `build_body` 读盘失败 / `media://` 404 → graceful「附件文件已丢失，请重新添加」。
- **F3（low）**：drop 文件夹 → stage 层拒收 + 提示「暂不支持文件夹」。

## GSTACK REVIEW REPORT

| Review | Trigger | Why | Runs | Status | Findings |
|--------|---------|-----|------|--------|----------|
| Design Review | `/plan-design-review` | UI/UX gaps | 1 | issues_open | 前端 4/10→8/10（暂定），D1–D5 按推荐落定待确认 |
| Eng Review | `/plan-eng-review` | 架构/测试/性能（必需门） | 1 | clean | 6 issues 全部折叠（媒体预算守卫 / ContextNote 事件修正 / 去重 / 媒体感知窗口 / file_kind DRY / base64 0.22 API）；1 critical gap（F1）已由预算守卫关闭 |
| CEO Review | `/plan-ceo-review` | 范围/策略 | 0 | — | 不需要（功能聚焦，无产品方向分歧） |

- **VERDICT:** Design + Eng 均已跑。Eng 自身发现全部折叠、0 未决，可进入 writing-plans。余 D1–D5（design review，按推荐已落定）待确认/推翻。
- **OUTSIDE VOICE:** 跳过（发现均已代码核实、高置信）。如需独立第二意见可补跑 codex/subagent。

**UNRESOLVED DECISIONS:**
- D1 local-render 气泡区分（已按推荐「本地」标签落定，待确认/可推翻）
- D2 编辑落点 staged 副本 vs 原文件（已按推荐「非破坏性 + 另存」落定，待确认/可推翻）
- D3 静默上下文是否渲染可见气泡（已按推荐「history-only」落定，待确认/可推翻）
- D4 忙时 drop 行为（按推荐落定；eng review 进一步确认排队由 channel 自动保证，无需额外逻辑，待确认/可推翻）
- D5 附件条空态（已按推荐「首个附件前不可见」落定，待确认/可推翻）
