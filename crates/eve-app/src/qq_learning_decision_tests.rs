//! 在真实 Memory 插件上检查 QQ 自主学习的决策、来源和人工覆盖边界。
use super::tests::{FixedLearning, RecordingStore, memory, run, run_memory, session, snapshot};
use super::*;
use eve_learning_api::{
    CandidateDraft, DecisionReason, LearningBatch, LearningDecisionAction, LearningDecisionRecord,
    LearningJob, LearningOptions, LearningOutcome, LearningResult,
};
use eve_learning_plugin::EvidenceConfirmationPolicy;
use eve_llm_api::{ChatMessage, ChatRole};
use eve_memory_api::{CompletedInteraction, InteractionEvidence};
use eve_memory_plugin::MemoryController;
use eve_session_api::{SessionKey, SessionSnapshot, SessionTurn, SessionTurnStatus};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

fn import_pair(
    admin: &MemoryController,
    session: &SessionKey,
    user_text: &str,
) -> Vec<InteractionEvidence> {
    let scope = crate::qq_memory::scope(session);
    let current = snapshot(admin, session);
    let mut turns: Vec<_> = current
        .evidence
        .iter()
        .filter_map(|source| match &source.source {
            EvidenceSource::CompletedInteraction {
                turn_id,
                user_text,
                assistant_text,
                ..
            } => Some(SessionTurn {
                id: *turn_id,
                input: user_text.clone(),
                status: SessionTurnStatus::Completed {
                    messages: vec![
                        ChatMessage::text(ChatRole::User, user_text),
                        ChatMessage::text(ChatRole::Assistant, assistant_text),
                    ],
                },
            }),
            EvidenceSource::UserStatement { .. } => None,
        })
        .collect();
    turns.sort_by_key(|turn| turn.id);
    let mut result = Vec::new();
    for _ in 0..2 {
        let number = turns.len() as u64 + 1;
        let id = format!("decision-evidence-{number}");
        turns.push(SessionTurn {
            id: number,
            input: user_text.into(),
            status: SessionTurnStatus::Completed {
                messages: vec![
                    ChatMessage::text(ChatRole::User, user_text),
                    ChatMessage::text(ChatRole::Assistant, "收到，按当前请求回复。"),
                ],
            },
        });
        let saved = admin
            .import_completed(
                &scope,
                snapshot(admin, session).revision,
                CompletedInteraction {
                    evidence_id: id.clone(),
                    message_id: format!("decision-source-message-{number}"),
                    at_ms: number * 10,
                    snapshot: SessionSnapshot {
                        key: session.clone(),
                        revision: number * 2,
                        turns: turns.clone(),
                    },
                    turn_id: number,
                },
            )
            .unwrap();
        result.push(
            saved
                .evidence
                .iter()
                .find(|source| source.id == id)
                .unwrap()
                .clone(),
        );
    }
    result
}

fn add_candidate(
    learning: &FixedLearning,
    scope: &MemoryScope,
    id: &str,
    text: &str,
    evidence: Vec<InteractionEvidence>,
) -> PreferenceCandidate {
    let batch_id = format!("decision-batch-{id}");
    let candidate = PreferenceCandidate {
        id: id.into(),
        batch_id: batch_id.clone(),
        draft: CandidateDraft {
            text: text.into(),
            confidence: 90,
            evidence_ids: evidence.iter().map(|source| source.id.clone()).collect(),
        },
        created_at_ms: evidence.iter().map(|source| source.at_ms).max().unwrap() + 2,
        expires_at_ms: u64::MAX,
    };
    learning
        .snapshots
        .lock()
        .unwrap()
        .entry(scope.clone())
        .or_insert_with(|| LearningSnapshot {
            scope: scope.clone(),
            jobs: vec![],
        })
        .jobs
        .push(LearningJob {
            batch: LearningBatch {
                id: batch_id,
                scope: scope.clone(),
                extractor_version: "decision-component-fixture-v1".into(),
                started_at_ms: candidate.created_at_ms - 1,
                evidence,
            },
            status: JobStatus::Completed,
            finished_at_ms: Some(candidate.created_at_ms),
            candidates: vec![candidate.clone()],
        });
    candidate
}

