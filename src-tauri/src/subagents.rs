//! 子代理专属：SubagentEmitter（流式上屏 + progress 快照）、SubagentStream 抽象。
use crate::jobs::{JobId, SubagentProgress, ToolTrace};
use crate::llm::Emitter;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// 子代理流式 delta 外推（真实实现走 AppHandle emit "subagent-stream"；测试用 Noop/Rec）。
pub trait SubagentStream: Send + Sync {
    fn delta(&self, job_id: &JobId, payload: Value);
}

pub struct NoopSubagentStream;
impl SubagentStream for NoopSubagentStream {
    fn delta(&self, _: &JobId, _: Value) {}
}

const PARTIAL_MAX: usize = 200;
const TRACE_CAP: usize = 5;
const BRIEF_MAX: usize = 80;

pub struct SubagentEmitter {
    pub progress: Arc<Mutex<SubagentProgress>>,
    pub stream: Arc<dyn SubagentStream>,
    pub job_id: JobId,
}

fn brief(s: &str) -> String {
    if s.chars().count() <= BRIEF_MAX {
        s.to_string()
    } else {
        let cut: String = s.chars().take(BRIEF_MAX).collect();
        format!("{cut}…")
    }
}

#[async_trait]
impl Emitter for SubagentEmitter {
    async fn content(&self, t: &str) {
        {
            let mut p = self.progress.lock().unwrap();
            p.partial.push_str(t);
            if p.partial.chars().count() > PARTIAL_MAX {
                let kept: String = p
                    .partial
                    .chars()
                    .rev()
                    .take(PARTIAL_MAX)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                p.partial = kept;
            }
        }
        self.stream
            .delta(&self.job_id, json!({ "kind": "content", "text": t }));
    }

    async fn tool_call(&self, name: &str, args: &str) {
        {
            let mut p = self.progress.lock().unwrap();
            p.rounds += 1;
            if p.recent_tools.len() >= TRACE_CAP {
                p.recent_tools.pop_front();
            }
            p.recent_tools.push_back(ToolTrace {
                name: name.into(),
                args_brief: brief(args),
                result_brief: None,
            });
        }
        self.stream.delta(
            &self.job_id,
            json!({ "kind": "tool_call", "name": name, "brief": brief(args) }),
        );
    }

    async fn tool_result(&self, name: &str, result: &str) {
        {
            let mut p = self.progress.lock().unwrap();
            let idx = p
                .recent_tools
                .iter()
                .position(|t| t.name == name && t.result_brief.is_none());
            if let Some(i) = idx {
                p.recent_tools[i].result_brief = Some(brief(result));
            }
        }
        self.stream.delta(
            &self.job_id,
            json!({ "kind": "tool_result", "name": name, "brief": brief(result) }),
        );
    }
    // thinking / turn_start / turn_end / error：继承 Emitter 的默认 noop（控噪音；终态由 spawned 任务经 job-update 推）
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::{SubagentProgress, ToolTrace};
    use crate::llm::{LlmRound, RoundResult, FinishReason, Emitter};
    use crate::config::Config;
    use crate::tools::ToolsCtx;
    use std::sync::Mutex;
    use async_trait::async_trait;

    #[tokio::test]
    async fn content_accumulates_partial_and_streams() {
        let prog = Arc::new(Mutex::new(SubagentProgress::new()));
        let rec = Arc::new(RecStream::default());
        let em = SubagentEmitter {
            progress: prog.clone(),
            stream: rec.clone(),
            job_id: "1".into(),
        };
        em.content("你好").await;
        em.content("世界").await;
        assert_eq!(prog.lock().unwrap().partial, "你好世界");
        let deltas = rec.0.lock().unwrap();
        assert_eq!(deltas.len(), 2, "应推 2 条 content delta");
        assert_eq!(deltas[0]["kind"], "content");
    }

