use super::*;
use eve_llm_api::{ChatMessage, ToolResult};
use eve_task_api::{TaskFrozenNodeOutcome, TaskResult, task_strategy_digest};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
struct EvidenceSource {
    evidence: Mutex<Vec<WorkspaceNodeEvidence>>,
    captures: AtomicUsize,
    fail: AtomicBool,
}
impl WorkspaceNodeEvidenceSource for EvidenceSource {
    fn capture(
        &self,
        _owner: &TaskOwner,
        _frame: &TurnCheckpoint,
    ) -> TaskResult<Vec<WorkspaceNodeEvidence>> {
        self.captures.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(eve_task_api::TaskError::Unavailable);
        }
        Ok(self.evidence.lock().unwrap().clone())
    }
}

fn frozen() -> FrozenRun {
    let mut frozen = tests::frozen();
    frozen.steps = 8;
    frozen.verification = TaskCompletionContract::WorkspaceNodeTest {
        workspace_sha256: "4".repeat(64),
        directory: "synthetic-private-directory".into(),
        entry: "test.cjs".into(),
        args: vec!["synthetic-private-argument".into()],
        runtime_sha256: "2".repeat(64),
        command_sha256: "3".repeat(64),
    };
    frozen
}

fn open(path: &std::path::Path, source: Arc<EvidenceSource>) -> Arc<RunStore> {
    RunStore::open_with_workspace_evidence(
        Arc::new(SqliteRunStorage::open(path).unwrap()),
        None,
        Some(source),
    )
}

fn append(frame: &mut TurnCheckpoint, call: ToolCall, receipt: ToolResult) {
    frame
        .messages
        .push(ChatMessage::assistant_tool_calls(vec![call]).unwrap());
    frame
        .messages
        .push(ChatMessage::tool_results(vec![receipt]).unwrap());
    frame.used_tool_calls += 1;
    frame.tool_batches += 1;
    frame.provider_requests += 1;
}

fn tested(frozen: &FrozenRun) -> (TurnCheckpoint, WorkspaceNodeEvidence) {
    let mut frame = tests::frame(frozen, true);
    frame.pending_calls.clear();
    frame.used_tool_calls = 0;
    frame.tool_batches = 0;
    frame.provider_requests = 0;
    let call = ToolCall {
        id: "tested-once".into(),
        name: eve_task_api::WORKSPACE_NODE_TEST_TOOL.into(),
        arguments: json!({
            "directory":"synthetic-private-directory",
            "entry":"test.cjs",
            "args":["synthetic-private-argument"],
        }),
    };
    let receipt = ToolResult::success(
        "tested-once",
        json!({"stdout":"synthetic-private-output","exit_code":0}),
    )
    .unwrap();
    let evidence = WorkspaceNodeEvidence {
        owner: frozen.owner.clone(),
        model_session: frozen.model_session.clone(),
        call_id: call.id.clone(),
        arguments_sha256: task_strategy_digest(&call.arguments),
        tool_result_sha256: task_strategy_digest(&receipt),
        input_sha256: "1".repeat(64),
        workspace_sha256: "4".repeat(64),
        directory: "synthetic-private-directory".into(),
        entry: "test.cjs".into(),
        args: vec!["synthetic-private-argument".into()],
        runtime_sha256: "2".repeat(64),
        command_sha256: "3".repeat(64),
        outcome: TaskFrozenNodeOutcome::Exited {
            exit_code: Some(0),
            signal: None,
        },
    };
    append(&mut frame, call, receipt);
    (frame, evidence)
}

fn current(frozen: &FrozenRun, frame: &TurnCheckpoint) -> WorkspaceObservation {
    WorkspaceObservation {
        owner: frozen.owner.clone(),
        model_session: frozen.model_session.clone(),
        checkpoint_sha256: eve_task_api::workspace_node_checkpoint_sha256(frame),
        workspace_sha256: "4".repeat(64),
        directory: "synthetic-private-directory".into(),
        entry: "test.cjs".into(),
        args: vec!["synthetic-private-argument".into()],
        input_sha256: "1".repeat(64),
        observed_at_ms: 1000,
    }
}

fn candidate() -> WorkerRun {
    WorkerRun {
        end: TaskEnd::Completed {
            result: "任务结果".into(),
        },
        usage: TaskUsage::default(),
        verification: Some(TaskCompletionAssessment::Verified),
        retryable: false,
    }
}

