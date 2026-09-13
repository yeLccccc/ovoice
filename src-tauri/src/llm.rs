// MiniMax LLM —— OpenAI 兼容的 chat completions。
use crate::config::Config;
use crate::tools;
use crate::AttachmentRef;
use async_trait::async_trait;
use serde_json::{json, Value};
use bytes::Bytes;
use futures_util::StreamExt;
use base64::Engine;
use std::sync::Arc;

const API_BASE: &str = "https://api.minimaxi.com/v1";

/// 取 API Key：优先配置里的值，回落到环境变量。
pub fn api_key(cfg: &Config) -> Result<String, String> {
    if !cfg.api_key.trim().is_empty() {
        return Ok(cfg.api_key.trim().to_string());
    }
    std::env::var("MINIMAX_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .ok_or_else(|| "MINIMAX_API_KEY 未设置（请在「设置」里填写，或写入 src-tauri/.env）".into())
}

/// 构造请求体（含 tools + reasoning_split + stream；省略 thinking 即默认开启）。
pub fn build_body(messages: &[Value], cfg: &Config) -> Result<Value, String> {
    let expanded = expand_messages_for_send(messages)?;
    let tools = cfg.active_tools.clone().unwrap_or_else(|| tools::schemas());
    Ok(json!({
        "model": cfg.llm_model,
        "messages": expanded,
        "tools": tools,
        "tool_choice": "auto",
        "reasoning_split": true,
        "stream": true,
        "stream_options": { "include_usage": true },
    }))
}

/// 文本类 kind：stage 时已内联，直接作为 text part（不再存 ref）。
fn is_inline_text_kind(kind: &str) -> bool {
    matches!(kind, "text" | "markdown" | "csv")
}

/// 抽取出的文档文本上限：超长在字符边界截断并附标记。
/// M3 有 1M token 上下文本可吃更多，但该文本随消息每轮重发，设上限避免历史膨胀。
pub(crate) const DOC_TEXT_MAX: usize = 200 * 1024;

/// 超 `DOC_TEXT_MAX` 则在字符边界截断并附 `[…已截断…]` 标记；否则原样返回。
pub(crate) fn cap_doc_text(t: &str) -> String {
    if t.len() <= DOC_TEXT_MAX {
        return t.to_string();
    }
    // 落在 ≤ 上限的最大字符边界（不切断多字节字符）
    let cut = t
        .char_indices()
        .take_while(|(i, _)| *i <= DOC_TEXT_MAX)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or_else(|| DOC_TEXT_MAX.min(t.len()));
    let head = &t[..cut];
    let total = t.chars().count();
    let kept = head.chars().count();
    format!("{head}\n\n[…已截断，原文约 {total} 字，仅取前 {kept} 字…]")
}

/// 组 user 消息：历史里存引用（image_ref/video_ref）或内联文本 part（文本类）/ agent note（二进制）。
/// **不内联 base64** —— build_body 发送时才展开（省内存、控每轮成本）。
pub fn user_message_with_attachments(text: &str, attachments: &[AttachmentRef]) -> Value {
    let mut parts: Vec<Value> = Vec::new();
    if !text.is_empty() {
        parts.push(json!({ "type": "text", "text": text }));
    }
    for a in attachments {
        match a.kind.as_str() {
            "image" => parts.push(json!({ "type": "image_ref", "path": a.staged_path })),
            "video" => parts.push(json!({ "type": "video_ref", "path": a.staged_path })),
            // PDF/DOCX：M3 无原生文档输入，抽取文本作 text part 内联（截断+标记）。
            // 抽取失败（损坏/加密/无文本层）回落到指针 note，由 agent 用 read/display 兜底。
            "pdf" | "docx" => {
                let k = a.kind.as_str();
                match crate::extract::doc_text(std::path::Path::new(&a.staged_path), k) {
                    Some(t) if !t.trim().is_empty() => {
                        parts.push(json!({ "type": "text", "text": format!("[用户上传了 {} ({})]\n{}", a.staged_path, k, cap_doc_text(&t)) }));
                    }
                    _ => parts.push(json!({ "type": "text", "text": format!("[用户上传了 {} ({})，无法抽取文本，可用 read/display 处理]", a.staged_path, k) })),
                }
            }
            k if is_inline_text_kind(k) => {
                // 文本内容已由 stage 提取；这里读 staged 副本（≤50KB）
                let body = std::fs::read_to_string(&a.staged_path).unwrap_or_default();
                parts.push(json!({ "type": "text", "text": format!("[用户上传了 {}]\n{}", a.staged_path, body) }));
            }
            _ => parts.push(json!({ "type": "text", "text": format!("[用户上传了 {}，可用 read/display 处理]", a.staged_path) })),
        }
    }
    if parts.is_empty() {
        json!({ "role": "user", "content": text })
    } else if parts.len() == 1 && parts[0]["type"] == "text" && text == parts[0]["text"].as_str().unwrap_or("\0") {
        json!({ "role": "user", "content": text })
    } else {
        json!({ "role": "user", "content": parts })
    }
}

const REQUEST_BUDGET_BYTES: u64 = 58 * 1024 * 1024; // 留余量 < 64MB

/// 发送前展开：image_ref/video_ref → 现场读盘 base64 成 image_url/video_url part；
/// text part 原样。累加 base64 体积，超 REQUEST_BUDGET_BYTES 返 Err（build_body → chat-error）。
pub fn expand_messages_for_send(messages: &[Value]) -> Result<Vec<Value>, String> {
    let mut total: u64 = 0;
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    for m in messages {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let content = m.get("content");
        // 仅展开 content 为数组、且含 image_ref/video_ref 的消息；其余原样克隆
        let needs_expand = role == "user"
            && content.and_then(|c| c.as_array()).map(|arr| arr.iter().any(|p|
                p.get("type").and_then(|t| t.as_str()).map(|s| s == "image_ref" || s == "video_ref").unwrap_or(false)
            )).unwrap_or(false);
        if !needs_expand {
            out.push(m.clone());
            continue;
        }
        let arr = content.unwrap().as_array().unwrap();
        let mut new_parts: Vec<Value> = Vec::with_capacity(arr.len());
        for p in arr {
            let t = p.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if t == "image_ref" || t == "video_ref" {
                let path = std::path::PathBuf::from(p["path"].as_str().unwrap_or(""));
                let bytes = std::fs::read(&path).map_err(|e| format!("附件文件已丢失，请重新添加: {}: {e}", path.display()))?;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                total = total.saturating_add(b64.len() as u64);
                if total > REQUEST_BUDGET_BYTES {
                    return Err("历史媒体过大（超 58MB 上限），请开新会话或移除旧图/视频".into());
                }
                let mime = crate::mime_from_ext(&path);
                let url = format!("data:{mime};base64,{b64}");
                let key = if t == "image_ref" { "image_url" } else { "video_url" };
                new_parts.push(json!({ "type": key, key: { "url": url } }));
            } else {
                new_parts.push(p.clone());
            }
        }
        let mut nm = m.clone();
        nm["content"] = Value::Array(new_parts);
        out.push(nm);
    }
    Ok(out)
}

/// 可注入的单轮调用抽象（Task 7 循环测试用 FakeRound）。
#[async_trait]
pub trait LlmRound: Send + Sync {
    async fn round(&self, messages: &[Value], cfg: &Config, emit: &dyn Emitter) -> Result<RoundResult, String>;
}

pub struct HttpRound;

/// 单次 chat completion 的最大尝试次数（1 首次 + 净化阶梯 4 级 + 瞬时错误退避 1）。
const MAX_HTTP_ATTEMPTS: usize = 6;

/// 净化重试全拦后的最终错误消息（四级净化都过不了才报给用户）。
const SUGGEST_OPEN_NEW_CHAT_OR_REPHRASE: &str =
    "输入内容被服务商风控拦截（敏感内容判定）：已自动省略全部工具输出并回退上下文重试仍被拦。\
     通常是上下文累积了触发敏感词的文本（如某些安全/网络主题的搜索结果）。请换措辞重新描述任务，或重置会话";

/// 内容风控净化的 tool 结果省略桩：告诉 agent 结果被省略 + 为什么 + 能做什么。
/// agent 看到桩可换措辞重新调工具（新结果进真身 history，不受净化影响）。
const TOOL_RESULT_STUB: &str =
    "[此工具输出因触发服务商内容风控被省略。若仍需要该信息，请换一种措辞重新调用工具获取。]";

/// 净化 level 3：把 assistant 的 tool_call arguments 也换成桩——搜索词本身
/// （如「公网暴露/鉴权密码」）也是敏感源，光省略结果不够。
const TOOL_CALL_ARGS_STUB: &str = "{\"note\":\"原参数因内容风控被省略\"}";

// ─── 内容风控隔离名单：污染源记一次，后续发送直接跳过（不再每轮撞 422）───
//
// 净化阶梯救回一个回合后，把「被换桩的原文 hash」记进 cache 下的 quarantine.json。
// 之后每次构造请求时，凡 hash 命中名单的 tool 结果/参数直接替换成桩——零 422、零重试。
// 真身 history 一条不动（保留原文，只是发送时跳过加载）。

/// 隔离名单：污染文本 hash 集合（OnceCell 全局，读写走 cache/quarantine.json）。
static QUARANTINE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<u64>>> =
    std::sync::OnceLock::new();

fn quarantine() -> &'static std::sync::Mutex<std::collections::HashSet<u64>> {
    QUARANTINE.get_or_init(|| std::sync::Mutex::new(Default::default()))
}

/// 简易字符串 hash（FNV-1a，u64）：只用于本地去重比对，非密码学用途。
fn text_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 从 cache/quarantine.json 恢复名单到内存（幂等：合并去重）。启动后首次请求时调用。
fn quarantine_load(cache: &std::path::Path) {
    let Ok(text) = std::fs::read_to_string(cache.join("quarantine.json")) else { return };
    let Ok(arr) = serde_json::from_str::<Vec<u64>>(&text) else { return };
    let mut g = quarantine().lock().unwrap();
    let before = g.len();
    g.extend(arr);
    if g.len() != before {
        let _ = std::fs::write(cache.join("quarantine.json"),
            serde_json::to_string(&g.iter().copied().collect::<Vec<u64>>()).unwrap_or_default());
    }
}

/// 把一批 hash 写入名单（内存 + 落盘）。
fn quarantine_add(hashes: &[u64], cache: &std::path::Path) {
    if hashes.is_empty() { return; }
    let mut g = quarantine().lock().unwrap();
    let before = g.len();
    g.extend(hashes.iter().copied());
    if g.len() != before {
        let _ = std::fs::create_dir_all(cache);
        let _ = std::fs::write(cache.join("quarantine.json"),
            serde_json::to_string(&g.iter().copied().collect::<Vec<u64>>()).unwrap_or_default());
    }
}

/// 收集指定净化级别会换桩的原文 hash（净化成功后记入隔离名单用）。
/// 与 stub_tool_results 的替换范围保持一致：level 1 取较新一半、≥2 全部 tool 结果；
/// ≥3 含 tool_call arguments。level 4 的「截断剥掉」不记（剥的是消息非污染定位）。
fn collect_stubbed_hashes(messages: &[Value], level: usize) -> Vec<u64> {
    if level == 0 { return vec![]; }
    let tool_contents: Vec<&str> = messages.iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["content"].as_str())
        .collect();
    let stub_from = if level >= 2 { 0 } else { tool_contents.len() / 2 };
    let mut hashes: Vec<u64> = tool_contents[stub_from.min(tool_contents.len())..]
        .iter().map(|s| text_hash(s)).collect();
    if level >= 3 {
        for m in messages.iter() {
            if m["role"] != "assistant" { continue; }
            if let Some(tcs) = m.get("tool_calls").and_then(|t| t.as_array()) {
                for c in tcs {
                    if let Some(args) = c["function"]["arguments"].as_str() {
                        hashes.push(text_hash(args));
                    }
                }
            }
        }
    }
    hashes
}

