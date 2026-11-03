//! 自然聊天复用原待办保存、反馈和反思读取服务，查询结果带当前真实版本。
use super::*;
use crate::qq_natural_goals::{Review, failed, input_id, session};
use eve_llm_api::{
    Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext, ToolFuture,
    ToolValidationError,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::OnceLock;

#[cfg(test)]
#[path = "qq_cognition_tool_tests.rs"]
mod tests;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    goal_id: Option<String>,
    text: Option<String>,
    expected_revision: Option<u64>,
}

fn args(value: &Value) -> Result<Args, ToolValidationError> {
    let invalid = || {
        ToolValidationError { message: "需要create(text)、inspect(可选goal_id)或feedback(goal_id/text/expected_revision)，不接受身份或原话参数".into() }
    };
    let a: Args = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
    let id_ok = a
        .goal_id
        .as_deref()
        .is_none_or(|id| eve_cognition_api::validate_id(id).is_ok());
    let valid = match a.action.as_str() {
        "create" => {
            a.goal_id.is_none()
                && a.expected_revision.is_none()
                && a.text
                    .as_deref()
                    .is_some_and(|text| validate_text(text).is_ok())
        }
        "inspect" => a.text.is_none() && a.expected_revision.is_none(),
        "feedback" => {
            a.goal_id.is_some()
                && a.expected_revision.is_some_and(|r| r > 0)
                && a.text
                    .as_deref()
                    .is_some_and(|text| !text.trim().is_empty() && text.len() <= 4096)
        }
        _ => false,
    };
    if !id_ok || !valid {
        return Err(invalid());
    }
    Ok(a)
}

pub(crate) struct ManageTool {
    pub(crate) commands: Arc<OnceLock<Arc<Commands>>>,
    pub(crate) review: Review,
}

impl Tool for ManageTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "manage_goal".into(),
            description: "自然聊天创建待办、查看反思草稿、提交用户反馈。create保存真实目标并启动无工具反思，保存不表示执行或完成现实工作；inspect读取当前归属、版本和草稿；feedback须先inspect取得当前版本，再按用户本人的反馈更新，旧版本拒绝。需要执行长期工作时按原目标另用后台任务或计划。身份和最近六条完整用户原话由宿主绑定，变更经独立审核，不能让用户输入内部命令。".into(),
            argument_schema: json!({"type":"object","properties":{"action":{"enum":["create","inspect","feedback"]},"goal_id":{"type":"string"},"text":{"type":"string"},"expected_revision":{"type":"integer","minimum":1}},"required":["action"],"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial { scope: "natural-goals".into() }),
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        args(value).map(|_| ())
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            let a = args(&call.arguments).map_err(|e| failed(e.message))?;
            self.review.authorize("manage_goal", &call, &context)?;
            let commands = self
                .commands
                .get()
                .ok_or_else(|| failed("待办反思服务尚未就绪"))?;
            if !commands.accepting.load(Ordering::SeqCst) {
                return Err(failed("待办服务正在停止"));
            }
            let session = session(&context)?;
            let view = commands.state(&session.user_id).map_err(failed)?;
            let owned = |goal: &Goal| {
                goal.source.kind == SourceKind::User
                    && goal.source.channel == CHANNEL
                    && goal.visibility == Visibility::User(session.user_id.clone())
            };
            let selected = match a.goal_id.as_ref() {
                Some(id) => Some(
                    view.state
                        .goals
                        .get(id)
                        .filter(|goal| owned(goal))
                        .cloned()
                        .ok_or_else(|| failed("没有属于当前可信用户的这件待办"))?,
                ),
                None => None,
            };
            if a.action == "inspect" {
                let items: Vec<_> = view
                    .state
                    .goals
                    .values()
                    .filter(|goal| owned(goal))
                    .take(10)
                    .cloned()
                    .collect();
                let draft = if let Some(goal) = selected.as_ref() {
                    let reflection =
                        current_reflection(&view.state, &view.subject_id, goal).map_err(failed)?;
                    match reflection {
                        Some(reflection) => {
                            let artifact = if crate::cognition_action_admission::verified_reflection(
                                reflection, goal,
                            ) {
                                let sessions = commands
                                    .sessions
                                    .as_ref()
                                    .ok_or_else(|| failed("反思会话服务不可用"))?;
                                Some(
                                    crate::cognition_action_admission::saved_artifact(
                                        sessions.as_ref(),
                                        reflection,
                                    )
                                    .map_err(failed)?,
                                )
                            } else {
                                None
                            };
                            Some(
                                json!({"reflection_id":reflection.id,"status":reflection.status,"goal_revision":goal.revision,"artifact":artifact}),
                            )
                        }
                        None => None,
                    }
                } else {
                    None
                };
                return Ok(
                    json!({"items":items,"goal":selected,"draft":draft,"goal_completed":selected.as_ref().is_some_and(|goal| goal.completion.is_some())}),
                );
            }
            if a.action == "feedback"
                && selected.as_ref().map(|goal| goal.revision) != a.expected_revision
            {
                return Err(failed("待办版本已变化，请先读取当前版本，原记录保留"));
            }
            self.review
                .approve(
                    "manage_goal",
                    &call,
                    &context,
                    json!({"arguments":call.arguments,"current_goal":selected}),
                )
                .await?;
            if !commands.accepting.load(Ordering::SeqCst) {
                return Err(failed("审核期间待办服务已停止，未提交变更"));
            }
            let input_id = input_id(&call, &context)?;
            let text = a.text.as_deref().ok_or_else(|| failed("待办正文缺失"))?;
            let input = QqCommandInput {
                message_id: &input_id,
                session: &session,
                text,
            };
            let (id, feedback) = if a.action == "create" {
                commands.add(&input, text).map_err(failed)?;
                (id_for(&input), None)
            } else {
                let goal = selected.ok_or_else(|| failed("待办不存在"))?;
                let admin = commands
                    .admin
                    .as_ref()
                    .ok_or_else(|| failed("待办服务不可用"))?;
                let service = UserGoalFeedback::new(
                    Arc::new(admin.clone()),
                    "eve".into(),
                    session.user_id.clone(),
                    CHANNEL.into(),
                    "qq.goal.feedback".into(),
                )
                .map_err(failed)?;
                let report = service
                    .submit(GoalFeedbackInput {
                        goal_id: goal.id.clone(),
                        expected_goal_revision: goal.revision,
                        feedback_id: id_for(&input).replacen("qq-goal-", "qq-goal-feedback-", 1),
                        text: text.trim().into(),
                        at_ms: now_ms().map_err(failed)?,
                    })
                    .map_err(|error| failed(format!("反馈未保存，保留原记录：{error}")))?;
                (goal.id, Some(report))
            };
            let current = commands
                .state(&session.user_id)
                .map_err(failed)?
                .state
                .goals
                .get(&id)
                .cloned()
                .ok_or_else(|| failed("待办没有保存成功，保留原记录，请查看后按当前版本重试"))?;
            Ok(
                json!({"stored":true,"goal":current,"feedback":feedback,"external_goal_completed":false}),
            )
        })
    }
}
