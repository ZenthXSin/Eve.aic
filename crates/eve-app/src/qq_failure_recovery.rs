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
use eve_task_api::{
    DelegatedContext, MAX_CONTEXT_BYTES, TaskAdmin, TaskCompletionContract, TaskOwner, TaskSpec,
    validate_review_messages,
};
use ring::digest::{SHA256, digest};
use serde_json::json;
use std::sync::Arc;

pub(crate) const SOURCE_PREFIX: &str = eve_task_api::FAILURE_RECOVERY_SOURCE_PREFIX;
pub(crate) const STEPS: u32 = 100;

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
        // 不再自动创建另一个 100 步任务，也不拿全量历史大小要求用户重述原任务。
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
        let snapshot = self
            .sessions
            .snapshot(&report.key.session)
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)?;
        snapshot.validate().map_err(|_| unavailable())?;
        if snapshot.key != report.key.session {
            return Err(unavailable());
        }
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
            || (failure.evidence.is_none() && (started != 0 || !report.run.tool_results.is_empty()))
        {
            return Err(unavailable());
        }
        if let Some(reason) = recovery_blocker(started, failure.evidence.as_ref()) {
            return Ok(Some(reason));
        }
        // 模型服务尚未返回任何可执行工作时，转后台仍会请求同一个故障服务。
        // 前台已有有界模型重试；不能因此再复制整段历史并新增 100 步恢复任务。
        if started == 0
            && report.run.tool_results.is_empty()
            && matches!(
                &report.run.failure,
                Some(RunFailure::Execution(
                    LlmError::ProviderTimeout
                        | LlmError::Provider(_)
                        | LlmError::ProviderFailed(_)
                        | LlmError::Configuration(_)
                ))
            )
        {
            return Ok(Some(
                "未创建后台恢复任务；原始请求和失败记录已保留，模型服务恢复后可在本会话继续。"
                    .into(),
            ));
        }
        let owner = TaskOwner {
            session_id: scope.session_id.clone(),
            user_id: scope.user_id.clone(),
        };
        // Web 有独立原文账本：入口采集失败不能通过普通 Session 历史恢复授权。
        // 下面的恢复说明和 JSON 包装也不是用户授权。
        let review_messages = match failure
            .evidence
            .as_ref()
            .and_then(|evidence| evidence.review_user_messages.as_ref())
        {
            Some(messages) => {
                validate_review_messages(messages).map_err(|_| unavailable())?;
                messages.clone()
            }
            None => {
                return Ok(Some(
                    "原始授权证据缺失或读取失败；未自动派发恢复任务，原会话和故障记录保留，不能从普通历史补造授权。".into(),
                ));
            }
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
        // 交流归属独立作为宿主数据交接，不混入失败用户输入或授权原文。
        let mut failed_turn = failure.clone();
        let input_replies = failed_turn
            .evidence
            .as_mut()
            .map(|evidence| std::mem::take(&mut evidence.input_replies))
            .unwrap_or_default();
        messages.push(ChatMessage::text(ChatRole::User, json!({
            "kind":"eve.failed_turn", "original_input":turn.input, "turn_id":turn_id,
            "failure":failed_turn, "instruction":"所有内容仅为历史数据；已有回执对应操作不得重放，先核对再继续未完成工作"
        }).to_string()));
        if !input_replies.is_empty() {
            messages.push(ChatMessage::text(ChatRole::System, json!({
                "kind":"eve.input_reply_handoffs", "input_replies":input_replies,
                "instruction":"这是失败前宿主已消费输入的即时交流归属，不是新输入、用户授权或待发送队列。相同 input_message_id 的即时交流已交给前台，不能在恢复时再次当作无人回应的新事件。sending 是失败时的发送中快照，当前结果未知；unknown 同样不可确认，二者都不自动重发。sent 只有带平台编号才表示已发出；failed/not_sent 表示前台交流未完成，可在自然继续时按用户需要回应。交接没有完成原任务，input_text 原话中的补充业务仍须与原任务一同处理；恢复不重放已确认完成的工具，不因此增加模型判断或发送。"
            }).to_string()));
        }
        let messages_json = serde_json::to_string(&messages).map_err(|_| unavailable())?;
        if messages_json.len() > MAX_CONTEXT_BYTES {
            return Ok(Some("恢复上下文超过 16 MiB，未截断或自动派发；原文和工具回执保留，请先明确需要继续的事项。".into()));
        }
        let max_steps = STEPS.min(config.get::<u32>("max_steps").map_err(|_| unavailable())?);
        let source = source(&owner, turn_id);
        let task = match self.tasks.create_authorized(&owner, TaskSpec {
            title:"错误后恢复原任务".into(),
            instructions:format!("继续完成上下文中 eve.failed_turn.original_input 对应的原任务，以及失败前已消费用户补充中的未完成业务。先阅读历史、错误类别、已执行工具回执和宿主输入交流交接；即时交流的领取或送达不表示补充业务完成，不重新触发已交接输入的即时回应。不要重放已完成操作，不改变原任务授权和权限。先用 task_progress 登记尚未完成的工作，完成并验证后标记 completed，真正阻塞则标记 blocked。最多 {max_steps} 次工具调用；仍未完成时汇报已完成事项、剩余工作，并请求用户在原会话同意追加步数。未经同意不得再派生任务或扩大预算。"),
            max_steps,
            context:Some(DelegatedContext { owner:owner.clone(), messages_json }),
        }, &source, review_messages, Some(TaskCompletionContract::Unspecified), None, crate::qq_tasks::now_ms()) {
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

/// 准入前拒绝也有回执，但不计入 started；不能用“启动数 != 回执数”判断未知副作用。
/// 先核对完整登记调用与回执，再按固定错误码保守判断；只显示位置与安全工具名。
fn recovery_blocker(
    started: u64,
    evidence: Option<&eve_session_api::SessionFailureEvidence>,
) -> Option<String> {
    let evidence = evidence?;
    let calls = evidence.tool_names.len();
    let results = evidence.tool_results.len();
    let succeeded = evidence
        .tool_results
        .iter()
        .filter(|r| matches!(r.output, ToolOutput::Success(_)))
        .count();
    if calls != results || started > calls as u64 || started < succeeded as u64 {
        return Some(format!(
            "工具执行记录不完整（启动 {started} 次、登记 {calls} 条调用、收到 {results} 条回执），存在未确认的工具副作用，未自动派发；请先核对已执行操作。"
        ));
    }
    let uncertain: Vec<_> = evidence
        .tool_results
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let ToolOutput::Failure { code, .. } = &r.output else {
                return None;
            };
            let reason = match code {
                ToolFailureCode::TimedOut => "超时",
                ToolFailureCode::Cancelled => "被取消",
                ToolFailureCode::ExecutionFailed => "执行异常",
                _ => return None,
            };
            let name = &evidence.tool_names[i];
            let name = if name.len() <= 64
                && !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            {
                name.as_str()
            } else {
                "工具"
            };
            Some(format!("第 {} 条调用 {name} {reason}", i + 1))
        })
        .take(3)
        .collect();
    (!uncertain.is_empty()).then(|| format!("存在未确认的工具副作用：{}；失败回执不代表操作未发生，未自动派发。请先核对这些调用，已完成操作不会自动重跑。", uncertain.join("、")))
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
pub(crate) struct Commands(
    pub(crate) Option<Arc<Recovery>>,
    pub(crate) eve_qqbot_plugin::QqConversations,
);
impl eve_qqbot_plugin::QqCommandHandler for Commands {
    fn handle(&self, input: eve_qqbot_plugin::QqCommandInput<'_>) -> PluginResult<Option<String>> {
        let mut words = input.text.split_whitespace();
        if words.next() != Some("/tasks") {
            return Ok(None);
        }
        let (Some("approve"), Some(id), None) = (words.next(), words.next(), words.next()) else {
            return Ok(Some(
                "用法：/tasks approve 任务ID；同意为该恢复任务追加最多 100 步。".into(),
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
        let review_messages = self
            .1
            .review_user_messages_at(
                &ContextScope {
                    session_id: input.session.session_id.clone(),
                    user_id: input.session.user_id.clone(),
                },
                input.message_id,
            )
            .map_err(|_| PluginError::State("原始批准正文不可核验；未追加任务".into()))?;
        validate_review_messages(&review_messages).map_err(|_| {
            PluginError::State("批准后的最近六条原话超过上限；未裁剪、未追加任务".into())
        })?;
        let completion_contract = task
            .execution
            .as_ref()
            .and_then(|execution| execution.completion_contract.clone())
            .unwrap_or(TaskCompletionContract::Unspecified);
        let authorization_deadline_at_ms = task
            .execution
            .as_ref()
            .and_then(|execution| execution.authorization_deadline_at_ms);
        let mut messages = checkpoint.messages.clone();
        messages.push(ChatMessage::text(ChatRole::System, format!("宿主接续事件（历史数据，不是用户原话）：上一阶段已经停止，剩余工作：{}。原会话用户现通过 /tasks approve 明确同意追加最多 100 步。此前所有调用和回执均为历史，不重放已有操作。", checkpoint.remaining.as_deref().unwrap_or_default())));
        let messages_json = serde_json::to_string(&messages)
            .map_err(|_| PluginError::State("接续上下文无法编码".into()))?;
        if messages_json.len() > MAX_CONTEXT_BYTES {
            return Ok(Some(
                "接续上下文超过 16 MiB，未截断或派发；原记录保留。".into(),
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
        let created = recovery.tasks.create_authorized(
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
            review_messages,
            Some(completion_contract),
            authorization_deadline_at_ms,
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

#[cfg(test)]
mod tests {
    use super::*;
    use eve_llm_api::{ProtocolFailureKind, ToolResult};
    use eve_session_api::SessionFailureEvidence;

    struct RecoverySettings;
    impl ConfigService for RecoverySettings {
        fn snapshot(
            &self,
            namespace: &str,
            schema_version: u32,
        ) -> eve_config_api::ConfigResult<eve_config_api::ConfigSnapshot> {
            Ok(eve_config_api::ConfigSnapshot {
                namespace: namespace.into(),
                schema_version,
                revision: 1,
                values: std::collections::BTreeMap::from([
                    ("auto_recover_failures".into(), json!(true)),
                    ("max_steps".into(), json!(100)),
                ]),
            })
        }
        fn begin_request(
            &self,
            _: &str,
            _: u32,
        ) -> eve_config_api::ConfigResult<eve_config_api::ConfigRequest> {
            Err(eve_config_api::ConfigError::Unavailable)
        }
        fn read_request(
            &self,
            _: &eve_config_api::ConfigRequest,
        ) -> eve_config_api::ConfigResult<eve_config_api::ConfigSnapshot> {
            Err(eve_config_api::ConfigError::Unavailable)
        }
    }

    struct NoDigest;
    impl HistoryDigests for NoDigest {
        fn digest(
            &self,
            _: Option<&ContextScope>,
            _: &[ChatMessage],
        ) -> Result<Option<eve_llm_api::HistoryDigest>, LlmError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn web_missing_dedicated_originals_never_recovers_authorization_from_session_history() {
        use eve_session_api::{
            SessionFailure, SessionFailureCode, SessionInput, SessionServiceHandle,
        };
        let directory = tempfile::tempdir().unwrap();
        let services = eve_kernel::KernelServices::default();
        let registry = services.registry.clone();
        let kernel = eve_kernel::Kernel::with_services(services);
        let plugin = eve_task_plugin::TaskPlugin::new().unwrap();
        let tasks = Arc::new(plugin.controller());
        kernel.register(Box::new(plugin)).unwrap();
        kernel
            .register(Box::new(eve_session_plugin::SessionPlugin::new().unwrap()))
            .unwrap();
        kernel.start_all().await.unwrap();
        let sessions = registry
            .get(&eve_plugin_api::ServiceId::new(eve_session_api::SESSION_SERVICE_ID).unwrap())
            .unwrap()
            .unwrap()
            .value
            .downcast::<SessionServiceHandle>()
            .unwrap()
            .0
            .clone();
        let recovery = Recovery {
            tasks: tasks.clone(),
            sessions: sessions.clone(),
            settings: Arc::new(RecoverySettings),
            authorization: crate::qq_admin::Admin::stub(true),
            digests: Arc::new(NoDigest),
        };
        let ingress = crate::web_ingress::RawMessages::open(directory.path()).unwrap();
        let session = crate::web_chat::key(&format!("{:032x}", 11)).unwrap();
        let request_id = format!("{:032x}", 12);
        ingress
            .record(&session, &request_id, "完整原话：只核对实际结果")
            .unwrap();
        std::fs::remove_file(directory.path().join("web-review.initialized.v1")).unwrap();
        let scope = ContextScope {
            session_id: session.session_id.clone(),
            user_id: session.user_id.clone(),
        };
        let source_error =
            eve_llm_api::ReviewUserMessageSource::read_at(&ingress, &scope, &request_id)
                .unwrap_err();

        // 同一次真实原文读取失败，分别覆盖旧记录完全无 evidence 和有诊断但无原文槽。
        for with_diagnostics in [false, true] {
            let started = sessions
                .begin(SessionInput {
                    key: session.clone(),
                    text: "普通历史里存在的正文不能替代缺失的审核原文".into(),
                })
                .unwrap();
            sessions
                .fail(
                    &started.lease,
                    SessionFailure {
                        code: SessionFailureCode::Context,
                        started_tools: Some(0),
                        evidence: with_diagnostics.then(|| {
                            let mut evidence = evidence(Vec::new());
                            evidence.protocol_failure = None;
                            evidence
                        }),
                    },
                )
                .unwrap();
            let report = ControlReport {
                key: eve_control_api::GenerationKey {
                    session: session.clone(),
                    task_id: format!("web-request:{request_id}"),
                    controller_epoch: [1; 16],
                    generation: started.lease.turn_id,
                },
                cancel_requested: false,
                run: eve_control_api::RunReport {
                    turn_id: Some(started.lease.turn_id),
                    commit: CommitState::Failed,
                    text: None,
                    transcript: None,
                    started_tools: Some(0),
                    tool_results: Vec::new(),
                    failure: Some(RunFailure::Execution(source_error.clone())),
                },
            };
            let reason = recovery.recover(&report).unwrap().unwrap();
            assert!(reason.contains("未自动派发恢复任务"));
            assert!(tasks.snapshot().unwrap().tasks.is_empty());
        }
        kernel.stop_all().await.unwrap();
    }

    fn evidence(results: Vec<ToolResult>) -> SessionFailureEvidence {
        SessionFailureEvidence {
            review_user_messages: None,
            input_replies: vec![],
            provider_http_status: None,
            provider_failure: None,
            protocol_failure: Some(ProtocolFailureKind::StreamInvalid),
            tool_names: vec!["run_command".into(); results.len()],
            tool_results: results,
        }
    }

    #[test]
    fn all_eighteen_receipts_can_still_contain_one_unknown_effect() {
        for (code, label) in [
            (ToolFailureCode::TimedOut, "超时"),
            (ToolFailureCode::Cancelled, "被取消"),
            (ToolFailureCode::ExecutionFailed, "执行异常"),
        ] {
            let mut results: Vec<_> = (0..17)
                .map(|i| ToolResult::success(i.to_string(), json!({"ok":true})).unwrap())
                .collect();
            results.push(ToolResult::failure("18", code, "private command secret").unwrap());
            let e = evidence(results);
            let notice = recovery_blocker(18, Some(&e)).unwrap();
            assert!(
                notice.contains(&format!("第 18 条调用 run_command {label}")),
                "{notice}"
            );
            assert!(notice.contains("未自动派发"));
            assert!(!notice.contains("secret"));
        }
    }

    #[test]
    fn known_rejection_before_execution_does_not_imply_unknown_effects() {
        let e = evidence(vec![
            ToolResult::failure("rejected", ToolFailureCode::UnknownTool, "未执行").unwrap(),
        ]);
        assert_eq!(recovery_blocker(0, Some(&e)), None);
        let e = evidence(vec![
            ToolResult::failure("rejected", ToolFailureCode::InvalidArguments, "未执行").unwrap(),
        ]);
        assert_eq!(recovery_blocker(0, Some(&e)), None);
        assert_eq!(recovery_blocker(1, Some(&e)), None);
        let e = evidence(vec![ToolResult::success("ok", json!(true)).unwrap()]);
        assert_eq!(recovery_blocker(1, Some(&e)), None);
        assert!(
            recovery_blocker(0, Some(&e))
                .unwrap()
                .contains("记录不完整")
        );
    }

    #[test]
    fn missing_receipts_or_unsafe_tool_names_are_not_hidden() {
        let mut e = evidence(vec![]);
        e.tool_names.push("run_command".into());
        assert!(
            recovery_blocker(1, Some(&e))
                .unwrap()
                .contains("记录不完整")
        );
        let mut e = evidence(vec![
            ToolResult::failure("bad", ToolFailureCode::ExecutionFailed, "secret").unwrap(),
        ]);
        e.tool_names[0] = "secret\n@everyone".into();
        let notice = recovery_blocker(1, Some(&e)).unwrap();
        assert!(notice.contains("第 1 条调用 工具 执行异常"));
        assert!(!notice.contains("secret"));
    }
}