#[test]
fn one_test_survives_reads_progress_and_restart_with_historical_settlement_only() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runs.sqlite3");
    let source = Arc::new(EvidenceSource::default());
    let store = open(&path, source.clone());
    let frozen = frozen();
    let journal = store.begin(frozen.clone()).unwrap();
    let id = journal.id.clone();
    let (mut frame, evidence) = tested(&frozen);
    *source.evidence.lock().unwrap() = vec![evidence.clone()];
    journal.save(&frame).unwrap();
    assert!(journal.workspace_observation_target().unwrap().is_none());
    source.evidence.lock().unwrap().clear();
    for tool in ["read_file", "task_progress"] {
        let call = ToolCall {
            id: tool.into(),
            name: tool.into(),
            arguments: json!({"synthetic":"private-body"}),
        };
        append(
            &mut frame,
            call,
            ToolResult::success(tool, json!({"status":"done"})).unwrap(),
        );
        journal.save(&frame).unwrap();
    }
    frame.result = Some("任务结果".into());
    journal.save(&frame).unwrap();
    let target = journal.workspace_observation_target().unwrap().unwrap();
    assert!(target.contract == frozen.verification);
    assert_eq!(target.owner, frozen.owner);
    assert_eq!(target.model_session, frozen.model_session);
    assert_eq!(
        target.checkpoint_sha256,
        current(&frozen, &frame).checkpoint_sha256
    );
    source.fail.store(true, Ordering::SeqCst);
    let mut run = candidate();
    journal
        .finish_with_workspace_observation(
            &mut run,
            &target.checkpoint_sha256,
            Some(current(&frozen, &frame)),
        )
        .unwrap();
    assert!(matches!(run.end, TaskEnd::Completed { .. }));
    assert_eq!(run.verification, Some(TaskCompletionAssessment::Verified));
    assert_eq!(source.captures.load(Ordering::SeqCst), 4);
    let before = store.backend.read(&id).unwrap();
    drop(journal);
    drop(store);

    let reopened = tests::open(&path);
    let saved = reopened.read(&id).unwrap().unwrap();
    assert_eq!(saved.workspace_node_evidence, vec![evidence]);
    assert_eq!(saved.checkpoint, Some(frame.clone()));
    assert_eq!(saved.workspace_observation, Some(current(&frozen, &frame)));
    let view = reopened.evidence_view(&frozen.owner, &id).unwrap();
    let row = &view["runs"][0];
    assert_eq!(row["verification"], "verified");
    assert_eq!(row["current_artifact_validity"], "not_checked");
    assert_eq!(row["automatic_resume"], false);
    assert_eq!(row["report"]["test_evidence"].as_array().unwrap().len(), 1);
    assert_eq!(
        row["report"]["test_evidence"][0]["matches_input_at_settlement"],
        true
    );
    assert!(!view.to_string().contains("synthetic-private"));
    assert!(!view.to_string().contains("private-body"));
    let stranger = TaskOwner {
        user_id: "stranger".into(),
        ..frozen.owner.clone()
    };
    assert_eq!(
        reopened.evidence_view(&stranger, &id).unwrap()["runs"],
        json!([])
    );
    assert!(reopened.begin(frozen).is_err());
    assert_eq!(reopened.backend.read(&id).unwrap(), before);
    assert_eq!(source.captures.load(Ordering::SeqCst), 4);
}

#[test]
fn changed_missing_or_unknown_evidence_never_upgrades_model_claims() {
    for case in [
        "changed",
        "missing_observation",
        "missing_witness",
        "unknown",
        "cancelled",
        "extra_usage",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runs.sqlite3");
        let source = Arc::new(EvidenceSource::default());
        let store = open(&path, source.clone());
        let frozen = frozen();
        let journal = store.begin(frozen.clone()).unwrap();
        let (mut frame, mut evidence) = tested(&frozen);
        frame.result = Some("任务结果".into());
        if case == "unknown" {
            evidence.outcome = TaskFrozenNodeOutcome::Unknown;
        }
        if case != "missing_witness" {
            *source.evidence.lock().unwrap() = vec![evidence];
        }
        journal.save(&frame).unwrap();
        let target = journal.workspace_observation_target().unwrap().unwrap();
        let mut observation = current(&frozen, &frame);
        if case == "changed" {
            observation.input_sha256 = "9".repeat(64);
        }
        let mut run = candidate();
        if case == "cancelled" {
            run.end = TaskEnd::Cancelled;
        }
        if case == "extra_usage" {
            run.usage.tool_calls = 7;
        }
        if case == "missing_observation" {
            journal.finish(&mut run).unwrap();
        } else {
            journal
                .finish_with_workspace_observation(
                    &mut run,
                    &target.checkpoint_sha256,
                    Some(observation),
                )
                .unwrap();
        }
        assert!(!run.verification.as_ref().unwrap().verified(), "{case}");
        assert!(!matches!(run.end, TaskEnd::Completed { .. }), "{case}");
        let record = store.read(&journal.id).unwrap().unwrap();
        assert!(!matches!(record.end, Some(End::Completed { .. })), "{case}");
        if case == "changed" {
            assert_eq!(
                run.verification,
                Some(TaskCompletionAssessment::Mismatch {
                    reason: "workspace_changed_since_test".into()
                })
            );
            assert_eq!(
                store.evidence_view(&frozen.owner, &journal.id).unwrap()["runs"][0]["report"]["test_evidence"]
                    [0]["matches_input_at_settlement"],
                false
            );
        }
        if case == "cancelled" {
            assert!(matches!(record.end, Some(End::Cancelled)));
        }
    }
}

