# 聊天文档渲染与编辑 — 设计规格

> 日期：2026-07-24 · 状态：已通过 brainstorming，待 writing-plans
> 前序：`feat/chat-media`（已并入 master @ 2a8fdf9）建立了 `display_media` 工具 + `media://` 自定义协议 + 玻璃媒体卡 + lightbox。
> 本规格在此基础上把对话的文件能力从「媒体」扩展到「文档」：查看 HTML/PDF/Word/CSV，编辑 markdown/text。

## 目标

让 agent 能在对话气泡里**展示和编辑**更丰富的本地文件类型，体验延续现有媒体卡（玻璃卡、工具区上方、可全屏放大、可系统打开）。具体：

1. 查看 HTML 文件 / HTML 片段（脚本可执行，沙箱隔离）
2. 查看 PDF（WebView2 内置阅读器）
3. 查看 Word `.docx`（docx-preview，视觉保真）
4. 查看 CSV（PapaParse + 表格）
5. 编辑 markdown / text（气泡内可编辑卡片：textarea + 实时预览 + 存盘）

## 非目标（明确排除 / 留 v2）

- `.doc` 旧二进制格式（无成熟客户端库；命中时返回提示卡 + "在系统查看器打开"）
- CSV 虚拟滚动 / 大表分页引擎（首 1000 行 + "显示更多"；>10k 行虚拟化留 v2）
- 跨域沙箱 iframe 自适应高度（用固定高度 + 内滚 + 全屏放大绕开）
- 编辑器语法高亮 / CodeMirror / Monaco（项目无打包器、deps 全是 UMD；textarea 够用）
- Word 追踪修订 / 批注 / SmartArt 保真
- 新增配置项（HTML 脚本统一 `allow-scripts`，不做设置开关）

## 现状约束（设计前提）

- **无打包器、无 npm。** 前端 deps 全是 `/vendor/*.min.js` UMD 经典脚本（markdown-it、purify、highlight、github-dark.css）。`main.js` 是裸 module、用全局变量（`window.markdownit`/`DOMPurify`/`hljs`）。**新库必须能以单文件 UMD 进 `/vendor/`。** 因此放弃 CodeMirror（纯 ESM，无 UMD）；docx-preview / JSZip / PapaParse 均有 UMD，可用。
- **`withGlobalTauri:true` + CSP=null。** `window.__TAURI__.core.invoke()` 暴露在主页面。任何能跑脚本/加载外链的渲染面（HTML、docx 衍生 HTML）必须走沙箱 iframe；DOMPurify 兜底 md/csv 衍生 HTML。
- **现有媒体管线（复用）：**
  - `media://` 自定义协议（lib.rs）— 路径 percent-decode → `is_within_roots`（workspace+app_data）→ mime → Range/206。前端 `convertFileSrc(path,"media")` = `https://media.localhost/<encoded>`。
  - `tool_display_media(args, workspace)`（tools.rs）— 返回 `{"display":bool,"path":abs,"kind":"image|video|audio","caption"}`。
  - `renderMediaCard(wrap, j)`（main.js）— `convertFileSrc` 取 url，建 `.media-card`，append 到 `.bubble-media`；图片点开走 `openLightbox(src)`。
  - 每个气泡结构含 `.bubble-media` 容器（在工具卡、正文之间）。
  - `resolve_path` / `is_within_roots` / `media_kind_from_ext` / `mime_from_ext`（lib.rs）可复用。

## 架构（走法 A：加法式）

新增**两个 agent 工具** + **一条前端命令**，不动已工作且有测试的 media 代码：

```
agent ──display_doc({path,caption?})──▶ tool_display_doc ──▶ {display,path,kind,caption}
agent ──edit_file({path,content?})────▶ tool_edit_file   ──▶ {edit,path,kind,content,caption?}
前端 ──invoke write_file({path,content})─▶ 写盘（scoped）
```

文档卡与媒体卡共用 `.bubble-media` 容器、玻璃卡样式、全屏覆层、"系统打开"链接。前端按 `kind` 分发渲染。

### 工具面（agent 侧，`tools.rs`）

