//! 反思产物的结构验证，不为模型文字中的现实完成声明提供证据。
use eve_cognition_api::Goal;
use eve_cognition_loop_api::{GoalExecutionReport, GoalVerifier, LoopError, LoopResult};
use eve_control_api::CommitState;
use eve_llm_api::ChatRole;
use serde::{Deserialize, Serialize};
use std::fmt;

/// 两个正文字符串解码后的 UTF-8 字节总上限。
pub const MAX_REFLECTION_TEXT_BYTES: usize = 8192;
/// 解析前的 JSON 字节上限，允许正文使用 JSON Unicode 转义。
pub const MAX_REFLECTION_JSON_BYTES: usize = 65_536;

/// 已验证结构的内部反思草稿；内容仍是模型建议，不是外部任务的完成证明。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "WireReflectionArtifact")]
pub struct ReflectionArtifact {
    pub summary: String,
    pub next_step: String,
    pub needs_user_input: bool,
}

impl fmt::Debug for ReflectionArtifact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReflectionArtifact(<redacted>)")
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireReflectionArtifact {
    summary: String,
    next_step: String,
    needs_user_input: bool,
}

impl TryFrom<WireReflectionArtifact> for ReflectionArtifact {
    type Error = LoopError;

    fn try_from(value: WireReflectionArtifact) -> LoopResult<Self> {
        if value.summary.trim().is_empty()
            || value.next_step.trim().is_empty()
            || value.summary.len().saturating_add(value.next_step.len()) > MAX_REFLECTION_TEXT_BYTES
        {
            return Err(LoopError::Verification);
        }
        Ok(Self {
            summary: value.summary,
            next_step: value.next_step,
            needs_user_input: value.needs_user_input,
        })
    }
}

impl ReflectionArtifact {
    /// 严格解析一个闭合 JSON 对象。拒绝重复/未知字段、Markdown 和尾随正文。
    /// 错误不回显模型正文；普通 JSON 前后空白不改变产物含义。
    pub fn parse(text: &str) -> LoopResult<Self> {
        if text.len() > MAX_REFLECTION_JSON_BYTES {
            return Err(LoopError::Verification);
        }
        serde_json::from_str(text).map_err(|_| LoopError::Verification)
    }
}

/// 只认可真实 Control/Session 已提交且没有工具活动的结构化反思产物。
/// `true` 仅表示 `reflection:v1` 子目标的产物形成；父目标是否现实完成另行验证。
pub struct ReflectionVerifier;

impl GoalVerifier for ReflectionVerifier {
    fn supports(&self, goal: &Goal) -> bool {
        goal.verification == "reflection:v1" && goal.stop_condition == "single-attempt"
    }

