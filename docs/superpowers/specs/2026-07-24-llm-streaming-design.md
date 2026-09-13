# ovoice 流式输出（SSE）设计

> 目标：LLM 回复从「整段蹦出」改为「思考 + 文本逐字浮现」，与现有工具循环正交、不破坏工具语义。

## 范围

**做：** MiniMax-M3 流式（`stream:true` + SSE）——思考（`reasoning_details`）与最终文本（`content`）逐字实时浮现；工具调用参数流式累积，与现有 12 轮工具循环无缝衔接；流式 token 用量（`usage`）回传。

**不做：** 多模态、附件上传、Markdown 每字符重渲（流式期间只 append 原始文本，结束时一次性渲染）。

## 实测的流式格式（决定实现方式）

| 字段 | 格式 | 处理 |
|---|---|---|
| `delta.content` | **增量**片段（实测 3 块，6+31+26 字） | append |
| `delta.reasoning_details[].text` | **增量**片段（实测 72 块，思考占 completion 90% token） | append，实时进思考区 |
| `delta.tool_calls` | 按 `index` 累积：首块带 `id`+`function.name`+`function.arguments`，后续块追加 `arguments` | 按 index 拼 id/name/args |
| `choices[0].finish_reason` | `stop`（纯文本结束）/ `tool_calls`（本轮要调工具） | `tool_calls`→执行→再开流 |
| `usage` | 仅最后一块（需 `stream_options.include_usage:true`） | 仅记入 `RoundResult`（日志/后续 token 展示用），不进 `ChatResponse` |

> ⚠️ 官方 Python 示例对 content 做 `text[len(buffer):]` diff 是**误导**——实测 delta 就是增量，直接 append。

**关键 UX 洞察：** completion token 里 ~90% 是 reasoning。即当前「等几秒才蹦出答案」的时间，**九成在等思考**。流式后最大收益是**思考过程逐字浮现**，思考完答案几块秒出。

## 关键决策

| 项 | 决策 | 理由 |
|---|---|---|
| SSE 解析 | reqwest `stream` feature + 手动解析 `data:` 行 | 不引新 crate；reqwest 已是依赖 |
| 渐进渲染 | 流式期间往 `.bubble-text` append **原始文本**（pre-wrap），流结束 `renderMarkdown` 全量美化 | 每 token 重渲 markdown 不可接受；append 文本轻量、逐字可见 |
| 思考流 | 复用 `llm-thinking` 事件，**增量** append（已支持） | 思考区已是 append 语义，零改动复用 |
| 文本流 | 新增 `llm-content` 事件，前端 append 原始文本 | 与思考流对称 |
| 工具正交 | `tool_calls` 按 index 累积，`finish_reason:tool_calls` 触发执行 + 再开一段流 | 工具语义零变化，只是「调用如何到达」从一次性变流式 |
| 护栏不变 | 12 轮上限、stdin=null、超时、输出截断、history/error 模型全部沿用 | 流式只换传输，不换策略 |
| 失败兜底 | 流中途断 → 已累积 content 当最终答案 + `error` 标注；首连失败 → error | 不丢已生成内容 |

## 架构

后端（`llm.rs`）：
- `HttpRound::round(messages, cfg, emit)` 改为**流式**：`reqwest` 拿 `bytes_stream()`，按行切分，解析 `data: {json}`，逐块 `emit.thinking/content` 并累积，结束时返回组装好的 `RoundResult`。
- `LlmRound` trait：`round(&self, messages, cfg, emit: &dyn Emitter) -> Result<RoundResult, String>`（比旧 `ParsedResp` 多 `finish` + `usage`；`emit` 参数让 round 边收边推）。
- `Emitter` trait 加 `async fn content(&self, text: &str)`（推送文本增量）。
- `run_loop`：消费 `RoundResult`——`finish==ToolCalls` 则执行工具、塞结果、`continue` 再开一轮流；`finish==Stop` 则收尾。12 轮上限/history/error 不变。

前端（`main.js`）：
- 监听新增 `llm-content`（增量）→ append 到当前 `.bubble-text`（流式期间原始文本、pre-wrap）。
- `chat` resolve 后 `renderMarkdown(content)` 全量美化替换原始文本（复用现有渲染）。
- `llm-thinking`（增量 append，已有）、工具卡（不变）。

## 数据结构

