//! v2 context 重建：history 尾段（最后 marker 之后 · main 线程）→ LLM messages。
//! 每轮 turn 开头由 driver 调 build_messages 重建；turn 内 run_turn 仍 mutate messages，但真相在 history。
//! P1 铁律②配对不变量、③marker(dream|reset) 重建边界 都在这里。
//! (cap 已移除:dream marker 当 cut,context = 前沿之后全部事件,不限条数;dream 涨大时再触发推进前沿。)
use crate::history::HistoryEvent;
use crate::AttachmentRef;
use serde_json::{json, Value};
use std::path::Path;

/// 子代理结算注入主上下文的渲染护栏（定值，非配置——防浪堤不是调优旋钮）。
/// 真身（jsonl 落盘 / dream 蒸馏输入 / registry status）永远全文；只有发给主 LLM 的工作集视图有界。
/// 正常产出（≤128KB≈3-6 万 token）不可见；防的是病态洪水单发打爆主回合。
const SUBAGENT_SUMMARY_RENDER_CAP: usize = 128 * 1024;

/// 4 个 pinned 块（spec §3）。合并成 1 条 system 消息（API 安全；MEMORY 逐轮重读可变）。
#[derive(Clone, Debug)]
pub struct PinnedBlocks {
    pub system_prompt: String,
    pub soul: Option<String>,
    pub agent: Option<String>,
    pub memory: Option<String>,
}

/// 运行环境提示：bash 工具跑 POSIX sh（非 cmd）。bash-green-sh 重写后必须告诉 LLM 用 sh 语法，
/// 否则吐 cmd 内建（dir/findstr/type/...）在新 sh 下失败。spec D10。注入 system 消息顶部（紧跟 system_prompt）。
fn shell_env_hint() -> &'static str {
    if cfg!(windows) {
        "# 运行环境\n\
         - 平台 Windows（绿色桌面 app）。bash 工具实际执行 **POSIX sh**（自带 busybox-w32，`sh -c \"<命令>\"`），**不是 cmd / PowerShell**。\n\
         - 工作目录 = workspace 根；输出 UTF-8。\n\
         - 用 POSIX 命令，勿用 cmd 内建：`ls`(非 dir)、`grep`(非 findstr)、`cat`(非 type)、`cp/mv/rm`(非 copy/move/del)、`$VAR`(非 %VAR%)。\n\
         - 仅 POSIX sh：不支持 `${var,,}`、`shopt`、`declare` 数组、`<(...)` 进程替换、`[[ ]]`（用 `[ ]`）。\n\
         - 命令作为单个 sh 脚本传入：引号 / 管道 / `$()` / `&&` 直接写，无需 cmd 转义；路径正/反斜杠均可。"
    } else {
        "# 运行环境\n\
         - 平台 macOS/Linux。bash 工具执行系统 **/bin/sh**（POSIX sh，`sh -c \"<命令>\"`）。\n\
         - 工作目录 = workspace 根。\n\
         - 仅 POSIX sh：不支持 bash-ism（`${var,,}`、`shopt`、`declare` 数组、`<(...)` 进程替换、`[[ ]]` 用 `[ ]`）。"
    }
}

impl PinnedBlocks {
    pub fn to_messages(&self) -> Vec<Value> {
        let mut combined = format!("{}\n\n{}", self.system_prompt.trim_end(), shell_env_hint());
        for (tag, body) in [
            ("# 灵魂（SOUL.md）", &self.soul),
            ("# 工具与规范（AGENT.md）", &self.agent),
            ("# 记忆索引（MEMORY.md）", &self.memory),
        ] {
            if let Some(b) = body {
                if !b.trim().is_empty() {
                    combined.push_str("\n\n");
                    combined.push_str(tag);
                    combined.push('\n');
                    combined.push_str(b);
                }
            }
        }
        vec![json!({ "role": "system", "content": combined })]
    }
}

/// 从 cache 读 SOUL/AGENT/MEMORY（人格/规范/记忆索引均归 cache，与用户 workspace 分离）；
/// 读不到（None）则跳过该块（§13.1 优雅降级）。
pub fn load_pinned(cache: &Path, system_prompt: &str) -> PinnedBlocks {
    let read = |name: &str| std::fs::read_to_string(cache.join(name)).ok().filter(|s| !s.trim().is_empty());
    PinnedBlocks {
        system_prompt: system_prompt.to_string(),
        soul: read("SOUL.md"),
        agent: read("AGENT.md"),
        memory: read("MEMORY.md"),
    }
}

