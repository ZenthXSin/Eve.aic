use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_llm_api::ContextScope;
use eve_plugin_api::*;
use eve_training_api::*;
use eve_training_plugin::TrainingPlugin;
use eve_training_sqlite::SqliteExpressions;
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[path = "conversation_flow/boundary.rs"]
mod boundary;
#[path = "conversation_flow/boundary_safety.rs"]
mod boundary_safety;

fn scope() -> ContextScope {
    ContextScope {
        session_id: "chat".into(),
        user_id: "human".into(),
    }
}
fn event(index: usize) -> ConversationMessage {
    ConversationMessage {
        id: format!("m-{index}"),
        speaker: if index.is_multiple_of(2) {
            "human"
        } else {
            "eve"
        }
        .into(),
        role: if index.is_multiple_of(2) {
            ConversationRole::Human
        } else {
            ConversationRole::Eve
        },
        text: format!(
            "消息 {index}\n```rust\nlet value = {index};\n```\n> 原样引用\nhttps://example.test"
        ),
        metadata: Some(ConversationMetadata {
            observed_at_ms: Some(index as u64),
            ..Default::default()
        }),
    }
}
async fn open(
    state: Arc<MemoryStateStore>,
    store: Arc<dyn ConversationStore>,
) -> (Kernel, Arc<dyn TrainingService>) {
    let backends = KernelServices {
        state,
        ..Default::default()
    };
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    kernel
        .register(Box::new(
            TrainingPlugin::new(true)
                .unwrap()
                .with_conversation_store(store),
        ))
        .unwrap();
    kernel
        .start(&PluginId::new(TRAINING_PLUGIN_ID).unwrap())
        .await
        .unwrap();
    let training = registry
        .get(&ServiceId::new(TRAINING_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<TrainingServiceHandle>()
        .unwrap()
        .0
        .clone();
    (kernel, training)
}
fn checkpoint(store: &dyn ConversationStore) -> serde_json::Value {
    serde_json::from_slice(&store.scope(&scope()).unwrap().unwrap().checkpoint.unwrap()).unwrap()
}
fn replace_checkpoint(store: &dyn ConversationStore, value: &serde_json::Value) {
    let row = store.scope(&scope()).unwrap().unwrap();
    assert!(
        store
            .commit(
                &scope(),
                &ConversationCommit {
                    expected_revision: row.revision,
                    checkpoint: serde_json::to_vec(value).unwrap(),
                    revisions: vec![],
                    batch: None,
                },
            )
            .unwrap()
    );
}
fn sweep_counts(status: &ConversationSweepStatus) -> &ConversationSweepMessageCounts {
    let counts = status.message_counts.as_ref().unwrap();
    assert_eq!(
        status.total,
        status.processed + counts.failed + counts.unknown + counts.unclassified + status.remaining
    );
    assert!(counts.partial <= status.remaining);
    assert!(counts.partial_failed + counts.partial_unknown <= counts.partial);
    counts
}
fn fail_sweep_batch(training: &dyn TrainingService, mut batch: ConversationBatch, now: &mut u64) {
    for attempt in 1..=3 {
        assert_eq!(batch.attempt, attempt);
        training.fail_conversation_batch(&batch, *now).unwrap();
        *now += 180_001;
        if attempt < 3 {
            let retry = training
                .take_conversation_maintenance_batch(*now, 4096)
                .unwrap()
                .unwrap();
            assert_eq!(retry.id, batch.id);
            assert_eq!(retry.messages, batch.messages);
            batch = retry;
        }
    }
}
fn observe(training: &dyn TrainingService, from: usize, count: usize) {
    for index in from..from + count {
        training
            .observe_conversation(&scope(), event(index))
            .unwrap();
    }
}
fn draft(
    batch: &ConversationBatch,
    indices: [usize; 2],
    id: Option<String>,
    assessment: ConversationAssessment,
    response: &str,
) -> ConversationLessonDraft {
    ConversationLessonDraft {
        lesson_id: id,
        situation: "需要实际查询时".into(),
        response: response.into(),
        avoid: "无需为了固定开场而重复发言".into(),
        assessment,
        evidence: indices
            .into_iter()
            .map(|index| ConversationEvidence {
                message_id: batch.messages[index].id.clone(),
                quote: batch.messages[index].text.lines().next().unwrap().into(),
            })
            .collect(),
    }
}

#[tokio::test]
async fn correction_and_counterexample_require_nearest_eve_in_both_stores() {
    for persistent in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
        let state = Arc::new(MemoryStateStore::default());
        let backends = KernelServices {
            state: state.clone(),
            ..Default::default()
        };
        let registry = backends.registry.clone();
        let kernel = Kernel::with_services(backends);
        let mut plugin = TrainingPlugin::new(true).unwrap();
        if persistent {
            plugin = plugin.with_conversation_store(sql.clone());
        }
        kernel.register(Box::new(plugin)).unwrap();
        let plugin_id = PluginId::new(TRAINING_PLUGIN_ID).unwrap();
        kernel.start(&plugin_id).await.unwrap();
        let training = registry
            .get(&ServiceId::new(TRAINING_SERVICE_ID).unwrap())
            .unwrap()
            .unwrap()
            .value
            .downcast::<TrainingServiceHandle>()
            .unwrap()
            .0
            .clone();
        for (round, assessment) in [
            ConversationAssessment::Correction,
            ConversationAssessment::Counterexample,
        ]
        .into_iter()
        .enumerate()
        {
            let first = round * 4;
            for (offset, role) in [
                ConversationRole::Human,
                ConversationRole::Eve,
                ConversationRole::Eve,
                ConversationRole::Human,
            ]
            .into_iter()
            .enumerate()
            {
                let mut message = event(first + offset);
                message.role = role;
                message.speaker = if role == ConversationRole::Human {
                    "human"
                } else {
                    "eve"
                }
                .into();
                training.observe_conversation(&scope(), message).unwrap();
            }
            let now = 100_000 + round as u64 * 2;
            let batch = training.take_conversation_batch(now).unwrap().unwrap();
            let locate = |offset: usize| {
                batch
                    .messages
                    .iter()
                    .position(|m| m.id == format!("m-{}", first + offset))
                    .unwrap()
            };
            let lesson_id = training
                .conversation_lessons(&scope())
                .unwrap()
                .first()
                .map(|l| l.id.clone());
            let prompt: serde_json::Value =
                serde_json::from_str(&conversation_prompt_json(&batch).unwrap()).unwrap();
            assert!(prompt["feedback_pairs"].as_array().unwrap().contains(&serde_json::json!({
                "eve_message_id":format!("m-{}", first + 2), "later_human_message_id":format!("m-{}", first + 3)
            })));
            let previous_json = state.get(&plugin_id, "conversation.v1").unwrap();
            let previous_scope = sql.scope(&scope()).unwrap();
            let previous_revisions = sql.revisions(&scope(), 0, 256).unwrap();
            let previous_lessons = training.conversation_lessons(&scope()).unwrap();
            let wrong_pair = draft(
                &batch,
                [locate(1), locate(3)],
                lesson_id.clone(),
                assessment,
                "依据对应反馈调整表达",
            );
            assert!(
                training
                    .finish_conversation_batch(&batch, vec![wrong_pair], now + 1)
                    .is_err()
            );
            assert_eq!(
                state.get(&plugin_id, "conversation.v1").unwrap(),
                previous_json
            );
            assert_eq!(sql.scope(&scope()).unwrap(), previous_scope);
            assert_eq!(sql.revisions(&scope(), 0, 256).unwrap(), previous_revisions);
            assert_eq!(
                training.conversation_lessons(&scope()).unwrap(),
                previous_lessons
            );
            let nearest_pair = draft(
                &batch,
                [locate(2), locate(3)],
                lesson_id,
                assessment,
                "依据对应反馈调整表达",
            );
            assert_eq!(
                training
                    .finish_conversation_batch(&batch, vec![nearest_pair], now + 1)
                    .unwrap(),
                1
            );
            let lesson = training.conversation_lessons(&scope()).unwrap().remove(0);
            assert_eq!(
                lesson.active,
                assessment == ConversationAssessment::Correction
            );
        }
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn full_backlog_consumes_in_sequence_without_overwriting_old_raw_messages() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, training) = open(state.clone(), store.clone()).await;
    observe(training.as_ref(), 0, 100);
    let mut seen = BTreeSet::new();
    let mut now = 100_000;
    while let Some(batch) = training.take_conversation_batch(now).unwrap() {
        assert!(conversation_prompt_json(&batch).unwrap().len() <= MAX_CONVERSATION_PROMPT_BYTES);
        for message in &batch.messages {
            let sequence = message.metadata.as_ref().unwrap().sequence.unwrap();
            assert_eq!(message.text, event(sequence as usize - 1).text);
            seen.insert(sequence);
        }
        training
            .finish_conversation_batch(&batch, vec![], now + 1)
            .unwrap();
        now += 2;
    }
    assert_eq!(seen, (1..=100).collect());
    assert_eq!(checkpoint(store.as_ref())["cursor"]["sequence"], 101);
    let events = store.events(&scope(), 1, 256).unwrap();
    assert_eq!(events.len(), 100);
    assert_eq!(events[0].message.text, event(0).text);
    kernel.stop_all().await.unwrap();
    let (kernel, training) = open(state, store.clone()).await;
    assert!(training.take_conversation_batch(now).unwrap().is_none());
    training.observe_conversation(&scope(), event(0)).unwrap();
    assert_eq!(store.events(&scope(), 1, 256).unwrap().len(), 100);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn escaped_long_body_and_context_are_consumed_as_exact_utf8_fragments() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let (kernel, training) = open(Arc::new(MemoryStateStore::default()), store.clone()).await;
    let mut original = event(0);
    original.text = "\"".repeat(MAX_CONVERSATION_BODY_BYTES);
    original.metadata.as_mut().unwrap().context = Some("\n".repeat(MAX_CONVERSATION_CONTEXT_BYTES));
    training
        .observe_conversation(&scope(), original.clone())
        .unwrap();
    let mut body = String::new();
    let mut context = String::new();
    let mut batches = 0;
    while let Some(batch) = training
        .take_conversation_batch(100_000 + batches * 2)
        .unwrap()
    {
        assert!(conversation_prompt_json(&batch).unwrap().len() <= MAX_CONVERSATION_PROMPT_BYTES);
        assert_eq!(batch.messages.len(), 1);
        let message = &batch.messages[0];
        let metadata = message.metadata.as_ref().unwrap();
        let fragment = metadata.fragment.as_ref().unwrap();
        assert_eq!(metadata.sequence, Some(1));
        assert_eq!(fragment.text_start, body.len());
        assert_eq!(fragment.context_start, context.len());
        assert!(conversation_message_valid(message));
        body.push_str(&message.text);
        context.push_str(metadata.context.as_deref().unwrap_or(""));
        training
            .finish_conversation_batch(&batch, vec![], 100_001 + batches * 2)
            .unwrap();
        batches += 1;
        assert!(batches <= 12);
    }
    assert!(batches >= 2);
    assert_eq!(body, original.text);
    assert_eq!(
        context,
        original
            .metadata
            .as_ref()
            .unwrap()
            .context
            .as_ref()
            .unwrap()
            .as_str()
    );
    let stored = &store.events(&scope(), 1, 1).unwrap()[0].message;
    assert_eq!(stored.text, original.text);
    assert!(stored.metadata.as_ref().unwrap().fragment.is_none());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn lone_short_tail_flushes_after_persisted_deadline_without_new_input() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, training) = open(state.clone(), store.clone()).await;
    let mut message = event(0);
    message.metadata = None;
    training.observe_conversation(&scope(), message).unwrap();
    assert!(training.take_conversation_batch(10).unwrap().is_none());
    kernel.stop_all().await.unwrap();
    let (kernel, training) = open(state, store).await;
    assert!(training.take_conversation_batch(60_009).unwrap().is_none());
    let batch = training.take_conversation_batch(60_010).unwrap().unwrap();
    assert_eq!(batch.messages.len(), 1);
    training
        .finish_conversation_batch(&batch, vec![], 60_011)
        .unwrap();
    assert!(training.take_conversation_batch(120_000).unwrap().is_none());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn explicit_failure_retries_but_restart_does_not_replay_unconfirmed_claim() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("training.sqlite3");
    let store = Arc::new(SqliteExpressions::open(&path).unwrap());
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, training) = open(state.clone(), store.clone()).await;
    observe(training.as_ref(), 0, 4);
    let first = training.take_conversation_batch(1).unwrap().unwrap();
    training.fail_conversation_batch(&first, 2).unwrap();
    training.fail_conversation_batch(&first, 3).unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, training) = open(state.clone(), store.clone()).await;
    let retry = training.take_conversation_batch(60_002).unwrap().unwrap();
    assert_eq!(retry.id, first.id);
    assert_eq!(retry.messages, first.messages);
    assert_eq!(retry.attempt, 2);
    observe(training.as_ref(), 4, 4);
    kernel.stop_all().await.unwrap();
    let (kernel, training) = open(state, store.clone()).await;
    let next = training.take_conversation_batch(60_003).unwrap().unwrap();
    assert_ne!(next.id, retry.id);
    assert_eq!(checkpoint(store.as_ref())["cursor"]["sequence"], 5);
    training
        .finish_conversation_batch(&next, vec![], 60_004)
        .unwrap();
    assert_eq!(checkpoint(store.as_ref())["cursor"]["sequence"], 9);
    assert_eq!(
        batch_outcomes(&path),
        ["failed", "interrupted", "completed"]
    );
    assert_eq!(store.events(&scope(), 1, 256).unwrap().len(), 8);
    kernel.stop_all().await.unwrap();
}
fn batch_outcomes(path: &Path) -> Vec<String> {
    let db = rusqlite::Connection::open(path).unwrap();
    db.prepare("SELECT outcome FROM conversation_batches ORDER BY sequence")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[tokio::test]
async fn pause_preserves_unclaimed_backlog_and_reset_keeps_history_without_reactivation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("training.sqlite3");
    let store = Arc::new(SqliteExpressions::open(&path).unwrap());
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, training) = open(state.clone(), store.clone()).await;
    observe(training.as_ref(), 0, 4);
    let first = training.take_conversation_batch(1).unwrap().unwrap();
    training
        .finish_conversation_batch(
            &first,
            vec![draft(
                &first,
                [1, 2],
                None,
                ConversationAssessment::Correction,
                "查之前先接住对话",
            )],
            2,
        )
        .unwrap();
    let lesson = training.conversation_lessons(&scope()).unwrap().remove(0);
    observe(training.as_ref(), 4, 20);
    let claimed = training.take_conversation_batch(3).unwrap().unwrap();
    training.set_enabled(&scope(), false).unwrap();
    let cursor = checkpoint(store.as_ref())["cursor"]["sequence"]
        .as_u64()
        .unwrap();
    // 已领取批次含一条旧 Eve 背景，只越过十五条新事件。
    assert_eq!(claimed.messages.first().unwrap().id, "m-3");
    assert_eq!(cursor, 20);
    training.observe_conversation(&scope(), event(100)).unwrap();
    assert!(
        training
            .finish_conversation_batch(&claimed, vec![], 4)
            .is_err()
    );
    training.set_enabled(&scope(), true).unwrap();
    let backlog = training.take_conversation_batch(5).unwrap().unwrap();
    assert!(backlog.messages.iter().any(|m| m.id == "m-23"));
    training
        .finish_conversation_batch(&backlog, vec![], 6)
        .unwrap();
    training.reset_expression(&scope()).unwrap();
    assert!(training.conversation_lessons(&scope()).unwrap().is_empty());
    assert_eq!(store.events(&scope(), 1, 256).unwrap().len(), 24);
    assert_eq!(
        store
            .latest_revision(&scope(), &lesson.id)
            .unwrap()
            .unwrap()
            .reason,
        "reset"
    );
    observe(training.as_ref(), 0, 24);
    training.observe_conversation(&scope(), event(100)).unwrap();
    assert!(training.take_conversation_batch(100_000).unwrap().is_none());
    observe(training.as_ref(), 24, 4);
    let fresh = training.take_conversation_batch(100_001).unwrap().unwrap();
    assert!(fresh.lessons.is_empty());
    assert!(fresh.lesson_history.is_empty());
    assert!(
        fresh
            .messages
            .iter()
            .all(|m| m.metadata.as_ref().unwrap().sequence.unwrap() >= 25)
    );
    training
        .finish_conversation_batch(&fresh, vec![], 100_002)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, training) = open(state, store).await;
    assert!(training.conversation_lessons(&scope()).unwrap().is_empty());
    kernel.stop_all().await.unwrap();
}