| 工具 | 参数 | 返回 JSON（字符串） | 说明 |
|---|---|---|---|
| `display_doc`（新） | `path` 或 `html`（二者其一）、`caption?` | path 路径：`{"display":true,"path":abs,"kind":<html\|pdf\|docx\|csv\|markdown>,"caption"}`；html 片段：`{"display":true,"html":<str>,"kind":"html","caption"}` | `path` 缺/不存在/不支持（如 `.doc`）→ `{"display":false,"error":...}`，契约同 `display_media`。`html` 用于 agent 直接产出 HTML 片段（图表/小工具）不落盘的场景 |
| `edit_file`（新） | `path`（必）、`content?` | `{"edit":true,"path":abs,"kind":<markdown\|text>,"content":<str>,"caption"?}` | `content` 缺省时从磁盘读（UTF-8）；给了则预填。非 md/txt → `{"edit":false,"error":...}` |

后端分类：在 lib.rs 扩展扩展名→kind 映射（新增 html/htm→html、pdf→pdf、docx→docx、csv→csv、md→markdown、txt→text）。`.doc`（旧二进制）归类为不支持，`display_doc` 返回 `{"display":false,"error":"不支持 .doc 旧格式，请在系统查看器打开"}`。

`schemas()` 由 4 → 6 工具；`dispatch` 加 `"display_doc"` / `"edit_file"` 两条路由。

### 新命令 `write_file`（前端→磁盘，`lib.rs`）

```rust
#[tauri::command]
fn write_file(path: String, content: String, app: AppHandle) -> Result<String, String>
```

`resolve_path`（相对 workspace）→ `is_within_roots`（workspace+app_data，越界返 `Err("路径不在允许范围内")`）→ UTF-8 写盘（自动建父目录，复用 `tool_write` 的 create_dir_all 逻辑）→ `Ok("已写入 <path>（N 字节）")`。**用户编辑存盘的唯一路径**；scope 模型与 `display_media` 一致。注册进 `invoke_handler`。

## 各类型渲染（前端 `main.js`）

新增 `renderDocCard(wrap, j)` 与 `renderEditCard(wrap, j)`，均 append 到 `wrap.querySelector(".bubble-media")`。`renderDocCard` 按 `j.kind` 分发：

### html（文件 或 内联片段）
- 文件（`path`）：`<iframe sandbox="allow-scripts" referrerpolicy="no-referrer" src="<media://url>">`
- 内联片段（`html`）：`<iframe sandbox="allow-scripts" referrerpolicy="no-referrer" srcdoc="<html 字符串>">`
- 安全：两种都 `sandbox="allow-scripts"`（**绝不加** `allow-same-origin`）→ opaque origin 切断对 `window.parent.__TAURI__` 的访问。脚本可跑，但摸不到 Tauri bridge / 主页 DOM。`srcdoc` 继承父 base URL（仅 `document.baseURI` 可读，轻微信息泄漏，可接受）。
- 尺寸：内容区高 `min(480px, 62vh)`、内滚（避免压满气泡）；右下"全屏"按钮放大查看。

### pdf
- `<iframe sandbox="allow-scripts" src="<media://url>.pdf">` → WebView2 内置 Chromium 阅读器（自带缩放/搜索/选词/打印/分页）。
- 尺寸：内容区高 `min(520px, 70vh)`、内滚；全屏按钮放大（大 PDF 主要靠全屏）。

### docx
- `fetch(convertFileSrc(path,"media")).then(r=>r.arrayBuffer())` → `docx-preview.renderAsync(arrayBuffer, container, null, {className:'docx-body'})` 注入卡片内一个 `.docx-container` div。
- 安全：`docx-preview` 输出 HTML + 绝对定位 CSS，直接渲染进容器——**不走 DOMPurify**（会破坏分页/绝对定位样式）。安全性基于：`.docx`（OOXML）经 docx-preview 解析为 HTML/CSS，**无脚本执行路径**（VBA 仅存于 `.docm`，本工具不处理）；路径经 scope 校验。不在卡片内执行任何来自 docx 的 `<script>`。
- `.doc` → `display_doc` 已返 display:false；前端渲染提示卡 + "系统打开"链接。