/// 取最新的 marker(dream 或 reset)的 until_seq —— context 重建边界 = dream 整理前沿。
///
/// dream marker.until_seq = dream 整理到哪;reset marker.until_seq = idle dream 清空到哪
/// (reset 写入时本就取自最后 dream 的 until_seq,见 mem_dream::write_reset_marker)。两者同源,
/// 都当 cut:context 只含「整理前沿之后」的事件,之前的老对话已被 dream 压进 MEMORY.md。
///
/// 仍认 data["until_seq"](F1 P0:marker.seq > until_seq,认 until_seq 才对)。
fn last_marker_until_seq(events: &[HistoryEvent]) -> Option<u64> {
    events.iter()
        .filter(|e| e.kind == "marker")
        .filter_map(|e| e.data.get("until_seq").and_then(|v| v.as_u64()))
        .max()
}

/// history → LLM messages。`_cap_after_dream` 保留兼容(handle_event 仍传)但不再使用。
///
/// cut = 最新 marker(dream/reset)的 until_seq;context = cut 之后的全部 main 事件,不限条数。
/// 每次新对话都 append 进来;context 涨到 token 上限时 dream 再触发、推进 until_seq → context 缩。
/// (旧 dream_cap_turns cap 已移除:它让 dream marker 永久生效时把 context 砍到 3 条,agent 失忆。)
pub fn build_messages(events: &[HistoryEvent], pinned: &PinnedBlocks, _cap_after_dream: usize) -> Vec<Value> {
    let cut = last_marker_until_seq(events);
    let after_cut = |e: &HistoryEvent| match cut { Some(s) => e.seq > s, None => true };
    let mut msgs: Vec<Value> = events.iter()
        .filter(|e| e.thread == "main" && after_cut(e))
        .filter_map(|e| event_to_message(e))
        .collect();
    strip_unpaired_tool_calls(&mut msgs);
    drop_trailing_orphan(&mut msgs);
    let mut out = pinned.to_messages();
    out.append(&mut msgs);
    out
}

/// 剥除发送视图里的**双向**未配对 tool 交互（自愈层，只动视图不改盘，幂等）：
/// ① assistant 侧：某 call 无对应 tool 消息时剥掉该 call，全剥空则整条转普通 assistant——
///    中断/崩溃留下的「assistant(tool_calls) 无配对 tool」中间孤儿会让 MiniMax 报 400 (2013)；
///    **name 为空的畸形 call 也剥**（M3 线上复现：空 name 通过不了服务端 tool_calls 结构校验，
///    即使配对完整也 2013）；
/// ② tool 侧：call_id 为空、或不在任何「合法」（id+name 齐全）assistant call 里的 tool 消息整条剥掉。
/// 注：末尾孤儿仍由 drop_trailing_orphan 整条丢弃（更彻底）。
pub(crate) fn strip_unpaired_tool_calls(msgs: &mut Vec<Value>) {
    // 配对锚点：assistant 侧全部「合法」call（id 与 function.name 都非空）
    let mut called: std::collections::HashSet<String> = Default::default();
    for m in msgs.iter() {
        if m["role"] != "assistant" { continue; }
        if let Some(tcs) = m.get("tool_calls").and_then(|t| t.as_array()) {
            for c in tcs {
                let id = c["id"].as_str().unwrap_or("");
                let name = c["function"]["name"].as_str().unwrap_or("");
                if !id.is_empty() && !name.is_empty() {
                    called.insert(id.to_string());
                }
            }
        }
    }
    // 先收全部 tool_call_id（每 id 计数：合法时恰 1 个 tool 消息配 1 个 call）
    let mut answered: std::collections::HashMap<String, usize> = Default::default();
    for m in msgs.iter() {
        if m["role"] == "tool" {
            if let Some(id) = m["tool_call_id"].as_str() {
                *answered.entry(id.to_string()).or_insert(0) += 1;
            }
        }
    }
    // ① 剥 assistant 侧无应答/畸形（空 name）的 call
    for m in msgs.iter_mut() {
        if m["role"] != "assistant" { continue; }
        let Some(tcs) = m.get_mut("tool_calls").and_then(|t| t.as_array_mut()) else { continue };
        let before = tcs.len();
        tcs.retain(|c| {
            let id = c["id"].as_str().unwrap_or("");
            let name = c["function"]["name"].as_str().unwrap_or("");
            !id.is_empty() && !name.is_empty() && answered.get(id).copied().unwrap_or(0) >= 1
        });
        if tcs.len() != before && tcs.is_empty() {
            // 全剥空：转普通 assistant（content 保留原值，可能为 null → 给空串防 API 拒空 content）
            if let Some(obj) = m.as_object_mut() {
                if obj.get("content").map(|c| c.is_null()).unwrap_or(true) {
                    obj.insert("content".into(), json!(""));
                }
                obj.remove("tool_calls");
            }
        }
    }
    // ② 剥 tool 侧孤儿：空 call_id 或配对的 call 不存在
    msgs.retain(|m| {
        if m["role"] != "tool" { return true; }
        let id = m["tool_call_id"].as_str().unwrap_or("");
        !id.is_empty() && called.contains(id)
    });
}

