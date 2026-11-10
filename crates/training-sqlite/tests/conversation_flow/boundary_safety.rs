use super::boundary::{locate, pair};
use super::*;

fn maintenance(training: &dyn TrainingService, now: u64) -> ConversationBatch {
    training
        .take_conversation_maintenance_batch(now, 4096)
        .unwrap()
        .unwrap()
}

fn with_role(index: usize, role: ConversationRole) -> ConversationMessage {
    let mut message = event(index);
    message.role = role;
    message.speaker = if role == ConversationRole::Human {
        "human"
    } else {
        "eve"
    }
    .into();
    message
}

fn reject_pair(
    training: &dyn TrainingService,
    sql: &SqliteExpressions,
    batch: &ConversationBatch,
    eve: &ConversationMessage,
    human: &ConversationMessage,
    now: u64,
) {
    let previous = sql.scope(&batch.scope).unwrap();
    let revisions = sql.revisions(&batch.scope, 0, 256).unwrap();
    let lessons = training.conversation_lessons(&batch.scope).unwrap();
    let invalid = ConversationLessonDraft {
        lesson_id: None,
        situation: "对方纠正刚才的表达时".into(),
        response: "根据真实对应的反馈调整表达".into(),
        avoid: "不借用其他发言来伪造反馈".into(),
        assessment: ConversationAssessment::Correction,
        evidence: [eve, human]
            .map(|message| ConversationEvidence {
                message_id: message.id.clone(),
                quote: message.text.lines().next().unwrap().into(),
            })
            .to_vec(),
    };
    assert!(
        training
            .finish_conversation_batch(batch, vec![invalid], now)
            .is_err()
    );
    assert_eq!(sql.scope(&batch.scope).unwrap(), previous);
    assert_eq!(sql.revisions(&batch.scope, 0, 256).unwrap(), revisions);
    assert_eq!(
        training.conversation_lessons(&batch.scope).unwrap(),
        lessons
    );
}