```rust
pub enum FinishReason { Stop, ToolCalls }

pub struct RoundResult {
    pub content: String,            // 累积的最终文本（已 strip 思考块）
    pub reasoning: String,          // 累积的思考
    pub tool_calls: Vec<Value>,     // 按 index 拼装好的 [{id, function:{name, arguments}}]
    pub finish: FinishReason,
    pub usage: Option<Value>,
    pub assistant_message: Value,   // 回传给模型（tool_calls 在场时 content=null）
}

#[async_trait]
pub trait LlmRound: Send + Sync {
    async fn round(&self, messages: &[Value], cfg: &Config, emit: &dyn Emitter) -> Result<RoundResult, String>;
}

#[async_trait]
pub trait Emitter: Send + Sync {
    async fn thinking(&self, text: &str);   // 增量
    async fn content(&self, text: &str);    // 增量（新增）
    async fn tool_call(&self, name: &str, args: &str);
    async fn tool_result(&self, name: &str, result: &str);
}
```

## SSE 解析要点（`HttpRound::round`）

- 请求体加 `"stream": true, "stream_options": {"include_usage": true}`（其余 tools/reasoning_split 不变）。
- `resp.bytes_stream()` → 按 `\n` 切行缓冲；`data: <json>` 行解析，`data: [DONE]` 结束。
- `tool_calls` 累积：`HashMap<index, {id, name, args_buf}>`；每块按 `index` 更新 id/name（若有）、`args_buf.push_str(arguments)`（若有）。结束时转成 `Vec<Value>`（顺序按 index）。
- 边收边 `emit.thinking(piece)` / `emit.content(piece)`。
- `finish_reason` 到达即记 `finish`；`usage` 到达即记。
- **思考块剥离**：现有 `strip_thinking_blocks` 仍作用于累积 `content`（模型偶尔仍把 `<details>思考` 塞 content），与流式不冲突（结束时对累积 content 清洗）。

## 工具循环与流式（正交）

```
for i in 0..12 {
    emit "[loop] round i"
    let r = round.stream(messages, cfg, &emitter).await?;   // 边推 thinking/content 增量，返回 RoundResult
    last_content = strip_thinking_blocks(&r.content);  // 兜底清洗
    if r.finish == ToolCalls {
        messages.push(r.assistant_message); history.push(r.assistant_message);
        for call in r.tool_calls { emit tool_call; dispatch; emit tool_result; push tool msg; }
        continue;   // 再开一段流
    } else {
        // Stop：收尾，最终文本已在流式期间逐字显示，此处 renderMarkdown 美化
        return ChatResponse { content, history, error: None };
    }
}
// 12 轮兜底（不变）
```

## 前端数据流（增量）

```
invoke('chat') 期间持续收事件：
  llm-thinking  → appendReasoning（已有，逐字进折叠区）
  llm-content   → appendContent：.bubble-text 追加原始文本（pre-wrap，逐字可见）
  llm-tool-call/result → 工具卡（不变）
resolve { content, history, error, usage }：
  messages.push(...history)
  .bubble-text = renderMarkdown(content)   // 流结束后全量美化替换原始文本
  attachSpeak
```

## 错误处理

- 首连失败（网络/鉴权）：`round` 返回 Err → `run_loop` 走现有 H4 路径（回传 partial history + error）。
- 流中途断：以**已累积**的 content/reasoning 组装 `RoundResult` 并标 `error`（不丢已生成内容）；前端把已 append 的文本保留 + 显示错误标注。
- 解析失败的单个 `data:` 行：跳过该行（不中断整条流）。

## 测试

- `llm.rs` 单测：SSE 行解析（`data:`/`[DONE]`/坏行跳过）；tool_calls 按 index 累积（多块 args 拼装）；`RoundResult` 组装（content 清洗、assistant_message content=null when tool_calls）。
- **循环单测（注入）**：`FakeRound` 实现新 trait——按脚本 `emit.thinking/content` 增量 + 返回脚本化 `RoundResult`；断言前端可见的增量事件顺序、工具轮→再开流、Stop 收尾、中途失败兜底。
- 冒烟：`pnpm tauri dev`，发一条需要思考的问题 → 看思考逐字浮现、答案逐字浮现；发一条两轮工具任务 → 工具照常、流式不破。

## 受影响文件

- 改：`src-tauri/src/llm.rs`（流式 `HttpRound::round` + `RoundResult`/`FinishReason` + `LlmRound`/`Emitter` 加 content）、`src-tauri/src/lib.rs`（`AppEmitter` 加 `content` → emit `llm-content`）、`src-tauri/Cargo.toml`（reqwest 加 `stream` feature）、`src/main.js`（`llm-content` 监听 + append + 流后 renderMarkdown）、`src/markdown.js`（不变）、`src/styles.css`（流式期间 `.bubble-text` 原始文本 pre-wrap，已有 `.bubble-text:not(.md)`）。
- 新增：`docs/superpowers/specs/2026-07-24-llm-streaming-design.md`（本文件）。
