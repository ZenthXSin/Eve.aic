use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_plugin_api::{PluginId, StateStore};
use eve_session_api::SessionKey;
use eve_trigger_api::*;
use eve_trigger_plugin::{TriggerController, TriggerPlugin};
use std::sync::Arc;

async fn open(state: Arc<MemoryStateStore>) -> (Kernel, TriggerController) {
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..Default::default()
    });
    let plugin = TriggerPlugin::new().unwrap();
    let controller = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, controller)
}

fn owner(session: &str) -> SessionKey {
    SessionKey::new(session, "trusted-user").unwrap()
}

fn spec() -> TriggerSpec {
    TriggerSpec {
        title: "检查目标".into(),
        instructions: "核对目标进展，不扩大原授权范围".into(),
        mode: MatchMode::All,
        conditions: vec![Condition::Time { at_ms: 2000 }],
        expires_at_ms: 1_000_000,
        recurrence: None,
        resume_task_id: None,
    }
}

fn originals() -> Vec<String> {
    (0..6)
        .map(|index| {
            format!(
                " 第 {index} 条用户原话\n{}\n保留尾部\t ",
                "原授权范围".repeat(200)
            )
        })
        .collect()
}

fn bytes(state: &MemoryStateStore) -> Vec<u8> {
    state
        .get(&PluginId::new(PLUGIN_ID).unwrap(), STATE_KEY)
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn frozen_originals_survive_restart_and_idempotency_cannot_replace_them() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, controller) = open(state.clone()).await;
    let trigger = controller
        .create_with_review_messages(owner("a"), "call".into(), spec(), originals(), 1000)
        .unwrap();
    let encoded = bytes(&state);
    let stored: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(stored["format_version"], 3);
    assert_eq!(trigger.review_messages, Some(originals()));
    assert_eq!(
        controller
            .create_with_review_messages(
                owner("a"),
                "call".into(),
                spec(),
                vec!["这段后来的文本不能替换冻结原话".into()],
                1100,
            )
            .unwrap(),
        trigger
    );
    assert_eq!(
        controller
            .create(owner("a"), "call".into(), spec(), 1100)
            .unwrap(),
        trigger
    );
    assert_eq!(bytes(&state), encoded);
    let foreign = controller
        .create_with_review_messages(
            owner("b"),
            "call".into(),
            spec(),
            vec!["另一个会话的原话".into()],
            1100,
        )
        .unwrap();
    assert_ne!(foreign.id, trigger.id);
    kernel.stop_all().await.unwrap();
    let (kernel, controller) = open(state).await;
    assert_eq!(controller.snapshot().unwrap(), vec![trigger, foreign]);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn versions_one_and_two_keep_missing_originals_missing_when_migrated() {
    for version in [1, 2] {
        let state = Arc::new(MemoryStateStore::default());
        let (kernel, controller) = open(state.clone()).await;
        let trigger = controller
            .create(owner("legacy"), "legacy-call".into(), spec(), 1000)
            .unwrap();
        assert!(trigger.review_messages.is_none());
        kernel.stop_all().await.unwrap();
        let mut old_trigger = serde_json::to_value(&trigger).unwrap();
        assert!(old_trigger.get("review_messages").is_none());
        if version == 1 {
            old_trigger.as_object_mut().unwrap().remove("runs");
        }
        state
            .set(
                &PluginId::new(PLUGIN_ID).unwrap(),
                STATE_KEY.into(),
                serde_json::to_vec(&serde_json::json!({
                    "format_version": version,
                    "triggers": [old_trigger],
                }))
                .unwrap(),
            )
            .unwrap();
        let (kernel, controller) = open(state.clone()).await;
        assert_eq!(controller.snapshot().unwrap(), vec![trigger.clone()]);
        let migrated: serde_json::Value = serde_json::from_slice(&bytes(&state)).unwrap();
        assert_eq!(migrated["format_version"], 3);
        assert!(migrated["triggers"][0].get("review_messages").is_none());
        // 相同幂等键不能用后来提供的授权给旧触发器补造证据。
        assert_eq!(
            controller
                .create_with_review_messages(
                    owner("legacy"),
                    "legacy-call".into(),
                    spec(),
                    originals(),
                    1100,
                )
                .unwrap(),
            trigger
        );
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn invalid_new_originals_never_change_the_committed_ledger() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, controller) = open(state.clone()).await;
    let trigger = controller
        .create_with_review_messages(owner("a"), "call".into(), spec(), originals(), 1000)
        .unwrap();
    let committed = bytes(&state);
    for invalid in [
        vec![],
        vec!["原話".into(); 7],
        vec!["x".repeat(64 * 1024)],
        vec!["\"".repeat(32 * 1024)],
    ] {
        assert_eq!(
            controller.create_with_review_messages(
                owner("a"),
                "new-call".into(),
                spec(),
                invalid,
                1100,
            ),
            Err(TriggerError::Invalid)
        );
        assert_eq!(bytes(&state), committed);
        assert_eq!(controller.snapshot().unwrap(), vec![trigger.clone()]);
    }
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn invalid_persisted_originals_fail_closed_without_clearing_the_data() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, controller) = open(state.clone()).await;
    let trigger = controller
        .create_with_review_messages(owner("a"), "call".into(), spec(), originals(), 1000)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let mut invalid_ledgers = vec![];
    for invalid in [
        vec![],
        vec!["原話".into(); 7],
        vec!["x".repeat(64 * 1024)],
        vec!["\"".repeat(32 * 1024)],
    ] {
        let mut invalid_trigger = trigger.clone();
        invalid_trigger.review_messages = Some(invalid);
        invalid_ledgers.push(serde_json::json!({
            "format_version": 3,
            "triggers": [invalid_trigger],
        }));
    }
    // 老格式从未冻结这一字段，不能允许降级版本标签绕过迁移语义。
    for version in [1, 2] {
        invalid_ledgers.push(serde_json::json!({
            "format_version": version,
            "triggers": [trigger.clone()],
        }));
    }
    for invalid in invalid_ledgers {
        let original = serde_json::to_vec(&invalid).unwrap();
        state
            .set(
                &PluginId::new(PLUGIN_ID).unwrap(),
                STATE_KEY.into(),
                original.clone(),
            )
            .unwrap();
        let kernel = Kernel::with_services(KernelServices {
            state: state.clone(),
            ..Default::default()
        });
        let plugin = TriggerPlugin::new().unwrap();
        let controller = plugin.controller();
        kernel.register(Box::new(plugin)).unwrap();
        assert!(kernel.start_all().await.is_err());
        assert_eq!(controller.snapshot(), Err(TriggerError::Unavailable));
        assert_eq!(bytes(&state), original);
    }
}