struct AmbiguousStore {
    inner: Arc<SqliteExpressions>,
    fail_after_commit: AtomicBool,
}
impl ConversationStore for AmbiguousStore {
    fn import_legacy(&self, digest: &str, scopes: &[ConversationLegacyScope]) -> PluginResult<()> {
        ConversationStore::import_legacy(self.inner.as_ref(), digest, scopes)
    }
    fn append(
        &self,
        s: &ContextScope,
        m: &ConversationMessage,
        e: bool,
    ) -> PluginResult<ConversationAppend> {
        self.inner.append(s, m, e)
    }
    fn scopes(&self) -> PluginResult<Vec<ConversationStoredScope>> {
        self.inner.scopes()
    }
    fn scope(&self, s: &ContextScope) -> PluginResult<Option<ConversationStoredScope>> {
        self.inner.scope(s)
    }
    fn events(
        &self,
        s: &ContextScope,
        from: u64,
        limit: usize,
    ) -> PluginResult<Vec<ConversationEvent>> {
        self.inner.events(s, from, limit)
    }
    fn revisions(
        &self,
        s: &ContextScope,
        after: u64,
        limit: usize,
    ) -> PluginResult<Vec<ConversationLessonRevision>> {
        self.inner.revisions(s, after, limit)
    }
    fn latest_revision(
        &self,
        s: &ContextScope,
        id: &str,
    ) -> PluginResult<Option<ConversationLessonRevision>> {
        self.inner.latest_revision(s, id)
    }
    fn commit(&self, s: &ContextScope, c: &ConversationCommit) -> PluginResult<bool> {
        let result = self.inner.commit(s, c)?;
        if self.fail_after_commit.swap(false, Ordering::SeqCst) {
            return Err(PluginError::State("模拟提交已落盘但确认丢失".into()));
        }
        Ok(result)
    }
}

