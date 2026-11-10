//! Regression coverage for historical revisions paired with authoritative latest lessons.
use super::*;

fn large_lesson(id: &str, revision: &str) -> ConversationLesson {
    let mut response = revision.to_owned();
    response.push_str(&"\\".repeat(480 - response.len()));
    ConversationLesson {
        id: id.into(),
        situation: "\"".repeat(240),
        response,
        avoid: "\"".repeat(480),
        observations: 2,
        corrections: 1,
        counterexamples: 0,
        active: true,
        updated_at_ms: 1,
        evidence: (0..4)
            .map(|index| ConversationEvidence {
                message_id: format!("{index:0256}"),
                quote: "\"".repeat(240),
            })
            .collect(),
        evaluated_message_ids: vec!["m-0".into(), "m-2".into()],
    }
}

fn historical(lesson: ConversationLesson, reason: &str) -> ConversationLessonRevision {
    ConversationLessonRevision {
        sequence: 0,
        lesson,
        reason: reason.into(),
        batch_id: Some("historical-source".into()),
        recorded_at_ms: Some(1),
    }
}

fn seeded_state(
    lessons: &[ConversationLesson],
    history: &[ConversationLessonRevision],
) -> Arc<MemoryStateStore> {
    let state = Arc::new(MemoryStateStore::default());
    state
        .set(
            &PluginId::new(TRAINING_PLUGIN_ID).unwrap(),
            "conversation.v1".into(),
            serde_json::to_vec(&serde_json::json!({
                "version": 1, "next": 1000, "scopes": [{
                    "scope": scope(), "messages": [], "seen": [], "pending": 0,
                    "generation": 1, "ready_at": 0, "inflight": null,
                    "lessons": lessons, "history": history
                }]
            }))
            .unwrap(),
        )
        .unwrap();
    state
}

fn assert_latest_baselines(store: &dyn ConversationStore, batch: &ConversationBatch) {
    assert!(batch.lessons.len() <= MAX_CONVERSATION_LESSONS);
    assert!(batch.lesson_history.len() <= 4);
    assert!(conversation_prompt_json(batch).unwrap().len() <= MAX_CONVERSATION_PROMPT_BYTES);
    let ids: BTreeSet<_> = batch.lessons.iter().map(|lesson| &lesson.id).collect();
    assert_eq!(ids.len(), batch.lessons.len());
    for revision in &batch.lesson_history {
        let latest = store
            .latest_revision(&batch.scope, &revision.lesson.id)
            .unwrap()
            .unwrap();
        assert_ne!(latest.reason, "reset");
        assert!(
            batch.lessons.contains(&latest.lesson),
            "history {} must be accompanied by its authoritative latest baseline",
            revision.lesson.id
        );
    }
}

