//! 任务策略与环境能力的组合层：只投影宿主已冻结的检查，不自行发起执行。
use eve_cognition_api::*;
use eve_llm_api::ToolOutput;
use eve_memory_api::UnifiedMemoryAdmin;
use eve_task_api::*;
use std::{fmt, sync::Arc};

/// 可信嵌入宿主的管理入口，不向模型工具或聊天参数暴露管理句柄。
/// 回调可装配显式跨通道授权及环境观测，失败会阻止启动。
pub trait CognitiveServices: Send + Sync + fmt::Debug {
    fn memory_ready(&self, _memory: Arc<dyn UnifiedMemoryAdmin>) -> Result<(), crate::AppError> {
        Ok(())
    }
    fn world_ready(&self, _world: Arc<dyn WorldModelAdmin>) -> Result<(), crate::AppError> {
        Ok(())
    }
}

pub(crate) struct TaskCognition {
    pub strategies: Arc<dyn TaskStrategyLearning>,
    pub world: Arc<dyn WorldModelAdmin>,
}
fn world_error(error: CognitionError) -> TaskError {
    match error {
        CognitionError::LimitReached => TaskError::LimitReached,
        _ => TaskError::Unavailable,
    }
}
fn scope(scope: &TaskStrategyScope, contract: &TaskEvaluationContract) -> WorldScope {
    WorldScope {
        // 相同工具在不同验收目标上的成绩不能混成同一个成功率。
        environment_sha256: task_strategy_digest(&(scope, contract)),
        capability: scope.family.clone(),
        visibility: Visibility::User(scope.owner.user_id.clone()),
    }
}
fn projected_pointer(check: &TaskReceiptCheck) -> String {
    format!("/observed/{}", task_strategy_digest(&check.output_pointer))
}
fn execution_id(task: &str) -> String {
    format!("eve.task/{}", task_strategy_digest(&task))
}
fn prediction_id(task: &str, index: usize) -> String {
    format!("task-{}", task_strategy_digest(&(task, index)))
}

