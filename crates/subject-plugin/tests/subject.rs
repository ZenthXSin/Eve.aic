use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_plugin_api::*;
use eve_subject_api::*;
use eve_subject_plugin::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

fn plugin_id() -> PluginId {
    PluginId::new(SUBJECT_PLUGIN_ID).unwrap()
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
async fn start(state: Arc<dyn StateStore>) -> (Kernel, SubjectController) {
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let plugin = SubjectPlugin::new().unwrap();
    let controller = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start(&plugin_id()).await.unwrap();
    (kernel, controller)
}
fn scope(conversation: &str, principal: &str) -> SubjectScope {
    SubjectScope {
        channel: "qq".into(),
        conversation_id: conversation.into(),
        principal_id: principal.into(),
    }
}
fn input(
    conversation: &str,
    principal: &str,
    key: &str,
    priority: EventPriority,
) -> SubjectEventInput {
    let source = match priority {
        EventPriority::Stop | EventPriority::User => EventSource::UserIngress,
        EventPriority::Recovery => EventSource::RecoverySupervisor,
        EventPriority::Autonomy => EventSource::AutonomyScheduler,
    };
    SubjectEventInput {
        scope: scope(conversation, principal),
        source,
        dedupe_key: key.into(),
        priority,
        kind: "test".into(),
        payload: format!("{principal} 的私有正文"),
    }
}
fn token(event: &SubjectEvent) -> LeaseToken {
    event.lease.as_ref().unwrap().token.clone()
}

#[tokio::test]
async fn one_subject_prioritizes_events_without_merging_windows_or_principals() {
    let (_kernel, admin) = start(Arc::new(MemoryStateStore::default())).await;
    let t = now();
    for (key, priority) in [
        ("autonomy", EventPriority::Autonomy),
        ("recovery", EventPriority::Recovery),
        ("user", EventPriority::User),
        ("stop", EventPriority::Stop),
    ] {
        admin
            .enqueue(input("group-a", "alice", key, priority), t)
            .unwrap();
    }
    admin
        .observe_completed(input("group-b", "alice", "user", EventPriority::User), t)
        .unwrap();
    admin
        .observe_completed(input("group-a", "bob", "user", EventPriority::User), t)
        .unwrap();
    let mine = admin.snapshot_for(&scope("group-a", "alice")).unwrap();
    assert_eq!(mine.events.len(), 4);
    assert!(
        mine.events
            .iter()
            .all(|e| e.input.payload == "alice 的私有正文")
    );
    for priority in [
        EventPriority::Stop,
        EventPriority::User,
        EventPriority::Recovery,
        EventPriority::Autonomy,
    ] {
        let event = admin.claim_next("worker", t, 1000).unwrap().unwrap();
        assert_eq!(event.input.priority, priority);
        assert_eq!(admin.claim_next("other-worker", t, 1000).unwrap(), None);
        admin
            .settle(&token(&event), SubjectOutcome::Completed("完成".into()), t)
            .unwrap();
    }
    assert!(admin.claim_next("worker", t, 1000).unwrap().is_none());
    let mut invalid = input("group-a", "alice", "forged", EventPriority::User);
    invalid.source = EventSource::AutonomyScheduler;
    assert_eq!(admin.enqueue(invalid, t), Err(SubjectError::InvalidInput));
}

#[tokio::test]
async fn cancellation_is_scoped_and_high_priority_preemption_is_cooperative() {
    let (_kernel, admin) = start(Arc::new(MemoryStateStore::default())).await;
    let t = now();
    admin
        .enqueue(input("a", "alice", "work", EventPriority::Autonomy), t)
        .unwrap();
    let running = admin.claim_next("w", t, 1000).unwrap().unwrap();
    admin
        .enqueue(input("b", "bob", "urgent", EventPriority::User), t + 1)
        .unwrap();
    let current = admin
        .snapshot_for(&scope("a", "alice"))
        .unwrap()
        .events
        .pop()
        .unwrap();
    assert!(current.interrupt_requested);
    assert_eq!(
        admin.renew(&token(&running), t + 1, 1000),
        Err(SubjectError::InvalidTransition)
    );
    admin.yield_work(&token(&running), t + 1).unwrap();
    let next = admin.claim_next("w", t + 1, 1000).unwrap().unwrap();
    assert_eq!(next.input.scope, scope("b", "bob"));
    assert_eq!(admin.cancel_scope(&scope("a", "alice"), t + 2).unwrap(), 1);
    assert!(!admin.snapshot_for(&scope("b", "bob")).unwrap().events[0].cancel_requested);
    assert_eq!(
        admin.settle(&token(&running), SubjectOutcome::Cancelled, t + 2),
        Err(SubjectError::LeaseLost)
    );
}

#[tokio::test]
async fn expired_lease_is_fenced_and_effect_unknown_is_never_replayed() {
    let (_kernel, admin) = start(Arc::new(MemoryStateStore::default())).await;
    let t = now();
    admin
        .enqueue(input("a", "alice", "safe", EventPriority::User), t)
        .unwrap();
    let old = admin.claim_next("old", t, 10).unwrap().unwrap();
    let recovered = admin.claim_next("new", t + 10, 100).unwrap().unwrap();
    assert_eq!(recovered.id, old.id);
    assert_eq!(recovered.recoveries, 1);
    assert_eq!(
        admin.checkpoint(&token(&old), SubjectCheckpoint::default(), t + 10),
        Err(SubjectError::LeaseLost)
    );
    admin
        .checkpoint(
            &token(&recovered),
            SubjectCheckpoint {
                boundary: CheckpointBoundary::EffectPending,
                resume_data: "tool call persisted".into(),
            },
            t + 11,
        )
        .unwrap();
    assert_eq!(
        admin.settle(
            &token(&recovered),
            SubjectOutcome::Completed("猜测完成".into()),
            t + 11
        ),
        Err(SubjectError::InvalidTransition)
    );
    assert_eq!(admin.claim_next("new", t + 110, 100).unwrap(), None);
    let unknown = admin
        .snapshot_for(&scope("a", "alice"))
        .unwrap()
        .events
        .pop()
        .unwrap();
    assert_eq!(unknown.status, EventStatus::Unknown);
    assert_eq!(
        admin
            .enqueue(input("a", "alice", "safe", EventPriority::User), t + 111)
            .unwrap()
            .status,
        EventStatus::Unknown
    );
    assert_eq!(
        admin.resolve_unknown(
            &scope("b", "alice"),
            &unknown.id,
            SubjectOutcome::Cancelled,
            t + 111
        ),
        Err(SubjectError::NotFound)
    );
    admin
        .resolve_unknown(
            &scope("a", "alice"),
            &unknown.id,
            SubjectOutcome::Completed("人工核对已完成".into()),
            t + 111,
        )
        .unwrap();
}

#[tokio::test]
async fn restart_recovers_only_safe_checkpoints_and_stops_after_budget() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, admin) = start(state.clone()).await;
    let t = now();
    admin
        .enqueue(input("a", "alice", "event", EventPriority::User), t)
        .unwrap();
    let event = admin.claim_next("worker", t, 1000).unwrap().unwrap();
    admin
        .checkpoint(
            &token(&event),
            SubjectCheckpoint {
                boundary: CheckpointBoundary::Safe,
                resume_data: "已完成第一步".into(),
            },
            t,
        )
        .unwrap();
    kernel.stop(&plugin_id()).await.unwrap();
    assert_eq!(
        admin.snapshot_for(&scope("a", "alice")),
        Err(SubjectError::Unavailable)
    );
    let (_kernel, admin) = start(state.clone()).await;
    let saved = admin
        .snapshot_for(&scope("a", "alice"))
        .unwrap()
        .events
        .pop()
        .unwrap();
    assert_eq!(saved.status, EventStatus::Queued);
    assert_eq!(saved.checkpoint.resume_data, "已完成第一步");
    let mut t = now().max(saved.updated_at_ms);
    for _ in 1..MAX_RECOVERIES {
        admin.claim_next("worker", t, 1).unwrap().unwrap();
        t += 1;
    }
    admin.claim_next("worker", t, 1).unwrap().unwrap();
    assert_eq!(admin.claim_next("worker", t + 1, 1).unwrap(), None);
    assert_eq!(
        admin.snapshot_for(&scope("a", "alice")).unwrap().events[0].status,
        EventStatus::Failed
    );
}