#[tokio::test]
async fn history_revision_carries_latest_baseline_when_current_page_would_omit_it() {
    let dir = tempfile::tempdir().unwrap();
    let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let current: Vec<_> = (0..MAX_CONVERSATION_LESSONS)
        .map(|index| large_lesson(&format!("lesson-{index}"), "v2-independent-boundary"))
        .collect();
    let latest = current.last().unwrap().clone();
    let mut old = latest.clone();
    old.response = "v1-before-independent-boundary".into();
    old.updated_at_ms = 0;
    let state = seeded_state(&current, &[historical(old.clone(), "before_update")]);
    let (kernel, training) = open(state, sql.clone()).await;
    observe(training.as_ref(), 0, 4);
    let batch = training.take_conversation_batch(100_000).unwrap().unwrap();
    assert!(
        batch
            .lesson_history
            .iter()
            .any(|revision| revision.lesson == old)
    );
    assert_latest_baselines(sql.as_ref(), &batch);
    assert!(batch.lessons.contains(&latest));
    assert!(!batch.lessons.contains(&old));
    training
        .finish_conversation_batch(&batch, vec![], 100_001)
        .unwrap();
    assert_eq!(training.conversation_lessons(&scope()).unwrap(), current);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn capacity_archive_baseline_retries_frozen_and_revisits_without_new_support() {
    let dir = tempfile::tempdir().unwrap();
    let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let current: Vec<_> = (0..MAX_CONVERSATION_LESSONS)
        .map(|index| large_lesson(&format!("resident-{index}"), "resident"))
        .collect();
    let latest = large_lesson("archived-target", "v2-independent-boundary");
    let mut old = latest.clone();
    old.response = "v1-before-independent-boundary".into();
    old.updated_at_ms = 0;
    let state = seeded_state(
        &current,
        &[
            historical(old.clone(), "updated"),
            historical(latest.clone(), "capacity"),
        ],
    );
    let (kernel, training) = open(state.clone(), sql.clone()).await;
    observe(training.as_ref(), 0, 4);
    training
        .request_conversation_sweep("baseline-revisit", &scope(), &[1, 2, 3, 4])
        .unwrap();
    let batch = training
        .take_conversation_maintenance_batch(100_000, 4096)
        .unwrap()
        .unwrap();
    assert!(
        batch
            .lesson_history
            .iter()
            .any(|revision| revision.lesson == old)
    );
    assert_latest_baselines(sql.as_ref(), &batch);
    assert!(batch.lessons.contains(&latest));
    assert_eq!(training.conversation_lessons(&scope()).unwrap(), current);
    training.fail_conversation_batch(&batch, 100_001).unwrap();
    kernel.stop_all().await.unwrap();

    let (kernel, training) = open(state, sql.clone()).await;
    let retry = training
        .take_conversation_maintenance_batch(300_000, 4096)
        .unwrap()
        .unwrap();
    assert_eq!(retry.id, batch.id);
    assert_eq!(retry.attempt, 2);
    assert_eq!(retry.messages, batch.messages);
    assert_eq!(retry.lessons, batch.lessons);
    assert_eq!(retry.lesson_history, batch.lesson_history);
    let indices = ["m-0", "m-2"].map(|id| retry.messages.iter().position(|m| m.id == id).unwrap());
    let candidate = draft(
        &retry,
        indices,
        Some(latest.id.clone()),
        ConversationAssessment::Observation,
        "revised-using-authoritative-latest",
    );
    assert_eq!(
        training
            .finish_conversation_batch(&retry, vec![candidate], 300_001)
            .unwrap(),
        1
    );
    let resident = training.conversation_lessons(&scope()).unwrap();
    assert_eq!(resident.len(), MAX_CONVERSATION_LESSONS);
    let restored = resident
        .iter()
        .find(|lesson| lesson.id == latest.id)
        .unwrap();
    assert_eq!(restored.response, "revised-using-authoritative-latest");
    assert_eq!(restored.observations, latest.observations);
    assert_eq!(restored.corrections, latest.corrections);
    assert_eq!(restored.counterexamples, latest.counterexamples);
    assert_eq!(restored.active, latest.active);
    let history = sql.revisions(&scope(), 0, 256).unwrap();
    assert!(history.iter().any(|revision| revision.lesson == latest));
    assert!(
        history
            .iter()
            .any(|revision| revision.reason == "capacity" && revision.lesson.id != latest.id)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn escaped_current_and_archive_pages_both_progress_with_exact_budget() {
    let dir = tempfile::tempdir().unwrap();
    let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let mut current: Vec<_> = (0..MAX_CONVERSATION_LESSONS)
        .map(|index| large_lesson(&format!("current-{index}"), "latest"))
        .collect();
    for lesson in &mut current {
        // The current contract permits these in avoid; JSON requires six bytes each.
        lesson.avoid = "\0".repeat(480);
    }
    let mut history = vec![];
    let mut expected_archives = BTreeSet::new();
    for index in 0..5 {
        let mut latest = large_lesson(&format!("archive-{index}"), "latest-archive");
        latest.avoid = "\0".repeat(480);
        let mut old = latest.clone();
        old.response = "old-archive".into();
        old.updated_at_ms = 0;
        expected_archives.insert(latest.id.clone());
        history.push(historical(old, "updated"));
        history.push(historical(latest, "capacity"));
    }
    let state = seeded_state(&current, &history);
    let (kernel, training) = open(state, sql.clone()).await;
    let mut seen_current = BTreeSet::new();
    let mut seen_archives = BTreeSet::new();
    let expected_current: BTreeSet<_> = current.iter().map(|lesson| lesson.id.clone()).collect();
    for round in 0..MAX_CONVERSATION_LESSONS {
        observe(training.as_ref(), round * 4, 4);
        let now = 100_000 + round as u64 * 100_000;
        let before = checkpoint(sql.as_ref())["cursor"]["sequence"]
            .as_u64()
            .unwrap();
        let batch = training.take_conversation_batch(now).unwrap().unwrap();
        assert_latest_baselines(sql.as_ref(), &batch);
        for lesson in &batch.lessons {
            if expected_current.contains(&lesson.id) {
                seen_current.insert(lesson.id.clone());
            }
        }
        for revision in &batch.lesson_history {
            if expected_archives.contains(&revision.lesson.id) {
                seen_archives.insert(revision.lesson.id.clone());
            }
        }
        training
            .finish_conversation_batch(&batch, vec![], now + 1)
            .unwrap();
        assert!(
            checkpoint(sql.as_ref())["cursor"]["sequence"]
                .as_u64()
                .unwrap()
                > before
        );
        if seen_current == expected_current && seen_archives == expected_archives {
            break;
        }
    }
    assert_eq!(seen_current, expected_current);
    assert_eq!(seen_archives, expected_archives);
    assert_eq!(training.conversation_lessons(&scope()).unwrap(), current);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn latest_only_archive_keeps_its_source_and_pending_input_frozen() {
    let dir = tempfile::tempdir().unwrap();
    let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let current = vec![large_lesson("resident", "resident")];
    let latest = large_lesson("only-archive", "authoritative-only-version");
    let state = seeded_state(&current, &[historical(latest.clone(), "capacity")]);
    let (kernel, training) = open(state.clone(), sql.clone()).await;
    observe(training.as_ref(), 0, 4);
    let batch = training.take_conversation_batch(100_000).unwrap().unwrap();
    let original = sql.latest_revision(&scope(), &latest.id).unwrap().unwrap();
    assert!(batch.lessons.contains(&latest));
    assert!(batch.lesson_history.contains(&original));
    assert_latest_baselines(sql.as_ref(), &batch);
    let frozen = checkpoint(sql.as_ref())["inflight"].clone();
    observe(training.as_ref(), 4, 4);
    assert_eq!(checkpoint(sql.as_ref())["inflight"], frozen);
    assert_eq!(
        sql.latest_revision(&scope(), &latest.id).unwrap(),
        Some(original)
    );
    assert_eq!(training.conversation_lessons(&scope()).unwrap(), current);
    assert!(training.take_conversation_batch(200_000).unwrap().is_none());
    training.fail_conversation_batch(&batch, 200_001).unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, training) = open(state, sql.clone()).await;
    let retry = training.take_conversation_batch(400_000).unwrap().unwrap();
    let mut expected = batch;
    expected.attempt = 2;
    assert_eq!(retry, expected);
    training
        .finish_conversation_batch(&retry, vec![], 400_001)
        .unwrap();
    assert_eq!(training.conversation_lessons(&scope()).unwrap(), current);
    assert_eq!(sql.events(&scope(), 1, 256).unwrap().len(), 8);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn legacy_history_only_claim_retries_original_input_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("training.sqlite3");
    let sql = Arc::new(SqliteExpressions::open(&path).unwrap());
    let latest = large_lesson("legacy-archive", "latest-authoritative");
    let mut old = latest.clone();
    old.response = "older-source".into();
    // Source order, rather than a timestamp heuristic, defines the latest revision.
    old.updated_at_ms = 999_999;
    let state = seeded_state(
        &[],
        &[
            historical(old, "updated"),
            historical(latest.clone(), "capacity"),
        ],
    );
    let (kernel, training) = open(state.clone(), sql.clone()).await;
    observe(training.as_ref(), 0, 4);
    let mut old_batch = training.take_conversation_batch(100_000).unwrap().unwrap();
    assert!(old_batch.lessons.contains(&latest));
    assert!(!old_batch.lesson_history.is_empty());
    // Reconstruct an otherwise-valid pre-upgrade claim through the existing fixture CAS.
    // The following failure is recorded by the service against these exact old inputs.
    old_batch.lessons.clear();
    let mut saved = checkpoint(sql.as_ref());
    saved["inflight"]["batch"] = serde_json::to_value(&old_batch).unwrap();
    replace_checkpoint(sql.as_ref(), &saved);
    training
        .fail_conversation_batch(&old_batch, 100_001)
        .unwrap();
    assert_eq!(batch_outcomes(&path), ["failed"]);
    let failed: ConversationBatch =
        serde_json::from_slice(&archived_batch_bytes(&path)[0]).unwrap();
    assert_eq!(failed, old_batch);
    let failed_checkpoint = sql.scope(&scope()).unwrap().unwrap().checkpoint.unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, training) = open(state, sql.clone()).await;
    assert_eq!(
        sql.scope(&scope()).unwrap().unwrap().checkpoint.unwrap(),
        failed_checkpoint
    );
    let retry = training.take_conversation_batch(300_000).unwrap().unwrap();
    old_batch.attempt = 2;
    assert_eq!(retry, old_batch);
    assert!(retry.lessons.is_empty());
    training
        .finish_conversation_batch(&retry, vec![], 300_001)
        .unwrap();
    assert_eq!(batch_outcomes(&path), ["failed", "completed"]);
    assert!(training.conversation_lessons(&scope()).unwrap().is_empty());
    assert_eq!(
        sql.latest_revision(&scope(), &latest.id)
            .unwrap()
            .unwrap()
            .lesson,
        latest
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn reset_keeps_archived_and_current_lessons_out_of_new_input() {
    let dir = tempfile::tempdir().unwrap();
    let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let resident = large_lesson("reset-resident", "resident");
    let archived = large_lesson("reset-archive", "archived");
    let state = seeded_state(
        std::slice::from_ref(&resident),
        &[historical(archived.clone(), "capacity")],
    );
    let (kernel, training) = open(state.clone(), sql.clone()).await;
    training.reset_expression(&scope()).unwrap();
    let history = sql.revisions(&scope(), 0, 256).unwrap();
    assert_eq!(
        sql.latest_revision(&scope(), &resident.id)
            .unwrap()
            .unwrap()
            .reason,
        "reset"
    );
    assert_eq!(
        sql.latest_revision(&scope(), &archived.id)
            .unwrap()
            .unwrap()
            .reason,
        "capacity"
    );
    observe(training.as_ref(), 0, 4);
    let batch = training.take_conversation_batch(100_000).unwrap().unwrap();
    assert!(batch.lessons.is_empty());
    assert!(batch.lesson_history.is_empty());
    training
        .finish_conversation_batch(&batch, vec![], 100_001)
        .unwrap();
    assert_eq!(sql.revisions(&scope(), 0, 256).unwrap(), history);
    kernel.stop_all().await.unwrap();
    let (kernel, training) = open(state, sql.clone()).await;
    assert!(training.conversation_lessons(&scope()).unwrap().is_empty());
    assert_eq!(sql.revisions(&scope(), 0, 256).unwrap(), history);
    kernel.stop_all().await.unwrap();
}

fn archived_batch_bytes(path: &Path) -> Vec<Vec<u8>> {
    let db = rusqlite::Connection::open(path).unwrap();
    db.prepare("SELECT batch_json FROM conversation_batches ORDER BY sequence")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[tokio::test]
async fn invalid_archive_baselines_are_rejected_without_rewriting_evidence() {
    for corruption in [
        "text",
        "counts",
        "old_version",
        "unbound",
        "foreign_scope",
        "before_reset",
        "reset_latest",
        "current_mismatch",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("training.sqlite3");
        let sql = Arc::new(SqliteExpressions::open(&path).unwrap());
        let resident = large_lesson("resident", "resident");
        let latest = large_lesson("archived-target", "latest-local-source");
        let mut old = latest.clone();
        old.response = "older-local-source".into();
        let state = seeded_state(
            &[resident],
            &[
                historical(old.clone(), "updated"),
                historical(latest.clone(), "capacity"),
            ],
        );
        let other_scope = ContextScope {
            session_id: "other-chat".into(),
            user_id: "other-human".into(),
        };
        if corruption == "foreign_scope" {
            let plugin = PluginId::new(TRAINING_PLUGIN_ID).unwrap();
            let mut legacy: serde_json::Value =
                serde_json::from_slice(&state.get(&plugin, "conversation.v1").unwrap().unwrap())
                    .unwrap();
            let mut foreign = legacy["scopes"][0].clone();
            foreign["scope"] = serde_json::to_value(&other_scope).unwrap();
            foreign["lessons"] = serde_json::json!([]);
            let mut foreign_old = old.clone();
            foreign_old.response = "older-foreign-source".into();
            let mut foreign_latest = latest.clone();
            foreign_latest.response = "latest-foreign-source".into();
            foreign["history"] = serde_json::to_value([
                historical(foreign_old, "updated"),
                historical(foreign_latest, "capacity"),
            ])
            .unwrap();
            legacy["scopes"].as_array_mut().unwrap().push(foreign);
            state
                .set(
                    &plugin,
                    "conversation.v1".into(),
                    serde_json::to_vec(&legacy).unwrap(),
                )
                .unwrap();
        }
        let (kernel, training) = open(state.clone(), sql.clone()).await;
        observe(training.as_ref(), 0, 4);
        let batch = training.take_conversation_batch(100_000).unwrap().unwrap();
        assert!(batch.lessons.contains(&latest));
        training.fail_conversation_batch(&batch, 100_001).unwrap();
        kernel.stop_all().await.unwrap();
        let mut saved = checkpoint(sql.as_ref());
        let index = saved["inflight"]["batch"]["lessons"]
            .as_array()
            .unwrap()
            .iter()
            .position(|lesson| lesson["id"] == latest.id)
            .unwrap();
        match corruption {
            "text" => {
                saved["inflight"]["batch"]["lessons"][index]["response"] = "forged-source".into()
            }
            "counts" => saved["inflight"]["batch"]["lessons"][index]["observations"] = 99.into(),
            "old_version" => {
                saved["inflight"]["batch"]["lessons"][index] = serde_json::to_value(&old).unwrap()
            }
            "unbound" => saved["inflight"]["batch"]["lesson_history"] = serde_json::json!([]),
            "foreign_scope" => {
                let foreign = sql.revisions(&other_scope, 0, 4).unwrap();
                saved["inflight"]["batch"]["lesson_history"] =
                    serde_json::to_value(&foreign).unwrap();
                saved["inflight"]["batch"]["lessons"][index] =
                    serde_json::to_value(&foreign.last().unwrap().lesson).unwrap();
            }
            "before_reset" => {
                let newest = sql
                    .revisions(&scope(), 0, 256)
                    .unwrap()
                    .last()
                    .unwrap()
                    .sequence;
                saved["history_from"] = newest.into();
                saved["history_cursor"] = newest.into();
            }
            "reset_latest" => {
                let row = sql.scope(&scope()).unwrap().unwrap();
                assert!(
                    sql.commit(
                        &scope(),
                        &ConversationCommit {
                            expected_revision: row.revision,
                            checkpoint: row.checkpoint.unwrap(),
                            revisions: vec![historical(latest.clone(), "reset")],
                            batch: None,
                        }
                    )
                    .unwrap()
                );
            }
            "current_mismatch" => {
                let resident_index = saved["inflight"]["batch"]["lessons"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .position(|lesson| lesson["id"] == "resident")
                    .unwrap();
                saved["inflight"]["batch"]["lessons"][resident_index]["response"] =
                    "forged-current".into();
            }
            _ => unreachable!(),
        }
        replace_checkpoint(sql.as_ref(), &saved);
        let bytes = sql.scope(&scope()).unwrap().unwrap().checkpoint.unwrap();
        let revisions = sql.revisions(&scope(), 0, 256).unwrap();
        let outcomes = batch_outcomes(&path);
        let archives = archived_batch_bytes(&path);
        let kernel = Kernel::with_services(KernelServices {
            state,
            ..Default::default()
        });
        kernel
            .register(Box::new(
                TrainingPlugin::new(true)
                    .unwrap()
                    .with_conversation_store(sql.clone()),
            ))
            .unwrap();
        assert!(
            kernel
                .start(&PluginId::new(TRAINING_PLUGIN_ID).unwrap())
                .await
                .is_err(),
            "{corruption}"
        );
        assert_eq!(
            sql.scope(&scope()).unwrap().unwrap().checkpoint.unwrap(),
            bytes,
            "{corruption}"
        );
        assert_eq!(
            sql.revisions(&scope(), 0, 256).unwrap(),
            revisions,
            "{corruption}"
        );
        assert_eq!(batch_outcomes(&path), outcomes, "{corruption}");
        assert_eq!(archived_batch_bytes(&path), archives, "{corruption}");
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn legacy_reset_history_is_skipped_without_restoring_its_lesson() {
    let dir = tempfile::tempdir().unwrap();
    let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let lesson = large_lesson("legacy-reset-target", "previously-current");
    let mut updated = historical(lesson.clone(), "updated");
    updated.sequence = 1;
    let reset = ConversationLessonRevision {
        sequence: 2,
        lesson: lesson.clone(),
        reason: "reset".into(),
        batch_id: None,
        recorded_at_ms: None,
    };
    // Legacy reset drained current lessons and appended a reset revision. It had no
    // history_from field; migration therefore legitimately starts that boundary at zero.
    let state = seeded_state(&[], &[updated, reset]);
    let plugin = PluginId::new(TRAINING_PLUGIN_ID).unwrap();
    let mut legacy: serde_json::Value =
        serde_json::from_slice(&state.get(&plugin, "conversation.v1").unwrap().unwrap()).unwrap();
    legacy["scopes"][0]["generation"] = 2.into();
    let legacy_bytes = serde_json::to_vec(&legacy).unwrap();
    state
        .set(&plugin, "conversation.v1".into(), legacy_bytes.clone())
        .unwrap();
    let (kernel, training) = open(state.clone(), sql.clone()).await;
    assert_eq!(checkpoint(sql.as_ref())["history_from"], 0);
    let history = sql.revisions(&scope(), 0, 256).unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(
        sql.latest_revision(&scope(), &lesson.id)
            .unwrap()
            .unwrap()
            .reason,
        "reset"
    );
    observe(training.as_ref(), 0, 4);
    let batch = training.take_conversation_batch(100_000).unwrap().unwrap();
    assert!(batch.lessons.is_empty());
    assert!(batch.lesson_history.is_empty());
    assert_eq!(
        checkpoint(sql.as_ref())["history_cursor"],
        history[1].sequence
    );
    training
        .finish_conversation_batch(&batch, vec![], 100_001)
        .unwrap();
    assert!(training.conversation_lessons(&scope()).unwrap().is_empty());
    assert_eq!(sql.revisions(&scope(), 0, 256).unwrap(), history);
    assert_eq!(
        state.get(&plugin, "conversation.v1").unwrap().unwrap(),
        legacy_bytes
    );
    kernel.stop_all().await.unwrap();
}
