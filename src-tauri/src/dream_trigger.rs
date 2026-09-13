//! dream 触发状态管理（DreamTrigger）——「何时跑 dream」的决策逻辑。
//! 「怎么跑」(提取逻辑) 在 mem_dream 模块。
//!
//! 触发条件（v2）：
//!   ① idle：距上次用户活动 ≥ idle_secs（默认 7200s=2h）→ 全量整理 + context 清空
//!   ② token：当前 context input token ≥ trigger_tokens（默认 300k）→ 整理 + 留最近 3 回合
//! （旧的「轮数 cap 触发」已移除；config.dream_cap_turns 字段保留只服务 build_messages 的 context 窗口。）
use crate::history::HistoryEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerReason {
    Idle,
    ContextLen,
}

/// dream 触发状态。owned by driver task。
#[derive(Debug, Clone)]
pub struct DreamTrigger {
    pub last_user_activity: u64,
    pub last_dream_marker_seq: u64,
    pub in_flight: bool,
    pub armed: bool,
}

impl Default for DreamTrigger {
    fn default() -> Self { Self::new() }
}

impl DreamTrigger {
    pub fn new() -> Self {
        Self { last_user_activity: 0, last_dream_marker_seq: 0, in_flight: false, armed: false }
    }

    pub fn note_activity(&mut self, ts: u64) {
        self.armed = true;
        if ts > self.last_user_activity { self.last_user_activity = ts; }
    }

    pub fn seed_from_history(&mut self, events: &[HistoryEvent]) {
        self.last_dream_marker_seq = events.iter()
            .filter(|e| e.kind == "marker" && e.data.get("marker").and_then(|v| v.as_str()) == Some("dream"))
            .map(|e| e.seq).max().unwrap_or(0);
    }

    pub fn dream_started(&mut self, marker_seq: u64) {
        self.in_flight = true;
        self.last_dream_marker_seq = marker_seq;
    }

    pub fn dream_finished(&mut self) { self.in_flight = false; }

    /// 门（gate）：armed + 非 in_flight + frontier 之后有 main 非 marker 事件可整理。
    /// idle/token 阈值判定共用此前置；force 触发（dream 工具）也走这道门。
    /// 返回 None=门未通过（不可整理），Some(())=门通过，可继续判阈值或直接 force。
    pub fn gate(&self, events: &[HistoryEvent]) -> Option<()> {
        if !self.armed || self.in_flight { return None; }
        // 必须有 main 非 marker 事件可整理：frontier=0（无 dream marker）时看全部；否则看 frontier 之后
        let has_new = events.iter().any(|e| {
            e.thread == "main" && e.kind != "marker"
                && (self.last_dream_marker_seq == 0 || e.seq > self.last_dream_marker_seq)
        });
        if has_new { Some(()) } else { None }
    }

    /// 判定是否触发 dream。
    /// - `idle_secs`：idle 触发阈值（墙钟秒）
    /// - `trigger_tokens`：token 触发阈值（当前 context input token）
    ///
    /// 当前 context token 量取最近一轮 main assistant 落盘的 usage.prompt_tokens（服务器真实计数）。
    /// **idle 优先于 token**：用户离开 → 全量整理；否则看 context 是否过长。
    pub fn check(&self, events: &[HistoryEvent], now: u64, idle_secs: u64, trigger_tokens: u64) -> Option<TriggerReason> {
        self.gate(events)?;
        // ① idle：墙钟达阈值
        if now.saturating_sub(self.last_user_activity) >= idle_secs * 1000 {
            return Some(TriggerReason::Idle);
        }
        // ② token：context 长度达阈值
        if recent_prompt_tokens(events) >= trigger_tokens {
            return Some(TriggerReason::ContextLen);
        }
        None
    }

    /// force 触发（dream 工具）：跳过 idle/token 阈值，但仍走 gate（防并发 in_flight + 防无新可整理）。
    /// 返回 ContextLen（语义=整理后留 cap 条，不写 reset marker；与 token 触发行为一致）。
    pub fn check_forced(&self, events: &[HistoryEvent]) -> Option<TriggerReason> {
        self.gate(events).map(|_| TriggerReason::ContextLen)
    }
}

/// 最近一轮 main assistant 的 usage.prompt_tokens（= 上一轮发送时的 context 总长度）。
/// 无 main assistant / 无 usage → 0（不触发 token 条件）。agent:N 线程的 assistant 不计入主 context。
fn recent_prompt_tokens(events: &[HistoryEvent]) -> u64 {
    events.iter().rev()
        .find(|e| e.kind == "assistant" && e.thread == "main")
        .and_then(|e| e.data.get("usage"))
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
}

