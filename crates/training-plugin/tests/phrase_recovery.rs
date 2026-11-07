use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_llm_api::ContextScope;
use eve_plugin_api::{PluginError, PluginId, PluginResult, ServiceId, StateStore};
use eve_training_api::*;
use eve_training_plugin::TrainingPlugin;
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

const KEY: &str = "phrases.v1";

#[derive(Default)]
struct FaultState {
    storage: MemoryStateStore,
    // 只在下一次说法写入失败：1 写前，2 已提交但返回错误。
    next_fault: AtomicU8,
    next_mode_fault: AtomicU8,
}
impl StateStore for FaultState {
    fn get(&self, owner: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.storage.get(owner, key)
    }
    fn set(&self, owner: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        let fault = if key == KEY {
            self.next_fault.swap(0, Ordering::SeqCst)
        } else if key == "modes.v1" {
            self.next_mode_fault.swap(0, Ordering::SeqCst)
        } else {
            0
        };
        if fault == 1 {
            return Err(PluginError::State("写前失败".into()));
        }
        self.storage.set(owner, key, value)?;
        if fault == 2 {
            return Err(PluginError::State("提交结果未知".into()));
        }
        Ok(())
    }
}
impl FaultState {
    fn raw(&self) -> Vec<u8> {
        self.storage
            .get(&PluginId::new(TRAINING_PLUGIN_ID).unwrap(), KEY)
            .unwrap()
            .unwrap()
    }
    fn document(&self) -> serde_json::Value {
        serde_json::from_slice(&self.raw()).unwrap()
    }
}