### csv
- `fetch` 文本 → `PapaParse.parse(text, {skipEmptyLines:true})` → `createElement` 建 `<table>`（表头取首行）。
- **首 1000 行渲染**，超出显示 `"显示更多（共 N 行）"` 按钮，点击再追加 1000。
- **空文件**（0 行）：渲染空状态卡「CSV 为空」。
- **样式（单色玻璃表）**：表头半透白浅底、奇行 zebra（半透黑 ~2%）、`<th scope="col">`、单元格 `padding`、容器 `overflow-x:auto` 横向滚动；文字用 `--text-secondary` 保证对比。不引新颜色。
- 安全：纯 DOM 构造（createElement/textContent），无注入面。

### markdown（查看）
- markdown-it + DOMPurify 渲染（同现有 bubble-text 管线）成卡片内 `.md-body`。

### 编辑卡（markdown / text）
- 结构：`.edit-card` 内左 `<textarea>`（可编辑）+ 右 `.edit-preview`（实时预览）+ 底部"保存"按钮。
- `content` 预填；textarea `input` 事件防抖（~200ms）刷新预览。
- 预览按 kind：`markdown` → markdown-it + DOMPurify 渲染；`text` → 右栏以等宽字体原样显示纯文本（`<pre>`，不隐藏）。
- "保存" → `await invoke("write_file",{path, content: textarea.value})` → 成功 toast「已保存 <path>」，失败 toast 错误。
- **未保存保护（轻量）**：编辑卡跟踪 dirty 位（内容自上次保存/载入后变过）。在 chat-reset、关闭编辑卡、或卸载该卡时若 dirty，弹原生 `confirm("有未保存的改动，放弃？")`；确认才放行。保存成功后清 dirty。

### 公共文档卡能力
- **玻璃卡样式**：直接复用 `.media-card`（同 `--glass-alpha` 玻璃底、圆角、阴影、`--text-secondary` 文字 token）；**不**另起 `.doc-card` 配色，仅按 kind 加内层类（`.docx-container` / `.csv-table` / `.md-body` / `.edit-card`）。
- **限高**：所有文档卡内容区高度 `min(<N>px, 62~70vh)`、内滚（500px 最小窗不溢出）；右下"全屏"按钮放大查看。文档卡**不**默认折叠（与媒体卡一致）。
- **右下"在系统查看器打开"**链接（复用 opener 插件）。
- **全屏**（复用 `.media-lightbox` 覆层）：html/pdf 在覆层里**新建**同 src/srcdoc 的大尺寸 iframe（**不搬原节点**——搬 iframe 会触发重载、搬 docx 容器会脱挂渲染）；docx/csv/md 在覆层里**重新渲染**一份全宽内容。Esc / 点遮罩关闭、焦点归还（复用 lightbox 焦点管理）。
- **编辑入口**：渲染的 markdown / text 查看卡右上"编辑"按钮 → 该卡切编辑态（渲染 HTML 换成 `renderEditCard` 编辑 UI，预填当前内容）。

## 交互状态覆盖（loading / empty / error / success / partial）

每个渲染器走「先占位 → 成功替换 / 失败替换为错误卡」，复用现有 `.media-loading` / `.media-error` 占位（与媒体卡一致）：

| 渲染器 | LOADING | EMPTY | ERROR | SUCCESS | PARTIAL |
|---|---|---|---|---|---|
| html（iframe） | `.media-loading`「渲染中…」至 iframe `load` | 空 HTML → 空白框 + 说明 | `load` 失败/超时 → 错误卡 + 系统打开 | iframe 显示 | — |
| pdf | `.media-loading`「加载 PDF…」 | —（阅读器自处理） | 加载失败 → 错误卡 + 系统打开 | 内置阅读器 | — |
| docx | `.media-loading`「解析文档…」（fetch+renderAsync） | 空文档 →「文档为空」 | zip 损坏/解析异常 → 错误卡 + 系统打开 | 渲染分页 | — |
| csv | `.media-loading`「解析表格…」 | 0 行 →「CSV 为空」 | PapaParse 错误 → 错误卡 | 表格 | >1000 行 →「显示更多（共 N 行）」 |
| markdown 查看 | 即时 | 空文件 → 空气泡 | 解析异常 → 原文回退 | 渲染 HTML | — |
| 编辑卡 | 即时（content 预填） | 空文件 → 空 textarea | `write_file` 失败 → toast 错误、**保留编辑内容不清空** | toast「已保存」 | dirty 未保存 → 离开确认 |

