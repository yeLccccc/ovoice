# edit_card markdown 预览间距修复（P-2026-001）

- **日期**：2026-07-28（2026-07-29 定位到真因）
- **关联提案**：`workspace/projects/ovoice-test/proposals/P-2026-001_edit_card间距优化.md`
- **优先级**：P1
- **状态**：已实现

## 症状

edit_card 渲染 .md 时，预览态（`.edit-preview.md`）与查看态（`.md-body.md` 内联）的 markdown 各元素间距过大、松散；用户反复描述为「行间距过大」「标题行间隔过大」。窄窗尤甚。

## 真因（定位过程）

**`.bubble { white-space: pre-wrap }`（styles.css:160）被后代继承。**

app 里 `.edit-preview.md`（编辑卡预览）与内联 `.md-body.md`（查看态）都包在 `.bubble` 内 → 继承了 `pre-wrap` → markdown-it 输出 HTML 里的源换行/空白被原样保留渲染 → 多余空行/间距。**这不是 margin、不是 line-height、不是宽度、不是渲染引擎**——是 `white-space` 继承。

定位证据（四象限互证，全部吻合）：

| 容器 | 在 `.bubble` 内？ | `white-space` | 用户观感 |
|---|---|---|---|
| 聊天气泡 `.bubble-text.md` | 是 | **`normal`**（显式覆盖，styles.css:437） | 正常 |
| 全屏 `.md-body.md` | 否（克隆挂 `document.body`） | 继承 `normal` | 正常 |
| 独立测试页 `.edit-preview.md` | 否 | 继承 `normal` | 正常 |
| **编辑预览 `.edit-preview.md` / 内联查看 `.md-body.md`** | **是** | **继承 `pre-wrap`，无覆盖** | **松散** |

代码作者本就知道这个坑——`.bubble.assistant .bubble-text.md { white-space: normal }`（styles.css:437）+ 注释「三段气泡里 .bubble-text 不再继承 pre-wrap」（styles.css:630）——但**只给聊天气泡文本兜了底，漏了 `.edit-preview.md` / `.md-body.md` 这两个同样在 `.bubble` 内的 markdown 容器**。

### 排查走过的弯路（记录以防再犯）

1. 误判为「缺元素排版」（h/p/ul 走浏览器默认 margin）→ 补了元素排版块。部分改善，但非真因。
2. 误判为 line-height（`.md-body.md` 曾是 1.6 异常值）→ 反复试 1.5/1/0.1/0.5/0.9。无果。
3. 误判为宽度（textarea 平分）→ 试纵向堆叠。无果。
4. 建独立测试页（`src/vendor` + 同份 `styles.css` + 同款容器，但**无 `.bubble` 祖先**）→ 测试页正常。这一步是转折：磁盘 CSS 正确、渲染管线正确，**唯独 app 上下文不同** → 锁定 `.bubble` 继承，找到 `white-space: pre-wrap`。

## 修复

**核心（一行）**：`.md-body.md, .edit-preview.md { white-space: normal }`——与聊天气泡同一个兜底，抵消从 `.bubble` 继承的 `pre-wrap`。

**附带紧凑排版**（用户要「行间距小一点」的密度偏好，非真因但保留）：`.md-body.md / .edit-preview.md` 共享块统一收紧 line-height（1.4）+ 全部块级 margin（p 3px / h 4px-1px / li 1px / blockquote·pre 4px）+ 补齐此前走浏览器默认的 hr/行内 code/table/img。

**附带**：edit-preview markdown 分支打 `.md` 类（main.js，与 `.bubble-text.md` / `.md-body.md` 对齐成统一「markdown 容器」标记）。

## 改动清单

| 文件:行 | 改动 |
|---|---|
| `src/styles.css`（共享排版块） | 新增 `.md-body.md, .edit-preview.md { white-space: normal }`（**真因修复**）+ 紧凑元素排版（line-height 1.4、全部 margin 收紧、hr/行内code/table/img 规则） |
| `src/styles.css:851` | `.md-body.md` line-height 1.6 → 1.4 |
| `src/styles.css:901` | `.edit-preview` line-height 设 1.4 |
| `src/main.js:1200` | edit-preview markdown 分支 className `"edit-preview"` → `"edit-preview md"`（text 分支不变） |

**不动**：assistant 气泡排版（styles.css:435-552）、`.bubble` 的 pre-wrap（聊天纯文本仍需它，styles.css:630）、渲染逻辑（markdown.js）、后端。

## 验证

- `node --check src/main.js`（className 改后语法过）。
- 独立测试页（`src/vendor` + 同份 CSS，无 `.bubble` 祖先）渲染正常 → 证明 CSS 本身正确；app 内补 `white-space:normal` 后与测试页一致。
- release 手测：用户确认编辑预览/查看态间距正常，与全屏/聊天气泡一致。

## 约束

- 分支 `feat/edit-card-md-spacing`（从 master），严禁直接落 master；merge 用 `--no-ff`。
- 纯前端 CSS+JS，dev server 锁 exe 与本改动无关。
- 提交信息以 `Co-Authored-By: Claude <noreply@anthropic.com>` 结尾。