#[tokio::test]
async fn failed_or_unknown_sweep_prefix_does_not_block_a_later_successful_suffix() {
    for interrupted in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
        let state = Arc::new(MemoryStateStore::default());
        let (kernel, training) = open(state.clone(), sql.clone()).await;
        observe(training.as_ref(), 0, 384);
        training
            .request_conversation_sweep("suffix-recovery", &scope(), &(1..=384).collect::<Vec<_>>())
            .unwrap();
        let first = maintenance(training.as_ref(), 100_000);
        assert_eq!(first.messages.len(), 128);
        let (kernel, training, now) = if interrupted {
            kernel.stop_all().await.unwrap();
            let (kernel, training) = open(state, sql.clone()).await;
            (kernel, training, 300_000)
        } else {
            let mut now = 100_001;
            fail_sweep_batch(training.as_ref(), first.clone(), &mut now);
            (kernel, training, now + 200_000)
        };
        assert!(checkpoint(sql.as_ref())["sweep"]["successful_context_from"].is_null());
        let next = maintenance(training.as_ref(), now);
        assert_eq!(next.messages.len(), 128);
        assert!(
            next.messages
                .iter()
                .all(|message| { !first.messages.iter().any(|old| old.id == message.id) })
        );
        assert!(!pair(&next, 127, 128));
        training
            .finish_conversation_batch(&next, vec![], now + 1)
            .unwrap();
        let third = maintenance(training.as_ref(), now + 200_000);
        assert!(pair(&third, 255, 256));
        assert!(!pair(&third, 127, 256));
        let correction = draft(
            &third,
            [locate(&third, 255), locate(&third, 256)],
            None,
            ConversationAssessment::Correction,
            "只根据后来成功保留的真实反馈调整表达",
        );
        assert_eq!(
            training
                .finish_conversation_batch(&third, vec![correction], now + 200_001)
                .unwrap(),
            1
        );
        assert!(training.conversation_lessons(&scope()).unwrap()[0].active);
        let last = maintenance(training.as_ref(), now + 400_000);
        training
            .finish_conversation_batch(&last, vec![], now + 400_001)
            .unwrap();
        let status = training
            .conversation_sweep_status("suffix-recovery")
            .unwrap();
        assert_eq!(
            (status.total, status.processed, status.remaining),
            (384, 256, 0)
        );
        assert_eq!(
            sweep_counts(&status).failed,
            if interrupted { 0 } else { 128 }
        );
        assert_eq!(
            sweep_counts(&status).unknown,
            if interrupted { 128 } else { 0 }
        );
        assert!(status.completed);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn successful_final_fragment_does_not_certify_a_failed_or_unknown_original() {
    for first_outcome in ["completed", "failed", "interrupted"] {
        let dir = tempfile::tempdir().unwrap();
        let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
        let state = Arc::new(MemoryStateStore::default());
        let (kernel, training) = open(state.clone(), sql.clone()).await;
        let mut eve = event(1);
        eve.text = "\\".repeat(MAX_CONVERSATION_BODY_BYTES);
        eve.metadata.as_mut().unwrap().context = Some("\n".repeat(MAX_CONVERSATION_CONTEXT_BYTES));
        training.observe_conversation(&scope(), eve).unwrap();
        training
            .request_conversation_sweep("partial-context", &scope(), &[1])
            .unwrap();
        let first = maintenance(training.as_ref(), 100_000);
        assert!(
            first.messages[0]
                .metadata
                .as_ref()
                .unwrap()
                .fragment
                .is_some()
        );
        let mut now = 100_001;
        match first_outcome {
            "completed" => {
                training
                    .finish_conversation_batch(&first, vec![], now)
                    .unwrap();
            }
            "failed" => fail_sweep_batch(training.as_ref(), first, &mut now),
            "interrupted" => {}
            _ => unreachable!(),
        }
        kernel.stop_all().await.unwrap();
        let (kernel, training) = open(state, sql.clone()).await;
        let partial = training
            .conversation_sweep_status("partial-context")
            .unwrap();
        assert_eq!(partial.processed, 0);
        assert_eq!(sweep_counts(&partial).partial, 1);
        assert!(checkpoint(sql.as_ref())["sweep"]["successful_context_from"].is_null());
        now += 200_000;
        while let Some(tail) = training
            .take_conversation_maintenance_batch(now, 4096)
            .unwrap()
        {
            training
                .finish_conversation_batch(&tail, vec![], now + 1)
                .unwrap();
            now += 200_000;
        }
        let status = training
            .conversation_sweep_status("partial-context")
            .unwrap();
        assert!(status.completed);
        assert_eq!(status.processed, u64::from(first_outcome == "completed"));
        assert_eq!(
            sweep_counts(&status).failed,
            u64::from(first_outcome == "failed")
        );
        assert_eq!(
            sweep_counts(&status).unknown,
            u64::from(first_outcome == "interrupted")
        );
        assert_eq!(
            checkpoint(sql.as_ref())["sweep"]["successful_context_from"].as_u64(),
            (first_outcome == "completed").then_some(1)
        );
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn sweep_background_follows_frozen_ordinals_without_importing_an_unselected_eve() {
    for reversed_time in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
        let (kernel, training) = open(Arc::new(MemoryStateStore::default()), sql.clone()).await;
        for index in 0..132 {
            let mut message = event(index);
            if index == 1 && reversed_time {
                message.metadata.as_mut().unwrap().observed_at_ms = Some(2_000);
            }
            training.observe_conversation(&scope(), message).unwrap();
        }
        // 第 128 个来源是原日志序号 2；原日志中更近的 m-129 不属于此次重访。
        let sources: Vec<_> = (3..=129).chain([2, 131]).collect();
        training
            .request_conversation_sweep("frozen-order", &scope(), &sources)
            .unwrap();
        let first = maintenance(training.as_ref(), 100_000);
        assert_eq!(first.messages.len(), 128);
        assert_eq!(first.messages.last().unwrap().id, "m-1");
        training
            .finish_conversation_batch(&first, vec![], 100_001)
            .unwrap();
        let second = maintenance(training.as_ref(), 300_000);
        assert_eq!(
            second
                .messages
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["m-1", "m-130"]
        );
        assert_eq!(pair(&second, 1, 130), !reversed_time);
        assert!(!pair(&second, 129, 130));
        if reversed_time {
            reject_pair(
                training.as_ref(),
                sql.as_ref(),
                &second,
                &event(1),
                &event(130),
                300_001,
            );
            training
                .finish_conversation_batch(&second, vec![], 300_002)
                .unwrap();
        } else {
            let correction = draft(
                &second,
                [locate(&second, 1), locate(&second, 130)],
                None,
                ConversationAssessment::Correction,
                "按冻结来源的可信反馈调整表达",
            );
            assert_eq!(
                training
                    .finish_conversation_batch(&second, vec![correction], 300_001)
                    .unwrap(),
                1
            );
        }
        let status = training.conversation_sweep_status("frozen-order").unwrap();
        assert_eq!(status.processed, 129);
        assert_eq!(sql.events(&scope(), 1, 256).unwrap().len(), 132);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn an_uncertain_nearest_eve_cannot_be_skipped_to_create_a_boundary_correction() {
    for revisit in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
        let (kernel, training) = open(Arc::new(MemoryStateStore::default()), sql.clone()).await;
        let limit = if revisit { 128 } else { 16 };
        for index in 0..2 * limit {
            let mut message = event(index);
            if index == limit - 1 {
                message.metadata.as_mut().unwrap().ordering_uncertain = true;
            }
            training.observe_conversation(&scope(), message).unwrap();
        }
        if revisit {
            training
                .request_conversation_sweep(
                    "uncertain-nearest",
                    &scope(),
                    &(1..=2 * limit as u64).collect::<Vec<_>>(),
                )
                .unwrap();
        }
        let take = |now| {
            if revisit {
                maintenance(training.as_ref(), now)
            } else {
                training.take_conversation_batch(now).unwrap().unwrap()
            }
        };
        let first = take(100_000);
        training
            .finish_conversation_batch(&first, vec![], 100_001)
            .unwrap();
        let second = take(300_000);
        assert!(!pair(&second, limit - 1, limit));
        assert!(!pair(&second, limit - 3, limit));
        reject_pair(
            training.as_ref(),
            sql.as_ref(),
            &second,
            &event(limit - 1),
            &event(limit),
            300_001,
        );
        reject_pair(
            training.as_ref(),
            sql.as_ref(),
            &second,
            &event(limit - 3),
            &event(limit),
            300_002,
        );
        training
            .finish_conversation_batch(&second, vec![], 300_003)
            .unwrap();
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn normal_background_stops_at_reset_reservation_and_scope_boundaries() {
    for barrier in ["reset", "reserved", "session", "user"] {
        let dir = tempfile::tempdir().unwrap();
        let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
        let (kernel, training) = open(Arc::new(MemoryStateStore::default()), sql.clone()).await;
        observe(training.as_ref(), 0, 16);
        let first = training.take_conversation_batch(100_000).unwrap().unwrap();
        training
            .finish_conversation_batch(&first, vec![], 100_001)
            .unwrap();
        let mut current = scope();
        let start = match barrier {
            "reset" => {
                training.reset_expression(&current).unwrap();
                16
            }
            "reserved" => {
                training
                    .observe_conversation(&current, with_role(16, ConversationRole::Eve))
                    .unwrap();
                training
                    .request_conversation_sweep("reserved-nearest", &current, &[17])
                    .unwrap();
                17
            }
            "session" => {
                current.session_id = "other-chat".into();
                16
            }
            "user" => {
                current.user_id = "another-human".into();
                16
            }
            _ => unreachable!(),
        };
        for index in start..start + 16 {
            training
                .observe_conversation(&current, with_role(index, ConversationRole::Human))
                .unwrap();
        }
        let reserved_before = (barrier == "reserved").then(|| {
            training
                .conversation_sweep_status("reserved-nearest")
                .unwrap()
        });
        let second = training.take_conversation_batch(300_000).unwrap().unwrap();
        assert_eq!(second.scope, current);
        assert!(
            second
                .messages
                .iter()
                .all(|m| m.role == ConversationRole::Human)
        );
        assert!(!pair(&second, 15, start));
        reject_pair(
            training.as_ref(),
            sql.as_ref(),
            &second,
            &event(15),
            &with_role(start, ConversationRole::Human),
            300_001,
        );
        training
            .finish_conversation_batch(&second, vec![], 300_002)
            .unwrap();
        if let Some(before) = reserved_before {
            assert_eq!(
                training
                    .conversation_sweep_status("reserved-nearest")
                    .unwrap(),
                before
            );
        }
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn missing_legacy_success_marker_is_not_reconstructed_from_covered_or_counts() {
    let dir = tempfile::tempdir().unwrap();
    let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, training) = open(state.clone(), sql.clone()).await;
    observe(training.as_ref(), 0, 256);
    training
        .request_conversation_sweep("legacy-marker", &scope(), &(1..=256).collect::<Vec<_>>())
        .unwrap();
    let first = maintenance(training.as_ref(), 100_000);
    training
        .finish_conversation_batch(&first, vec![], 100_001)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let mut old = checkpoint(sql.as_ref());
    old["sweep"]
        .as_object_mut()
        .unwrap()
        .remove("successful_context_from");
    replace_checkpoint(sql.as_ref(), &old);
    let bytes = sql.scope(&scope()).unwrap().unwrap().checkpoint.unwrap();
    let (kernel, training) = open(state, sql.clone()).await;
    assert_eq!(
        training
            .conversation_sweep_status("legacy-marker")
            .unwrap()
            .processed,
        128
    );
    assert_eq!(
        sql.scope(&scope()).unwrap().unwrap().checkpoint.unwrap(),
        bytes
    );
    let second = maintenance(training.as_ref(), 300_000);
    assert_eq!(second.messages.len(), 128);
    assert!(!pair(&second, 127, 128));
    assert!(
        second
            .messages
            .iter()
            .all(|message| !first.messages.iter().any(|old| old.id == message.id))
    );
    training
        .finish_conversation_batch(&second, vec![], 300_001)
        .unwrap();
    assert!(
        checkpoint(sql.as_ref())["sweep"]["successful_context_from"]
            .as_u64()
            .unwrap()
            >= 129
    );
    assert_eq!(
        training
            .conversation_sweep_status("legacy-marker")
            .unwrap()
            .processed,
        256
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn restart_rejects_a_forged_or_non_nearest_persisted_sweep_anchor() {
    for corruption in ["wrong_eve", "metadata", "marker"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("training.sqlite3");
        let sql = Arc::new(SqliteExpressions::open(&path).unwrap());
        let state = Arc::new(MemoryStateStore::default());
        let (kernel, training) = open(state.clone(), sql.clone()).await;
        observe(training.as_ref(), 0, 256);
        training
            .request_conversation_sweep(
                "anchor-integrity",
                &scope(),
                &(1..=256).collect::<Vec<_>>(),
            )
            .unwrap();
        let first = maintenance(training.as_ref(), 100_000);
        training
            .finish_conversation_batch(&first, vec![], 100_001)
            .unwrap();
        let second = maintenance(training.as_ref(), 300_000);
        assert!(pair(&second, 127, 128));
        kernel.stop_all().await.unwrap();
        let mut value = checkpoint(sql.as_ref());
        match corruption {
            "wrong_eve" => {
                let other = sql.events(&scope(), 126, 1).unwrap().remove(0);
                value["inflight"]["batch"]["messages"][0] =
                    serde_json::to_value(other.message).unwrap();
            }
            "metadata" => {
                value["inflight"]["batch"]["messages"][0]["metadata"]["observed_at_ms"] =
                    999_999.into()
            }
            "marker" => value["sweep"]["successful_context_from"] = 0.into(),
            _ => unreachable!(),
        }
        replace_checkpoint(sql.as_ref(), &value);
        let bytes = sql.scope(&scope()).unwrap().unwrap().checkpoint.unwrap();
        let outcomes = batch_outcomes(&path);
        let services = KernelServices {
            state,
            ..Default::default()
        };
        let kernel = Kernel::with_services(services);
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
            bytes
        );
        assert_eq!(batch_outcomes(&path), outcomes);
        kernel.stop_all().await.unwrap();
    }
}
