use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::{ChatMessage, ChatRole, ContextScope, ToolCall, ToolResult, TurnCheckpoint};
use eve_task_api::*;
use eve_task_plugin::{
    PairedTaskStrategyPolicy, TaskStrategyPlugin, evaluate_task_effect, extract_task_strategy,
};
use serde_json::json;
use std::sync::Arc;

fn scope() -> TaskStrategyScope {
    TaskStrategyScope {
        owner: TaskOwner {
            user_id: "owner".into(),
            session_id: "session".into(),
        },
        family: "coding".into(),
        runtime_sha256: "a".repeat(64),
        evaluator_version: RECEIPT_EVALUATOR_VERSION.into(),
    }
}
fn contract() -> TaskEvaluationContract {
    TaskEvaluationContract {
        checks: vec![TaskReceiptCheck {
            tool: "verify".into(),
            arguments: json!({"target": "fixture"}),
            output_pointer: "/valid".into(),
            expected: json!(true),
        }],
        require_final_batch: true,
    }
}
fn batch(
    id: &str,
    name: &str,
    arguments: serde_json::Value,
    output: serde_json::Value,
) -> Vec<ChatMessage> {
    vec![
        ChatMessage::assistant_tool_calls(vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        }])
        .unwrap(),
        ChatMessage::tool_results(vec![ToolResult::success(id, output).unwrap()]).unwrap(),
    ]
}
fn task(corrective: bool, initial_valid: bool) -> Task {
    let mut messages = vec![ChatMessage::text(ChatRole::User, "完成当前任务")];
    messages.extend(batch(
        "before",
        "verify",
        json!({"target": "fixture"}),
        json!({"valid": initial_valid}),
    ));
    if corrective {
        messages.extend(batch(
            "fix",
            "edit_file",
            json!({"private": "do-not-copy-secret"}),
            json!({"private": "do-not-copy-output"}),
        ));
    }
    messages.extend(batch(
        "after",
        "verify",
        json!({"target": "fixture"}),
        json!({"valid": true}),
    ));
    messages.push(ChatMessage::text(ChatRole::Assistant, "完成"));
    let count = if corrective { 3 } else { 2 };
    Task {
        id: "training-1".into(),
        owner: scope().owner,
        spec: TaskSpec {
            title: "训练来源".into(),
            instructions: "修复示例".into(),
            max_steps: 50,
            context: None,
        },
        source: "trusted".into(),
        created_at_ms: 1,
        status: TaskStatus::Completed,
        cancel_requested: false,
        started_at_ms: Some(2),
        finished_at_ms: Some(3),
        usage: Some(TaskUsage {
            tool_calls: count as u32,
            model_requests: count as u32 + 1,
        }),
        result: Some("完成".into()),
        failure: None,
        notice: None,
        execution: Some(TaskExecution {
            checkpoint: Some(TurnCheckpoint {
                version: 1,
                scope: ContextScope {
                    user_id: "owner".into(),
                    session_id: "session".into(),
                },
                messages,
                pending_calls: vec![],
                used_tool_calls: count,
                delegated_tool_calls: 0,
                delegated_model_requests: 0,
                provider_requests: count + 1,
                tool_batches: count,
                remaining: None,
                continuation_checks: 0,
                closing: false,
                result: Some("完成".into()),
            }),
            ..Default::default()
        }),
    }
}
fn source(task: &Task) -> TaskStrategySource {
    TaskStrategySource {
        task_id: task.id.clone(),
        input_sha256: task_strategy_input(task),
        scope: scope(),
        contract: contract(),
        max_steps: task.spec.max_steps,
        observation: Some(evaluate_task_effect(task, &contract()).unwrap()),
        extracted: false,
    }
}
#[test]
fn extracts_only_receipted_correction_without_private_arguments_or_outputs() {
    let task = task(true, false);
    let proposal = extract_task_strategy(&task, &source(&task))
        .unwrap()
        .unwrap();
    assert!(proposal.guidance.contains("edit_file"));
    assert!(!proposal.guidance.contains("do-not-copy"));
    assert_eq!(proposal.source_input_sha256, task_strategy_input(&task));
}
#[test]
fn success_without_a_failed_check_or_without_corrective_action_is_not_experience() {
    for task in [task(true, true), task(false, false)] {
        assert!(
            extract_task_strategy(&task, &source(&task))
                .unwrap()
                .is_none()
        );
    }
}
#[test]
fn changed_or_cross_owner_source_is_rejected() {
    let task = task(true, false);
    let mut source = source(&task);
    source.scope.owner.user_id = "other".into();
    assert!(extract_task_strategy(&task, &source).is_err());
}
#[derive(Debug)]
struct Environment;
impl TaskStrategyEnvironment for Environment {
    fn scope(&self, _: &Task) -> TaskResult<Option<TaskStrategyScope>> {
        Ok(Some(scope()))
    }
}
fn policy(task: &Task) -> PairedTaskStrategyPolicy {
    let cases = ["b", "c"]
        .iter()
        .map(|c| TaskStrategyCase {
            scope: scope(),
            input_sha256: c.repeat(64),
            contract: contract(),
        })
        .collect();
    PairedTaskStrategyPolicy::new(Arc::new(Environment), vec![], cases)
        .unwrap()
        .with_training_cases(vec![TaskStrategyCase {
            scope: scope(),
            input_sha256: task_strategy_input(task),
            contract: contract(),
        }])
        .unwrap()
}
#[test]
fn training_inputs_cannot_be_used_for_held_out_promotion() {
    let task = task(true, false);
    let cases = vec![
        TaskStrategyCase {
            scope: scope(),
            input_sha256: task_strategy_input(&task),
            contract: contract(),
        },
        TaskStrategyCase {
            scope: scope(),
            input_sha256: "b".repeat(64),
            contract: contract(),
        },
    ];
    assert!(
        PairedTaskStrategyPolicy::new(Arc::new(Environment), vec![], cases)
            .unwrap()
            .with_training_cases(vec![TaskStrategyCase {
                scope: scope(),
                input_sha256: task_strategy_input(&task),
                contract: contract()
            }])
            .is_err()
    );
}
#[tokio::test]
async fn frozen_sources_generate_once_and_survive_plugin_restart() {
    let finished = task(true, false);
    let mut running = finished.clone();
    running.status = TaskStatus::Running;
    running.execution = None;
    running.result = None;
    running.finished_at_ms = None;
    running.usage = None;
    let directory = tempfile::tempdir().unwrap();
    let state = Arc::new(FileStateStore::open(directory.path()).unwrap());
    let snapshot = TaskSnapshot {
        tasks: vec![finished.clone()],
    };
    for pass in 0..2 {
        let kernel = Kernel::with_services(KernelServices {
            state: state.clone(),
            ..Default::default()
        });
        let plugin = TaskStrategyPlugin::new(Arc::new(policy(&finished))).unwrap();
        let strategies = plugin.controller();
        kernel.register(Box::new(plugin)).unwrap();
        kernel.start_all().await.unwrap();
        if pass == 0 {
            let guidance = strategies.prepare(&running).unwrap().unwrap();
            assert!(guidance.instructions.is_none());
            assert!(guidance.evaluation == contract());
        }
        let reader = EvidenceOnly {
            tasks: snapshot.clone(),
            requested: std::sync::Mutex::new(Vec::new()),
        };
        strategies.reconcile_from(&reader).unwrap();
        strategies.reconcile_from(&reader).unwrap();
        {
            let requested = reader.requested.lock().unwrap();
            assert_eq!(requested.len(), 2);
            assert!(requested.iter().all(|keys| keys
                == &vec![TaskEvidenceKey {
                    id: finished.id.clone(),
                    owner: finished.owner.clone()
                }]));
        }
        let stored = strategies.snapshot().unwrap();
        assert_eq!(stored.versions.len(), 1);
        assert_eq!(stored.sources.len(), 1);
        assert!(stored.sources[0].extracted);
        assert!(stored.selections.is_empty());
        kernel.stop_all().await.unwrap();
    }
}