/// 发送前应用隔离名单：命中 hash 的 tool content / tool_call arguments 直接换桩。
/// 返回（发送副本, 被隔离的原文 hash 列表）——副本用于发请求；hash 暂不出名单（净化成功才记）。
fn apply_quarantine(messages: &[Value]) -> (Vec<Value>, Vec<u64>) {
    let g = quarantine().lock().unwrap();
    if g.is_empty() { return (messages.to_vec(), vec![]); }
    let mut hits: Vec<u64> = vec![];
    let mut out = messages.to_vec();
    for m in out.iter_mut() {
        if m["role"] == "tool" {
            if let Some(c) = m["content"].as_str() {
                let h = text_hash(c);
                if g.contains(&h) {
                    hits.push(h);
                    if let Some(obj) = m.as_object_mut() {
                        obj.insert("content".into(), json!(TOOL_RESULT_STUB));
                    }
                }
            }
        } else if m["role"] == "assistant" {
            if let Some(tcs) = m.get_mut("tool_calls").and_then(|t| t.as_array_mut()) {
                for c in tcs.iter_mut() {
                    if let Some(args) = c.get_mut("function").and_then(|f| f.get_mut("arguments")).and_then(|a| a.as_str().map(String::from)) {
                        let h = text_hash(&args);
                        if g.contains(&h) {
                            hits.push(h);
                            if let Some(f) = c.get_mut("function").and_then(|f| f.as_object_mut()) {
                                f.insert("arguments".into(), json!(TOOL_CALL_ARGS_STUB));
                            }
                        }
                    }
                }
            }
        }
    }
    (out, hits)
}

/// 内容风控净化阶梯（422 sensitive 逐级升级，全部只动发送副本）：
/// - 0：原文
/// - 1：较新一半 tool 结果 → 桩（元凶通常是最近的搜索结果）
/// - 2：全部 tool 结果 → 桩
/// - 3：2 + assistant 的 tool_call arguments → 桩（搜索词本身也是敏感源）
/// - 4：3 + 只保最近 2 条完整 turn，更早的整段（user/assistant/tool 三件套）剥掉
///      ——「回退几条」语义；真身 history 不动，agent 后续照常从 history 重建
/// 返回发送副本；role/tool_call_id 全保留 → 配对不变量不破。
fn stub_tool_results(messages: &[Value], level: usize) -> Vec<Value> {
    if level == 0 {
        return messages.to_vec();
    }
    // tool 消息的索引列表（按出现顺序）
    let tool_idx: Vec<usize> = messages.iter()
        .enumerate()
        .filter(|(_, m)| m["role"] == "tool")
        .map(|(i, _)| i)
        .collect();
    // level 1 省略较新的一半（后一半——最近的搜索结果最可能是元凶）；level ≥2 全部
    let stub_from = if level >= 2 { 0 } else { tool_idx.len() / 2 };
    let to_stub: std::collections::HashSet<usize> = tool_idx[stub_from..].iter().copied().collect();
    let mut out: Vec<Value> = messages.iter().enumerate().map(|(i, m)| {
        if to_stub.contains(&i) {
            let mut c = m.clone();
            if let Some(obj) = c.as_object_mut() {
                obj.insert("content".into(), json!(TOOL_RESULT_STUB));
            }
            c
        } else {
            m.clone()
        }
    }).collect();
    if level >= 3 {
        // assistant 的 tool_call arguments 也换桩（搜索词本身是敏感源）
        for m in out.iter_mut() {
            if m["role"] != "assistant" { continue; }
            if let Some(tcs) = m.get_mut("tool_calls").and_then(|t| t.as_array_mut()) {
                for c in tcs.iter_mut() {
                    if let Some(f) = c.get_mut("function").and_then(|f| f.as_object_mut()) {
                        f.insert("arguments".into(), json!(TOOL_CALL_ARGS_STUB));
                    }
                }
            }
        }
    }
    if level >= 4 {
        // 回退截断：只保最近 2 个 user 消息起的完整段（system + 最近 2 turn）。
        // 从后往前找第 2 个 user 的位置，之前全部剥掉（保留 idx 0 的 system）。
        let user_positions: Vec<usize> = out.iter()
            .enumerate()
            .filter(|(_, m)| m["role"] == "user")
            .map(|(i, _)| i)
            .collect();
        if user_positions.len() > 2 {
            let keep_from = user_positions[user_positions.len() - 2];
            // 从 keep_from 往前剥到 system 之后：重组成 [system] + [keep_from..]
            let mut truncated: Vec<Value> = vec![out[0].clone()]; // system
            truncated.extend_from_slice(&out[keep_from..]);
            // 剥掉中段后可能出现孤儿 tool（assistant 在剥掉区间）→ 再过一遍配对剥除
            crate::context::strip_unpaired_tool_calls(&mut truncated);
            out = truncated;
        }
    }
    out
}

#[async_trait]
impl LlmRound for HttpRound {
    async fn round(&self, messages: &[Value], cfg: &Config, emit: &dyn Emitter) -> Result<RoundResult, String> {
        let key = api_key(cfg)?;
        // 只设 connect_timeout；不设总 timeout——流式 SSE 长推理会被总 timeout 砍断。
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| format!("HTTP client 构造失败: {e}"))?;
        let url = format!("{API_BASE}/chat/completions");

        // 隔离名单：启动后首次请求恢复落盘名单；命中 hash 的污染文本直接换桩（零 422）。
        let cache = std::path::PathBuf::from(&cfg.cache_dir);
        quarantine_load(&cache);
        let (mut messages_q, _hits) = apply_quarantine(messages);
        // 发送前净化（纵深防御最后一道）：无论毒来自历史重建还是轮内实时追加，
        // 每次请求前统一剥除双向孤儿 tool 交互（幂等；与 build_messages 同一规则）。
        crate::context::strip_unpaired_tool_calls(&mut messages_q);

        // 内容风控净化级别：0=原文；1=半数 tool 结果；2=全部 tool 结果；3=+tool_call 参数；
        // 4=+回退截断（只保最近 2 turn）。422 sensitive 时逐级升级重试——常驻 agent 没有
        // 新会话可开，自愈只能靠净化发送副本：真身 history/messages 不动。
        let mut stub_level: usize = 0;
        for attempt in 1..=MAX_HTTP_ATTEMPTS {
            let messages_eff = stub_tool_results(&messages_q, stub_level);
            let body = build_body(&messages_eff, cfg)?;
            let send_result = client.post(&url).bearer_auth(&key).json(&body).send().await;
            match send_result {
                Err(e) => {
                    // 网络层错误（连接/DNS/connect 超时）→ 一律可重试
                    if attempt == MAX_HTTP_ATTEMPTS {
                        return Err(format!("LLM 请求失败（已重试 {MAX_HTTP_ATTEMPTS} 次）: {e}"));
                    }
                    emit.retry(attempt, &format!("网络错误: {e}")).await;
                    tokio::time::sleep(backoff_delay(attempt)).await;
                    continue;
                }
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        // 净化重试成功（stub_level > 0）→ 本级净化的原文 hash 记入隔离名单，
                        // 后续请求 apply_quarantine 直接换桩，不再撞 422 爬阶梯。
                        if stub_level > 0 {
                            let hashes = collect_stubbed_hashes(&messages_q, stub_level);
                            if !hashes.is_empty() {
                                quarantine_add(&hashes, &cache);
                                eprintln!("[llm] 风控净化 level {stub_level} 成功，{} 条污染文本入隔离名单", hashes.len());
                            }
                        }
                        let stream = resp.bytes_stream().map(|r| r.unwrap_or_default());
                        return consume_sse(stream, emit).await;
                    }
                    // ⚠ headers 必须在 text() 消费 response 之前取（Retry-After）
                    let headers = resp.headers().clone();
                    let text = resp.text().await.unwrap_or_default();
                    // 内容风控：升级净化级别重试（不走通用分类——那会直接 Fail）
                    if status.as_u16() == 422 && text.contains("sensitive") {
                        if stub_level < 4 {
                            stub_level += 1;
                            let action = match stub_level {
                                1 | 2 => "省略工具输出",
                                3 => "省略工具输出与调用参数",
                                _ => "回退到最近对话",
                            };
                            emit.retry(attempt, &format!("内容风控拦截，{action}后重试")).await;
                            continue;
                        }
                        return Err(SUGGEST_OPEN_NEW_CHAT_OR_REPHRASE.to_string());
                    }
                    match classify_http_error(status.as_u16(), &text) {
                        HttpErrClass::Fail(msg) => return Err(msg),
                        HttpErrClass::Retry { reason } => {
                            if attempt == MAX_HTTP_ATTEMPTS {
                                return Err(format!(
                                    "LLM HTTP {status}（已重试 {MAX_HTTP_ATTEMPTS} 次）: {}",
                                    truncate(&text, 600)
                                ));
                            }
                            emit.retry(attempt, &reason).await;
                            let delay = retry_after_secs(&headers)
                                .map(std::time::Duration::from_secs)
                                .unwrap_or_else(|| backoff_delay(attempt));
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                    }
                }
            }
        }
        unreachable!() // loop 内每个分支都 return 或 continue
    }
}

/// HTTP 错误分类：可重试（带原因）vs 不可重试（带面向用户的中文消息）。
#[derive(Debug)]
enum HttpErrClass {
    Retry { reason: String },
    Fail(String),
}

/// 按 status 把 HTTP 错误分成「可重试」与「立即报错」。瞬时错误（5xx/429/408）走退避重试；
/// 4xx 类请求本身有错的立即给可读中文提示，不浪费重试次数。
fn classify_http_error(status: u16, body: &str) -> HttpErrClass {
    // MiniMax 内容风控：422 + sensitive。正常路径下 HttpRound 已在到达这里之前用净化重试
    // 处理过（stub_tool_results 两级）；此分支只是兜底（如非 HttpRound 调用方）。
    if status == 422 && body.contains("sensitive") {
        return HttpErrClass::Fail(SUGGEST_OPEN_NEW_CHAT_OR_REPHRASE.to_string());
    }
    match status {
        408 => HttpErrClass::Retry { reason: "请求超时".into() },
        429 => HttpErrClass::Retry { reason: "账号限流".into() },
        500 => HttpErrClass::Retry { reason: "服务端内部错误".into() },
        502 => HttpErrClass::Retry { reason: "网关错误".into() },
        503 => HttpErrClass::Retry { reason: "服务暂不可用".into() },
        504 => HttpErrClass::Retry { reason: "网关超时".into() },
        529 => HttpErrClass::Retry { reason: "集群过载".into() },
        401 => HttpErrClass::Fail("API Key 无效或已过期，请在「设置」里检查".into()),
        403 => HttpErrClass::Fail("无权限访问（API Key 权限不足或被禁用）".into()),
        404 => HttpErrClass::Fail("模型或端点不存在，请检查「设置 → 模型」".into()),
        413 => HttpErrClass::Fail("请求体过大（历史太长或附件太大），请开新会话或精简内容".into()),
        400 | 422 => HttpErrClass::Fail(format!("请求参数错误（HTTP {status}）: {}", truncate(body, 300))),
        _ => HttpErrClass::Fail(format!("LLM HTTP {status}: {}", truncate(body, 600))),
    }
}

