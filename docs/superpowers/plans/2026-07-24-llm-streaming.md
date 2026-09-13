# ovoice 流式输出（SSE）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** LLM 回复从整段蹦出改为流式——思考（`reasoning_details`）与文本（`content`）逐字实时浮现，工具调用流式累积，与现有 12 轮工具循环无缝衔接。

**Architecture:** `HttpRound::round` 改为流式：reqwest `bytes_stream()` + 手动 SSE 解析，边收边 `emit.thinking/content` 增量、按 index 累积 `tool_calls`，返回组装好的 `RoundResult`；`run_loop` 按 `finish_reason`（`tool_calls`→执行→再开流 / `stop`→收尾）。前端流式期间 append 原始文本、流结束 `renderMarkdown` 全量美化。

**Tech Stack:** Rust + Tauri v2，reqwest（加 `stream` feature），bytes，futures-util（已依赖），async-trait；前端 Vanilla JS（无打包器）。

**Spec:** `docs/superpowers/specs/2026-07-24-llm-streaming-design.md`

## Global Constraints

- 平台 **Windows**，命令行 PowerShell；Bash 工具用 Git Bash（正斜杠）。
- 编译：`cargo build --manifest-path src-tauri/Cargo.toml`；测试：`cargo test --manifest-path src-tauri/Cargo.toml`。
- **改前端必须重新 `cargo build`**（Tauri 把 `../src` 编译进 exe，运行时不读 src）。
- MiniMax OpenAI 兼容端点 `https://api.minimaxi.com/v1/chat/completions`，model `MiniMax-M3`，Bearer 鉴权。
- 流式请求体加 `"stream": true, "stream_options": {"include_usage": true}`（其余 tools/reasoning_split 不变）。
- SSE：`delta.content`/`delta.reasoning_details[].text` 是**增量**（直接 append）；`delta.tool_calls` 按 `index` 累积；`finish_reason` ∈ {`stop`,`tool_calls`}。
- 工具循环护栏不变：12 轮上限、stdin=null、bash 30s 超时、输出截断、history/error 模型。
- 失败兜底：流中途断→已累积 content 当答案 + error 标注；首连失败→error（走现有 H4）。
- `usage` 仅记入 `RoundResult`（日志/后续），**不进 `ChatResponse`**（YAGNI）。
- API key 仅 Rust 侧；`.env` gitignored。

## 文件结构 / 任务依赖

```
Task1 Emitter.content + AppEmitter.content + Cargo(bytes,reqwest stream) + RecEmitter   独立（管道）
Task2 纯函数：parse_data_line + ToolCallAccum                                           独立
Task3 consume_sse + RoundResult/FinishReason 类型（fake stream 测）                      依赖 Task1,Task2
Task4 原子切换：LlmRound 新签名 + HttpRound 流式 + run_loop finish 分支 + build_body + 删 chat_once/parse_resp + 更新测试   依赖 Task3
Task5 前端：llm-content 监听 + append + 流后 renderMarkdown                              依赖 Task4
Task6 冒烟验证                                                                          依赖 全部
```

## 关键接口契约（跨任务共享，名字/类型必须一致）

```rust
// Task1 加到 Emitter
async fn content(&self, text: &str);

// Task2 纯函数
fn parse_data_line(line: &str) -> Option<&str>;
struct ToolCallAccum { ... }  // push(index,id?,name?,args?); finalize()->Vec<Value>

// Task3 类型 + 流式核心
enum FinishReason { Stop, ToolCalls }
struct RoundResult { content, reasoning, tool_calls:Vec<Value>, finish:FinishReason, usage:Option<Value>, assistant_message:Value }
async fn consume_sse<S: Stream<Item=Bytes> + Unpin>(stream:S, emit:&dyn Emitter) -> Result<RoundResult,String>;

// Task4 签名切换
#[async_trait] trait LlmRound { async fn round(&self, messages:&[Value], cfg:&Config, emit:&dyn Emitter) -> Result<RoundResult,String>; }
```

---

### Task 1: Emitter.content 管道 + Cargo 依赖

