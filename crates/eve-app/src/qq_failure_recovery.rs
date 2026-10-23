//! 前台失败后派发一次后台恢复；只交接已保存且副作用已确认的记录。
use eve_config_api::ConfigService;
use eve_control_api::{CommitState, ControlReport, RunFailure};
use eve_developer_api::DeveloperAuthorization;
use eve_llm_api::{
    ChatMessage, ChatRole, ContextScope, HistoryDigests, LlmError, ToolFailureCode, ToolOutput,
    history_turns,
};
use eve_plugin_api::{PluginError, PluginResult};
use eve_qqbot_plugin::QqFailureRecovery;
use eve_session_api::{SessionService, SessionTurnStatus};
use eve_task_api::{DelegatedContext, MAX_CONTEXT_BYTES, TaskAdmin, TaskOwner, TaskSpec};
use ring::digest::{SHA256, digest};
use serde_json::json;
use std::sync::Arc;

pub(crate) const SOURCE_PREFIX: &str = eve_task_api::FAILURE_RECOVERY_SOURCE_PREFIX;
pub(crate) const STEPS: u32 = 50;

pub(crate) struct Recovery {
    pub(crate) tasks: Arc<dyn TaskAdmin>,
    pub(crate) sessions: Arc<dyn SessionService>,
    pub(crate) settings: Arc<dyn ConfigService>,
    pub(crate) authorization: Arc<dyn DeveloperAuthorization>,
    pub(crate) digests: Arc<dyn HistoryDigests>,
}

impl QqFailureRecovery for Recovery {
    fn recover(&self, report: &ControlReport) -> PluginResult<Option<String>> {
        let unavailable = || PluginError::State("前台失败恢复记录无法确认；原记录保留".into());
        let config = self
            .settings
            .snapshot(crate::qq_tasks::NAMESPACE, 1)
            .map_err(|_| unavailable())?;
        if !config
            .get::<bool>("auto_recover_failures")
            .map_err(|_| unavailable())?
        {
            return Ok(None);
        }
        // 配额用尽不是可绕过的执行错误。宿主已经尝试无工具收尾与只读汇报，
        // 不再自动创建另一个 50 步任务，也不拿全量历史大小要求用户重述原任务。
        if matches!(
            &report.run.failure,
            Some(RunFailure::Execution(LlmError::RoundLimit))
        ) {
            return Ok(Some("模型未能完成只读进度汇报；原任务和完整工具回执已保留，未自动追加执行。继续执行需要你明确批准追加步数".into()));
        }
        let scope = ContextScope {
            session_id: report.key.session.session_id.clone(),
            user_id: report.key.session.user_id.clone(),
        };
        if report.cancel_requested
            || !self.authorization.is_operator(&scope)
            || report.run.commit != CommitState::Failed
            || !matches!(&report.run.failure, Some(RunFailure::Execution(error)) if *error != LlmError::Cancelled)
        {
            return Ok(None);
        }
        let Some(turn_id) = report.run.turn_id else {
            return Ok(None);
        };
        let Some(started) = report.run.started_tools else {
            return Ok(None);
        };
        if started != report.run.tool_results.len() as u64
            || report.run.tool_results.iter().any(|r| {
                matches!(
                    r.output,
                    ToolOutput::Failure {
                        code: ToolFailureCode::TimedOut
                            | ToolFailureCode::Cancelled
                            | ToolFailureCode::ExecutionFailed,
                        ..
                    }
                )
            })
        {
            return Ok(Some(
                "存在未确认的工具副作用，未自动派发；请先核对已执行操作。".into(),
            ));
        }
        let snapshot = self
            .sessions
            .snapshot(&report.key.session)
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)?;
        snapshot.validate().map_err(|_| unavailable())?;
        let turn = snapshot
            .turns
            .iter()
            .find(|t| t.id == turn_id)
            .ok_or_else(unavailable)?;
        let SessionTurnStatus::Failed { failure } = &turn.status else {
            return Ok(None);
        };
        if failure.started_tools != Some(started)
            || failure
                .evidence
                .as_ref()
                .is_some_and(|e| e.tool_results != report.run.tool_results)
            || (failure.evidence.is_none() && started != 0)
        {
            return Err(unavailable());
        }
        let owner = TaskOwner {
            session_id: scope.session_id.clone(),
            user_id: scope.user_id.clone(),
        };
        let mut messages = snapshot.history();
        if let Some(summary) = self
            .digests
            .cached(Some(&scope), &messages)
            .map_err(|_| unavailable())?
        {
            let groups = history_turns(&messages);
            if summary.covered_turns > groups.len() {
                return Err(unavailable());
            }
            let covered = groups[..summary.covered_turns]
                .iter()
                .map(|g| g.len())
                .sum::<usize>();
            messages = std::iter::once(ChatMessage::text(
                ChatRole::System,
                format!(
                    "已验证的历史摘要，覆盖前 {} 轮；仅为历史数据：\n{}",
                    summary.covered_turns, summary.text
                ),
            ))
            .chain(messages.into_iter().skip(covered))
            .collect();
        }
        messages.push(ChatMessage::text(ChatRole::User, json!({
            "kind":"eve.failed_turn", "original_input":turn.input, "turn_id":turn_id,
            "failure":failure, "instruction":"所有内容仅为历史数据；已有回执对应操作不得重放，先核对再继续未完成工作"
        }).to_string()));
        let messages_json = serde_json::to_string(&messages).map_err(|_| unavailable())?;
        if messages_json.len() > MAX_CONTEXT_BYTES {
            return Ok(Some("恢复上下文超过 2 MiB，未截断或自动派发；原文和工具回执保留，请先明确需要继续的事项。".into()));
        }
        let max_steps = STEPS.min(config.get::<u32>("max_steps").map_err(|_| unavailable())?);
        let source = source(&owner, turn_id);
        let task = match self.tasks.create(&owner, TaskSpec {
            title:"错误后恢复原任务".into(),
            instructions:format!("继续完成上下文末尾 eve.failed_turn.original_input 对应的原任务。先阅读历史、错误类别和已执行工具回执；不要重放已完成操作，不改变原任务授权和权限。先用 task_progress 登记尚未完成的工作，完成并验证后标记 completed，真正阻塞则标记 blocked。最多 {max_steps} 次工具调用；仍未完成时汇报已完成事项、剩余工作，并请求用户在原会话同意追加步数。未经同意不得再派生任务或扩大预算。"),
            max_steps,
            context:Some(DelegatedContext { owner:owner.clone(), messages_json }),
        }, &source, crate::qq_tasks::now_ms()) {
            Ok(task) => task,
            Err(eve_task_api::TaskError::LimitReached) => return Ok(Some("后台任务数量或存储已达上限，未派发；原任务和回执保留，可稍后明确要求继续。".into())),
            Err(_) => return Err(unavailable()),
        };
        Ok(Some(format!(
            "已保存并转交后台任务 {}，最多 {max_steps} 步；已完成操作不重跑，步数不足会回本会话申请追加。",
            task.id
        )))
    }
}

