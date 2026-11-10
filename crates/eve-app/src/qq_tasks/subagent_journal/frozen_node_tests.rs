use super::*;
use eve_llm_api::{ChatMessage, ToolResult};
use eve_task_api::{TaskFrozenNodeOutcome, TaskResult, task_strategy_digest};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
struct EvidenceSource {
    evidence: Mutex<Vec<TaskFrozenNodeEvidence>>,
    captures: AtomicUsize,
    fail: AtomicBool,
}
impl TaskCompletionEvidenceSource for EvidenceSource {
    fn capture(
        &self,
        _owner: &TaskOwner,
        _frame: &TurnCheckpoint,
    ) -> TaskResult<Vec<TaskFrozenNodeEvidence>> {
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
    frozen.verification = TaskCompletionContract::FrozenNodeTest {
        input_sha256: "1".repeat(64),
        runtime_sha256: "2".repeat(64),
        command_sha256: "3".repeat(64),
    };
    frozen
}

fn open(path: &std::path::Path, source: Arc<EvidenceSource>) -> Arc<RunStore> {
    RunStore::open_with_completion_evidence(
        Arc::new(SqliteRunStorage::open(path).unwrap()),
        Some(source),
    )
}

fn frame(frozen: &FrozenRun) -> TurnCheckpoint {
    let mut frame = tests::frame(frozen, true);
    frame.pending_calls.clear();
    frame.used_tool_calls = 0;
    frame.tool_batches = 0;
    frame.provider_requests = 0;
    frame
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

fn tested(
    frozen: &FrozenRun,
    id: &str,
    outcome: TaskFrozenNodeOutcome,
) -> (TurnCheckpoint, TaskFrozenNodeEvidence) {
    let mut frame = frame(frozen);
    let call = ToolCall {
        id: id.into(),
        name: eve_task_api::FROZEN_NODE_TEST_TOOL.into(),
        arguments: json!({"input":"synthetic-private-test-input"}),
    };
    let receipt = ToolResult::success(
        id,
        json!({
            "status":"finished", "stdout":"synthetic-private-output",
            "credential":"synthetic-secret-not-evidence", "exit_code":0,
        }),
    )
    .unwrap();
    let evidence = TaskFrozenNodeEvidence {
        owner: frozen.owner.clone(),
        model_session: frozen.model_session.clone(),
        call_id: id.into(),
        arguments_sha256: task_strategy_digest(&call.arguments),
        tool_result_sha256: task_strategy_digest(&receipt),
        input_sha256: "1".repeat(64),
        runtime_sha256: "2".repeat(64),
        command_sha256: "3".repeat(64),
        outcome,
    };
    append(&mut frame, call, receipt);
    (frame, evidence)
}

fn success() -> TaskFrozenNodeOutcome {
    TaskFrozenNodeOutcome::Exited {
        exit_code: Some(0),
        signal: None,
    }
}

fn candidate(result: &str) -> WorkerRun {
    WorkerRun {
        end: TaskEnd::Completed {
            result: result.into(),
        },
        usage: TaskUsage::default(),
        verification: Some(TaskCompletionAssessment::Verified),
        retryable: false,
    }
}

#[test]
fn captured_test_survives_progress_finish_and_sqlite_reopen_without_live_source() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runs.sqlite3");
    let source = Arc::new(EvidenceSource::default());
    let store = open(&path, source.clone());
    let frozen = frozen();
    let journal = store.begin(frozen.clone()).unwrap();
    let id = journal.id.clone();
    let (mut frame, evidence) = tested(&frozen, "tested-once", success());
    *source.evidence.lock().unwrap() = vec![evidence.clone()];
    journal.save(&frame).unwrap();
    source.evidence.lock().unwrap().clear();
    append(
        &mut frame,
        ToolCall {
            id: "progress".into(),
            name: "task_progress".into(),
            arguments: json!({"status":"completed"}),
        },
        ToolResult::success("progress", json!({"status":"completed"})).unwrap(),
    );
    frame.result = Some("冻结输入测试通过".into());
    journal.save(&frame).unwrap();
    assert_eq!(source.captures.load(Ordering::SeqCst), 2);
    source.fail.store(true, Ordering::SeqCst);
    let mut run = candidate(frame.result.as_deref().unwrap());
    journal.finish(&mut run).unwrap();
    assert!(matches!(run.end, TaskEnd::Completed { .. }));
    assert_eq!(run.verification, Some(TaskCompletionAssessment::Verified));
    assert_eq!(
        source.captures.load(Ordering::SeqCst),
        2,
        "结算只能读已保存见证"
    );
    let before = store.backend.read(&id).unwrap();
    drop(journal);
    drop(store);

    let reopened = tests::open(&path);
    let saved = reopened.read(&id).unwrap().unwrap();
    assert_eq!(saved.frozen_node_evidence, vec![evidence]);
    assert_eq!(saved.checkpoint, Some(frame));
    assert!(matches!(saved.end, Some(End::Completed { .. })));
    assert_eq!(saved.verification, Some(TaskCompletionAssessment::Verified));
    let view = reopened.evidence_view(&frozen.owner, &id).unwrap();
    assert_eq!(view["runs"][0]["current_artifact_validity"], "not_checked");
    assert!(!view.to_string().contains("synthetic-private"));
    assert!(!view.to_string().contains("synthetic-secret"));
    assert!(reopened.begin(frozen).is_err());
    assert_eq!(reopened.backend.read(&id).unwrap(), before);
    assert_eq!(source.captures.load(Ordering::SeqCst), 2);
}

#[test]
fn new_contract_requires_source_and_model_supplied_receipt_does_not_replace_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runs.sqlite3");
    let frozen = frozen();
    let no_source = tests::open(&path);
    assert!(no_source.begin(frozen.clone()).is_err());
    assert_eq!(
        no_source.views(&frozen.owner, None, 0).unwrap()["runs"],
        json!([])
    );
    drop(no_source);
    let source = Arc::new(EvidenceSource::default());
    let store = open(&path, source);
    let journal = store.begin(frozen.clone()).unwrap();
    let (mut frame, _) = tested(&frozen, "claimed-success", success());
    frame.result = Some("模型声称通过".into());
    journal.save(&frame).unwrap();
    let mut run = candidate(frame.result.as_deref().unwrap());
    journal.finish(&mut run).unwrap();
    assert!(matches!(run.end, TaskEnd::Blocked { .. }));
    assert_eq!(
        run.verification,
        Some(TaskCompletionAssessment::Unverifiable {
            reason: "missing_trusted_frozen_node_evidence".into(),
        })
    );
    let saved = store.read(&journal.id).unwrap().unwrap();
    assert!(saved.frozen_node_evidence.is_empty());
    assert!(matches!(saved.end, Some(End::Blocked { .. })));
}