**Files:**
- Modify: `src-tauri/src/llm.rs`（`Emitter` trait 加 `content`；测试 `RecEmitter` 加 `content` 实现）
- Modify: `src-tauri/src/lib.rs`（`AppEmitter` 加 `content` → emit `llm-content`）
- Modify: `src-tauri/Cargo.toml`（reqwest 加 `stream` feature；新增 `bytes`）

**Interfaces:**
- Produces: `Emitter::content(&self, text:&str)`（async）；`AppEmitter.content` emit 事件 `llm-content`（payload=string）。

- [ ] **Step 1: Cargo 依赖**

`src-tauri/Cargo.toml`：reqwest 行加 `stream` feature，新增 bytes：
```toml
reqwest = { version = "0.12", features = ["json", "stream"] }
bytes = "1"
```

- [ ] **Step 2: Emitter trait 加 content（llm.rs）**

在 `Emitter` trait 里 `thinking` 之后加：
```rust
    async fn content(&self, text: &str);
```

- [ ] **Step 3: AppEmitter 加 content（lib.rs）**

`impl llm::Emitter for AppEmitter` 里加：
```rust
    async fn content(&self, text: &str) {
        let _ = self.app.emit("llm-content", text);
    }
```

- [ ] **Step 4: RecEmitter 加 content（llm.rs 测试 mod）**

测试 mod 顶部 `RecEmitter` 结构加字段：
```rust
struct RecEmitter { thinking: Mutex<Vec<String>>, contents: Mutex<Vec<String>>, calls: Mutex<Vec<(String,String,String)>> }
```
impl 里加：
```rust
    async fn content(&self, t: &str) { self.contents.lock().unwrap().push(t.into()); }
```
（`#[derive(Default)]` 仍可用。）

