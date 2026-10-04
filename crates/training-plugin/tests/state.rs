use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_llm_api::{ContextAssembler, ContextScope, TurnInput};
use eve_plugin_api::*;
use eve_training_api::*;
use eve_training_plugin::{TrainingContext, TrainingPlugin};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct FallibleState {
    storage: MemoryStateStore,
    fail: AtomicBool,
}
impl StateStore for FallibleState {
    fn get(&self, owner: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.storage.get(owner, key)
    }
    fn set(&self, owner: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(PluginError::State("test failure".into()));
        }
        self.storage.set(owner, key, value)
    }
}
fn scope(session: &str, user: &str) -> ContextScope {
    ContextScope {
        session_id: session.into(),
        user_id: user.into(),
    }
}
fn service(registry: &Arc<dyn ServiceRegistry>) -> Arc<dyn TrainingService> {
    registry
        .get(&ServiceId::new(TRAINING_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<TrainingServiceHandle>()
        .unwrap()
        .0
        .clone()
}

#[tokio::test]
async fn storage_failure_preserves_mode_and_stopped_handles_fail() {
    let state = Arc::new(FallibleState {
        storage: MemoryStateStore::default(),
        fail: AtomicBool::new(false),
    });
    let backends = KernelServices {
        state: state.clone(),
        ..KernelServices::default()
    };
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    kernel
        .register(Box::new(TrainingPlugin::new(false).unwrap()))
        .unwrap();
    let id = PluginId::new(TRAINING_PLUGIN_ID).unwrap();
    kernel.start(&id).await.unwrap();
    let old = service(&registry);
    let one = scope("group-1", "user-1");
    state.fail.store(true, Ordering::SeqCst);
    assert!(old.set_enabled(&one, true).is_err());
    assert!(!old.enabled(&one).unwrap());
    state.fail.store(false, Ordering::SeqCst);
    old.set_enabled(&one, true).unwrap();
    assert!(!old.enabled(&scope("group-1", "user-2")).unwrap());
    let context = TrainingContext(old.clone());
    assert!(
        context
            .assemble(TurnInput {
                text: "/train start user-1".into()
            })
            .await
            .unwrap()
            .memories
            .is_empty()
    );
    assert!(
        !context
            .assemble_scoped(
                TurnInput {
                    text: "答案".into()
                },
                Some(one.clone())
            )
            .await
            .unwrap()
            .memories
            .is_empty()
    );
    kernel.stop(&id).await.unwrap();
    assert!(old.enabled(&one).is_err());
    assert!(
        context
            .assemble_scoped(
                TurnInput {
                    text: "答案".into()
                },
                Some(one.clone())
            )
            .await
            .is_err()
    );
    kernel.start(&id).await.unwrap();
    assert!(service(&registry).enabled(&one).unwrap());
    assert!(old.set_enabled(&one, false).is_err());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn capacity_rejects_new_scope_without_evicting_existing_stop() {
    let backends = KernelServices::default();
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    kernel
        .register(Box::new(TrainingPlugin::new(true).unwrap()))
        .unwrap();
    kernel
        .start(&PluginId::new(TRAINING_PLUGIN_ID).unwrap())
        .await
        .unwrap();
    let modes = service(&registry);
    for i in 0..256 {
        modes
            .set_enabled(&scope(&format!("s-{i}"), "u"), false)
            .unwrap();
    }
    assert!(modes.set_enabled(&scope("overflow", "u"), false).is_err());
    assert!(!modes.enabled(&scope("s-0", "u")).unwrap());
    modes.set_enabled(&scope("s-0", "u"), true).unwrap();
    assert!(modes.enabled(&scope("s-0", "u")).unwrap());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn expressions_persist_deduplicate_reset_and_preserve_state_on_failure() {
    let state = Arc::new(FallibleState {
        storage: MemoryStateStore::default(),
        fail: AtomicBool::new(false),
    });
    let backends = KernelServices {
        state: state.clone(),
        ..KernelServices::default()
    };
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    let id = PluginId::new(TRAINING_PLUGIN_ID).unwrap();
    kernel
        .register(Box::new(TrainingPlugin::new(true).unwrap()))
        .unwrap();
    kernel.start(&id).await.unwrap();
    let old = service(&registry);
    let one = scope("group-1", "user-1");
    old.observe_user_message(&one, "m-1", "今天下雨了").unwrap();
    let first = old.expression_snapshot(&one).unwrap().unwrap();
    old.observe_user_message(&one, "m-1", "今天下雨了").unwrap();
    assert_eq!(old.expression_snapshot(&one).unwrap(), Some(first.clone()));
    assert!(old.observe_user_message(&one, "m-1", "冲突原文").is_err());
    assert_eq!(
        old.expression_snapshot(&scope("group-1", "user-2"))
            .unwrap(),
        None
    );
    state.fail.store(true, Ordering::SeqCst);
    assert!(old.observe_user_message(&one, "m-2", "天气如何？").is_err());
    assert!(old.reset_expression(&one).is_err());
    assert_eq!(old.expression_snapshot(&one).unwrap(), Some(first));
    state.fail.store(false, Ordering::SeqCst);
    old.observe_user_message(&one, "m-2", "天气如何？").unwrap();
    old.set_enabled(&one, false).unwrap();
    old.observe_user_message(&one, "off", "停止后不学习")
        .unwrap();
    assert_eq!(old.expression_snapshot(&one).unwrap().unwrap().samples, 2);
    old.set_enabled(&one, true).unwrap();
    old.observe_user_message(&one, "off", "停止后不学习")
        .unwrap();
    assert_eq!(old.expression_snapshot(&one).unwrap().unwrap().samples, 2);
    old.reset_expression(&one).unwrap();
    old.observe_user_message(&one, "m-1", "今天下雨了").unwrap();
    assert_eq!(old.expression_snapshot(&one).unwrap(), None);
    old.observe_user_message(&one, "m-3", "新的示范").unwrap();
    let saved = old.expression_snapshot(&one).unwrap();
    let bytes = state.get(&id, "expression.v1").unwrap().unwrap();
    assert!(!String::from_utf8(bytes).unwrap().contains("今天下雨了"));
    kernel.stop(&id).await.unwrap();
    assert!(
        old.observe_user_message(&one, "stopped", "不应学习")
            .is_err()
    );
    assert!(old.expression_baseline().is_err());
    assert!(old.reset_expression(&one).is_err());
    kernel.start(&id).await.unwrap();
    let restored = service(&registry);
    restored
        .observe_user_message(&one, "m-2", "天气如何？")
        .unwrap();
    assert_eq!(restored.expression_snapshot(&one).unwrap(), saved);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn expression_features_exclude_commands_quoted_material_and_freeze_scoped_context() {
    let backends = KernelServices::default();
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    kernel
        .register(Box::new(TrainingPlugin::new(true).unwrap()))
        .unwrap();
    kernel
        .start(&PluginId::new(TRAINING_PLUGIN_ID).unwrap())
        .await
        .unwrap();
    let learner = service(&registry);
    let one = scope("s-1", "u-1");
    let two = scope("s-2", "u-2");
    for (i, text) in [
        "/train start",
        "<@bot> /train stats",
        "```text\n引用\n```",
        "https://example.com",
        "> 引用文字",
        &"长".repeat(513),
    ]
    .iter()
    .enumerate()
    {
        learner
            .observe_user_message(&one, &format!("skip-{i}"), text)
            .unwrap();
    }
    assert_eq!(learner.expression_snapshot(&one).unwrap(), None);
    let context = TrainingContext(learner.clone());
    for i in 0..7 {
        learner
            .observe_user_message(&one, &format!("a-{i}"), "<@bot> 好呀")
            .unwrap();
    }
    let input = || TurnInput {
        text: "详细解释一个复杂任务".into(),
    };
    assert!(
        context
            .assemble_scoped(input(), Some(one.clone()))
            .await
            .unwrap()
            .profile
            .is_empty()
    );
    learner
        .observe_user_message(&one, "a-7", "请问今天怎么样？\n\n我想出去")
        .unwrap();
    let stats = learner.expression_snapshot(&one).unwrap().unwrap();
    assert_eq!(stats.samples, 8);
    assert_eq!(stats.median_chars, 2);
    assert_eq!(stats.short_percent, 100);
    assert_eq!(stats.single_paragraph_percent, 87);
    assert_eq!(stats.question_percent, 12);
    let frozen = context
        .assemble_scoped(input(), Some(one.clone()))
        .await
        .unwrap();
    assert!(frozen.revision.starts_with("eve-training-3:"));
    assert!(frozen.profile.contains("当前可信会话"));
    assert!(!frozen.profile.contains("我想出去"));
    let fallback = context
        .assemble_scoped(input(), Some(two.clone()))
        .await
        .unwrap();
    assert!(fallback.profile.contains("一般表达示范"));
    assert_eq!(learner.expression_snapshot(&two).unwrap(), None);
    learner.observe_user_message(&one, "a-8", "新消息").unwrap();
    assert_ne!(
        context
            .assemble_scoped(input(), Some(one))
            .await
            .unwrap()
            .revision,
        frozen.revision
    );
    assert!(frozen.profile.contains("有效消息 8 条"));
    assert!(context.assemble(input()).await.unwrap().profile.is_empty());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn corrupted_expression_state_refuses_start_without_clearing() {
    for bytes in [
        br#"{"version":99,"rows":[]}"#.to_vec(),
        br#"{"version":1,"rows":[],"unknown":true}"#.to_vec(),
        b"invalid".to_vec(),
    ] {
        let state = Arc::new(MemoryStateStore::default());
        let id = PluginId::new(TRAINING_PLUGIN_ID).unwrap();
        state
            .set(&id, "expression.v1".into(), bytes.clone())
            .unwrap();
        let kernel = Kernel::with_services(KernelServices {
            state: state.clone(),
            ..KernelServices::default()
        });
        kernel
            .register(Box::new(TrainingPlugin::new(true).unwrap()))
            .unwrap();
        assert!(kernel.start(&id).await.is_err());
        assert_eq!(state.get(&id, "expression.v1").unwrap(), Some(bytes));
    }
}
