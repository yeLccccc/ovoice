# ovoice Agent —— 工具调用 + write/read/bash + 思考分离 设计

> 目标：把 ovoice 从「单轮问答」升级为「能调用工具的 agent」。LLM 可自主调用 `write`/`read`/`bash` 完成多步任务；推理过程（thinking）与最终答复分离展示。

## 范围（本轮）

**做：** 工具调用基础设施 + `write`/`read`/`bash` 三工具 + `reasoning_split` 思考分离展示 + 可配置工作目录 + 全自动执行（配非交互护栏）。

**不做（留作后续独立 spec）：** 流式输出（逐字浮现）、多模态图片/视频输入、附件上传。

> MiniMax-M3 的 OpenAI 兼容端点原生支持上述全部能力（`tools`、`reasoning_split`、`stream`、`image_url`/`video_url`），本轮只取工具 + 思考两块，结构为后续留出扩展位。

## 关键决策

| 项 | 决策 | 理由 |
|---|---|---|
| 执行确认 | **全自动执行**，不弹确认 | 贴合语音自动发送体验；用非交互护栏兜底 |
| 工具循环归属 | **Rust 权威循环** + Tauri 事件实时推送 | 安全边界在 Rust；复用现有事件模式（同语音功能）；前端可「看 agent 干活」 |
| 思考展示 | `reasoning_split:true`，思考进独立可折叠区 | `content` 干净（替代 `<think>` strip hack）；speak 不读思考噪声 |
| 工作目录 | 可配置 `workspace_dir`，默认 `app_data_dir/workspace` | 相对路径可预测；绝对路径仍放行（模型可访问任意位置） |
| 多轮上下文 | 前端**独占** `messages`；Rust 无跨调用状态，`ChatResponse` 回传本轮追加的 history 片段，前端 append | 工具中间消息必须进历史，否则下一轮模型丢上下文 |
| speak 卫生 | 气泡拆三段，speak 只读 `.bubble-text`（最终答案） | 思考/工具噪声不被朗读 |
| 工作目录默认 | serde 默认 `""`，`config::load(app)` 把空值解析为 `app_data_dir/workspace` | Config 现为静态 serde 默认，运行时路径需 AppHandle，load 时补齐 |
| bash 输出编码 | 命令前缀 `chcp 65001>nul &&` 强制 UTF-8 码页，兜底 encoding_rs 按 OEM 解码 | Windows cmd 默认 cp936(GBK)，`from_utf8` 会乱码 |
| 进程超时清理 | bash 子进程入 Windows Job Object，超时杀整棵树 | `cmd /C` 杀 cmd 不杀孙进程，孤儿常驻 |
| 失败保历史 | `chat()` 永不丢历史：`ChatResponse{content,history,error}`，中途 API 失败也回传 partial history | 否则下一轮模型重做、丢失已学上下文 |

## 架构

后端（Rust / Tauri v2）：
- `llm.rs`：新增 `chat_once(messages, cfg) -> serde_json::Value`（单轮，返回含 content/reasoning_details/tool_calls 的整份响应）。`chat()` 重写为工具循环。
- `tools.rs`（新模块）：工具注册表（name → JSON Schema + 执行函数）、`write`/`read`/`bash` 实现、路径解析（相对工作目录）、输出截断、循环调度。
- `lib.rs`：`chat` 命令返回 `ChatResponse { content, history, error: Option<String> }`（`error` 非空表示本轮失败但已保留 partial history）；循环内 `app.emit("llm-thinking" | "llm-tool-call" | "llm-tool-result", ...)`。

前端（Vanilla JS，无打包器）：
- `main.js`：监听三个新事件，实时渲染思考区/工具卡；`chat` resolve 后把最终文本渲染进 `.bubble-text`，并把 `history` 追加进 `messages`。
- `markdown.js`：不变（最终答案仍走 `renderMarkdown`）。
- `styles.css`：新增 `.bubble-reasoning`（可折叠灰字）、`.bubble-tools`/工具卡样式。
- `index.html`：不变。

## API 调用（`llm.rs`）

请求体（OpenAI 兼容 `/chat/completions`）：
```json
{
  "model": "MiniMax-M3",
  "messages": [...],
  "tools": [ {write}, {read}, {bash} ],
  "tool_choice": "auto",
  "reasoning_split": true
}
> `thinking` 省略即默认开启（文档：adaptive 等同开启），无需显式传。
```

开启 `reasoning_split` 后响应：
- `choices[0].message.content` —— 干净的最终文本（不再含 `<think>`）。
- `choices[0].message.reasoning_content` / `reasoning_details[].text` —— 思考过程。
- `choices[0].message.tool_calls[]` —— `[{id, type:"function", function:{name, arguments(JSON 字符串)}}]`。

`strip_think` 退役为主路径不再需要，保留作 `speak` 的防御性兜底。

多轮 Function Call 铁律（来自 MiniMax 文档）：**必须把完整 assistant 消息（含 `tool_calls`，含 reasoning）push 进历史**，再追加 `{role:"tool", tool_call_id, content}` 结果消息。