/// 指数退避：attempt 1→1s, 2→2s, 3→4s, 4→8s（封顶 8s）。不引 crate，无 jitter。
fn backoff_delay(attempt: usize) -> std::time::Duration {
    let secs = 1u64 << attempt.saturating_sub(1).min(3); // 1,2,4,8
    std::time::Duration::from_secs(secs)
}

/// 解析 Retry-After 头：只认数字秒；HTTP-date 不解析，返 None 回落到 backoff。
fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

// usage 不再后端聚合：run_turn 直接把最后一次服务器返回的 usage 原值 emit/落盘（不累加不重组）。
// cached_tokens 在 prompt_tokens_details.cached_tokens（MiniMax OpenAI 兼容真实结构，
// live 验证 2026-07-28），前端 attachTokenBadge 读该路径（顶层 fallback 兼容旧扁平落盘）。

use std::path::Path;

/// chat 命令的统一返回（永不抛 Err：失败也带 partial history + error）。
///
/// Clone derive 跳过 awaiting：oneshot::Receiver 单次消费、不能 Clone。
/// driver move awaiting 出去；本结构 Clone 不复制 handle。
#[derive(serde::Serialize, Debug)]
pub struct ChatResponse {
    pub content: String,
    pub history: Vec<Value>,
    pub error: Option<String>,
    pub usage: Option<Value>,
    #[serde(skip)]
    pub awaiting: Option<crate::ask_user::AskUserHandle>,
}
// 手动 Clone：跳过 awaiting（oneshot receiver 单次消费）。
impl Clone for ChatResponse {
    fn clone(&self) -> Self {
        Self {
            content: self.content.clone(),
            history: self.history.clone(),
            error: self.error.clone(),
            usage: self.usage.clone(),
            awaiting: None, // 关键：clone 不复制 awaiting；move 它走
        }
    }
}
impl Default for ChatResponse {
    fn default() -> Self {
        Self { content: String::new(), history: Vec::new(), error: None, usage: None, awaiting: None }
    }
}

/// 事件出口抽象（真实 AppEmitter 在 lib.rs；测试用录音实现）。
#[async_trait]
pub trait Emitter: Send + Sync {
    async fn thinking(&self, _text: &str) {}
    async fn content(&self, _text: &str) {}
    async fn tool_call(&self, _name: &str, _args: &str) {}
    async fn tool_result(&self, _name: &str, _result: &str) {}
    async fn turn_start(&self) {}
    async fn turn_end(&self) {}
    async fn error(&self, _msg: &str) {}
    /// LLM 即将重试（瞬时错误）。默认仅打 stderr 日志；AppEmitter 可覆盖以发前端事件。
    async fn retry(&self, _attempt: usize, _reason: &str) {
        eprintln!("[llm] 重试 ({_attempt}): {_reason}");
    }
    /// 一轮结束的 token 用量汇总（prompt/completion/cached）。默认仅日志；AppEmitter 覆盖发前端。
    async fn usage(&self, _u: &Value) {
        eprintln!("[llm] usage: {:?}", _u);
    }
}

pub const MAX_ITERS: usize = 12;

/// 螺旋检测阈值:连续 SPIRAL_LIMIT 轮「同名工具 + 归一化后相同参数 + 无文本产出」即强制收尾。
/// 防 agent 陷入读自己输出式死循环(如反复 `mem history --seq N`,只有 N 在变)。
/// 文本产出(content 非空)视为有进展 → 计数清零。max_tool_iters 是硬墙,这是早停。
pub const SPIRAL_LIMIT: usize = 6;

/// 单轮工具循环：每事件一次 turn。开头 emit turn_start、Stop/上限 emit turn_end、出错 emit error。
/// messages 由调用方（driver）私有持有并 &mut 传入；history 为本轮新增消息副本。
/// `thread` 标记本事件流归属（"main" / "agent:{id}"）：assistant/tool_result 事件落 history 单写时按此区分。
pub async fn run_turn<E: Emitter>(
    round: Arc<dyn LlmRound>,
    emitter: &E,
    cfg: &Config,
    messages: &mut Vec<Value>,
    ctx: &tools::ToolsCtx,
    max_iters: usize,
    thread: &str,
) -> ChatResponse {
    emitter.turn_start().await;
    let mut history: Vec<Value> = vec![];
    let mut last_content = String::new();
    // 角标语义 = 最后一次服务器返回的 usage 原样（不累加、不重组）。多轮工具循环时只呈现最后那次；
    // 前面各轮的 per-round usage 仍逐轮落盘（见 ToolCalls 分支 resp.usage.clone()），可 drill。
    let mut last_usage: Option<Value> = None;
    // 螺旋检测状态:连续「同名工具+归一化相同参数+无文本」的轮数
    let mut spiral_count: usize = 0;
    let mut prev_sig: Option<String> = None;
    let mut spiral_stopped = false;
    // 中断:driver/interrupt_task 命令 cancel ctx.interrupt 里的 token → run_turn 退出写「用户中断」兜底。
    // current() 在 turn 开始时由 handle_event set;None = 此 ctx 不支持中断(legacy/测试),走原路径。
    let cancel_token = ctx.interrupt.current();
    let mut interrupted = false;
    for i in 0..max_iters {
        eprintln!("[turn] round {i}: 调用模型（流式）…");
        // select 包 round:有 cancel_token 时,生成途中 cancel 先就绪 → round future drop → reqwest 请求 abort。
        // 无 token(legacy/测试)走原 await。中断 → interrupted=true + break(本轮 resp 未入 history,无孤儿)。
        let resp = if let Some(t) = &cancel_token {
            let mut got: Option<Result<RoundResult, String>> = None;
            tokio::select! {
                r = round.round(messages, cfg, emitter) => got = Some(r),
                _ = t.cancelled() => interrupted = true,
            }
            if interrupted { eprintln!("[turn] round {i}: 用户中断"); break; }
            match got.unwrap() {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[turn] round {i}: 失败 {e}");
                    emitter.error(&e).await;
                    return ChatResponse { content: String::new(), history, error: Some(e), usage: None, awaiting: None };
                }
            }
        } else {
            match round.round(messages, cfg, emitter).await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[turn] round {i}: 失败 {e}");
                    emitter.error(&e).await;
                    return ChatResponse { content: String::new(), history, error: Some(e), usage: None, awaiting: None };
                }
            }
        };
        if let Some(u) = &resp.usage { last_usage = Some(u.clone()); }
        last_content = resp.content.clone();
        eprintln!("[turn] round {i}: finish={:?} tool_calls={}", resp.finish, resp.tool_calls.len());
        match resp.finish {
            FinishReason::ToolCalls => {
                messages.push(resp.assistant_message.clone());
                history.push(resp.assistant_message);
                // v2：assistant(tool_calls) 事件落 history（线程内调用→结果天然相邻）；带本轮 usage
                ctx.history.append(crate::history::HistoryEvent::assistant_with_usage(
                    now_ms_llm(), thread, &resp.content, &resp.reasoning, resp.tool_calls.clone(),
                    resp.usage.clone(),
                ));
                // 螺旋检测:本轮签名(name|归一化args)vs 上轮。有文本产出=有进展→清零;
                // 否则签名相同→累加,不同→从 1 起。达 SPIRAL_LIMIT 在执行完本轮工具后强停。
                let sig = iter_signature(&resp.tool_calls);
                if !resp.content.trim().is_empty() {
                    spiral_count = 0;
                } else if prev_sig.as_deref() == Some(sig.as_str()) {
                    spiral_count += 1;
                } else {
                    spiral_count = 1;
                }
                prev_sig = Some(sig);
                let round_calls = resp.tool_calls.clone(); // 中断补齐要用（for 会 move 原 vec）
                for call in resp.tool_calls {
                    // 中断快捷检查：LLM 流式期间 cancel（上面 select 已 break）不会到这，
                    // 但前一个工具执行中 cancel 的场景在此拦下剩余 tool_calls。
                    if interrupted { break; }
                    let name = call["function"]["name"].as_str().unwrap_or("").to_string();
                    let args_str = call["function"]["arguments"].as_str().unwrap_or("{}").to_string();
                    let call_id = call["id"].as_str().unwrap_or("").to_string();
                    emitter.tool_call(&name, &args_str).await;
                    // select 包 dispatch：工具执行（尤其 bash 前台子进程）期间 cancel →
                    // 工具 future drop（bash 内部 select 先行杀树）→ interrupted=true 立即退出。
                    // 无 token（legacy/测试）走原 await。
                    //
                    // ask_user 走挂起路径：dispatch 返 Err(AskUserHandle)→不写 tool_result,
                    // return ChatResponse.awaiting 让 driver 接管。
                    let dispatch_outcome = match serde_json::from_str::<Value>(&args_str) {
                        Err(e) => Ok(format!("参数解析失败: {e}")),
                        Ok(args) => {
                            if let Some(t) = &cancel_token {
                                let mut got: Option<Result<String, crate::ask_user::AskUserHandle>> = None;
                                tokio::select! {
                                    r = tools::dispatch(&name, args, ctx, cfg, round.clone()) => got = Some(r),
                                    _ = t.cancelled() => interrupted = true,
                                }
                                if interrupted {
                                    eprintln!("[turn] round {i}: 工具 {name} 执行中用户中断");
                                    break;
                                }
                                // 竞态兜底：bash 内部 select 与 cancel 同时就绪时 dispatch 臂可能
                                // 先赢（随机公平），此时 token 已 cancelled——按结果查一次状态，
                                // 已 cancel 则视同中断（工具已自行杀树返回，无孤儿）。
                                if t.is_cancelled() {
                                    interrupted = true;
                                    eprintln!("[turn] round {i}: 工具 {name} 返回时发现已 cancel");
                                    break;
                                }
                                got.unwrap()
                            } else {
                                tools::dispatch(&name, args, ctx, cfg, round.clone()).await
                            }
                        }
                    };
                    // ask_user 挂起:不写 tool_result、不 emit、不落 history;返 awaiting 让 driver 接管。
                    if let Err(mut handle) = dispatch_outcome {
                        if name == "ask_user" {
                            // 回填 call_id:答案最终按 tool_result 落 history,靠它与 assistant.tool_calls 配对
                            handle.call_id = call_id;
                            // 收尾:本 assistant 已 emit (上面 tool_call 调过 emitter.tool_call);
                            // turn_end 也别 emit（前端由 driver 后续 chat-turn-start/end 推）。
                            return ChatResponse {
                                content: last_content,
                                history,
                                error: None,
                                usage: last_usage,
                                awaiting: Some(handle),
                            };
                        }
                        // 防御:其他工具走 Result 不该返 Err（dispatch match 全 Ok 包装），
                        // 真出现就当普通 tool_result 写"挂起未实现"提示。
                        let fallback = format!("工具 {name} 触发挂起但 run_turn 未实现该路径（question_id={}）。", handle.question_id);
                        emitter.tool_result(&name, &fallback).await;
                        let m = json!({ "role": "tool", "tool_call_id": call_id, "content": fallback });
                        messages.push(m.clone());
                        history.push(m.clone());
                        ctx.history.append(crate::history::HistoryEvent::tool_result(
                            now_ms_llm(), thread, &name, &fallback, &call_id,
                        ));
                        continue;
                    }
                    let result = dispatch_outcome.unwrap();
                    emitter.tool_result(&name, &result).await;
                    let m = json!({ "role": "tool", "tool_call_id": call_id, "content": result });
                    messages.push(m.clone());
                    history.push(m.clone());
                    // v2：tool_result 事件落 history（call→result 相邻，配对天然成立）
                    ctx.history.append(crate::history::HistoryEvent::tool_result(
                        now_ms_llm(), thread, &name, &result, &call_id,
                    ));
                }
                // 中断兜底：本轮 assistant 的 tool_calls 逐个执行时中断/竞态 break 会跳过
                // 未执行的那些——若不补 tool_result，history 留下「assistant(tool_calls) 无配对
                // tool」的中间孤儿，MiniMax 在后续含 tool 交互的请求里报 400 invalid params
                // (2013)（drop_trailing_orphan 只删末尾孤儿，救不了中间的）。这里给本轮所有
                // 未配对的 call 补「用户中断，未执行」tool_result：只落盘进 messages/history，
                // 不 emit tool_result（用户已点中断，无需再刷一张结果卡）。
                if interrupted {
                    let done: std::collections::HashSet<String> = messages.iter()
                        .filter(|m| m["role"] == "tool")
                        .filter_map(|m| m["tool_call_id"].as_str().map(String::from))
                        .collect();
                    for c in &round_calls {
                        let id = c["id"].as_str().unwrap_or("").to_string();
                        if id.is_empty() || done.contains(&id) { continue; }
                        let name = c["function"]["name"].as_str().unwrap_or("").to_string();
                        let m = json!({ "role": "tool", "tool_call_id": id, "content": "用户中断，未执行" });
                        messages.push(m.clone());
                        history.push(m.clone());
                        ctx.history.append(crate::history::HistoryEvent::tool_result(
                            now_ms_llm(), thread, &name, "用户中断，未执行", &id,
                        ));
                    }
                }
                if spiral_count >= SPIRAL_LIMIT {
                    spiral_stopped = true;
                    break; // 螺旋强停 → 走兜底(siral_stopped 标记选「螺旋」文案)
                }
                if interrupted { break; } // 工具循环中 cancel → 不再进入下一轮 LLM 调用
                continue;
            }
            FinishReason::Stop => {
                history.push(resp.assistant_message.clone());
                // 最终 assistant 事件落 history + emit：带最后一次服务器返回的 usage 原值
                // （不累加不重组）。history 重建该气泡显示 = 服务器最后返回，与 live 一致。
                let usage = last_usage.clone();
                ctx.history.append(crate::history::HistoryEvent::assistant_with_usage(
                    now_ms_llm(), thread, &last_content, "", vec![], usage.clone(),
                ));
                if let Some(u) = &usage { emitter.usage(u).await; }
                emitter.turn_end().await;
                return ChatResponse { content: last_content, history, error: None, usage, awaiting: None };
            }
        }
    }
    // 循环结束:用户中断 / 螺旋强停 / max_iters 用尽。按原因给兜底文本。
    let stopped = if interrupted { "interrupt" } else if spiral_stopped { "spiral" } else { "maxiters" };
    let (full_msg, suffix): (String, &'static str) = match stopped {
        "interrupt" => (
            if last_content.trim().is_empty() {
                "（用户中断了本轮任务。以上是中断前的工具调用与进展；请结合这些 + 我的新指令决定继续还是重做。）".to_string()
            } else {
                format!("{last_content}\n\n_（用户中断了本轮任务）_")
            },
            "\n\n_（用户中断了本轮任务）_",
        ),
        "spiral" => (
            if last_content.trim().is_empty() {
                "（检测到工具调用螺旋：连续多次以相同方式调用同一工具且无文本产出，已强制停止以防死循环。可重试或换种方式。）".to_string()
            } else {
                format!("{last_content}\n\n_（检测到工具调用螺旋，已强制停止）_")
            },
            "\n\n_（检测到工具调用螺旋，已强制停止）_",
        ),
        _ => (
            if last_content.trim().is_empty() {
                "（已达工具调用上限，未产生最终答复）".to_string()
            } else {
                format!("{last_content}\n\n_（已达工具调用上限）_")
            },
            "\n\n_（已达工具调用上限）_",
        ),
    };
    let content = full_msg;
    history.push(json!({ "role": "assistant", "content": &content }));
    // v2：兜底 assistant 也落 history（与 Stop 路径一致）；兜底文本是合成的，不带 usage
    ctx.history.append(crate::history::HistoryEvent::assistant(
        now_ms_llm(), thread, &content, "", vec![],
    ));
    // C2 修：兜底文本也流式给前端——否则 live wrap 的 bubble-text 是空的（工具卡在、
    // 最终文本不在；只有重启从 history 重建才显示）。last_content 正文已在轮内流式过，
    // 这里只补后缀；last_content 全空时补整句。
    if last_content.trim().is_empty() {
        emitter.content(&content).await;
    } else {
        emitter.content(suffix).await;
    }
    let usage = last_usage.clone();
    if let Some(u) = &usage { emitter.usage(u).await; }
    emitter.turn_end().await;
    ChatResponse { content, history, error: None, usage, awaiting: None }
}