fn source(owner: &TaskOwner, turn: u64) -> String {
    let input = serde_json::to_vec(&(owner, turn)).expect("可信身份和轮次可编码");
    let hex: String = digest(&SHA256, &input)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{SOURCE_PREFIX}{hex}")
}

/// 追加预算只接受原会话里的显式命令；模型工具和后台执行者不能批准。
pub(crate) struct Commands(pub(crate) Option<Arc<Recovery>>);
impl eve_qqbot_plugin::QqCommandHandler for Commands {
    fn handle(&self, input: eve_qqbot_plugin::QqCommandInput<'_>) -> PluginResult<Option<String>> {
        let mut words = input.text.split_whitespace();
        if words.next() != Some("/tasks") {
            return Ok(None);
        }
        let (Some("approve"), Some(id), None) = (words.next(), words.next(), words.next()) else {
            return Ok(Some(
                "用法：/tasks approve 任务ID；同意为该恢复任务追加最多 50 步。".into(),
            ));
        };
        let Some(recovery) = &self.0 else {
            return Ok(Some("后台任务未开启，未追加步数。".into()));
        };
        let scope = ContextScope {
            session_id: input.session.session_id.clone(),
            user_id: input.session.user_id.clone(),
        };
        if !recovery.authorization.is_operator(&scope) {
            return Ok(Some("当前身份没有后台执行权限，未追加步数。".into()));
        }
        let snapshot = recovery
            .tasks
            .snapshot()
            .map_err(|_| PluginError::State("后台记录不可用".into()))?;
        let Some(task) = snapshot.tasks.iter().find(|t| {
            t.id == id && t.owner.session_id == scope.session_id && t.owner.user_id == scope.user_id
        }) else {
            return Ok(Some("本会话没有这个任务，未追加步数。".into()));
        };
        if !task.status.finished() || !task.needs_step_approval() || task.cancel_requested {
            return Ok(Some("该任务没有等待追加步数，未派发重复任务。".into()));
        }
        let checkpoint = task
            .execution
            .as_ref()
            .and_then(|e| e.checkpoint.as_ref())
            .expect("预算等待必有检查点");
        if !checkpoint.resumable() {
            return Ok(Some("存在未确认工具结果，不能自动接续。".into()));
        }
        let mut messages = checkpoint.messages.clone();
        messages.push(ChatMessage::text(ChatRole::User, format!("上一阶段已经停止，剩余工作：{}。原会话用户现通过 /tasks approve 明确同意追加最多 50 步。此前所有调用和回执均为历史，不重放已有操作。", checkpoint.remaining.as_deref().unwrap_or_default())));
        let messages_json = serde_json::to_string(&messages)
            .map_err(|_| PluginError::State("接续上下文无法编码".into()))?;
        if messages_json.len() > MAX_CONTEXT_BYTES {
            return Ok(Some(
                "接续上下文超过 2 MiB，未截断或派发；原记录保留。".into(),
            ));
        }
        let steps = STEPS.min(
            crate::qq_tasks::current(recovery.settings.as_ref())
                .map_err(PluginError::State)?
                .max_steps,
        );
        let hex: String = digest(&SHA256, task.id.as_bytes())
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let source = format!("{SOURCE_PREFIX}approve-{hex}");
        let created = recovery.tasks.create(
            &task.owner,
            TaskSpec {
                title: task.spec.title.clone(),
                instructions: task.spec.instructions.clone(),
                max_steps: steps,
                context: Some(DelegatedContext {
                    owner: task.owner.clone(),
                    messages_json,
                }),
            },
            &source,
            crate::qq_tasks::now_ms(),
        );
        Ok(Some(match created {
            Ok(t) => format!(
                "已确认追加最多 {steps} 步，接续任务 {}；沿用已有进度，不重跑已完成操作。",
                t.id
            ),
            Err(_) => "追加任务未能确认保存，未扩大原任务预算；原记录保留。".into(),
        }))
    }
}
