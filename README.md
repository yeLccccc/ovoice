# ovoice

语音驱动的常驻 AI 助手（Windows 桌面应用）。按住热键说话 → 语音转写 → 常驻 agent 自主工作 → 结果语音/文字返回。基于 Tauri 2（Rust 后端 + 原生 JS 前端），模型为 MiniMax M3（1M token 上下文）。

## 功能

- **语音交互**：热键按住说话、百度 ASR 转写、MiniMax TTS 朗读（可关）
- **常驻会话**：事件驱动 driver；历史 = append-only JSONL（唯一真相源），上下文每轮从盘重建，重启不丢
- **记忆系统（dream）**：空闲 2h 或上下文达 300k token 时，自动把近期历史整理进四级漏斗（日记忆 → 月/年摘要 → MEMORY.md 索引），marker 硬切控制上下文窗口
- **子代理**：主 agent 可 spawn 后台子代理（write/read/edit/bash，干净上下文），完成自动回注结果；失败原因与部分产出诚实回传；螺旋熔断防死循环
- **ask_user**：主 agent 主动向用户提问（候选项 + 自定义输入 + 15s 倒计时，超时自动选推荐项），回答以 tool_result 落盘
- **任务管理**：sqlite 任务看板（Current/Short/Long/Vision 四视野）+ 精确 timer 调度
- **后台作业**：进程型 job（编译/测试长任务）完成自动汇报

## 构建

依赖：Rust（MSVC）+ Node/pnpm + Tauri 2 前置依赖（Windows：WebView2）。

```powershell
pnpm install
pnpm tauri dev      # 开发
./build.ps1         # 绿色 portable 包（release/ovoice-portable/）
```

## 配置

首次启动在「设置」页填写（**自备，仓库不含任何密钥**）：

- MiniMax API Key（chat + TTS）
- 百度智能云语音凭据（AppID / API Key / Secret Key，纯中文普通话模型 devpid=1537）

所有会话历史、记忆、录音、任务库均在本地（`Documents/ovoice` 与应用 cache 目录），无遥测、无上传。

## 仓库结构

```
src/                 前端（vanilla JS/CSS）
src-tauri/src/       Rust 后端（agent driver / llm / tools / context / memory / subagents / tasks ...）
openspec/            规格驱动开发：change 提案 → spec → tasks → archive
docs/superpowers/    开发日志（specs / plans / smoke 记录，AI 协作过程完整留痕）
.claude/             openspec 技能套件（slash commands + skills）
```

开发约定见 [CLAUDE.md](CLAUDE.md)：feat 分支开发、master 只进 --no-ff 合并、openspec 全流程。

## License

MIT，见 [LICENSE](LICENSE)。

portable 包内置的 `busybox64u.exe` 来自 [busybox-w32](https://frippery.org/busybox/)（GPL-2.0），随包分发时其源码获取途径以此链接满足；如自行构建可从上述地址获取源码。