/// 把参数串里的连续数字折叠成单个 `#`(序号/时间戳/计数只变数字的循环 → 归一化后相同)。
/// 只折叠 ASCII 数字;结构/字母不变,故 `cat log1.txt` 与 `cat log2.txt` 算同,`echo a` 与 `echo b` 不同。
fn normalize_args(args: &str) -> String {
    let mut out = String::with_capacity(args.len());
    let mut in_digit = false;
    for ch in args.chars() {
        if ch.is_ascii_digit() {
            if !in_digit { out.push('#'); in_digit = true; }
        } else {
            in_digit = false;
            out.push(ch);
        }
    }
    out
}

/// 一轮所有 tool_call 的 `name|归一化args` 排序拼接 → 签名。多 tool 时顺序无关、稳定。
fn iter_signature(tool_calls: &[Value]) -> String {
    let mut parts: Vec<String> = tool_calls.iter().map(|c| {
        let name = c["function"]["name"].as_str().unwrap_or("");
        let args = c["function"]["arguments"].as_str().unwrap_or("");
        format!("{}|{}", name, normalize_args(args))
    }).collect();
    parts.sort();
    parts.join(";;")
}

/// epoch ms（llm 模块本地副本；不引 jobs/subagents 避免循环）。跨模块同名无冲突。
fn now_ms_llm() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 工具循环（向后兼容壳）：构造前台 ctx，委托 run_turn。chat() 旧路径 + 既有测试用。
pub async fn run_loop<E: Emitter>(
    round: Arc<dyn LlmRound>,
    emitter: E,
    cfg: &Config,
    mut messages: Vec<Value>,
    workspace: &Path,
) -> ChatResponse {
    let ctx = tools::ToolsCtx::foreground(workspace.to_path_buf(), cfg.minimax_region.clone());
    run_turn(round, &emitter, cfg, &mut messages, &ctx, MAX_ITERS, "main").await
}

