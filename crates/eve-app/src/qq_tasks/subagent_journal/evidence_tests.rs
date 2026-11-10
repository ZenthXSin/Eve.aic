use super::*;
use eve_llm_api::{ChatMessage, ToolFailureCode, ToolResult};
use eve_task_api::{TaskCompletionAssessment, TaskEvaluationContract, TaskReceiptCheck};

const PRIVATE_BODY: &str = "private synthetic conversation body";
const PRIVATE_CREDENTIAL: &str = "synthetic-secret-do-not-project-1847";
const PRIVATE_PATH: &str = "private-synthetic-workspace/source.rs";
const EXPECTED_DIGEST: &str = "synthetic-observed-digest";

fn receipt_frozen() -> FrozenRun {
    let mut frozen = tests::frozen();
    frozen.steps = 8;
    frozen.instructions = format!("核验文件；{PRIVATE_BODY}；token={PRIVATE_CREDENTIAL}");
    frozen.verification = TaskCompletionContract::Receipts {
        contract: TaskEvaluationContract {
            checks: vec![TaskReceiptCheck {
                tool: "read_file".into(),
                arguments: json!({"path":PRIVATE_PATH}),
                output_pointer: "/sha256".into(),
                expected: json!(EXPECTED_DIGEST),
            }],
            require_final_batch: true,
        },
    };
    frozen
}

fn check_call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "read_file".into(),
        arguments: json!({"path":PRIVATE_PATH}),
    }
}

fn append_batch(frame: &mut TurnCheckpoint, call: ToolCall, receipt: ToolResult) {
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

fn matched_frame(frozen: &FrozenRun) -> TurnCheckpoint {
    let mut frame = tests::frame(frozen, true);
    frame.pending_calls.clear();
    frame.used_tool_calls = 0;
    frame.tool_batches = 0;
    frame.provider_requests = 0;
    append_batch(
        &mut frame,
        check_call("original-check"),
        ToolResult::success(
            "original-check",
            json!({
                "sha256":EXPECTED_DIGEST,
                "content":PRIVATE_BODY,
                "authorization":PRIVATE_CREDENTIAL,
            }),
        )
        .unwrap(),
    );
    frame
}

fn assert_historical_only(view: &Value, id: &str) {
    assert_eq!(view["automatic_resume"], false);
    assert_eq!(view["runs"].as_array().unwrap().len(), 1);
    let row = &view["runs"][0];
    assert_eq!(row["run_id"], id);
    assert_eq!(row["evidence_kind"], "historical_tool_receipts");
    assert_eq!(row["current_artifact_validity"], "not_checked");
    assert_eq!(row["automatic_resume"], false);
    let serialized = serde_json::to_string(view).unwrap();
    for private in [
        PRIVATE_BODY,
        PRIVATE_CREDENTIAL,
        PRIVATE_PATH,
        EXPECTED_DIGEST,
    ] {
        assert!(!serialized.contains(private));
    }
}

#[test]
fn persisted_receipt_survives_later_reads_or_progress_without_changing_old_verification() {
    for progress in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runs.sqlite3");
        let store = tests::open(&path);
        let frozen = receipt_frozen();
        let journal = store.begin(frozen.clone()).unwrap();
        let id = journal.id.clone();
        let mut frame = matched_frame(&frozen);
        let call = if progress {
            ToolCall {
                id: "later-progress".into(),
                name: "task_progress".into(),
                arguments: json!({"status":"completed","summary":PRIVATE_BODY}),
            }
        } else {
            ToolCall {
                id: "later-read".into(),
                name: "read_file".into(),
                arguments: json!({"path":"logs/diagnostics.log"}),
            }
        };
        let receipt = ToolResult::success(
            &call.id,
            json!({"status":"completed","content":PRIVATE_BODY}),
        )
        .unwrap();
        append_batch(&mut frame, call, receipt);
        frame.result = Some("核验结果已整理".into());
        journal.save(&frame).unwrap();
        let verification = eve_task_plugin::evaluate_completion_contract(
            &frozen.verification,
            &frame.messages,
            frame.result.as_deref().unwrap(),
            None,
        );
        assert_eq!(
            verification,
            TaskCompletionAssessment::Mismatch {
                reason: "unbound_final_batch".into(),
            }
        );
        let mut run = WorkerRun {
            end: TaskEnd::Completed {
                result: frame.result.clone().unwrap(),
            },
            usage: TaskUsage::default(),
            verification: Some(verification.clone()),
            retryable: false,
        };
        journal.finish(&mut run).unwrap();
        let saved = store.backend.read(&id).unwrap();
        let view = store.evidence_view(&frozen.owner, &id).unwrap();
        assert_historical_only(&view, &id);
        assert_eq!(view["runs"][0]["verification"], "mismatch");
        assert_eq!(view["runs"][0]["report"]["status"], "historical_match");
        assert_eq!(
            view["runs"][0]["report"]["checks"][0]["source"]["call_id"],
            "original-check"
        );
        assert_eq!(
            view["runs"][0]["report"]["checks"][0]["source"]["batch_ordinal"],
            1
        );
        assert_eq!(store.backend.read(&id).unwrap(), saved);
        drop(journal);
        drop(store);

        let reopened = tests::open(&path);
        assert_eq!(reopened.evidence_view(&frozen.owner, &id).unwrap(), view);
        assert!(reopened.begin(frozen.clone()).is_err());
        assert_eq!(reopened.backend.read(&id).unwrap(), saved);
        let record = reopened.read(&id).unwrap().unwrap();
        assert!(record.frozen == frozen);
        assert_eq!(record.checkpoint, Some(frame));
        assert_eq!(record.verification, Some(verification));
        assert_eq!(record.usage, Some(run.usage));
    }
}