/// 占位（同步，持锁）：set in_flight + 捕获 a。不推进 frontier。
pub fn prepare_dream(trigger: &mut DreamTrigger) -> Option<u64> {
    if trigger.in_flight { return None; }
    let a = trigger.last_dream_marker_seq + 1;
    trigger.in_flight = true;
    Some(a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn build_events(n: usize) -> Vec<HistoryEvent> {
        (0..n).flat_map(|i| {
            let s = (i * 2) as u64;
            vec![
                HistoryEvent::user(1000 + s, "main", &format!("u{i}"), &[]),
                HistoryEvent::assistant(1000 + s + 1, "main", &format!("a{i}"), "", vec![]),
            ]
        }).collect()
    }

    fn usage(prompt: u64) -> Option<serde_json::Value> {
        Some(json!({"prompt_tokens": prompt, "completion_tokens": 5}))
    }

    #[test]
    fn check_returns_none_when_not_armed() {
        let t = DreamTrigger::new();
        let evs = build_events(3);
        assert!(t.check(&evs, 10000, 10, 1_000_000).is_none());
    }

    #[test]
    fn check_returns_idle_when_idle_secs_elapsed() {
        let mut t = DreamTrigger::new();
        t.note_activity(1000);
        let evs = build_events(2);
        // idle 阈值 10s，now 距活动 11s → Idle（trigger_tokens 设极高不干扰）
        assert_eq!(t.check(&evs, 1000 + 11_000, 10, 1_000_000), Some(TriggerReason::Idle));
    }

    #[test]
    fn check_returns_context_len_when_tokens_exceed() {
        let mut t = DreamTrigger::new();
        t.note_activity(1000); // arm + 活动
        // 构造带 usage 的 assistant：prompt_tokens=350_000 > 阈值 300_000
        let mut evs = vec![HistoryEvent::user(1000, "main", "u", &[])];
        let mut a = HistoryEvent::assistant_with_usage(1001, "main", "a", "", vec![], usage(350_000));
        a.seq = 1;
        evs.push(a);
        // now 紧跟活动（不 idle），但 token 超阈值 → ContextLen
        assert_eq!(t.check(&evs, 2000, 7200, 300_000), Some(TriggerReason::ContextLen));
    }

    #[test]
    fn check_idle_wins_over_token() {
        // 同时满足 idle 与 token → Idle 优先（用户离开 → 全清）
        let mut t = DreamTrigger::new();
        t.note_activity(1000);
        let mut evs = vec![HistoryEvent::user(1000, "main", "u", &[])];
        let mut a = HistoryEvent::assistant_with_usage(1001, "main", "a", "", vec![], usage(999_999));
        a.seq = 1;
        evs.push(a);
        assert_eq!(t.check(&evs, 1000 + 8_000_000, 7200, 300_000), Some(TriggerReason::Idle));
    }

    #[test]
    fn check_returns_none_when_no_new_since_marker() {
        // frontier 之后无新事件 → 不触发（dream 无东西整理）
        let mut t = DreamTrigger::new();
        t.note_activity(1000);
        t.last_dream_marker_seq = 100; // frontier 远超所有事件 seq
        let evs = build_events(3); // seq 0..4
        assert!(t.check(&evs, 1000 + 8_000_000, 10, 1).is_none());
    }

    #[test]
    fn check_returns_none_when_in_flight() {
        let mut t = DreamTrigger::new();
        t.note_activity(1000);
        t.in_flight = true;
        let evs = build_events(3);
        assert!(t.check(&evs, 10000, 10, 1).is_none());
    }

    #[test]
    fn recent_prompt_tokens_picks_last_main_assistant() {
        let mut evs = vec![HistoryEvent::user(1000, "main", "u", &[])];
        let mut a1 = HistoryEvent::assistant_with_usage(1001, "main", "a1", "", vec![], usage(100_000));
        a1.seq = 1;
        let mut a2 = HistoryEvent::assistant_with_usage(1002, "main", "a2", "", vec![], usage(350_000));
        a2.seq = 2;
        evs.push(a1);
        evs.push(a2);
        assert_eq!(recent_prompt_tokens(&evs), 350_000, "取最近 main assistant 的 prompt_tokens");
    }

    #[test]
    fn recent_prompt_tokens_ignores_agent_thread() {
        // agent:N 线程的 assistant 不计入主 context token
        let mut a = HistoryEvent::assistant_with_usage(1000, "agent:1", "子代理", "", vec![], usage(999_999));
        a.seq = 0;
        assert_eq!(recent_prompt_tokens(&[a]), 0, "agent 线程 assistant 不算");
    }

    #[test]
    fn recent_prompt_tokens_zero_when_no_usage() {
        let evs = build_events(2); // assistant 无 usage
        assert_eq!(recent_prompt_tokens(&evs), 0);
    }

    #[test]
    fn check_forced_skips_thresholds_but_keeps_gate() {
        // armed + 有新 + 不 idle + token 远低阈值 → check 返回 None，但 check_forced 应触发
        let mut t = DreamTrigger::new();
        t.note_activity(1000);
        let evs = build_events(2);
        assert!(t.check(&evs, 2000, 7200, 300_000).is_none(), "阈值未达 check 不触发");
        assert_eq!(t.check_forced(&evs), Some(TriggerReason::ContextLen), "force 跳过阈值");
    }

    #[test]
    fn check_forced_returns_none_when_in_flight() {
        let mut t = DreamTrigger::new();
        t.note_activity(1000);
        t.in_flight = true;
        let evs = build_events(3);
        assert!(t.check_forced(&evs).is_none(), "in_flight 时 force 也不触发");
    }

    #[test]
    fn check_forced_returns_none_when_no_new() {
        let mut t = DreamTrigger::new();
        t.note_activity(1000);
        t.last_dream_marker_seq = 100;
        let evs = build_events(3); // seq 0..4 全在 frontier 之前
        assert!(t.check_forced(&evs).is_none(), "无新可整理时 force 也不触发");
    }

    #[test]
    fn check_forced_returns_none_when_not_armed() {
        let t = DreamTrigger::new(); // 未 note_activity → 未 armed
        let evs = build_events(3);
        assert!(t.check_forced(&evs).is_none(), "未 armed 时 force 不触发");
    }

    #[test]
    fn prepare_sets_in_flight_and_returns_a() {
        let mut t = DreamTrigger::new();
        t.last_dream_marker_seq = 5;
        let a = prepare_dream(&mut t).unwrap();
        assert_eq!(a, 6);
        assert!(t.in_flight);
    }

    #[test]
    fn prepare_returns_none_when_in_flight() {
        let mut t = DreamTrigger::new();
        t.in_flight = true;
        assert!(prepare_dream(&mut t).is_none());
    }
}