/// 移除所有 <think...> 块（未闭合则丢弃其后全部内容）。
pub fn strip_think(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("<think") {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        match rest.find("</think") {
            Some(end) => rest = &rest[end + "</think".len()..],
            None => return out.trim().to_string(), // 未闭合，丢弃剩余
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// 从 content 剥离模型自带的思考块，返回 (干净正文, 剥出的思考文本)。
/// 处理 `…`（未闭合取到结尾）与 `<details>…</details>` 且块内含「思考」。
/// 剥出的文本去掉内联标签后并进 reasoning（独立折叠区），正文保持干净。
pub fn strip_thinking_blocks(s: &str) -> (String, String) {
    let (after_think, think_text) = extract_delimited(s, "…", "…", |_| true);
    let (clean, details_text) =
        extract_delimited(&after_think, "<details", "</details>", |inner| inner.contains("思考"));
    let mut thinking = think_text;
    if !details_text.is_empty() {
        let stripped = strip_inline_tags(&details_text);
        if !stripped.trim().is_empty() {
            if !thinking.is_empty() {
                thinking.push('\n');
            }
            thinking.push_str(&stripped);
        }
    }
    (clean.trim().to_string(), thinking.trim().to_string())
}

/// 扫描 input：每个 `open…close` 块，predicate 为真则抽出 inner 进 extracted 并从输出移除，
/// 为假则原样保留；未闭合的块取到结尾。
fn extract_delimited(input: &str, open: &str, close: &str, predicate: impl Fn(&str) -> bool) -> (String, String) {
    let mut out = String::new();
    let mut extracted = String::new();
    let mut rest = input;
    while let Some(start) = rest.find(open) {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + open.len()..];
        match after_open.find(close) {
            Some(end) => {
                let inner = &after_open[..end];
                if predicate(inner) {
                    if !extracted.is_empty() {
                        extracted.push('\n');
                    }
                    extracted.push_str(inner);
                } else {
                    out.push_str(open);
                    out.push_str(inner);
                    out.push_str(close);
                }
                rest = &after_open[end + close.len()..];
            }
            None => {
                if predicate(after_open) {
                    if !extracted.is_empty() {
                        extracted.push('\n');
                    }
                    extracted.push_str(after_open);
                } else {
                    out.push_str(open);
                    out.push_str(after_open);
                }
                rest = "";
            }
        }
    }
    out.push_str(rest);
    (out, extracted)
}

/// 去掉 `<...>` 内联标签，仅留文本（清理 <summary>…</summary> 等成可读思考）。
fn strip_inline_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() > n {
        format!("{}…", &s[..n])
    } else {
        s.to_string()
    }
}

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
        self.map.into_iter().filter_map(|(idx, (id, name, args))| {
            // 空 name 的 call 是模型畸形输出（M3 线上两次复现：name/id 双空）。它无法被执行、
            // 也无法通过服务端 tool_calls 结构校验（MiniMax 400 (2013)）——直接丢弃，绝不落盘/
            // 进 echo：合成 id 曾让它"配对成立"，反而把毒保活成每轮 400 死循环。
            // name 非空而 id 缺失 → 合成稳定 id，echo 与 tool_result 恒配对。
            let name = match name {
                Some(n) if !n.is_empty() => n,
                _ => return None,
            };
            let id = match id {
                Some(s) if !s.is_empty() => s,
                _ => format!("call_synth_{idx}"),
            };
            Some(json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": args }
            }))
        }).collect()
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::collections::VecDeque;

    #[test]
    fn removes_think_block() {
        let s = "<think这是推理过程，不该被朗读</think\n\n这是最终回答。";
        assert_eq!(strip_think(s), "这是最终回答。");
    }

    #[test]
    fn no_block_unchanged() {
        assert_eq!(strip_think("一段普通文本"), "一段普通文本");
    }

    #[test]
    fn unclosed_drops_tail() {
        assert_eq!(strip_think("保留这部分<think后面没闭合"), "保留这部分");
    }

    #[test]
    fn multiple_blocks() {
        let s = "甲<thinkx</think乙<thinky</think丙";
        assert_eq!(strip_think(s), "甲乙丙");
    }

    #[test]
    fn strip_thinking_removes_think_block() {
        let (c, t) = strip_thinking_blocks("…隐藏的推理…这是回答");
        assert_eq!(c, "这是回答");
        assert_eq!(t, "隐藏的推理");
    }

    #[test]
    fn strip_thinking_removes_details_thinking() {
        // 复现线上：模型把思考塞进 <details><summary>💭 思考过程</summary>…</details>
        let s = "<details><summary>💭 思考过程</summary>用户要验证中文不乱码。简单单步调用。</details>答案是：用 dir。";
        let (c, t) = strip_thinking_blocks(s);
        assert_eq!(c, "答案是：用 dir。");
        assert!(t.contains("思考过程"), "thinking routed: {t}");
        assert!(t.contains("用户要验证"), "body kept in thinking: {t}");
    }

    #[test]
    fn strip_thinking_keeps_non_thinking_details() {
        let (c, t) = strip_thinking_blocks("<details><summary>参考资料</summary>一些资料</details>答案");
        assert_eq!(c, "<details><summary>参考资料</summary>一些资料</details>答案");
        assert_eq!(t, "");
    }

    #[test]
    fn accum_synthesizes_id_and_drops_empty_name() {
        // 空/缺 id 的 call → 合成 call_synth_N；空 name 的畸形 call → 整条丢弃（不落盘不进 echo）
        let mut a = ToolCallAccum::new();
        a.push(0, None, Some("display".into()), Some("{}".into()));
        a.push(1, Some("".into()), Some("".into()), Some("{}".into()));
        a.push(2, Some("call_x".into()), Some("bash".into()), Some("{}".into()));
        let v = a.finalize();
        assert_eq!(v.len(), 2, "空 name 畸形 call 须丢弃: {v:?}");
        assert!(v[0]["id"].as_str().unwrap().starts_with("call_synth_0"), "缺 id 须合成: {}", v[0]);
        assert_eq!(v[1]["id"], "call_x", "合法 id 原样保留");
    }

    #[test]
    fn build_body_has_tools_and_reasoning_split() {
        let cfg = Config::default();
        let body = build_body(&[serde_json::json!({"role":"user","content":"hi"})], &cfg).unwrap();
        assert_eq!(body["reasoning_split"], true);
        assert_eq!(body["stream"], true);
        assert_eq!(body["tool_choice"], "auto");
        // 24 + ask_user = 25（须与 tools.rs::schemas_has_twenty_five_tools 同步）
        assert_eq!(body["tools"].as_array().unwrap().len(), 25);
    }

    #[test]
    fn build_body_uses_active_tools_when_set() {
        let mut cfg = Config::default();
        cfg.active_tools = Some(vec![serde_json::json!({"type":"function","function":{"name":"read"}})]);
        let body = build_body(&[serde_json::json!({"role":"user","content":"hi"})], &cfg).unwrap();
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "read");
    }

    #[test]
    fn build_body_defaults_to_all_schemas_when_none() {
        let cfg = Config::default();
        let body = build_body(&[serde_json::json!({"role":"user","content":"hi"})], &cfg).unwrap();
        assert_eq!(body["tools"].as_array().unwrap().len(), tools::schemas().len());
    }

    // Test helpers for Step 1
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
    #[derive(Default)]
    struct RecEmitter { thinking: Mutex<Vec<String>>, contents: Mutex<Vec<String>>, calls: Mutex<Vec<(String,String,String)>> }
    #[async_trait]
    impl Emitter for RecEmitter {
        async fn thinking(&self, t: &str) { self.thinking.lock().unwrap().push(t.into()); }
        async fn content(&self, t: &str) { self.contents.lock().unwrap().push(t.into()); }
        async fn tool_call(&self, n: &str, a: &str) { self.calls.lock().unwrap().push((n.into(),a.into(),"call".into())); }
        async fn tool_result(&self, n: &str, r: &str) { self.calls.lock().unwrap().push((n.into(),r.into(),"result".into())); }
    }
    impl RecEmitter {
        /// 已发出的 tool_call 数（"call" 记录），中断时序测试用。
        fn tool_calls_len(&self) -> usize {
            self.calls.lock().unwrap().iter().filter(|(_,_,k)| k == "call").count()
        }
    }

    fn tool_round(name:&str, args:&str, id:&str) -> RoundResult {
        let tc = json!({"id":id,"type":"function","function":{"name":name,"arguments":args}});
        RoundResult {
            content: String::new(), reasoning: "想一下".into(),
            tool_calls: vec![tc.clone()],
            finish: FinishReason::ToolCalls, usage: None,
            assistant_message: json!({"role":"assistant","content":null,"tool_calls":[tc]}),
        }
    }

    #[tokio::test]
    async fn loop_one_tool_then_final() {
        // 用 bash echo（真实执行）验证 dispatch 也被调用
        let round: Arc<dyn LlmRound> = Arc::new(FakeRound(Mutex::new(VecDeque::from(vec![
            Ok(tool_round("bash", r#"{"command":"echo hi"}"#, "c1")),
            Ok(RoundResult { content:"完成".into(), reasoning:String::new(), tool_calls:vec![],
                finish:FinishReason::Stop, usage:None, assistant_message:json!({"role":"assistant","content":"完成"}) }),
        ]))));
        let em = RecEmitter::default();
        let cfg = Config::default();
        let ws = std::env::temp_dir();
        let res = run_loop(round, em, &cfg, vec![json!({"role":"user","content":"跑下"})], &ws).await;
        assert_eq!(res.content, "完成");
        assert!(res.error.is_none());
        // history: assistant(tool_calls) + tool(result) + assistant(final)
        assert_eq!(res.history.len(), 3);
        assert_eq!(res.history[0]["tool_calls"][0]["id"], "c1");
        assert_eq!(res.history[1]["role"], "tool");
        assert!(res.history[1]["content"].as_str().unwrap().contains("hi")); // echo hi 的输出
        assert_eq!(res.history[2]["content"], "完成");
    }

    #[tokio::test]
    async fn loop_mid_fail_keeps_partial_history() {
        let round: Arc<dyn LlmRound> = Arc::new(FakeRound(Mutex::new(VecDeque::from(vec![
            Ok(tool_round("bash", r#"{"command":"echo a"}"#, "c1")),
            Err("LLM HTTP 500: boom".into()),
        ]))));
        let em = RecEmitter::default();
        let cfg = Config::default();
        let ws = std::env::temp_dir();
        let res = run_loop(round, em, &cfg, vec![], &ws).await;
        assert_eq!(res.error.as_deref(), Some("LLM HTTP 500: boom"));
        assert_eq!(res.content, "");
        // partial history 保留：1 个 assistant(tool_calls) + 1 个 tool(result)
        assert_eq!(res.history.len(), 2);
    }

    #[tokio::test]
    async fn loop_cap_at_12() {
        // 每轮参数不同(字母变、非数字 → 归一化后仍不同,不触发螺旋检测),纯测 max_iters=12 兜底
        let mut steps = VecDeque::new();
        for i in 0..13u32 {
            let letter = (b'a' + (i as u8 % 26)) as char;
            let args = format!("{{\"command\":\"echo {letter}\"}}");
            steps.push_back(Ok(tool_round("bash", &args, "c")));
        }
        let round: Arc<dyn LlmRound> = Arc::new(FakeRound(Mutex::new(steps)));
        let em = RecEmitter::default();
        let cfg = Config::default();
        let ws = std::env::temp_dir();
        let res = run_loop(round, em, &cfg, vec![], &ws).await;
        assert!(res.content.contains("工具调用上限"), "got: {}", res.content);
        // 12 轮 × (assistant + tool) = 24 条，加 1 条兜底 assistant = 25
        assert_eq!(res.history.len(), 25);
    }

    #[tokio::test]
    async fn loop_spiral_stops_early() {
        // 连续相同工具 + 只有数字变的参数(归一化相同) + 无文本 → 螺旋检测在 SPIRAL_LIMIT 强停(早于 max_iters)
        let mut steps = VecDeque::new();
        for i in 0..20u32 {
            let args = format!("{{\"command\":\"mem history 2026-08-03 --seq {i} | tail -10\"}}");
            steps.push_back(Ok(tool_round("bash", &args, "c")));
        }
        let round: Arc<dyn LlmRound> = Arc::new(FakeRound(Mutex::new(steps)));
        let em = RecEmitter::default();
        let cfg = Config::default();
        let ws = std::env::temp_dir();
        let res = run_loop(round, em, &cfg, vec![], &ws).await;
        assert!(res.content.contains("螺旋"), "应被螺旋检测停住,got: {}", res.content);
        // SPIRAL_LIMIT 轮 × (assistant + tool) + 1 兜底
        assert_eq!(res.history.len(), SPIRAL_LIMIT * 2 + 1, "应在 SPIRAL_LIMIT 早停");
    }

    // round 永不返回(模拟 M3 长生成);外部 cancel token → select 取 cancelled 分支 → 中断兜底。
    struct HangingRound;
    #[async_trait]
    impl LlmRound for HangingRound {
        async fn round(&self, _: &[Value], _: &Config, _: &dyn Emitter) -> Result<RoundResult, String> {
            let (): () = std::future::pending().await; // 永不完成
            unreachable!()
        }
    }

    #[tokio::test]
    async fn run_turn_interrupt_writes_fallback() {
        // ctx.interrupt 装一个已 set 的 token(run_turn 会走 select 分支);foreground 默认是空 handle,这里覆盖。
        let interrupt = crate::tools::InterruptHandle::new();
        let token = tokio_util::sync::CancellationToken::new();
        interrupt.set(token.clone());
        let ctx = {
            let mut c = crate::tools::ToolsCtx::foreground(std::env::temp_dir(), "cn".into());
            c.interrupt = interrupt;
            c
        };
        let em = RecEmitter::default();
        let cfg = Config::default();
        let round: Arc<dyn LlmRound> = Arc::new(HangingRound);
        let mut messages = vec![json!({"role":"user","content":"hi"})];
        // 跑 run_turn;80ms 后 cancel → 应中断兜底(否则测试会因 round 永挂而超时)。
        let turn = tokio::spawn(async move {
            run_turn(round, &em, &cfg, &mut messages, &ctx, 100, "main").await
        });
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        token.cancel();
        let res = turn.await.unwrap();
        assert!(res.content.contains("中断"), "应被中断兜底,got: {}", res.content);
    }

    // ─── 工具执行期间中断（bugfix：bash 跑着时点中断不生效）───
    // 慢工具 round：LLM 返回一个 bash tool_call（真跑 sleep），dispatch 执行期间 cancel →
    // 应立即中断兜底，而不是等 sleep 跑完。
    #[tokio::test]
    async fn run_turn_interrupt_during_tool_execution() {
        let interrupt = crate::tools::InterruptHandle::new();
        let token = tokio_util::sync::CancellationToken::new();
        interrupt.set(token.clone());
        let ctx = {
            let mut c = crate::tools::ToolsCtx::foreground(std::env::temp_dir(), "cn".into());
            c.interrupt = interrupt;
            c
        };
        let em = std::sync::Arc::new(RecEmitter::default());
        let cfg = Config::default();
        // 第一轮：tool_call bash sleep 30（超时上限给足，确保不是超时路径杀的）；
        // 后续轮 pending（不该被走到——中断应发生在工具执行中）。
        let mut steps = VecDeque::new();
        steps.push_back(Ok(tool_round("bash", r#"{"command":"sleep 30","timeout_secs":300}"#, "before")));
        steps.push_back(Ok(RoundResult {
            finish: FinishReason::Stop, content: "不该到这".into(), reasoning: String::new(),
            assistant_message: json!({"role":"assistant","content":"不该到这"}),
            tool_calls: vec![], usage: None,
        }));
        let round: Arc<dyn LlmRound> = Arc::new(FakeRound(Mutex::new(steps)));
        let mut messages = vec![json!({"role":"user","content":"hi"})];
        let t0 = std::time::Instant::now();
        let em_turn = em.clone();
        let turn = tokio::spawn(async move {
            run_turn(round, &*em_turn, &cfg, &mut messages, &ctx, 100, "main").await
        });
        // 等 tool_call 已发出（dispatch 开始）再 cancel：轮询 emitter 记录到 bash tool_call。
        for _ in 0..200 {
            if em.tool_calls_len() >= 1 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(em.tool_calls_len() >= 1, "应在 cancel 前已发出 tool_call");
        token.cancel();
        let res = turn.await.unwrap();
        assert!(res.content.contains("中断"), "工具执行中 cancel 应中断兜底, got: {}", res.content);
        // 关键断言：中断应在秒级生效（sleep 30 没跑完）。留 10s 余量（CI 慢机）。
        assert!(t0.elapsed() < std::time::Duration::from_secs(10),
            "中断应立即生效而非等工具跑完, elapsed: {:?}", t0.elapsed());
    }

    // 同轮多个 tool_calls：第 1 个执行中 cancel → 第 2 个不再执行（跳过剩余）。
    #[tokio::test]
    async fn run_turn_interrupt_skips_remaining_tool_calls() {
        let interrupt = crate::tools::InterruptHandle::new();
        let token = tokio_util::sync::CancellationToken::new();
        interrupt.set(token.clone());
        let ctx = {
            let mut c = crate::tools::ToolsCtx::foreground(std::env::temp_dir(), "cn".into());
            c.interrupt = interrupt;
            c
        };
        let em = std::sync::Arc::new(RecEmitter::default());
        let cfg = Config::default();
        let mut steps = VecDeque::new();
        // 一轮俩 tool_call：sleep 30 + echo（echo 不该被执行到）
        let tc = |name: &str, args: &str| json!({"id":"c","type":"function","function":{"name":name,"arguments":args}});
        let r = RoundResult {
            finish: FinishReason::ToolCalls, content: String::new(), reasoning: String::new(),
            assistant_message: json!({"role":"assistant","content":""}),
            tool_calls: vec![
                tc("bash", r#"{"command":"sleep 30","timeout_secs":300}"#),
                tc("bash", r#"{"command":"echo second"}"#),
            ],
            usage: None,
        };
        steps.push_back(Ok(r));
        let round: Arc<dyn LlmRound> = Arc::new(FakeRound(Mutex::new(steps)));
        let mut messages = vec![json!({"role":"user","content":"hi"})];
        let em_turn = em.clone();
        let turn = tokio::spawn(async move {
            run_turn(round, &*em_turn, &cfg, &mut messages, &ctx, 100, "main").await
        });
        for _ in 0..200 {
            if em.tool_calls_len() >= 1 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        token.cancel();
        let res = turn.await.unwrap();
        assert!(res.content.contains("中断"), "got: {}", res.content);
        assert_eq!(em.tool_calls_len(), 1, "第 2 个 tool_call 不应执行, got {} 个", em.tool_calls_len());
    }

    // ─── 中断补齐 tool_result（毒化 history 根因修复回归）───
    // 中断 break 跳过未执行 tool_call 时，必须给所有未配对 call 补「用户中断，未执行」
    // tool_result——否则 history 留中间孤儿，MiniMax 后续含 tool 交互请求 400 (2013)。
    #[tokio::test]
    async fn run_turn_interrupt_backfills_tool_results_for_skipped_calls() {
        let interrupt = crate::tools::InterruptHandle::new();
        let token = tokio_util::sync::CancellationToken::new();
        interrupt.set(token.clone());
        let ctx = {
            let mut c = crate::tools::ToolsCtx::foreground(std::env::temp_dir(), "cn".into());
            c.interrupt = interrupt;
            c
        };
        let em = std::sync::Arc::new(RecEmitter::default());
        let cfg = Config::default();
        // 一轮俩 tool_call：sleep 30（执行中 cancel）+ echo（被跳过）——两者都须被补 result
        let tc = |name: &str, args: &str| json!({"id":"c","type":"function","function":{"name":name,"arguments":args}});
        let r = RoundResult {
            finish: FinishReason::ToolCalls, content: String::new(), reasoning: String::new(),
            assistant_message: json!({"role":"assistant","content":""}),
            tool_calls: vec![
                tc("bash", r#"{"command":"sleep 30","timeout_secs":300}"#),
                tc("bash", r#"{"command":"echo second"}"#),
            ],
            usage: None,
        };
        let round: Arc<dyn LlmRound> = Arc::new(FakeRound(Mutex::new(VecDeque::from(vec![Ok(r)]))));
        let mut messages = vec![json!({"role":"user","content":"hi"})];
        let em_turn = em.clone();
        let turn = tokio::spawn(async move {
            run_turn(round, &*em_turn, &cfg, &mut messages, &ctx, 100, "main").await
        });
        for _ in 0..200 {
            if em.tool_calls_len() >= 1 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        token.cancel();
        let res = turn.await.unwrap();
        assert!(res.content.contains("中断"), "got: {}", res.content);
        // 关键断言：history 里 assistant(tool_calls×2) 后紧跟 2 条 tool 消息（配对不变量）
        let tool_msgs: Vec<&Value> = res.history.iter().filter(|m| m["role"] == "tool").collect();
        assert_eq!(tool_msgs.len(), 2,
            "2 个 call 都应有配对 tool 消息(1 真实执行 + 1 补齐), got {}: {:#?}",
            tool_msgs.len(), res.history);
        // 执行中的那个：select cancel 分支返回「用户中断，已终止」或 dispatch 竞态先赢的输出
        let skipped = tool_msgs.iter().find(|m| m["tool_call_id"] == "c2");
        if let Some(sk) = skipped {
            assert_eq!(sk["content"], "用户中断，未执行",
                "被跳过的 call 须补「用户中断，未执行」: {}", sk["content"]);
        }
    }

    // 竞态分支（dispatch 先赢、break 前一个 call 已有 result）也须补齐剩余 call。
    #[tokio::test]
    async fn run_turn_interrupt_race_backfills_remaining() {
        let interrupt = crate::tools::InterruptHandle::new();
        let token = tokio_util::sync::CancellationToken::new();
        interrupt.set(token.clone());
        let ctx = {
            let mut c = crate::tools::ToolsCtx::foreground(std::env::temp_dir(), "cn".into());
            c.interrupt = interrupt;
            c
        };
        let em = std::sync::Arc::new(RecEmitter::default());
        let cfg = Config::default();
        // echo（快，dispatch 臂先赢）+ sleep（不该执行到）——echo 有真实 result，sleep 须补
        let tc = |name: &str, args: &str, id: &str| json!({"id":id,"type":"function","function":{"name":name,"arguments":args}});
        let r = RoundResult {
            finish: FinishReason::ToolCalls, content: String::new(), reasoning: String::new(),
            assistant_message: json!({"role":"assistant","content":""}),
            tool_calls: vec![
                tc("bash", r#"{"command":"echo fast"}"#, "c1"),
                tc("bash", r#"{"command":"sleep 30","timeout_secs":300}"#, "c2"),
            ],
            usage: None,
        };
        let round: Arc<dyn LlmRound> = Arc::new(FakeRound(Mutex::new(VecDeque::from(vec![Ok(r)]))));
        let mut messages = vec![json!({"role":"user","content":"hi"})];
        let em_turn = em.clone();
        let turn = tokio::spawn(async move {
            run_turn(round, &*em_turn, &cfg, &mut messages, &ctx, 100, "main").await
        });
        // 等 echo 完成（tool_result 已 emit）再 cancel → 走「返回时发现已 cancel」竞态分支
        for _ in 0..300 {
            let done = em.calls.lock().unwrap().iter().any(|(_, r, k)| k == "result" && r.contains("fast"));
            if done { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        token.cancel();
        let res = turn.await.unwrap();
        assert!(res.content.contains("中断"), "got: {}", res.content);
        let tool_msgs: Vec<&Value> = res.history.iter().filter(|m| m["role"] == "tool").collect();
        let ids: Vec<&str> = tool_msgs.iter().filter_map(|m| m["tool_call_id"].as_str()).collect();
        assert!(ids.contains(&"c1") && ids.contains(&"c2"),
            "c1(真实) + c2(补齐) 都须有 tool 消息, got {ids:?}");
        let c2 = tool_msgs.iter().find(|m| m["tool_call_id"] == "c2").unwrap();
        assert_eq!(c2["content"], "用户中断，未执行");
    }

    #[test]
    fn normalize_args_collapses_digit_runs() {
    assert_eq!(normalize_args(r#"mem history --seq 10798 | tail -10"#), r#"mem history --seq # | tail -#"#);
    assert_eq!(normalize_args("echo hi"), "echo hi"); // 无数字不变
    assert_eq!(normalize_args("v1.2.3"), "v#.#.#"); // 连续数字折叠成一个 #
    }

    #[test]
    fn iter_signature_order_invariant_and_sensitive() {
    let tc = |name: &str, args: &str| json!({"id":"c","type":"function","function":{"name":name,"arguments":args}});
    // 同名 + 只有数字变 → 同签名(螺旋判定的关键)
    assert_eq!(iter_signature(&[tc("bash", "--seq 1")]), iter_signature(&[tc("bash", "--seq 999")]));
    // 不同名 / 结构不同 → 不同签名
    assert_ne!(iter_signature(&[tc("bash", "echo a")]), iter_signature(&[tc("ls", "echo a")]));
    assert_ne!(iter_signature(&[tc("bash", "echo a")]), iter_signature(&[tc("bash", "echo b")]));
    }

    // ─── LLM HTTP 重试：分类 + 退避 + Retry-After 纯函数 ───
    #[test]
    fn classify_retries_transient_errors() {
        for s in [408, 429, 500, 502, 503, 504, 529] {
            match classify_http_error(s, "{}") {
                HttpErrClass::Retry { .. } => {}
                other => panic!("status {s} 应可重试，得到 {other:?}"),
            }
        }
    }

    #[test]
    fn classify_fail_4xx_with_chinese_hint() {
        let m = match classify_http_error(401, "") { HttpErrClass::Fail(m) => m, o => panic!("{o:?}") };
        assert!(m.contains("API Key"), "401 提示: {m}");
        let m = match classify_http_error(404, "") { HttpErrClass::Fail(m) => m, o => panic!("{o:?}") };
        assert!(m.contains("模型"), "404 提示: {m}");
        let m = match classify_http_error(413, "") { HttpErrClass::Fail(m) => m, o => panic!("{o:?}") };
        assert!(m.contains("过大"), "413 提示: {m}");
        let m = match classify_http_error(403, "") { HttpErrClass::Fail(m) => m, o => panic!("{o:?}") };
        assert!(m.contains("权限"), "403 提示: {m}");
    }

    #[test]
    fn classify_400_includes_body_snippet() {
        let m = match classify_http_error(400, "bad model name") { HttpErrClass::Fail(m) => m, o => panic!("{o:?}") };
        assert!(m.contains("400") && m.contains("bad model name"), "400 应带 body 片段: {m}");
    }

    #[test]
    fn classify_422_sensitive_gives_risk_control_hint() {
        // MiniMax 内容风控（如 "input new_sensitive (1026)"）：须报风控而非「请求参数错误」，
        // 并给恢复路径（重置会话/换措辞）。HttpRound 正常路径会先净化重试，此处兜底。
        let body = r#"{"error":{"message":"input new_sensitive (1026)","http_code":"422"}}"#;
        let m = match classify_http_error(422, body) { HttpErrClass::Fail(m) => m, o => panic!("{o:?}") };
        assert!(m.contains("风控"), "应报风控拦截: {m}");
        assert!(m.contains("换措辞"), "应给恢复路径: {m}");
        assert!(!m.contains("请求参数错误"), "不该误导为参数错误: {m}");
    }

    // ─── 内容风控净化重试：stub_tool_results（发送副本替换 tool 结果为省略桩）───

    fn tool_msg(id: &str, content: &str) -> Value {
        json!({ "role": "tool", "tool_call_id": id, "content": content })
    }

    #[test]
    fn stub_level0_returns_original() {
        let msgs = vec![json!({"role":"user","content":"q"}), tool_msg("c1", "结果A")];
        let out = stub_tool_results(&msgs, 0);
        assert_eq!(out[1]["content"], "结果A", "level 0 不净化");
    }

    #[test]
    fn stub_level1_replaces_newer_half_only() {
        // 4 个 tool 结果：level 1 净化较新的一半（后 2 个），前 2 个保留
        let msgs = vec![
            json!({"role":"user","content":"q"}),
            tool_msg("c1", "旧结果1"), tool_msg("c2", "旧结果2"),
            tool_msg("c3", "新结果3"), tool_msg("c4", "新结果4"),
        ];
        let out = stub_tool_results(&msgs, 1);
        assert_eq!(out[1]["content"], "旧结果1", "前半保留");
        assert_eq!(out[2]["content"], "旧结果2", "前半保留");
        assert_eq!(out[3]["content"], TOOL_RESULT_STUB, "后半净化");
        assert_eq!(out[4]["content"], TOOL_RESULT_STUB, "后半净化");
        // 配对不变量：role/tool_call_id 不动
        assert_eq!(out[3]["role"], "tool");
        assert_eq!(out[3]["tool_call_id"], "c3");
    }

    #[test]
    fn stub_level2_replaces_all_and_input_untouched() {
        let msgs = vec![tool_msg("c1", "A"), tool_msg("c2", "B")];
        let out = stub_tool_results(&msgs, 2);
        assert!(out.iter().all(|m| m["content"] == TOOL_RESULT_STUB), "全部净化");
        // 真身不动（发送副本语义）
        assert_eq!(msgs[0]["content"], "A");
        assert_eq!(msgs[1]["content"], "B");
    }

    #[test]
    fn stub_level3_also_stubs_tool_call_args() {
        // 搜索词本身（tool_call arguments）也是敏感源：level 3 连参数一起换桩
        let msgs = vec![
            json!({"role":"user","content":"q"}),
            json!({"role":"assistant","content":null,"tool_calls":[
                {"id":"c1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"mmx search 公网暴露 鉴权密码\"}"}}
            ]}),
            tool_msg("c1", "结果"),
        ];
        let out = stub_tool_results(&msgs, 3);
        assert_eq!(out[1]["tool_calls"][0]["function"]["arguments"], TOOL_CALL_ARGS_STUB,
            "参数须换桩: {}", out[1]["tool_calls"][0]["function"]["arguments"]);
        assert_eq!(out[2]["content"], TOOL_RESULT_STUB, "结果也须桩");
        // name/id 保留（配对不变量）
        assert_eq!(out[1]["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(out[1]["tool_calls"][0]["id"], "c1");
    }

    // ─── 隔离名单：污染源记一次，后续发送直接换桩（不再撞 422）───

    #[test]
    fn quarantine_roundtrip_stubs_on_next_send() {
        // 污染文本入名单 → apply_quarantine 命中换桩；干净文本不动
        let dir = tempfile::tempdir().unwrap();
        let poisoned = "搜索结果：公网暴露 鉴权密码的最佳实践……";
        let clean = "正常工具输出";
        quarantine_add(&[text_hash(poisoned)], dir.path());
        // 落盘恢复（模拟重启）
        *quarantine().lock().unwrap() = Default::default();
        quarantine_load(dir.path());
        let msgs = vec![
            json!({"role":"user","content":"q"}),
            tool_msg("c1", poisoned),
            tool_msg("c2", clean),
            json!({"role":"assistant","content":null,"tool_calls":[
                {"id":"c3","type":"function","function":{"name":"bash","arguments":"{\"command\":\"mmx search 公网暴露\"}"}}
            ]}),
        ];
        quarantine_add(&[text_hash("{\"command\":\"mmx search 公网暴露\"}")], dir.path());
        let (out, hits) = apply_quarantine(&msgs);
        assert_eq!(out[1]["content"], TOOL_RESULT_STUB, "命中名单的污染文本换桩");
        assert_eq!(out[2]["content"], clean, "干净文本不动");
        let args = out[3]["tool_calls"][0]["function"]["arguments"].as_str().unwrap();
        assert_eq!(args, TOOL_CALL_ARGS_STUB, "命中名单的参数换桩");
        assert_eq!(hits.len(), 2, "2 处命中");
        // 真身不动
        assert_eq!(msgs[1]["content"], poisoned);
        // 名单文件真实落盘
        let saved = std::fs::read_to_string(dir.path().join("quarantine.json")).unwrap();
        assert!(saved.contains(&text_hash(poisoned).to_string()), "落盘: {saved}");
    }

    #[test]
    fn collect_stubbed_hashes_matches_stub_range() {
        // level 1 后半、level 2 全部、level 3 含参数——与 stub_tool_results 范围一致
        let msgs = vec![
            tool_msg("c1", "A"), tool_msg("c2", "B"),
            json!({"role":"assistant","content":null,"tool_calls":[
                {"id":"c3","type":"function","function":{"name":"bash","arguments":"CMD"}}
            ]}),
        ];
        let l1: std::collections::HashSet<u64> = collect_stubbed_hashes(&msgs, 1).into_iter().collect();
        assert!(l1.contains(&text_hash("B")) && !l1.contains(&text_hash("A")), "level1 只含后半");
        let l2: std::collections::HashSet<u64> = collect_stubbed_hashes(&msgs, 2).into_iter().collect();
        assert!(l2.contains(&text_hash("A")) && l2.contains(&text_hash("B")));
        let l3: std::collections::HashSet<u64> = collect_stubbed_hashes(&msgs, 3).into_iter().collect();
        assert!(l3.contains(&text_hash("CMD")), "level3 含参数");
    }

    #[test]
    fn stub_level4_truncates_to_recent_two_turns() {
        // 回退截断：只保 system + 最近 2 个 user 起的段；中段孤儿 tool 剥除
        let msgs = vec![
            json!({"role":"system","content":"SYS"}),
            json!({"role":"user","content":"旧问题1"}),
            json!({"role":"assistant","content":null,"tool_calls":[
                {"id":"old1","type":"function","function":{"name":"bash","arguments":"{}"}}
            ]}),
            tool_msg("old1", "旧结果"),
            json!({"role":"assistant","content":"旧答1"}),
            json!({"role":"user","content":"新问题1"}),
            json!({"role":"assistant","content":"新答1"}),
            json!({"role":"user","content":"新问题2"}),
            json!({"role":"assistant","content":"新答2"}),
        ];
        let out = stub_tool_results(&msgs, 4);
        // system 保留 + 只剩最近 2 个 user 的段
        assert_eq!(out[0]["role"], "system");
        let users: Vec<&str> = out.iter().filter(|m| m["role"]=="user")
            .filter_map(|m| m["content"].as_str()).collect();
        assert_eq!(users, vec!["新问题1", "新问题2"], "只保最近 2 turn: {users:?}");
        // 剥掉中段不产生孤儿（old1 的 tool 在剥掉区间，assistant 也一起剥了）
        for m in out.iter() {
            if m["role"] == "tool" {
                let id = m["tool_call_id"].as_str().unwrap();
                let has_owner = out.iter().any(|a| a["role"]=="assistant" &&
                    a.get("tool_calls").and_then(|t| t.as_array())
                        .map(|arr| arr.iter().any(|c| c["id"]==id)).unwrap_or(false));
                assert!(has_owner, "level 4 截断后不得有孤儿 tool (id={id})");
            }
        }
    }

    #[test]
    fn classify_422_non_sensitive_keeps_param_error() {
        // 422 但非 sensitive：仍走「请求参数错误」通配（带 body 片段）
        let m = match classify_http_error(422, "some other validation") { HttpErrClass::Fail(m) => m, o => panic!("{o:?}") };
        assert!(m.contains("422") && m.contains("some other validation"), "非 sensitive 422 保持原格式: {m}");
    }

    #[test]
    fn classify_unknown_status_keeps_legacy_format() {
        let m = match classify_http_error(418, "teapot") { HttpErrClass::Fail(m) => m, o => panic!("{o:?}") };
        assert!(m.contains("418") && m.contains("teapot"), "未知 status 保留旧格式: {m}");
    }

    #[test]
    fn backoff_delay_grows_then_caps_at_8s() {
        use std::time::Duration;
        assert_eq!(backoff_delay(1), Duration::from_secs(1));
        assert_eq!(backoff_delay(2), Duration::from_secs(2));
        assert_eq!(backoff_delay(3), Duration::from_secs(4));
        assert_eq!(backoff_delay(4), Duration::from_secs(8));
        assert_eq!(backoff_delay(5), Duration::from_secs(8), "封顶 8s");
        let mut prev = Duration::ZERO;
        for a in 1..=6 {
            let d = backoff_delay(a);
            assert!(d >= prev, "attempt {a} 退避 {d:?} 小于前一次 {prev:?}");
            prev = d;
        }
    }

    #[test]
    fn retry_after_parses_seconds() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(reqwest::header::RETRY_AFTER, "5".parse().unwrap());
        assert_eq!(retry_after_secs(&h), Some(5));
    }

    #[test]
    fn retry_after_ignores_http_date() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(reqwest::header::RETRY_AFTER, "Wed, 21 Oct 2026 07:28:00 GMT".parse().unwrap());
        assert_eq!(retry_after_secs(&h), None, "HTTP-date 不解析，回落 backoff");
    }

    #[test]
    fn retry_after_missing_returns_none() {
        let h = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after_secs(&h), None);
    }

    // （usage 后端不再加工——见 run_turn：直接用最后一次服务器返回原值。字段路径测试覆盖在前端 attachTokenBadge。）

    #[tokio::test]
    async fn run_turn_usage_is_last_round_raw_value() {
        use crate::history::{spawn_writer, read_all};
        use crate::tools::ToolsCtx;
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"), 0);
        let mut ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        ctx.history = h.clone();
        // 两轮：第一轮 tool_calls（p=100/c=10/cache=80），第二轮 Stop（p=120/c=5/cache=90）
        // → 取最后轮服务器原值：prompt=120、completion=5、cached=90（mk_usage 构造真实嵌套结构）
        let mk_usage = |p: u64, c: u64, cache: u64| {
            Some(json!({"prompt_tokens": p, "completion_tokens": c, "prompt_tokens_details": {"cached_tokens": cache}}))
        };
        let tc = json!({"id":"c1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"echo hi\"}"}});
        let round: Arc<dyn LlmRound> = Arc::new(FakeRound(Mutex::new(VecDeque::from(vec![
            Ok(RoundResult { content:String::new(), reasoning:String::new(), tool_calls:vec![tc.clone()],
                finish:FinishReason::ToolCalls, usage:mk_usage(100, 10, 80),
                assistant_message:json!({"role":"assistant","content":null,"tool_calls":[tc]}) }),
            Ok(RoundResult { content:"done".into(), reasoning:String::new(), tool_calls:vec![],
                finish:FinishReason::Stop, usage:mk_usage(120, 5, 90),
                assistant_message:json!({"role":"assistant","content":"done"}) }),
        ]))));
        let em = RecEmitter::default();
        let cfg = Config::default();
        let mut msgs = vec![json!({"role":"user","content":"go"})];
        let res = run_turn(round, &em, &cfg, &mut msgs, &ctx, MAX_ITERS, "main").await;
        let u = res.usage.expect("应有最后轮 usage 原值");
        assert_eq!(u["prompt_tokens"], 120, "prompt = 最后轮");
        assert_eq!(u["completion_tokens"], 5, "completion = 最后轮（不累加）");
        assert_eq!(u["prompt_tokens_details"]["cached_tokens"], 90, "cached = 最后轮嵌套路径（不累加）");
        // 两条 assistant 事件各带该轮 usage（落盘细粒度）
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let evs = read_all(&dir.path().join("history"));
        let asst: Vec<_> = evs.iter().filter(|e| e.kind == "assistant").collect();
        assert_eq!(asst.len(), 2);
        assert_eq!(asst[0].data["usage"]["prompt_tokens"], 100, "第一轮落盘 usage");
        assert_eq!(asst[1].data["usage"]["prompt_tokens"], 120, "第二轮落盘 usage");
    }

    // Task 2: SSE parsing tests
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

    // Task 3: consume_sse tests
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

    // Task 3: user_message_with_attachments tests
    use crate::AttachmentRef;

    fn aref(path: &str, kind: &str) -> AttachmentRef {
        AttachmentRef { staged_path: path.into(), kind: kind.into() }
    }

    #[test]
    fn user_message_image_ref_no_inline_base64() {
        let m = user_message_with_attachments("看图", &[aref("C:/a.png", "image")]);
        assert_eq!(m["role"], "user");
        let content = m["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "看图");
        assert_eq!(content[1]["type"], "image_ref");
        assert_eq!(content[1]["path"], "C:/a.png");
        assert!(content[1].get("url").is_none(), "历史里不内联 base64");
    }

    #[test]
    fn user_message_binary_becomes_agent_note() {
        let m = user_message_with_attachments("", &[aref("C:/a.pdf", "pdf")]);
        let note = m["content"].as_array().unwrap().iter()
            .find(|p| p["type"] == "text").unwrap();
        assert!(note["text"].as_str().unwrap().contains("a.pdf"));
        assert!(note["text"].as_str().unwrap().contains("read/display"));
    }

    #[test]
    fn cap_doc_text_under_limit_unchanged() {
        let s = "x".repeat(100);
        assert_eq!(cap_doc_text(&s), s);
    }

    #[test]
    fn cap_doc_text_over_limit_truncates_with_marker() {
        // "字" 是 3 字节；DOC_TEXT_MAX=204800 非 3 的倍数 → 必须按字符边界裁
        let s = "字".repeat(DOC_TEXT_MAX + 1000);
        let capped = cap_doc_text(&s);
        assert!(capped.contains("已截断"), "缺截断标记");
        assert!(capped.len() < s.len(), "截断后反而更长?");
        let head_end = capped.find("\n\n[…已截断").expect("应找到标记起点");
        assert_eq!(head_end % 3, 0, "切点不在字符边界: {head_end}");
    }

    #[test]
    fn user_message_docx_inlines_extracted_text() {
        let p = crate::extract::build_minimal_docx(&["季度报告", "营收增长"]);
        let ps = p.to_string_lossy().to_string();
        let m = user_message_with_attachments("", &[aref(&ps, "docx")]);
        let note = m["content"].as_array().unwrap().iter()
            .find(|x| x["type"] == "text").unwrap();
        let t = note["text"].as_str().unwrap();
        assert!(t.contains("(docx)"), "应标注 kind: {t}");
        assert!(t.contains("季度报告"), "应内联抽取的文本: {t}");
        assert!(t.contains("营收增长"), "应含第二段: {t}");
        assert!(!t.contains("read/display"), "能抽取时不应回落指针 note");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn user_message_pdf_unextractable_falls_back() {
        // 真实非-PDF 文件按 pdf kind：pdf-extract 解析失败 → 回落指针 note
        let p = crate::extract::build_minimal_docx(&["not a pdf"]);
        let ps = p.to_string_lossy().to_string();
        let m = user_message_with_attachments("", &[aref(&ps, "pdf")]);
        let note = m["content"].as_array().unwrap().iter()
            .find(|x| x["type"] == "text").unwrap();
        let t = note["text"].as_str().unwrap();
        assert!(t.contains("无法抽取文本"), "应回落指针 note: {t}");
        assert!(t.contains("(pdf)"));
        let _ = std::fs::remove_file(&p);
    }

    // Task 3: expand_messages_for_send tests
    #[test]
    fn expand_plain_string_messages_unchanged() {
        let msgs = vec![json!({"role":"user","content":"hi"})];
        let out = expand_messages_for_send(&msgs).unwrap();
        assert_eq!(out[0]["content"], "hi"); // 纯文本原样，不展开
    }

    #[test]
    fn expand_image_ref_to_data_url() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("cat.png");
        std::fs::write(&p, b"png").unwrap();
        let abs = p.canonicalize().unwrap().to_string_lossy().to_string();
        let msgs = vec![json!({"role":"user","content":[
            {"type":"text","text":"看"},
            {"type":"image_ref","path": abs}
        ]})];
        let out = expand_messages_for_send(&msgs).unwrap();
        let parts = out[0]["content"].as_array().unwrap();
        assert_eq!(parts[1]["type"], "image_url");
        let url = parts[1]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"), "{url}");
    }

    #[test]
    fn expand_budget_guard_rejects_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.png");
        // 59MB 超过 58MB 预算
        std::fs::write(&p, vec![0u8; 59 * 1024 * 1024]).unwrap();
        let abs = p.canonicalize().unwrap().to_string_lossy().to_string();
        let msgs = vec![json!({"role":"user","content":[{"type":"image_ref","path": abs}]})];
        let err = expand_messages_for_send(&msgs).err().unwrap();
        assert!(err.contains("过大"), "{err}");
    }

    // ─── Task 4: run_turn 按 thread append assistant/tool_result 事件 ───
    #[tokio::test]
    async fn run_turn_appends_assistant_and_tool_events_main_thread() {
        use crate::history::{spawn_writer, read_all};
        use crate::tools::ToolsCtx;
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"), 0);
        let mut ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        ctx.history = h.clone();
        let round: Arc<dyn LlmRound> = Arc::new(FakeRound(Mutex::new(VecDeque::from(vec![
            Ok(tool_round("bash", r#"{"command":"echo hi"}"#, "c1")),
            Ok(RoundResult { content:"完成".into(), reasoning:String::new(), tool_calls:vec![],
                finish:FinishReason::Stop, usage:None, assistant_message:json!({"role":"assistant","content":"完成"}) }),
        ]))));
        let em = RecEmitter::default();
        let cfg = Config::default();
        let mut msgs = vec![json!({"role":"user","content":"hi"})];
        let _ = run_turn(round, &em, &cfg, &mut msgs, &ctx, MAX_ITERS, "main").await;
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let evs = read_all(&dir.path().join("history"));
        let kinds: Vec<&str> = evs.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, vec!["assistant", "tool_result", "assistant"], "应落 3 条事件");
        assert_eq!(evs[0].thread, "main");
        assert_eq!(evs[0].data["tool_calls"][0]["id"], "c1");
        assert_eq!(evs[1].data["call_id"], "c1");
        assert!(evs[1].data["result"].as_str().unwrap().contains("hi"));
        assert_eq!(evs[2].data["content"], "完成");
    }

    #[tokio::test]
    async fn run_turn_appends_with_agent_thread() {
        use crate::history::{spawn_writer, read_all};
        use crate::tools::ToolsCtx;
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"), 0);
        let mut ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        ctx.history = h.clone();
        let round: Arc<dyn LlmRound> = Arc::new(FakeRound(Mutex::new(VecDeque::from(vec![
            Ok(RoundResult { content:"子代理done".into(), reasoning:String::new(), tool_calls:vec![],
                finish:FinishReason::Stop, usage:None, assistant_message:json!({"role":"assistant","content":"子代理done"}) }),
        ]))));
        let em = RecEmitter::default();
        let cfg = Config::default();
        let mut msgs = vec![json!({"role":"user","content":"do"})];
        let _ = run_turn(round, &em, &cfg, &mut msgs, &ctx, MAX_ITERS, "agent:7").await;
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let evs = read_all(&dir.path().join("history"));
        assert_eq!(evs[0].thread, "agent:7", "子代理事件 thread=agent:N");
    }

    // ─── #226 C2 回归：打满 max_iters 的兜底文本必须流式给 emitter ───
    #[tokio::test]
    async fn run_turn_cap_emits_fallback_text_to_emitter() {
        use crate::tools::ToolsCtx;
        let dir = tempfile::tempdir().unwrap();
        let h = crate::history::spawn_writer(dir.path().join("history"), 0);
        let mut ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        ctx.history = h.clone();
        // 每轮都工具调用、永不 Stop → 3 轮打满走兜底（deque 给 5 个，防 FakeRound 空 deque 回退到 Stop）
        let round: Arc<dyn LlmRound> = Arc::new(FakeRound(Mutex::new(VecDeque::from(vec![
            Ok(tool_round("bash", r#"{"command":"echo z"}"#, "c")),
            Ok(tool_round("bash", r#"{"command":"echo z"}"#, "c")),
            Ok(tool_round("bash", r#"{"command":"echo z"}"#, "c")),
            Ok(tool_round("bash", r#"{"command":"echo z"}"#, "c")),
            Ok(tool_round("bash", r#"{"command":"echo z"}"#, "c")),
        ]))));
        let em = RecEmitter::default();
        let cfg = Config::default();
        let mut msgs = vec![json!({"role":"user","content":"go"})];
        let res = run_turn(round, &em, &cfg, &mut msgs, &ctx, 3, "main").await;
        // 兜底文本进了返回 content（落 history，重启能见）
        assert!(res.content.contains("工具调用上限"), "got: {}", res.content);
        // C2 关键：也流式给了 emitter——修前 em.contents 为空 → 前端 live wrap "工具卡在、最终文本不在"
        let contents = em.contents.lock().unwrap().join("");
        assert!(contents.contains("工具调用上限"), "兜底文本应流式给 emitter: {contents}");
    }

    // ─── usage 语义：取最后一次服务器返回原值（不累加不重组）；命中 ≤ 输入 ───
    #[tokio::test]
    async fn run_turn_usage_is_last_round_with_nested_cache_field() {
        use crate::tools::ToolsCtx;
        let dir = tempfile::tempdir().unwrap();
        let h = crate::history::spawn_writer(dir.path().join("history"), 0);
        let mut ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        ctx.history = h.clone();
        // 两轮：r1 工具调用（继续循环），r2 Stop（结束）。各带不同 usage（真实嵌套结构）。
        let u1 = json!({"prompt_tokens":1000,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":800}});
        let u2 = json!({"prompt_tokens":1200,"completion_tokens":30,"prompt_tokens_details":{"cached_tokens":1000}});
        let tc = json!({"id":"c1","type":"function","function":{"name":"bash","arguments":r#"{"command":"echo x"}"#}});
        let round: Arc<dyn LlmRound> = Arc::new(FakeRound(Mutex::new(VecDeque::from(vec![
            Ok(RoundResult { content: String::new(), reasoning: String::new(), tool_calls: vec![tc.clone()],
                finish: FinishReason::ToolCalls, usage: Some(u1),
                assistant_message: json!({"role":"assistant","content":null,"tool_calls":[tc]}) }),
            Ok(RoundResult { content: "完成".into(), reasoning: String::new(), tool_calls: vec![],
                finish: FinishReason::Stop, usage: Some(u2),
                assistant_message: json!({"role":"assistant","content":"完成"}) }),
        ]))));
        let em = RecEmitter::default();
        let cfg = Config::default();
        let mut msgs = vec![json!({"role":"user","content":"go"})];
        let res = run_turn(round, &em, &cfg, &mut msgs, &ctx, 5, "main").await;
        let u = res.usage.expect("应有最后轮 usage 原值");
        // 取最后轮 u2，不累加不重组——角标 = 服务器最后一次返回
        assert_eq!(u["prompt_tokens"], 1200, "prompt = 最后轮（不累加）");
        assert_eq!(u["completion_tokens"], 30, "completion = 最后轮（不累加）");
        assert_eq!(u["prompt_tokens_details"]["cached_tokens"], 1000, "cached = 最后轮嵌套路径（不累加）");
        // 命中是输入子集，恒 ≤ 输入
        assert!(u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap() <= u["prompt_tokens"].as_u64().unwrap(),
            "命中 ≤ 输入（物理不变量）");
    }
}