#[test]
fn latest_persisted_mismatch_or_failed_check_cannot_reuse_older_success() {
    for code in [
        None,
        Some(ToolFailureCode::PermissionDenied),
        Some(ToolFailureCode::TimedOut),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runs.sqlite3");
        let store = tests::open(&path);
        let frozen = receipt_frozen();
        let journal = store.begin(frozen.clone()).unwrap();
        let id = journal.id.clone();
        let mut frame = matched_frame(&frozen);
        let receipt = match code {
            None => ToolResult::success("latest-check", json!({"sha256":"changed-digest"})),
            Some(code) => ToolResult::failure("latest-check", code, PRIVATE_CREDENTIAL),
        }
        .unwrap();
        append_batch(&mut frame, check_call("latest-check"), receipt);
        journal.save(&frame).unwrap();
        drop(journal);
        drop(store);

        let reopened = tests::open(&path);
        let before = reopened.backend.read(&id).unwrap();
        let view = reopened.evidence_view(&frozen.owner, &id).unwrap();
        assert_historical_only(&view, &id);
        let row = &view["runs"][0];
        assert_eq!(
            row["report"]["status"],
            if code.is_some() {
                "unknown"
            } else {
                "historical_mismatch"
            }
        );
        assert_eq!(
            row["report"]["checks"][0]["source"]["call_id"],
            "latest-check"
        );
        assert_eq!(row["report"]["checks"][0]["source"]["batch_ordinal"], 2);
        assert_eq!(
            row["effects_unknown"],
            code == Some(ToolFailureCode::TimedOut)
        );
        assert!(row["verification"].is_null());
        assert_eq!(reopened.backend.read(&id).unwrap(), before);
        assert!(reopened.begin(frozen).is_err());
    }
}

#[test]
fn pending_side_effect_keeps_matched_source_unknown_without_replay_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runs.sqlite3");
    let store = tests::open(&path);
    let frozen = receipt_frozen();
    let journal = store.begin(frozen.clone()).unwrap();
    let id = journal.id.clone();
    let mut frame = matched_frame(&frozen);
    frame.pending_calls.push(ToolCall {
        id: "pending-send".into(),
        name: "send_message".into(),
        arguments: json!({"text":PRIVATE_BODY,"credential":PRIVATE_CREDENTIAL}),
    });
    frame.used_tool_calls += 1;
    frame.tool_batches += 1;
    frame.provider_requests += 1;
    journal.save(&frame).unwrap();
    drop(journal);
    drop(store);

    let reopened = tests::open(&path);
    let before = reopened.backend.read(&id).unwrap();
    let view = reopened.evidence_view(&frozen.owner, &id).unwrap();
    assert_historical_only(&view, &id);
    let row = &view["runs"][0];
    assert_eq!(row["effects_unknown"], true);
    assert_eq!(row["report"]["status"], "unknown");
    assert_eq!(row["report"]["reason"], "pending_tool_effect");
    assert_eq!(
        row["report"]["checks"][0]["source"]["call_id"],
        "original-check"
    );
    assert!(row["verification"].is_null());
    assert_eq!(reopened.evidence_view(&frozen.owner, &id).unwrap(), view);
    assert!(reopened.begin(frozen).is_err());
    assert_eq!(reopened.backend.read(&id).unwrap(), before);
    assert_eq!(reopened.read(&id).unwrap().unwrap().checkpoint, Some(frame));
}

#[test]
fn evidence_lookup_hides_other_users_and_sessions_like_missing_records() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runs.sqlite3");
    let store = tests::open(&path);
    let frozen = receipt_frozen();
    let journal = store.begin(frozen.clone()).unwrap();
    journal.save(&matched_frame(&frozen)).unwrap();
    let id = journal.id.clone();
    let expected = json!({"runs":[],"automatic_resume":false});
    let before = store.backend.read(&id).unwrap();
    for owner in [
        TaskOwner {
            user_id: "other-owner".into(),
            ..frozen.owner.clone()
        },
        TaskOwner {
            session_id: "other-session".into(),
            ..frozen.owner.clone()
        },
    ] {
        assert_eq!(store.evidence_view(&owner, &id).unwrap(), expected);
    }
    assert_eq!(
        store.evidence_view(&frozen.owner, &"0".repeat(64)).unwrap(),
        expected
    );
    assert_eq!(store.backend.read(&id).unwrap(), before);
    assert_historical_only(&store.evidence_view(&frozen.owner, &id).unwrap(), &id);
}

