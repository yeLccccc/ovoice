# 聊天多模态渲染（图/视频/音频）设计

> 日期：2026-07-24 · 决策已定：触发用 `display_media` 工具；长任务完成不自动弹、等 agent 主动展示。

## 目标

在聊天对话里渲染 agent 产出的本地媒体文件：图片显示、视频播放（可拖进度）、音频播放。复用现有 tool-call 事件管线 + 卡片 UI，最小侵入。

## 非目标（v1 不做）

- 自动扫描回复文本里的路径插播放器（易误判，已否决）。
- job 完成自动弹媒体（等 agent 调 display_media，已决策）。
- markdown `![](path)` 内联图（可二期，URL 重写路径→media://）。
- 转码/缩略图/字幕；格式必须是 WebView2(Chromium) 可解码（mmx 的 mp4/mp3 没问题）。
- 媒体持久化画廊（v1 随气泡存在，刷新即失，与 Session 内存态一致）。

## 现状管线（插入点）

- 助手气泡三段：`.bubble-reasoning` / `.bubble-tools`（工具卡）/ `.bubble-text`（正文）。
- 正文：流式纯文本 → `chat-turn-end` 一次性 `renderMarkdown`（markdown-it `html:false` + DOMPurify 禁 iframe/object/embed）。
- 工具：`llm-tool-call`→建 tool-card；`llm-tool-result`→填结果文本。
- 媒体由 agent 跑 mmx 产出，落 workspace（`--download cat.mp4` 或 `minimax-output/`），路径出现在工具结果/回复里。

→ 媒体卡走 tool-call 管线：`display_media` 这个工具名前端特判，画媒体卡而非文本 tool-card。媒体 DOM 用 `createElement` 直接搭（绕开 markdown/DOMPurify，src 由后端校验后给，安全）。

## 架构与数据流

```
agent 调 display_media({path, kind?, caption?})
   │  (tools.rs dispatch)
   ▼
后端：resolve_path(workspace) → 校验存在 → 推断 kind → canonicalize
   │  返 tool-result（JSON）：{"display":true,"path":"<abs>","kind":"video","caption?":"..."}
   │  llm-tool-result 事件推前端 {name:"display_media", result:"{...}"}
   ▼
前端 fillToolResult 特判 name==="display_media"
   │  解析 result JSON → 用 convertFileSrc(abs,"media") 得 URL
   │  → 按 kind 建 <img>/<video controls>/<audio controls>，塞进 .bubble-tools
   ▼
浏览器请求 media:// URL
   ▼
后端 media:// 协议 handler（lib.rs register_uri_scheme_protocol）
   解码路径 → canonicalize → 校验在 workspace/app_data 下 → 按 ext 给 mime
   → 读 Range 头 → 返 200 全量 或 206 分段（视频可拖进度）
```

模型侧看到的 tool-result 是简洁确认 + 结构化 JSON（前端解析用）。

## 组件

### 1. 后端 `media://` 协议（lib.rs）

- 注册：`tauri::Builder::register_uri_scheme_protocol("media", handler)`。
- 前端 URL：`convertFileSrc(absPath, "media")` → Windows 为 `https://media.localhost/<encoded>`；CSP=null 故放行。
- handler 职责：
  1. 从 `request.uri()` 取 path 部分，percent-decode。
  2. `canonicalize`；**校验落点在 `workspace_dir` 或 `app_data_dir` 之下**（防 `../` 越权），否则 403。
  3. `mime_from_ext`（复用 bg 那套，扩 video/audio）。
  4. 读 `Range` 头：有 → 206 + `Content-Range`/`Accept-Ranges: bytes` + 分段体；无 → 200 全量 + `Accept-Ranges: bytes`。
- 抽出纯函数便测：`is_within_roots(path, &[workspace, app_data])`、`parse_range(header, total) -> Option<(start,end)>`。

### 2. 后端 `display_media` 工具（tools.rs）

- `tool_display_media(args, workspace)`：
  - `path` 必填 → `resolve_path`（相对 workspace）。
  - 不存在 → `"文件不存在: {path}"`。
  - `kind` 缺省则按扩展名推断；不在 image/video/audio 表 → `"不支持的媒体类型: {ext}"`。
  - 返 `serde_json::json!({"display":true,"path":abs.to_string(),"kind":kind})` 的字符串。
- `schemas()` 加 display_media：description 明确"展示本地媒体文件（图/视频/音频），路径相对工作目录；长任务产物完成后调用"。
- `dispatch` 加 `"display_media" => tool_display_media(...)`。

