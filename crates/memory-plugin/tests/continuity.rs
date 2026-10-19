use eve_kernel::Kernel;
use eve_memory_api::*;
use eve_memory_plugin::{MemoryContinuity, MemoryPlugin};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn scope(session: &str) -> MemoryScope {
    MemoryScope {
        channel: "qq".into(),
        session_id: session.into(),
        user_id: "same-trusted-person".into(),
    }
}
struct Policy {
    enabled: AtomicBool,
    scopes: Vec<MemoryScope>,
    interactions: bool,
}
impl MemoryContinuityPolicy for Policy {
    fn grants(&self, _: &MemoryScope, _: &[MemoryScope]) -> MemoryResult<Vec<MemoryScopeGrant>> {
        Ok(if self.enabled.load(Ordering::SeqCst) {
            self.scopes
                .iter()
                .map(|scope| MemoryScopeGrant {
                    scope: scope.clone(),
                    interactions: self.interactions,
                })
                .collect()
        } else {
            vec![]
        })
    }
}
fn change(
    memory: &dyn MemoryAdmin,
    source: &MemoryScope,
    action: PreferenceAction,
) -> MemorySnapshot {
    let revision = memory
        .reader(source.clone())
        .unwrap()
        .snapshot()
        .unwrap()
        .revision;
    memory
        .update_preference(
            source,
            revision,
            PreferenceChange {
                operation_id: format!("op-{revision}"),
                at_ms: 100 + revision,
                evidence: PreferenceEvidence::Statement(UserStatement {
                    evidence_id: format!("e-{revision}"),
                    message_id: format!("m-{revision}"),
                    text: "真实用户命令".into(),
                    at_ms: 100 + revision,
                }),
                action,
            },
        )
        .unwrap()
}

#[tokio::test]
async fn bound_reads_preserve_each_origin_and_follow_corrections_revocations_and_policy_changes() {
    let kernel = Kernel::new();
    let plugin = MemoryPlugin::new().unwrap();
    let memory = Arc::new(plugin.controller());
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let a = scope("old-a");
    let b = scope("old-b");
    for source in [&a, &b] {
        change(
            memory.as_ref(),
            source,
            PreferenceAction::Confirm {
                id: "same-local-id".into(),
                text: format!("持续偏好 {}", source.session_id),
            },
        );
    }
    let policy = Arc::new(Policy {
        enabled: AtomicBool::new(true),
        scopes: vec![a.clone(), b.clone()],
        interactions: false,
    });
    let continuity = MemoryContinuity::new(memory.clone(), policy.clone());
    let reader = continuity.reader(scope("new-conversation")).unwrap();
    let before_a = memory.reader(a.clone()).unwrap().snapshot().unwrap();
    let before_b = memory.reader(b.clone()).unwrap().snapshot().unwrap();
    let view = reader.read("持续偏好").unwrap();
    assert_eq!(view.sources.len(), 2);
    assert_ne!(view.sources[0].scope, view.sources[1].scope);
    assert_eq!(
        view.sources[0].preferences[0].id,
        view.sources[1].preferences[0].id
    );
    assert_eq!(
        memory.reader(a.clone()).unwrap().snapshot().unwrap(),
        before_a
    );
    assert_eq!(
        memory.reader(b.clone()).unwrap().snapshot().unwrap(),
        before_b
    );
    change(
        memory.as_ref(),
        &a,
        PreferenceAction::Correct {
            id: "same-local-id".into(),
            text: "已经修正".into(),
        },
    );
    assert_eq!(
        reader.read("").unwrap().sources[0].preferences[0].text,
        "已经修正"
    );
    change(
        memory.as_ref(),
        &a,
        PreferenceAction::Revoke {
            id: "same-local-id".into(),
        },
    );
    let view = reader.read("").unwrap();
    assert_eq!(view.sources.len(), 1);
    assert_eq!(view.sources[0].scope, b);
    policy.enabled.store(false, Ordering::SeqCst);
    assert!(reader.read("持续偏好").unwrap().sources.is_empty());
    kernel.stop_all().await.unwrap();
    assert!(
        reader.read("").is_err(),
        "stopped memory handles must not become an empty success"
    );
    kernel.start_all().await.unwrap();
    assert!(
        reader.read("").is_err(),
        "old bound readers must not revive after plugin restart"
    );
    policy.enabled.store(true, Ordering::SeqCst);
    assert_eq!(
        continuity
            .reader(scope("new-conversation"))
            .unwrap()
            .read("")
            .unwrap()
            .sources
            .len(),
        1
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn unlisted_duplicate_and_current_scope_grants_are_rejected_without_writes() {
    let kernel = Kernel::new();
    let plugin = MemoryPlugin::new().unwrap();
    let memory = Arc::new(plugin.controller());
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let a = scope("old");
    change(
        memory.as_ref(),
        &a,
        PreferenceAction::Confirm {
            id: "p".into(),
            text: "已保存".into(),
        },
    );
    let original = memory.reader(a.clone()).unwrap().snapshot().unwrap();
    for scopes in [
        vec![scope("never-stored")],
        vec![a.clone(), a.clone()],
        vec![a.clone()],
    ] {
        let policy = Arc::new(Policy {
            enabled: AtomicBool::new(true),
            scopes: scopes.clone(),
            interactions: true,
        });
        let target = if scopes.len() == 1 && scopes[0] == a {
            a.clone()
        } else {
            scope("new")
        };
        assert!(
            MemoryContinuity::new(memory.clone(), policy)
                .reader(target)
                .unwrap()
                .read("saved")
                .is_err()
        );
    }
    assert_eq!(
        memory.reader(a.clone()).unwrap().snapshot().unwrap(),
        original
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn large_person_profiles_are_bounded_and_keep_complete_preference_versions() {
    let kernel = Kernel::new();
    let plugin = MemoryPlugin::new().unwrap();
    let memory = Arc::new(plugin.controller());
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let source = scope("old");
    for i in 0..12 {
        change(
            memory.as_ref(),
            &source,
            PreferenceAction::Confirm {
                id: format!("p-{i}"),
                text: "长".repeat(1000),
            },
        );
    }
    let policy = Arc::new(Policy {
        enabled: AtomicBool::new(true),
        scopes: vec![source],
        interactions: false,
    });
    let target = scope("new");
    let view = MemoryContinuity::new(memory, policy)
        .reader(target.clone())
        .unwrap()
        .read("")
        .unwrap();
    view.validate_for(&target, "").unwrap();
    assert!(view.truncated);
    assert!(serde_json::to_vec(&view).unwrap().len() <= MAX_CONTINUITY_BYTES);
    assert!(
        view.sources
            .iter()
            .flat_map(|s| &s.preferences)
            .all(|p| p.text == "长".repeat(1000))
    );
    kernel.stop_all().await.unwrap();
}
