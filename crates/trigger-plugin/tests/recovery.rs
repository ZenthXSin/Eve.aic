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
    SessionKey::new(session, "user").unwrap()
}
fn spec() -> TriggerSpec {
    TriggerSpec {
        title: "构建后继续".into(),
        instructions: "核对构建结果".into(),
        mode: MatchMode::All,
        conditions: vec![Condition::Time { at_ms: 2000 }],
        expires_at_ms: 1_000_000,
        recurrence: None,
        resume_task_id: None,
    }
}
#[tokio::test]
async fn idempotency_and_cancellation_are_bound_to_exact_session() {
    let (k, c) = open(Arc::default()).await;
    let a = c
        .create(owner("a"), "turn-1-call-1".into(), spec(), 1000)
        .unwrap();
    assert_eq!(
        c.create(owner("a"), "turn-1-call-1".into(), spec(), 1100)
            .unwrap(),
        a
    );
    assert_eq!(c.cancel(&owner("b"), &a.id), Err(TriggerError::NotFound));
    let b = c
        .create(owner("b"), "turn-1-call-1".into(), spec(), 1000)
        .unwrap();
    assert_ne!(a.id, b.id);
    c.cancel(&owner("a"), &a.id).unwrap();
    assert_eq!(
        c.ready(&a.id, vec!["time".into()]),
        Err(TriggerError::Transition)
    );
    k.stop_all().await.unwrap();
}
#[tokio::test]
async fn restart_preserves_waiting_and_never_replays_running_or_sending() {
    let state = Arc::new(MemoryStateStore::default());
    let (k, c) = open(state.clone()).await;
    let armed = c.create(owner("a"), "armed".into(), spec(), 1000).unwrap();
    let running = c
        .create(owner("a"), "running".into(), spec(), 1000)
        .unwrap();
    let sending = c
        .create(owner("a"), "sending".into(), spec(), 1000)
        .unwrap();
    for t in [&running, &sending] {
        c.ready(&t.id, vec!["time".into()]).unwrap();
        c.transition(&t.id, Status::Ready, Status::Running, None)
            .unwrap();
    }
    c.transition(
        &sending.id,
        Status::Running,
        Status::Completed,
        Some("完成".into()),
    )
    .unwrap();
    c.delivery(&sending.id, Delivery::Pending, Delivery::Sending)
        .unwrap();
    k.stop_all().await.unwrap();
    assert_eq!(c.snapshot(), Err(TriggerError::Unavailable));
    let (k, c) = open(state).await;
    let restored = c.snapshot().unwrap();
    assert_eq!(
        restored.iter().find(|t| t.id == armed.id).unwrap().status,
        Status::Armed
    );
    assert_eq!(
        restored.iter().find(|t| t.id == running.id).unwrap().status,
        Status::Interrupted
    );
    assert_eq!(
        restored
            .iter()
            .find(|t| t.id == sending.id)
            .unwrap()
            .delivery,
        Some(Delivery::Unknown)
    );
    assert_eq!(
        c.transition(&running.id, Status::Running, Status::Ready, None),
        Err(TriggerError::Transition)
    );
    k.stop_all().await.unwrap();
}
#[tokio::test]
async fn invalid_and_duplicate_key_state_is_preserved_and_rejected() {
    for bytes in [
        br#"{"format_version":1,"format_version":1,"triggers":[]}"#.to_vec(),
        br#"{"format_version":3,"triggers":[]}"#.to_vec(),
        b"broken".to_vec(),
    ] {
        let state = Arc::new(MemoryStateStore::default());
        let id = PluginId::new(PLUGIN_ID).unwrap();
        state.set(&id, STATE_KEY.into(), bytes.clone()).unwrap();
        let k = Kernel::with_services(KernelServices {
            state: state.clone(),
            ..Default::default()
        });
        k.register(Box::new(TriggerPlugin::new().unwrap())).unwrap();
        assert!(k.start_all().await.is_err());
        assert_eq!(state.get(&id, STATE_KEY).unwrap().unwrap(), bytes);
    }
}
#[tokio::test]
async fn capacity_rejection_never_evicts_active_or_terminal_evidence() {
    let (k, c) = open(Arc::default()).await;
    for i in 0..MAX_ACTIVE_PER_SESSION {
        c.create(owner("a"), format!("call-{i}"), spec(), 1000)
            .unwrap();
    }
    assert_eq!(
        c.create(owner("a"), "overflow".into(), spec(), 1000),
        Err(TriggerError::Limit)
    );
    assert_eq!(c.snapshot().unwrap().len(), MAX_ACTIVE_PER_SESSION);
    k.stop_all().await.unwrap();
}