kind 推断表（扩展名小写）：
- image: png jpg jpeg webp gif bmp svg ico
- video: mp4 webm mov mkv m4v（avi 可能无解码器，标注）
- audio: mp3 wav flac ogg m4a aac opus

### 3. 前端媒体卡（main.js + styles.css）

- `fillToolResult` 特判 `name==="display_media"`：
  - 解析 result JSON；失败/`display!=true` → 回退普通文本卡。
  - `const url = convertFileSrc(j.path, "media")`（`window.__TAURI__.core.convertFileSrc`）。
  - 按 `j.kind` 建：img(`src=url`) / video(`src=url controls`) / audio(`src=url controls`)。
  - 容器：复用 `.bubble-tools`（与工具卡同序），但样式独立（非深色）。caption 有则附 `<div class="media-caption">`。
- on `llm-tool-call` display_media：先建占位卡（显示 path + "准备中"），result 到再换成播放器（与现有 call→result 两段一致）。
- CSS：img `max-width:100%` 圆角；video `max-width:100%` 圆角 + 控制条；audio `width:100%`；统一 `media-card` 包裹 + 边距。

## 安全

- 协议 handler 只放行 workspace/app_data 下文件；canonicalize 消解 `../`。
- `src` 由后端解析的绝对路径经 `convertFileSrc` 生成；agent 仅传 path 参数，无法注入任意 URL/scheme。
- 播放器元素由前端受控创建（非 markdown 内 HTML），不经 DOMPurify 但也无注入面（无 innerHTML 拼接用户串；caption 走 textContent）。

## 边界与降级

- 文件缺失/移动：tool-result 返错误串 → 前端画普通文本卡显错，不崩。
- 不支持的扩展名：同上。
- 超大视频：v1 整文件读入内存返 206 切片（mmx 短视频可接受）；后续可 mmap/流式（TODO）。
- 编解码不支持（avi/某些 mkv）：`<video>` 黑屏/不播 → 卡片下方加"若无法播放，点打开"按钮（`plugin:opener|open_path`）兜底。
- 回合进行中（流式）：媒体卡随 tool-call/result 事件实时插入，不依赖 turn-end 的 markdown 重渲。

## 测试

- 后端单测（纯函数）：`is_within_roots`（内/外/`../` 越权）、`parse_range`（正常/开放区间/畸形）、`tool_display_media`（缺失/不支持/正常返 JSON 结构）、kind 推断表。
- 协议 handler：难直接单测（闭包+Request），靠纯函数覆盖 + 手测。
- 前端：手测（无打包器、无 DOM 测试框架）——见 smoke 清单：图片显示/视频拖进度/音频播放/缺失文件降级/open 兜底。
- 全量 `cargo test --lib` + `cargo build` 零 warning。

## 涉及文件

- `src-tauri/src/tools.rs`：`tool_display_media` + `schemas` + `dispatch` + kind 表 + `is_within_roots`/`parse_range`（或放 lib.rs）。
- `src-tauri/src/lib.rs`：`register_uri_scheme_protocol("media", …)` + handler + mime 扩展；invoke_handler 无需改（工具走 dispatch）。
- `src/main.js`：`fillToolResult`/`appendToolCard` 特判 display_media + `renderMediaCard`。
- `src/styles.css`：`.media-card` / img / video / audio 样式。

## 前端与交互设计（plan-design-review 补充，2026-07-24）

> review 焦点：前端 + 交互。已定决策：① 媒体卡位于工具区（正文上方）；② 图片点击→应用内 lightbox；③ 媒体卡玻璃质感（`--glass-alpha`）。

### 媒体卡位置与排序
- 媒体卡挂 `.bubble-tools`（与 tool-card 同容器），按 tool-call 顺序追加；正文 `.bubble-text` 在其下方。
- on `llm-tool-call` name=display_media：建占位卡（玻璃底 + 文件名 + "准备中"骨架）。on `llm-tool-result`：解析 result JSON，换成对应播放器。与现有 call→result 两段一致。
- 多个媒体：纵向堆叠，gap 与 tool-card 一致（6px），按调用顺序。

### 各类型渲染与尺寸
- 图片：`<img>` `max-width:100%` `max-height:360px` `object-fit:contain`，圆角 `--radius-bubble`。**点击→应用内 lightbox**。`alt` = caption || 文件名。
- 视频：`<video controls preload="metadata">`，`max-width:100%` `max-height:360px`，圆角。**不自动播放**（用户点播放），无 poster（不生成缩略图）。
- 音频：`<audio controls preload="metadata">` `width:100%`，上方一行 caption/文件名。
- 统一包 `.media-card`：玻璃底（rgba via `--glass-alpha`，与助手气泡同源、随透明度滑块变）+ 8px 内距 + 圆角。

