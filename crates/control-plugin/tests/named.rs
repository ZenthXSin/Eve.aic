use eve_control_api::*;
use eve_control_plugin::ControlPlugin;
use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{LlmError, TurnEvent, TurnEventKind, TurnEventSink};
use eve_plugin_api::{Plugin, PluginId, ServiceId, ServiceRegistry};
use eve_session_api::{SessionInput, SessionKey};
use std::{sync::Arc, time::Duration};

const INTERNAL_PLUGIN: &str = "eve.cognition.control";
const INTERNAL_SERVICE: &str = "eve.cognition.control.service";

struct Runner(&'static str);
impl ControlRunner for Runner {
    fn run<'a>(&'a self, input: SessionInput, sink: &'a dyn TurnEventSink) -> RunFuture<'a> {
        Box::pin(async move {
            let failure = if input.text == "wait" {
                let _ = sink.closed().await;
                Some(RunFailure::Execution(LlmError::Cancelled))
            } else {
                sink.emit(TurnEvent {
                    turn_id: Some(1),
                    kind: TurnEventKind::SessionSaved,
                })
                .await
                .err()
                .map(RunFailure::Execution)
            };
            RunReport {
                turn_id: Some(1),
                commit: if failure.is_some() {
                    CommitState::Failed
                } else {
                    CommitState::Completed
                },
                text: Some(self.0.into()),
                transcript: None,
                started_tools: Some(0),
                tool_results: Vec::new(),
                failure,
            }
        })
    }
}

fn input(text: &str) -> ControlInput {
    ControlInput {
        session: SessionInput {
            key: SessionKey::new("same-session", "same-owner").unwrap(),
            text: text.into(),
        },
        task_id: "same-task".into(),
    }
}

fn service(registry: &dyn ServiceRegistry, name: &str) -> Arc<dyn ControlService> {
    registry
        .get(&ServiceId::new(name).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<ControlServiceHandle>()
        .unwrap()
        .0
        .clone()
}

async fn wait(control: &dyn ControlService, key: &GenerationKey) -> ControlReport {
    tokio::time::timeout(Duration::from_secs(3), control.wait(key))
        .await
        .unwrap()
        .unwrap()
}

fn setup() -> (Kernel, Arc<dyn ServiceRegistry>) {
    let backends = KernelServices::default();
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    kernel
        .register(Box::new(
            ControlPlugin::new(Arc::new(Runner("normal")), Vec::new()).unwrap(),
        ))
        .unwrap();
    kernel
        .register(Box::new(
            ControlPlugin::with_identity(
                INTERNAL_PLUGIN,
                INTERNAL_SERVICE,
                Arc::new(Runner("internal")),
                Vec::new(),
            )
            .unwrap(),
        ))
        .unwrap();
    (kernel, registry)
}

#[tokio::test]
async fn named_and_builtin_controllers_isolate_same_session_and_generation() {
    let (kernel, registry) = setup();
    kernel.start_all().await.unwrap();
    let normal = service(registry.as_ref(), CONTROL_SERVICE_ID);
    let internal = service(registry.as_ref(), INTERNAL_SERVICE);
    let a = normal
        .submit(input("finish"), Arc::new(DiscardControlEvents))
        .unwrap();
    let b = internal
        .submit(input("finish"), Arc::new(DiscardControlEvents))
        .unwrap();
    assert_eq!(a.generation, b.generation);
    assert_ne!(a.controller_epoch, b.controller_epoch);
    assert_eq!(
        wait(normal.as_ref(), &a).await.run.text.as_deref(),
        Some("normal")
    );
    assert_eq!(
        wait(internal.as_ref(), &b).await.run.text.as_deref(),
        Some("internal")
    );
    assert_eq!(internal.cancel(&a), Err(ControlError::StaleGeneration));
    assert_eq!(normal.cancel(&b), Err(ControlError::StaleGeneration));
    assert_eq!(normal.wait(&b).await, Err(ControlError::StaleGeneration));
    let event = ControlEvent {
        key: a.clone(),
        event: TurnEvent {
            turn_id: Some(1),
            kind: TurnEventKind::SessionSaved,
        },
    };
    assert!(normal.accepts(&event));
    assert!(!internal.accepts(&event));
    assert_eq!(normal.snapshot(&a.session).unwrap().unwrap().key, a);
    assert_eq!(internal.snapshot(&b.session).unwrap().unwrap().key, b);
    for (name, owner) in [
        (CONTROL_SERVICE_ID, CONTROL_PLUGIN_ID),
        (INTERNAL_SERVICE, INTERNAL_PLUGIN),
    ] {
        let entry = registry
            .get(&ServiceId::new(name).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(entry.owner, PluginId::new(owner).unwrap());
    }
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn stopping_and_restarting_named_controller_does_not_stop_builtin() {
    let (kernel, registry) = setup();
    kernel.start_all().await.unwrap();
    let normal = service(registry.as_ref(), CONTROL_SERVICE_ID);
    let internal = service(registry.as_ref(), INTERNAL_SERVICE);
    let a = normal
        .submit(input("wait"), Arc::new(DiscardControlEvents))
        .unwrap();
    let b = internal
        .submit(input("wait"), Arc::new(DiscardControlEvents))
        .unwrap();
    // Captured waits remain valid while the old controller is removed from discovery.
    let old_wait = internal.wait(&b);
    tokio::time::timeout(
        Duration::from_secs(3),
        kernel.stop(&PluginId::new(INTERNAL_PLUGIN).unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    let stopped = old_wait.await.unwrap();
    assert!(stopped.cancel_requested);
    assert_eq!(stopped.run.commit, CommitState::Failed);
    assert!(
        registry
            .get(&ServiceId::new(INTERNAL_SERVICE).unwrap())
            .unwrap()
            .is_none()
    );
    assert_eq!(
        internal.submit(input("finish"), Arc::new(DiscardControlEvents)),
        Err(ControlError::Unavailable)
    );
    assert!(
        !normal
            .snapshot(&a.session)
            .unwrap()
            .unwrap()
            .cancel_requested
    );
    kernel
        .start(&PluginId::new(INTERNAL_PLUGIN).unwrap())
        .await
        .unwrap();
    let restarted = service(registry.as_ref(), INTERNAL_SERVICE);
    let next = restarted
        .submit(input("finish"), Arc::new(DiscardControlEvents))
        .unwrap();
    assert_ne!(next.controller_epoch, b.controller_epoch);
    assert_eq!(restarted.cancel(&b), Err(ControlError::StaleGeneration));
    assert_eq!(
        wait(restarted.as_ref(), &next).await.run.commit,
        CommitState::Completed
    );
    normal.cancel(&a).unwrap();
    assert!(wait(normal.as_ref(), &a).await.cancel_requested);
    kernel.stop_all().await.unwrap();
    kernel
        .unregister(&PluginId::new(INTERNAL_PLUGIN).unwrap())
        .unwrap();
    kernel
        .unregister(&PluginId::new(CONTROL_PLUGIN_ID).unwrap())
        .unwrap();
}

#[tokio::test]
async fn conflicting_service_identity_cannot_replace_existing_owner() {
    let backends = KernelServices::default();
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    kernel
        .register(Box::new(
            ControlPlugin::new(Arc::new(Runner("original")), Vec::new()).unwrap(),
        ))
        .unwrap();
    kernel
        .start(&PluginId::new(CONTROL_PLUGIN_ID).unwrap())
        .await
        .unwrap();
    let original = service(registry.as_ref(), CONTROL_SERVICE_ID);
    kernel
        .register(Box::new(
            ControlPlugin::with_identity(
                INTERNAL_PLUGIN,
                CONTROL_SERVICE_ID,
                Arc::new(Runner("conflict")),
                Vec::new(),
            )
            .unwrap(),
        ))
        .unwrap();
    assert!(
        kernel
            .start(&PluginId::new(INTERNAL_PLUGIN).unwrap())
            .await
            .is_err()
    );
    let current = service(registry.as_ref(), CONTROL_SERVICE_ID);
    assert!(Arc::ptr_eq(&original, &current));
    assert_eq!(
        registry
            .get(&ServiceId::new(CONTROL_SERVICE_ID).unwrap())
            .unwrap()
            .unwrap()
            .owner,
        PluginId::new(CONTROL_PLUGIN_ID).unwrap()
    );
    let key = current
        .submit(input("finish"), Arc::new(DiscardControlEvents))
        .unwrap();
    assert_eq!(
        wait(current.as_ref(), &key).await.run.text.as_deref(),
        Some("original")
    );
    kernel.stop_all().await.unwrap();
}

#[test]
fn default_identity_is_compatible_and_named_identifiers_are_validated() {
    let default = ControlPlugin::new(Arc::new(Runner("normal")), Vec::new()).unwrap();
    assert_eq!(
        default.manifest().id,
        PluginId::new(CONTROL_PLUGIN_ID).unwrap()
    );
    assert!(
        ControlPlugin::with_identity(
            "",
            INTERNAL_SERVICE,
            Arc::new(Runner("invalid")),
            Vec::new()
        )
        .is_err()
    );
    assert!(
        ControlPlugin::with_identity(INTERNAL_PLUGIN, "", Arc::new(Runner("invalid")), Vec::new())
            .is_err()
    );
}

fn input_for(session: &str, user: &str, text: &str) -> ControlInput {
    ControlInput {
        session: SessionInput {
            key: SessionKey::new(session, user).unwrap(),
            text: text.into(),
        },
        task_id: format!("task-{session}"),
    }
}

#[tokio::test]
async fn lists_admitted_keys_before_runner_starts_with_bounded_owned_pages() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct DeferredRunner(Arc<AtomicUsize>);
    impl ControlRunner for DeferredRunner {
        fn run<'a>(&'a self, _: SessionInput, sink: &'a dyn TurnEventSink) -> RunFuture<'a> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                sink.closed().await.unwrap();
                RunReport {
                    turn_id: None,
                    commit: CommitState::NotStarted,
                    text: None,
                    transcript: None,
                    started_tools: Some(0),
                    tool_results: vec![],
                    failure: Some(RunFailure::Execution(LlmError::Cancelled)),
                }
            })
        }
    }
    let backends = KernelServices::default();
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    let calls = Arc::new(AtomicUsize::new(0));
    kernel
        .register(Box::new(
            ControlPlugin::new(Arc::new(DeferredRunner(calls.clone())), vec![]).unwrap(),
        ))
        .unwrap();
    kernel.start_all().await.unwrap();
    let control = service(registry.as_ref(), CONTROL_SERVICE_ID);
    assert!(control.list_keys(None, 100).unwrap().is_empty());
    let mut expected = Vec::new();
    // 当前线程不让出执行权：这些任务已准入，但 runner 与 Session 均未开始。
    for index in (0..103).rev() {
        let session = format!("session-{index:03}");
        let owner = format!("owner-{}", index % 7);
        let key = control
            .submit(
                input_for(&session, &owner, "不返回任务正文"),
                Arc::new(DiscardControlEvents),
            )
            .unwrap();
        expected.push(key.session);
    }
    expected.reverse();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(control.list_keys(None, 100).unwrap(), expected[..100]);
    assert_eq!(
        control.list_keys(Some("session-099"), 100).unwrap(),
        expected[100..]
    );
    assert!(
        control
            .list_keys(Some("session-102"), 1)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        control.list_keys(Some("session-050x"), 2).unwrap(),
        expected[51..53]
    );
    assert_eq!(control.list_keys(Some(" "), 1).unwrap(), expected[..1]);
    assert_eq!(
        control.list_keys(Some("session-050 "), 1).unwrap(),
        expected[51..52]
    );
    assert!(
        control
            .list_keys(Some(&"界".repeat(85)), 1)
            .unwrap()
            .is_empty()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    for after in [
        Some(""),
        Some("line\nbreak"),
        Some("delete\u{7f}"),
        Some("\u{85}"),
    ] {
        assert_eq!(control.list_keys(after, 1), Err(ControlError::InvalidInput));
    }
    for after in [&"x".repeat(257), &"界".repeat(86)] {
        assert_eq!(
            control.list_keys(Some(after), 1),
            Err(ControlError::InvalidInput)
        );
    }
    for limit in [0, 101, usize::MAX] {
        assert_eq!(
            control.list_keys(None, limit),
            Err(ControlError::InvalidInput)
        );
    }
    // 无效的跨所有者准入不能污染列举出来的可信身份。
    assert_eq!(
        control.submit(
            input_for("session-000", "wrong-owner", "伪造"),
            Arc::new(DiscardControlEvents)
        ),
        Err(ControlError::OwnerMismatch)
    );
    assert_eq!(control.list_keys(None, 1).unwrap(), expected[..1]);
    tokio::time::timeout(Duration::from_secs(3), kernel.stop_all())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(control.list_keys(None, 1), Err(ControlError::Unavailable));
    assert_eq!(
        control.list_keys(Some(""), 0),
        Err(ControlError::Unavailable)
    );
}

#[tokio::test]
async fn listed_keys_stay_with_their_controller_and_restart_does_not_restore_tasks() {
    let (kernel, registry) = setup();
    kernel.start_all().await.unwrap();
    let normal = service(registry.as_ref(), CONTROL_SERVICE_ID);
    let internal = service(registry.as_ref(), INTERNAL_SERVICE);
    let a = normal
        .submit(
            input_for("normal-only", "owner-normal", "finish"),
            Arc::new(DiscardControlEvents),
        )
        .unwrap();
    let b = internal
        .submit(
            input_for("internal-only", "owner-internal", "wait"),
            Arc::new(DiscardControlEvents),
        )
        .unwrap();
    wait(normal.as_ref(), &a).await;
    assert_eq!(
        normal.list_keys(None, 100).unwrap(),
        vec![a.session.clone()]
    );
    assert_eq!(
        internal.list_keys(None, 100).unwrap(),
        vec![b.session.clone()]
    );
    let normal_before = normal.snapshot(&a.session).unwrap();
    kernel
        .stop(&PluginId::new(INTERNAL_PLUGIN).unwrap())
        .await
        .unwrap();
    assert_eq!(internal.list_keys(None, 1), Err(ControlError::Unavailable));
    kernel
        .start(&PluginId::new(INTERNAL_PLUGIN).unwrap())
        .await
        .unwrap();
    let restarted = service(registry.as_ref(), INTERNAL_SERVICE);
    assert!(restarted.list_keys(None, 100).unwrap().is_empty());
    assert_eq!(normal.list_keys(None, 100).unwrap(), vec![a.session]);
    assert_eq!(
        normal
            .snapshot(&normal_before.as_ref().unwrap().key.session)
            .unwrap(),
        normal_before
    );
    kernel.stop_all().await.unwrap();
}
