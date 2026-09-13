//! 主 agent 主动询问用户工具的数据结构与协议定义。
//!
//! 设计意图：tool_ask_user 返回 String 不够——需要"挂起当前 turn、等前端用户答、再解冻"。
//! 走 oneshot + AskUserHandle 哨兵：
//!   - tool_ask_user 经 SessionEvent::AwaitUserRequest 派发给前端
//!   - 等待 SessionEvent::AwaitUserAnswer 经 driver 路由回 oneshot
//!   - 用户答/超时/跳过/取消 → tool_result 注入 messages、turn 解冻
//!
//! Stage 1（当前提交）：只定义数据结构 + SessionEvent 变体 + schema 校验。
//! Stage 2（下一笔 commit）：driver 挂起/解冻的 oneshot 注册与路由。
//! Stage 3（再下一笔）：run_turn 工具返回类型分派（AskUserHandle 不走 tool_result 路径）。

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// 单个候选项的 JSON 表示（schema 暴露给 LLM 用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub recommended: bool,
    #[serde(default)]
    pub is_custom: bool,
}

/// AwaitUserRequest 携带的完整请求负载（driver → 前端）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskUserRequest {
    pub question_id: String,            // uuid v4
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default)]
    pub options: Vec<AskOption>,
    #[serde(default = "default_true")]
    pub allow_custom: bool,
    #[serde(default = "default_true")]
    pub require_confirm: bool,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_true")]
    pub pause_on_activity: bool,
    #[serde(default = "default_resume")]
    pub resume_after_idle_secs: u64,
}
fn default_true() -> bool { true }
fn default_timeout() -> u64 { 15 }
fn default_resume() -> u64 { 5 }

/// 用户答/跳过/超时/取消的终态（driver → run_turn）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskUserResult {
    pub question_id: String,
    /// 用户答的实际文本（None = 跳过/超时/取消）。
    pub answer: Option<String>,
    pub skipped: bool,
    pub timed_out: bool,
    pub cancelled: bool,
    /// 超时且有推荐项时自动提交——这是 answer 的来源。
    pub auto_submitted: bool,
    /// 超时但无推荐项时降级 continue 的标记。
    pub fallback_no_recommended: bool,
}

/// tool_ask_user 返回的挂起哨兵（run_turn 拿这个就不写 tool_result，提前返）。
/// 内含完整 AskUserRequest,driver 拿到后能 emit 给前端 + 注册 oneshot 等答。
/// call_id 由 run_turn 回填（工具循环里才有）——最终答案按 tool_result 落 history 需要它配对。
#[derive(Debug)]
pub struct AskUserHandle {
    pub question_id: String,
    pub call_id: String,
    pub request: AskUserRequest,
    pub receiver: oneshot::Receiver<AskUserResult>,
}

/// 校验 schema 字段约束：recommended 最多 1 个。违反返错误字符串。
pub fn validate_options(options: &[AskOption]) -> Result<(), String> {
    let rec_count = options.iter().filter(|o| o.recommended).count();
    if rec_count > 1 {
        return Err(format!(
            "schema 错：options 里最多 1 个 recommended:true，当前有 {rec_count} 个"
        ));
    }
    Ok(())
}

/// 从 schema 选项中挑出推荐项 label（若存在）。
pub fn pick_recommended_label(options: &[AskOption]) -> Option<String> {
    options.iter().find(|o| o.recommended).map(|o| o.label.clone())
}

/// 等待中的 ask_user 注册表:question_id → oneshot sender。
///
/// 关键设计:driver 主循环在等待用户答案期间是**阻塞**的(run_one 内联 select),
/// 若 AwaitUserAnswer 经通道回到 driver,永远没人消费 → 自锁直到超时。
/// 故 user_answered 命令**直接**从注册表取 sender fire oneshot,不过通道。
/// driver 解冻后自行把结果转 AwaitUserAnswer 事件重投通道走 history/turn。
pub type SharedPendingAsk = std::sync::Arc<
    std::sync::Mutex<std::collections::HashMap<String, oneshot::Sender<AskUserResult>>>,
>;

pub fn new_registry() -> SharedPendingAsk {
    std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 从注册表取 sender 并 fire 结果。返回 false = 该 question_id 不在等待中（已超时/已答/重复点击）。
pub fn fire_answer(reg: &SharedPendingAsk, question_id: &str, result: AskUserResult) -> bool {
    let sender = reg.lock().unwrap().remove(question_id);
    match sender {
        Some(tx) => tx.send(result).is_ok(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_options_allows_zero_or_one_recommended() {
        assert!(validate_options(&[]).is_ok());
        assert!(validate_options(&[
            AskOption { label: "A".into(), description: None, recommended: false, is_custom: false },
            AskOption { label: "B".into(), description: None, recommended: false, is_custom: false },
        ]).is_ok());
        assert!(validate_options(&[
            AskOption { label: "A".into(), description: None, recommended: true, is_custom: false },
        ]).is_ok());
    }

    #[test]
    fn validate_options_rejects_multiple_recommended() {
        let opts = vec![
            AskOption { label: "A".into(), description: None, recommended: true, is_custom: false },
            AskOption { label: "B".into(), description: None, recommended: true, is_custom: false },
        ];
        let err = validate_options(&opts).unwrap_err();
        assert!(err.contains("最多 1 个"), "错误信息应说明上限: {err}");
    }

    #[test]
    fn pick_recommended_returns_label_or_none() {
        let none = vec![
            AskOption { label: "A".into(), description: None, recommended: false, is_custom: false },
        ];
        assert_eq!(pick_recommended_label(&none), None);

        let some = vec![
            AskOption { label: "A".into(), description: None, recommended: false, is_custom: false },
            AskOption { label: "B".into(), description: None, recommended: true, is_custom: false },
        ];
        assert_eq!(pick_recommended_label(&some), Some("B".into()));
    }
}