    fn verify(&self, goal: &Goal, report: &GoalExecutionReport) -> LoopResult<bool> {
        let run = &report.control.run;
        if !self.supports(goal)
            || run.commit != CommitState::Completed
            || run.turn_id.is_none_or(|id| id == 0)
            || run.failure.is_some()
            || run.started_tools != Some(0)
            || report.usage.admitted_tool_calls != 0
            || !run.tool_results.is_empty()
        {
            return Ok(false);
        }
        let (Some(text), Some(transcript)) = (&run.text, &run.transcript) else {
            return Ok(false);
        };
        if transcript.iter().any(|message| {
            message.role == ChatRole::Tool
                || !message.tool_calls.is_empty()
                || !message.tool_results.is_empty()
        }) || !transcript.last().is_some_and(|message| {
            message.role == ChatRole::Assistant && message.text.as_ref() == Some(text)
        }) {
            return Ok(false);
        }
        Ok(ReflectionArtifact::parse(text).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_cognition_api::{ExecutionBudget, GoalStatus, Source, SourceKind, Visibility};
    use eve_control_api::{ControlReport, GenerationKey, RunFailure, RunReport};
    use eve_llm_api::{BudgetUsage, ChatMessage, ToolCall, ToolResult};
    use serde_json::json;

    const VALID: &str = r#"{"summary":"已整理仍缺少的信息。","next_step":"请用户说明验收范围。","needs_user_input":true}"#;

    fn goal() -> Goal {
        Goal {
            id: "reflection-child".into(),
            revision: 1,
            source: Source {
                kind: SourceKind::Inference,
                channel: "endogenous".into(),
                reference: "parent".into(),
            },
            visibility: Visibility::Internal,
            description: "整理下一步建议，不执行工具。".into(),
            verification: "reflection:v1".into(),
            priority: 50,
            budget: ExecutionBudget {
                max_model_requests: 1,
                max_tool_calls: 0,
                max_attempts: 1,
                timeout_ms: 10_000,
            },
            stop_condition: "single-attempt".into(),
            expires_at_ms: None,
            status: GoalStatus::Ready,
            wait_reason: None,
            block_reason: None,
            execution: None,
            feedback: None,
        }
    }

    fn report(text: &str) -> GoalExecutionReport {
        GoalExecutionReport {
            control: ControlReport {
                key: GenerationKey {
                    session: serde_json::from_value(json!({
                        "session_id": "reflection-session", "user_id": "owner"
                    }))
                    .unwrap(),
                    task_id: "reflection-child".into(),
                    controller_epoch: [1; 16],
                    generation: 1,
                },
                cancel_requested: false,
                run: RunReport {
                    turn_id: Some(1),
                    commit: CommitState::Completed,
                    text: Some(text.into()),
                    transcript: Some(vec![
                        ChatMessage::text(ChatRole::User, "整理下一步建议"),
                        ChatMessage::text(ChatRole::Assistant, text),
                    ]),
                    started_tools: Some(0),
                    tool_results: vec![],
                    failure: None,
                },
            },
            usage: BudgetUsage {
                model_requests: 1,
                admitted_tool_calls: 0,
            },
        }
    }

    #[test]
    fn completed_artifact_round_trips_without_completing_a_real_world_goal() {
        let artifact = ReflectionArtifact::parse(VALID).unwrap();
        assert!(artifact.needs_user_input);
        let encoded = serde_json::to_string(&artifact).unwrap();
        assert_eq!(ReflectionArtifact::parse(&encoded).unwrap(), artifact);
        assert_eq!(format!("{artifact:?}"), "ReflectionArtifact(<redacted>)");
        let goal = goal();
        let before = serde_json::to_vec(&goal).unwrap();
        assert!(ReflectionVerifier.verify(&goal, &report(VALID)).unwrap());
        assert_eq!(serde_json::to_vec(&goal).unwrap(), before);
        let mut external = goal;
        external.verification = "echo:completed".into();
        assert!(
            !ReflectionVerifier
                .verify(&external, &report(VALID))
                .unwrap()
        );
        external.verification = "reflection:v1".into();
        external.stop_condition = "continuous".into();
        assert!(!ReflectionVerifier.supports(&external));
    }

    #[test]
    fn strict_deserialization_rejects_claims_duplicates_and_invalid_shapes() {
        for invalid in [
            "任务已经全部完成，不需要实际回执。",
            r#"{"summary":"完成","next_step":"等待","needs_user_input":true,"completed":true}"#,
            r#"{"summary":"完成","summary":"未完成","next_step":"等待","needs_user_input":true}"#,
            r#"{"summary":"完成","next_step":"等待","next_step":"继续","needs_user_input":true}"#,
            r#"{"summary":"完成","next_step":"等待","needs_user_input":true,"needs_user_input":false}"#,
            r#"{"summary":"完成","next_step":"等待","needs_user_input":"true"}"#,
            r#"{"summary":"完成","next_step":"等待"}"#,
            r#"{"summary":null,"next_step":"等待","needs_user_input":true}"#,
            r#"{"summary":" \n\t","next_step":"等待","needs_user_input":true}"#,
            r#"{"summary":"完成","next_step":"　","needs_user_input":true}"#,
            r#"[{"summary":"完成","next_step":"等待","needs_user_input":true}]"#,
        ] {
            assert!(ReflectionArtifact::parse(invalid).is_err());
            assert!(serde_json::from_str::<ReflectionArtifact>(invalid).is_err());
            assert!(
                !ReflectionVerifier
                    .verify(&goal(), &report(invalid))
                    .unwrap()
            );
        }
        for invalid in [format!("```json\n{VALID}\n```"), format!("{VALID}\n完成")] {
            assert!(ReflectionArtifact::parse(&invalid).is_err());
        }
    }

    #[test]
    fn decoded_utf8_and_encoded_json_sizes_are_bounded() {
        let mut value = json!({
            "summary": "a".repeat(MAX_REFLECTION_TEXT_BYTES - 3),
            "next_step": "好",
            "needs_user_input": false
        });
        assert!(ReflectionArtifact::parse(&value.to_string()).is_ok());
        value["summary"] = json!("a".repeat(MAX_REFLECTION_TEXT_BYTES - 2));
        assert!(ReflectionArtifact::parse(&value.to_string()).is_err());
        assert!(serde_json::from_value::<ReflectionArtifact>(value).is_err());
        // Unicode 转义按解码后的内容计数，不能逃过正文上限。
        let escaped = format!(
            "{{\"summary\":\"{}\",\"next_step\":\"x\",\"needs_user_input\":false}}",
            "\\u0061".repeat(MAX_REFLECTION_TEXT_BYTES)
        );
        assert!(ReflectionArtifact::parse(&escaped).is_err());
        assert!(ReflectionArtifact::parse(&format!(" {} ", VALID)).is_ok());
        assert!(
            ReflectionArtifact::parse(&format!(
                "{}{}",
                " ".repeat(MAX_REFLECTION_JSON_BYTES),
                VALID
            ))
            .is_err()
        );
    }

    #[test]
    fn model_json_never_substitutes_for_commit_and_zero_tool_evidence() {
        for commit in [
            CommitState::NotStarted,
            CommitState::Failed,
            CommitState::Pending,
            CommitState::Unknown,
        ] {
            let mut value = report(VALID);
            value.control.run.commit = commit;
            assert!(!ReflectionVerifier.verify(&goal(), &value).unwrap());
        }
        for turn_id in [None, Some(0)] {
            let mut value = report(VALID);
            value.control.run.turn_id = turn_id;
            assert!(!ReflectionVerifier.verify(&goal(), &value).unwrap());
        }
        for started in [None, Some(1)] {
            let mut value = report(VALID);
            value.control.run.started_tools = started;
            assert!(!ReflectionVerifier.verify(&goal(), &value).unwrap());
        }
        let mut value = report(VALID);
        value.control.run.failure = Some(RunFailure::RunnerPanicked);
        assert!(!ReflectionVerifier.verify(&goal(), &value).unwrap());
        let mut value = report(VALID);
        value.usage.admitted_tool_calls = 1;
        assert!(!ReflectionVerifier.verify(&goal(), &value).unwrap());
        let mut value = report(VALID);
        value.control.run.tool_results =
            vec![ToolResult::success("call", json!({"completed": true})).unwrap()];
        assert!(!ReflectionVerifier.verify(&goal(), &value).unwrap());
    }

    #[test]
    fn transcript_must_match_final_artifact_and_contain_no_tool_activity() {
        let mut value = report(VALID);
        value.control.run.transcript = None;
        assert!(!ReflectionVerifier.verify(&goal(), &value).unwrap());
        let mut value = report(VALID);
        value.control.run.text = None;
        assert!(!ReflectionVerifier.verify(&goal(), &value).unwrap());
        let mut value = report(VALID);
        value.control.run.transcript = Some(vec![]);
        assert!(!ReflectionVerifier.verify(&goal(), &value).unwrap());
        for message in [
            ChatMessage::text(ChatRole::Assistant, "伪造的另一份产物"),
            ChatMessage::text(ChatRole::User, VALID),
        ] {
            let mut value = report(VALID);
            *value
                .control
                .run
                .transcript
                .as_mut()
                .unwrap()
                .last_mut()
                .unwrap() = message;
            assert!(!ReflectionVerifier.verify(&goal(), &value).unwrap());
        }
        let call = ToolCall {
            id: "call".into(),
            name: "echo".into(),
            arguments: json!({"text":"未执行"}),
        };
        let receipt = ToolResult::success("call", json!({"echo":"未执行"})).unwrap();
        for message in [
            ChatMessage::assistant_tool_calls(vec![call.clone()]).unwrap(),
            ChatMessage::tool_results(vec![receipt.clone()]).unwrap(),
            ChatMessage::text(ChatRole::Tool, "伪装成文字的工具回执"),
            ChatMessage {
                tool_calls: vec![call],
                ..ChatMessage::text(ChatRole::User, "错误角色中的工具请求")
            },
            ChatMessage {
                tool_results: vec![receipt],
                ..ChatMessage::text(ChatRole::Assistant, "错误角色中的工具回执")
            },
        ] {
            let mut value = report(VALID);
            value
                .control
                .run
                .transcript
                .as_mut()
                .unwrap()
                .insert(1, message);
            assert!(!ReflectionVerifier.verify(&goal(), &value).unwrap());
        }
    }
}