impl TaskStrategyLearning for TaskCognition {
    fn snapshot(&self) -> TaskResult<TaskStrategySnapshot> {
        self.strategies.snapshot()
    }
    fn propose(&self, proposal: TaskStrategyProposal) -> TaskResult<TaskStrategyVersion> {
        self.strategies.propose(proposal)
    }
    fn prepare(&self, task: &Task) -> TaskResult<Option<TaskStrategyGuidance>> {
        let Some(mut guidance) = self.strategies.prepare(task)? else {
            return Ok(None);
        };
        let ledger = self.strategies.snapshot()?;
        let frozen = ledger
            .selections
            .iter()
            .find(|s| s.task_id == task.id)
            .map(|s| (&s.assignment.scope, &s.assignment.contract))
            .or_else(|| {
                ledger
                    .sources
                    .iter()
                    .find(|s| s.task_id == task.id)
                    .map(|s| (&s.scope, &s.contract))
            });
        let Some((strategy_scope, contract)) = frozen else {
            return Err(TaskError::CorruptState);
        };
        let scope = scope(strategy_scope, contract);
        let estimates = self
            .world
            .reader(ReadAccess::User(task.owner.user_id.clone()))
            .map_err(world_error)?
            .capabilities(&scope.environment_sha256)
            .map_err(world_error)?;
        let estimate = estimates.iter().find(|e| e.scope == scope);
        let probability = estimate.map_or(50, |e| e.success_probability_percent);
        // 重启后只能沿用已写入的预测，不允许执行后补登记预测。
        let started = task.started_at_ms.ok_or(TaskError::InvalidTransition)?;
        let fresh = task
            .execution
            .as_ref()
            .is_none_or(|e| e.checkpoint.is_none() && e.recoveries == 0);
        let current = self.world.snapshot().map_err(world_error)?;
        for (index, check) in contract.checks.iter().enumerate() {
            let id = prediction_id(&task.id, index);
            if let Some(existing) = current
                .trials
                .iter()
                .find(|trial| trial.prediction.id == id)
            {
                let old = &existing.prediction;
                if old.execution_id != execution_id(&task.id)
                    || old.scope != scope
                    || old.created_at_ms != started
                    || old.check.tool != check.tool
                    || old.check.arguments_sha256 != task_strategy_digest(&check.arguments)
                    || old.check.output_pointer != projected_pointer(check)
                    || old.check.expected != check.expected
                {
                    return Err(TaskError::CorruptState);
                }
                continue;
            }
            if !fresh {
                continue;
            }
            let prediction = WorldPrediction {
                id,
                execution_id: execution_id(&task.id),
                scope: scope.clone(),
                observation_ids: vec![],
                check: WorldCheck {
                    tool: check.tool.clone(),
                    arguments_sha256: task_strategy_digest(&check.arguments),
                    output_pointer: projected_pointer(check),
                    expected: check.expected.clone(),
                },
                success_probability_percent: probability,
                created_at_ms: started,
                deadline_ms: task
                    .execution
                    .as_ref()
                    .and_then(|e| e.deadline_at_ms)
                    .filter(|deadline| *deadline > started)
                    .unwrap_or_else(|| started.saturating_add(600_000)),
            };
            match self.world.predict(prediction) {
                Ok(()) => {}
                // 能力账本满时不增加预算、不删除旧证据；原任务仍可执行。
                Err(CognitionError::LimitReached) => break,
                Err(error) => return Err(world_error(error)),
            }
        }
        // 双臂试验只允许策略版本不同，不能再混入随时间变化的能力提示。
        let paired_trial = ledger.selections.iter().any(|selection| {
            selection.task_id == task.id
                && matches!(
                    selection.assignment.arm,
                    TaskStrategyArm::Control { .. } | TaskStrategyArm::Trial { .. }
                )
        });
        if !paired_trial && let Some(estimate) = estimate {
            let note = format!(
                "\n同一身份、环境与验收条件的宿主能力记录：通过 {}，不符 {}，不确定 {}，平滑成功率 {}%。这是历史统计，不能代替本轮验收、授权或预算。",
                estimate.verified,
                estimate.failed,
                estimate.inconclusive,
                estimate.success_probability_percent
            );
            guidance
                .instructions
                .get_or_insert_with(String::new)
                .push_str(&note);
        }
        Ok(Some(guidance))
    }
    fn reconcile(&self, tasks: &TaskSnapshot) -> TaskResult<()> {
        self.strategies.reconcile(tasks)?;
        let ledger = self.strategies.snapshot()?;
        let world = self.world.snapshot().map_err(world_error)?;
        for trial in world.trials.iter().filter(|t| t.resolution.is_none()) {
            // 世界账本还可保存其他宿主能力；只结算本适配器创建且策略账本仍能绑定的预测。
            let frozen = ledger
                .selections
                .iter()
                .map(|s| {
                    (
                        s.task_id.as_str(),
                        &s.assignment.scope,
                        &s.assignment.contract,
                        s.observation.as_ref(),
                    )
                })
                .chain(ledger.sources.iter().map(|s| {
                    (
                        s.task_id.as_str(),
                        &s.scope,
                        &s.contract,
                        s.observation.as_ref(),
                    )
                }))
                .find(|(id, _, contract, _)| {
                    execution_id(id) == trial.prediction.execution_id
                        && contract
                            .checks
                            .iter()
                            .enumerate()
                            .any(|(i, _)| prediction_id(id, i) == trial.prediction.id)
                });
            let Some((task_id, strategy_scope, contract, observation)) = frozen else {
                continue;
            };
            let index = contract
                .checks
                .iter()
                .enumerate()
                .find(|(i, _)| prediction_id(task_id, *i) == trial.prediction.id)
                .ok_or(TaskError::CorruptState)?
                .0;
            let check = &contract.checks[index];
            let prediction = &trial.prediction;
            if prediction.scope != scope(strategy_scope, contract)
                || prediction.check.tool != check.tool
                || prediction.check.arguments_sha256 != task_strategy_digest(&check.arguments)
                || prediction.check.output_pointer != projected_pointer(check)
                || prediction.check.expected != check.expected
            {
                return Err(TaskError::CorruptState);
            }
            let Some(task) = tasks.tasks.iter().find(|t| t.id == task_id) else {
                // 来源已归档且无法核对时，只封存不确定；不重放历史任务。
                let at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| TaskError::Unavailable)?
                    .as_millis()
                    .min(u64::MAX as u128) as u64;
                self.world
                    .resolve(&prediction.id, None, at.max(prediction.created_at_ms))
                    .map_err(world_error)?;
                continue;
            };
            if task.owner != strategy_scope.owner
                || task.started_at_ms != Some(prediction.created_at_ms)
            {
                return Err(TaskError::CorruptState);
            }
            if !task.status.finished() {
                continue;
            }
            let at = task
                .finished_at_ms
                .unwrap_or(prediction.created_at_ms.saturating_add(1));
            let receipt = observation
                .filter(|o| o.effect != TaskEffect::Inconclusive)
                .and_then(|_| {
                    project_receipt(task, check, &prediction.scope.environment_sha256, at)
                });
            self.world
                .resolve(&trial.prediction.id, receipt, at)
                .map_err(world_error)?;
        }
        Ok(())
    }
}