#[tokio::test]
async fn ambiguous_finish_never_overwrites_committed_result_or_requeues_it() {
    let dir = tempfile::tempdir().unwrap();
    let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let store = Arc::new(AmbiguousStore {
        inner: sql.clone(),
        fail_after_commit: AtomicBool::new(false),
    });
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, training) = open(state.clone(), store.clone()).await;
    observe(training.as_ref(), 0, 4);
    let batch = training.take_conversation_batch(1).unwrap().unwrap();
    store.fail_after_commit.store(true, Ordering::SeqCst);
    assert!(matches!(
        training.finish_conversation_batch(
            &batch,
            vec![draft(
                &batch,
                [1, 2],
                None,
                ConversationAssessment::Correction,
                "先回应再查询"
            )],
            2
        ),
        Err(PluginError::State(_))
    ));
    let persisted = sql.scope(&scope()).unwrap().unwrap();
    assert!(training.fail_conversation_batch(&batch, 3).is_err());
    assert!(training.observe_conversation(&scope(), event(5)).is_err());
    assert_eq!(sql.scope(&scope()).unwrap().unwrap(), persisted);
    kernel.stop_all().await.unwrap();
    let (kernel, training) = open(state, sql).await;
    assert_eq!(training.conversation_lessons(&scope()).unwrap().len(), 1);
    assert!(training.take_conversation_batch(60_000).unwrap().is_none());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn legacy_json_is_unchanged_and_available_pending_events_keep_unknown_time() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("training.sqlite3");
    let sql = Arc::new(SqliteExpressions::open(&path).unwrap());
    let state = Arc::new(MemoryStateStore::default());
    let id = PluginId::new(TRAINING_PLUGIN_ID).unwrap();
    let messages: Vec<_> = (0..6)
        .map(|index| {
            let mut message = event(index);
            message.metadata = None;
            message
        })
        .collect();
    let legacy_batch = ConversationBatch {
        id: "conversation-40-3".into(),
        scope: scope(),
        messages: messages[..4].to_vec(),
        lessons: vec![],
        lesson_history: vec![],
        attempt: 1,
    };
    let bytes = serde_json::to_vec(&serde_json::json!({"version":1,"next":41,"scopes":[{
        "scope":scope(),"messages":messages,"seen":[],"pending":2,"generation":3,"ready_at":0,
        "inflight":legacy_batch,"lessons":[]
    }]}))
    .unwrap();
    state
        .set(&id, "conversation.v1".into(), bytes.clone())
        .unwrap();
    let (kernel, training) = open(state.clone(), sql.clone()).await;
    assert_eq!(state.get(&id, "conversation.v1").unwrap().unwrap(), bytes);
    let rows = sql.events(&scope(), 1, 256).unwrap();
    assert_eq!(rows.len(), 6);
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row.sequence, index as u64 + 1);
        let metadata = row.message.metadata.as_ref().unwrap();
        assert_eq!(metadata.sequence, Some(row.sequence));
        assert!(metadata.observed_at_ms.is_none() && metadata.source_at_ms.is_none());
    }
    assert_eq!(checkpoint(sql.as_ref())["cursor"]["sequence"], 5);
    assert_eq!(batch_outcomes(&path), ["interrupted"]);
    assert!(training.take_conversation_batch(1).unwrap().is_none());
    let batch = training.take_conversation_batch(60_001).unwrap().unwrap();
    assert!(batch.messages.iter().any(|m| m.id == "m-5"));
    training
        .finish_conversation_batch(&batch, vec![], 60_002)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, training) = open(state.clone(), sql).await;
    assert_eq!(state.get(&id, "conversation.v1").unwrap().unwrap(), bytes);
    assert!(training.take_conversation_batch(120_000).unwrap().is_none());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn lesson_updates_keep_stable_id_and_versions_and_capacity_archives_sources() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let (kernel, training) = open(Arc::new(MemoryStateStore::default()), store.clone()).await;
    observe(training.as_ref(), 0, 4);
    let first = training.take_conversation_batch(100_000).unwrap().unwrap();
    training
        .finish_conversation_batch(
            &first,
            vec![draft(
                &first,
                [0, 2],
                None,
                ConversationAssessment::Observation,
                "先回应",
            )],
            100_001,
        )
        .unwrap();
    let id = training.conversation_lessons(&scope()).unwrap()[0]
        .id
        .clone();
    observe(training.as_ref(), 4, 4);
    let second = training.take_conversation_batch(100_002).unwrap().unwrap();
    let position = |batch: &ConversationBatch, id: &str| {
        batch.messages.iter().position(|m| m.id == id).unwrap()
    };
    training
        .finish_conversation_batch(
            &second,
            vec![draft(
                &second,
                [position(&second, "m-4"), position(&second, "m-6")],
                Some(id.clone()),
                ConversationAssessment::Observation,
                "需要查询时先自然回应，再按事实汇报",
            )],
            100_003,
        )
        .unwrap();
    let updated = training.conversation_lessons(&scope()).unwrap().remove(0);
    assert_eq!(updated.id, id);
    assert!(updated.active);
    assert_eq!(updated.observations, 2);
    assert_eq!(updated.response, "需要查询时先自然回应，再按事实汇报");
    let history = store.revisions(&scope(), 0, 256).unwrap();
    assert!(
        history
            .iter()
            .any(|r| r.lesson.id == id && r.lesson.response == "先回应")
    );
    assert!(
        history
            .iter()
            .any(|r| r.lesson.id == id && r.lesson.response == updated.response)
    );
    observe(training.as_ref(), 8, 4);
    let third = training.take_conversation_batch(100_004).unwrap().unwrap();
    training
        .finish_conversation_batch(
            &third,
            vec![draft(
                &third,
                [position(&third, "m-9"), position(&third, "m-10")],
                Some(id.clone()),
                ConversationAssessment::Counterexample,
                "忽略这个被反对的候选文本",
            )],
            100_005,
        )
        .unwrap();
    assert!(!training.conversation_lessons(&scope()).unwrap()[0].active);
    observe(training.as_ref(), 12, 4);
    let fourth = training.take_conversation_batch(100_006).unwrap().unwrap();
    training
        .finish_conversation_batch(
            &fourth,
            vec![draft(
                &fourth,
                [position(&fourth, "m-12"), position(&fourth, "m-14")],
                Some(id.clone()),
                ConversationAssessment::Observation,
                "仅在适合时自然回应",
            )],
            100_007,
        )
        .unwrap();
    let inactive = training.conversation_lessons(&scope()).unwrap().remove(0);
    assert!(!inactive.active);
    assert_eq!(inactive.counterexamples, 1);
    for n in 0..24 {
        let from = 16 + n * 4;
        observe(training.as_ref(), from, 4);
        let batch = training
            .take_conversation_batch(100_008 + n as u64 * 2)
            .unwrap()
            .unwrap();
        let mut candidate = draft(
            &batch,
            [
                position(&batch, &format!("m-{}", from + 1)),
                position(&batch, &format!("m-{}", from + 2)),
            ],
            None,
            ConversationAssessment::Correction,
            "按当前场合调整说法",
        );
        candidate.situation = format!("不同场合 {n}");
        training
            .finish_conversation_batch(&batch, vec![candidate], 100_009 + n as u64 * 2)
            .unwrap();
    }
    assert_eq!(training.conversation_lessons(&scope()).unwrap().len(), 24);
    assert!(
        !training
            .conversation_lessons(&scope())
            .unwrap()
            .iter()
            .any(|l| l.id == id)
    );
    let archived = store.latest_revision(&scope(), &id).unwrap().unwrap();
    assert_eq!(archived.reason, "capacity");
    assert_eq!(archived.lesson.evidence, inactive.evidence);
    assert_eq!(store.events(&scope(), 1, 256).unwrap().len(), 112);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn full_legacy_lessons_and_large_source_share_exact_budget_without_starvation() {
    let dir = tempfile::tempdir().unwrap();
    let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let state = Arc::new(MemoryStateStore::default());
    let lessons: Vec<_> = (0..MAX_CONVERSATION_LESSONS)
        .map(|index| ConversationLesson {
            id: format!("lesson-{index}"),
            situation: "\"".repeat(240),
            response: "\\".repeat(480),
            avoid: "\"".repeat(480),
            observations: 2,
            corrections: 0,
            counterexamples: 0,
            active: true,
            updated_at_ms: 0,
            evidence: (0..4)
                .map(|n| ConversationEvidence {
                    message_id: format!("{:0256}", n),
                    quote: "\"".repeat(240),
                })
                .collect(),
            evaluated_message_ids: (0..64).map(|n| format!("{:0256}", n)).collect(),
        })
        .collect();
    let bytes = serde_json::to_vec(&serde_json::json!({"version":1,"next":100,"scopes":[{
        "scope":scope(),"messages":[],"seen":[],"pending":0,"generation":1,"ready_at":0,
        "inflight":null,"lessons":lessons
    }]}))
    .unwrap();
    state
        .set(
            &PluginId::new(TRAINING_PLUGIN_ID).unwrap(),
            "conversation.v1".into(),
            bytes,
        )
        .unwrap();
    let (kernel, training) = open(state, sql.clone()).await;
    let mut message = event(0);
    message.text = "\"".repeat(MAX_CONVERSATION_BODY_BYTES);
    training
        .observe_conversation(&scope(), message.clone())
        .unwrap();
    let mut learned_pages = BTreeSet::new();
    let mut body = String::new();
    let mut now = 100_000;
    while let Some(batch) = training.take_conversation_batch(now).unwrap() {
        let serialized = conversation_prompt_json(&batch).unwrap();
        assert!(serialized.len() <= MAX_CONVERSATION_PROMPT_BYTES);
        assert!(!serialized.contains("evaluated_message_ids"));
        for lesson in &batch.lessons {
            learned_pages.insert(lesson.id.clone());
        }
        body.push_str(&batch.messages[0].text);
        training
            .finish_conversation_batch(&batch, vec![], now + 1)
            .unwrap();
        now += 2;
        assert!(now < 100_100);
    }
    assert_eq!(body, message.text);
    assert_eq!(learned_pages.len(), MAX_CONVERSATION_LESSONS);
    assert_eq!(
        sql.events(&scope(), 1, 256).unwrap()[0].message.text,
        message.text
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn oversized_legacy_claim_is_archived_whole_without_becoming_a_new_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("training.sqlite3");
    let store = Arc::new(SqliteExpressions::open(&path).unwrap());
    let state = Arc::new(MemoryStateStore::default());
    let messages: Vec<_> = (0..16)
        .map(|index| {
            let mut message = event(index);
            message.text = "\"".repeat(2_048);
            message.metadata = None;
            message
        })
        .collect();
    let batch = ConversationBatch {
        id: "legacy-large".into(),
        scope: scope(),
        messages: messages.clone(),
        lessons: vec![],
        lesson_history: vec![],
        attempt: 2,
    };
    assert!(conversation_prompt_json(&batch).unwrap().len() > MAX_CONVERSATION_PROMPT_BYTES);
    let bytes = serde_json::to_vec(&serde_json::json!({"version":1,"next":500,"scopes":[{
        "scope":scope(),"messages":messages,"seen":[],"pending":0,"generation":1,"ready_at":100,
        "inflight":batch,"lessons":[]
    }]}))
    .unwrap();
    let plugin = PluginId::new(TRAINING_PLUGIN_ID).unwrap();
    state
        .set(&plugin, "conversation.v1".into(), bytes.clone())
        .unwrap();
    let (kernel, training) = open(state.clone(), store.clone()).await;
    assert_eq!(
        state.get(&plugin, "conversation.v1").unwrap().unwrap(),
        bytes
    );
    assert_eq!(checkpoint(store.as_ref())["cursor"]["sequence"], 17);
    assert!(training.take_conversation_batch(100_000).unwrap().is_none());
    let db = rusqlite::Connection::open(&path).unwrap();
    let archived: Vec<u8> = db
        .query_row(
            "SELECT batch_json FROM conversation_batches WHERE outcome='interrupted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let archived: ConversationBatch = serde_json::from_slice(&archived).unwrap();
    assert_eq!(archived.id, batch.id);
    assert_eq!(archived.attempt, 2);
    assert_eq!(archived.messages.len(), 16);
    assert!(archived.messages.iter().all(|m| m.text.len() == 2_048 && m.metadata.as_ref().unwrap().observed_at_ms.is_none()));
    assert!(conversation_prompt_json(&archived).unwrap().len() > MAX_CONVERSATION_PROMPT_BYTES);
    kernel.stop_all().await.unwrap();
    let (kernel, training) = open(state, store).await;
    assert!(training.take_conversation_batch(200_000).unwrap().is_none());
    assert_eq!(batch_outcomes(&path), ["interrupted"]);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn full_history_sweep_preserves_lessons_order_live_input_and_request_identity() {
    let dir = tempfile::tempdir().unwrap();
    let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let state = Arc::new(MemoryStateStore::default());
    let (_kernel, training) = open(state.clone(), sql.clone()).await;
    observe(training.as_ref(), 0, 4);
    let first = training.take_conversation_batch(100000).unwrap().unwrap();
    training
        .finish_conversation_batch(
            &first,
            vec![draft(
                &first,
                [1, 2],
                None,
                ConversationAssessment::Correction,
                "先说明正在查",
            )],
            100001,
        )
        .unwrap();
    let old = training.conversation_lessons(&scope()).unwrap()[0].clone();
    let append = training
        .import_conversation_history(&ConversationHistoryRecord {
            scope: scope(),
            event: ConversationEvent {
                sequence: 5,
                message: event(4),
            },
        })
        .unwrap();
    assert!(append.inserted);
    observe(training.as_ref(), 5, 1); // 真实新输入位于两条历史导入之间。
    training
        .import_conversation_history(&ConversationHistoryRecord {
            scope: scope(),
            event: ConversationEvent {
                sequence: 6,
                message: event(6),
            },
        })
        .unwrap();
    let sources = [1, 2, 3, 4, 5, 7];
    training
        .request_conversation_sweep("full-a", &scope(), &sources)
        .unwrap();
    training
        .request_conversation_sweep("full-a", &scope(), &sources)
        .unwrap();
    let batch = training
        .take_conversation_maintenance_batch(200000, 4096)
        .unwrap()
        .unwrap();
    assert_eq!(
        batch
            .messages
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        ["m-0", "m-1", "m-2", "m-3", "m-4", "m-6"]
    );
    assert_eq!(batch.lessons.as_slice(), std::slice::from_ref(&old));
    let revised = draft(
        &batch,
        [1, 2],
        Some(old.id.clone()),
        ConversationAssessment::Correction,
        "有实际发现再说明进展",
    );
    training
        .finish_conversation_batch(&batch, vec![revised], 200001)
        .unwrap();
    let learned = training.conversation_lessons(&scope()).unwrap();
    assert_eq!(learned[0].corrections, old.corrections);
    assert_eq!(learned[0].response, "有实际发现再说明进展");
    let status = training.conversation_sweep_status("full-a").unwrap();
    assert!(status.completed);
    assert_eq!((status.total, status.processed, status.updated), (6, 6, 1));
    assert_eq!(
        sweep_counts(&status),
        &ConversationSweepMessageCounts::default()
    );
    let live = training.take_conversation_batch(300000).unwrap().unwrap();
    assert_eq!(live.messages.last().unwrap().id, "m-5");
    assert!(!live.messages.iter().any(|m| m.id == "m-6"));
    training
        .finish_conversation_batch(&live, vec![], 300001)
        .unwrap();
    assert!(training.take_conversation_batch(400000).unwrap().is_none());
    assert_eq!(checkpoint(sql.as_ref())["cursor"]["sequence"], 8);
    training
        .request_conversation_sweep("full-b", &scope(), &sources)
        .unwrap();
    let second = training
        .take_conversation_maintenance_batch(500000, 4096)
        .unwrap()
        .unwrap();
    training
        .finish_conversation_batch(&second, vec![], 500001)
        .unwrap();
    training
        .request_conversation_sweep("full-a", &scope(), &sources)
        .unwrap();
    assert!(training.take_conversation_batch(600000).unwrap().is_none());
    assert!(
        training
            .request_conversation_sweep("full-a", &scope(), &[1])
            .is_err()
    );
}

#[tokio::test]
async fn full_history_import_restores_disabled_original_without_changing_identity() {
    let dir = tempfile::tempdir().unwrap();
    let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let (_kernel, training) = open(Arc::new(MemoryStateStore::default()), sql.clone()).await;
    training.set_enabled(&scope(), false).unwrap();
    training.observe_conversation(&scope(), event(0)).unwrap();
    let record = ConversationHistoryRecord {
        scope: scope(),
        event: ConversationEvent {
            sequence: 1,
            message: event(0),
        },
    };
    let added = training.import_conversation_history(&record).unwrap();
    assert!(added.inserted);
    assert_eq!(added.sequence, Some(1));
    assert!(
        !training
            .import_conversation_history(&record)
            .unwrap()
            .inserted
    );
    training
        .request_conversation_sweep("explicit-full", &scope(), &[1])
        .unwrap();
    let batch = training
        .take_conversation_maintenance_batch(100000, 4096)
        .unwrap()
        .unwrap();
    assert_eq!(batch.messages[0].id, "m-0");
    training
        .finish_conversation_batch(&batch, vec![], 100001)
        .unwrap();
    assert!(
        training
            .conversation_sweep_status("explicit-full")
            .unwrap()
            .completed
    );
    assert!(!training.enabled(&scope()).unwrap());
    let mut conflicting = record;
    conflicting.event.message.text.push_str("changed");
    assert!(training.import_conversation_history(&conflicting).is_err());
}

#[tokio::test]
async fn full_history_budget_and_restart_keep_unknown_terminal_and_later_events_available() {
    let dir = tempfile::tempdir().unwrap();
    let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, training) = open(state.clone(), sql.clone()).await;
    observe(training.as_ref(), 0, 140);
    training
        .request_conversation_sweep("full-unknown", &scope(), &(1..=140).collect::<Vec<_>>())
        .unwrap();
    let claimed = training
        .take_conversation_maintenance_batch(100000, 1)
        .unwrap()
        .unwrap();
    assert!(claimed.messages.len() > MAX_CONVERSATION_MESSAGES);
    assert!(claimed.messages.len() <= MAX_CONVERSATION_REVISIT_MESSAGES);
    assert!(conversation_prompt_json(&claimed).unwrap().len() <= MAX_CONVERSATION_PROMPT_BYTES);
    assert!(
        training
            .take_conversation_maintenance_batch(100001, 1)
            .unwrap()
            .is_none()
    );
    kernel
        .stop(&PluginId::new(TRAINING_PLUGIN_ID).unwrap())
        .await
        .unwrap();
    let (_kernel, reopened) = open(state, sql).await;
    let progress = reopened.conversation_sweep_status("full-unknown").unwrap();
    assert_eq!(progress.interrupted_batches, 1);
    assert_eq!(progress.model_requests, 1);
    assert_eq!(progress.processed, 0);
    assert_eq!(
        sweep_counts(&progress).unknown,
        claimed.messages.len() as u64
    );
    let next = reopened
        .take_conversation_maintenance_batch(200000, 4096)
        .unwrap()
        .unwrap();
    assert!(
        !next
            .messages
            .iter()
            .any(|m| claimed.messages.iter().any(|old| old.id == m.id))
    );
    reopened
        .finish_conversation_batch(&next, vec![], 200001)
        .unwrap();
    assert!(
        reopened
            .conversation_sweep_status("full-unknown")
            .unwrap()
            .completed
    );
}

#[tokio::test]
async fn full_history_fragment_outcomes_survive_restart_and_count_whole_messages() {
    for (first_outcome, last_outcome, failed, unknown, processed) in [
        ("failed", "completed", 1, 0, 1),
        ("interrupted", "completed", 0, 1, 1),
        ("completed", "failed", 2, 0, 0),
        ("failed", "interrupted", 0, 2, 0),
        ("interrupted", "failed", 1, 1, 0),
        ("completed", "completed", 0, 0, 2),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("training.sqlite3");
        let sql = Arc::new(SqliteExpressions::open(&path).unwrap());
        let state = Arc::new(MemoryStateStore::default());
        let (kernel, training) = open(state.clone(), sql.clone()).await;
        let mut original = event(0);
        original.text = "\\".repeat(MAX_CONVERSATION_BODY_BYTES);
        original.metadata.as_mut().unwrap().context =
            Some("\n".repeat(MAX_CONVERSATION_CONTEXT_BYTES));
        training
            .observe_conversation(&scope(), original.clone())
            .unwrap();
        training.observe_conversation(&scope(), event(1)).unwrap();
        training
            .request_conversation_sweep("fragment-outcomes", &scope(), &[1, 2])
            .unwrap();
        let mut now = 100_000;
        let first = training
            .take_conversation_maintenance_batch(now, 4096)
            .unwrap()
            .unwrap();
        assert_eq!(first.messages.len(), 1);
        let first_fragment = first.messages[0]
            .metadata
            .as_ref()
            .unwrap()
            .fragment
            .as_ref()
            .unwrap()
            .clone();
        assert!(
            first_fragment.text_end + first_fragment.context_end
                < original.text.len() + MAX_CONVERSATION_CONTEXT_BYTES
        );
        match first_outcome {
            "failed" => fail_sweep_batch(training.as_ref(), first.clone(), &mut now),
            "completed" => {
                training
                    .finish_conversation_batch(&first, vec![], now)
                    .unwrap();
            }
            "interrupted" => {}
            _ => unreachable!(),
        }
        kernel.stop_all().await.unwrap();
        let (kernel, training) = open(state.clone(), sql.clone()).await;
        let partial = training
            .conversation_sweep_status("fragment-outcomes")
            .unwrap();
        let counts = sweep_counts(&partial);
        assert_eq!(
            (partial.processed, partial.remaining, counts.partial),
            (0, 2, 1)
        );
        assert_eq!((counts.failed, counts.unknown), (0, 0));
        assert_eq!(counts.partial_failed, u64::from(first_outcome == "failed"));
        assert_eq!(
            counts.partial_unknown,
            u64::from(first_outcome == "interrupted")
        );
        now += 200_000;
        let last = training
            .take_conversation_maintenance_batch(now, 4096)
            .unwrap()
            .unwrap();
        assert_ne!(last.id, first.id);
        assert_eq!(last.messages.len(), 2);
        let last_fragment = last.messages[0]
            .metadata
            .as_ref()
            .unwrap()
            .fragment
            .as_ref()
            .unwrap();
        assert_eq!(last_fragment.text_start, first_fragment.text_end);
        assert_eq!(last_fragment.context_start, first_fragment.context_end);
        assert_eq!(
            format!("{}{}", first.messages[0].text, last.messages[0].text),
            original.text
        );
        match last_outcome {
            "failed" => fail_sweep_batch(training.as_ref(), last, &mut now),
            "completed" => {
                training
                    .finish_conversation_batch(&last, vec![], now)
                    .unwrap();
            }
            "interrupted" => {}
            _ => unreachable!(),
        }
        kernel.stop_all().await.unwrap();
        let (kernel, training) = open(state, sql.clone()).await;
        let done = training
            .conversation_sweep_status("fragment-outcomes")
            .unwrap();
        let counts = sweep_counts(&done);
        assert!(done.completed);
        assert_eq!(
            (done.processed, counts.failed, counts.unknown),
            (processed, failed, unknown)
        );
        assert_eq!(counts.partial, 0);
        assert_eq!(
            done.failed_batches,
            u64::from(first_outcome == "failed") + u64::from(last_outcome == "failed")
        );
        assert_eq!(
            done.interrupted_batches,
            u64::from(first_outcome == "interrupted") + u64::from(last_outcome == "interrupted")
        );
        assert!(
            training
                .take_conversation_maintenance_batch(now + 200_000, 4096)
                .unwrap()
                .is_none()
        );
        assert!(
            training
                .take_conversation_batch(now + 200_000)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            checkpoint(sql.as_ref())["sweep"]["covered"],
            serde_json::json!([1, 2])
        );
        assert_eq!(
            sql.events(&scope(), 1, 2).unwrap()[0].message.text,
            original.text
        );
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn full_history_legacy_statistics_stay_unclassified_without_changing_consumption() {
    let dir = tempfile::tempdir().unwrap();
    let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, training) = open(state.clone(), sql.clone()).await;
    observe(training.as_ref(), 0, 4);
    training
        .request_conversation_sweep("legacy-complete", &scope(), &[1, 2, 3, 4])
        .unwrap();
    let batch = training
        .take_conversation_maintenance_batch(100_000, 4096)
        .unwrap()
        .unwrap();
    training
        .finish_conversation_batch(&batch, vec![], 100_001)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let mut old = checkpoint(sql.as_ref());
    old["sweep"].as_object_mut().unwrap().remove("accounting");
    old["sweep"]
        .as_object_mut()
        .unwrap()
        .remove("successful_context_from");
    replace_checkpoint(sql.as_ref(), &old);
    let old_bytes = sql.scope(&scope()).unwrap().unwrap().checkpoint.unwrap();
    let (kernel, training) = open(state.clone(), sql.clone()).await;
    let status = training
        .conversation_sweep_status("legacy-complete")
        .unwrap();
    let counts = sweep_counts(&status);
    assert!(status.completed);
    assert_eq!(
        (
            status.processed,
            status.remaining,
            counts.unclassified,
            counts.legacy_processed
        ),
        (0, 0, 4, 4)
    );
    assert_eq!(
        sql.scope(&scope()).unwrap().unwrap().checkpoint.unwrap(),
        old_bytes
    );
    training
        .request_conversation_sweep("after-legacy", &scope(), &[1])
        .unwrap();
    let saved = checkpoint(sql.as_ref());
    assert_eq!(saved["cursor"], old["cursor"]);
    assert_eq!(saved["lessons"], old["lessons"]);
    assert_eq!(
        saved["sweep_archive"][0]["covered"],
        old["sweep"]["covered"]
    );
    kernel.stop_all().await.unwrap();
    let (kernel, training) = open(state, sql).await;
    assert_eq!(
        training
            .conversation_sweep_status("legacy-complete")
            .unwrap(),
        status
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn full_history_legacy_partial_cannot_become_success_after_its_last_fragment() {
    let dir = tempfile::tempdir().unwrap();
    let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, training) = open(state.clone(), sql.clone()).await;
    let mut message = event(0);
    message.text = "\\".repeat(MAX_CONVERSATION_BODY_BYTES);
    training.observe_conversation(&scope(), message).unwrap();
    training
        .request_conversation_sweep("legacy-partial", &scope(), &[1])
        .unwrap();
    let first = training
        .take_conversation_maintenance_batch(100_000, 4096)
        .unwrap()
        .unwrap();
    training
        .finish_conversation_batch(&first, vec![], 100_001)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let mut old = checkpoint(sql.as_ref());
    old["sweep"].as_object_mut().unwrap().remove("accounting");
    old["sweep"]
        .as_object_mut()
        .unwrap()
        .remove("successful_context_from");
    replace_checkpoint(sql.as_ref(), &old);
    let (kernel, training) = open(state, sql.clone()).await;
    let status = training
        .conversation_sweep_status("legacy-partial")
        .unwrap();
    assert_eq!(
        (
            status.processed,
            status.remaining,
            sweep_counts(&status).partial_unknown
        ),
        (0, 1, 1)
    );
    assert_eq!(checkpoint(sql.as_ref()), old);
    let last = training
        .take_conversation_maintenance_batch(200_000, 4096)
        .unwrap()
        .unwrap();
    assert_ne!(first.id, last.id);
    training
        .finish_conversation_batch(&last, vec![], 200_001)
        .unwrap();
    let done = training
        .conversation_sweep_status("legacy-partial")
        .unwrap();
    assert!(done.completed);
    assert_eq!((done.processed, sweep_counts(&done).unknown), (0, 1));
    assert_eq!(
        checkpoint(sql.as_ref())["sweep"]["covered"],
        serde_json::json!([1])
    );
    kernel.stop_all().await.unwrap();
}

#[test]
fn old_sweep_status_does_not_default_message_results_to_success() {
    let status: ConversationSweepStatus = serde_json::from_value(serde_json::json!({
        "id": "old", "scopes": 1, "total": 4, "processed": 4, "remaining": 0,
        "updated": 0, "failed_batches": 0, "interrupted_batches": 0,
        "model_requests": 1, "completed": true
    }))
    .unwrap();
    assert!(status.message_counts.is_none());
}

#[tokio::test]
async fn pending_and_archived_sweeps_leave_unreserved_live_messages_available() {
    let dir = tempfile::tempdir().unwrap();
    let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let (kernel, training) = open(Arc::new(MemoryStateStore::default()), sql.clone()).await;
    observe(training.as_ref(), 0, 150);
    let sources: Vec<_> = (1..=150)
        .filter(|sequence| *sequence != 3 && *sequence != 149)
        .collect();
    training
        .request_conversation_sweep("with-live-holes", &scope(), &sources)
        .unwrap();
    let first = training
        .take_conversation_maintenance_batch(100_000, 1)
        .unwrap()
        .unwrap();
    assert!(training.take_conversation_batch(100_001).unwrap().is_none());
    training
        .finish_conversation_batch(&first, vec![], 100_002)
        .unwrap();
    let progress = training
        .conversation_sweep_status("with-live-holes")
        .unwrap();
    assert!(!progress.completed);
    assert!(
        training
            .take_conversation_maintenance_batch(200_000, 1)
            .unwrap()
            .is_none()
    );
    let live = training.take_conversation_batch(200_000).unwrap().unwrap();
    assert_eq!(live.messages.last().unwrap().id, "m-2");
    assert_eq!(checkpoint(sql.as_ref())["cursor"]["sequence"], 3);
    training
        .finish_conversation_batch(&live, vec![], 200_001)
        .unwrap();
    assert_eq!(
        training
            .conversation_sweep_status("with-live-holes")
            .unwrap(),
        progress
    );

    // 旧请求已终态但普通位置尚未经过尾部；换请求后仍不得重投它。
    let last = training
        .take_conversation_maintenance_batch(300_000, 4096)
        .unwrap()
        .unwrap();
    let mut now = 300_001;
    fail_sweep_batch(training.as_ref(), last, &mut now);
    let done = training
        .conversation_sweep_status("with-live-holes")
        .unwrap();
    assert!(done.completed);
    assert!(sweep_counts(&done).failed > 0);
    assert_eq!(checkpoint(sql.as_ref())["cursor"]["sequence"], 149);
    training
        .request_conversation_sweep("later-request", &scope(), &[1])
        .unwrap();
    let live = training.take_conversation_batch(now).unwrap().unwrap();
    assert_eq!(live.messages.last().unwrap().id, "m-148");
    assert!(!live.messages.iter().any(|message| message.id == "m-149"));
    training
        .finish_conversation_batch(&live, vec![], now + 1)
        .unwrap();
    assert!(training.take_conversation_batch(now + 2).unwrap().is_none());
    assert_eq!(checkpoint(sql.as_ref())["cursor"]["sequence"], 151);
    assert!(
        !training
            .conversation_sweep_status("later-request")
            .unwrap()
            .completed
    );
    assert_eq!(
        training
            .conversation_sweep_status("with-live-holes")
            .unwrap(),
        done
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn exhausted_sweep_releases_only_explicitly_failed_claim_without_another_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("training.sqlite3");
    let sql = Arc::new(SqliteExpressions::open(&path).unwrap());
    let (kernel, training) = open(Arc::new(MemoryStateStore::default()), sql.clone()).await;
    observe(training.as_ref(), 0, 140);
    training
        .request_conversation_sweep("hard-budget", &scope(), &(1..=140).collect::<Vec<_>>())
        .unwrap();
    let mut seeded = checkpoint(sql.as_ref());
    // 种下累计请求计数，只运行最后一次真实状态机领取；不伪造批次或结果。
    seeded["sweep"]["model_requests"] = 4095.into();
    replace_checkpoint(sql.as_ref(), &seeded);
    let claimed = training
        .take_conversation_maintenance_batch(100_000, 4096)
        .unwrap()
        .unwrap();
    assert_eq!(claimed.attempt, 1);
    observe(training.as_ref(), 140, 4);
    let in_flight = checkpoint(sql.as_ref());
    assert!(training.take_conversation_batch(100_001).unwrap().is_none());
    assert_eq!(checkpoint(sql.as_ref()), in_flight);
    assert!(batch_outcomes(&path).is_empty());
    training.fail_conversation_batch(&claimed, 100_002).unwrap();
    assert!(
        training
            .take_conversation_maintenance_batch(100_003, 4096)
            .unwrap()
            .is_none()
    );
    assert!(training.take_conversation_batch(100_003).unwrap().is_none());
    let progress = training.conversation_sweep_status("hard-budget").unwrap();
    assert_eq!(
        (
            progress.model_requests,
            progress.failed_batches,
            progress.processed
        ),
        (4096, 1, 0)
    );
    assert_eq!(
        sweep_counts(&progress).failed,
        claimed.messages.len() as u64
    );
    assert_eq!(batch_outcomes(&path), ["failed", "budget_exhausted"]);
    let live = training.take_conversation_batch(100_004).unwrap().unwrap();
    assert_eq!(live.messages.last().unwrap().id, "m-143");
    training
        .finish_conversation_batch(&live, vec![], 100_005)
        .unwrap();
    assert_eq!(
        training.conversation_sweep_status("hard-budget").unwrap(),
        progress
    );
    assert!(!progress.completed);
    assert!(checkpoint(sql.as_ref())["inflight"].is_null());
    assert_eq!(checkpoint(sql.as_ref())["cursor"]["sequence"], 145);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn sweep_reservation_keeps_existing_normal_claim_and_disabled_scope_rules() {
    let dir = tempfile::tempdir().unwrap();
    let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let (kernel, training) = open(Arc::new(MemoryStateStore::default()), sql.clone()).await;
    observe(training.as_ref(), 0, 8);
    let ordinary = training.take_conversation_batch(100_000).unwrap().unwrap();
    training
        .request_conversation_sweep("existing-normal", &scope(), &[1, 2])
        .unwrap();
    assert!(
        training
            .take_conversation_maintenance_batch(100_001, 4096)
            .unwrap()
            .is_none()
    );
    training
        .finish_conversation_batch(&ordinary, vec![], 100_002)
        .unwrap();
    let normal_cursor = checkpoint(sql.as_ref())["cursor"].clone();
    training.set_enabled(&scope(), false).unwrap();
    assert!(training.take_conversation_batch(200_000).unwrap().is_none());
    let maintenance = training
        .take_conversation_maintenance_batch(200_000, 4096)
        .unwrap()
        .unwrap();
    training
        .finish_conversation_batch(&maintenance, vec![], 200_001)
        .unwrap();
    assert_eq!(checkpoint(sql.as_ref())["cursor"], normal_cursor);
    assert_eq!(
        training
            .conversation_sweep_status("existing-normal")
            .unwrap()
            .processed,
        2
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn full_history_unknown_order_is_not_a_human_feedback_pair() {
    let dir = tempfile::tempdir().unwrap();
    let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let (_kernel, training) = open(Arc::new(MemoryStateStore::default()), sql).await;
    for index in 0..4 {
        let mut message = event(index);
        message.metadata.as_mut().unwrap().ordering_uncertain = true;
        training.observe_conversation(&scope(), message).unwrap();
    }
    training
        .request_conversation_sweep("full-uncertain", &scope(), &[1, 2, 3, 4])
        .unwrap();
    let batch = training
        .take_conversation_maintenance_batch(100000, 4096)
        .unwrap()
        .unwrap();
    let prompt: serde_json::Value =
        serde_json::from_str(&conversation_prompt_json(&batch).unwrap()).unwrap();
    assert_eq!(prompt["feedback_pairs"], serde_json::json!([]));
    assert!(
        training
            .finish_conversation_batch(
                &batch,
                vec![draft(
                    &batch,
                    [1, 2],
                    None,
                    ConversationAssessment::Correction,
                    "不能猜测原话时序"
                )],
                100001
            )
            .is_err()
    );
    training
        .finish_conversation_batch(&batch, vec![], 100002)
        .unwrap();
}