#[test]
fn latest_test_failure_overrides_old_success_even_when_worker_claims_verified() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runs.sqlite3");
    let source = Arc::new(EvidenceSource::default());
    let store = open(&path, source.clone());
    let frozen = frozen();
    let journal = store.begin(frozen.clone()).unwrap();
    let (mut frame, old) = tested(&frozen, "old-success", success());
    *source.evidence.lock().unwrap() = vec![old];
    journal.save(&frame).unwrap();
    let (later, mut failed) = tested(
        &frozen,
        "latest-failure",
        TaskFrozenNodeOutcome::Exited {
            exit_code: Some(1),
            signal: None,
        },
    );
    let call = later
        .messages
        .iter()
        .flat_map(|m| &m.tool_calls)
        .next()
        .unwrap()
        .clone();
    let receipt = ToolResult::success("latest-failure", json!({"exit_code":1})).unwrap();
    failed.tool_result_sha256 = task_strategy_digest(&receipt);
    append(&mut frame, call, receipt);
    frame.result = Some("仍声称全部通过".into());
    *source.evidence.lock().unwrap() = vec![failed];
    journal.save(&frame).unwrap();
    let mut run = candidate(frame.result.as_deref().unwrap());
    journal.finish(&mut run).unwrap();
    assert!(matches!(run.end, TaskEnd::Blocked { .. }));
    assert_eq!(
        run.verification,
        Some(TaskCompletionAssessment::Mismatch {
            reason: "frozen_node_test_failed".into(),
        })
    );
    assert_eq!(
        store
            .read(&journal.id)
            .unwrap()
            .unwrap()
            .frozen_node_evidence
            .len(),
        2
    );
}

#[test]
fn source_error_conflict_or_sqlite_failure_keeps_previous_checkpoint_and_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runs.sqlite3");
    let source = Arc::new(EvidenceSource::default());
    let store = open(&path, source.clone());
    let frozen = frozen();
    let journal = store.begin(frozen.clone()).unwrap();
    let (mut frame, evidence) = tested(&frozen, "first", success());
    *source.evidence.lock().unwrap() = vec![evidence.clone()];
    journal.save(&frame).unwrap();
    let before = store.backend.read(&journal.id).unwrap();
    append(
        &mut frame,
        ToolCall {
            id: "read-log".into(),
            name: "read_file".into(),
            arguments: json!({"path":"log"}),
        },
        ToolResult::success("read-log", json!({"content":"log"})).unwrap(),
    );
    source.fail.store(true, Ordering::SeqCst);
    assert!(journal.save(&frame).is_err());
    assert_eq!(store.backend.read(&journal.id).unwrap(), before);
    source.fail.store(false, Ordering::SeqCst);
    let mut conflicting = evidence;
    conflicting.outcome = TaskFrozenNodeOutcome::Exited {
        exit_code: Some(1),
        signal: None,
    };
    *source.evidence.lock().unwrap() = vec![conflicting];
    assert!(journal.save(&frame).is_err());
    assert_eq!(store.backend.read(&journal.id).unwrap(), before);
    source.evidence.lock().unwrap().clear();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_frozen_save BEFORE UPDATE ON run_states BEGIN SELECT RAISE(ABORT,'synthetic save failure'); END;").unwrap();
    assert!(journal.save(&frame).is_err());
    assert_eq!(store.backend.read(&journal.id).unwrap(), before);
    connection
        .execute_batch("DROP TRIGGER fail_frozen_save;")
        .unwrap();
    journal.save(&frame).unwrap();
    assert_eq!(
        store
            .read(&journal.id)
            .unwrap()
            .unwrap()
            .frozen_node_evidence
            .len(),
        1
    );
}