#[test]
fn stale_observation_and_final_checkpoint_mutation_are_rejected_without_writes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runs.sqlite3");
    let source = Arc::new(EvidenceSource::default());
    let store = open(&path, source.clone());
    let frozen = frozen();
    let journal = store.begin(frozen.clone()).unwrap();
    let (mut frame, evidence) = tested(&frozen);
    *source.evidence.lock().unwrap() = vec![evidence];
    frame.result = Some("任务结果".into());
    journal.save(&frame).unwrap();
    let target = journal.workspace_observation_target().unwrap().unwrap();
    let before = store.backend.read(&journal.id).unwrap();
    let mut changed = frame.clone();
    changed.provider_requests += 1;
    assert!(
        journal.save(&changed).is_err(),
        "最终检查点不再接受后续写入"
    );
    let mut observation = current(&frozen, &frame);
    observation.checkpoint_sha256 = "f".repeat(64);
    assert!(
        journal
            .finish_with_workspace_observation(
                &mut candidate(),
                &target.checkpoint_sha256,
                Some(observation)
            )
            .is_err()
    );
    assert_eq!(store.backend.read(&journal.id).unwrap(), before);

    // 模拟采样后持久记录发生另一写入；收尾必须复读而非只信目标副本。
    let mut record = store.read(&journal.id).unwrap().unwrap();
    record.checkpoint = Some(changed);
    store.write(&journal.id, &record).unwrap();
    let modified = store.backend.read(&journal.id).unwrap();
    assert!(
        journal
            .finish_with_workspace_observation(
                &mut candidate(),
                &target.checkpoint_sha256,
                Some(current(&frozen, &frame))
            )
            .is_err()
    );
    assert_eq!(store.backend.read(&journal.id).unwrap(), modified);
    assert!(store.read(&journal.id).unwrap().unwrap().end.is_none());
}

#[test]
fn source_conflict_and_failed_atomic_finish_preserve_previous_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runs.sqlite3");
    let source = Arc::new(EvidenceSource::default());
    let store = open(&path, source.clone());
    let frozen = frozen();
    let journal = store.begin(frozen.clone()).unwrap();
    let (mut frame, evidence) = tested(&frozen);
    *source.evidence.lock().unwrap() = vec![evidence.clone()];
    journal.save(&frame).unwrap();
    let before = store.backend.read(&journal.id).unwrap();
    frame.result = Some("任务结果".into());
    source.fail.store(true, Ordering::SeqCst);
    assert!(journal.save(&frame).is_err());
    assert_eq!(store.backend.read(&journal.id).unwrap(), before);
    source.fail.store(false, Ordering::SeqCst);
    let mut conflicting = evidence;
    conflicting.input_sha256 = "9".repeat(64);
    *source.evidence.lock().unwrap() = vec![conflicting];
    assert!(journal.save(&frame).is_err());
    assert_eq!(store.backend.read(&journal.id).unwrap(), before);
    source.evidence.lock().unwrap().clear();
    journal.save(&frame).unwrap();
    let target = journal.workspace_observation_target().unwrap().unwrap();
    let before_finish = store.backend.read(&journal.id).unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_workspace_finish BEFORE UPDATE ON run_states BEGIN SELECT RAISE(ABORT,'synthetic finish failure'); END;").unwrap();
    assert!(
        journal
            .finish_with_workspace_observation(
                &mut candidate(),
                &target.checkpoint_sha256,
                Some(current(&frozen, &frame))
            )
            .is_err()
    );
    assert_eq!(store.backend.read(&journal.id).unwrap(), before_finish);
    assert!(
        store
            .read(&journal.id)
            .unwrap()
            .unwrap()
            .workspace_observation
            .is_none()
    );
    assert_eq!(
        store.views(&frozen.owner, Some(&journal.id), 0).unwrap()["runs"][0]["status"],
        "running"
    );
}

#[test]
fn new_contract_needs_source_and_old_rows_omit_new_fields() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runs.sqlite3");
    let store = tests::open(&path);
    assert!(store.begin(frozen()).is_err());
    let legacy = tests::frozen();
    let journal = store.begin(legacy.clone()).unwrap();
    journal.save(&tests::frame(&legacy, false)).unwrap();
    assert!(journal.workspace_observation_target().unwrap().is_none());
    let (_, bytes) = store.backend.read(&journal.id).unwrap().unwrap();
    let state: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(state.get("workspace_node_evidence").is_none());
    assert!(state.get("workspace_observation").is_none());
    drop(journal);
    drop(store);
    assert!(SqliteRunStorage::open(&path).is_ok());
}