**回传形状（H5，最易让第 2 轮 400）**：先按 MiniMax 返回的 message 对象**原样回传**——`tool_calls` 在场时 `content` 置 `null`，保留 `reasoning_details`（若响应含 `reasoning_content` 也一并保留）。若第 2 轮返回 400，按报错调整字段后重试。冒烟测试必须覆盖一轮「工具调用 → 工具结果 → 最终答复」的两轮交互。

## 工具定义（`tools.rs`）

三工具的 JSON Schema（传给模型）：

- **`write`** —— 写文件
  - 参数：`{ path: string, content: string }`
  - 行为：UTF-8 写入；相对路径基于工作目录；父目录自动创建；覆盖已存在文件。
  - 返回：`已写入 {path}（{n} 字节）`

- **`read`** —— 读文件
  - 参数：`{ path: string }`
  - 行为：UTF-8 读；超过 50KB 截断并附 `…[已截断，共 {total} 字节]`；二进制或读失败返回错误字符串。
  - 返回：文件文本（或错误说明）。

- **`bash`** —— 执行命令
  - 参数：`{ command: string }`（实际执行：`cmd /C chcp 65001>nul && <command>`，工作目录 = workspace）
  - 行为：合并 stdout+stderr；**30s 超时**；输出截断 ~20KB。**编码**：`chcp 65001` 强制 UTF-8 码页，仍失败时 encoding_rs 按系统 OEM 码页兜底解码（避免中文/路径乱码）。**进程清理**：子进程入 Windows Job Object（`KILL_ON_JOB_CLOSE`），超时杀整棵进程树（`cmd /C` 的孙进程一并结束）。
  - 返回：`退出码 {code}\n{output}` 摘要。

> **工具 `description` 要写充分（L3）**：自动执行 agent 的调用质量极度依赖描述。三工具的 JSON Schema `description` 必须详尽并说明 Windows/cmd 上下文（如 bash 注明「Windows cmd、工作目录=workspace、避免破坏性命令」），引导模型正确传参与规避危险。

## 工具执行循环（Rust）

```
// 入参 messages 为前端独占历史的副本；Rust 不持久化、不回写调用方。
// history 收集本轮所有「追加进 messages」的消息，供前端同步。
// dispatch 为 async（bash 用 tokio 子进程 + tokio::time::timeout）。
// 工作目录 = cfg.workspace_dir（load 时已解析为绝对路径，空→app_data_dir/workspace）。
let mut history: Vec<Value> = vec![];
let mut last_content = String::new();
for _ in 0..12 {                              // 上限 12 轮
    let resp = match chat_once(messages, cfg).await {
        Ok(r) => r,
        Err(e) => return ChatResponse { content: String::new(), history, error: Some(e) }, // H4 中途失败也回传历史
    };
    last_content = resp.content.clone();
    emit llm-thinking(resp.reasoning);
    if !resp.tool_calls.is_empty() {
        // 1) 完整 assistant 消息(含 tool_calls + reasoning_details，content 置 null)入历史
        messages.push(resp.assistant_message);
        history.push(resp.assistant_message);
        // 2) 逐个执行（arguments 是 JSON 字符串，需解析）
        for call in resp.tool_calls {
            let name = call.function.name;
            let args_str = call.function.arguments;            // JSON 字符串
            emit llm-tool-call { name, args: args_str };
            let result = match serde_json::from_str::<Value>(&args_str) {
                Err(e) => format!("参数解析失败: {e}"),          // M1 坏 JSON 不崩，回错误给模型
                Ok(args) => dispatch(&name, args, &cfg).await,  // M5 async
            };
            emit llm-tool-result { name, result };
            let m = json!({ "role":"tool", "tool_call_id": call.id, "content": result });
            messages.push(m.clone());
            history.push(m);
        }
        continue;
    } else {
        let final_msg = json!({ "role":"assistant", "content": last_content });
        messages.push(final_msg.clone());
        history.push(final_msg);
        return ChatResponse { content: last_content, history, error: None };
    }
}
// M4 兜底：12 轮未出最终文本。content 为空时合成提示，避免空气泡。
let content = if last_content.trim().is_empty() {
    "（已达工具调用上限，未产生最终答复）".to_string()
} else {
    format!("{last_content}\n\n_（已达工具调用上限）_")
};
history.push(json!({ "role":"assistant", "content": &content })); // 兜底答复也入历史
ChatResponse { content, history, error: None }
```

**`history` 定义**：本轮 push 进 `messages` 的全部消息，依次为——每个工具轮的 `assistant(tool_calls)` + 对应的 `tool(result)` 消息，最后是 `assistant(content)` 最终答复。前端 `messages.push(...history)` 后，其历史与 Rust 内部一致。

**边界简化**：若同一响应同时含 `content` 与 `tool_calls`（M3 interleaved），按工具轮处理——执行 tool_calls，该 `content` 本轮忽略（后续可改为分段展示）。