#[tokio::test]
async fn lost_commit_confirmation_closes_handle_and_restart_never_replays() {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Uncertain {
        state: MemoryStateStore,
        lose: AtomicBool,
    }
    impl StateStore for Uncertain {
        fn get(
            &self,
            namespace: &PluginId,
            key: &str,
        ) -> eve_plugin_api::PluginResult<Option<Vec<u8>>> {
            self.state.get(namespace, key)
        }
        fn set(
            &self,
            namespace: &PluginId,
            key: String,
            value: Vec<u8>,
        ) -> eve_plugin_api::PluginResult<()> {
            self.state.set(namespace, key, value)?;
            if self.lose.load(Ordering::SeqCst) {
                Err(eve_plugin_api::PluginError::State(
                    "lost confirmation".into(),
                ))
            } else {
                Ok(())
            }
        }
    }
    let state = Arc::new(Uncertain {
        state: MemoryStateStore::default(),
        lose: AtomicBool::new(false),
    });
    let k = Kernel::with_services(KernelServices {
        state: state.clone(),
        ..Default::default()
    });
    let p = TriggerPlugin::new().unwrap();
    let c = p.controller();
    k.register(Box::new(p)).unwrap();
    k.start_all().await.unwrap();
    let t = c
        .create(owner("a"), "uncertain".into(), spec(), 1000)
        .unwrap();
    c.ready(&t.id, vec!["time".into()]).unwrap();
    state.lose.store(true, Ordering::SeqCst);
    assert_eq!(
        c.transition(&t.id, Status::Ready, Status::Running, None),
        Err(TriggerError::Storage)
    );
    assert_eq!(c.snapshot(), Err(TriggerError::Unavailable));
    k.stop_all().await.unwrap();
    state.lose.store(false, Ordering::SeqCst);
    let next = Kernel::with_services(KernelServices {
        state,
        ..Default::default()
    });
    let p = TriggerPlugin::new().unwrap();
    let c = p.controller();
    next.register(Box::new(p)).unwrap();
    next.start_all().await.unwrap();
    assert_eq!(c.snapshot().unwrap()[0].status, Status::Interrupted);
    next.stop_all().await.unwrap();
}

#[tokio::test]
async fn recurrence_is_bounded_and_unknown_delivery_never_rearms() {
    let (k, c) = open(Arc::default()).await;
    let mut recurring = spec();
    recurring.recurrence = Some(Recurrence {
        interval_ms: 60_000,
        max_runs: 2,
    });
    let t = c
        .create(owner("cycle"), "cycle".into(), recurring, 1000)
        .unwrap();
    for run in 1..=2 {
        c.ready(&t.id, vec!["time".into()]).unwrap();
        c.transition(&t.id, Status::Ready, Status::Running, None)
            .unwrap();
        c.transition(
            &t.id,
            Status::Running,
            Status::Completed,
            Some("已检查目标".into()),
        )
        .unwrap();
        assert_eq!(c.rearm(&t.id, 3000), Err(TriggerError::Transition));
        c.delivery_for_run(&t.id, run, Delivery::Pending, Delivery::Sending)
            .unwrap();
        assert_eq!(
            c.delivery_for_run(
                &t.id,
                run.saturating_sub(1),
                Delivery::Sending,
                Delivery::Sent
            ),
            Err(TriggerError::Transition)
        );
        c.delivery_for_run(&t.id, run, Delivery::Sending, Delivery::Sent)
            .unwrap();
        if run == 1 {
            c.rearm(&t.id, 3000).unwrap();
            let restored = c.snapshot().unwrap().remove(0);
            assert_eq!(
                restored.spec.conditions,
                vec![Condition::Time { at_ms: 63_000 }]
            );
        } else {
            assert_eq!(c.rearm(&t.id, 3000), Err(TriggerError::Transition));
        }
    }
    let mut uncertain_spec = spec();
    uncertain_spec.recurrence = Some(Recurrence {
        interval_ms: 60_000,
        max_runs: 2,
    });
    let uncertain = c
        .create(
            owner("unknown"),
            "unknown-cycle".into(),
            uncertain_spec,
            1000,
        )
        .unwrap();
    c.ready(&uncertain.id, vec!["time".into()]).unwrap();
    c.transition(&uncertain.id, Status::Ready, Status::Running, None)
        .unwrap();
    c.transition(
        &uncertain.id,
        Status::Running,
        Status::Completed,
        Some("检查完成，发送未确认".into()),
    )
    .unwrap();
    c.delivery(&uncertain.id, Delivery::Pending, Delivery::Sending)
        .unwrap();
    c.delivery(&uncertain.id, Delivery::Sending, Delivery::Unknown)
        .unwrap();
    assert_eq!(c.rearm(&uncertain.id, 3000), Err(TriggerError::Transition));
    let mut boundary = spec();
    boundary.expires_at_ms = 2000;
    assert_eq!(
        c.create(owner("boundary"), "equal".into(), boundary, 1000),
        Err(TriggerError::Invalid)
    );
    k.stop_all().await.unwrap();
}

