# edit_card 按类型自适应（P-2026-002）

- **日期**：2026-07-29
- **提案**：`workspace/projects/ovoice-test/proposals/P-2026-002_edit_card按类型自适应.md`
- **关联**：P-2026-001（markdown 预览间距，已合并 master `6733a45`）
- **分支**：`feat/edit-card-kind-adaptive`（基于 master `4e6ac2f`）

## 现状

`renderEditCard`（main.js:1187）不论文件类型一律渲染"左编辑右预览"双栏（`.edit-split { display:flex }`，textarea 与 preview 各 `flex:1 1 0` 等分宽度）。后端 `tool_edit_card`（tools.rs:198）按扩展名判 `kind`，但 edit_card 只接受两类：
- `markdown`（.md）—— 预览有实际价值（渲染标题/列表/代码高亮）
- `text`（.txt / .py / .rs / .json / .yaml …）—— 预览区只是把 textarea 内容原样塞进 `<pre>`，与编辑区视觉几乎一致，预览意义低

**痛点**（提案原文）：
1. 双栏占用屏幕宽度，编辑区被压缩
2. .txt / 配置类不需要预览

## 方案：只有 markdown 需要预览

| kind | 布局 | 预览 | 预览按钮 |
|---|---|---|---|
| `markdown` | **双栏**（左编辑右预览）—— 不变 | 实时 markdown 渲染（不变） | 无（本就双栏） |
| `text` | **单栏**（textarea 撑满） | **无**（不建 preview DOM） | **无** |

text/code 纯单栏：不创建 preview DOM、不加任何按钮，textarea 作为 `.edit-split`(flex) 的唯一子元素 + `.edit-textarea{flex:1 1 0}` 自然撑满宽度。**布局完全由 DOM 子元素数量决定，零 CSS 新增。**

> 设计取舍：曾考虑给 text 模式加「预览」按钮 toggle 右侧只读预览（贴提案字面"必要时提供点按预览按钮"），用户明确否决——text/code 预览与编辑区同质（都是等宽纯文本），无价值，遂移除。

## 改动

### `src/main.js` — `renderEditCard`

- 提取 `const isMd = j.kind === "markdown"`。
- `split.className = "edit-split"`（不再按 kind 分叉）。
- **preview DOM 只在 `isMd` 时创建并 append**（`let prev = null; if (isMd) { prev = ...; split.appendChild(prev); }`）。text 模式 split 内只有 textarea。
- `refresh()` early-return：`if (!prev) return;` —— text 模式直接跳过（不再有"塞裸 `<pre>`"分支）。
- `view` 按钮里的 kind 判断改用 `isMd`（DRY）。
- **不新增任何按钮**（无「预览」按钮）；actions 仍是「查看 / 保存」。
- 其余（save / dirty 守卫 / 200ms debounce）不变。

### `src/styles.css`

**零改动。** `.edit-split { display: flex; gap: 8px; }` 保持原样——markdown 时 split 有两个 flex 子（textarea + preview，等分）；text 时只有一个（textarea，撑满）。窄屏纵向堆叠规则（`@media max-width:640px`）继续对两种模式生效。

## 不动

- **后端**（tools.rs）：kind 判定已就绪，edit_card 仍只接受 markdown/text。
- **markdown 路径**：双栏 + 实时预览 + P-2026-001 紧凑排版，零改动。
- **doc-card 路径**（renderDocCard / appendEditButton）：appendEditButton 切回 edit-card 时带 kind，同样走 renderEditCard 的新分支，自动覆盖。
- **live vs history 双路径**：renderEditCard 是 live（setupAgentEvents）与 history（buildHistoryBubbles，main.js:1349）的共用渲染函数，改一处两路径同时生效（[[ovoice-live-vs-history-render-paths]] 不踩坑）。

## 验证

1. `node --check src/main.js`。
2. release exe 手测：
   - agent `edit_card` 一个 `.md` → 双栏 + 实时预览（与改前一致，P-2026-001 紧凑排版不变）。
   - agent `edit_card` 一个 `.txt` / `.py` / `.json` → **单栏**（textarea 撑满），**无预览按钮**，actions 只有「查看 / 保存」。
   - 历史重载（重启 / 切会话回看）：text edit_card 仍单栏，markdown 仍双栏。
   - 窄窗（<640px）：两种模式都纵向堆叠。

## 约束

- dev server 锁 debug exe → 前端纯 JS 改动靠 release 重嵌（`cargo build --release` 重跑 `generate_context!`）或 dev server Reload 验证。
- 分支策略（CLAUDE.md）：在 `feat/edit-card-kind-adaptive` 上做，严禁直接落 master；merge 用 `--no-ff`。
- 提交信息以 `Co-Authored-By: Claude <noreply@anthropic.com>` 结尾。