#[tokio::test]
async fn focus_checks_are_persistent_half_hourly_coalesced_and_do_not_notify() {
    let (_kernel, admin) = start(Arc::new(MemoryStateStore::default())).await;
    let t = now();
    let focus = SubjectFocus {
        id: "goal-a".into(),
        scope: scope("a", "alice"),
        title: "学习地图编辑".into(),
        goal: "有实质进展才提议交流".into(),
        task_id: Some("task-1".into()),
        status: FocusStatus::Active,
        next_check_at_ms: t,
        updated_at_ms: t,
    };
    let focus = admin.upsert_focus(focus, t).unwrap();
    assert_eq!(focus.next_check_at_ms, t + AUTONOMY_INTERVAL_MS);
    assert_eq!(admin.schedule_due(t + AUTONOMY_INTERVAL_MS - 1).unwrap(), 0);
    assert_eq!(admin.schedule_due(t + AUTONOMY_INTERVAL_MS).unwrap(), 1);
    assert_eq!(
        admin.schedule_due(t + 2 * AUTONOMY_INTERVAL_MS).unwrap(),
        0,
        "未处理检查不堆积"
    );
    assert!(
        admin
            .snapshot_for(&scope("b", "alice"))
            .unwrap()
            .foci
            .is_empty()
    );
    let snapshot = admin.snapshot_for(&scope("a", "alice")).unwrap();
    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.events[0].input.payload, "goal-a");
    assert_eq!(snapshot.events[0].input.priority, EventPriority::Autonomy);
    admin
        .cancel_scope(&scope("a", "alice"), t + 2 * AUTONOMY_INTERVAL_MS)
        .unwrap();
    assert_eq!(admin.schedule_due(t + 3 * AUTONOMY_INTERVAL_MS).unwrap(), 0);
}

