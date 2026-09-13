# chat-media-rendering 冒烟记录

> 日期：2026-07-24 · 分支 `feat/chat-media`（off feat/background-image @ d492f33）
> 自动化：controller 已跑，PASS（75/75 lib 测试，零 warning 构建）。
> 交互式真机 smoke 需人工运行 `pnpm tauri dev` 后按下表执行。

## 1. 自动化（controller 已跑，PASS）

| 检查 | 命令 | 结果 |
|---|---|---|
| 单元/回归 | `cargo test --manifest-path src-tauri/Cargo.toml --lib` | **75 passed; 0 failed**（config 8 + jobs 9 + tools 22 + llm 18 + agent 4 + media 4 + 其它 10） |
| 全量构建 | `cargo build --manifest-path src-tauri/Cargo.toml` | **Finished，零 warning** |
| 前端语法 | `node --check src/main.js` | OK |

关键单测：media kind/mime/is_within_roots/parse_range（含 `../` 越权拒绝、Range 三种形式 + 畸形）；display_media 缺失/不支持/正常返 JSON；schemas 含 4 工具。

## 2. 交互式真机 smoke（人工执行，待回填）

前置：`taskkill //F //IM ovoice.exe` → `pnpm tauri dev`。让 agent 跑 mmx 产出媒体，或在工作目录放测试文件后让 agent 调 `display_media`。

| # | 操作 | 预期 | 实测 | 备注 |
|---|---|---|---|---|
| 1 | agent 调 `display_media({path:"t.png"})`（工作目录放张图） | 玻璃媒体卡出现在**工具区（正文上方）**；图片显示；max-height 360px 不撑爆 | ☐ | 验证位置 + 尺寸 |
| 2 | 点图片 | 应用内 lightbox 全屏放大；Esc/点遮罩/点图关闭；焦点归还 | ☐ | 验证 lightbox + 焦点 |
| 3 | agent 调 `display_media({path:"v.mp4"})` | 视频卡；`controls` 显示；**不自动播放**；可拖进度条（Range/206） | ☐ | **核心**：Range 支持拖进度 |
| 4 | agent 调 `display_media({path:"a.mp3",caption:"示例"})` | 音频原生控制条 + caption 文字 | ☐ | caption 渲染 |
| 5 | `display_media({path:"missing.mp4"})` | 文本卡："文件不存在：…" | ☐ | 缺失降级 |
| 6 | `display_media({path:"a.txt"})` | 文本卡："不支持的媒体类型：.txt" | ☐ | 不支持降级 |
| 7 | 指向一个内容损坏的 .mp4（或改后缀的乱码） | 黑屏 + "无法加载或解码该文件"提示 + "在系统查看器打开"链接可调系统播放器 | ☐ | error 兜底 |
| 8 | 任意媒体卡右下"在系统查看器打开" | 系统默认程序打开该文件 | ☐ | opener 插件 |
| 9 | 设置页拖"面板/气泡 不透明度"滑块 | 媒体卡玻璃底随滑块变透/实 | ☐ | --glass-alpha 联动 |
| 10 | 一轮里连续两次 display_media | 两张卡纵向堆叠、按调用顺序、不串 | ☐ | 多媒体排序 |
| 11 | 任务页/设置页切换后回来 | 媒体卡仍在（同气泡内） | ☐ | 视图切换不丢 |

## 3. 已知限制 / 后续

- **编解码**：仅 WebView2(Chromium) 可解码格式能播（mmx 的 mp4/mp3 没问题）；avi/部分 mkv 可能黑屏 → 走"打开"兜底。
- **非 Range 请求整文件入内存**：视频走 Range（浏览器默认），200 全量路径极少触发；超大文件流式/mmap 留 v2。
- **scope**：仅放行 workspace + app_data 下文件（防 `../` 越权）。
- **lightbox 仅图片**：视频/音频用原生 controls + 系统打开，不做应用内放大。
- **路径**：agent 给相对路径（基于工作目录）或绝对路径均可；后端 canonicalize 后经 `convertFileSrc(path,"media")` 喂前端。

## 4. 回滚

单提交回退 `git revert <hash>`；整支回退 `git reset --hard d492f33`（feat/chat-media 起点前）。分支未推送。