// 新原生对账只能请求冻结来源；任何退回完整快照或执行操作立即失败。
struct EvidenceOnly {
    tasks: TaskSnapshot,
    requested: std::sync::Mutex<Vec<Vec<TaskEvidenceKey>>>,
}
impl TaskAdmin for EvidenceOnly {
    fn snapshot(&self) -> TaskResult<TaskSnapshot> {
        panic!("native reconciliation must not load full task bodies")
    }
    fn evidence_snapshot(&self, keys: &[TaskEvidenceKey]) -> TaskResult<TaskEvidenceSnapshot> {
        self.requested.lock().unwrap().push(keys.to_vec());
        TaskEvidenceSnapshot::select(&self.tasks.tasks, keys)
    }
    fn create(&self, _: &TaskOwner, _: TaskSpec, _: &str, _: u64) -> TaskResult<Task> {
        panic!("no execution")
    }
    fn start(&self, _: &str, _: u64) -> TaskResult<Task> {
        panic!("no execution")
    }
    fn checkpoint(&self, _: &str, _: TurnCheckpoint, _: u64, _: u64) -> TaskResult<Task> {
        panic!("no execution")
    }
    fn retry(&self, _: &str, _: u64) -> TaskResult<Task> {
        panic!("no execution")
    }
    fn finish(&self, _: &str, _: TaskEnd, _: TaskUsage, _: u64) -> TaskResult<Task> {
        panic!("no execution")
    }
    fn cancel(&self, _: &str, _: &str, _: u64) -> TaskResult<Task> {
        panic!("no execution")
    }
    fn claim(&self, _: &str, _: DeliveryChannel, _: u64) -> TaskResult<Task> {
        panic!("no delivery")
    }
    fn record_delivery(&self, _: &str, _: DeliveryResult, _: u64) -> TaskResult<Task> {
        panic!("no delivery")
    }
    fn expire(&self, _: &str, _: u64) -> TaskResult<Task> {
        panic!("no delivery")
    }
}