/// 末尾若剩带 tool_calls 但无后续 tool 的 assistant（崩溃/中断尾巴）→ 丢弃（§6 配对不变量）。
fn drop_trailing_orphan(msgs: &mut Vec<Value>) {
    while let Some(last) = msgs.last() {
        let is_orphan = last["role"] == "assistant"
            && last.get("tool_calls").and_then(|t| t.as_array()).map(|a| !a.is_empty()).unwrap_or(false);
        if is_orphan { msgs.pop(); } else { break; }
    }
}

/// prefix 非空时用 `<prefix>...</prefix>` 标签包裹拼到 text 前（标签内保留前缀原换行，
/// 标签后紧跟 text 不额外换行）；空串原样返回。供 user-role 消息 content 前拼接（发给 LLM）。
/// 例：prefix="请用英文\n回答"，text="你好" → "<prefix>请用英文\n回答</prefix>你好"
fn with_prefix(prefix: &str, text: &str) -> String {
    if prefix.trim().is_empty() {
        text.to_string()
    } else {
        format!("<prefix>{prefix}</prefix>{text}")
    }
}

fn event_to_message(e: &HistoryEvent) -> Option<Value> {
    match e.kind.as_str() {
        "user" => {
            let text = e.data.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let prefix = e.data.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
            let text = with_prefix(prefix, text);
            let atts: Vec<AttachmentRef> = e.data.get("attachments")
                .and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
            Some(crate::llm::user_message_with_attachments(&text, &atts))
        }
        "assistant" => {
            let content = e.data.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let tcs = e.data.get("tool_calls").cloned();
            let mut m = serde_json::Map::new();
            m.insert("role".into(), json!("assistant"));
            match tcs.and_then(|t| t.as_array().map(|a| a.to_vec())).filter(|a| !a.is_empty()) {
                Some(calls) => {
                    // tool_calls 在场时 content 强制 null（OpenAI 合规，与 llm.rs consume_sse 对称）。
                    // history 落盘保留原文 content（llm.rs:238 用 resp.content）供 mem CLI drill 显示；
                    // 但发给 API 时必须 null——否则 assistant(content=非空, tool_calls=[...]) 违反 spec，
                    // MiniMax 误判为已回答完毕、后续 tool 失去 parent → 工具上下文断（重启后拆轮 bug #1）。
                    m.insert("content".into(), Value::Null);
                    m.insert("tool_calls".into(), Value::Array(calls));
                }
                None => { m.insert("content".into(), json!(content)); }
            }
            Some(Value::Object(m))
        }
        "tool_result" => {
            let call_id = e.data.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
            let result = e.data.get("result").and_then(|v| v.as_str()).unwrap_or("");
            Some(json!({ "role": "tool", "tool_call_id": call_id, "content": result }))
        }
        "subagent_result" => {
            let prefix = e.data.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
            // agent_id 构造器存字符串（JobId），历史版本曾按 u64 读 → 恒渲染 #0 的存量 bug，此处双兼容。
            let id = e
                .data
                .get("agent_id")
                .map(|v| {
                    v.as_str()
                        .map(|s| s.to_string())
                        .or_else(|| v.as_u64().map(|n| n.to_string()))
                        .unwrap_or_else(|| "0".into())
                })
                .unwrap_or_else(|| "0".into());
            let summary = e.data.get("summary").and_then(|v| v.as_str()).unwrap_or("");
            let refs = e.data.get("ref").and_then(|v| v.as_str()).unwrap_or("");
            // 存量 jsonl 事件缺 ok → 按「完成」处理（向后兼容，不迁移）。
            let ok = e.data.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
            // 渲染层护栏：真身（jsonl/dream/registry）永远全文，只有发给主 LLM 的工作集视图有界。
            let capped = crate::tools::truncate(
                summary,
                SUBAGENT_SUMMARY_RENDER_CAP,
                "…（已截断，完整产出可用 subagent status 查看）",
            );
            let state = if ok { "完成" } else { "失败" };
            Some(json!({ "role": "user", "content": with_prefix(prefix, &format!("[子代理 #{id} {state}] {capped} [完整对话: {refs}]")) }))
        }
        "external" => {
            let prefix = e.data.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
            let what = e.data.get("what").and_then(|v| v.as_str()).unwrap_or("");
            let path = e.data.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let content = if path.is_empty() {
                format!("[外部] {what}")
            } else {
                format!("[外部] {what} {path}")
            };
            Some(json!({ "role": "user", "content": with_prefix(prefix, &content) }))
        }
        "edit" | "marker" => None, // 记录但不回放进 context
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryEvent;
    use serde_json::json;

    fn pinned(sys: &str) -> PinnedBlocks {
        PinnedBlocks { system_prompt: sys.into(), soul: None, agent: None, memory: None }
    }
    fn tc(id: &str) -> serde_json::Value {
        json!({"id":id,"type":"function","function":{"name":"bash","arguments":"{}"}})
    }

    fn seq_events(kinds: &[&str]) -> Vec<HistoryEvent> {
        // 造一条主线程事件序列，每条 seq 递增；user/assistant/tool_result/marker 按需
        let mut evs = Vec::new();
        let mut seq = 0u64;
        let mut ts = 1000u64;
        for k in kinds {
            let ev = match *k {
                "user" => HistoryEvent::user(ts, "main", &format!("u{seq}"), &[]),
                "assistant" => HistoryEvent::assistant(ts, "main", &format!("a{seq}"), "", vec![]),
                "assistant_tc" => HistoryEvent::assistant(ts, "main", "", "", vec![tc(&format!("c{seq}"))]),
                "tool" => HistoryEvent::tool_result(ts, "main", "bash", "ok", &format!("c{}", seq.saturating_sub(1))),
                "external" => HistoryEvent::external(ts, "拖入", "x.pdf", None),
                "subagent_result" => HistoryEvent::subagent_result(ts, "1", "完成X", "thread=agent:1 seq[0,3]", true),
                "marker_dream" => HistoryEvent::marker(ts, "dream", seq),
                "marker_reset" => HistoryEvent::marker(ts, "reset", seq),
                other => panic!("unknown kind {other}"),
            };
            let mut ev = ev;
            ev.seq = seq;
            evs.push(ev);
            seq += 1; ts += 1000;
        }
        evs
    }

    #[test]
    fn user_event_becomes_user_message() {
        let evs = seq_events(&["user"]);
        let m = build_messages(&evs, &pinned("S"), 50);
        assert_eq!(m[0]["role"], "system");
        assert_eq!(m[1]["role"], "user");
    }

    #[test]
    fn assistant_with_toolcalls_emits_tool_calls() {
        let evs = seq_events(&["user", "assistant_tc", "tool", "assistant"]);
        let m = build_messages(&evs, &pinned("S"), 50);
        let asst = m.iter().find(|x| x["role"] == "assistant" && x.get("tool_calls").is_some()).unwrap();
        assert_eq!(asst["tool_calls"][0]["id"], "c1");
        assert_eq!(asst["content"], serde_json::Value::Null);
    }

    #[test]
    fn assistant_with_toolcalls_forces_null_content_even_when_history_has_text() {
        // 回归（bug #1 重启拆轮）：llm.rs run_turn 落 history 用 resp.content（同条带 tool_calls 时可能非空）。
        // rebuild 时 event_to_message 必须强制 content=null（OpenAI 合规），否则 MiniMax 收到
        // assistant(content=非空, tool_calls=[...]) 违反 spec → 工具上下文断 → 多轮工具调用被拆。
        let mut ev = HistoryEvent::assistant(1000, "main", "我先看一下", "", vec![
            json!({"id":"c1","type":"function","function":{"name":"bash","arguments":"{}"}})
        ]);
        ev.seq = 1;
        let m = event_to_message(&ev).unwrap();
        assert_eq!(m["role"], "assistant");
        assert!(m.get("tool_calls").is_some(), "应有 tool_calls");
        assert_eq!(m["content"], serde_json::Value::Null,
            "tool_calls 在场时 content 必须强制 null（不管 history 原文），实际: {}", m["content"]);
    }

    #[test]
    fn marker_window_only_after_last_marker() {
        // reset marker 之前的事件不进 context；之后才进（v2: 只 reset cut，dream marker 不 cut）
        let evs = seq_events(&["user", "marker_reset", "user", "assistant"]);
        let m = build_messages(&evs, &pinned("S"), 50);
        let users: Vec<&serde_json::Value> = m.iter().filter(|x| x["role"]=="user").collect();
        assert_eq!(users.len(), 1, "marker 前的 user 不应进 context");
    }

    #[test]
    fn reset_marker_same_boundary_as_dream() {
        // reset 与 dream 都是重建边界（marker kind 不分 dream/reset）
        let evs = seq_events(&["user", "marker_reset", "user", "assistant"]);
        let m = build_messages(&evs, &pinned("S"), 50);
        let users: Vec<&serde_json::Value> = m.iter().filter(|x| x["role"]=="user").collect();
        assert_eq!(users.len(), 1, "reset 后只应剩 1 个 user");
    }

    #[test]
    fn thread_agent_filtered_out() {
        let mut evs = seq_events(&["user"]);
        // 加一条 agent:N 事件（thread 不同）
        let mut a = HistoryEvent::assistant(5000, "agent:1", "子代理输出", "", vec![]);
        a.seq = 99;
        evs.push(a);
        let m = build_messages(&evs, &pinned("S"), 50);
        assert!(m.iter().all(|x| x["role"] != "assistant" || x["content"] != json!("子代理输出")),
            "thread=agent:N 不应进主 context");
    }

    #[test]
    fn truncate_does_not_split_pair() {
        // 不截断(无 cap):全部事件保留,tool_result 必有 owner assistant(配对不变量)
        let mut kinds = Vec::new();
        for _ in 0..52 {
            kinds.push("user"); kinds.push("assistant_tc"); kinds.push("tool");
        }
        kinds.push("user"); kinds.push("assistant"); // 最后一个完整 turn（无 tool_call）
        let evs = seq_events(&kinds);
        let m = build_messages(&evs, &pinned("S"), 50);
        // 不应出现「role:tool 但前面无对应 assistant.tool_calls」的孤儿
        for (i, x) in m.iter().enumerate() {
            if x["role"] == "tool" {
                let call_id = x["tool_call_id"].as_str().unwrap_or("");
                let has_owner = m[..i].iter().rev().any(|a|
                    a["role"]=="assistant" && a.get("tool_calls").and_then(|t| t.as_array())
                        .map(|arr| arr.iter().any(|c| c["id"]==call_id)).unwrap_or(false));
                assert!(has_owner, "孤儿 tool_result（call_id={call_id}），配对不变量被破坏");
            }
        }
    }

    #[test]
    fn drop_trailing_orphan_toolcall() {
        // P1 铁律②：末尾 assistant 带 tool_calls 但无后续 tool → 丢弃（崩溃/中断尾巴）
        let evs = seq_events(&["user", "assistant_tc"]); // assistant_tc 后无 tool
        let m = build_messages(&evs, &pinned("S"), 50);
        assert!(m.iter().all(|x| !(x["role"]=="assistant" && x.get("tool_calls").is_some())),
            "末尾孤儿 tool_call assistant 必须丢弃");
    }

    // ─── 中间孤儿自愈：未配对 tool_calls 剥除（fix/interrupt-orphan-toolcalls 400 回归）───

    #[test]
    fn mid_orphan_tool_call_stripped() {
        // 线上 400 (2013) 复现：中断留下 assistant(tc) 无 tool_result，之后又有正常 tool 交互。
        // 手工构造（seq_events 的 call_id 由 seq 推导，两组拼接会撞 id）：孤儿 orphan1 + 配对 ok1。
        let mk = |seq: u64, kind: &str| -> HistoryEvent {
            let mut e = match kind {
                "user" => HistoryEvent::user(seq, "main", &format!("u{seq}"), &[]),
                "assistant" => HistoryEvent::assistant(seq, "main", &format!("a{seq}"), "", vec![]),
                "assistant_orphan" => HistoryEvent::assistant(seq, "main", "", "", vec![tc("orphan1")]),
                "assistant_ok" => HistoryEvent::assistant(seq, "main", "", "", vec![tc("ok1")]),
                _ => HistoryEvent::tool_result(seq, "main", "bash", "ok", "ok1"),
            };
            e.seq = seq;
            e
        };
        let evs = vec![
            mk(1, "user"),
            mk(2, "assistant_orphan"), // 中断孤儿：无 tool_result
            mk(3, "assistant"),        // 兜底「用户中断」文本
            mk(4, "user"),
            mk(5, "assistant_ok"),     // 正常 tool 交互
            mk(6, "tool"),             // call_id=ok1
            mk(7, "assistant"),
        ];
        let m = build_messages(&evs, &pinned("S"), 50);
        // 唯一保留 tool_calls 的 assistant 必须是配对过的 ok1（孤儿 orphan1 被剥）
        let with_tc: Vec<&Value> = m.iter()
            .filter(|x| x["role"]=="assistant" && x.get("tool_calls").is_some()).collect();
        assert_eq!(with_tc.len(), 1, "只应剩 1 个带 tool_calls 的 assistant: {m:#?}");
        assert_eq!(with_tc[0]["tool_calls"][0]["id"], "ok1", "孤儿 orphan1 必须被剥掉");
        for x in m.iter() {
            if x["role"] == "tool" {
                assert_eq!(x["tool_call_id"].as_str().unwrap(), "ok1", "只应有配对 call 的 tool 消息");
            }
        }
    }

    #[test]
    fn orphan_assistant_all_stripped_becomes_plain() {
        // 孤儿 assistant 的 tool_calls 全剥空 → 转普通 assistant（content null → 空串，防 API 拒空）
        let mut evs = seq_events(&["user", "assistant_tc"]); // 末尾孤儿（drop_trailing 会整条删）
        // 在其后追加正常 turn，让孤儿变「中间」孤儿
        let mut more = seq_events(&["user", "assistant"]);
        evs.append(&mut more);
        let m = build_messages(&evs, &pinned("S"), 50);
        assert!(m.iter().all(|x| x.get("tool_calls").is_none()),
            "中间孤儿剥空后不应再有 tool_calls: {m:#?}");
        // 剥空那条的 content 不该是 null（空串兜底）
        for x in m.iter() {
            if x["role"] == "assistant" {
                assert!(!x["content"].is_null(), "剥空后 content 须非 null: {x}");
            }
        }
    }

    #[test]
    fn orphan_tool_message_with_empty_call_id_stripped() {
        // 2026-09-11 线上 400 (2013) 复现：M3 吐 name/id 双空畸形 call，dispatch 写下
        // call_id="" 的 tool_result。旧自愈只剥 assistant 侧 call、不剥 tool 侧消息 →
        // 空 id tool 消息常驻发送视图，每次 rebuild 都 400 死循环。双向清理后必须自愈，
        // 且同序列里后续的「正常配对」不受误伤。
        let malformed_call = serde_json::json!({
            "id": "", "type": "function",
            "function": {"name": "", "arguments": "{\"path\":\"render/x.png\"}"}
        });
        let mut evs = vec![
            HistoryEvent::user(1, "main", "直接display", &[]),
            HistoryEvent::assistant(2, "main", "", "", vec![malformed_call]),
            HistoryEvent::tool_result(3, "main", "", "未知工具: ", ""),
        ];
        let mut good = seq_events(&["assistant_tc", "tool", "assistant"]);
        evs.append(&mut good);
        let m = build_messages(&evs, &pinned("S"), 50);
        assert!(m.iter().all(|x| x["role"] != "tool" || x["tool_call_id"].as_str().unwrap_or("") != ""),
            "空 call_id 的 tool 消息必须剥除: {m:#?}");
        let bad = m.iter().find(|x| x["role"] == "assistant" && x["content"] == "").unwrap();
        assert!(bad.get("tool_calls").is_none(), "空 id call 须从 assistant 剥除: {bad}");
        // 后续正常配对恰好保留 1 组（ok1）
        let tools: Vec<&Value> = m.iter().filter(|x| x["role"] == "tool").collect();
        assert_eq!(tools.len(), 1, "正常配对的 tool 消息应保留: {m:#?}");
    }

    #[test]
    fn empty_name_call_with_answer_stripped() {
        // 2026-09-13 线上 400 (2013) 复现：M3 吐空 name call，合成 id 让配对"成立"，
        // 但 assistant 回显里的 name:"" 过不了服务端结构校验 → 每轮 400。
        // strip 须把空 name call 与其 tool_result 一并剥除，assistant 转普通空消息。
        let malformed_call = serde_json::json!({
            "id": "call_synth_0", "type": "function",
            "function": {"name": "", "arguments": "{\"path\":\"render/x.png\"}"}
        });
        let mut evs = vec![
            HistoryEvent::user(1, "main", "https://github.com/...", &[]),
            HistoryEvent::assistant(2, "main", "", "", vec![malformed_call]),
            HistoryEvent::tool_result(3, "main", "", "工具名为空（上一条 tool_call 是畸形输出…）", "call_synth_0"),
        ];
        let mut good = seq_events(&["assistant_tc", "tool", "assistant"]);
        evs.append(&mut good);
        let m = build_messages(&evs, &pinned("S"), 50);
        assert!(m.iter().all(|x| x["role"] != "tool" || x["tool_call_id"].as_str().unwrap_or("") != "call_synth_0"),
            "畸形 call 的 tool_result 须剥除: {m:#?}");
        let bad = m.iter().find(|x| x["role"] == "assistant" && x["content"] == "").unwrap();
        assert!(bad.get("tool_calls").is_none(), "空 name call 须从 assistant 剥除: {bad}");
        // 正常配对恰好保留 1 组
        let tools: Vec<&Value> = m.iter().filter(|x| x["role"] == "tool").collect();
        assert_eq!(tools.len(), 1, "正常配对应保留: {m:#?}");
    }

    #[test]
    fn paired_tool_calls_untouched() {
        // 正常配对序列不受 strip 影响（防误伤回归）
        let evs = seq_events(&["user", "assistant_tc", "tool", "assistant"]);
        let m = build_messages(&evs, &pinned("S"), 50);
        let with_tc: Vec<&Value> = m.iter()
            .filter(|x| x["role"]=="assistant" && x.get("tool_calls").is_some()).collect();
        assert_eq!(with_tc.len(), 1);
        assert_eq!(with_tc[0]["tool_calls"][0]["id"], "c1");
        let tools: Vec<&Value> = m.iter().filter(|x| x["role"]=="tool").collect();
        assert_eq!(tools.len(), 1, "配对的 tool 消息必须保留");
    }

    #[test]
    fn restart_rebuild_no_marker_keeps_all_up_to_cap() {
        let mut kinds = Vec::new();
        for _ in 0..3 { kinds.push("user"); kinds.push("assistant"); }
        let evs = seq_events(&kinds);
        let m = build_messages(&evs, &pinned("S"), 50);
        assert_eq!(m.iter().filter(|x| x["role"]=="user").count(), 3);
    }

    #[test]
    fn build_messages_no_cap_after_dream_marker() {
        // cap 已移除:dream marker 当 cut,其后事件全部进 context(旧 cap=3 会砍到最近 3 条)
        let mut kinds = vec!["marker_dream"]; // seq 0, until_seq=0 → cut=0
        for _ in 0..10 { kinds.push("user"); kinds.push("assistant"); } // 20 事件 seq 1..20
        let evs = seq_events(&kinds);
        let m = build_messages(&evs, &pinned("S"), 3); // 第 3 参(cap)现已无效
        let users = m.iter().filter(|x| x["role"] == "user").count();
        assert_eq!(users, 10, "dream marker 当 cut 后,10 个 user 全进,不被 cap=3 砍");
    }

    #[test]
    fn pinned_prepended_single_system_message() {
        let p = PinnedBlocks {
            system_prompt: "SYS".into(),
            soul: Some("灵魂".into()),
            agent: Some("规范".into()),
            memory: Some("记忆".into()),
        };
        let m = build_messages(&seq_events(&["user"]), &p, 50);
        assert_eq!(m[0]["role"], "system");
        let c = m[0]["content"].as_str().unwrap();
        assert!(c.contains("SYS") && c.contains("灵魂") && c.contains("规范") && c.contains("记忆"),
            "pinned 4 块应合并进首条 system：{c}");
    }

    #[test]
    fn system_prompt_carries_posix_sh_hint() {
        // D10：bash-green-sh 后 LLM 必须被告知用 POSIX sh（非 cmd），否则吐 dir/findstr 在 sh 下失败
        let m = pinned("SYS").to_messages();
        let c = m[0]["content"].as_str().unwrap();
        assert!(c.contains("POSIX sh"), "system 应注入 sh 环境提示: {c}");
        assert!(c.contains("SYS"), "原 system_prompt 应保留: {c}");
    }

    // ─── F1 P0 回归：build_messages 认 until_seq 而非 marker.seq ───

    #[test]
    fn tail_events_survive_marker_in_next_build_messages() {
        // dream 跑后：marker.seq=5（append 时位置），until_seq=2（b=cur−tail）。
        // build_messages 应认 until_seq=2 → tail 事件（seq 3,4）保留可见。
        // 旧逻辑认 marker.seq=5 → tail 全被切掉（P0 bug）。
        let mut evs = seq_events(&["user", "user"]);           // seq 0,1（dream 前）
        // 模拟 dream 段已被 marker 收尾：marker until_seq=2（b=2），marker 自己 seq=5
        let mut m = HistoryEvent::marker(2000, "dream", 2);    // until_seq = 2
        m.seq = 5;
        evs.push(m);
        // tail 事件（dream 之后、marker 之前发生的；seq ∈ (2, 5)）
        let mut t1 = HistoryEvent::user(3000, "main", "tail 消息 1", &[]);
        t1.seq = 3;
        let mut t2 = HistoryEvent::user(4000, "main", "tail 消息 2", &[]);
        t2.seq = 4;
        evs.push(t1);
        evs.push(t2);
        let m_out = build_messages(&evs, &pinned("S"), 50);
        let users: Vec<&str> = m_out.iter()
            .filter(|x| x["role"] == "user")
            .filter_map(|x| x["content"].as_str())
            .collect();
        assert!(users.iter().any(|c| c.contains("tail 消息 1")), "tail 事件 seq=3 应保留可见（认 until_seq=2）: {users:?}");
        assert!(users.iter().any(|c| c.contains("tail 消息 2")), "tail 事件 seq=4 应保留可见: {users:?}");
    }

    #[test]
    fn dream_during_events_survive_after_marker() {
        // dream 期间新事件（marker 之后落盘）也应可见（顺带修的现状 bug）。
        let mut evs = seq_events(&["user"]);                   // seq 0
        let mut m = HistoryEvent::marker(1000, "dream", 1);    // until_seq=1, marker.seq=1
        m.seq = 1;
        evs.push(m);
        // marker 之后的新事件
        let mut after = HistoryEvent::user(3000, "main", "marker 后新消息", &[]);
        after.seq = 5;
        evs.push(after);
        let m_out = build_messages(&evs, &pinned("S"), 50);
        let has_after = m_out.iter().any(|x| x["content"].as_str().unwrap_or("").contains("marker 后新消息"));
        assert!(has_after, "marker 后新事件应可见（认 until_seq）");
    }

    #[test]
    fn marker_until_seq_equal_marker_seq_legacy_unchanged() {
        // 向后兼容：旧数据 marker.seq == until_seq（无 tail 时），filter 行为不变。
        let mut evs = seq_events(&["user", "user"]);           // seq 0,1
        let mut m = HistoryEvent::marker(2000, "reset", 2);    // until_seq=2 == marker.seq=2
        m.seq = 2;
        evs.push(m);
        let mut after = HistoryEvent::user(3000, "main", "新", &[]);
        after.seq = 3;
        evs.push(after);
        let m_out = build_messages(&evs, &pinned("S"), 50);
        // marker 后只 1 个 user
        let users: Vec<&serde_json::Value> = m_out.iter().filter(|x| x["role"] == "user").collect();
        assert_eq!(users.len(), 1, "until_seq==marker.seq 时行为不变");
    }

    #[test]
    fn with_prefix_prepends_when_non_empty() {
        assert_eq!(with_prefix("P", "hi"), "<prefix>P</prefix>hi");
    }

    #[test]
    fn with_prefix_passthrough_when_empty() {
        assert_eq!(with_prefix("", "hi"), "hi");
        assert_eq!(with_prefix("   ", "hi"), "hi");
    }

    #[test]
    fn user_message_gets_prefix_from_data() {
        let mut ev = HistoryEvent::user(1000, "main", "原文", &[]);
        ev.seq = 0;
        ev.data.insert("prefix".into(), json!("PFX"));
        let m = event_to_message(&ev).unwrap();
        assert_eq!(m["content"], "<prefix>PFX</prefix>原文");
    }

    #[test]
    fn user_message_no_prefix_field_means_no_prepend() {
        let mut ev = HistoryEvent::user(1000, "main", "原文", &[]);
        ev.seq = 0;
        let m = event_to_message(&ev).unwrap();
        assert_eq!(m["content"], "原文", "无 prefix 字段 → 行为同前（回归保护）");
    }

    #[test]
    fn subagent_result_gets_prefix_from_data() {
        let mut ev = HistoryEvent::subagent_result(1000, "1", "完成", "thread=agent:1", true);
        ev.seq = 0;
        ev.data.insert("prefix".into(), json!("PFX"));
        let m = event_to_message(&ev).unwrap();
        assert_eq!(m["role"], "user");
        assert!(m["content"].as_str().unwrap().starts_with("<prefix>PFX</prefix>[子代理"),
            "subagent_result content 前应拼 prefix: {}", m["content"]);
    }

    #[test]
    fn subagent_result_ok_false_renders_failed() {
        let mut ev = HistoryEvent::subagent_result(1000, "7", "失败原因：连接超时\n部分产出：已扫描 80%", "thread=agent:7", false);
        ev.seq = 0;
        let m = event_to_message(&ev).unwrap();
        let c = m["content"].as_str().unwrap();
        assert!(c.contains("[子代理 #7 失败]"), "失败事件应渲染「失败」: {c}");
        assert!(c.contains("失败原因：连接超时"), "原因应随 summary 渲染: {c}");
        assert!(c.contains("部分产出：已扫描 80%"), "partial 应随 summary 渲染: {c}");
    }

    #[test]
    fn subagent_result_missing_ok_defaults_completed() {
        // 存量 jsonl 事件无 ok 字段 → 按「完成」渲染（向后兼容，不迁移）。
        let mut ev = HistoryEvent::subagent_result(1000, "1", "老事件", "thread=agent:1", true);
        ev.seq = 0;
        ev.data.remove("ok");
        let m = event_to_message(&ev).unwrap();
        let c = m["content"].as_str().unwrap();
        assert!(c.contains("[子代理 #1 完成]"), "缺 ok 应默认完成: {c}");
    }

    #[test]
    fn subagent_result_render_cap_truncates_only_view() {
        let big = "x".repeat(SUBAGENT_SUMMARY_RENDER_CAP + 4096);
        let mut ev = HistoryEvent::subagent_result(1000, "2", &big, "thread=agent:2", true);
        ev.seq = 0;
        let m = event_to_message(&ev).unwrap();
        let c = m["content"].as_str().unwrap();
        assert!(c.contains("已截断，完整产出可用 subagent status 查看"), "超限应带显式标记: …{}", &c[c.len()-120..]);
        assert!(c.len() < SUBAGENT_SUMMARY_RENDER_CAP + 256, "渲染视图应有界");
        assert_eq!(ev.data["summary"].as_str().unwrap().len(), big.len(), "落盘真身必须保持全文");
        // 限额内不截断
        let small = HistoryEvent::subagent_result(1000, "3", "正常产出", "thread=agent:3", true);
        let m2 = event_to_message(&small).unwrap();
        let c2 = m2["content"].as_str().unwrap();
        assert!(c2.contains("[子代理 #3 完成] 正常产出"), "限额内应全文且带完成态: {c2}");
        assert!(!c2.contains("已截断"));
    }

    #[test]
    fn external_gets_prefix_from_data() {
        let mut ev = HistoryEvent::external(1000, "拖入", "x.pdf", None);
        ev.seq = 0;
        ev.data.insert("prefix".into(), json!("PFX"));
        let m = event_to_message(&ev).unwrap();
        assert!(m["content"].as_str().unwrap().starts_with("<prefix>PFX</prefix>[外部]"),
            "external content 前应拼 prefix: {}", m["content"]);
    }
}