async fn open(state: Arc<FaultState>) -> (Kernel, Arc<dyn TrainingService>) {
    let services = KernelServices {
        state,
        ..Default::default()
    };
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
    kernel
        .register(Box::new(
            TrainingPlugin::new(true).unwrap().with_phrase_learning(),
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
fn scope(user: &str) -> ContextScope {
    ContextScope {
        session_id: "same-group".into(),
        user_id: user.into(),
    }
}
fn seed(training: &dyn TrainingService, scope: &ContextScope, prefix: &str) {
    for index in 0..PHRASE_BATCH {
        training
            .observe_user_message(
                scope,
                &format!("{prefix}-{index}"),
                &format!("这游戏yyds，{prefix}{index}"),
            )
            .unwrap();
    }
}
fn drafts() -> Vec<PhraseDraft> {
    vec![PhraseDraft {
        kind: PhraseKind::Term,
        text: "yyds".into(),
        meaning: "夸东西特别好".into(),
        example: "这游戏yyds".into(),
    }]
}

fn text_with_bytes(bytes: usize) -> String {
    let mut text = "原".repeat(bytes / 3);
    match bytes % 3 {
        1 => text.push('x'),
        2 => text.push('é'),
        _ => {}
    }
    assert!(text.chars().count() <= 512);
    text
}

/// 合法 v1 文档恰好填满 2 MiB：每范围最多 24 条，每条至多 512 字符。
/// 用 JSON 实际长度填充，不依赖新实现的阶段预留常量。
fn full_legacy_document() -> Vec<u8> {
    let mut document = serde_json::json!({
        "version": 1,
        "next": 2,
        "pending": [],
        "learned": [{
            "scope": scope("one"),
            "phrase": {
                "id": "p1", "kind": "term", "text": "原话", "meaning": "已有解释",
                "example": "原话", "uses": 1, "updated_at_ms": 1
            }
        }]
    });
    let mut pending = Vec::new();
    for _ in 0..PHRASE_BATCH {
        pending.push(serde_json::json!({"scope": scope("one"), "text": "原话"}));
    }
    let padding = pending.len();
    pending.push(serde_json::json!({"scope": scope("padding"), "text": "x"}));
    let mut size = serde_json::to_vec(&document).unwrap().len()
        + pending
            .iter()
            .map(|entry| serde_json::to_vec(entry).unwrap().len())
            .sum::<usize>()
        + pending.len()
        - 1;
    let maximum = 2 * 1_048_576;
    for index in 0..(256 - 2) * 24 {
        let mut entry = serde_json::json!({
            "scope": scope(&format!("filler-{}", index / 24)),
            "text": text_with_bytes(512 * 3)
        });
        let cost = serde_json::to_vec(&entry).unwrap().len() + 1;
        if size + cost <= maximum {
            size += cost;
            pending.push(entry);
            continue;
        }
        let overhead = cost - 512 * 3;
        let spare = maximum - size;
        if spare > overhead {
            entry["text"] = text_with_bytes(spare - overhead).into();
            pending.push(entry);
        } else {
            pending[padding]["text"] = text_with_bytes(spare + 1).into();
        }
        document["pending"] = pending.into();
        let bytes = serde_json::to_vec(&document).unwrap();
        assert_eq!(bytes.len(), maximum);
        return bytes;
    }
    panic!("应先达到字节上限");
}

fn full_v2_document(phase: serde_json::Value) -> serde_json::Value {
    let mut document: serde_json::Value = serde_json::from_slice(&full_legacy_document()).unwrap();
    document["version"] = 2.into();
    document["next"] = 3.into();
    let messages: Vec<_> = document["pending"]
        .as_array_mut()
        .unwrap()
        .drain(..PHRASE_BATCH)
        .map(|pending| pending["text"].clone())
        .collect();
    document["inflight"] = serde_json::json!([{
        "batch": {
            "id": "b2", "scope": scope("one"), "messages": messages,
            "attempt": 1, "learner_version": PHRASE_LEARNER_VERSION
        },
        "phase": phase
    }]);
    let maximum = 2 * 1_048_576;
    let remaining = maximum - serde_json::to_vec(&document).unwrap().len();
    let padding = document["pending"][0]["text"].as_str().unwrap().len();
    document["pending"][0]["text"] = text_with_bytes(padding + remaining).into();
    assert_eq!(serde_json::to_vec(&document).unwrap().len(), maximum);
    document
}

#[tokio::test]
async fn v2_restore_checks_phase_capacity_without_rejecting_the_valid_boundary() {
    for phase in [
        serde_json::json!({"state": "running"}),
        serde_json::json!({"state": "unknown"}),
        serde_json::json!({"state": "retry_wait", "failure": "timeout", "ready_at_ms": 0}),
    ] {
        let mut document = full_v2_document(phase.clone());
        let bytes = serde_json::to_vec(&document).unwrap();
        let state = Arc::new(FaultState::default());
        let id = PluginId::new(TRAINING_PLUGIN_ID).unwrap();
        state.storage.set(&id, KEY.into(), bytes.clone()).unwrap();
        let kernel = Kernel::with_services(KernelServices {
            state: state.clone(),
            ..Default::default()
        });
        kernel
            .register(Box::new(
                TrainingPlugin::new(true).unwrap().with_phrase_learning(),
            ))
            .unwrap();
        assert!(
            kernel.start(&id).await.is_err(),
            "v2 原始字节合规仍须保留阶段容量"
        );
        assert_eq!(state.raw(), bytes, "拒绝恢复不改写原始文档");
        kernel.stop_all().await.unwrap();

        // 各阶段合计 128 字节的既定格式边界，不因恢复校验而拒绝可写状态。
        let reserve = 128 - serde_json::to_vec(&phase).unwrap().len();
        let padding = document["pending"][0]["text"].as_str().unwrap().len();
        document["pending"][0]["text"] = text_with_bytes(padding - reserve).into();
        let bytes = serde_json::to_vec(&document).unwrap();
        assert_eq!(bytes.len() + reserve, 2 * 1_048_576);
        state.storage.set(&id, KEY.into(), bytes.clone()).unwrap();
        let (kernel, training) = open(state.clone()).await;
        assert_eq!(state.raw(), bytes, "边界状态只读恢复");
        if phase["state"] == "retry_wait" {
            let batch = training.take_phrase_batch(0).unwrap().unwrap();
            assert_eq!(batch.id, "b2");
            assert_eq!(batch.attempt, 2);
            assert_eq!(batch.scope, scope("one"));
        }
        kernel.stop_all().await.unwrap();
    }
}

fn capacity_drafts() -> Vec<PhraseDraft> {
    let mut result = vec![PhraseDraft {
        kind: PhraseKind::Term,
        text: "原话".into(),
        meaning: "试图覆盖已有解释".into(),
        example: "原话".into(),
    }];
    for index in 1..MAX_PHRASE_DRAFTS {
        result.push(PhraseDraft {
            kind: PhraseKind::Expression,
            text: format!("{index}{}", "学".repeat(MAX_PHRASE_TEXT_CHARS - 1)),
            meaning: "义".repeat(MAX_PHRASE_DETAIL_CHARS),
            example: "原话".into(),
        });
    }
    result
}

#[tokio::test]
async fn retry_preserves_original_batch_across_restart_and_saves_once() {
    let state = Arc::new(FaultState::default());
    let (kernel, training) = open(state.clone()).await;
    let owner = scope("one");
    seed(training.as_ref(), &owner, "first");
    let first = training.take_phrase_batch(10).unwrap().unwrap();
    seed(training.as_ref(), &owner, "later");
    assert_eq!(first.attempt, 1);
    assert_eq!(
        state.document()["inflight"][0]["batch"]["messages"],
        serde_json::json!(first.messages)
    );
    assert_eq!(
        training
            .fail_phrase_batch(&first, PhraseFailure::Timeout, 20)
            .unwrap(),
        PhraseFailureOutcome::Retry {
            ready_at_ms: 60_020
        }
    );
    assert!(training.take_phrase_batch(60_019).unwrap().is_none());
    let before = state.raw();
    kernel.stop_all().await.unwrap();
    let (kernel, training) = open(state.clone()).await;
    assert_eq!(state.raw(), before, "只读恢复不改写状态");
    let second = training.take_phrase_batch(60_020).unwrap().unwrap();
    assert_eq!(second.id, first.id);
    assert_eq!(second.scope, first.scope);
    assert_eq!(second.messages, first.messages);
    assert_eq!(second.attempt, 2);
    assert_eq!(
        training
            .finish_phrase_batch(&first, drafts(), 60_021)
            .unwrap(),
        PhraseFinish::Stale
    );
    assert_eq!(
        training
            .finish_phrase_batch(&second, drafts(), 60_022)
            .unwrap(),
        PhraseFinish::Saved(1)
    );
    assert_eq!(
        training
            .finish_phrase_batch(&second, drafts(), 60_023)
            .unwrap(),
        PhraseFinish::Stale
    );
    assert_eq!(training.phrases(&owner).unwrap()[0].uses, 1);
    let next = training.take_phrase_batch(60_024).unwrap().unwrap();
    assert_ne!(next.id, first.id);
    assert!(next.messages.iter().all(|m| m.contains("later")));
    assert_eq!(
        training.finish_phrase_batch(&next, vec![], 60_025).unwrap(),
        PhraseFinish::Saved(0)
    );
    kernel.stop_all().await.unwrap();
    let (kernel, training) = open(state).await;
    assert!(training.take_phrase_batch(u64::MAX).unwrap().is_none());
    assert_eq!(training.phrases(&owner).unwrap()[0].uses, 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn retry_budget_is_persistent_and_exhausted_evidence_is_not_rebuffered() {
    let state = Arc::new(FaultState::default());
    let (mut kernel, mut training) = open(state.clone()).await;
    seed(training.as_ref(), &scope("one"), "batch");
    let first = training.take_phrase_batch(0).unwrap().unwrap();
    let mut batch = first.clone();
    let mut now = 100;
    for attempt in 1..=MAX_PHRASE_ATTEMPTS {
        assert_eq!(batch.attempt, attempt);
        let outcome = training
            .fail_phrase_batch(&batch, PhraseFailure::ServerError, now)
            .unwrap();
        kernel.stop_all().await.unwrap();
        (kernel, training) = open(state.clone()).await;
        if attempt == MAX_PHRASE_ATTEMPTS {
            assert_eq!(outcome, PhraseFailureOutcome::Exhausted);
            assert!(training.take_phrase_batch(u64::MAX).unwrap().is_none());
        } else {
            let ready = now + 60_000 * u64::from(attempt);
            assert_eq!(outcome, PhraseFailureOutcome::Retry { ready_at_ms: ready });
            assert!(training.take_phrase_batch(ready - 1).unwrap().is_none());
            batch = training.take_phrase_batch(ready).unwrap().unwrap();
            assert_eq!(batch.id, first.id);
            assert_eq!(batch.messages, first.messages);
            now = ready;
        }
    }
    assert_eq!(state.document()["pending"], serde_json::json!([]));
    assert_eq!(state.document()["inflight"], serde_json::json!([]));
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn cancelled_rejected_invalid_and_empty_results_do_not_replay() {
    for case in 0..4 {
        let state = Arc::new(FaultState::default());
        let (kernel, training) = open(state.clone()).await;
        seed(training.as_ref(), &scope("one"), "batch");
        let batch = training.take_phrase_batch(0).unwrap().unwrap();
        match case {
            0 | 1 => assert_eq!(
                training
                    .fail_phrase_batch(
                        &batch,
                        if case == 0 {
                            PhraseFailure::Cancelled
                        } else {
                            PhraseFailure::Rejected
                        },
                        1
                    )
                    .unwrap(),
                PhraseFailureOutcome::Finished
            ),
            2 => {
                let mut invalid = drafts();
                invalid[0].example = "不是本批原话".into();
                assert_eq!(
                    training.finish_phrase_batch(&batch, invalid, 1).unwrap(),
                    PhraseFinish::Rejected
                );
            }
            _ => assert_eq!(
                training.finish_phrase_batch(&batch, vec![], 1).unwrap(),
                PhraseFinish::Saved(0)
            ),
        }
        kernel.stop_all().await.unwrap();
        let (kernel, training) = open(state.clone()).await;
        assert!(training.take_phrase_batch(u64::MAX).unwrap().is_none());
        assert!(training.phrases(&scope("one")).unwrap().is_empty());
        assert_eq!(state.document()["inflight"], serde_json::json!([]));
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn storage_unknown_never_retries_or_overwrites_committed_learning() {
    for fault in [1, 2] {
        let state = Arc::new(FaultState::default());
        let (kernel, training) = open(state.clone()).await;
        let owner = scope("one");
        seed(training.as_ref(), &owner, "batch");
        let batch = training.take_phrase_batch(0).unwrap().unwrap();
        state.next_fault.store(fault, Ordering::SeqCst);
        assert!(training.finish_phrase_batch(&batch, drafts(), 1).is_err());
        let after = state.raw();
        assert!(training.take_phrase_batch(u64::MAX).is_err());
        assert!(
            training
                .fail_phrase_batch(&batch, PhraseFailure::Timeout, 2)
                .is_err()
        );
        assert!(
            training
                .observe_user_message(&owner, "new", "新的原话")
                .is_err()
        );
        assert!(training.forget_phrase(&owner, "p2").is_err());
        assert!(training.reset_expression(&owner).is_err());
        assert_eq!(state.raw(), after, "旧句柄不能覆盖可能已提交的快照");
        kernel.stop_all().await.unwrap();
        let (kernel, training) = open(state.clone()).await;
        assert_eq!(state.raw(), after, "恢复必须先只读");
        assert!(training.take_phrase_batch(u64::MAX).unwrap().is_none());
        assert_eq!(
            training.finish_phrase_batch(&batch, drafts(), 3).unwrap(),
            PhraseFinish::Stale
        );
        assert_eq!(
            training.phrases(&owner).unwrap().len(),
            usize::from(fault == 2)
        );
        if fault == 2 {
            assert_eq!(training.phrases(&owner).unwrap()[0].uses, 1);
        } else {
            assert_eq!(state.document()["inflight"][0]["batch"]["id"], batch.id);
        }
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn claim_and_failure_write_errors_follow_the_persisted_phase() {
    for fault in [1, 2] {
        let state = Arc::new(FaultState::default());
        let (kernel, training) = open(state.clone()).await;
        seed(training.as_ref(), &scope("one"), "batch");
        state.next_fault.store(fault, Ordering::SeqCst);
        assert!(
            training.take_phrase_batch(0).is_err(),
            "未知领取不返回可发模型的批次"
        );
        assert!(training.take_phrase_batch(1).is_err());
        kernel.stop_all().await.unwrap();
        let (kernel, training) = open(state.clone()).await;
        let maybe = training.take_phrase_batch(u64::MAX).unwrap();
        assert_eq!(maybe.is_some(), fault == 1, "已落盘 Running 只能保留未知");
        kernel.stop_all().await.unwrap();
    }
    for fault in [1, 2] {
        let state = Arc::new(FaultState::default());
        let (kernel, training) = open(state.clone()).await;
        seed(training.as_ref(), &scope("one"), "batch");
        let batch = training.take_phrase_batch(0).unwrap().unwrap();
        state.next_fault.store(fault, Ordering::SeqCst);
        assert!(
            training
                .fail_phrase_batch(&batch, PhraseFailure::Network, 1)
                .is_err()
        );
        assert!(training.take_phrase_batch(u64::MAX).is_err());
        kernel.stop_all().await.unwrap();
        let (kernel, training) = open(state).await;
        let maybe = training.take_phrase_batch(60_001).unwrap();
        if fault == 2 {
            let recovered = maybe.unwrap();
            assert_eq!(recovered.id, batch.id);
            assert_eq!(recovered.attempt, 2);
        } else {
            assert!(maybe.is_none());
        }
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn running_after_restart_is_unknown_but_other_scopes_can_continue() {
    let state = Arc::new(FaultState::default());
    let (kernel, training) = open(state.clone()).await;
    seed(training.as_ref(), &scope("one"), "batch");
    let batch = training.take_phrase_batch(0).unwrap().unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, training) = open(state).await;
    seed(training.as_ref(), &scope("one"), "new");
    seed(training.as_ref(), &scope("two"), "other");
    let other = training.take_phrase_batch(u64::MAX).unwrap().unwrap();
    assert_eq!(other.scope, scope("two"));
    assert_eq!(
        training.finish_phrase_batch(&batch, drafts(), 1).unwrap(),
        PhraseFinish::Stale
    );
    assert_eq!(
        training
            .fail_phrase_batch(&batch, PhraseFailure::Timeout, 1)
            .unwrap(),
        PhraseFailureOutcome::Stale
    );
    assert!(training.take_phrase_batch(u64::MAX).unwrap().is_none());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn stop_resume_reset_and_forget_reject_late_results_without_cross_scope_effects() {
    for operation in ["stop", "reset", "forget"] {
        let state = Arc::new(FaultState::default());
        let (kernel, training) = open(state.clone()).await;
        let one = scope("one");
        let two = scope("two");
        seed(training.as_ref(), &one, "learn");
        let learned = training.take_phrase_batch(0).unwrap().unwrap();
        training.finish_phrase_batch(&learned, drafts(), 1).unwrap();
        let learned_id = training.phrases(&one).unwrap()[0].id.clone();
        seed(training.as_ref(), &one, "first");
        seed(training.as_ref(), &two, "other");
        let late = training.take_phrase_batch(2).unwrap().unwrap();
        let other = training.take_phrase_batch(2).unwrap().unwrap();
        assert_eq!(late.scope, one);
        assert_eq!(other.scope, two);
        training
            .fail_phrase_batch(&late, PhraseFailure::Timeout, 3)
            .unwrap();
        match operation {
            "stop" => {
                training.set_enabled(&one, false).unwrap();
                training.set_enabled(&one, true).unwrap();
            }
            "reset" => training.reset_expression(&one).unwrap(),
            _ => assert!(training.forget_phrase(&one, &learned_id).unwrap()),
        }
        assert_eq!(
            training.finish_phrase_batch(&late, drafts(), 4).unwrap(),
            PhraseFinish::Stale
        );
        assert_eq!(
            training
                .fail_phrase_batch(&late, PhraseFailure::Timeout, 4)
                .unwrap(),
            PhraseFailureOutcome::Stale
        );
        assert_eq!(
            training.finish_phrase_batch(&other, drafts(), 4).unwrap(),
            PhraseFinish::Saved(1)
        );
        kernel.stop_all().await.unwrap();
        let (kernel, training) = open(state).await;
        assert!(training.take_phrase_batch(u64::MAX).unwrap().is_none());
        assert_eq!(
            training.phrases(&one).unwrap().len(),
            usize::from(operation == "stop")
        );
        assert_eq!(training.phrases(&two).unwrap().len(), 1);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn forged_attempt_scope_or_original_messages_cannot_finish_a_batch() {
    let state = Arc::new(FaultState::default());
    let (kernel, training) = open(state.clone()).await;
    seed(training.as_ref(), &scope("one"), "batch");
    let batch = training.take_phrase_batch(0).unwrap().unwrap();
    let before = state.raw();
    for field in 0..4 {
        let mut forged = batch.clone();
        match field {
            0 => forged.attempt += 1,
            1 => forged.scope.user_id = "two".into(),
            2 => forged.messages[0] = "别人的原话".into(),
            _ => forged.learner_version += 1,
        }
        assert_eq!(
            training.finish_phrase_batch(&forged, drafts(), 1).unwrap(),
            PhraseFinish::Stale
        );
        assert_eq!(
            training
                .fail_phrase_batch(&forged, PhraseFailure::Timeout, 1)
                .unwrap(),
            PhraseFailureOutcome::Stale
        );
        assert_eq!(state.raw(), before);
    }
    assert_eq!(
        training.finish_phrase_batch(&batch, drafts(), 2).unwrap(),
        PhraseFinish::Saved(1)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn unknown_mode_commit_stops_new_claims_and_does_not_undo_persisted_pause() {
    for fault in [1, 2] {
        let state = Arc::new(FaultState::default());
        let (kernel, training) = open(state.clone()).await;
        let owner = scope("one");
        seed(training.as_ref(), &owner, "old");
        let old = training.take_phrase_batch(0).unwrap().unwrap();
        seed(training.as_ref(), &owner, "pending");
        state.next_mode_fault.store(fault, Ordering::SeqCst);
        assert!(training.set_enabled(&owner, false).is_err());
        let after = state.raw();
        assert!(training.take_phrase_batch(u64::MAX).is_err());
        assert!(training.finish_phrase_batch(&old, drafts(), 1).is_err());
        assert!(
            training
                .observe_user_message(&owner, "new", "新的原话")
                .is_err()
        );
        assert_eq!(state.raw(), after);
        kernel.stop_all().await.unwrap();
        let (kernel, training) = open(state).await;
        assert_eq!(training.enabled(&owner).unwrap(), fault == 1);
        let fresh = training.take_phrase_batch(u64::MAX).unwrap();
        if fault == 1 {
            assert_ne!(fresh.unwrap().id, old.id);
        } else {
            assert!(fresh.is_none(), "已经提交的停止模式不能被旧内存覆盖");
        }
        assert_eq!(
            training.finish_phrase_batch(&old, drafts(), 2).unwrap(),
            PhraseFinish::Stale
        );
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn capacity_rejection_is_persisted_and_only_backend_errors_stop_the_instance() {
    for fault in [0, 1, 2] {
        let state = Arc::new(FaultState::default());
        let initial = full_legacy_document();
        state
            .storage
            .set(
                &PluginId::new(TRAINING_PLUGIN_ID).unwrap(),
                KEY.into(),
                initial.clone(),
            )
            .unwrap();
        let (kernel, training) = open(state.clone()).await;
        assert_eq!(state.raw(), initial, "合法旧满状态只读加载");
        let batch = training.take_phrase_batch(2).unwrap().unwrap();
        assert_eq!(batch.scope, scope("one"));
        let before = state.document();
        state.next_fault.store(fault, Ordering::SeqCst);
        let result = training.finish_phrase_batch(&batch, capacity_drafts(), 3);
        if fault == 0 {
            assert_eq!(
                result.unwrap(),
                PhraseFinish::CapacityLimited {
                    saved: 0,
                    skipped: MAX_PHRASE_DRAFTS,
                }
            );
            assert_eq!(training.phrases(&scope("one")).unwrap()[0].uses, 1);
            assert_eq!(
                training
                    .finish_phrase_batch(&batch, capacity_drafts(), 4)
                    .unwrap(),
                PhraseFinish::Stale
            );
        } else {
            assert!(result.is_err(), "终态提交错误不能伪装明确拒绝");
            assert!(training.take_phrase_batch(4).is_err());
            assert!(training.phrases(&scope("one")).is_err());
            assert!(training.reset_expression(&scope("one")).is_err());
        }
        let saved = state.raw();
        let after = state.document();
        for field in ["pending", "learned", "next"] {
            assert_eq!(after[field], before[field], "完整回退 {field}");
        }
        assert_eq!(
            after["inflight"].as_array().unwrap().len(),
            usize::from(fault == 1)
        );
        if fault == 0 {
            let other = training.take_phrase_batch(5).unwrap().unwrap();
            assert_ne!(other.scope, batch.scope, "明确容量拒绝后仍能继续学习");
            assert_eq!(
                training.finish_phrase_batch(&other, vec![], 6).unwrap(),
                PhraseFinish::Saved(0)
            );
        }
        kernel.stop_all().await.unwrap();
        let (kernel, training) = open(state.clone()).await;
        if fault != 0 {
            assert_eq!(state.raw(), saved);
        }
        assert_eq!(training.phrases(&scope("one")).unwrap()[0].uses, 1);
        assert_eq!(
            training
                .finish_phrase_batch(&batch, capacity_drafts(), 7)
                .unwrap(),
            PhraseFinish::Stale
        );
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn terminal_failure_write_errors_preserve_the_committed_cancellation() {
    for failure in [PhraseFailure::Cancelled, PhraseFailure::Rejected] {
        for fault in [1, 2] {
            let state = Arc::new(FaultState::default());
            let (kernel, training) = open(state.clone()).await;
            seed(training.as_ref(), &scope("one"), "batch");
            let batch = training.take_phrase_batch(0).unwrap().unwrap();
            state.next_fault.store(fault, Ordering::SeqCst);
            assert!(training.fail_phrase_batch(&batch, failure, 1).is_err());
            let saved = state.raw();
            assert!(training.take_phrase_batch(u64::MAX).is_err());
            assert!(training.finish_phrase_batch(&batch, drafts(), 2).is_err());
            assert_eq!(state.raw(), saved);
            kernel.stop_all().await.unwrap();
            let (kernel, training) = open(state.clone()).await;
            assert_eq!(state.raw(), saved);
            assert!(training.take_phrase_batch(u64::MAX).unwrap().is_none());
            assert_eq!(
                state.document()["inflight"].as_array().unwrap().len(),
                usize::from(fault == 1)
            );
            assert_eq!(
                training.fail_phrase_batch(&batch, failure, 3).unwrap(),
                PhraseFailureOutcome::Stale
            );
            kernel.stop_all().await.unwrap();
        }
    }
}

#[tokio::test]
async fn unknown_phase_fields_refuse_start_without_altering_the_document() {
    let original = Arc::new(FaultState::default());
    let (kernel, training) = open(original.clone()).await;
    seed(training.as_ref(), &scope("one"), "batch");
    training.take_phrase_batch(0).unwrap().unwrap();
    let template = original.document();
    kernel.stop_all().await.unwrap();
    for phase in [
        serde_json::json!({"state": "running", "failure": "cancelled"}),
        serde_json::json!({"state": "unknown", "extra": true}),
        serde_json::json!({
            "state": "retry_wait", "failure": "timeout", "ready_at_ms": 100, "extra": true
        }),
    ] {
        let mut invalid = template.clone();
        invalid["inflight"][0]["phase"] = phase;
        let bytes = serde_json::to_vec(&invalid).unwrap();
        let state = Arc::new(FaultState::default());
        let id = PluginId::new(TRAINING_PLUGIN_ID).unwrap();
        state.storage.set(&id, KEY.into(), bytes.clone()).unwrap();
        let kernel = Kernel::with_services(KernelServices {
            state: state.clone(),
            ..Default::default()
        });
        kernel
            .register(Box::new(
                TrainingPlugin::new(true).unwrap().with_phrase_learning(),
            ))
            .unwrap();
        assert!(kernel.start(&id).await.is_err());
        assert_eq!(state.raw(), bytes);
    }
}
