# ovoice — Tauri + Rust 接入 MiniMax (LLM + TTS) 设计

> 目标：用 Rust + Tauri 跑通 MiniMax 的 LLM 访问（MiniMax-M3）与文本转语音（WebSocket T2A v2）。

## 范围
一个桌面聊天应用：与 MiniMax-M3 多轮对话，每条 AI 回复可一键「朗读」(TTS)。两套 API 各自验证可用。

## 架构
- 前端：Vanilla HTML/JS（无框架），单页聊天界面。
- 后端：Rust (Tauri v2)，模块化：
  - `llm.rs`：messages → 文本（POST `https://api.minimaxi.com/v1/chat/completions`，OpenAI 兼容，model `MiniMax-M3`）。
  - `tts.rs`：text → 音频字节（WebSocket `wss://api.minimaxi.com/ws/v1/t2a_v2`，事件流 `connected_success → task_start → task_continue → task_finish`，音频为 hex，解码为字节）。
  - `commands.rs`：Tauri command 桥接（`chat`、`speak`）。
  - `main.rs`/`lib.rs`：组装并注册 command。

## 关键决策
| 项 | 决策 | 理由 |
|---|---|---|
| LLM 流式 | 先非流式整段返回 | 跑通优先、最可靠；结构预留 SSE 流式 |
| TTS 音频回传 | base64 data URL 给前端 `<audio>` | 无需配 asset protocol |
| 多轮上下文 | 前端维护 messages 历史，整体发送 | 实现简单 |
| API Key 存储 | `src-tauri/.env`（gitignored），`dotenvy` 读取，可被环境变量覆盖 | 不硬编码进源码 |

## 默认参数
- LLM：`MiniMax-M3`，temperature 默认。
- TTS：model `speech-2.8-hd`，voice `male-qn-qingse`，format `mp3`，sample_rate 32000，channel 1。

## 数据流
- 对话：前端 `invoke('chat', {messages})` → Rust → MiniMax → 文本 → 渲染气泡。
- 朗读：前端 `invoke('speak', {text})` → Rust WebSocket → 收集 hex 音频 → 解码 → base64 → `<audio src="data:audio/mp3;base64,...">` 播放。

## 安全
- API Key 仅存 Rust 侧，前端永不获取。
- `.env` 加入 `.gitignore`。

## 验证
- `pnpm tauri dev` 启动；输入提问确认 LLM 返回；点朗读确认音频可播放。
