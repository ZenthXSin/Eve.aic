use eve_cognition_api::*;
use eve_cognition_plugin::{COGNITION_STATE_KEY, read_cognitive_snapshot};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct ReadProbe {
    bytes: Option<Vec<u8>>,
    fail_read: bool,
    reads: AtomicUsize,
    writes: AtomicUsize,
}

impl StateStore for ReadProbe {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        assert_eq!(namespace, &PluginId::new(COGNITION_PLUGIN_ID).unwrap());
        assert_eq!(key, COGNITION_STATE_KEY);
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.fail_read {
            return Err(PluginError::State("backend-secret".into()));
        }
        Ok(self.bytes.clone())
    }

    fn set(&self, _: &PluginId, _: String, _: Vec<u8>) -> PluginResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        Err(PluginError::State("unexpected write".into()))
    }
}

fn executing_snapshot() -> CognitiveSnapshot {
    let mut state = CognitiveState::default();
    state.goals.insert(
        "goal-1".into(),
        Goal {
            id: "goal-1".into(),
            revision: 2,
            source: Source {
                kind: SourceKind::User,
                channel: "cognition.cli".into(),
                reference: "message-1".into(),
            },
            visibility: Visibility::User("owner".into()),
            description: "private-goal-body".into(),
            verification: "保存执行结果".into(),
            priority: 50,
            budget: ExecutionBudget {
                max_model_requests: 1,
                max_tool_calls: 0,
                max_attempts: 1,
                timeout_ms: 1000,
            },
            stop_condition: "保存结果后停止".into(),
            expires_at_ms: None,
            status: GoalStatus::Executing,
            wait_reason: None,
            block_reason: None,
            execution: Some(ExecutionAttempt {
                attempt_id: "attempt-1".into(),
                session_id: "session-1".into(),
                task_id: "task-1".into(),
                turn_id: Some(1),
                started_at_ms: 1,
            }),
            feedback: None,
        },
    );
    state.agenda = Some(Agenda {
        visibility: Visibility::Internal,
        candidates: Vec::new(),
        selected: Some("goal-1".into()),
        reason: "private-agenda-body".into(),
        valid_until_ms: 100,
    });
    CognitiveSnapshot {
        format_version: COGNITION_FORMAT_VERSION,
        subject_id: "eve".into(),
        revision: 2,
        state,
    }
}

#[test]
fn missing_state_is_an_empty_snapshot_without_initialization() {
    let store = ReadProbe::default();
    let actual = read_cognitive_snapshot(&store, "eve").unwrap();
    assert_eq!(
        actual,
        CognitiveSnapshot {
            format_version: COGNITION_FORMAT_VERSION,
            subject_id: "eve".into(),
            revision: 0,
            state: CognitiveState::default(),
        }
    );
    assert_eq!(store.reads.load(Ordering::SeqCst), 1);
    assert_eq!(store.writes.load(Ordering::SeqCst), 0);
    assert!(store.bytes.is_none());
}

#[test]
fn executing_goal_and_selected_agenda_are_not_recovered_or_incremented() {
    // At the counter boundary recovery would overflow; reading must still succeed.
    let mut expected = executing_snapshot();
    expected.revision = u64::MAX;
    expected.state.goals.get_mut("goal-1").unwrap().revision = u64::MAX;
    let bytes = serde_json::to_vec(&expected).unwrap();
    let store = ReadProbe {
        bytes: Some(bytes.clone()),
        ..ReadProbe::default()
    };
    for _ in 0..2 {
        assert_eq!(read_cognitive_snapshot(&store, "eve").unwrap(), expected);
    }
    assert_eq!(store.reads.load(Ordering::SeqCst), 2);
    assert_eq!(store.writes.load(Ordering::SeqCst), 0);
    assert_eq!(store.bytes, Some(bytes));
}

#[test]
fn invalid_subject_is_rejected_before_reading_the_backend() {
    for subject in ["", " eve", "eve\n"] {
        let store = ReadProbe::default();
        assert_eq!(
            read_cognitive_snapshot(&store, subject),
            Err(CognitionError::InvalidInput)
        );
        assert_eq!(store.reads.load(Ordering::SeqCst), 0);
        assert_eq!(store.writes.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn invalid_persisted_documents_are_rejected_without_repair_or_disclosure() {
    let snapshot = executing_snapshot();
    let valid = serde_json::to_value(&snapshot).unwrap();
    let mut cases = vec![
        (
            b"corrupt-private-secret".to_vec(),
            CognitionError::CorruptState,
        ),
        (
            vec![b'x'; MAX_STATE_BYTES + 1],
            CognitionError::LimitReached,
        ),
    ];
    for (pointer, value, error) in [
        (
            "/format_version",
            json!(2),
            CognitionError::UnsupportedVersion,
        ),
        (
            "/subject_id",
            json!("another-subject"),
            CognitionError::SubjectMismatch,
        ),
        ("/revision", json!(0), CognitionError::CorruptState),
        (
            "/state/goals/goal-1/revision",
            json!(0),
            CognitionError::CorruptState,
        ),
        (
            "/state/goals/goal-1/revision",
            json!(3),
            CognitionError::CorruptState,
        ),
        (
            "/state/goals/goal-1/budget/timeout_ms",
            json!(0),
            CognitionError::CorruptState,
        ),
        (
            "/state/goals/goal-1/source/kind",
            json!({"User": {"secret": true}}),
            CognitionError::CorruptState,
        ),
    ] {
        let mut document = valid.clone();
        *document.pointer_mut(pointer).unwrap() = value;
        cases.push((serde_json::to_vec(&document).unwrap(), error));
    }
    let mut unknown = valid;
    unknown["state"]["unknown"] = json!("private-secret");
    cases.push((
        serde_json::to_vec(&unknown).unwrap(),
        CognitionError::CorruptState,
    ));
    let goal_json = serde_json::to_string(&snapshot.state.goals["goal-1"]).unwrap();
    let duplicate = format!(
        "{{\"format_version\":1,\"subject_id\":\"eve\",\"revision\":2,\"state\":{{\"goals\":{{\"goal-1\":{goal_json},\"goal-1\":{goal_json}}},\"drives\":{{}},\"agenda\":null,\"events\":[]}}}}"
    );
    cases.push((duplicate.into_bytes(), CognitionError::CorruptState));
    let mut trailing = serde_json::to_vec(&snapshot).unwrap();
    trailing.extend_from_slice(b"{}");
    cases.push((trailing, CognitionError::CorruptState));
    for (bytes, error) in cases {
        let store = ReadProbe {
            bytes: Some(bytes.clone()),
            ..ReadProbe::default()
        };
        let actual = read_cognitive_snapshot(&store, "eve").unwrap_err();
        assert_eq!(actual, error);
        assert!(!format!("{actual:?}: {actual}").contains("private-secret"));
        assert_eq!(store.reads.load(Ordering::SeqCst), 1);
        assert_eq!(store.writes.load(Ordering::SeqCst), 0);
        assert_eq!(store.bytes, Some(bytes));
    }
}

#[test]
fn backend_failure_is_not_treated_as_missing_state_or_exposed() {
    let store = ReadProbe {
        fail_read: true,
        ..ReadProbe::default()
    };
    let error = read_cognitive_snapshot(&store, "eve").unwrap_err();
    assert_eq!(error, CognitionError::Storage);
    assert!(!format!("{error:?}: {error}").contains("backend-secret"));
    assert_eq!(store.reads.load(Ordering::SeqCst), 1);
    assert_eq!(store.writes.load(Ordering::SeqCst), 0);
}