错误卡统一：`.media-error` 样式 + 错误文案 +「在系统查看器打开」链接。

## 可访问性与响应式

**a11y：**
- 所有文档 iframe 带 `title`（如 `title="PDF 预览：report.pdf"`）。
- 编辑卡 `<textarea>` 关联 `<label>` 或 `aria-label="编辑 <file>"`；保存按钮 `type="button"` + 清晰文案。
- CSV `<th scope="col">`；表格外包 `role="region" aria-label="CSV 表格"`。
- 全屏覆层复用 lightbox 焦点管理（打开聚焦、Esc/点遮罩关闭、焦点归还触发元素、`aria-modal`）。
- 颜色对比沿用 `--text-secondary`（charcoal，已校对比），不引低对比灰字。

**响应式（窗口最小 600×500）：**
- 文档卡内容区高度一律 `min(<N>px, 62~70vh)`——500px 高窗口不溢出。
- 编辑卡 textarea｜预览 分栏：窗口宽 `<640px` 纵向堆叠（textarea 上、预览下），宽时左右。
- CSV/表格横向溢出 → 容器 `overflow-x:auto`。

## 安全模型（汇总）

| 渲染面 | 机制 |
|---|---|
| HTML 文件 | 沙箱 iframe `allow-scripts`（无 allow-same-origin）；opaque origin 阻断 `__TAURI__` |
| PDF | 同上沙箱；内置阅读器无脚本注入面 |
| docx | docx-preview 输出（OOXML→HTML）；DOMPurify 兜底；无 VBA |
| csv | 纯 DOM 构建（textContent），无注入面 |
| markdown 查看/预览 | markdown-it + DOMPurify（现有管线） |
| 所有路径 | `is_within_roots`（workspace+app_data），防 `../` 越权 |

硬规则：**绝不**对不可信内容用 `allow-scripts allow-same-origin` 组合；**绝不**用 Shadow DOM 当安全边界。

## 要加的 vendor 库（UMD → `/vendor/`）

- `docx-preview.min.js` + `jszip.min.js`（docx 渲染依赖 JSZip）
- `papaparse.min.js`（CSV 解析）
- PDF / HTML / markdown / 编辑：**无需新库**（WebView2 内置 + 现有 markdown-it/purify/highlight）。

`index.html` `<head>` 加 3 条经典 `<script src="/vendor/...">`（先于 deferred main.js 执行，暴露全局 `docx`/`JSZip`/`Papa`）。

## 改动文件

- `src-tauri/src/tools.rs` — `tool_display_doc` / `tool_edit_file` + `dispatch` 两条路由 + `schemas()` +6 → 单测。
- `src-tauri/src/lib.rs` — 扩展名→kind 映射（html/pdf/docx/csv/md/txt）+ `write_file` 命令 + `invoke_handler` 注册 + 单测。
- `src/index.html` — 3 条 vendor `<script>`。
- `src/main.js` — `renderDocCard` / `renderEditCard` / edit 按钮绑定 / `write_file` 调用 / 文档卡全屏（复用 lightbox）。
- `src/styles.css` — 复用 `.media-card`（玻璃底）+ 内层 `.docx-container` / `.csv-table` / `.md-body` / `.edit-card`（textarea+预览分栏 + dirty 确认）/ 文档卡限高 `min(Npx,62~70vh)` / CSV 单色玻璃表 / 窄窗堆叠 / toast。
- `docs/superpowers/smoke/2026-07-24-chat-documents.md` — 真机 smoke 清单。

## 测试策略（TDD、subagent 友好）

**Rust 单测（`cargo test --lib`，纯逻辑、可离线测）：**
- `tool_display_doc`：缺参数（path 与 html 都没给）/ path 不存在 / `.doc` 不支持 / 各 kind（html/pdf/docx/csv/md）按 path 正常返 JSON / `html` 片段返 `{"display":true,"html":...,"kind":"html"}`。
- `tool_edit_file`：缺 path / 非 md-txt / content 缺省读磁盘 / content 给定预填。
- `write_file`：workspace 内写入成功 + 读回一致；workspace 外越权返 Err；自动建父目录。
- 扩展名→kind 分类表覆盖各扩展名（含 `.htm`、大小写）。
- `schemas()` 断言 6 工具且名字序列正确。