#[tokio::test]
async fn active_and_unknown_capacity_is_never_silently_evicted_and_dedupe_survives_compaction() {
    let (_kernel, admin) = start(Arc::new(MemoryStateStore::default())).await;
    let t = now();
    for n in 0..MAX_EVENTS {
        admin
            .enqueue(
                input("a", "alice", &format!("event-{n}"), EventPriority::User),
                t,
            )
            .unwrap();
    }
    assert_eq!(
        admin.enqueue(input("a", "alice", "overflow", EventPriority::User), t),
        Err(SubjectError::LimitReached)
    );
    let event = admin.claim_next("w", t, 1000).unwrap().unwrap();
    admin
        .settle(&token(&event), SubjectOutcome::Completed("完成".into()), t)
        .unwrap();
    admin
        .enqueue(input("a", "alice", "overflow", EventPriority::User), t + 1)
        .unwrap();
    assert_eq!(
        admin.enqueue(event.input, t + 1),
        Err(SubjectError::InvalidTransition),
        "去重墓碑不能重建已执行事件"
    );
    assert_eq!(
        admin
            .snapshot_for(&scope("a", "alice"))
            .unwrap()
            .events
            .len(),
        MAX_EVENTS
    );
}

#[tokio::test]
async fn corrupted_duplicate_or_future_state_refuses_start_without_clearing_data() {
    for bytes in [b"broken".to_vec(), br#"{"format_version":1,"format_version":1}"#.to_vec(), br#"{"format_version":99}"#.to_vec(), br#"{"format_version":1,"next":0,"generation":0,"events":[],"foci":[],"dedupe":[{"scope":{"channel":"qq","conversation_id":"x","principal_id":"a"},"source":"user_ingress","key":"x","event_id":"subject-1","at_ms":1}]}"#.to_vec()] {
        let state = Arc::new(MemoryStateStore::default());
        state.set(&plugin_id(), SUBJECT_STATE_KEY.into(), bytes.clone()).unwrap();
        let kernel = Kernel::with_services(KernelServices { state: state.clone(), ..KernelServices::default() });
        kernel.register(Box::new(SubjectPlugin::new().unwrap())).unwrap();
        assert!(kernel.start(&plugin_id()).await.is_err());
        assert_eq!(state.get(&plugin_id(), SUBJECT_STATE_KEY).unwrap(), Some(bytes));
    }
}