fn decision_for(learning: &FixedLearning, scope: &MemoryScope, id: &str) -> LearningDecisionRecord {
    learning
        .decisions(scope)
        .unwrap()
        .into_iter()
        .rev()
        .find(|record| record.decision.candidate_id == id)
        .unwrap()
}

#[tokio::test]
async fn normalized_duplicate_preserves_memory_and_replays_decision_without_writes() {
    let store = Arc::new(RecordingStore::default());
    let (kernel, admin) = memory(store.clone()).await;
    let session = session();
    let scope = crate::qq_memory::scope(&session);
    let learning = Arc::new(FixedLearning::default());
    let first = add_candidate(
        &learning,
        &scope,
        "first",
        "每次回复请先说结论",
        import_pair(&admin, &session, "每次回复请先说结论"),
    );
    let before = snapshot(&admin, &session);
    auto_confirm(
        &admin,
        learning.as_ref(),
        &EvidenceConfirmationPolicy,
        &scope,
        1000,
    )
    .unwrap();
    let confirmed = snapshot(&admin, &session);
    assert_eq!(confirmed.revision, before.revision + 1);
    assert_eq!(confirmed.preferences.len(), 1);
    assert_eq!(confirmed.preferences[0].id, preference_id(&first.id));
    assert_eq!(confirmed.preferences[0].history.len(), 1);
    assert_eq!(
        confirmed.evidence, before.evidence,
        "自主确认不能伪造用户命令"
    );
    let record = decision_for(&learning, &scope, "first");
    assert_eq!(record.decision.action, LearningDecisionAction::Confirm);
    assert_eq!(record.decision.reason, DecisionReason::Eligible);
    assert_eq!(record.decision.memory_revision, before.revision);

    let duplicate = add_candidate(
        &learning,
        &scope,
        "duplicate",
        "  每次回复请先说结论。  ",
        import_pair(&admin, &session, "还是请先说结论。"),
    );
    let before_duplicate = snapshot(&admin, &session);
    let memory_writes = store.writes.load(Ordering::SeqCst);
    auto_confirm(
        &admin,
        learning.as_ref(),
        &EvidenceConfirmationPolicy,
        &scope,
        1001,
    )
    .unwrap();
    assert_eq!(snapshot(&admin, &session), before_duplicate);
    assert_eq!(store.writes.load(Ordering::SeqCst), memory_writes);
    let duplicate_record = decision_for(&learning, &scope, &duplicate.id);
    assert_eq!(
        duplicate_record.decision.action,
        LearningDecisionAction::Reject
    );
    assert_eq!(duplicate_record.decision.reason, DecisionReason::Duplicate);
    let records = learning.decisions(&scope).unwrap();
    let decision_writes = learning.record_writes.load(Ordering::SeqCst);
    auto_confirm(
        &admin,
        learning.as_ref(),
        &EvidenceConfirmationPolicy,
        &scope,
        1002,
    )
    .unwrap();
    assert_eq!(learning.decisions(&scope).unwrap(), records);
    assert_eq!(
        learning.record_writes.load(Ordering::SeqCst),
        decision_writes
    );
    assert_eq!(store.writes.load(Ordering::SeqCst), memory_writes);
    kernel.stop_all().await.unwrap();

    let (kernel, reopened) = memory(store.clone()).await;
    auto_confirm(
        &reopened,
        learning.as_ref(),
        &EvidenceConfirmationPolicy,
        &scope,
        1003,
    )
    .unwrap();
    assert_eq!(snapshot(&reopened, &session), before_duplicate);
    assert_eq!(learning.decisions(&scope).unwrap(), records);
    assert_eq!(
        learning.record_writes.load(Ordering::SeqCst),
        decision_writes
    );
    assert_eq!(store.writes.load(Ordering::SeqCst), memory_writes);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn explicit_newer_setting_updates_original_preference_and_preserves_history() {
    let store = Arc::new(RecordingStore::default());
    let (kernel, admin) = memory(store.clone()).await;
    let session = session();
    let scope = crate::qq_memory::scope(&session);
    let learning = Arc::new(FixedLearning::default());
    let first = add_candidate(
        &learning,
        &scope,
        "two-segments",
        "回复最多分成2段",
        import_pair(&admin, &session, "回复最多分成2段"),
    );
    auto_confirm(
        &admin,
        learning.as_ref(),
        &EvidenceConfirmationPolicy,
        &scope,
        1000,
    )
    .unwrap();
    let first_saved = snapshot(&admin, &session).preferences[0].clone();
    let updated = add_candidate(
        &learning,
        &scope,
        "three-segments",
        "回复最多分成3段",
        import_pair(&admin, &session, "现在回复最多分成3段"),
    );
    let before = snapshot(&admin, &session);
    auto_confirm(
        &admin,
        learning.as_ref(),
        &EvidenceConfirmationPolicy,
        &scope,
        1001,
    )
    .unwrap();
    let saved = snapshot(&admin, &session);
    assert_eq!(saved.revision, before.revision + 1);
    assert_eq!(saved.evidence, before.evidence);
    assert_eq!(saved.preferences.len(), 1);
    let preference = &saved.preferences[0];
    assert_eq!(preference.id, preference_id(&first.id));
    assert_eq!(preference.revision, 2);
    assert_eq!(preference.text, updated.draft.text);
    assert_eq!(preference.history.len(), 2);
    assert_eq!(preference.history[0], first_saved.history[0]);
    assert_eq!(
        preference.history[1].evidence_id,
        updated.draft.evidence_ids[1]
    );
    let record = decision_for(&learning, &scope, &updated.id);
    assert_eq!(record.decision.memory_revision, before.revision);
    assert_eq!(
        record.decision.reason,
        DecisionReason::ExplicitRevisionUpdate
    );
    assert_eq!(
        record.decision.action,
        LearningDecisionAction::Update {
            preference_id: preference.id.clone(),
            expected_revision: 1,
        }
    );

    let memory_writes = store.writes.load(Ordering::SeqCst);
    let records = learning.decisions(&scope).unwrap();
    auto_confirm(
        &admin,
        learning.as_ref(),
        &EvidenceConfirmationPolicy,
        &scope,
        1002,
    )
    .unwrap();
    let commands = Commands::autonomous(learning.clone(), Arc::new(admin.clone()));
    let reply = run(
        &commands,
        &session,
        "accept-updated",
        "/accept-memory three-segments",
    )
    .unwrap()
    .unwrap();
    assert!(reply.contains(&preference.id));
    assert_eq!(
        snapshot(&admin, &session),
        saved,
        "映射到原偏好后不得再新增同文记忆"
    );
    assert_eq!(learning.decisions(&scope).unwrap(), records);
    assert_eq!(store.writes.load(Ordering::SeqCst), memory_writes);
    let listing = run(&commands, &session, "list", "/memory-candidates")
        .unwrap()
        .unwrap();
    assert!(listing.contains("two-segments") && listing.contains("three-segments"));
    assert!(listing.contains(&preference.id));
    kernel.stop_all().await.unwrap();
}

struct AllowEverything {
    calls: AtomicUsize,
}
impl AutoConfirmationPolicy for AllowEverything {
    fn allows(
        &self,
        _: &PreferenceCandidate,
        _: &LearningBatch,
        _: &MemorySnapshot,
        _: u64,
    ) -> LearningResult<bool> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }
}

