use eve_kernel::{Kernel, KernelServices};
use eve_memory_api::*;
use eve_memory_plugin::{
    MemoryContinuity, MemoryController, MemoryPlugin, UnifiedMemoryController, UnifiedMemoryPlugin,
};
use std::sync::Arc;

fn scope(channel: &str) -> MemoryScope {
    MemoryScope {
        channel: channel.into(),
        session_id: "private".into(),
        user_id: format!("{channel}-trusted-user"),
    }
}
async fn open(
    services: KernelServices,
) -> (Kernel, Arc<MemoryController>, UnifiedMemoryController) {
    let kernel = Kernel::with_services(services);
    let memory = MemoryPlugin::new().unwrap();
    let admin = Arc::new(memory.controller());
    let unified = UnifiedMemoryPlugin::new().unwrap();
    let controller = unified.controller();
    kernel.register(Box::new(memory)).unwrap();
    kernel.register(Box::new(unified)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, admin, controller)
}
fn binding(id: &str, source: &str, target: &str, revision: u64) -> UnifiedMemoryBinding {
    UnifiedMemoryBinding {
        id: id.into(),
        revision,
        principal: "authenticated-person".into(),
        source: scope(source),
        target: scope(target),
        interactions: true,
        revoked: false,
        authorization_reference: format!("consent-{id}"),
    }
}
fn preference(memory: &dyn MemoryAdmin, id: &str) {
    let source = scope("qq");
    let revision = memory
        .reader(source.clone())
        .unwrap()
        .snapshot()
        .unwrap()
        .revision;
    memory
        .update_preference(
            &source,
            revision,
            PreferenceChange {
                operation_id: id.into(),
                at_ms: 100 + revision,
                evidence: PreferenceEvidence::Statement(UserStatement {
                    evidence_id: id.into(),
                    message_id: id.into(),
                    text: "Use Rust examples".into(),
                    at_ms: 100 + revision,
                }),
                action: PreferenceAction::Confirm {
                    id: id.into(),
                    text: "Use Rust examples".into(),
                },
            },
        )
        .unwrap();
}
fn holdout(controller: &UnifiedMemoryController, id: &str, revision: u64) {
    let s = controller.snapshot().unwrap();
    controller
        .register_holdout(
            s.revision,
            RecallHoldout {
                id: id.into(),
                query: "Rust".into(),
                target: scope("web"),
                sources: vec![RecallHoldoutSource {
                    scope: scope("qq"),
                    revision,
                }],
                expected: vec![RecallExpectedEvidence {
                    scope: scope("qq"),
                    evidence_id: "first".into(),
                }],
                binding_revision: s.binding_revision,
            },
        )
        .unwrap();
}
#[tokio::test]
async fn grants_are_directed_non_transitive_and_revoked_for_existing_readers() {
    let (kernel, memory, controller) = open(KernelServices::default()).await;
    preference(memory.as_ref(), "first");
    controller.bind(0, binding("a", "qq", "web", 1)).unwrap();
    controller.bind(1, binding("b", "web", "phone", 2)).unwrap();
    assert!(
        controller
            .grants(&scope("qq"), &[scope("web")])
            .unwrap()
            .is_empty()
    );
    assert!(
        controller
            .grants(&scope("phone"), &[scope("qq")])
            .unwrap()
            .is_empty()
    );
    let continuity = MemoryContinuity::new(memory, Arc::new(controller.clone()));
    let reader = continuity.reader(scope("web")).unwrap();
    assert_eq!(reader.read("Rust").unwrap().sources.len(), 1);
    controller.revoke(2, "a").unwrap();
    assert!(reader.read("Rust").unwrap().sources.is_empty());
    assert_eq!(controller.snapshot().unwrap().bindings.len(), 3);
    kernel.stop_all().await.unwrap();
}
#[tokio::test]
async fn evaluation_freezes_sources_and_never_replaces_a_version_score() {
    let (kernel, memory, controller) = open(KernelServices::default()).await;
    preference(memory.as_ref(), "first");
    controller.bind(0, binding("a", "qq", "web", 1)).unwrap();
    holdout(&controller, "held", 1);
    let continuity = MemoryContinuity::new(memory.clone(), Arc::new(controller.clone()));
    let result = controller
        .evaluate("held", "retriever-v1", &[], memory.as_ref(), &continuity)
        .unwrap();
    assert!(result.status == RecallEvaluationStatus::Measured);
    assert_eq!((result.precision_bps, result.recall_bps), (10000, 10000));
    preference(memory.as_ref(), "second");
    let repeated = controller
        .evaluate("held", "retriever-v1", &[], memory.as_ref(), &continuity)
        .unwrap();
    assert!(repeated == result);
    let changed = controller
        .evaluate("held", "retriever-v2", &[], memory.as_ref(), &continuity)
        .unwrap();
    assert!(changed.status == RecallEvaluationStatus::Inconclusive);
    assert_eq!(changed.reason, "scope_changed");
    let contaminated = controller
        .evaluate(
            "held",
            "retriever-v3",
            &["  RUST ".into()],
            memory.as_ref(),
            &continuity,
        )
        .unwrap();
    assert_eq!(contaminated.reason, "training_overlap");
    kernel.stop_all().await.unwrap();
}
#[tokio::test]
async fn restart_preserves_revocation_and_old_readers_stay_closed() {
    let services = KernelServices::default();
    let state = services.state.clone();
    let (kernel, memory, controller) = open(services).await;
    preference(memory.as_ref(), "first");
    controller.bind(0, binding("a", "qq", "web", 1)).unwrap();
    controller.revoke(1, "a").unwrap();
    kernel.stop_all().await.unwrap();
    assert!(matches!(
        controller.snapshot(),
        Err(MemoryError::Unavailable)
    ));
    let (kernel, _, restored) = open(KernelServices {
        state,
        ..KernelServices::default()
    })
    .await;
    assert_eq!(restored.snapshot().unwrap().bindings.len(), 2);
    assert!(
        restored
            .grants(&scope("web"), &[scope("qq")])
            .unwrap()
            .is_empty()
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn full_ledger_reserves_space_for_revocation_and_rejects_identity_reassignment() {
    let (kernel, _, controller) = open(KernelServices::default()).await;
    for i in 0..128 {
        controller
            .bind(i, binding(&format!("binding-{i}"), "qq", "web", i + 1))
            .unwrap();
    }
    assert!(matches!(
        controller.bind(128, binding("overflow", "qq", "web", 129)),
        Err(MemoryError::LimitReached)
    ));
    controller.revoke(128, "binding-0").unwrap();
    assert_eq!(controller.snapshot().unwrap().revision, 129);
    let mut wrong_person = binding("wrong-person", "qq", "web", 130);
    wrong_person.principal = "another-person".into();
    // 容量或身份冲突都不能修改现有历史。
    assert!(controller.bind(129, wrong_person).is_err());
    assert_eq!(controller.snapshot().unwrap().revision, 129);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn malformed_persisted_history_is_not_reset() {
    let services = KernelServices::default();
    let store = services.state.clone();
    let owner = eve_plugin_api::PluginId::new(UNIFIED_MEMORY_PLUGIN_ID).unwrap();
    let bytes = br#"{"format_version":1,"format_version":1,"state":{}}"#.to_vec();
    store
        .set(&owner, "unified.v1".into(), bytes.clone())
        .unwrap();
    let kernel = Kernel::with_services(services);
    kernel
        .register(Box::new(UnifiedMemoryPlugin::new().unwrap()))
        .unwrap();
    assert!(kernel.start_all().await.is_err());
    assert_eq!(store.get(&owner, "unified.v1").unwrap(), Some(bytes));
}

struct ForgedRecall(MemoryContinuity);
struct ForgedReader(Arc<dyn MemoryContinuityReader>);
impl MemoryContinuityFactory for ForgedRecall {
    fn reader(&self, target: MemoryScope) -> MemoryResult<Arc<dyn MemoryContinuityReader>> {
        Ok(Arc::new(ForgedReader(self.0.reader(target)?)))
    }
}
impl MemoryContinuityReader for ForgedReader {
    fn read(&self, query: &str) -> MemoryResult<MemoryContinuityView> {
        let mut view = self.0.read(query)?;
        view.sources[0].hits[0].excerpt = "invented content absent from source".into();
        Ok(view)
    }
}
#[tokio::test]
async fn independent_evaluator_rejects_fabricated_excerpts() {
    let (kernel, memory, controller) = open(KernelServices::default()).await;
    preference(memory.as_ref(), "first");
    controller.bind(0, binding("a", "qq", "web", 1)).unwrap();
    holdout(&controller, "held", 1);
    let recall = ForgedRecall(MemoryContinuity::new(
        memory.clone(),
        Arc::new(controller.clone()),
    ));
    let result = controller
        .evaluate("held", "forged-v1", &[], memory.as_ref(), &recall)
        .unwrap();
    assert!(result.status == RecallEvaluationStatus::Inconclusive);
    assert_eq!(result.reason, "invalid_response");
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn independent_evaluator_accepts_real_truncated_excerpts() {
    let (kernel, memory, controller) = open(KernelServices::default()).await;
    let text = format!("{} Rust {}", "prefix ".repeat(100), "suffix ".repeat(100));
    memory
        .update_preference(
            &scope("qq"),
            0,
            PreferenceChange {
                operation_id: "first".into(),
                at_ms: 100,
                evidence: PreferenceEvidence::Statement(UserStatement {
                    evidence_id: "first".into(),
                    message_id: "first".into(),
                    text: text.clone(),
                    at_ms: 100,
                }),
                action: PreferenceAction::Confirm {
                    id: "first".into(),
                    text,
                },
            },
        )
        .unwrap();
    controller.bind(0, binding("a", "qq", "web", 1)).unwrap();
    holdout(&controller, "held", 1);
    let continuity = MemoryContinuity::new(memory.clone(), Arc::new(controller.clone()));
    let view = continuity
        .reader(scope("web"))
        .unwrap()
        .read("Rust")
        .unwrap();
    assert!(view.sources[0].hits[0].excerpt_truncated);
    let result = controller
        .evaluate("held", "real-v1", &[], memory.as_ref(), &continuity)
        .unwrap();
    assert!(result.status == RecallEvaluationStatus::Measured);
    assert_eq!(result.recall_bps, 10000);
    kernel.stop_all().await.unwrap();
}