// 两个真实操作系统进程依次读写 FileStateStore，验证半小时周期跨进程保存。
#[test]
fn recurring_schedule_survives_a_real_process_boundary() {
    let directory = tempfile::tempdir().unwrap();
    for phase in ["seed", "resume"] {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "recurrence_subprocess_helper", "--nocapture"])
            .env("EVE_TRIGGER_SIMULATION_STATE", directory.path())
            .env("EVE_TRIGGER_SIMULATION_PHASE", phase)
            .status()
            .unwrap();
        assert!(status.success(), "模拟子进程失败：{phase}");
    }
}
#[tokio::test]
async fn recurrence_subprocess_helper() {
    let Some(path) = std::env::var_os("EVE_TRIGGER_SIMULATION_STATE") else {
        return;
    };
    let state = Arc::new(
        eve_kernel::backends::FileStateStore::open(std::path::PathBuf::from(path)).unwrap(),
    );
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..Default::default()
    });
    let plugin = TriggerPlugin::new().unwrap();
    let c = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    if std::env::var("EVE_TRIGGER_SIMULATION_PHASE").unwrap() == "seed" {
        let mut recurring = spec();
        recurring.expires_at_ms = 7_200_000;
        recurring.recurrence = Some(Recurrence {
            interval_ms: 1_800_000,
            max_runs: 3,
        });
        let t = c
            .create(owner("simulation"), "periodic-goal".into(), recurring, 1000)
            .unwrap();
        c.ready(&t.id, vec!["模拟时钟达到2000".into()]).unwrap();
        c.transition(&t.id, Status::Ready, Status::Running, None)
            .unwrap();
        c.transition(
            &t.id,
            Status::Running,
            Status::Completed,
            Some("模拟检查完成".into()),
        )
        .unwrap();
        c.delivery(&t.id, Delivery::Pending, Delivery::Sending)
            .unwrap();
        c.delivery(&t.id, Delivery::Sending, Delivery::Sent)
            .unwrap();
        c.rearm(&t.id, 3000).unwrap();
    } else {
        let t = c.snapshot().unwrap().remove(0);
        assert_eq!(t.status, Status::Armed);
        assert_eq!(t.runs, 1);
        assert_eq!(
            t.spec.conditions,
            vec![Condition::Time { at_ms: 1_803_000 }]
        );
        assert_eq!(t.result.as_deref(), Some("模拟检查完成"));
    }
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn old_trigger_ledger_migrates_without_losing_armed_conditions() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, controller) = open(state.clone()).await;
    let trigger = controller
        .create(owner("legacy"), "legacy-source".into(), spec(), 1000)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let namespace = PluginId::new(PLUGIN_ID).unwrap();
    let mut legacy = serde_json::to_value(&trigger).unwrap();
    legacy.as_object_mut().unwrap().remove("runs");
    state
        .set(
            &namespace,
            STATE_KEY.into(),
            serde_json::to_vec(&serde_json::json!({"format_version":1,"triggers":[legacy]}))
                .unwrap(),
        )
        .unwrap();
    let (kernel, controller) = open(state.clone()).await;
    assert_eq!(controller.snapshot().unwrap(), vec![trigger]);
    let stored: serde_json::Value =
        serde_json::from_slice(&state.get(&namespace, STATE_KEY).unwrap().unwrap()).unwrap();
    assert_eq!(stored["format_version"], 2);
    kernel.stop_all().await.unwrap();
}