#[test]
fn pending_cancelled_and_uncheckpointed_results_cannot_become_completed() {
    for case in [
        "pending",
        "cancelled",
        "wrong_result",
        "extra_usage",
        "unknown_witness",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runs.sqlite3");
        let source = Arc::new(EvidenceSource::default());
        let store = open(&path, source.clone());
        let frozen = frozen();
        let journal = store.begin(frozen.clone()).unwrap();
        let (mut frame, evidence) = tested(
            &frozen,
            "tested",
            if case == "unknown_witness" {
                TaskFrozenNodeOutcome::Unknown
            } else {
                success()
            },
        );
        *source.evidence.lock().unwrap() = vec![evidence];
        if case == "pending" {
            frame.pending_calls.push(ToolCall {
                id: "pending-side-effect".into(),
                name: "run_command".into(),
                arguments: json!({"program":"synthetic"}),
            });
            frame.used_tool_calls += 1;
            frame.tool_batches += 1;
            frame.provider_requests += 1;
        } else {
            frame.result = Some("保存的实际结果".into());
        }
        journal.save(&frame).unwrap();
        let mut run = candidate(if case == "wrong_result" {
            "未保存的模型改写"
        } else {
            "保存的实际结果"
        });
        if case == "cancelled" {
            run.end = TaskEnd::Cancelled;
        }
        if case == "extra_usage" {
            run.usage.tool_calls = 7;
        }
        journal.finish(&mut run).unwrap();
        assert!(!run.verification.as_ref().unwrap().verified(), "{case}");
        let saved = store.read(&journal.id).unwrap().unwrap();
        assert!(!matches!(saved.end, Some(End::Completed { .. })), "{case}");
        if case == "cancelled" {
            assert!(matches!(saved.end, Some(End::Cancelled)));
        }
    }
}

#[test]
fn wrong_identity_budget_or_evidence_binding_is_rejected_before_persisting() {
    for case in ["owner", "budget", "receipt", "witness_owner"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runs.sqlite3");
        let source = Arc::new(EvidenceSource::default());
        let store = open(&path, source.clone());
        let frozen = frozen();
        let journal = store.begin(frozen.clone()).unwrap();
        let before = store.backend.read(&journal.id).unwrap();
        let (mut frame, mut evidence) = tested(&frozen, "tested", success());
        match case {
            "owner" => frame.scope.user_id = "other".into(),
            "budget" => frame.used_tool_calls = frozen.steps as usize + 1,
            "receipt" => evidence.tool_result_sha256 = "f".repeat(64),
            "witness_owner" => evidence.owner.user_id = "other".into(),
            _ => unreachable!(),
        }
        *source.evidence.lock().unwrap() = vec![evidence];
        assert!(journal.save(&frame).is_err(), "{case}");
        assert_eq!(store.backend.read(&journal.id).unwrap(), before);
        if matches!(case, "owner" | "budget") {
            assert_eq!(source.captures.load(Ordering::SeqCst), 0);
        }
    }
}

#[test]
fn old_records_keep_omitted_sidecar_and_completed_witness_tampering_fails_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let old_path = directory.path().join("old.sqlite3");
    let old = tests::open(&old_path);
    let legacy = tests::frozen();
    let journal = old.begin(legacy.clone()).unwrap();
    journal.save(&tests::frame(&legacy, false)).unwrap();
    let (_, bytes) = old.backend.read(&journal.id).unwrap().unwrap();
    assert!(
        serde_json::from_slice::<Value>(&bytes)
            .unwrap()
            .get("frozen_node_evidence")
            .is_none()
    );
    drop(journal);
    drop(old);
    assert!(SqliteRunStorage::open(&old_path).is_ok());

    let path = directory.path().join("frozen.sqlite3");
    let source = Arc::new(EvidenceSource::default());
    let store = open(&path, source.clone());
    let frozen = frozen();
    let journal = store.begin(frozen.clone()).unwrap();
    let id = journal.id.clone();
    let (mut frame, evidence) = tested(&frozen, "test", success());
    frame.result = Some("已核验".into());
    *source.evidence.lock().unwrap() = vec![evidence];
    journal.save(&frame).unwrap();
    journal.finish(&mut candidate("已核验")).unwrap();
    let (_, state) = store.backend.read(&id).unwrap().unwrap();
    drop(journal);
    drop(store);
    let mut changed: Value = serde_json::from_slice(&state).unwrap();
    changed["frozen_node_evidence"][0]["owner"]["user_id"] = json!("other");
    let changed = serde_json::to_vec(&changed).unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE run_states SET state=?2 WHERE id=?1",
            rusqlite::params![id, changed],
        )
        .unwrap();
    assert!(SqliteRunStorage::open(&path).is_err());
    let retained: Vec<u8> = connection
        .query_row("SELECT state FROM run_states WHERE id=?1", [&id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(retained, changed, "损坏见证只拒绝，不清除原证据");
}