struct Faulty {
    base: MemoryStateStore,
    failure: AtomicU8,
}
impl StateStore for Faulty {
    fn get(&self, ns: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.base.get(ns, key)
    }
    fn set(&self, ns: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        let mode = self.failure.load(Ordering::SeqCst);
        if mode != 1 {
            self.base.set(ns, key, value)?;
        }
        if mode == 0 {
            Ok(())
        } else {
            Err(PluginError::State("uncertain write".into()))
        }
    }
}
#[tokio::test]
async fn uncertain_commits_close_all_handles_and_restart_reads_actual_storage() {
    for mode in [1, 2] {
        let state = Arc::new(Faulty {
            base: MemoryStateStore::default(),
            failure: AtomicU8::new(0),
        });
        let (kernel, admin) = start(state.clone()).await;
        let t = now();
        admin
            .observe_completed(input("a", "alice", "first", EventPriority::User), t)
            .unwrap();
        state.failure.store(mode, Ordering::SeqCst);
        assert_eq!(
            admin.enqueue(input("a", "alice", "second", EventPriority::User), t),
            Err(SubjectError::Storage)
        );
        assert_eq!(
            admin.snapshot_for(&scope("a", "alice")),
            Err(SubjectError::Unavailable)
        );
        kernel.stop(&plugin_id()).await.unwrap();
        state.failure.store(0, Ordering::SeqCst);
        let (_kernel, admin) = start(state).await;
        assert_eq!(
            admin
                .snapshot_for(&scope("a", "alice"))
                .unwrap()
                .events
                .len(),
            if mode == 1 { 1 } else { 2 }
        );
    }
}

#[tokio::test]
async fn driver_dispatches_multiple_windows_without_waiting_for_background_tasks() {
    let (_kernel, admin) = start(Arc::new(MemoryStateStore::default())).await;
    let driver = SubjectDriver::new(Arc::new(admin.clone()), "subject-host").unwrap();
    let t = now();
    admin
        .enqueue(input("a", "alice", "user-a", EventPriority::User), t)
        .unwrap();
    admin
        .enqueue(input("b", "bob", "user-b", EventPriority::User), t)
        .unwrap();
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut started = Vec::new();
    for _ in 0..2 {
        let tx = done_tx.clone();
        let event = driver
            .run_one(t, |event| {
                let task_id = format!("task-{}", event.id);
                started.push(task_id.clone());
                let scope = event.input.scope.clone();
                let completed_id = task_id.clone();
                tokio::spawn(async move {
                    tx.send((scope, completed_id)).unwrap();
                });
                Ok(SubjectDispatch::Started {
                    task_id,
                    title: "独立任务".into(),
                    goal: "各自完成，不拼接原文".into(),
                })
            })
            .unwrap()
            .unwrap();
        assert_eq!(event.status, EventStatus::Completed);
    }
    assert_eq!(
        started.len(),
        2,
        "两个窗口的后台任务均已派发，全局lease没有等待长执行"
    );
    for _ in 0..2 {
        let (scope, task_id) = done_rx.recv().await.unwrap();
        admin
            .enqueue(
                SubjectEventInput {
                    scope,
                    source: EventSource::TaskExecutor,
                    dedupe_key: format!("done:{task_id}"),
                    priority: EventPriority::Recovery,
                    kind: "task_completed".into(),
                    payload: task_id,
                },
                t + 1,
            )
            .unwrap();
        driver
            .run_one(t + 1, |event| {
                assert_eq!(event.input.kind, "task_completed");
                Ok(SubjectDispatch::Completed {
                    summary: "已处理进展；由原范围决定是否通知".into(),
                })
            })
            .unwrap();
    }
    for (window, user) in [("a", "alice"), ("b", "bob")] {
        let snapshot = admin.snapshot_for(&scope(window, user)).unwrap();
        assert_eq!(snapshot.foci.len(), 1);
        assert_eq!(snapshot.events.len(), 2);
        assert!(
            snapshot
                .events
                .iter()
                .all(|e| e.status == EventStatus::Completed)
        );
    }
}