`dispatch` 内 `match name { "write" | "read" | "bash" => … }`，未知工具名返回错误字符串（不 panic）。工具执行错误**作为 tool result 回给模型**让其自纠，不中断循环。

## 非交互护栏（全自动执行下兜底）

- 工具循环上限 **12 轮**。
- `bash` 超时 **30s**（`tokio::time::timeout`）；子进程入 **Windows Job Object**，超时杀整棵进程树（含 `cmd /C` 孙进程）。
- `bash` 输出 **UTF-8 化**（`chcp 65001` + encoding_rs OEM 兜底解码），避免中文乱码。
- `read` 输出截断 50KB、`bash` 输出截断 ~20KB。
- 工具内 panic / 任意错误兜成错误字符串，进程不崩。
- 工作目录启动时 `create_dir_all` 确保存在；`workspace_dir` 空值在 `config::load` 解析为 `app_data_dir/workspace`。

## 前端数据流

```
用户发送
  → messages.push({role:"user"})
  → 置「chat 忙」全局闸（发送按钮禁用 + 语音 voice-result 也认，防重入 M3）
  → 新建 assistant 气泡（typing）
  → invoke('chat', {messages})   // 长任务，期间持续收事件
       llm-thinking  → 追加到 .bubble-reasoning（可折叠，默认折）
       llm-tool-call → .bubble-tools 追加工具卡（名 + 参数）
       llm-tool-result → 对应卡下追加结果（可折叠）
     resolve { content, history, error }
  → messages.push(...history)    // L1 替换旧的 {role:assistant} 单条 push；含工具中间消息保上下文
  → if error: 红色 error 气泡显示 error；否则 .bubble-text 渲染 renderMarkdown(content)
  → attachSpeak（只读 .bubble-text）；解除「chat 忙」闸
```

气泡 DOM（三段式，关键：speak 只读 `.bubble-text`）：
```
.bubble.assistant
  .bubble-reasoning   ← 思考（灰字、可折叠）   speak 不读
  .bubble-tools       ← 工具卡片               speak 不读
  .bubble-text.md     ← 最终 markdown 答案     speak 读这个
```

## 配置（`config.rs` + 设置页）

新增字段：
- `workspace_dir: String`：`#[serde(default)]` 默认 `""`；**`config::load(app)` 把空值解析为 `app_data_dir/workspace`**（运行时路径需 AppHandle，静态 serde 默认无法表达）。设置页可改；保存空值即回落默认。tools.rs 直接读已解析的绝对路径，无需 AppHandle。

本轮**不**做：思考开关、工具开关（YAGNI：`reasoning_split` 常开、三工具常开，后续可加）。

## 错误处理

- API/网络/鉴权错误：构造 `LLM HTTP {status}: …` 字符串；`chat()` **不抛 Err**，改回 `ChatResponse{ content:"", history:<已积累>, error:Some(msg) }`——前端把 partial history push 进 messages（保上下文），再显示红色 error 气泡（H4）。
- 工具执行错误：作为 tool result 回模型，不中断循环。
- 循环超上限：content 为空时合成「（已达工具调用上限，未产生最终答复）」，否则追加「（已达工具调用上限）」（M4）。
- 历史过长：本轮沿用「保留全部历史」（与现状一致）；标注为后续 context-management 议题。

## 测试

- `tools.rs` 纯函数单测：相对/绝对路径解析、输出截断、`write`→`read` 往返、bash 超时与退出码、Job Object 进程树清理、坏 JSON 参数兜底。
- `llm.rs` 单测：`reasoning_split` 响应解析（mock JSON，分离 content/reasoning/tool_calls）；`chat_once` 请求体构造（含 tools/reasoning_split）。
- **循环编排单测（M2）**：把 `chat_once` 抽成可注入（trait 或函数指针），测试注入脚本化响应（轮1 tool_calls → 轮2 最终文本），断言 history 顺序、终止、12 轮上限、中途失败回传 partial history。
- 冒烟：`pnpm tauri dev`，让模型「读某文件并总结」「在 workspace 写个文件」「跑 `dir`/`echo`」验证三工具与思考展示；**并跑一条需两轮工具调用的任务**（如「读 a.txt 再把内容写进 b.txt」）验证回传形状 H5 与多轮历史。

## 受影响文件

- 新增：`src-tauri/src/tools.rs`、`docs/superpowers/specs/2026-07-24-llm-agent-tools-design.md`
- 改：`src-tauri/src/llm.rs`（`chat_once` + 循环）、`src-tauri/src/lib.rs`（`ChatResponse{content,history,error}`）、`src-tauri/src/config.rs`（`workspace_dir` + load 解析）、`src/main.js`（三事件 + history/error 处理 + chat-busy 闸）、`src/styles.css`（思考区/工具卡）、`src/index.html`（设置页加工作目录字段）
- 依赖：`Cargo.toml` 给 `windows-sys` 加 `Win32_System_JobObjects`、`Win32_System_Threading` feature（bash 进程树清理）；新增 `encoding_rs`（bash 输出兜底解码）
