# ovoice 前端设计审计 — 2026-07-22

> 工具：`/gstack-design-review`。目标=ovoice 桌面应用前端（深色聊天 + 朗读）。
> 适配说明：本项目**无 git 仓库**，故 skill 的"每修复一个原子提交"改为**直接编辑 + dev 重启验证**；Tauri 是原生窗口而非 URL，故用 PowerShell (user32 EnumWindows/SetWindowPos) **抓取真实运行窗口**做视觉审查。

## 评分
- **Design Score: B** — 合格的 App-UI：克制配色、清晰层级、比例均衡、功能完整。
- **AI Slop Score: A−** — 几乎无 slop 特征（无紫渐变 / 无三栏卡片网格 / 无装饰性 emoji 标题 / 圆角有层级 / 开场白是对话非"Welcome to"）。

## 发现与修复（全部已落地验证）

| # | 影响 | 类别 | 问题 | 修复 | 状态 |
|---|---|---|---|---|---|
| F1 | 中 | 交互态 | 发送按钮缺 hover/active，点击无反馈 | 加 `:hover`(brightness 1.1)/`:active`(brightness .93 + 下沉) + transition | ✅ verified |
| F2 | 中 | 可访问性 | 按钮无键盘聚焦环 | 加 `.speak-btn/#send-btn/#chat-input:focus-visible` 绿色 outline | ✅ verified |
| F3 | 低 | 排版 | 正文 15px < 16px 基线 | 提升到 16px | ✅ verified |
| F4 | 低 | 交互态 | 朗读按钮缺按下态 | 加 `:active` translateY(1px) | ✅ verified |

验证方式：杀残留 ovoice.exe → 重启 `pnpm tauri dev` → 抓图确认编译/启动/渲染均无异常（PID 30508，初始态：1 个开场白气泡 + 朗读按钮，无破坏）。

## 刻意保留（不视为缺陷）
**系统字体栈** `-apple-system, "Segoe UI", "Microsoft YaHei", …`：skill 把 `-apple-system` 列为 AI slop，但该规则是拉丁字体语境。这是**中文应用**——CJK 网页字体 5–15MB，桌面端加载不现实，系统 YaHei/PingFang 本就是中文渲染最优解。**正确决策，保留。**

## 可选后续（功能向，非设计缺陷）
- LLM 流式打字效果（SSE）— 体感提升大
- TTS 音色下拉选择
- 清空对话 / 滚到底部按钮