**前端：** `node --check src/main.js`（项目无 JS 测试框架，沿用 chat-media 的做法）。

**真机 smoke（人工，写入 smoke 文档）：** agent 调 `display_doc`/`edit_file` 覆盖各类型；PDF 跳页；CSV 显示更多；docx 表格/图；HTML 脚本跑但摸不到 invoke（可放一个试探 `parent.__TAURI__` 的 HTML 验证返回 undefined/被拦）；编辑卡保存后磁盘一致；全屏/Esc；系统打开。

## 数据流（一图）

```
用户："看看 report.pdf / 把 notes.md 改一下"
  └─agent─display_doc({path:"report.pdf"})─▶ tool_display_doc ─▶ {kind:pdf,...}
       └─前端 renderDocCard ─▶ <iframe sandbox=allow-scripts src=media://report.pdf>（内置阅读器）
  └─agent─edit_file({path:"notes.md"})─────▶ tool_edit_file ─▶ {kind:markdown,content:"..."}
       └─前端 renderEditCard ─▶ textarea+预览 ─保存─▶ invoke write_file ─▶ 磁盘
```

## 实现任务（源自本次 design-review findings）

- [ ] **T1 (P1, CC ~15min)** — 后端：`display_doc` + `edit_file` 工具 + 扩展名→kind 映射 + `dispatch`/`schemas(6)` + 单测。Files: `src-tauri/src/tools.rs`, `lib.rs`. Verify: `cargo test --lib`.
- [ ] **T2 (P1, CC ~10min)** — 后端：`write_file` 命令（`resolve_path`+`is_within_roots`+写盘+建父目录）+ `invoke_handler` 注册 + scope/越权/建目录单测。Files: `lib.rs`.
- [ ] **T3 (P1, CC ~20min)** — 前端：vendor 引入（docx-preview/jszip/papaparse）+ `renderDocCard`（html/pdf iframe·docx·csv·md）含 loading/empty/error 占位 + 全屏（html/pdf 新建 iframe，不搬节点）。Files: `index.html`, `main.js`.
- [ ] **T4 (P1, CC ~15min)** — 前端：`renderEditCard`（textarea+实时预览+保存→`write_file`+dirty 轻量保护）+ 查看卡"编辑"按钮切态。Files: `main.js`.
- [ ] **T5 (P2, CC ~15min)** — 样式：复用 `.media-card` 玻璃底 + 内层类 + 文档卡限高 `min(Npx,62~70vh)` + CSV 单色玻璃表 + 编辑卡窄窗堆叠 + a11y（iframe `title`/label/`th scope`/焦点）。Files: `styles.css`.
- [ ] **T6 (P2, CC ~5min)** — smoke 文档 + 真机手测（各类型渲染/全屏/Esc/保存一致/HTML 沙箱试探 `parent.__TAURI__` 被拦/CSV 显示更多）。Files: `docs/superpowers/smoke/2026-07-24-chat-documents.md`.

## GSTACK REVIEW REPORT

| Review | Trigger | Why | Runs | Status | Findings |
|---|---|---|---|---|---|
| CEO Review | `/plan-ceo-review` | Scope & strategy | 0 | — | — |
| Codex Review | `/codex review` | Independent 2nd opinion | 0 | — | — |
| Eng Review | `/plan-eng-review` | Architecture & tests (required) | 0 | — | — |
| Design Review | `/plan-design-review` | UI/UX gaps | 1 | CLEAR | score: 6/10 → 9/10, 2 decisions |
| DX Review | `/plan-devex-review` | Developer experience gaps | 0 | — | — |

**VERDICT:** DESIGN CLEARED — frontend/interaction 缺口已折进 spec（交互状态表、a11y、响应式限高/堆叠、复用 .media-card token、全屏不搬节点）；2 个 UX 选择已定（文档卡限高+全屏、未保存轻量保护）。Eng review 尚未跑——实现前建议 `/plan-eng-review`。

NO UNRESOLVED DECISIONS
