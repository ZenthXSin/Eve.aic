use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_plugin_api::{PluginId, PluginResult, StateStore};
use eve_session_api::SessionKey;
use eve_trigger_api::*;
use eve_trigger_plugin::{TriggerController, TriggerPlugin};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

const SILENCE: &str = " {\"action\":\"silence\"}\n";

#[derive(Default)]
struct ObservedStore {
    state: MemoryStateStore,
    writes: Mutex<Vec<Vec<u8>>>,
    lose_confirmation: AtomicBool,
}
impl StateStore for ObservedStore {
    fn get(&self, owner: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.state.get(owner, key)
    }
    fn set(&self, owner: &PluginId, key: String, bytes: Vec<u8>) -> PluginResult<()> {
        self.state.set(owner, key, bytes.clone())?;
        self.writes.lock().unwrap().push(bytes);
        if self.lose_confirmation.load(Ordering::SeqCst) {
            Err(eve_plugin_api::PluginError::State(
                "fixture lost commit confirmation".into(),
            ))
        } else {
            Ok(())
        }
    }
}

async fn open(state: Arc<ObservedStore>) -> (Kernel, TriggerController) {
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

fn create_running(controller: &TriggerController, source: &str) -> Trigger {
    let trigger = controller
        .create_with_review_messages(
            SessionKey::new("qq:owned", "qq:owned").unwrap(),
            source.into(),
            TriggerSpec {
                title: "安静核验".into(),
                instructions: "检查两次，无需通知时可以保持安静".into(),
                mode: MatchMode::All,
                conditions: vec![Condition::Time { at_ms: 2000 }],
                expires_at_ms: 1_000_000,
                recurrence: Some(Recurrence {
                    interval_ms: 60_000,
                    max_runs: 2,
                }),
                resume_task_id: None,
            },
            vec!["稍后检查两次，没有新情况就不用告诉我".into()],
            1000,
        )
        .unwrap();
    controller.ready(&trigger.id, vec!["time".into()]).unwrap();
    controller
        .transition(&trigger.id, Status::Ready, Status::Running, None)
        .unwrap();
    controller
        .snapshot()
        .unwrap()
        .into_iter()
        .find(|item| item.id == trigger.id)
        .unwrap()
}

#[tokio::test]
async fn completion_and_silence_are_one_write_with_owner_run_and_terminal_cas() {
    let state = Arc::new(ObservedStore::default());
    let (kernel, controller) = open(state.clone()).await;
    let running = create_running(&controller, "atomic");
    state.writes.lock().unwrap().clear();
    let wrong_user = SessionKey::new(&running.owner.session_id, "other").unwrap();
    assert_eq!(
        controller.complete_silently_for_run(&wrong_user, &running.id, 1, SILENCE.into()),
        Err(TriggerError::NotFound)
    );
    for run in [0, 2] {
        assert_eq!(
            controller.complete_silently_for_run(&running.owner, &running.id, run, SILENCE.into()),
            Err(TriggerError::Transition)
        );
    }
    for invalid in [String::new(), "x".repeat(32769)] {
        assert_eq!(
            controller.complete_silently_for_run(&running.owner, &running.id, 1, invalid),
            Err(TriggerError::Invalid)
        );
    }
    assert!(state.writes.lock().unwrap().is_empty());
    controller
        .complete_silently_for_run(&running.owner, &running.id, 1, SILENCE.into())
        .unwrap();
    {
        let writes = state.writes.lock().unwrap();
        assert_eq!(writes.len(), 1);
        let ledger: serde_json::Value = serde_json::from_slice(&writes[0]).unwrap();
        assert_eq!(ledger["format_version"], 4);
        assert_eq!(ledger["triggers"][0]["status"], "Completed");
        assert_eq!(ledger["triggers"][0]["delivery"], "Silenced");
        assert_eq!(ledger["triggers"][0]["result"], SILENCE);
    }
    let complete = controller.snapshot().unwrap().remove(0);
    assert_eq!(complete.spec, running.spec);
    assert_eq!(complete.review_messages, running.review_messages);
    assert_eq!(complete.evidence, running.evidence);
    assert_eq!(complete.runs, 1);
    assert_eq!(
        controller.delivery_for_run(&running.id, 1, Delivery::Pending, Delivery::Sending),
        Err(TriggerError::Transition)
    );
    assert_eq!(
        controller.complete_silently_for_run(&running.owner, &running.id, 1, "overwrite".into()),
        Err(TriggerError::Transition)
    );
    assert_eq!(controller.snapshot().unwrap(), vec![complete.clone()]);
    kernel.stop_all().await.unwrap();

    let (kernel, controller) = open(state.clone()).await;
    assert_eq!(controller.snapshot().unwrap(), vec![complete]);
    controller.rearm(&running.id, 3000).unwrap();
    controller
        .ready(&running.id, vec!["time again".into()])
        .unwrap();
    controller
        .transition(&running.id, Status::Ready, Status::Running, None)
        .unwrap();
    let next = controller.snapshot().unwrap();
    assert_eq!(next[0].runs, 2);
    assert_eq!(
        controller.complete_silently_for_run(&running.owner, &running.id, 1, SILENCE.into()),
        Err(TriggerError::Transition)
    );
    assert_eq!(controller.snapshot().unwrap(), next);
    controller
        .complete_silently_for_run(&running.owner, &running.id, 2, SILENCE.into())
        .unwrap();
    assert_eq!(
        controller.rearm(&running.id, 64_000),
        Err(TriggerError::Transition)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn cancelled_or_existing_pending_results_cannot_be_reclassified_as_silence() {
    let state = Arc::new(ObservedStore::default());
    let (kernel, controller) = open(state.clone()).await;
    let cancelled = create_running(&controller, "cancelled");
    let pending = create_running(&controller, "legacy-pending");
    controller.cancel(&cancelled.owner, &cancelled.id).unwrap();
    controller
        .transition(
            &pending.id,
            Status::Running,
            Status::Completed,
            Some(SILENCE.into()),
        )
        .unwrap();
    let before = controller.snapshot().unwrap();
    state.writes.lock().unwrap().clear();
    for trigger in [&cancelled, &pending] {
        assert_eq!(
            controller.complete_silently_for_run(&trigger.owner, &trigger.id, 1, SILENCE.into()),
            Err(TriggerError::Transition)
        );
    }
    assert_eq!(controller.snapshot().unwrap(), before);
    assert!(state.writes.lock().unwrap().is_empty());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn lost_confirmation_restores_completed_silence_without_a_pending_window() {
    let state = Arc::new(ObservedStore::default());
    let (kernel, controller) = open(state.clone()).await;
    let running = create_running(&controller, "uncertain-commit");
    state.writes.lock().unwrap().clear();
    state.lose_confirmation.store(true, Ordering::SeqCst);
    assert_eq!(
        controller.complete_silently_for_run(&running.owner, &running.id, 1, SILENCE.into()),
        Err(TriggerError::Storage)
    );
    assert_eq!(controller.snapshot(), Err(TriggerError::Unavailable));
    assert_eq!(state.writes.lock().unwrap().len(), 1);
    kernel.stop_all().await.unwrap();
    state.lose_confirmation.store(false, Ordering::SeqCst);
    let (kernel, controller) = open(state.clone()).await;
    let restored = controller.snapshot().unwrap().remove(0);
    assert_eq!(restored.status, Status::Completed);
    assert_eq!(restored.delivery, Some(Delivery::Silenced));
    assert_eq!(restored.result.as_deref(), Some(SILENCE));
    assert_eq!(restored.review_messages, running.review_messages);
    assert_eq!(state.writes.lock().unwrap().len(), 1);
    kernel.stop_all().await.unwrap();
}
