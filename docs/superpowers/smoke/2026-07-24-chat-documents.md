# chat-documents 冒烟记录

> 日期：2026-07-24 · 分支 `feat/chat-documents`（off master @ 2a8fdf9）
> 自动化：cargo test --lib 全 PASS + 零 warning（cargo check --all-targets）+ node --check OK。
> 真机 smoke：dev server 已在跑（`npm run tauri dev`），按下表执行并回填「实测」。

## 1. 自动化（已跑）

| 检查 | 命令 | 结果 |
|---|---|---|
| 单元/回归 | `cargo test --manifest-path src-tauri/Cargo.toml --lib` | 全 PASS（**93 passed**，含终审加固的 traversal 测试） |
| 零 warning | `cargo check --manifest-path src-tauri/Cargo.toml --all-targets` | 零 warning 零 error |
| 前端语法 | `node --check src/main.js` | OK |

## 1.5 smoke 样本文件（已就位于 workspace/smoke/）

workspace = `%APPDATA%\com.ovoice.app\workspace`。agent 相对路径解析到此，故下表用 `smoke/<file>` 即可。media:// scope 在 workspace+app_data 内，可正常服务。

| 文件 | 大小 | 验证点 |
|---|---|---|
| `smoke/sample.html` | 1.5KB | 沙箱 iframe 渲染 + `parent.__TAURI__` 探针（#1/#2/#3） |
| `smoke/sample.pdf` | 590B | 内置阅读器（#4）；手算 xref 的最小合法 PDF |
| `smoke/sample.docx` | 1.6KB | docx-preview（#5）；最小合法 docx（Content_Types + rels + document.xml） |
| `smoke/data.csv` | 1501 行 | 首 1000 行 + 「显示更多（共 1500 行）」（#7） |
| `smoke/notes.md` | 0.5KB | markdown 查看 + 编辑切态（#8/#11） |
| `smoke/old.doc` | 59B | 不支持 .doc 提示卡（#6） |

> 驱动方式：在 ovoice 对话里发自然语言让 agent 调工具，如「用 display_doc 展示 smoke/sample.pdf」；或直接要求「调用 display_doc 工具，path=smoke/sample.html」。


## 2. 真机 smoke（人工，回填）

| # | 操作 | 预期 | 实测 |
|---|---|---|---|
| 1 | agent `display_doc({path:"a.html"})` | 沙箱 iframe 渲染；脚本跑；限高内滚 | ☐ |
| 2 | 该 HTML 含 `parent.__TAURI__` 探针 | 探针拿不到 invoke（opaque origin 阻断） | ☐ |
| 3 | agent `display_doc({html:"<b>内联</b>"})` | srcdoc iframe 渲染片段 | ☐ |
| 4 | agent `display_doc({path:"r.pdf"})` | 内置阅读器；缩放/搜索；`min(520px,70vh)` | ☐ |
| 5 | agent `display_doc({path:"d.docx"})` | docx-preview 表格/图/分页 | ☐ |
| 6 | agent `display_doc({path:"old.doc"})` | 提示卡「不支持 .doc」+ 系统打开 | ☐ |
| 7 | agent `display_doc({path:"data.csv"})` | 表格；首 1000 行；"显示更多（共 N 行）" | ☐ |
| 8 | agent `edit_file({path:"n.md"})` | 编辑卡 textarea+预览；预填磁盘内容 | ☐ |
| 9 | 改后点"保存" | toast「已写入 …」；磁盘内容一致 | ☐ |
| 10 | 编辑后不保存，保存设置（触发 reset） | confirm「有未保存…放弃？」 | ☐ |
| 11 | markdown 查看卡点"编辑" | 切编辑态，预填当前内容 | ☐ |
| 12 | 任意文档卡点"全屏" | 覆层全宽展示；Esc/点遮罩关；焦点归还 | ☐ |
| 13 | 窗口收到 <640px 宽 | 编辑卡 textarea/预览纵向堆叠 | ☐ |
| 14 | write_file 越权路径（前端构造） | Err「路径不在允许范围内」；不落盘 | ☐ |

## 3. 已知限制

- `.doc` 旧二进制不支持（提示卡 + 系统打开）。
- CSV 仅首 1000 行 + 显示更多（>10k 行虚拟化留 v2）。
- 跨域沙箱 iframe 无法自适应高度 → 限高 + 全屏放大。
- docx 全屏走 `cloneNode`（静态 DOM 克隆，不重跑脚本）。