- [ ] **Step 5: 编译 + 全量测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: 全部 PASS（新 trait 方法已由 AppEmitter/RecEmitter 实现，无破坏）。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/llm.rs src-tauri/src/lib.rs
git commit -m "feat(llm): Emitter.content 管道 + reqwest stream/bytes 依赖"
```

---

### Task 2: 纯函数 SSE 行解析 + ToolCall 累积器

**Files:**
- Modify: `src-tauri/src/llm.rs`

**Interfaces:**
- Produces: `parse_data_line(line:&str)->Option<&str>`；`ToolCallAccum { push(...); finalize()->Vec<Value> }`。

- [ ] **Step 1: 写失败测试**（追加到 `llm.rs` 的 `tests` mod）

```rust
    #[test]
    fn parse_data_line_handles_spaces() {
        assert_eq!(parse_data_line("data: {\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(parse_data_line("data:{\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(parse_data_line(": ping"), None);
        assert_eq!(parse_data_line("event: x"), None);
    }

    #[test]
    fn toolcall_accum_single_chunk() {
        let mut a = ToolCallAccum::new();
        a.push(0, Some("c1".into()), Some("bash".into()), Some("{\"command\":\"echo\"}".into()));
        let v = a.finalize();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["id"], "c1");
        assert_eq!(v[0]["function"]["name"], "bash");
        assert_eq!(v[0]["function"]["arguments"], "{\"command\":\"echo\"}");
    }

    #[test]
    fn toolcall_accum_multi_chunk_args() {
        let mut a = ToolCallAccum::new();
        a.push(0, Some("c1".into()), Some("bash".into()), Some("{\"comm".into()));
        a.push(0, None, None, Some("and\":\"hi\"}".into()));
        let v = a.finalize();
        assert_eq!(v[0]["function"]["arguments"], "{\"command\":\"hi\"}");
    }

    #[test]
    fn toolcall_accum_multiple_indexed() {
        let mut a = ToolCallAccum::new();
        a.push(1, Some("c2".into()), Some("read".into()), Some("{}".into()));
        a.push(0, Some("c1".into()), Some("bash".into()), Some("{}".into()));
        let v = a.finalize();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0]["id"], "c1"); // index 升序
        assert_eq!(v[1]["id"], "c2");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml llm::tests`
Expected: 编译失败（`parse_data_line`/`ToolCallAccum` 未定义）。

- [ ] **Step 3: 实现**（`llm.rs`，放在 `truncate` 附近）

```rust
/// 从一行 SSE 文本中取出 `data:` 后的 payload（兼容 `data:`与`data: `）。非 data 行返回 None。
fn parse_data_line(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("data:")?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

/// 按 index 累积流式 tool_call（id/name 首块带、arguments 多块拼）。
struct ToolCallAccum {
    map: std::collections::BTreeMap<usize, (Option<String>, Option<String>, String)>,
}
impl ToolCallAccum {
    fn new() -> Self { Self { map: Default::default() } }
    fn push(&mut self, index: usize, id: Option<String>, name: Option<String>, args: Option<String>) {
        let e = self.map.entry(index).or_insert((None, None, String::new()));
        if let Some(i) = id { e.0 = Some(i); }
        if let Some(n) = name { e.1 = Some(n); }
        if let Some(a) = args { e.2.push_str(&a); }
    }
    fn finalize(self) -> Vec<Value> {
        self.map.into_iter().map(|(_, (id, name, args))| json!({
            "id": id.unwrap_or_default(),
            "type": "function",
            "function": { "name": name.unwrap_or_default(), "arguments": args }
        })).collect()
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml llm::tests`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/llm.rs
git commit -m "feat(llm): SSE 行解析 + ToolCall 按 index 累积器（纯函数）"
```

---

### Task 3: consume_sse + RoundResult/FinishReason 类型

**Files:**
- Modify: `src-tauri/src/llm.rs`

**Interfaces:**
- Consumes: `parse_data_line`、`ToolCallAccum`（Task 2）、`Emitter::content`（Task 1）、`strip_thinking_blocks`（已存在）。
- Produces: `FinishReason`、`RoundResult`、`async fn consume_sse<S>(stream, emit)->Result<RoundResult,String>`。**本任务不触碰 `LlmRound`/`HttpRound`/`run_loop`**——新类型与函数与旧代码并存，app 仍非流式可跑。

- [ ] **Step 1: 写失败测试**（追加到 `llm.rs` 的 `tests` mod）

```rust
    fn sse_line(delta: Value, finish: Option<&str>) -> String {
        let mut o = json!({ "choices": [{ "delta": delta }] });
        if let Some(f) = finish {
            o["choices"][0]["finish_reason"] = json!(f);
            o["choices"][0].as_object_mut().unwrap().remove("delta");
        }
        format!("data: {}\n", o)
    }

    #[tokio::test]
    async fn consume_sse_content_and_reasoning() {
        let sse = format!(
            "{}{}{}{}{}",
            sse_line(json!({"reasoning_details":[{"text":"想"}]}), None),
            sse_line(json!({"content":"你好"}), None),
            sse_line(json!({"content":"世界"}), None),
            sse_line(json!({}), Some("stop")),
            "data: [DONE]\n",
        );
        let stream = futures_util::stream::iter(vec![bytes::Bytes::from(sse)]);
        let rec = RecEmitter::default();
        let r = consume_sse(stream, &rec).await.unwrap();
        assert_eq!(r.content, "你好世界");
        assert_eq!(r.finish, FinishReason::Stop);
        assert!(r.tool_calls.is_empty());
        assert_eq!(rec.contents.lock().unwrap().join(""), "你好世界");
        assert!(rec.thinking.lock().unwrap().join("").contains("想"));
    }

    #[tokio::test]
    async fn consume_sse_tool_calls_by_index() {
        let sse = format!(
            "{}{}{}{}",
            sse_line(json!({"tool_calls":[{"index":0,"id":"c1","function":{"name":"bash","arguments":"{\"comm"}}]}), None),
            sse_line(json!({"tool_calls":[{"index":0,"function":{"arguments":"and\":\"echo\"}"}}]}), None),
            sse_line(json!({}), Some("tool_calls")),
            "data: [DONE]\n",
        );
        let stream = futures_util::stream::iter(vec![bytes::Bytes::from(sse)]);
        let r = consume_sse(stream, &RecEmitter::default()).await.unwrap();
        assert_eq!(r.finish, FinishReason::ToolCalls);
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0]["function"]["arguments"], "{\"command\":\"echo\"}");
        assert_eq!(r.assistant_message["content"], Value::Null); // tool_calls 在场 → content null
    }

    #[tokio::test]
    async fn consume_sse_skips_bad_lines() {
        let sse = ": ping\n\nevent: x\n\ndata: not-json\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\ndata: {\"choices\":[{\"finish_reason\":\"stop\"}]}\ndata: [DONE]\n";
        let stream = futures_util::stream::iter(vec![bytes::Bytes::from(sse)]);
        let r = consume_sse(stream, &RecEmitter::default()).await.unwrap();
        assert_eq!(r.content, "ok");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml llm::tests`
Expected: 编译失败（`FinishReason`/`RoundResult`/`consume_sse` 未定义）。

- [ ] **Step 3: 实现**（`llm.rs`，`ToolCallAccum` 之后）

```rust
use bytes::Bytes;
use futures_util::StreamExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason { Stop, ToolCalls }

#[derive(Debug, Clone)]
pub struct RoundResult {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<Value>,
    pub finish: FinishReason,
    pub usage: Option<Value>,
    pub assistant_message: Value,
}

/// 消费 SSE 字节流：边 emit thinking/content 增量，累积 tool_calls，结束组装 RoundResult。
pub async fn consume_sse<S>(mut stream: S, emit: &dyn Emitter) -> Result<RoundResult, String>
where
    S: futures_util::Stream<Item = Bytes> + Unpin,
{
    let mut buf = String::new();
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut accum = ToolCallAccum::new();
    let mut finish = FinishReason::Stop;
    let mut usage: Option<Value> = None;

    while let Some(chunk) = stream.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(nl) = buf.find('\n') {
            let line = buf[..nl].trim_end_matches('\r').to_string();
            buf = buf[nl + 1..].to_string();
            let Some(payload) = parse_data_line(&line) else { continue };
            if payload == "[DONE]" { continue; }
            let Ok(o) = serde_json::from_str::<Value>(payload) else { continue };
            if o.get("usage").is_some() { usage = Some(o["usage"].clone()); }
            let Some(ch) = o.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first()) else { continue };
            if let Some(fr) = ch.get("finish_reason").and_then(|v| v.as_str()) {
                finish = match fr { "tool_calls" => FinishReason::ToolCalls, _ => FinishReason::Stop };
            }
            if let Some(d) = ch.get("delta") {
                if let Some(c) = d.get("content").and_then(|v| v.as_str()) {
                    if !c.is_empty() { content.push_str(c); emit.content(c).await; }
                }
                if let Some(arr) = d.get("reasoning_details").and_then(|v| v.as_array()) {
                    for x in arr {
                        if let Some(t) = x.get("text").and_then(|v| v.as_str()) {
                            if !t.is_empty() { reasoning.push_str(t); emit.thinking(t).await; }
                        }
                    }
                }
                if let Some(tcs) = d.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        let id = tc.get("id").and_then(|v| v.as_str()).map(String::from);
                        let name = tc.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()).map(String::from);
                        let args = tc.get("function").and_then(|f| f.get("arguments")).and_then(|v| v.as_str()).map(String::from);
                        accum.push(idx, id, name, args);
                    }
                }
            }
        }
    }

    let tool_calls = accum.finalize();
    let (content_clean, extra) = strip_thinking_blocks(&content);
    let reasoning_full = if extra.is_empty() { reasoning } else { format!("{reasoning}\n{extra}") };
    let mut echo = json!({ "role": "assistant" });
    if tool_calls.is_empty() {
        echo["content"] = json!(content_clean.clone());
    } else {
        echo["content"] = Value::Null;
        echo["tool_calls"] = json!(tool_calls);
    }
    Ok(RoundResult { content: content_clean, reasoning: reasoning_full, tool_calls, finish, usage, assistant_message: echo })
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml llm::tests`
Expected: PASS（含三个 consume_sse 测试）。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/llm.rs
git commit -m "feat(llm): consume_sse 流式核心 + RoundResult/FinishReason（fake stream 单测）"
```

---

### Task 4: 原子切换到流式（LlmRound 新签名 + HttpRound 流式 + run_loop finish 分支）

**Files:**
- Modify: `src-tauri/src/llm.rs`（`LlmRound` 新签名、`HttpRound` 流式、`run_loop` 重写、`build_body` 加 stream、删除 `chat_once`/`parse_resp`/`ParsedResp`、更新所有测试）

**Interfaces:**
- Consumes: `consume_sse`/`RoundResult`/`FinishReason`（Task 3）、`Emitter::content`（Task 1）。
- Produces: 流式 `LlmRound::round(messages,cfg,emit)->Result<RoundResult,String>`；`HttpRound` 流式实现；`run_loop` 按 `finish` 分支。

> 这是原子改动：签名一变，`HttpRound`/`run_loop`/`FakeRound`/`tool_round`/loop 测试必须同步改，否则不编译。下面给出全部代码。

- [ ] **Step 1: 改 build_body 加流式参数**

`llm.rs` 的 `build_body`：
```rust
pub fn build_body(messages: &[Value], cfg: &Config) -> Value {
    json!({
        "model": cfg.llm_model,
        "messages": messages,
        "tools": tools::schemas(),
        "tool_choice": "auto",
        "reasoning_split": true,
        "stream": true,
        "stream_options": { "include_usage": true },
    })
}
```
并把 `build_body_has_tools_and_reasoning_split` 测试加断言 `assert_eq!(body["stream"], true);`。

- [ ] **Step 2: 删除旧的非流式类型与函数**

删除 `ParsedResp` 结构、`parse_resp` 函数、`chat_once` 函数（流式后不再用）。保留 `strip_think`/`strip_thinking_blocks`/`truncate`/`api_key`/`extract_delimited`/`strip_inline_tags`/`parse_data_line`/`ToolCallAccum`/`consume_sse`/`RoundResult`/`FinishReason`。

删除依赖 `parse_resp`/`ParsedResp` 的旧测试：`parse_resp_splits_content_and_reasoning`、`parse_resp_tool_calls_nulls_content_in_echo`（其断言已被 consume_sse 测试覆盖）。

- [ ] **Step 3: 改 LlmRound trait + HttpRound 流式实现**

```rust
#[async_trait]
pub trait LlmRound: Send + Sync {
    async fn round(&self, messages: &[Value], cfg: &Config, emit: &dyn Emitter) -> Result<RoundResult, String>;
}

pub struct HttpRound;
#[async_trait]
impl LlmRound for HttpRound {
    async fn round(&self, messages: &[Value], cfg: &Config, emit: &dyn Emitter) -> Result<RoundResult, String> {
        let key = api_key(cfg)?;
        let client = reqwest::Client::new();
        let body = build_body(messages, cfg);
        let resp = client
            .post(format!("{}/chat/completions", API_BASE))
            .bearer_auth(&key)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("LLM 请求失败: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("LLM HTTP {status}: {}", truncate(&text, 600)));
        }
        let stream = resp.bytes_stream().map(|r| r.unwrap_or_default());
        consume_sse(stream, emit).await
    }
}
```
（`resp.bytes_stream()` 来自 reqwest `stream` feature；`.map(|r| r.unwrap_or_default())` 把 `Result<Bytes,Error>` 映射成 `Bytes`，单块失败丢弃不中断。）

- [ ] **Step 4: 重写 run_loop 按 finish 分支**

```rust
pub async fn run_loop<R: LlmRound, E: Emitter>(
    round: R,
    emitter: E,
    cfg: &Config,
    mut messages: Vec<Value>,
    workspace: &Path,
) -> ChatResponse {
    let mut history: Vec<Value> = vec![];
    let mut last_content = String::new();
    for i in 0..MAX_ITERS {
        eprintln!("[loop] round {i}: 调用模型（流式）…");
        let resp = match round.round(&messages, cfg, &emitter).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[loop] round {i}: 失败 {e}");
                return ChatResponse { content: String::new(), history, error: Some(e) };
            }
        };
        last_content = resp.content.clone();
        eprintln!("[loop] round {i}: finish={:?} tool_calls={}", resp.finish, resp.tool_calls.len());
        match resp.finish {
            FinishReason::ToolCalls => {
                messages.push(resp.assistant_message.clone());
                history.push(resp.assistant_message);
                for call in resp.tool_calls {
                    let name = call["function"]["name"].as_str().unwrap_or("").to_string();
                    let args_str = call["function"]["arguments"].as_str().unwrap_or("{}").to_string();
                    emitter.tool_call(&name, &args_str).await;
                    let result = match serde_json::from_str::<Value>(&args_str) {
                        Err(e) => format!("参数解析失败: {e}"),
                        Ok(args) => tools::dispatch(&name, args, workspace).await,
                    };
                    emitter.tool_result(&name, &result).await;
                    let m = json!({ "role": "tool", "tool_call_id": call["id"], "content": result });
                    messages.push(m.clone());
                    history.push(m);
                }
                continue;
            }
            FinishReason::Stop => {
                history.push(resp.assistant_message.clone());
                return ChatResponse { content: last_content, history, error: None };
            }
        }
    }
    // 12 轮兜底（不变）
    let content = if last_content.trim().is_empty() {
        "（已达工具调用上限，未产生最终答复）".to_string()
    } else {
        format!("{last_content}\n\n_（已达工具调用上限）_")
    };
    history.push(json!({ "role": "assistant", "content": &content }));
    ChatResponse { content, history, error: None }
}
```

- [ ] **Step 5: 更新测试（FakeRound / tool_round / loop_* 适配新签名）**

测试 mod 里 `FakeRound` 改为：
```rust
    struct FakeRound(Mutex<VecDeque<Result<RoundResult, String>>>);
    #[async_trait]
    impl LlmRound for FakeRound {
        async fn round(&self, _m: &[Value], _cfg: &Config, _emit: &dyn Emitter) -> Result<RoundResult, String> {
            self.0.lock().unwrap().pop_front().unwrap_or_else(|| Ok(RoundResult {
                content: "默认最终".into(), reasoning: String::new(), tool_calls: vec![],
                finish: FinishReason::Stop, usage: None,
                assistant_message: json!({"role":"assistant","content":"默认最终"}),
            }))
        }
    }
```
`tool_round` 改为返回 `RoundResult`：
```rust
    fn tool_round(name:&str, args:&str, id:&str) -> RoundResult {
        let tc = json!({"id":id,"type":"function","function":{"name":name,"arguments":args}});
        RoundResult {
            content: String::new(), reasoning: "想一下".into(),
            tool_calls: vec![tc.clone()],
            finish: FinishReason::ToolCalls, usage: None,
            assistant_message: json!({"role":"assistant","content":null,"tool_calls":[tc]}),
        }
    }
```
`loop_one_tool_then_final` 最后一轮用：
```rust
        Ok(RoundResult { content:"完成".into(), reasoning:String::new(), tool_calls:vec![],
            finish:FinishReason::Stop, usage:None, assistant_message:json!({"role":"assistant","content":"完成"}) }),
```
其余两个 loop 测试（mid_fail、cap）的 `Ok(...)` 包装改 `RoundResult`（同上结构），断言不变（mid_fail 仍断 `history.len()==2`、error；cap 仍断 content 含「工具调用上限」、`history.len()==25`）。注意 `tool_round` 的 `reasoning` 现在不会被 run_loop 重复 emit（流式由 round 内部 emit；FakeRound 不 emit，测试只验 history/finish，不验 thinking 事件，符合预期）。

- [ ] **Step 6: 编译 + 全量测试**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: 全部 PASS（无 `ParsedResp`/`parse_resp`/`chat_once` 残留引用，无 dead code 警告）。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/llm.rs
git commit -m "feat(llm): 原子切换流式（LlmRound 新签名 + HttpRound 流式 + run_loop finish 分支）"
```

---

### Task 5: 前端 llm-content 监听 + append + 流后渲染

**Files:**
- Modify: `src/main.js`

**Interfaces:**
- Consumes: 事件 `llm-content`（payload=string，Task 1 AppEmitter 已 emit）、`chat` resolve 的 `content`（全量，Task 4）。

- [ ] **Step 1: 加 appendContent helper + 注册 llm-content 监听**

`main.js` 的 `appendReasoning` 附近加：
```js
function appendContent(text) {
  if (!activeAssistantWrap || !text) return;
  const t = activeAssistantWrap.querySelector(".bubble-text");
  if (!t) return;
  if (t.querySelector(".typing")) t.textContent = ""; // 首次内容清掉打字指示器
  t.classList.remove("md"); // 流式期间用原始文本（pre-wrap）
  t.textContent += text;
  scrollBottom();
}
```
`setupAgentEvents` 里加（与现有三个 listen 并列）：
```js
  await listen("llm-content", (e) => appendContent(typeof e.payload === "string" ? e.payload : (e.payload && e.payload.text) || ""));
```

> 流式期间 `.bubble-text` 累积原始文本；`chat` resolve 后现有逻辑 `aiText.innerHTML = renderMarkdown(res.content)` 会用全量 markdown 替换原始文本（无需改）。

- [ ] **Step 2: 重新 build（前端改动必须 build）+ 提交**

Run: `cargo build --manifest-path src-tauri/Cargo.toml`
Expected: 编译成功（前端嵌入）。

```bash
git add src/main.js
git commit -m "feat(ui): llm-content 流式增量监听 + 流后 renderMarkdown"
```

---

### Task 6: 冒烟验证

**Files:** 无（仅运行验证）

- [ ] **Step 1: 全量测试 + 编译**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`
Expected: 全部 PASS。
Run: `cargo build --manifest-path src-tauri/Cargo.toml`
Expected: 编译成功、零警告。

- [ ] **Step 2: 启动应用**（先 kill 旧实例）

Run（PowerShell，由用户/控制器执行）:
```bash
taskkill //F //IM ovoice.exe 2>/dev/null; ./src-tauri/target/debug/ovoice.exe
```
Expected: 窗口启动，顶栏「MiniMax-M3 · …」，无 panic。

- [ ] **Step 3: 验证思考 + 文本逐字流**

发「用一句话介绍你自己」→ 期望：思考区（折叠）**逐字浮现**（不再干等数秒），最终答案在气泡里**逐字浮现**，结束后转 markdown 美化。

- [ ] **Step 4: 验证工具 + 流式不破**

发「用 bash 跑 echo hi」→ 期望：思考逐字、bash 工具卡出现并执行、结果回填，最终答复逐字流。再发「读 a.txt 再把内容写进 b.txt」→ 两轮工具调用顺利完成（`finish_reason:tool_calls`→执行→再开流→`stop`）。

- [ ] **Step 5: 验证护栏 + 失败兜底**

让它「反复读写 20 次」→ 12 轮后「（已达工具调用上限）」。临时断网发一条 → 期望 error 气泡（首连失败走 H4）。

- [ ] **Step 6: 最终提交（若有验证中发现的小修）**

```bash
git add -A && git commit -m "chore: 流式冒烟验证通过"
```

---

## Self-Review

**1. Spec 覆盖：**
- 流式调用（reqwest bytes_stream + 手动 SSE）→ Task 3 consume_sse + Task 4 HttpRound ✓
- 思考逐字流（reasoning_details 增量）→ Task 3 emit.thinking（run_loop 不再重复 emit）✓
- 文本逐字流（content 增量）→ Task 1 Emitter.content + Task 3 emit.content + Task 5 appendContent ✓
- 工具正交（tool_calls 按 index 累积，finish=tool_calls 触发执行+再开流）→ Task 2 ToolCallAccum + Task 3 consume_sse + Task 4 run_loop finish 分支 ✓
- 渐进渲染（流式 append 原始文本，结束 renderMarkdown）→ Task 5 ✓
- 护栏不变（12 轮/stdin=null/超时/截断/history/error）→ Task 4 run_loop 沿用 ✓
- 失败兜底（中途断→已累积；首连失败→H4）→ Task 3 accumulate + Task 4 Err 分支 ✓
- usage 不进 ChatResponse（仅 RoundResult）→ Task 3 RoundResult.usage、ChatResponse 未改 ✓
- 删除 thinking:adaptive（已在 agent 阶段做）、strip_thinking_blocks 仍作用 → Task 3 ✓

**2. 占位符扫描：** 无 TBD/TODO；每步含真实代码与命令。

**3. 类型一致性：** `RoundResult`/`FinishReason`/`consume_sse`/`LlmRound::round`/`Emitter::content`/`parse_data_line`/`ToolCallAccum` 跨任务签名一致；`RecEmitter.contents` 字段 Task1 加、Task3 测用，名字统一。Task4 删除 `ParsedResp`/`parse_resp`/`chat_once` 并同步清理其测试，无悬空引用。