#[test]
fn invalid_identity_budget_or_receipt_is_rejected_before_storage_changes() {
    for case in [
        "owner",
        "model_session",
        "budget",
        "receipt",
        "witness_owner",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runs.sqlite3");
        let source = Arc::new(EvidenceSource::default());
        let store = open(&path, source.clone());
        let frozen = frozen();
        let journal = store.begin(frozen.clone()).unwrap();
        let before = store.backend.read(&journal.id).unwrap();
        let (mut frame, mut evidence) = tested(&frozen);
        match case {
            "owner" => frame.scope.user_id = "stranger".into(),
            "model_session" => {
                frame.model_session = Some(frozen.model_session.fork("other", None).unwrap())
            }
            "budget" => frame.used_tool_calls = frozen.steps as usize + 1,
            "receipt" => evidence.tool_result_sha256 = "f".repeat(64),
            "witness_owner" => evidence.owner.user_id = "stranger".into(),
            _ => unreachable!(),
        }
        *source.evidence.lock().unwrap() = vec![evidence];
        assert!(journal.save(&frame).is_err(), "{case}");
        assert_eq!(store.backend.read(&journal.id).unwrap(), before);
        if matches!(case, "owner" | "model_session" | "budget") {
            assert_eq!(source.captures.load(Ordering::SeqCst), 0);
        }
    }
}

#[test]
fn pending_effect_has_no_observation_target_and_restart_does_not_resume() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runs.sqlite3");
    let source = Arc::new(EvidenceSource::default());
    let store = open(&path, source.clone());
    let frozen = frozen();
    let journal = store.begin(frozen.clone()).unwrap();
    let id = journal.id.clone();
    let (mut frame, evidence) = tested(&frozen);
    *source.evidence.lock().unwrap() = vec![evidence];
    frame.pending_calls.push(ToolCall {
        id: "pending-effect".into(),
        name: "run_command".into(),
        arguments: json!({"program":"synthetic"}),
    });
    frame.used_tool_calls += 1;
    frame.tool_batches += 1;
    frame.provider_requests += 1;
    journal.save(&frame).unwrap();
    assert!(journal.workspace_observation_target().unwrap().is_none());
    let before = store.backend.read(&id).unwrap();
    drop(journal);
    drop(store);
    let reopened = tests::open(&path);
    let view = reopened.evidence_view(&frozen.owner, &id).unwrap();
    assert_eq!(view["runs"][0]["run_status"], "interrupted_unknown");
    assert_eq!(view["runs"][0]["effects_unknown"], true);
    assert_eq!(
        view["runs"][0]["report"]["test_evidence"][0]["matches_input_at_settlement"],
        Value::Null
    );
    assert_eq!(view["runs"][0]["report"]["verified_at_settlement"], false);
    assert!(reopened.begin(frozen).is_err());
    assert_eq!(reopened.backend.read(&id).unwrap(), before);
    assert_eq!(source.captures.load(Ordering::SeqCst), 1);
}

#[test]
fn corrupted_completed_observation_is_rejected_on_reopen_and_not_erased() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runs.sqlite3");
    let source = Arc::new(EvidenceSource::default());
    let store = open(&path, source.clone());
    let frozen = frozen();
    let journal = store.begin(frozen.clone()).unwrap();
    let id = journal.id.clone();
    let (mut frame, evidence) = tested(&frozen);
    frame.result = Some("任务结果".into());
    *source.evidence.lock().unwrap() = vec![evidence];
    journal.save(&frame).unwrap();
    let target = journal.workspace_observation_target().unwrap().unwrap();
    journal
        .finish_with_workspace_observation(
            &mut candidate(),
            &target.checkpoint_sha256,
            Some(current(&frozen, &frame)),
        )
        .unwrap();
    let (_, bytes) = store.backend.read(&id).unwrap().unwrap();
    drop(journal);
    drop(store);
    let mut corrupted: Value = serde_json::from_slice(&bytes).unwrap();
    corrupted["workspace_observation"]["input_sha256"] = json!("9".repeat(64));
    let corrupted = serde_json::to_vec(&corrupted).unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE run_states SET state=?2 WHERE id=?1",
            rusqlite::params![id, corrupted],
        )
        .unwrap();
    assert!(SqliteRunStorage::open(&path).is_err());
    let retained: Vec<u8> = connection
        .query_row("SELECT state FROM run_states WHERE id=?1", [&id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(retained, corrupted);
}