    #[tokio::test]
    async fn tool_result_fifo_pairs_same_name() {
        let prog = Arc::new(Mutex::new(SubagentProgress::new()));
        let em = SubagentEmitter {
            progress: prog.clone(),
            stream: Arc::new(NoopSubagentStream),
            job_id: "1".into(),
        };
        em.tool_call("read", r#"{"path":"a"}"#).await;
        em.tool_call("read", r#"{"path":"b"}"#).await;
        em.tool_result("read", "ra").await;
        em.tool_result("read", "rb").await;
        let p = prog.lock().unwrap();
        let v: Vec<&ToolTrace> = p.recent_tools.iter().collect();
        assert_eq!(
            v[0].result_brief.as_deref(),
            Some("ra"),
            "FIFO：第一个 read 配 ra"
        );
        assert_eq!(v[1].result_brief.as_deref(), Some("rb"));
    }

    #[tokio::test]
    async fn last_unpaired_tool_shown_as_running() {
        let prog = Arc::new(Mutex::new(SubagentProgress::new()));
        let em = SubagentEmitter {
            progress: prog.clone(),
            stream: Arc::new(NoopSubagentStream),
            job_id: "1".into(),
        };
        em.tool_call("bash", r#"{"command":"cargo test"}"#).await;
        let p = prog.lock().unwrap();
        assert!(
            p.recent_tools.back().unwrap().result_brief.is_none(),
            "未配对 ⇒ 运行中"
        );
        assert_eq!(p.rounds, 1);
    }

    #[tokio::test]
    async fn thinking_is_noop() {
        let prog = Arc::new(Mutex::new(SubagentProgress::new()));
        let em = SubagentEmitter {
            progress: prog.clone(),
            stream: Arc::new(NoopSubagentStream),
            job_id: "1".into(),
        };
        em.thinking("推理").await;
        assert!(
            prog.lock().unwrap().partial.is_empty(),
            "thinking 不入 partial"
        );
    }

    #[derive(Default)]
    struct RecStream(Mutex<Vec<serde_json::Value>>);
    impl SubagentStream for RecStream {
        fn delta(&self, _id: &JobId, payload: Value) {
            self.0.lock().unwrap().push(payload);
        }
    }

    struct StopRound;
    #[async_trait]
    impl LlmRound for StopRound {
        async fn round(
            &self,
            _m: &[serde_json::Value],
            _cfg: &Config,
            emit: &dyn Emitter,
        ) -> Result<RoundResult, String> {
            emit.content("done").await;
            Ok(RoundResult {
                content: "done".into(),
                reasoning: String::new(),
                tool_calls: vec![],
                finish: FinishReason::Stop,
                usage: None,
                assistant_message: serde_json::json!({"role":"assistant","content":"done"}),
            })
        }
    }

    fn agent_ctx(ws: std::path::PathBuf) -> ToolsCtx {
        let mut c = ToolsCtx::foreground(ws, "cn".into());
        c.allow_background = false;
        c
    }

    #[tokio::test]
    async fn spawn_returns_and_completes_sending_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let registry = std::sync::Arc::new(std::sync::Mutex::new(jobs::JobRegistry::new()));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let cfg = Config::default();
        let id = spawn_agent(
            "做X",
            "X",
            &cfg,
            std::sync::Arc::new(StopRound) as std::sync::Arc<dyn LlmRound>,
            &agent_ctx(dir.path().to_path_buf()),
            &cfg,
            registry.clone(),
            tx,
            std::sync::Arc::new(NoopSubagentStream),
        )
        .unwrap();
        let o = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(o.job_id, id);
        assert!(o.ok, "StopRound 完成 → ok:true");
        assert_eq!(o.answer.as_deref(), Some("done"));
        let r = registry.lock().unwrap();
        assert!(matches!(
            r.get(&id).unwrap().status,
            jobs::JobStatus::Done { code: 0 }
        ));
    }

    struct FailRound;
    #[async_trait]
    impl LlmRound for FailRound {
        async fn round(
            &self,
            _m: &[serde_json::Value],
            _cfg: &Config,
            emit: &dyn Emitter,
        ) -> Result<RoundResult, String> {
            emit.content("已扫描 80%").await;
            Err("模拟失败".into())
        }
    }

    struct EmptyRound;
    #[async_trait]
    impl LlmRound for EmptyRound {
        async fn round(
            &self,
            _m: &[serde_json::Value],
            _cfg: &Config,
            _emit: &dyn Emitter,
        ) -> Result<RoundResult, String> {
            Ok(RoundResult {
                content: String::new(),
                reasoning: String::new(),
                tool_calls: vec![],
                finish: FinishReason::Stop,
                usage: None,
                assistant_message: serde_json::json!({"role":"assistant","content":""}),
            })
        }
    }

    #[tokio::test]
    async fn failure_reports_reason_and_partial() {
        let dir = tempfile::tempdir().unwrap();
        let registry = std::sync::Arc::new(std::sync::Mutex::new(jobs::JobRegistry::new()));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let cfg = Config::default();
        let id = spawn_agent(
            "x", "x", &cfg, std::sync::Arc::new(FailRound) as std::sync::Arc<dyn LlmRound>,
            &agent_ctx(dir.path().to_path_buf()), &cfg, registry.clone(), tx,
            std::sync::Arc::new(NoopSubagentStream),
        )
        .unwrap();
        let o = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(!o.ok, "失败 → ok:false");
        let a = o.answer.as_deref().unwrap();
        assert!(a.contains("失败原因：模拟失败"), "错误原因须回传: {a}");
        assert!(a.contains("部分产出：已扫描 80%"), "partial 须随失败回传: {a}");
        let r = registry.lock().unwrap();
        assert!(matches!(r.get(&id).unwrap().status, jobs::JobStatus::Failed { .. }));
        assert_eq!(r.get(&id).unwrap().answer.as_deref(), Some(a));
    }

    #[tokio::test]
    async fn empty_success_gets_literal_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let registry = std::sync::Arc::new(std::sync::Mutex::new(jobs::JobRegistry::new()));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let cfg = Config::default();
        spawn_agent(
            "x", "x", &cfg, std::sync::Arc::new(EmptyRound) as std::sync::Arc<dyn LlmRound>,
            &agent_ctx(dir.path().to_path_buf()), &cfg, registry.clone(), tx,
            std::sync::Arc::new(NoopSubagentStream),
        )
        .unwrap();
        let o = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(o.ok, "空成功仍属完成 → ok:true");
        assert_eq!(o.answer.as_deref(), Some("(无输出)"), "空成功须字面兜底，不注入空正文");
    }

    #[tokio::test]
    async fn spawn_at_max_returns_err() {
        let dir = tempfile::tempdir().unwrap();
        let registry = std::sync::Arc::new(std::sync::Mutex::new(jobs::JobRegistry::new()));
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let cfg = Config::default();
        for _ in 0..jobs::MAX_AGENTS {
            spawn_agent(
                "x",
                "x",
                &cfg,
                std::sync::Arc::new(StopRound) as std::sync::Arc<dyn LlmRound>,
                &agent_ctx(dir.path().to_path_buf()),
                &cfg,
                registry.clone(),
                tx.clone(),
                std::sync::Arc::new(NoopSubagentStream),
            )
            .unwrap();
        }
        let over = spawn_agent(
            "over",
            "over",
            &cfg,
            std::sync::Arc::new(StopRound) as std::sync::Arc<dyn LlmRound>,
            &agent_ctx(dir.path().to_path_buf()),
            &cfg,
            registry.clone(),
            tx,
            std::sync::Arc::new(NoopSubagentStream),
        );
        assert!(over.is_err(), "达 MAX_AGENTS 应拒绝");
    }

    #[tokio::test]
    async fn spawn_at_custom_max_reports_configured_limit() {
        // 拒绝消息须反映配置上限（new_with_max(2) → "2/2"），而非常量 4。
        let dir = tempfile::tempdir().unwrap();
        let registry: crate::jobs::SharedRegistry = std::sync::Arc::new(std::sync::Mutex::new(
            crate::jobs::JobRegistry::new_with_max(2),
        ));
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let cfg = Config::default();
        for _ in 0..2 {
            spawn_agent(
                "x", "x", &cfg, std::sync::Arc::new(StopRound) as std::sync::Arc<dyn LlmRound>,
                &agent_ctx(dir.path().to_path_buf()), &cfg, registry.clone(), tx.clone(),
                std::sync::Arc::new(NoopSubagentStream),
            )
            .unwrap();
        }
        let over = spawn_agent(
            "over", "over", &cfg, std::sync::Arc::new(StopRound) as std::sync::Arc<dyn LlmRound>,
            &agent_ctx(dir.path().to_path_buf()), &cfg, registry.clone(), tx,
            std::sync::Arc::new(NoopSubagentStream),
        );
        assert!(over.is_err());
        assert!(over.unwrap_err().contains("2/2"), "拒绝消息应反映配置上限 2，而非常量 4");
    }

    #[tokio::test]
    async fn status_returns_snapshot_and_kill_reclaims() {
        let dir = tempfile::tempdir().unwrap();
        let registry = std::sync::Arc::new(std::sync::Mutex::new(jobs::JobRegistry::new()));
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let cfg = Config::default();

        struct LoopRound;
        #[async_trait]
        impl LlmRound for LoopRound {
            async fn round(
                &self,
                _m: &[serde_json::Value],
                _cfg: &Config,
                emit: &dyn Emitter,
            ) -> Result<RoundResult, String> {
                // 模拟 LLM 延迟：每轮 sleep，保证 status 测试 sleep(200ms) 时子代理还在跑（进行中）。
                // 否则 mock+bash 太快在 200ms 内跑满 8 轮到上限 → status 显示"已完成"而非"进行中"。
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                emit.content("partial...").await;
                let tc = serde_json::json!({"id":"c","type":"function","function":{"name":"bash","arguments":"{\"command\":\"echo x\"}"}});
                Ok(RoundResult {
                    content: String::new(),
                    reasoning: String::new(),
                    tool_calls: vec![tc.clone()],
                    finish: FinishReason::ToolCalls,
                    usage: None,
                    assistant_message: serde_json::json!({"role":"assistant","content":null,"tool_calls":[tc]}),
                })
            }
        }

        let id = spawn_agent(
            "long",
            "long",
            &cfg,
            std::sync::Arc::new(LoopRound) as std::sync::Arc<dyn LlmRound>,
            &agent_ctx(dir.path().to_path_buf()),
            &cfg,
            registry.clone(),
            tx,
            std::sync::Arc::new(NoopSubagentStream),
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let snap = status_snapshot(&id, &registry);
        assert!(snap.contains("进行中"), "status: {snap}");
        let reclaimed = kill(&id, &registry);
        assert!(reclaimed.contains("已终止"), "kill: {reclaimed}");
    }

    #[test]
    fn status_unknown_id() {
        let registry = std::sync::Arc::new(std::sync::Mutex::new(jobs::JobRegistry::new()));
        assert!(status_snapshot(&"999".to_string(), &registry).contains("不存在"));
    }
}

use crate::config::Config;
use crate::jobs::{self, JobKind, JobOutcome, JobStatus, SharedRegistry};
use crate::llm::LlmRound;
use crate::tools::{self, ToolsCtx, SUBAGENT_TOOLS};
use tokio_util::sync::CancellationToken;

pub fn spawn_agent(
    prompt: &str,
    caption: &str,
    cfg: &Config,
    round: Arc<dyn LlmRound>,
    sub_ctx: &ToolsCtx,
    sub_cfg_tpl: &Config,
    registry: SharedRegistry,
    job_done_tx: tokio::sync::mpsc::Sender<JobOutcome>,
    stream: Arc<dyn SubagentStream>,
) -> Result<JobId, String> {
    let label = if caption.is_empty() {
        prompt.chars().take(20).collect::<String>()
    } else {
        caption.to_string()
    };
    let (id, cancel, progress, snap) = {
        let mut r = registry.lock().unwrap();
        if !r.can_spawn(JobKind::Agent) {
            return Err(format!(
                "子代理并发已满({}/{})",
                r.running_agents,
                r.max_agents
            ));
        }
        let id = r.register(JobKind::Agent, label.clone(), std::path::PathBuf::new(), now_ms());
        let cancel = CancellationToken::new();
        let progress = Arc::new(std::sync::Mutex::new(jobs::SubagentProgress::new()));
        let j = r.jobs.get_mut(&id).unwrap();
        j.cancel = Some(cancel.clone());
        j.progress = Some(progress.clone());
        let snap = r.get(&id).cloned().unwrap();
        (id, cancel, progress, snap)
    };
    // 推 job-update：前端 jobs 面板立刻显示该子代理行（与 process job 一致）
    sub_ctx.job_writer.append(jobs::JobEvent::started(&snap));
    sub_ctx.job_update.update(&snap);
    let upd2 = sub_ctx.job_update.clone();
    let mut sub_cfg = sub_cfg_tpl.clone();
    sub_cfg.active_tools = Some(tools::schemas_subset(SUBAGENT_TOOLS));
    let mut sub_ctx = clone_ctx(sub_ctx);
    sub_ctx.allow_background = false;
    let mut msgs = vec![
        serde_json::json!({ "role": "system", "content": cfg.subagent_system_prompt }),
        serde_json::json!({ "role": "user", "content": prompt }),
    ];
    let id_for_spawn = id.clone();
    let emit = SubagentEmitter {
        progress: progress.clone(),
        stream: stream.clone(),
        job_id: id_for_spawn.clone(),
    };
    let reg2 = registry.clone();
    let done2 = job_done_tx.clone();
    let writer_for_task = sub_ctx.job_writer.clone();
    let label2 = label.clone();
    // 迭代上限从配置读（默认 5000；0 = 不限制）。失控主保险丝是 run_turn 内的螺旋熔断，
    // 这里只是「缓慢漂移型失控」的兜底，所以给得宽且可调（数小时/数千循环的长任务是正常负载）。
    let max_iters = if cfg.subagent_max_iters == 0 { usize::MAX } else { cfg.subagent_max_iters as usize };
    tokio::spawn(async move {
        let mut cancelled = false;
        let thread = format!("agent:{id_for_spawn}");
        let res: Result<String, String> = tokio::select! {
            _ = cancel.cancelled() => { cancelled = true; Err("cancelled".into()) }
            r = crate::llm::run_turn(round, &emit, &sub_cfg, &mut msgs, &sub_ctx, max_iters, &thread) => {
                match r.error {
                    Some(e) => Err(e),
                    None => Ok(r.content),
                }
            }
        };
        let (answer, suppress, already, snap_opt, outcome_ok) = {
            let mut r = reg2.lock().unwrap();
            let already = !matches!(r.get(&id_for_spawn).map(|j| &j.status), Some(JobStatus::Running));
            if already {
                (
                    r.get(&id_for_spawn).and_then(|j| j.answer.clone()).unwrap_or_default(),
                    r.get(&id_for_spawn).map(|j| j.suppress_inject).unwrap_or(false),
                    true,
                    None,
                    false, // already 终态 ⇒ should_send=false，该值不会被使用
                )
            } else {
                let partial = r
                    .get(&id_for_spawn)
                    .and_then(|j| {
                        j.progress.as_ref().map(|p| p.lock().unwrap().clone())
                    })
                    .unwrap_or_else(jobs::SubagentProgress::new);
                let status = if cancelled {
                    JobStatus::Killed
                } else {
                    match &res {
                        Ok(_) => JobStatus::Done { code: 0 },
                        Err(e) => JobStatus::Failed { reason: e.clone() },
                    }
                };
                // 结算三态诚实化：失败原因 + partial 必须回传（主 agent 只有这一条路知道「为什么失败、
                // 死前干到哪」）；空成功给字面兜底，绝不注入空正文。cancelled 维持 partial 语义。
                let (answer, outcome_ok) = if cancelled {
                    (partial.partial.clone(), false)
                } else {
                    match &res {
                        Ok(s) if s.trim().is_empty() => ("(无输出)".to_string(), true),
                        Ok(s) => (s.clone(), true),
                        Err(e) => {
                            let mut a = format!("失败原因：{e}");
                            if !partial.partial.trim().is_empty() {
                                a.push_str(&format!("\n部分产出：{}", partial.partial));
                            }
                            (a, false)
                        }
                    }
                };
                r.finish(&id_for_spawn, status, now_ms());
                if let Some(j) = r.jobs.get_mut(&id_for_spawn) {
                    j.answer = Some(answer.clone());
                }
                let snap = r.get(&id_for_spawn).cloned().unwrap();
                let sup = r.get(&id_for_spawn).map(|j| j.suppress_inject).unwrap_or(false);
                (answer, sup, false, Some(snap), outcome_ok)
            }
        };
        // 推 job-update：前端面板更新到终态（含 answer）
        {
            let r = reg2.lock().unwrap();
            if let Some(s) = r.get(&id_for_spawn) { upd2.update(s); }
        }
        // 写终态 JobEvent（done/failed；killed 不写——与 process job 一致）
        if let Some(snap) = snap_opt {
            if !cancelled && !already {
                writer_for_task.append(jobs::terminal_event(&snap));
            }
        }
        let should_send = !already && !(cancelled && suppress);
        if should_send {
            let outcome = JobOutcome {
                job_id: id_for_spawn,
                kind: JobKind::Agent,
                label: Some(label2),
                ok: outcome_ok,
                answer: Some(answer.clone()),
                note: if cancelled && !suppress {
                    Some("被用户终止".into())
                } else {
                    None
                },
                code: None,
                tail: String::new(),
            };
            let _ = done2.send(outcome).await;
        }
    });
    Ok(id)
}

pub fn status_snapshot(id: &JobId, registry: &SharedRegistry) -> String {
    let r = registry.lock().unwrap();
    let Some(j) = r.get(id) else {
        return format!("子代理 #{id} 不存在");
    };
    if !matches!(j.kind, JobKind::Agent) {
        return format!("#{id} 不是子代理");
    }
    let prog = j
        .progress
        .as_ref()
        .map(|p| p.lock().unwrap().clone())
        .unwrap_or_else(jobs::SubagentProgress::new);
    let elapsed = prog.started.elapsed();
    let status_word = match &j.status {
        JobStatus::Running => "进行中",
        JobStatus::Done { .. } => "已完成",
        JobStatus::Failed { .. } => "已失败",
        JobStatus::Killed => "已终止",
    };
    let mut s = format!(
        "子代理 #{}（{}）：{}，已 {} 轮，耗时 {}s。",
        j.label, id, status_word, prog.rounds, elapsed.as_secs()
    );
    if !prog.recent_tools.is_empty() {
        s.push_str("\n最近工具：");
        for (i, t) in prog.recent_tools.iter().enumerate() {
            let mark = if t.result_brief.is_some() {
                "✓"
            } else {
                "⏳运行中"
            };
            s.push_str(&format!(
                "\n  {}) {:<6} {:<30} {}",
                i + 1,
                t.name,
                t.args_brief,
                mark
            ));
        }
    }
    if !prog.partial.is_empty() {
        s.push_str(&format!("\n部分产出：「{}」", prog.partial));
    }
    if let Some(a) = &j.answer {
        s.push_str(&format!("\n最终：{}", a));
    }
    s
}

pub fn kill(id: &JobId, registry: &SharedRegistry) -> String {
    let (snapshot, cancel, already_terminal, terminal_answer) = {
        let mut r = registry.lock().unwrap();
        let Some(j) = r.jobs.get_mut(id) else {
            return format!("子代理 #{id} 不存在");
        };
        if !matches!(j.kind, JobKind::Agent) {
            return format!("#{id} 不是子代理");
        }
        let already = !matches!(j.status, JobStatus::Running);
        if !already {
            j.suppress_inject = true;
        }
        let snap = j
            .progress
            .as_ref()
            .map(|p| p.lock().unwrap().clone())
            .unwrap_or_else(jobs::SubagentProgress::new);
        (
            snap,
            j.cancel.clone(),
            already,
            j.answer.clone(),
        )
    };
    if let Some(c) = cancel {
        c.cancel();
    }
    if already_terminal {
        return format!(
            "子代理 #{} 已结束。结果：{}",
            id,
            terminal_answer.unwrap_or_default()
        );
    }
    let mut s = format!("已终止子代理 #{}。回收 ——", id);
    if !snapshot.recent_tools.is_empty() {
        s.push_str(" 最近工具：");
        for t in &snapshot.recent_tools {
            let mark = if t.result_brief.is_some() { "✓" } else { "⏳" };
            s.push_str(&format!("[{} {} {}]", t.name, t.args_brief, mark));
        }
    }
    if !snapshot.partial.is_empty() {
        s.push_str(&format!("；部分产出：「{}」", snapshot.partial));
    }
    s
}

fn clone_ctx(src: &ToolsCtx) -> ToolsCtx {
    ToolsCtx {
        workspace: src.workspace.clone(),
        cache: src.cache.clone(),
        jobs: src.jobs.clone(),
        job_done_tx: src.job_done_tx.clone(),
        job_update: src.job_update.clone(),
        minimax_region: src.minimax_region.clone(),
        allow_background: src.allow_background,
        subagent_stream: src.subagent_stream.clone(),
        history: src.history.clone(),
        job_writer: src.job_writer.clone(),
        session_tx: src.session_tx.clone(),
        // 子代理不共享主 driver 的中断 token:主 turn 的「中断」按钮不该越级打断子代理
        // (子代理有自己的 job CancellationToken,reset 时统一 cancel)。给个永 None 的新句柄。
        interrupt: crate::tools::InterruptHandle::new(),
        tasks: src.tasks.clone(),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