fn project_receipt(
    task: &Task,
    check: &TaskReceiptCheck,
    environment_sha256: &str,
    at_ms: u64,
) -> Option<WorldReceipt> {
    let checkpoint = task.execution.as_ref()?.checkpoint.as_ref()?;
    let batch = checkpoint
        .messages
        .iter()
        .rev()
        .find(|m| !m.tool_calls.is_empty())?;
    let call = batch
        .tool_calls
        .iter()
        .find(|c| c.name == check.tool && c.arguments == check.arguments)?;
    let receipt = checkpoint
        .messages
        .iter()
        .flat_map(|m| &m.tool_results)
        .find(|r| r.call_id == call.id)?;
    let ToolOutput::Success(output) = &receipt.output else {
        return None;
    };
    let observed = output.pointer(&check.output_pointer)?;
    // 仅持久化固定检查字段，不复制整份工具回执或模型正文。
    if serde_json::to_vec(observed).ok()?.len() > 4096 {
        return None;
    }
    Some(WorldReceipt {
        environment_sha256: environment_sha256.into(),
        execution_id: execution_id(&task.id),
        source: Source {
            kind: SourceKind::Tool,
            channel: "task".into(),
            reference: format!(
                "receipt-{}",
                task_strategy_digest(&(task.id.as_str(), call.id.as_str()))
            ),
        },
        tool: call.name.clone(),
        arguments_sha256: task_strategy_digest(&call.arguments),
        output: serde_json::json!({"observed": {task_strategy_digest(&check.output_pointer): observed}}),
        succeeded: true,
        final_batch: true,
        at_ms,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use eve_cognition_plugin::{WorldModelController, WorldModelPlugin};
    use eve_kernel::Kernel;
    use eve_llm_api::{ChatMessage, ChatRole, ContextScope, ToolCall, ToolResult, TurnCheckpoint};
    use eve_task_plugin::{TaskStrategyController, TaskStrategyPlugin};
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn task_scope(changed: bool) -> TaskStrategyScope {
        TaskStrategyScope {
            owner: TaskOwner {
                user_id: "alice".into(),
                session_id: "private-session".into(),
            },
            family: "verify-code".into(),
            runtime_sha256: if changed { "b" } else { "a" }.repeat(64),
            evaluator_version: RECEIPT_EVALUATOR_VERSION.into(),
        }
    }
    fn contract() -> TaskEvaluationContract {
        TaskEvaluationContract {
            checks: vec![TaskReceiptCheck {
                tool: "verify".into(),
                arguments: json!({"target":"fixture"}),
                output_pointer: "/checks/passed".into(),
                expected: json!(true),
            }],
            require_final_batch: true,
        }
    }
    struct Policy {
        changed: AtomicBool,
        contract: TaskEvaluationContract,
    }
    impl fmt::Debug for Policy {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("Policy(<redacted>)")
        }
    }
    impl TaskStrategyPolicy for Policy {
        fn scope(&self, _: &Task) -> TaskResult<Option<TaskStrategyScope>> {
            Ok(Some(task_scope(self.changed.load(Ordering::SeqCst))))
        }
        fn source_contract(&self, _: &Task) -> TaskResult<Option<TaskEvaluationContract>> {
            Ok(Some(self.contract.clone()))
        }
        fn assign(
            &self,
            _: &Task,
            _: &TaskStrategySnapshot,
        ) -> TaskResult<Option<TaskStrategyAssignment>> {
            Ok(None)
        }
    }
    async fn open(
        contract: TaskEvaluationContract,
    ) -> (
        Kernel,
        TaskCognition,
        TaskStrategyController,
        WorldModelController,
        Arc<Policy>,
    ) {
        let kernel = Kernel::new();
        let policy = Arc::new(Policy {
            changed: AtomicBool::new(false),
            contract,
        });
        let strategies = TaskStrategyPlugin::new(policy.clone()).unwrap();
        let strategy_admin = strategies.controller();
        let world = WorldModelPlugin::new("eve").unwrap();
        let world_admin = world.controller();
        kernel.register(Box::new(strategies)).unwrap();
        kernel.register(Box::new(world)).unwrap();
        kernel.start_all().await.unwrap();
        let combined = TaskCognition {
            strategies: Arc::new(strategy_admin.clone()),
            world: Arc::new(world_admin.clone()),
        };
        (kernel, combined, strategy_admin, world_admin, policy)
    }
    fn running() -> Task {
        Task {
            id: "task-1".into(),
            owner: task_scope(false).owner,
            spec: TaskSpec {
                title: "核验".into(),
                instructions: "核验固定目标".into(),
                max_steps: 50,
                context: None,
            },
            source: "trusted".into(),
            created_at_ms: 1,
            status: TaskStatus::Running,
            cancel_requested: false,
            started_at_ms: Some(2),
            finished_at_ms: None,
            usage: None,
            result: None,
            failure: None,
            notice: None,
            execution: None,
        }
    }
    fn finish(mut task: Task, output: serde_json::Value) -> Task {
        task.status = TaskStatus::Completed;
        task.finished_at_ms = Some(4);
        task.result = Some("完成".into());
        task.usage = Some(TaskUsage {
            tool_calls: 1,
            model_requests: 2,
        });
        task.execution = Some(TaskExecution {
            checkpoint: Some(TurnCheckpoint {
                version: 1,
                scope: ContextScope {
                    user_id: task.owner.user_id.clone(),
                    session_id: task.owner.session_id.clone(),
                },
                messages: vec![
                    ChatMessage::text(ChatRole::User, "核验"),
                    ChatMessage::assistant_tool_calls(vec![ToolCall {
                        id: "call-1".into(),
                        name: "verify".into(),
                        arguments: json!({"target":"fixture"}),
                    }])
                    .unwrap(),
                    ChatMessage::tool_results(vec![ToolResult::success("call-1", output).unwrap()])
                        .unwrap(),
                    ChatMessage::text(ChatRole::Assistant, "完成"),
                ],
                pending_calls: vec![],
                used_tool_calls: 1,
                delegated_tool_calls: 0,
                delegated_model_requests: 0,
                provider_requests: 2,
                tool_batches: 1,
                remaining: None,
                continuation_checks: 0,
                closing: false,
                result: Some("完成".into()),
            }),
            ..Default::default()
        });
        task
    }
    fn foreign_prediction(id: &str) -> WorldPrediction {
        WorldPrediction {
            id: id.into(),
            execution_id: format!("external/{id}"),
            scope: WorldScope {
                environment_sha256: "f".repeat(64),
                capability: "external".into(),
                visibility: Visibility::Internal,
            },
            observation_ids: vec![],
            check: WorldCheck {
                tool: "check".into(),
                arguments_sha256: "e".repeat(64),
                output_pointer: "/passed".into(),
                expected: json!(true),
            },
            success_probability_percent: 50,
            created_at_ms: 1,
            deadline_ms: 100,
        }
    }
    #[tokio::test]
    async fn fixed_field_receipts_are_private_and_idempotent() {
        let (kernel, cognition, _, world, _) = open(contract()).await;
        let task = running();
        cognition.prepare(&task).unwrap().unwrap();
        let before = world.snapshot().unwrap();
        assert_eq!(before.trials.len(), 1);
        assert!(before.trials[0].resolution.is_none());
        let task = finish(
            task,
            json!({"checks":{"passed":true}, "private":"private-receipt-must-not-copy"}),
        );
        let tasks = TaskSnapshot { tasks: vec![task] };
        cognition.reconcile(&tasks).unwrap();
        let settled = world.snapshot().unwrap();
        cognition.reconcile(&tasks).unwrap();
        assert_eq!(settled, world.snapshot().unwrap());
        let trial = &settled.trials[0];
        let resolution = trial.resolution.as_ref().unwrap();
        assert_eq!(resolution.effect, WorldEffect::Verified);
        let receipt = resolution.receipt.as_ref().unwrap();
        assert_eq!(
            receipt
                .output
                .pointer(&trial.prediction.check.output_pointer),
            Some(&json!(true))
        );
        assert!(
            !serde_json::to_string(&settled)
                .unwrap()
                .contains("private-receipt-must-not-copy")
        );
        let environment = &trial.prediction.scope.environment_sha256;
        assert!(
            world
                .reader(ReadAccess::User("bob".into()))
                .unwrap()
                .capabilities(environment)
                .unwrap()
                .is_empty()
        );
        assert!(
            world
                .reader(ReadAccess::Public)
                .unwrap()
                .capabilities(environment)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            world
                .reader(ReadAccess::User("alice".into()))
                .unwrap()
                .capabilities(environment)
                .unwrap()[0]
                .verified,
            1
        );
        kernel.stop_all().await.unwrap();
    }
    #[tokio::test]
    async fn environment_change_is_unknown_not_capability_success() {
        let (kernel, cognition, _, world, policy) = open(contract()).await;
        let task = running();
        cognition.prepare(&task).unwrap();
        policy.changed.store(true, Ordering::SeqCst);
        cognition
            .reconcile(&TaskSnapshot {
                tasks: vec![finish(task, json!({"checks":{"passed":true}}))],
            })
            .unwrap();
        let world = world.snapshot().unwrap();
        let resolution = world.trials[0].resolution.as_ref().unwrap();
        assert_eq!(resolution.effect, WorldEffect::Inconclusive);
        assert!(resolution.receipt.is_none());
        kernel.stop_all().await.unwrap();
    }
    #[tokio::test]
    async fn recovery_never_backfills_missing_prediction() {
        let (kernel, cognition, strategies, world, _) = open(contract()).await;
        let mut task = running();
        strategies.prepare(&task).unwrap();
        task.execution = Some(TaskExecution {
            recoveries: 1,
            ..Default::default()
        });
        assert!(cognition.prepare(&task).unwrap().is_some());
        assert!(world.snapshot().unwrap().trials.is_empty());
        cognition
            .reconcile(&TaskSnapshot {
                tasks: vec![finish(task, json!({"checks":{"passed":true}}))],
            })
            .unwrap();
        assert!(world.snapshot().unwrap().trials.is_empty());
        kernel.stop_all().await.unwrap();
    }
    #[tokio::test]
    async fn foreign_predictions_are_not_closed_by_task_reconciliation() {
        let (kernel, cognition, _, world, _) = open(contract()).await;
        world.predict(foreign_prediction("other-host")).unwrap();
        cognition
            .reconcile(&TaskSnapshot { tasks: vec![] })
            .unwrap();
        assert!(world.snapshot().unwrap().trials[0].resolution.is_none());
        kernel.stop_all().await.unwrap();
    }
    #[tokio::test]
    async fn full_world_ledger_does_not_block_original_task() {
        let (kernel, cognition, _, world, _) = open(contract()).await;
        for i in 0..MAX_WORLD_RECORDS {
            world
                .predict(foreign_prediction(&format!("external-{i}")))
                .unwrap();
        }
        assert!(cognition.prepare(&running()).unwrap().is_some());
        assert_eq!(world.snapshot().unwrap().trials.len(), MAX_WORLD_RECORDS);
        kernel.stop_all().await.unwrap();
    }
    #[tokio::test]
    async fn conflicting_prediction_identity_is_rejected_even_on_recovery() {
        let (kernel, cognition, strategies, world, _) = open(contract()).await;
        let mut task = running();
        strategies.prepare(&task).unwrap();
        let mut conflict = foreign_prediction(&prediction_id(&task.id, 0));
        conflict.execution_id = execution_id(&task.id);
        world.predict(conflict).unwrap();
        task.execution = Some(TaskExecution {
            recoveries: 1,
            ..Default::default()
        });
        assert!(matches!(
            cognition.prepare(&task),
            Err(TaskError::CorruptState)
        ));
        let tasks = TaskSnapshot {
            tasks: vec![finish(task, json!({"checks":{"passed":true}}))],
        };
        assert_eq!(cognition.reconcile(&tasks), Err(TaskError::CorruptState));
        assert!(world.snapshot().unwrap().trials[0].resolution.is_none());
        kernel.stop_all().await.unwrap();
    }
    #[tokio::test]
    async fn distinct_fields_in_one_tool_result_are_distinct_predictions() {
        let mut contract = contract();
        let mut other = contract.checks[0].clone();
        other.output_pointer = "/checks/other".into();
        contract.checks.push(other);
        let (kernel, cognition, _, world, _) = open(contract).await;
        let task = running();
        assert!(cognition.prepare(&task).unwrap().is_some());
        assert_eq!(world.snapshot().unwrap().trials.len(), 2);
        cognition
            .reconcile(&TaskSnapshot {
                tasks: vec![finish(
                    task,
                    json!({"checks":{"passed":true,"other":false}}),
                )],
            })
            .unwrap();
        let state = world.snapshot().unwrap();
        assert_eq!(
            state.trials[0].resolution.as_ref().unwrap().effect,
            WorldEffect::Verified
        );
        assert_eq!(
            state.trials[1].resolution.as_ref().unwrap().effect,
            WorldEffect::Failed
        );
        kernel.stop_all().await.unwrap();
    }
    #[derive(Debug)]
    struct FixedEnvironment;
    impl TaskStrategyEnvironment for FixedEnvironment {
        fn scope(&self, _: &Task) -> TaskResult<Option<TaskStrategyScope>> {
            Ok(Some(task_scope(false)))
        }
    }
    #[tokio::test]
    async fn paired_arms_keep_dynamic_capability_notes_out_but_record_predictions() {
        use eve_task_plugin::PairedTaskStrategyPolicy;
        let kernel = Kernel::new();
        let control = running();
        let proposal = TaskStrategyProposal {
            scope: task_scope(false),
            source_id: "seed-strategy".into(),
            source_input_sha256: "d".repeat(64),
            source_evidence_sha256: "e".repeat(64),
            guidance: "先核对差异，再验证结果".into(),
        };
        let cases = vec![
            TaskStrategyCase {
                scope: task_scope(false),
                input_sha256: task_strategy_input(&control),
                contract: contract(),
            },
            TaskStrategyCase {
                scope: task_scope(false),
                input_sha256: "c".repeat(64),
                contract: contract(),
            },
        ];
        let policy = PairedTaskStrategyPolicy::new(
            Arc::new(FixedEnvironment),
            vec![proposal.clone()],
            cases,
        )
        .unwrap();
        let strategies = TaskStrategyPlugin::new(Arc::new(policy)).unwrap();
        let strategy_admin = strategies.controller();
        let plugin = WorldModelPlugin::new("eve").unwrap();
        let world = plugin.controller();
        kernel.register(Box::new(strategies)).unwrap();
        kernel.register(Box::new(plugin)).unwrap();
        kernel.start_all().await.unwrap();
        let cognition = TaskCognition {
            strategies: Arc::new(strategy_admin),
            world: Arc::new(world.clone()),
        };
        let mut seed = foreign_prediction("prior-evidence");
        seed.scope = scope(&task_scope(false), &contract());
        world.predict(seed.clone()).unwrap();
        world
            .resolve(
                &seed.id,
                Some(WorldReceipt {
                    execution_id: seed.execution_id.clone(),
                    environment_sha256: seed.scope.environment_sha256.clone(),
                    source: Source {
                        kind: SourceKind::Tool,
                        channel: "host".into(),
                        reference: "prior-check".into(),
                    },
                    tool: seed.check.tool.clone(),
                    arguments_sha256: seed.check.arguments_sha256.clone(),
                    output: json!({"passed":true}),
                    succeeded: true,
                    final_batch: true,
                    at_ms: 2,
                }),
                2,
            )
            .unwrap();
        let guidance = cognition.prepare(&control).unwrap().unwrap();
        assert!(guidance.instructions.is_none());
        let state = world.snapshot().unwrap();
        assert_eq!(state.trials.len(), 2);
        assert!(
            state
                .trials
                .iter()
                .any(|t| t.prediction.id == prediction_id(&control.id, 0))
        );
        cognition
            .reconcile(&TaskSnapshot {
                tasks: vec![finish(control.clone(), json!({"checks":{"passed":true}}))],
            })
            .unwrap();
        let mut trial = control;
        trial.id = "trial-2".into();
        let guidance = cognition.prepare(&trial).unwrap().unwrap();
        assert_eq!(
            guidance.instructions.as_deref(),
            Some(proposal.guidance.as_str())
        );
        let state = world.snapshot().unwrap();
        assert_eq!(state.trials.len(), 3);
        assert!(
            state
                .trials
                .iter()
                .any(|t| t.prediction.id == prediction_id(&trial.id, 0))
        );
        kernel.stop_all().await.unwrap();
    }
}
