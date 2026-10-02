use eve_cognition_api::Goal;
use eve_cognition_loop_api::*;
use eve_control_api::CommitState;
use eve_llm_api::{ChatRole, ToolOutput};

pub struct PriorityDrivePolicy;
impl DrivePolicy for PriorityDrivePolicy {
    fn rank(&self, goals: &[Goal], now_ms: u64) -> LoopResult<Vec<RankedGoal>> {
        let mut ranked: Vec<_> = goals
            .iter()
            .map(|goal| {
                let urgent = goal
                    .expires_at_ms
                    .is_some_and(|expires| expires.saturating_sub(now_ms) <= 60_000);
                RankedGoal {
                    goal_id: goal.id.clone(),
                    strength: goal
                        .priority
                        .saturating_add(if urgent { 10 } else { 0 })
                        .min(100),
                    reason: "未完成目标可推进；按优先级与临近期限排序".into(),
                }
            })
            .collect();
        ranked.sort_by(|a, b| b.strength.cmp(&a.strength).then(a.goal_id.cmp(&b.goal_id)));
        Ok(ranked)
    }
}
/// 第一项受信验证规则：echo:<预期文本>；不把模型回复当成执行证据。
pub struct EchoReceiptVerifier;
impl GoalVerifier for EchoReceiptVerifier {
    fn supports(&self, goal: &Goal) -> bool {
        goal.verification
            .strip_prefix("echo:")
            .is_some_and(|text| !text.is_empty())
            && goal.stop_condition == "single-attempt"
    }
    fn verify(&self, goal: &Goal, report: &GoalExecutionReport) -> LoopResult<bool> {
        let Some(expected) = goal.verification.strip_prefix("echo:") else {
            return Ok(false);
        };
        let run = &report.control.run;
        if !self.supports(goal)
            || run.commit != CommitState::Completed
            || run.turn_id.is_none()
            || run.started_tools != Some(1)
            || report.usage.admitted_tool_calls != 1
            || run.tool_results.len() != 1
        {
            return Ok(false);
        }
        let Some(transcript) = &run.transcript else {
            return Ok(false);
        };
        let calls: Vec<_> = transcript
            .iter()
            .filter(|message| message.role == ChatRole::Assistant)
            .flat_map(|message| &message.tool_calls)
            .collect();
        let receipts: Vec<_> = transcript
            .iter()
            .filter(|message| message.role == ChatRole::Tool)
            .flat_map(|message| &message.tool_results)
            .collect();
        if calls.len() != 1 || receipts.len() != 1 {
            return Ok(false);
        }
        let call = calls[0];
        let receipt = &run.tool_results[0];
        Ok(call.name == "echo"
            && call.arguments.as_object().is_some_and(|args| {
                args.len() == 1
                    && args.get("text").and_then(|value| value.as_str()) == Some(expected)
            })
            && receipt.call_id == call.id
            && receipts[0] == receipt
            && matches!(&receipt.output, ToolOutput::Success(value)
                if value.as_object().is_some_and(|object| object.len() == 1)
                    && value.get("echo").and_then(|value| value.as_str()) == Some(expected)))
    }
}