#[tokio::test]
async fn driver_unknown_dispatch_does_not_retry_external_side_effect() {
    let (_kernel, admin) = start(Arc::new(MemoryStateStore::default())).await;
    let driver = SubjectDriver::new(Arc::new(admin.clone()), "subject-host")
        .unwrap()
        .with_lease_ms(1)
        .unwrap();
    let t = now();
    admin
        .enqueue(input("a", "alice", "event", EventPriority::User), t)
        .unwrap();
    let mut calls = 0;
    assert_eq!(
        driver.run_one(t, |_| {
            calls += 1;
            Err(SubjectError::Storage)
        }),
        Err(SubjectError::Storage)
    );
    assert_eq!(
        driver
            .run_one(t + 1, |_| {
                calls += 1;
                unreachable!("unknown dispatch must not replay")
            })
            .unwrap(),
        None
    );
    assert_eq!(calls, 1);
    assert_eq!(
        admin.snapshot_for(&scope("a", "alice")).unwrap().events[0].status,
        EventStatus::Unknown
    );
    admin
        .enqueue(input("b", "bob", "next", EventPriority::User), t + 2)
        .unwrap();
    assert!(
        driver
            .run_one(t + 2, |_| Ok(SubjectDispatch::Completed {
                summary: "其他范围继续推进".into()
            }))
            .unwrap()
            .is_some(),
        "明确未知后立即释放主体调度，不阻塞其他窗口"
    );
}

struct FailNth {
    base: MemoryStateStore,
    writes: AtomicUsize,
    fail_at: AtomicUsize,
}
impl StateStore for FailNth {
    fn get(&self, ns: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.base.get(ns, key)
    }
    fn set(&self, ns: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        let nth = self.writes.fetch_add(1, Ordering::SeqCst) + 1;
        if nth == self.fail_at.load(Ordering::SeqCst) {
            return Err(PluginError::State(
                "interrupted between receipt and settlement".into(),
            ));
        }
        self.base.set(ns, key, value)
    }
}

#[tokio::test]
async fn driver_restart_settles_saved_dispatch_receipt_without_repeating_external_start() {
    let state = Arc::new(FailNth {
        base: MemoryStateStore::default(),
        writes: AtomicUsize::new(0),
        fail_at: AtomicUsize::new(6),
    });
    let (kernel, admin) = start(state.clone()).await;
    let driver = SubjectDriver::new(Arc::new(admin.clone()), "host").unwrap();
    let t = now();
    admin
        .enqueue(input("a", "alice", "start", EventPriority::User), t)
        .unwrap();
    let mut starts = 0;
    assert_eq!(
        driver.run_one(t, |_| {
            starts += 1;
            Ok(SubjectDispatch::Started {
                task_id: "task-persisted".into(),
                title: "已派发任务".into(),
                goal: "后台独立执行".into(),
            })
        }),
        Err(SubjectError::Storage)
    );
    assert_eq!(starts, 1);
    kernel.stop(&plugin_id()).await.unwrap();
    let (_kernel, admin) = start(state).await;
    let driver = SubjectDriver::new(Arc::new(admin.clone()), "restarted-host").unwrap();
    let event = driver
        .run_one(now(), |_| {
            starts += 1;
            panic!("已保存的派发回执不得重跑外部动作")
        })
        .unwrap()
        .unwrap();
    assert_eq!(starts, 1);
    assert_eq!(event.status, EventStatus::Completed);
    let snapshot = admin.snapshot_for(&scope("a", "alice")).unwrap();
    assert_eq!(snapshot.foci.len(), 1);
    assert_eq!(snapshot.foci[0].task_id.as_deref(), Some("task-persisted"));
}

#[tokio::test]
async fn process_restart_preserves_unknown_effect_and_never_requeues_it() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, admin) = start(state.clone()).await;
    let t = now();
    admin
        .enqueue(input("a", "alice", "effect", EventPriority::User), t)
        .unwrap();
    let event = admin.claim_next("host", t, 1000).unwrap().unwrap();
    admin
        .checkpoint(
            &token(&event),
            SubjectCheckpoint {
                boundary: CheckpointBoundary::EffectPending,
                resume_data: "外部发送前已记账".into(),
            },
            t,
        )
        .unwrap();
    kernel.stop(&plugin_id()).await.unwrap();
    let (_kernel, admin) = start(state).await;
    assert_eq!(
        admin.claim_next("restarted-host", now(), 1000).unwrap(),
        None
    );
    let saved = admin
        .snapshot_for(&scope("a", "alice"))
        .unwrap()
        .events
        .pop()
        .unwrap();
    assert_eq!(saved.status, EventStatus::Unknown);
    assert_eq!(saved.checkpoint.resume_data, "外部发送前已记账");
    assert_eq!(
        admin.enqueue(event.input, now()).unwrap().status,
        EventStatus::Unknown
    );
}