### 应用内 lightbox（图片放大）
- 点 `.media-card img` → 全屏遮罩 `position:fixed; inset:0; z-index:9999; background:rgba(0,0,0,.85)`，图片居中 `max-width:92vw; max-height:92vh; object-fit:contain`。
- 关闭：点遮罩 / 按 Esc / 点图片以外。Esc 关、打开焦点移遮罩、关闭归还触发元素，`aria-modal`。
- 轻量自实现（一个 `.media-lightbox` DOM + 事件），不引依赖。

### 交互状态表（用户看到什么，非后端行为）
| 场景 | 触发 | 用户看到 |
|---|---|---|
| 加载中 | tool-call 到、result 未回 | 玻璃卡 + 文件名 + "准备中…"骨架 |
| 图片字节加载 | `<img>` 未 onload | 占位透出玻璃底 |
| 视频缓冲 | `<video>` buffering | 原生控制条转圈 |
| 成功 | 正常 | 播放器/图片（图片可点放大） |
| 文件缺失 | tool 返"文件不存在" | 文本卡："文件不存在：`<path>`" |
| 不支持扩展名 | tool 返"不支持的媒体类型" | 文本卡 + 扩展名 |
| 损坏/0字节 | 加载失败 | broken 图标 / video 黑屏 + 下方"打开"兜底 |
| 编解码不支持 | video 播不了 | 黑屏 + "若无法播放，点打开"链接 |
- 每卡常驻右下角"在系统查看器打开"小链接（`plugin:opener`），作统一逃生口。

### 无障碍
- `<img alt>` = caption 或文件名（非空）。
- `<video>`/`<audio>` 原生 controls 自带键盘可达。
- lightbox：Esc 关、焦点进出、`aria-modal`。
- "打开"链接触控目标 ≥34px（沿用 icon-btn 规格）。

### 复用既有资产
`--glass-alpha`（透明度滑块）、`--radius-bubble`、炭黑文字变量、`plugin:opener`（系统打开）、现有 call/result 事件管线、`scrollBottom()`。

## Implementation Tasks
由本 review 的前端/交互发现综合而来，供 writing-plans 拆解。

- [ ] **T1 (P1, CC ~15min)** — 前端媒体卡渲染 — `fillToolResult`/`appendToolCard` 特判 display_media，建 `.media-card`（玻璃底）+ img/video/audio。
  - Surfaced by: Pass 1/5 — 位置与卡视觉未定（已定工具区+玻璃）。
  - Files: src/main.js, src/styles.css
- [ ] **T2 (P1, CC ~15min)** — 应用内 lightbox — 点图全屏遮罩看大图（Esc/点遮罩关、焦点进出、aria-modal）。
  - Surfaced by: 决策② — 图片放大选了应用内 lightbox。
  - Files: src/main.js, src/styles.css
- [ ] **T3 (P1, CC ~10min)** — 交互状态覆盖 — 加载骨架 / 缺失 / 不支持 / 损坏 / 编解码失败的降级卡 + "在系统查看器打开"逃生链接。
  - Surfaced by: Pass 2 — 缺状态表。
  - Files: src/main.js, src/styles.css
- [ ] **T2-backend (P1)** — `media://` 协议 + Range/206 + scope 校验（见上节，本 review 未改后端设计）。
- [ ] **T3-backend (P1)** — `display_media` 工具 + schemas + dispatch + kind 推断表。
- [ ] **T4 (P2, CC ~5min)** — 无障碍收尾 — img alt、lightbox 焦点、链接触控尺寸。

## GSTACK REVIEW REPORT

| Review | Trigger | Why | Runs | Status | Findings |
|--------|---------|-----|------|--------|----------|
| Design Review | `/plan-design-review` | UI/UX gaps | 1 | CLEAR | score: 4/10 → 9/10, 3 decisions（位置/图片放大/卡视觉） |

- **VERDICT:** DESIGN REVIEW CLEARED — 前端/交互缺口已补（状态表 + lightbox + 玻璃卡 + a11y）。后端（media:// 协议 + display_media）未在本次 review 改动；落地前建议跑 `/plan-eng-review` 验证协议 Range/scope 与工具契约。

NO UNRESOLVED DECISIONS