#[tokio::test]
async fn legacy_allow_policy_cannot_override_manual_correction_or_revocation() {
    for revoke in [false, true] {
        let store = Arc::new(RecordingStore::default());
        let (kernel, admin) = memory(store.clone()).await;
        let session = session();
        let scope = crate::qq_memory::scope(&session);
        let learning = Arc::new(FixedLearning::default());
        add_candidate(
            &learning,
            &scope,
            "original",
            "回复最多分成2段",
            import_pair(&admin, &session, "回复最多分成2段"),
        );
        auto_confirm(
            &admin,
            learning.as_ref(),
            &EvidenceConfirmationPolicy,
            &scope,
            1000,
        )
        .unwrap();
        let ordinary = crate::qq_memory::Commands::new(Arc::new(admin.clone()));
        run_memory(
            &ordinary,
            &session,
            "manual-choice",
            if revoke {
                "/forget learned-original"
            } else {
                "/correct-memory learned-original 回复最多分成3段"
            },
        );
        add_candidate(
            &learning,
            &scope,
            "later",
            "回复最多分成4段",
            import_pair(&admin, &session, "回复最多分成4段"),
        );
        let before = snapshot(&admin, &session);
        let writes = store.writes.load(Ordering::SeqCst);
        let policy = AllowEverything {
            calls: AtomicUsize::new(0),
        };
        auto_confirm(&admin, learning.as_ref(), &policy, &scope, 1001).unwrap();
        assert_eq!(policy.calls.load(Ordering::SeqCst), 1);
        assert_eq!(snapshot(&admin, &session), before);
        assert_eq!(store.writes.load(Ordering::SeqCst), writes);
        let record = decision_for(&learning, &scope, "later");
        assert_eq!(record.decision.action, LearningDecisionAction::Defer);
        assert_eq!(
            record.decision.reason,
            if revoke {
                DecisionReason::RevokedConflict
            } else {
                DecisionReason::ManualConflict
            }
        );
        assert_eq!(record.decision.policy_version, policy.version());
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn decision_command_shows_audit_sources_and_update_target_only_in_bound_scope() {
    let store = Arc::new(RecordingStore::default());
    let (kernel, admin) = memory(store.clone()).await;
    let owner = session();
    let scope = crate::qq_memory::scope(&owner);
    let learning = Arc::new(FixedLearning::default());
    add_candidate(
        &learning,
        &scope,
        "source-setting",
        "回复最多分成2段",
        import_pair(&admin, &owner, "回复最多分成2段"),
    );
    auto_confirm(
        &admin,
        learning.as_ref(),
        &EvidenceConfirmationPolicy,
        &scope,
        1000,
    )
    .unwrap();
    let updated = add_candidate(
        &learning,
        &scope,
        "new-setting",
        "回复最多分成3段",
        import_pair(&admin, &owner, "回复最多分成3段"),
    );
    auto_confirm(
        &admin,
        learning.as_ref(),
        &EvidenceConfirmationPolicy,
        &scope,
        1001,
    )
    .unwrap();
    let commands = Commands::autonomous(learning.clone(), Arc::new(admin.clone()));
    let saved = snapshot(&admin, &owner);
    let records = learning.decisions(&scope).unwrap();
    let memory_writes = store.writes.load(Ordering::SeqCst);
    let decision_writes = learning.record_writes.load(Ordering::SeqCst);
    let reply = run(&commands, &owner, "audit", "/memory-decision new-setting")
        .unwrap()
        .unwrap();
    assert!(reply.contains("new-setting"));
    assert!(reply.contains(EvidenceConfirmationPolicy.version()));
    assert!(reply.contains("learned-source-setting"));
    for evidence in &updated.draft.evidence_ids {
        assert!(reply.contains(evidence), "决策查询应保留真实交互来源 ID");
    }
    assert!(reply.contains("修订") || reply.contains("版本"));
    for foreign in [
        SessionKey::new("other-app", &owner.user_id).unwrap(),
        SessionKey::new("other-group", &owner.user_id).unwrap(),
        SessionKey::new(&owner.session_id, "other-user").unwrap(),
    ] {
        let reply = run(&commands, &foreign, "audit", "/memory-decision new-setting")
            .unwrap()
            .unwrap();
        assert!(reply.contains("没有这条候选"));
        assert!(!reply.contains("new-setting"));
        assert!(!reply.contains("learned-source-setting"));
        assert!(!reply.contains(EvidenceConfirmationPolicy.version()));
        assert!(
            learning
                .decisions(&crate::qq_memory::scope(&foreign))
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(snapshot(&admin, &owner), saved);
    assert_eq!(learning.decisions(&scope).unwrap(), records);
    assert_eq!(store.writes.load(Ordering::SeqCst), memory_writes);
    assert_eq!(
        learning.record_writes.load(Ordering::SeqCst),
        decision_writes
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn unacknowledged_decision_prevents_memory_change_and_can_retry_after_recovery() {
    let store = Arc::new(RecordingStore::default());
    let (kernel, admin) = memory(store.clone()).await;
    let session = session();
    let scope = crate::qq_memory::scope(&session);
    let learning = Arc::new(FixedLearning::default());
    add_candidate(
        &learning,
        &scope,
        "unacknowledged",
        "每次回复请先说结论",
        import_pair(&admin, &session, "每次回复请先说结论"),
    );
    let before = snapshot(&admin, &session);
    let writes = store.writes.load(Ordering::SeqCst);
    *learning.record_error.lock().unwrap() = Some(LearningError::Storage);
    assert!(matches!(
        auto_confirm(
            &admin,
            learning.as_ref(),
            &EvidenceConfirmationPolicy,
            &scope,
            1000
        ),
        Err(PluginError::State(_))
    ));
    assert_eq!(snapshot(&admin, &session), before);
    assert_eq!(store.writes.load(Ordering::SeqCst), writes);
    assert!(learning.decisions(&scope).unwrap().is_empty());
    *learning.record_error.lock().unwrap() = None;
    auto_confirm(
        &admin,
        learning.as_ref(),
        &EvidenceConfirmationPolicy,
        &scope,
        1001,
    )
    .unwrap();
    let saved = snapshot(&admin, &session);
    assert_eq!(saved.preferences.len(), 1);
    assert_eq!(saved.preferences[0].id, "learned-unacknowledged");
    assert_eq!(saved.revision, before.revision + 1);
    assert_eq!(store.writes.load(Ordering::SeqCst), writes + 1);
    assert_eq!(learning.decisions(&scope).unwrap().len(), 1);
    kernel.stop_all().await.unwrap();
}

type ReadHook = Mutex<Option<Box<dyn FnOnce() + Send>>>;

struct ReadHookLearning {
    inner: Arc<FixedLearning>,
    after_snapshot: ReadHook,
    after_decisions: ReadHook,
}

impl LearningAdmin for ReadHookLearning {
    fn snapshot(&self, scope: &MemoryScope) -> LearningResult<LearningSnapshot> {
        let captured = self.inner.snapshot(scope)?;
        if let Some(hook) = self.after_snapshot.lock().unwrap().take() {
            hook();
        }
        Ok(captured)
    }
    fn decisions(&self, scope: &MemoryScope) -> LearningResult<Vec<LearningDecisionRecord>> {
        let captured = self.inner.decisions(scope)?;
        if let Some(hook) = self.after_decisions.lock().unwrap().take() {
            hook();
        }
        Ok(captured)
    }
    fn record_decision(
        &self,
        scope: &MemoryScope,
        decision: eve_learning_api::LearningDecision,
        at_ms: u64,
    ) -> LearningResult<LearningDecisionRecord> {
        self.inner.record_decision(scope, decision, at_ms)
    }
    fn reserve(
        &self,
        memory: &MemorySnapshot,
        now_ms: u64,
        version: &str,
        options: &LearningOptions,
    ) -> LearningResult<Option<LearningBatch>> {
        self.inner.reserve(memory, now_ms, version, options)
    }
    fn finish(
        &self,
        batch: &LearningBatch,
        at_ms: u64,
        outcome: LearningOutcome,
    ) -> LearningResult<()> {
        self.inner.finish(batch, at_ms, outcome)
    }
}

#[tokio::test]
async fn concurrent_auto_update_after_decision_read_cannot_duplicate_manual_acceptance() {
    let store = Arc::new(RecordingStore::default());
    let (kernel, admin) = memory(store.clone()).await;
    let owner = session();
    let scope = crate::qq_memory::scope(&owner);
    let learning = Arc::new(FixedLearning::default());
    add_candidate(
        &learning,
        &scope,
        "first",
        "回复最多分成2段",
        import_pair(&admin, &owner, "回复最多分成2段"),
    );
    auto_confirm(
        &admin,
        learning.as_ref(),
        &EvidenceConfirmationPolicy,
        &scope,
        1000,
    )
    .unwrap();
    add_candidate(
        &learning,
        &scope,
        "later",
        "回复最多分成3段",
        import_pair(&admin, &owner, "回复最多分成3段"),
    );
    let before = snapshot(&admin, &owner);
    let writes = store.writes.load(Ordering::SeqCst);
    let hook_admin = admin.clone();
    let hook_learning = learning.clone();
    let hook_scope = scope.clone();
    let hooked = Arc::new(ReadHookLearning {
        inner: learning,
        after_snapshot: Mutex::new(None),
        after_decisions: Mutex::new(Some(Box::new(move || {
            // 精确落在“已读旧账本”和下一次宿主读取之间，无调度和定时器依赖。
            auto_confirm(
                &hook_admin,
                hook_learning.as_ref(),
                &EvidenceConfirmationPolicy,
                &hook_scope,
                1001,
            )
            .unwrap();
        }))),
    });
    let commands = Commands::autonomous(hooked, Arc::new(admin.clone()));
    let reply = run(&commands, &owner, "racing-accept", "/accept-memory later")
        .unwrap()
        .unwrap();
    assert!(reply.contains("刚刚发生变化") || reply.contains("后续修改"));
    let saved = snapshot(&admin, &owner);
    assert_eq!(saved.revision, before.revision + 1);
    assert_eq!(
        saved.preferences.len(),
        1,
        "旧账本不能掩盖已更新关联并再创建 learned-later"
    );
    assert_eq!(saved.preferences[0].id, "learned-first");
    assert_eq!(saved.preferences[0].text, "回复最多分成3段");
    assert_eq!(saved.preferences[0].revision, 2);
    assert_eq!(
        saved.evidence, before.evidence,
        "被 CAS 拒绝的命令不能留下用户证据"
    );
    assert_eq!(store.writes.load(Ordering::SeqCst), writes + 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn concurrent_new_candidate_after_jobs_read_keeps_read_only_command_consistent() {
    let store = Arc::new(RecordingStore::default());
    let (kernel, admin) = memory(store.clone()).await;
    let owner = session();
    let scope = crate::qq_memory::scope(&owner);
    let learning = Arc::new(FixedLearning::default());
    add_candidate(
        &learning,
        &scope,
        "initial",
        "回复最多分成2段",
        import_pair(&admin, &owner, "回复最多分成2段"),
    );
    auto_confirm(
        &admin,
        learning.as_ref(),
        &EvidenceConfirmationPolicy,
        &scope,
        1000,
    )
    .unwrap();
    let hook_admin = admin.clone();
    let hook_learning = learning.clone();
    let hook_scope = scope.clone();
    let hook_owner = owner.clone();
    let hooked = Arc::new(ReadHookLearning {
        inner: learning,
        after_decisions: Mutex::new(None),
        after_snapshot: Mutex::new(Some(Box::new(move || {
            add_candidate(
                &hook_learning,
                &hook_scope,
                "concurrent",
                "回复最多分成3段",
                import_pair(&hook_admin, &hook_owner, "回复最多分成3段"),
            );
            auto_confirm(
                &hook_admin,
                hook_learning.as_ref(),
                &EvidenceConfirmationPolicy,
                &hook_scope,
                1001,
            )
            .unwrap();
        }))),
    });
    let commands = Commands::autonomous(hooked, Arc::new(admin.clone()));
    let reply = run(&commands, &owner, "racing-list", "/memory-candidates")
        .expect("并发追加候选不能被误判为账本损坏")
        .unwrap();
    assert!(reply.contains("initial"));
    assert!(!reply.contains("concurrent"), "这次读取仍展示捕获的旧快照");
    let saved = snapshot(&admin, &owner);
    assert_eq!(saved.preferences.len(), 1);
    assert_eq!(saved.preferences[0].revision, 2);
    kernel.stop_all().await.unwrap();
}

#[test]
fn decision_command_parser_rejects_extra_input_without_disclosing_candidate_id() {
    assert!(parse("/memory-decision-extra id").is_none());
    for text in [
        "/memory-decision",
        "/memory-decision id other",
        "/memory-decision id\0",
    ] {
        assert_eq!(parse(text), Some(Command::Help));
    }
    let valid = parse(" /memory-decision private-id ").unwrap();
    assert_ne!(valid, Command::Help);
    assert!(!format!("{valid:?}").contains("private-id"));
    let commands = Commands::disabled();
    assert_eq!(
        run(&commands, &session(), "audit", "/memory-decision id")
            .unwrap()
            .as_deref(),
        Some("偏好提炼未启用。")
    );
}