#[test]
fn missing_checkpoint_or_receipt_contract_does_not_invent_evidence() {
    for unspecified in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runs.sqlite3");
        let store = tests::open(&path);
        let mut frozen = receipt_frozen();
        if unspecified {
            frozen.verification = TaskCompletionContract::Unspecified;
        }
        let journal = store.begin(frozen.clone()).unwrap();
        if unspecified {
            journal.save(&matched_frame(&frozen)).unwrap();
        }
        let id = journal.id.clone();
        let before = store.backend.read(&id).unwrap();
        let view = store.evidence_view(&frozen.owner, &id).unwrap();
        assert_historical_only(&view, &id);
        assert!(view["runs"][0]["report"].is_null());
        assert_eq!(
            view["runs"][0]["reason"],
            if unspecified {
                "no_frozen_receipt_contract"
            } else {
                "missing_checkpoint"
            }
        );
        assert!(view["runs"][0]["verification"].is_null());
        assert_eq!(store.backend.read(&id).unwrap(), before);
    }
}

#[test]
fn cancelled_run_exposes_history_without_becoming_complete_or_leaking_assessment_reason() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runs.sqlite3");
    let store = tests::open(&path);
    let frozen = receipt_frozen();
    let journal = store.begin(frozen.clone()).unwrap();
    journal.save(&matched_frame(&frozen)).unwrap();
    let id = journal.id.clone();
    let verification = TaskCompletionAssessment::Blocked {
        reason: format!("{PRIVATE_BODY}; token={PRIVATE_CREDENTIAL}"),
    };
    let mut run = WorkerRun {
        end: TaskEnd::Cancelled,
        usage: TaskUsage::default(),
        verification: Some(verification.clone()),
        retryable: false,
    };
    journal.finish(&mut run).unwrap();
    drop(journal);
    drop(store);

    let reopened = tests::open(&path);
    let before = reopened.backend.read(&id).unwrap();
    let view = reopened.evidence_view(&frozen.owner, &id).unwrap();
    assert_historical_only(&view, &id);
    assert_eq!(view["runs"][0]["run_status"], "cancelled");
    assert_eq!(view["runs"][0]["verification"], "blocked");
    assert_eq!(view["runs"][0]["report"]["status"], "historical_match");
    assert!(reopened.begin(frozen).is_err());
    assert_eq!(reopened.backend.read(&id).unwrap(), before);
    let record = reopened.read(&id).unwrap().unwrap();
    assert!(matches!(record.end, Some(End::Cancelled)));
    assert_eq!(record.verification, Some(verification));
}

#[test]
fn inconsistent_saved_verified_does_not_become_a_current_success_or_rewrite_history() {
    for pending in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runs.sqlite3");
        let store = tests::open(&path);
        let frozen = receipt_frozen();
        let journal = store.begin(frozen.clone()).unwrap();
        let id = journal.id.clone();
        let mut frame = matched_frame(&frozen);
        if pending {
            frame.pending_calls.push(check_call("pending-check"));
            frame.used_tool_calls += 1;
            frame.tool_batches += 1;
            frame.provider_requests += 1;
        }
        journal.save(&frame).unwrap();
        let mut run = WorkerRun {
            end: TaskEnd::Cancelled,
            usage: TaskUsage::default(),
            verification: Some(TaskCompletionAssessment::Verified),
            retryable: false,
        };
        journal.finish(&mut run).unwrap();
        drop(journal);
        drop(store);
        let reopened = tests::open(&path);
        let before = reopened.backend.read(&id).unwrap();
        let view = reopened.evidence_view(&frozen.owner, &id).unwrap();
        assert_historical_only(&view, &id);
        assert_eq!(view["runs"][0]["run_status"], "cancelled");
        assert_eq!(view["runs"][0]["verification"], "inconsistent");
        assert_eq!(
            view["runs"][0]["report"]["status"],
            if pending {
                "unknown"
            } else {
                "historical_match"
            }
        );
        assert_eq!(reopened.backend.read(&id).unwrap(), before);
        assert_eq!(
            reopened.read(&id).unwrap().unwrap().verification,
            Some(TaskCompletionAssessment::Verified)
        );
    }
}

#[test]
fn evidence_arguments_require_exact_run_and_preserve_ordinary_list_pagination() {
    let tool = ListRuns {
        late: Arc::new(Late::default()),
    };
    let id = "a".repeat(64);
    for arguments in [
        json!({"run_id":id,"evidence":true}),
        json!({}),
        json!({"offset":20}),
        json!({"run_id":id,"offset":10}),
        json!({"evidence":false}),
        json!({"run_id":id,"evidence":false,"offset":10}),
    ] {
        tool.validate_arguments(&arguments).unwrap();
    }
    for arguments in [
        json!({"evidence":true}),
        json!({"run_id":id,"evidence":true,"offset":0}),
        json!({"run_id":"invalid","evidence":true}),
        json!({"run_id":id,"evidence":"true"}),
        json!({"run_id":id,"evidence":null}),
        json!({"run_id":id,"evidence":true,"extra":false}),
    ] {
        assert!(tool.validate_arguments(&arguments).is_err());
    }
}