#[tokio::test]
async fn full_ledger_rejects_more_originals_without_trimming_or_eviction() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, controller) = open(state.clone()).await;
    let originals = vec!["完整授权范围".repeat(3000)];
    assert_eq!(validate_review_messages(&originals), Ok(()));
    let mut accepted = vec![];
    for index in 0..MAX_TRIGGERS {
        let prior = (!accepted.is_empty()).then(|| bytes(&state));
        match controller.create_with_review_messages(
            owner(&format!("capacity-{index}")),
            "call".into(),
            spec(),
            originals.clone(),
            1000,
        ) {
            Ok(trigger) => accepted.push(trigger),
            Err(TriggerError::Limit) => {
                assert_eq!(bytes(&state), prior.unwrap());
                break;
            }
            unexpected => panic!("unexpected creation result: {unexpected:?}"),
        }
    }
    assert!(!accepted.is_empty() && accepted.len() < MAX_TRIGGERS);
    assert_eq!(controller.snapshot().unwrap(), accepted);
    assert!(
        accepted
            .iter()
            .all(|trigger| trigger.review_messages.as_ref() == Some(&originals))
    );
    kernel.stop_all().await.unwrap();
    let (kernel, controller) = open(state).await;
    assert_eq!(controller.snapshot().unwrap(), accepted);
    kernel.stop_all().await.unwrap();
}
