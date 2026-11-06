use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use eve_task_api::*;
use eve_task_plugin::{TaskController, TaskPlugin};
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

type Mutation = Box<dyn Fn(&mut Value)>;

#[path = "tasks/working_set.rs"]
mod working_set;

struct Store {
    inner: FileStateStore,
    fail: AtomicBool,
    fail_after: AtomicBool,
    hide_ledger: AtomicBool,
    fail_initialized: AtomicBool,
}
impl StateStore for Store {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        if key == TASK_STATE_KEY && self.hide_ledger.load(Ordering::SeqCst) {
            return Ok(None);
        }
        self.inner.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        if key == "tasks.initialized.v1" && self.fail_initialized.load(Ordering::SeqCst) {
            return Err(PluginError::State("private-initialization-detail".into()));
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err(PluginError::State("private-storage-detail".into()));
        }
        self.inner.set(namespace, key, value)?;
        if self.fail_after.load(Ordering::SeqCst) {
            return Err(PluginError::State("private-after-commit-detail".into()));
        }
        Ok(())
    }
}

struct World {
    _directory: tempfile::TempDir,
    state: Arc<Store>,
}
impl World {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(Store {
            inner: FileStateStore::open(directory.path().join("state")).unwrap(),
            fail: AtomicBool::new(false),
            fail_after: AtomicBool::new(false),
            hide_ledger: AtomicBool::new(false),
            fail_initialized: AtomicBool::new(false),
        });
        Self {
            _directory: directory,
            state,
        }
    }
    async fn open(&self) -> PluginResult<(Kernel, TaskController)> {
        let kernel = Kernel::with_services(KernelServices {
            state: self.state.clone(),
            ..KernelServices::default()
        });
        let plugin = TaskPlugin::new()?;
        let controller = plugin.controller();
        kernel.register(Box::new(plugin))?;
        kernel.start_all().await?;
        Ok((kernel, controller))
    }
    fn raw(&self) -> Option<Vec<u8>> {
        self.state
            .get(&PluginId::new(TASK_PLUGIN_ID).unwrap(), TASK_STATE_KEY)
            .unwrap()
    }
    fn write(&self, bytes: Vec<u8>) {
        self.state
            .inner
            .set(
                &PluginId::new(TASK_PLUGIN_ID).unwrap(),
                TASK_STATE_KEY.into(),
                bytes,
            )
            .unwrap();
    }
}

fn owner(user: &str) -> TaskOwner {
    TaskOwner {
        session_id: format!("qq:session-{user}"),
        user_id: user.into(),
    }
}

fn spec(title: &str) -> TaskSpec {
    TaskSpec {
        title: title.into(),
        instructions: format!("{title}：读取资料并给出结论"),
        max_steps: DEFAULT_STEPS,
        context: None,
    }
}

fn usage(calls: u32) -> TaskUsage {
    TaskUsage {
        tool_calls: calls,
        model_requests: calls + 1,
    }
}

fn sent() -> DeliveryResult {
    DeliveryResult::Sent {
        platform_message_id: Some("out-1".into()),
    }
}

#[tokio::test]
async fn expected_notice_claim_and_receipt_cannot_cross_attempts_or_resumed_endings() {
    for cold in [false, true] {
        let world = World::new();
        let archive = world.archive();
        let (kernel, tasks) = if cold {
            world.cold(archive.clone()).await.unwrap()
        } else {
            world.open().await.unwrap()
        };
        let alice = owner("alice");
        let task = tasks
            .create_bounded(&alice, spec("等待条件后接续"), "expected-notice", 1000, 10)
            .unwrap();
        tasks.start(&task.id, 20).unwrap();
        let mut frame = checkpoint("alice");
        frame.result = Some("等待条件".into());
        frame.remaining = None;
        tasks.checkpoint(&task.id, frame, 900, 30).unwrap();
        tasks
            .finish(
                &task.id,
                TaskEnd::Blocked {
                    result: "等待条件".into(),
                },
                usage(0),
                40,
            )
            .unwrap();
        let wrong_scope = TaskOwner {
            session_id: "qq:another-session".into(),
            ..alice.clone()
        };
        let before = world.raw();
        assert_eq!(
            tasks.claim_notice_expected(
                &wrong_scope,
                &task.id,
                40,
                0,
                DeliveryChannel::Proactive,
                50
            ),
            Err(TaskError::NotFound)
        );
        for (finish, attempts) in [(39, 0), (40, 1)] {
            assert_eq!(
                tasks.claim_notice_expected(
                    &alice,
                    &task.id,
                    finish,
                    attempts,
                    DeliveryChannel::Proactive,
                    50
                ),
                Err(TaskError::InvalidTransition)
            );
        }
        assert_eq!(world.raw(), before, "旧判断不得先消耗一次尝试");
        let claimed = tasks
            .claim_notice_expected(&alice, &task.id, 40, 0, DeliveryChannel::Proactive, 50)
            .unwrap();
        assert_eq!(claimed.notice.as_ref().unwrap().attempts.len(), 1);
        tasks
            .record_delivery_expected(&alice, &task.id, 40, 1, DeliveryResult::NotSent, 60)
            .unwrap();
        tasks
            .claim_notice_expected(
                &alice,
                &task.id,
                40,
                1,
                DeliveryChannel::Passive {
                    message_id: "new-input".into(),
                },
                65,
            )
            .unwrap();
        let before = world.raw();
        assert_eq!(
            tasks.record_delivery_expected(&alice, &task.id, 40, 1, sent(), 66),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(
            world.raw(),
            before,
            "同一终态的第一条迟到回执不能结算第二次尝试"
        );
        tasks
            .record_delivery_expected(&alice, &task.id, 40, 2, sent(), 66)
            .unwrap();
        tasks
            .resume_blocked(&alice, &task.id, "条件已经满足", 70)
            .unwrap();
        tasks
            .finish(
                &task.id,
                TaskEnd::Completed {
                    result: "实际验证完成".into(),
                },
                usage(1),
                80,
            )
            .unwrap();
        let before = world.raw();
        assert_eq!(
            tasks.claim_notice_expected(&alice, &task.id, 40, 0, DeliveryChannel::Proactive, 90),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(world.raw(), before);
        tasks
            .claim_notice_expected(&alice, &task.id, 80, 0, DeliveryChannel::Proactive, 90)
            .unwrap();
        let before = world.raw();
        for (finish, attempts) in [(40, 1), (40, 2), (80, 2)] {
            assert_eq!(
                tasks.record_delivery_expected(&alice, &task.id, finish, attempts, sent(), 100),
                Err(TaskError::InvalidTransition)
            );
        }
        assert_eq!(
            tasks.record_delivery_expected(&wrong_scope, &task.id, 80, 1, sent(), 100),
            Err(TaskError::NotFound)
        );
        assert_eq!(world.raw(), before, "旧终态回执不能结算新终态的发送中通知");
        let delivered = tasks
            .record_delivery_expected(&alice, &task.id, 80, 1, sent(), 100)
            .unwrap();
        assert_eq!(
            delivered.notice.as_ref().unwrap().status,
            NoticeStatus::Delivered
        );
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = if cold {
            world.cold(archive).await.unwrap()
        } else {
            world.open().await.unwrap()
        };
        assert_eq!(tasks.get_owned(&alice, &task.id).unwrap(), Some(delivered));
        let before = world.raw();
        assert_eq!(
            tasks.record_delivery_expected(&alice, &task.id, 40, 1, sent(), 110),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(world.raw(), before);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn unknown_notice_results_settle_immediately_and_remain_unknown_after_restart() {
    for cold in [false, true] {
        for expected in [false, true] {
            for outcome in [
                DeliveryResult::Unknown,
                DeliveryResult::Sent {
                    platform_message_id: None,
                },
            ] {
                let world = World::new();
                let archive = world.archive();
                let (kernel, tasks) = if cold {
                    world.cold(archive.clone()).await.unwrap()
                } else {
                    world.open().await.unwrap()
                };
                let alice = owner("alice");
                let task = tasks
                    .create(&alice, spec("核验结束"), "unknown-result", 10)
                    .unwrap();
                tasks.start(&task.id, 20).unwrap();
                tasks
                    .finish(
                        &task.id,
                        TaskEnd::Completed {
                            result: "实际结果".into(),
                        },
                        usage(1),
                        30,
                    )
                    .unwrap();
                tasks
                    .claim_notice_expected(&alice, &task.id, 30, 0, DeliveryChannel::Proactive, 40)
                    .unwrap();
                let settled = if expected {
                    tasks
                        .record_delivery_expected(&alice, &task.id, 30, 1, outcome, 50)
                        .unwrap()
                } else {
                    tasks.record_delivery(&task.id, outcome, 50).unwrap()
                };
                let notice = settled.notice.as_ref().unwrap();
                assert_eq!(notice.status, NoticeStatus::Unknown);
                assert_eq!(notice.attempts[0].result, Some(DeliveryResult::Unknown));
                assert_eq!(notice.attempts[0].finished_at_ms, Some(50));
                assert_eq!(notice.closed_at_ms, None);
                assert!(tasks.delivery_snapshot().unwrap().due_proactive().is_none());
                assert!(
                    tasks
                        .delivery_snapshot()
                        .unwrap()
                        .due_passive("alice", "next-input")
                        .is_none()
                );
                let committed = world.raw();
                kernel.stop_all().await.unwrap();
                let (kernel, tasks) = if cold {
                    world.cold(archive).await.unwrap()
                } else {
                    world.open().await.unwrap()
                };
                assert_eq!(tasks.get_owned(&alice, &task.id).unwrap(), Some(settled));
                assert_eq!(world.raw(), committed);
                assert_eq!(
                    tasks.record_delivery_expected(&alice, &task.id, 30, 1, sent(), 60),
                    Err(TaskError::InvalidTransition)
                );
                assert_eq!(
                    tasks.claim_notice_expected(
                        &alice,
                        &task.id,
                        30,
                        1,
                        DeliveryChannel::Passive {
                            message_id: "next-input".into()
                        },
                        60
                    ),
                    Err(TaskError::InvalidTransition)
                );
                assert_eq!(world.raw(), committed);
                kernel.stop_all().await.unwrap();
            }
        }
    }
}

#[tokio::test]
async fn uncertain_expected_notice_claim_closes_handle_and_recovers_only_persisted_attempt() {
    for cold in [false, true] {
        for after in [false, true] {
            let world = World::new();
            let archive = world.archive();
            let (kernel, tasks) = if cold {
                world.cold(archive.clone()).await.unwrap()
            } else {
                world.open().await.unwrap()
            };
            let alice = owner("alice");
            let task = tasks
                .create(&alice, spec("提交边界"), "expected-commit", 10)
                .unwrap();
            tasks.start(&task.id, 20).unwrap();
            tasks
                .finish(
                    &task.id,
                    TaskEnd::Completed {
                        result: "完成".into(),
                    },
                    usage(1),
                    30,
                )
                .unwrap();
            let fail = if after {
                &world.state.fail_after
            } else {
                &world.state.fail
            };
            fail.store(true, Ordering::SeqCst);
            assert_eq!(
                tasks.claim_notice_expected(
                    &alice,
                    &task.id,
                    30,
                    0,
                    DeliveryChannel::Proactive,
                    40
                ),
                Err(TaskError::Storage)
            );
            assert_eq!(tasks.snapshot().err(), Some(TaskError::Unavailable));
            fail.store(false, Ordering::SeqCst);
            kernel.stop_all().await.unwrap();
            let (kernel, tasks) = if cold {
                world.cold(archive).await.unwrap()
            } else {
                world.open().await.unwrap()
            };
            let restored = tasks.get_owned(&alice, &task.id).unwrap().unwrap();
            let notice = restored.notice.unwrap();
            assert_eq!(
                notice.status,
                if after {
                    NoticeStatus::Unknown
                } else {
                    NoticeStatus::Pending
                }
            );
            assert_eq!(notice.attempts.len(), usize::from(after));
            if after {
                assert_eq!(notice.attempts[0].result, Some(DeliveryResult::Unknown));
            }
            kernel.stop_all().await.unwrap();
        }
    }
}

#[tokio::test]
async fn local_delivery_records_one_receipt_and_replays_without_writing_after_restart() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let alice = owner("alice");
    let task = tasks
        .create(&alice, spec("本地完成通知"), "local-receipt", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let completed = tasks
        .finish(
            &task.id,
            TaskEnd::Completed {
                result: "已验证完成".into(),
            },
            usage(1),
            30,
        )
        .unwrap();
    let delivered = tasks
        .record_local_delivery(&alice, &task.id, 30, "notification-1", 40)
        .unwrap();
    let mut expected = completed;
    expected.notice = Some(Notice {
        status: NoticeStatus::Delivered,
        attempts: vec![DeliveryAttempt {
            channel: DeliveryChannel::LocalInbox,
            started_at_ms: 40,
            finished_at_ms: Some(40),
            result: Some(DeliveryResult::StoredLocally {
                notification_id: "notification-1".into(),
            }),
        }],
        closed_at_ms: Some(40),
    });
    assert_eq!(delivered, expected);
    assert_eq!(delivered.settled_at(), Some(40));
    let directory = tasks.delivery_snapshot().unwrap();
    directory.validate().unwrap();
    assert!(directory.due_proactive().is_none());
    assert!(directory.due_passive("alice", "next-message").is_none());
    let committed = world.raw().unwrap();

    // 重复确认只读已提交回执；即便存储暂时不可写，也不改时间或追加尝试。
    world.state.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        tasks
            .record_local_delivery(&alice, &task.id, 30, "notification-1", 90)
            .unwrap(),
        delivered
    );
    assert_eq!(
        tasks.get_owned(&alice, &task.id).unwrap(),
        Some(delivered.clone())
    );
    assert_eq!(world.raw().unwrap(), committed);
    world.state.fail.store(false, Ordering::SeqCst);

    let other_session = TaskOwner {
        session_id: "web:another-session".into(),
        user_id: alice.user_id.clone(),
    };
    assert_eq!(
        tasks.record_local_delivery(&other_session, &task.id, 30, "notification-1", 90),
        Err(TaskError::NotFound),
        "相同回执不能绕过完整身份检查"
    );
    assert_eq!(
        tasks.record_local_delivery(&alice, &task.id, 30, "notification-2", 90),
        Err(TaskError::InvalidTransition)
    );
    assert_eq!(world.raw().unwrap(), committed);
    kernel.stop_all().await.unwrap();

    let (kernel, tasks) = world.open().await.unwrap();
    assert_eq!(
        tasks.get_owned(&alice, &task.id).unwrap(),
        Some(delivered.clone())
    );
    assert_eq!(
        tasks
            .record_local_delivery(&alice, &task.id, 30, "notification-1", 100)
            .unwrap(),
        delivered
    );
    assert_eq!(world.raw().unwrap(), committed);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn local_delivery_preserves_failed_platform_attempts_and_monotonic_receipt_times() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let alice = owner("alice");
    let task = tasks
        .create(&alice, spec("通知路由不可用"), "local-fallback", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    tasks
        .finish(
            &task.id,
            TaskEnd::Completed {
                result: "完成".into(),
            },
            usage(0),
            30,
        )
        .unwrap();
    tasks
        .claim(&task.id, DeliveryChannel::Proactive, 40)
        .unwrap();
    let previous = tasks
        .record_delivery(&task.id, DeliveryResult::NotSent, 50)
        .unwrap()
        .notice
        .unwrap()
        .attempts;
    let delivered = tasks
        .record_local_delivery(&alice, &task.id, 30, "notification-1", 35)
        .unwrap();
    let notice = delivered.notice.unwrap();
    assert_eq!(notice.status, NoticeStatus::Delivered);
    assert_eq!(notice.closed_at_ms, Some(50));
    assert_eq!(notice.attempts.len(), 2);
    assert_eq!(&notice.attempts[..1], previous.as_slice());
    assert_eq!(
        notice.attempts[1],
        DeliveryAttempt {
            channel: DeliveryChannel::LocalInbox,
            started_at_ms: 50,
            finished_at_ms: Some(50),
            result: Some(DeliveryResult::StoredLocally {
                notification_id: "notification-1".into(),
            }),
        }
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn racing_local_delivery_receipts_settle_exactly_one_attempt() {
    for receipts in [
        ["notification-1", "notification-1"],
        ["notification-1", "notification-2"],
    ] {
        let world = World::new();
        let (kernel, tasks) = world.open().await.unwrap();
        let alice = owner("alice");
        let task = tasks
            .create(&alice, spec("并发本地回执"), "local-race", 10)
            .unwrap();
        tasks.start(&task.id, 20).unwrap();
        tasks
            .finish(
                &task.id,
                TaskEnd::Completed {
                    result: "完成".into(),
                },
                usage(0),
                30,
            )
            .unwrap();
        let barrier = std::sync::Barrier::new(2);
        let outcomes = std::thread::scope(|scope| {
            let barrier = &barrier;
            let tasks = &tasks;
            let alice = &alice;
            let task_id = &task.id;
            let handles: Vec<_> = receipts
                .iter()
                .map(|&receipt| {
                    scope.spawn(move || {
                        barrier.wait();
                        tasks.record_local_delivery(alice, task_id, 30, receipt, 40)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        let delivered = tasks.get_owned(&alice, &task.id).unwrap().unwrap();
        assert_eq!(
            delivered.notice.as_ref().unwrap().status,
            NoticeStatus::Delivered
        );
        assert_eq!(delivered.notice.as_ref().unwrap().attempts.len(), 1);
        let success_count = if receipts[0] == receipts[1] { 2 } else { 1 };
        assert_eq!(
            outcomes.iter().filter(|outcome| outcome.is_ok()).count(),
            success_count
        );
        for outcome in outcomes {
            match outcome {
                Ok(saved) => assert_eq!(saved, delivered),
                Err(error) => assert_eq!(error, TaskError::InvalidTransition),
            }
        }
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn local_delivery_validates_full_identity_receipt_and_finished_epoch_without_writing() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let alice = owner("alice");
    let task = tasks
        .create(&alice, spec("身份范围"), "local-scope", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    tasks
        .finish(
            &task.id,
            TaskEnd::Failed {
                reason: "等待服务恢复".into(),
            },
            usage(0),
            30,
        )
        .unwrap();
    let before = world.raw();
    for mismatched in [
        owner("bob"),
        TaskOwner {
            session_id: "web:another-session".into(),
            user_id: alice.user_id.clone(),
        },
        TaskOwner {
            session_id: alice.session_id.clone(),
            user_id: "bob".into(),
        },
    ] {
        assert_eq!(
            tasks.record_local_delivery(&mismatched, &task.id, 30, "notification-1", 40),
            Err(TaskError::NotFound)
        );
        assert_eq!(world.raw(), before);
    }
    for invalid in [
        TaskOwner {
            session_id: String::new(),
            user_id: alice.user_id.clone(),
        },
        TaskOwner {
            session_id: alice.session_id.clone(),
            user_id: "alice\n".into(),
        },
    ] {
        assert_eq!(
            tasks.record_local_delivery(&invalid, &task.id, 30, "notification-1", 40),
            Err(TaskError::InvalidInput)
        );
        assert_eq!(world.raw(), before);
    }
    for invalid in [
        String::new(),
        " bad".into(),
        "bad\nreceipt".into(),
        "x".repeat(257),
    ] {
        assert_eq!(
            tasks.record_local_delivery(&alice, &invalid, 30, "notification-1", 40),
            Err(TaskError::InvalidInput)
        );
        assert_eq!(
            tasks.record_local_delivery(&alice, &task.id, 30, &invalid, 40),
            Err(TaskError::InvalidInput)
        );
        assert_eq!(world.raw(), before);
    }
    assert_eq!(
        tasks.record_local_delivery(&alice, "task-missing", 30, "notification-1", 40),
        Err(TaskError::NotFound)
    );
    assert_eq!(
        tasks.record_local_delivery(&alice, &task.id, 0, "notification-1", 40),
        Err(TaskError::InvalidInput)
    );
    assert_eq!(
        tasks.record_local_delivery(&alice, &task.id, 30, "notification-1", 0),
        Err(TaskError::InvalidInput)
    );
    for stale_epoch in [29, 31] {
        assert_eq!(
            tasks.record_local_delivery(&alice, &task.id, stale_epoch, "notification-1", 40),
            Err(TaskError::InvalidTransition)
        );
    }
    assert_eq!(world.raw(), before);
    assert!(
        tasks
            .get_owned(&alice, &task.id)
            .unwrap()
            .unwrap()
            .notice_pending()
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn local_delivery_cannot_be_forged_through_the_platform_delivery_api() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("专用本地入口"), "local-only", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    tasks
        .finish(
            &task.id,
            TaskEnd::Completed {
                result: "完成".into(),
            },
            usage(0),
            30,
        )
        .unwrap();
    let before = world.raw();
    assert_eq!(
        tasks.claim(&task.id, DeliveryChannel::LocalInbox, 40),
        Err(TaskError::InvalidInput)
    );
    assert_eq!(world.raw(), before);
    let sending = tasks
        .claim(&task.id, DeliveryChannel::Proactive, 40)
        .unwrap();
    let before = world.raw();
    assert_eq!(
        tasks.record_delivery(
            &task.id,
            DeliveryResult::StoredLocally {
                notification_id: "notification-1".into(),
            },
            50,
        ),
        Err(TaskError::InvalidInput)
    );
    assert_eq!(world.raw(), before);
    assert_eq!(tasks.snapshot().unwrap().get(&task.id), Some(&sending));
    tasks.record_delivery(&task.id, sent(), 60).unwrap();
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn local_delivery_never_replaces_inflight_or_settled_platform_notices() {
    for status in [
        NoticeStatus::Delivering,
        NoticeStatus::Delivered,
        NoticeStatus::Expired,
        NoticeStatus::Unknown,
    ] {
        let world = World::new();
        let (kernel, tasks) = world.open().await.unwrap();
        let alice = owner("alice");
        let task = tasks
            .create(&alice, spec("保留已有通知结局"), "local-status", 10)
            .unwrap();
        tasks.start(&task.id, 20).unwrap();
        tasks
            .finish(
                &task.id,
                TaskEnd::Completed {
                    result: "完成".into(),
                },
                usage(0),
                30,
            )
            .unwrap();
        if status == NoticeStatus::Expired {
            tasks.expire(&task.id, 31 + DELIVERY_WINDOW_MS).unwrap();
        } else {
            tasks
                .claim(&task.id, DeliveryChannel::Proactive, 40)
                .unwrap();
            if status == NoticeStatus::Delivered {
                tasks.record_delivery(&task.id, sent(), 50).unwrap();
            }
        }
        let (kernel, tasks) = if status == NoticeStatus::Unknown {
            kernel.stop_all().await.unwrap();
            world.open().await.unwrap()
        } else {
            (kernel, tasks)
        };
        let saved = tasks.get_owned(&alice, &task.id).unwrap().unwrap();
        assert_eq!(saved.notice.as_ref().unwrap().status, status);
        let before = world.raw();
        assert_eq!(
            tasks.record_local_delivery(
                &alice,
                &task.id,
                30,
                "notification-1",
                40 + DELIVERY_WINDOW_MS,
            ),
            Err(TaskError::InvalidTransition),
            "不能改写已有通知状态：{status:?}"
        );
        assert_eq!(tasks.get_owned(&alice, &task.id).unwrap(), Some(saved));
        assert_eq!(world.raw(), before);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn local_delivery_requires_a_finished_task_with_a_pending_notice() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let alice = owner("alice");
    let task = tasks
        .create(&alice, spec("尚未产生通知"), "local-no-notice", 10)
        .unwrap();
    let before = world.raw();
    assert_eq!(
        tasks.record_local_delivery(&alice, &task.id, 30, "notification-1", 40),
        Err(TaskError::InvalidTransition)
    );
    assert_eq!(world.raw(), before);
    tasks.start(&task.id, 20).unwrap();
    let before = world.raw();
    assert_eq!(
        tasks.record_local_delivery(&alice, &task.id, 30, "notification-1", 40),
        Err(TaskError::InvalidTransition)
    );
    assert_eq!(world.raw(), before);
    tasks.cancel("alice", &task.id, 25).unwrap();
    tasks
        .finish(&task.id, TaskEnd::Cancelled, usage(0), 30)
        .unwrap();
    let before = world.raw();
    assert_eq!(
        tasks.record_local_delivery(&alice, &task.id, 30, "notification-1", 40),
        Err(TaskError::InvalidTransition)
    );
    assert_eq!(world.raw(), before);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn failed_local_delivery_writes_close_the_instance_and_recover_the_committed_state() {
    for committed_before_error in [false, true] {
        let world = World::new();
        let (kernel, tasks) = world.open().await.unwrap();
        let alice = owner("alice");
        let task = tasks
            .create(&alice, spec("本地回执确认丢失"), "local-write-failure", 10)
            .unwrap();
        tasks.start(&task.id, 20).unwrap();
        let pending = tasks
            .finish(
                &task.id,
                TaskEnd::Completed {
                    result: "完成".into(),
                },
                usage(0),
                30,
            )
            .unwrap();
        let before = world.raw();
        let failure = if committed_before_error {
            &world.state.fail_after
        } else {
            &world.state.fail
        };
        failure.store(true, Ordering::SeqCst);
        assert_eq!(
            tasks.record_local_delivery(&alice, &task.id, 30, "notification-1", 40),
            Err(TaskError::Storage)
        );
        assert_eq!(tasks.snapshot().unwrap_err(), TaskError::Unavailable);
        assert_eq!(
            tasks.record_local_delivery(&alice, &task.id, 30, "notification-1", 50),
            Err(TaskError::Unavailable)
        );
        failure.store(false, Ordering::SeqCst);
        let saved_bytes = world.raw();
        if committed_before_error {
            assert_ne!(saved_bytes, before);
        } else {
            assert_eq!(saved_bytes, before);
        }
        kernel.stop_all().await.unwrap();

        let (kernel, tasks) = world.open().await.unwrap();
        let recovered = tasks.get_owned(&alice, &task.id).unwrap().unwrap();
        if committed_before_error {
            assert_eq!(
                recovered.notice.as_ref().unwrap().status,
                NoticeStatus::Delivered
            );
        } else {
            assert_eq!(recovered, pending);
        }
        let delivered = tasks
            .record_local_delivery(&alice, &task.id, 30, "notification-1", 50)
            .unwrap();
        let receipt_at = if committed_before_error { 40 } else { 50 };
        assert_eq!(
            delivered.notice,
            Some(Notice {
                status: NoticeStatus::Delivered,
                attempts: vec![DeliveryAttempt {
                    channel: DeliveryChannel::LocalInbox,
                    started_at_ms: receipt_at,
                    finished_at_ms: Some(receipt_at),
                    result: Some(DeliveryResult::StoredLocally {
                        notification_id: "notification-1".into(),
                    }),
                }],
                closed_at_ms: Some(receipt_at),
            })
        );
        if committed_before_error {
            assert_eq!(world.raw(), saved_bytes);
        }
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn local_delivery_rejects_receipts_from_before_the_same_blocked_task_resumed() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let alice = owner("alice");
    let task = tasks
        .create_bounded(&alice, spec("等待构建退出"), "local-epoch", 1000, 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let mut frame = checkpoint("alice");
    frame.result = Some("等待构建退出".into());
    frame.remaining = None;
    tasks.checkpoint(&task.id, frame, 900, 30).unwrap();
    tasks
        .finish(
            &task.id,
            TaskEnd::Blocked {
                result: "等待构建退出".into(),
            },
            usage(0),
            40,
        )
        .unwrap();
    tasks
        .record_local_delivery(&alice, &task.id, 40, "notification-blocked", 50)
        .unwrap();
    tasks
        .resume_blocked(&alice, &task.id, "构建已退出，请核对产物", 60)
        .unwrap();
    let before = world.raw();
    assert_eq!(
        tasks.record_local_delivery(&alice, &task.id, 40, "notification-blocked", 70),
        Err(TaskError::InvalidTransition)
    );
    assert_eq!(world.raw(), before);
    let completed = tasks
        .finish(
            &task.id,
            TaskEnd::Completed {
                result: "产物已验证".into(),
            },
            usage(1),
            80,
        )
        .unwrap();
    assert_eq!(completed.finished_at_ms, Some(80));
    let before = world.raw();
    for receipt in ["notification-blocked", "notification-completed"] {
        assert_eq!(
            tasks.record_local_delivery(&alice, &task.id, 40, receipt, 90),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(world.raw(), before);
    }
    assert_eq!(tasks.get_owned(&alice, &task.id).unwrap(), Some(completed));
    let delivered = tasks
        .record_local_delivery(&alice, &task.id, 80, "notification-completed", 100)
        .unwrap();
    let notice = delivered.notice.unwrap();
    assert_eq!(notice.status, NoticeStatus::Delivered);
    assert_eq!(notice.attempts.len(), 1);
    assert_eq!(
        notice.attempts[0].result,
        Some(DeliveryResult::StoredLocally {
            notification_id: "notification-completed".into(),
        })
    );
    let before = world.raw();
    assert_eq!(
        tasks.record_local_delivery(&alice, &task.id, 40, "notification-completed", 110),
        Err(TaskError::InvalidTransition)
    );
    assert_eq!(world.raw(), before);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn local_delivery_epoch_advances_when_a_blocked_task_resumes_without_clock_progress() {
    for resumed_at in [40, 1] {
        let world = World::new();
        let (kernel, tasks) = world.open().await.unwrap();
        let alice = owner("alice");
        let task = tasks
            .create_bounded(
                &alice,
                spec("时钟边界接续"),
                "local-monotonic-epoch",
                1000,
                10,
            )
            .unwrap();
        tasks.start(&task.id, 20).unwrap();
        let mut frame = checkpoint("alice");
        frame.result = Some("等待构建退出".into());
        frame.remaining = None;
        tasks.checkpoint(&task.id, frame, 900, 30).unwrap();
        let blocked = tasks
            .finish(
                &task.id,
                TaskEnd::Blocked {
                    result: "等待构建退出".into(),
                },
                usage(0),
                40,
            )
            .unwrap();
        let old_epoch = blocked.finished_at_ms.unwrap();
        assert_eq!(old_epoch, 40);
        tasks
            .record_local_delivery(&alice, &task.id, old_epoch, "notification-blocked", 40)
            .unwrap();
        tasks
            .resume_blocked(&alice, &task.id, "已确认构建退出", resumed_at)
            .unwrap();
        let completed = tasks
            .finish(
                &task.id,
                TaskEnd::Completed {
                    result: "已验证产物".into(),
                },
                usage(1),
                resumed_at,
            )
            .unwrap();
        let new_epoch = completed.finished_at_ms.unwrap();
        assert!(
            new_epoch > old_epoch,
            "同毫秒或回拨时钟下，接续后的终局仍须有新的结束批次"
        );
        let before = world.raw();
        assert_eq!(
            tasks.record_local_delivery(
                &alice,
                &task.id,
                old_epoch,
                "notification-blocked",
                resumed_at,
            ),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(world.raw(), before);
        let delivered = tasks
            .record_local_delivery(
                &alice,
                &task.id,
                new_epoch,
                "notification-completed",
                resumed_at,
            )
            .unwrap();
        assert_eq!(
            delivered.notice.as_ref().unwrap().status,
            NoticeStatus::Delivered
        );
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open().await.unwrap();
        assert_eq!(tasks.get_owned(&alice, &task.id).unwrap(), Some(delivered));
        let before = world.raw();
        assert_eq!(
            tasks.record_local_delivery(&alice, &task.id, old_epoch, "notification-blocked", 100),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(world.raw(), before);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn local_delivery_legacy_blocked_epochs_survive_restart_and_clock_rollback() {
    for resumed_at in [40, 1] {
        let world = World::new();
        let (kernel, tasks) = world.open().await.unwrap();
        let alice = owner("alice");
        let task = tasks
            .create_bounded(&alice, spec("旧任务接续"), "local-legacy-epoch", 1000, 10)
            .unwrap();
        tasks.start(&task.id, 20).unwrap();
        let mut frame = checkpoint("alice");
        frame.result = Some("等待构建退出".into());
        frame.remaining = None;
        tasks.checkpoint(&task.id, frame, 900, 30).unwrap();
        tasks
            .finish(
                &task.id,
                TaskEnd::Blocked {
                    result: "等待构建退出".into(),
                },
                usage(0),
                40,
            )
            .unwrap();
        tasks
            .claim(&task.id, DeliveryChannel::Proactive, 40)
            .unwrap();
        tasks.record_delivery(&task.id, sent(), 40).unwrap();
        kernel.stop_all().await.unwrap();

        let mut legacy: Value = serde_json::from_slice(&world.raw().unwrap()).unwrap();
        assert!(
            legacy["tasks"][0]["execution"]
                .as_object_mut()
                .unwrap()
                .remove("last_finished_at_ms")
                .is_some()
        );
        let original = serde_json::to_vec(&legacy).unwrap();
        world.write(original.clone());
        let (kernel, tasks) = world.open().await.unwrap();
        let restored = tasks.get_owned(&alice, &task.id).unwrap().unwrap();
        assert_eq!(restored.status, TaskStatus::Blocked);
        assert_eq!(restored.finished_at_ms, Some(40));
        assert_eq!(restored.execution.unwrap().last_finished_at_ms, None);
        assert_eq!(world.raw().unwrap(), original, "打开旧任务不改写原账本");

        let resumed = tasks
            .resume_blocked(&alice, &task.id, "已确认构建退出", resumed_at)
            .unwrap();
        assert_eq!(resumed.finished_at_ms, None);
        assert_eq!(resumed.execution.unwrap().last_finished_at_ms, Some(40));
        let saved: Value = serde_json::from_slice(&world.raw().unwrap()).unwrap();
        assert_eq!(saved["tasks"][0]["execution"]["last_finished_at_ms"], 40);
        let completed = tasks
            .finish(
                &task.id,
                TaskEnd::Completed {
                    result: "旧任务已验证完成".into(),
                },
                usage(1),
                resumed_at,
            )
            .unwrap();
        let new_epoch = completed.finished_at_ms.unwrap();
        assert!(new_epoch > 40, "旧账本接续后也不能复用结束批次");
        let before = world.raw();
        assert_eq!(
            tasks.record_local_delivery(&alice, &task.id, 40, "notification-old", resumed_at),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(world.raw(), before);
        let delivered = tasks
            .record_local_delivery(
                &alice,
                &task.id,
                new_epoch,
                "notification-current",
                resumed_at,
            )
            .unwrap();
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open().await.unwrap();
        assert_eq!(tasks.get_owned(&alice, &task.id).unwrap(), Some(delivered));
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn corrupt_finished_epoch_bounds_refuse_restart_without_replacing_original_bytes() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let alice = owner("alice");
    let task = tasks
        .create_bounded(
            &alice,
            spec("结束下界校验"),
            "invalid-finished-epoch",
            1000,
            10,
        )
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let mut frame = checkpoint("alice");
    frame.result = Some("等待构建退出".into());
    frame.remaining = None;
    tasks.checkpoint(&task.id, frame, 900, 30).unwrap();
    let blocked = tasks
        .finish(
            &task.id,
            TaskEnd::Blocked {
                result: "等待构建退出".into(),
            },
            usage(0),
            40,
        )
        .unwrap();
    let terminal_bytes = world.raw().unwrap();
    let terminal: Value = serde_json::from_slice(&terminal_bytes).unwrap();
    let resumed = tasks
        .resume_blocked(&alice, &task.id, "已确认构建退出", 50)
        .unwrap();
    assert_eq!(resumed.status, TaskStatus::Running);
    assert_eq!(resumed.execution.unwrap().last_finished_at_ms, Some(40));
    let running: Value = serde_json::from_slice(&world.raw().unwrap()).unwrap();
    kernel.stop_all().await.unwrap();

    for (case, source, invalid_bound) in [
        ("zero", &terminal, Value::from(0)),
        ("before-start", &running, Value::from(19)),
        ("before-terminal-finish", &terminal, Value::from(39)),
        ("after-terminal-finish", &terminal, Value::from(41)),
        ("wrong-type", &terminal, Value::from("40")),
        ("running-epoch-overflow", &running, Value::from(u64::MAX)),
    ] {
        let mut corrupt = (*source).clone();
        corrupt["tasks"][0]["execution"]["last_finished_at_ms"] = invalid_bound;
        let original = serde_json::to_vec(&corrupt).unwrap();
        world.write(original.clone());
        assert!(
            world.open().await.is_err(),
            "畸形结束下界必须拒绝启动：{case}"
        );
        assert_eq!(world.raw().unwrap(), original, "启动失败保留证据：{case}");
    }

    world.write(terminal_bytes.clone());
    let (kernel, tasks) = world.open().await.unwrap();
    assert_eq!(tasks.get_owned(&alice, &task.id).unwrap(), Some(blocked));
    assert_eq!(world.raw().unwrap(), terminal_bytes);
    kernel.stop_all().await.unwrap();
}

fn checkpoint(user: &str) -> eve_llm_api::TurnCheckpoint {
    let owner = owner(user);
    eve_llm_api::TurnCheckpoint {
        pending_model_request: None,
        pending_request_fingerprint: None,
        model_session: None,
        version: 1,
        scope: eve_llm_api::ContextScope {
            session_id: owner.session_id,
            user_id: owner.user_id,
        },
        messages: vec![eve_llm_api::ChatMessage::text(
            eve_llm_api::ChatRole::User,
            "执行任务",
        )],
        pending_calls: vec![],
        used_tool_calls: 0,
        delegated_tool_calls: 0,
        delegated_model_requests: 0,
        provider_requests: 1,
        tool_batches: 0,
        remaining: Some("核验剩余步骤".into()),
        continuation_checks: 0,
        closing: false,
        result: None,
    }
}

#[tokio::test]
async fn checkpoints_resume_with_a_bounded_retry_count_and_preserve_the_deadline() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("检查点"), "resume", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    tasks
        .checkpoint(&task.id, checkpoint("alice"), u64::MAX, 30)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let saved = tasks.snapshot().unwrap().get(&task.id).unwrap().clone();
    assert_eq!(saved.status, TaskStatus::Running);
    let execution = saved.execution.unwrap();
    assert_eq!(execution.recoveries, 1);
    assert!(execution.resume_at_ms.is_some());
    assert_eq!(execution.deadline_at_ms, Some(u64::MAX));
    assert_eq!(execution.checkpoint, Some(checkpoint("alice")));
    let mut frame = checkpoint("alice");
    for expected in 2..=MAX_AUTO_RECOVERIES {
        frame.provider_requests += 1;
        tasks
            .checkpoint(&task.id, frame.clone(), u64::MAX, 40)
            .unwrap();
        assert_eq!(
            tasks
                .retry(&task.id, 50)
                .unwrap()
                .execution
                .unwrap()
                .recoveries,
            expected
        );
    }
    frame.provider_requests += 1;
    tasks.checkpoint(&task.id, frame, u64::MAX, 60).unwrap();
    let before = world.raw();
    assert_eq!(tasks.retry(&task.id, 70), Err(TaskError::InvalidTransition));
    assert_eq!(world.raw(), before);
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    assert_eq!(
        tasks.snapshot().unwrap().get(&task.id).unwrap().status,
        TaskStatus::Interrupted
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn a_saved_final_result_is_settled_after_restart_without_a_second_execution() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("最终结果"), "final", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let mut frame = checkpoint("alice");
    frame.remaining = None;
    frame.result = Some("已经完成且验证".into());
    tasks.checkpoint(&task.id, frame, u64::MAX, 30).unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let saved = tasks.snapshot().unwrap().get(&task.id).unwrap().clone();
    assert_eq!(saved.status, TaskStatus::Completed);
    assert_eq!(saved.result.as_deref(), Some("已经完成且验证"));
    assert_eq!(saved.usage, Some(usage(0))); // 一个模型请求，没有工具。
    assert!(saved.notice_pending());
    assert_eq!(saved.execution.unwrap().recoveries, 0);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn blocked_result_survives_restart_and_is_never_reexecuted_or_counted_completed() {
    use eve_llm_api::{ChatMessage, ToolCall, ToolOutput, ToolResult};
    use serde_json::json;
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("待用户选择"), "blocked", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let mut frame = checkpoint("alice");
    let args = json!({"status":"blocked","remaining":"已经读完配置；部署到哪个环境？"});
    frame.messages.push(
        ChatMessage::assistant_tool_calls(vec![ToolCall {
            id: "question".into(),
            name: "task_progress".into(),
            arguments: args.clone(),
        }])
        .unwrap(),
    );
    frame.messages.push(
        ChatMessage::tool_results(vec![ToolResult {
            call_id: "question".into(),
            output: ToolOutput::Success(args),
        }])
        .unwrap(),
    );
    frame.used_tool_calls = 1;
    frame.tool_batches = 1;
    frame.provider_requests = 2;
    frame.remaining = None;
    frame.result = Some("已核对配置，部署到哪个环境？".into());
    tasks.checkpoint(&task.id, frame, u64::MAX, 30).unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let saved = tasks.snapshot().unwrap().get(&task.id).unwrap().clone();
    assert_eq!(saved.status, TaskStatus::Blocked);
    assert_eq!(
        saved.result.as_deref(),
        Some("已核对配置，部署到哪个环境？")
    );
    assert!(saved.notice_pending());
    assert!(tasks.snapshot().unwrap().queued().is_empty());
    assert_eq!(saved.execution.unwrap().recoveries, 0);
    let bytes = world.raw().unwrap();
    let wire: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(wire["format_version"], 3);
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    assert_eq!(
        tasks.snapshot().unwrap().get(&task.id).unwrap().status,
        TaskStatus::Blocked
    );
    kernel.stop_all().await.unwrap();
    let mut old = wire;
    old["format_version"] = json!(2);
    world.write(serde_json::to_vec(&old).unwrap());
    assert!(world.open().await.is_err(), "旧格式不能伪装携带新状态");
}

#[tokio::test]
async fn explicit_blocked_finish_persists_progress_and_pending_notice() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("等待选择"), "blocked-finish", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let saved = tasks
        .finish(
            &task.id,
            TaskEnd::Blocked {
                result: "已检查；保留哪份文件？".into(),
            },
            usage(1),
            30,
        )
        .unwrap();
    assert_eq!(saved.status, TaskStatus::Blocked);
    assert!(saved.notice_pending());
    assert_eq!(saved.result.as_deref(), Some("已检查；保留哪份文件？"));
    assert!(saved.failure.is_none());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn unfinished_recovery_result_still_requires_budget_approval_after_restart() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("等待预算"), "failure:budget", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let mut frame = checkpoint("alice");
    frame.closing = true;
    frame.result = Some("已有进度，尚未完成".into());
    tasks.checkpoint(&task.id, frame, u64::MAX, 30).unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let saved = tasks.snapshot().unwrap().get(&task.id).unwrap().clone();
    assert_eq!(saved.status, TaskStatus::Blocked);
    assert!(
        saved
            .result
            .as_deref()
            .is_some_and(|result| result.contains("已有进度，尚未完成"))
    );
    assert!(saved.needs_step_approval());
    assert!(saved.notice_pending());
    assert_eq!(saved.execution.unwrap().recoveries, 0);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn invalid_or_cross_scope_checkpoints_never_replace_the_saved_bytes() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("校验"), "strict", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let before = world.raw();
    assert_eq!(
        tasks.checkpoint(&task.id, checkpoint("bob"), u64::MAX, 30),
        Err(TaskError::InvalidInput)
    );
    let mut bad = checkpoint("alice");
    bad.version = 999;
    assert_eq!(
        tasks.checkpoint(&task.id, bad, u64::MAX, 30),
        Err(TaskError::InvalidInput)
    );
    assert_eq!(world.raw(), before);
    tasks
        .checkpoint(&task.id, checkpoint("alice"), u64::MAX, 30)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let good: Value = serde_json::from_slice(&world.raw().unwrap()).unwrap();
    for mutate in [0, 1, 2] {
        let mut bad = good.clone();
        match mutate {
            0 => bad["tasks"][0]["execution"]["checkpoint"]["scope"]["user_id"] = "bob".into(),
            1 => bad["tasks"][0]["execution"]["checkpoint"]["version"] = 999.into(),
            _ => bad["tasks"][0]["execution"]["checkpoint"]["used_tool_calls"] = 51.into(),
        }
        let bytes = serde_json::to_vec(&bad).unwrap();
        world.write(bytes.clone());
        assert!(world.open().await.is_err());
        assert_eq!(world.raw(), Some(bytes));
    }
}

#[tokio::test]
async fn tasks_run_once_finish_with_a_notice_and_replays_return_the_original() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let alice = owner("alice");
    let created = tasks
        .create(&alice, spec("整理日志"), "call-1", 100)
        .unwrap();
    assert_eq!(created.status, TaskStatus::Queued);
    assert!(created.id.starts_with("task-"));
    // 同一次派发重复执行：返回原记录，不另建。
    let replay = tasks
        .create(&alice, spec("另一个标题"), "call-1", 200)
        .unwrap();
    assert!(replay == created);
    assert_eq!(tasks.snapshot().unwrap().tasks.len(), 1);
    assert_eq!(tasks.snapshot().unwrap().queued()[0].id, created.id);

    let running = tasks.start(&created.id, 300).unwrap();
    assert_eq!(running.status, TaskStatus::Running);
    assert_eq!(
        tasks.start(&created.id, 301).unwrap_err(),
        TaskError::InvalidTransition
    );
    // 用量不能超过步数预算；没有取消请求时不能以取消结束。
    assert_eq!(
        tasks
            .finish(
                &created.id,
                TaskEnd::Completed {
                    result: "好".into()
                },
                usage(DEFAULT_STEPS + 1),
                400
            )
            .unwrap_err(),
        TaskError::InvalidInput
    );
    assert_eq!(
        tasks
            .finish(&created.id, TaskEnd::Cancelled, usage(1), 400)
            .unwrap_err(),
        TaskError::InvalidTransition
    );
    let long = "结论".repeat(MAX_RESULT_BYTES);
    let done = tasks
        .finish(
            &created.id,
            TaskEnd::Completed { result: long },
            usage(7),
            400,
        )
        .unwrap();
    assert_eq!(done.status, TaskStatus::Completed);
    let result = done.result.clone().unwrap();
    assert!(result.len() <= MAX_RESULT_BYTES && result.ends_with("（已截断）"));
    assert_eq!(done.usage, Some(usage(7)));
    assert!(done.notice_pending());

    // 主动私聊至多一次；没写出时回到等待，随下一条私聊回复附带。
    let snapshot = tasks.snapshot().unwrap();
    assert_eq!(snapshot.due_proactive().unwrap().id, created.id);
    tasks
        .claim(&created.id, DeliveryChannel::Proactive, 500)
        .unwrap();
    assert!(
        tasks
            .snapshot()
            .unwrap()
            .due_passive("alice", "m1")
            .is_none()
    );
    tasks
        .record_delivery(&created.id, DeliveryResult::NotSent, 501)
        .unwrap();
    let snapshot = tasks.snapshot().unwrap();
    assert!(snapshot.due_proactive().is_none());
    assert_eq!(snapshot.due_passive("alice", "m1").unwrap().id, created.id);
    assert!(snapshot.due_passive("bob", "m1").is_none());
    tasks
        .claim(
            &created.id,
            DeliveryChannel::Passive {
                message_id: "m1".into(),
            },
            502,
        )
        .unwrap();
    let delivered = tasks.record_delivery(&created.id, sent(), 503).unwrap();
    assert_eq!(
        delivered.notice.as_ref().unwrap().status,
        NoticeStatus::Delivered
    );
    assert_eq!(delivered.settled_at(), Some(503));
    kernel.stop_all().await.unwrap();

    let (kernel, tasks) = world.open().await.unwrap();
    assert!(tasks.snapshot().unwrap().tasks[0] == delivered);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn owners_are_isolated_and_limits_refuse_without_writing() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let alice = owner("alice");
    for index in 0..MAX_ACTIVE_PER_OWNER {
        tasks
            .create(&alice, spec("并行任务"), &format!("active-{index}"), 100)
            .unwrap();
    }
    let before = world.raw();
    assert_eq!(
        tasks
            .create(&alice, spec("第五个"), "active-extra", 100)
            .unwrap_err(),
        TaskError::LimitReached
    );
    assert_eq!(world.raw(), before, "拒绝时不写状态");
    // 别人的任务如同不存在；自己的排队任务直接取消。
    let id = tasks.snapshot().unwrap().tasks[0].id.clone();
    assert_eq!(
        tasks.cancel("bob", &id, 150).unwrap_err(),
        TaskError::NotFound
    );
    let cancelled = tasks.cancel("alice", &id, 150).unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    assert!(cancelled.notice.is_none());
    assert_eq!(
        tasks.cancel("alice", &id, 151).unwrap_err(),
        TaskError::InvalidTransition
    );
    // 24 小时内的派发数另有上限，结束了的也计入。
    let bob = owner("bob");
    for index in 0..MAX_CREATED_PER_OWNER_PER_DAY {
        let task = tasks
            .create(&bob, spec("短任务"), &format!("daily-{index}"), 1_000)
            .unwrap();
        tasks.cancel("bob", &task.id, 1_001).unwrap();
    }
    assert_eq!(
        tasks
            .create(&bob, spec("超出"), "daily-extra", 1_002)
            .unwrap_err(),
        TaskError::LimitReached
    );
    assert!(
        tasks
            .create(&bob, spec("第二天"), "next-day", 1_000 + DAY_MS)
            .is_ok()
    );
    // 无效输入不写入。
    let mut bad = spec("坏");
    bad.max_steps = 0;
    assert_eq!(
        tasks.create(&bob, bad, "bad", 2_000).unwrap_err(),
        TaskError::InvalidInput
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn running_cancellation_is_requested_then_confirmed_by_the_executor() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("长任务"), "c", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let requested = tasks.cancel("alice", &task.id, 30).unwrap();
    assert_eq!(requested.status, TaskStatus::Running);
    assert!(requested.cancel_requested);
    assert_eq!(
        tasks.cancel("alice", &task.id, 31).unwrap_err(),
        TaskError::InvalidTransition
    );
    let ended = tasks
        .finish(&task.id, TaskEnd::Cancelled, usage(3), 40)
        .unwrap();
    assert_eq!(ended.status, TaskStatus::Cancelled);
    assert!(ended.notice.is_none(), "用户取消的任务不再通知");
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn cancellation_wins_over_an_error_arriving_at_the_same_time() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("取消竞态"), "cancel-race", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    tasks.cancel("alice", &task.id, 30).unwrap();
    let saved = tasks
        .finish(
            &task.id,
            TaskEnd::Failed {
                reason: "模型已返回错误".into(),
            },
            usage(0),
            40,
        )
        .unwrap();
    assert_eq!(saved.status, TaskStatus::Cancelled);
    assert!(saved.notice.is_none());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn restart_interrupts_running_tasks_and_marks_in_flight_notices_unknown() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let alice = owner("alice");
    let running = tasks.create(&alice, spec("执行中"), "run", 10).unwrap();
    tasks.start(&running.id, 20).unwrap();
    let cancelling = tasks.create(&alice, spec("取消中"), "cancel", 10).unwrap();
    tasks.start(&cancelling.id, 20).unwrap();
    tasks.cancel("alice", &cancelling.id, 25).unwrap();
    let failed = tasks.create(&alice, spec("会失败"), "fail", 10).unwrap();
    tasks.start(&failed.id, 20).unwrap();
    tasks
        .finish(
            &failed.id,
            TaskEnd::Failed {
                reason: "模型不可用".into(),
            },
            usage(0),
            30,
        )
        .unwrap();
    tasks
        .claim(&failed.id, DeliveryChannel::Proactive, 40)
        .unwrap();
    let queued = tasks.create(&owner("bob"), spec("排队"), "q", 10).unwrap();
    kernel.stop_all().await.unwrap();
    // 迁移前的任务没有执行日志，继续保留“不猜测、不自动重跑”的旧语义。
    let mut old: Value = serde_json::from_slice(&world.raw().unwrap()).unwrap();
    old["tasks"][0].as_object_mut().unwrap().remove("execution");
    world.write(serde_json::to_vec(&old).unwrap());

    let (kernel, tasks) = world.open().await.unwrap();
    let snapshot = tasks.snapshot().unwrap();
    let interrupted = snapshot.get(&running.id).unwrap();
    assert_eq!(interrupted.status, TaskStatus::Interrupted);
    assert!(interrupted.notice_pending(), "中断也要告诉用户");
    let cancelled = snapshot.get(&cancelling.id).unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    assert!(cancelled.notice.is_none());
    let unknown = snapshot.get(&failed.id).unwrap();
    assert_eq!(
        unknown.notice.as_ref().unwrap().status,
        NoticeStatus::Unknown
    );
    // 结果未知不重发，也不随回复附带。
    assert!(
        snapshot
            .due_proactive()
            .is_some_and(|task| task.id == running.id)
    );
    assert_eq!(snapshot.get(&queued.id).unwrap().status, TaskStatus::Queued);
    assert_eq!(snapshot.queued().len(), 1, "排队任务重启后照常等待执行");
    kernel.stop_all().await.unwrap();

    // 再次重启不再改写。
    let before = world.raw();
    let (kernel, _) = world.open().await.unwrap();
    assert_eq!(world.raw(), before);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn corrupt_unknown_or_inconsistent_ledgers_refuse_to_open_and_keep_bytes() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("任务"), "c", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    tasks
        .finish(
            &task.id,
            TaskEnd::Completed {
                result: "完成".into(),
            },
            usage(2),
            30,
        )
        .unwrap();
    kernel.stop_all().await.unwrap();
    let good: Value = serde_json::from_slice(&world.raw().unwrap()).unwrap();
    let mutations: Vec<Mutation> = vec![
        Box::new(|v| v["format_version"] = 999.into()),
        Box::new(|v| v["tasks"][0]["id"] = "task-forged".into()),
        Box::new(|v| v["tasks"][0]["result"] = Value::Null),
        Box::new(|v| v["tasks"][0]["usage"]["tool_calls"] = 9_999.into()),
        Box::new(|v| v["tasks"][0]["notice"] = Value::Null),
        Box::new(|v| v["tasks"][0]["extra"] = true.into()),
        Box::new(|v| v["tasks"][0]["spec"]["max_steps"] = 0.into()),
        Box::new(|v| v["tasks"][0]["started_at_ms"] = 1.into()),
    ];
    for mutate in mutations {
        let mut value = good.clone();
        mutate(&mut value);
        let bytes = serde_json::to_vec(&value).unwrap();
        world.write(bytes.clone());
        assert!(world.open().await.is_err());
        assert_eq!(world.raw(), Some(bytes), "拒绝打开时保留原字节");
    }
    let mut duplicate = serde_json::to_vec(&good).unwrap();
    let text = String::from_utf8(duplicate.clone()).unwrap();
    duplicate = text
        .replacen(
            "\"format_version\":2",
            "\"format_version\":2,\"format_version\":2",
            1,
        )
        .into_bytes();
    world.write(duplicate);
    assert!(world.open().await.is_err());
}

#[tokio::test]
async fn initialized_task_ledger_disappearance_is_not_a_fresh_empty_database() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("保留任务"), "kept-source", 10)
        .unwrap();
    let original = world.raw().unwrap();
    kernel.stop_all().await.unwrap();
    world.state.hide_ledger.store(true, Ordering::SeqCst);
    assert!(
        world.open().await.is_err(),
        "已初始化账本缺失必须拒绝，不能当空状态启动"
    );
    world.state.hide_ledger.store(false, Ordering::SeqCst);
    assert_eq!(world.raw().unwrap(), original, "拒绝启动不能覆盖原始记录");
    let (kernel, tasks) = world.open().await.unwrap();
    assert_eq!(tasks.snapshot().unwrap().tasks, vec![task]);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn legacy_ledger_is_sealed_only_after_validation_without_changing_queued_records() {
    let source = World::new();
    let (kernel, tasks) = source.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("旧账本"), "legacy-source", 10)
        .unwrap();
    let original = source.raw().unwrap();
    kernel.stop_all().await.unwrap();
    let legacy = World::new();
    legacy.write(original.clone());
    let plugin = PluginId::new(TASK_PLUGIN_ID).unwrap();
    assert!(
        legacy
            .state
            .get(&plugin, "tasks.initialized.v1")
            .unwrap()
            .is_none()
    );
    let (kernel, tasks) = legacy.open().await.unwrap();
    assert_eq!(tasks.snapshot().unwrap().tasks, vec![task]);
    assert_eq!(legacy.raw().unwrap(), original);
    assert!(
        legacy
            .state
            .get(&plugin, "tasks.initialized.v1")
            .unwrap()
            .is_some()
    );
    kernel.stop_all().await.unwrap();
    let corrupt = World::new();
    corrupt.write(b"not-json-original-evidence".to_vec());
    assert!(corrupt.open().await.is_err());
    assert!(
        corrupt
            .state
            .get(&plugin, "tasks.initialized.v1")
            .unwrap()
            .is_none()
    );
    assert_eq!(corrupt.raw().unwrap(), b"not-json-original-evidence");
}

#[tokio::test]
async fn partial_initialization_and_corrupt_seal_never_clear_or_execute_task_data() {
    let world = World::new();
    let plugin = PluginId::new(TASK_PLUGIN_ID).unwrap();
    world.state.fail_initialized.store(true, Ordering::SeqCst);
    assert!(
        world.open().await.is_err(),
        "标记确认失败不能返回可用控制器"
    );
    let empty = world.raw().unwrap();
    assert!(
        world
            .state
            .get(&plugin, "tasks.initialized.v1")
            .unwrap()
            .is_none()
    );
    world.state.fail_initialized.store(false, Ordering::SeqCst);
    let (kernel, tasks) = world.open().await.unwrap();
    assert!(tasks.snapshot().unwrap().tasks.is_empty());
    assert_eq!(world.raw().unwrap(), empty, "只补标记，不重写已落盘空账本");
    let task = tasks
        .create(&owner("alice"), spec("不可丢弃"), "known-source", 10)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let original = world.raw().unwrap();
    world
        .state
        .inner
        .set(
            &plugin,
            "tasks.initialized.v1".into(),
            b"broken-seal".to_vec(),
        )
        .unwrap();
    assert!(world.open().await.is_err());
    assert_eq!(world.raw().unwrap(), original);
    world
        .state
        .inner
        .set(
            &plugin,
            "tasks.initialized.v1".into(),
            br#"{"format_version":1,"ledger":"tasks.v1"}"#.to_vec(),
        )
        .unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    assert_eq!(tasks.snapshot().unwrap().tasks, vec![task]);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn failed_writes_close_the_instance_instead_of_trusting_the_cache() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    world.state.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        tasks
            .create(&owner("alice"), spec("任务"), "c", 10)
            .unwrap_err(),
        TaskError::Storage
    );
    assert_eq!(tasks.snapshot().unwrap_err(), TaskError::Unavailable);
    assert_eq!(
        tasks.scheduling_snapshot().unwrap_err(),
        TaskError::Unavailable
    );
    assert_eq!(
        tasks.delivery_snapshot().unwrap_err(),
        TaskError::Unavailable
    );
    world.state.fail.store(false, Ordering::SeqCst);
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    assert!(tasks.snapshot().unwrap().tasks.is_empty());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn lost_create_acknowledgement_reopens_committed_task_without_duplicate_creation() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let alice = owner("alice");
    world.state.fail_after.store(true, Ordering::SeqCst);
    assert_eq!(
        tasks
            .create(&alice, spec("已写入但确认丢失"), "lost-create", 10)
            .unwrap_err(),
        TaskError::Storage
    );
    assert_eq!(tasks.snapshot().unwrap_err(), TaskError::Unavailable);
    world.state.fail_after.store(false, Ordering::SeqCst);
    let committed = world.raw().unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let recovered = tasks.snapshot().unwrap().tasks.pop().unwrap();
    assert_eq!(recovered.status, TaskStatus::Queued);
    assert!(
        tasks
            .create(&alice, spec("不得覆盖原任务"), "lost-create", 20)
            .unwrap()
            == recovered
    );
    assert_eq!(tasks.snapshot().unwrap().tasks.len(), 1);
    assert_eq!(world.raw().unwrap(), committed);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn lost_finish_acknowledgement_reopens_completed_result_and_never_rewinds_it() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let alice = owner("alice");
    let task = tasks
        .create(&alice, spec("确认丢失的结局"), "lost-finish", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    world.state.fail_after.store(true, Ordering::SeqCst);
    assert_eq!(
        tasks
            .finish(
                &task.id,
                TaskEnd::Completed {
                    result: "已有真实回执".into()
                },
                usage(1),
                30
            )
            .unwrap_err(),
        TaskError::Storage
    );
    assert_eq!(
        tasks.get_owned(&alice, &task.id).unwrap_err(),
        TaskError::Unavailable
    );
    world.state.fail_after.store(false, Ordering::SeqCst);
    let committed = world.raw().unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let recovered = tasks.get_owned(&alice, &task.id).unwrap().unwrap();
    assert_eq!(recovered.status, TaskStatus::Completed);
    assert_eq!(recovered.result.as_deref(), Some("已有真实回执"));
    assert_eq!(recovered.usage, Some(usage(1)));
    assert_eq!(
        tasks.start(&task.id, 40).unwrap_err(),
        TaskError::InvalidTransition
    );
    assert_eq!(world.raw().unwrap(), committed);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn notices_expire_after_the_window_and_full_ledgers_prune_only_settled_tasks() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("旧任务"), "old", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    tasks
        .finish(
            &task.id,
            TaskEnd::Failed {
                reason: "超时".into(),
            },
            usage(0),
            30,
        )
        .unwrap();
    assert_eq!(
        tasks.expire(&task.id, 30 + DELIVERY_WINDOW_MS).unwrap_err(),
        TaskError::InvalidTransition
    );
    assert_eq!(
        tasks
            .snapshot()
            .unwrap()
            .overdue(31 + DELIVERY_WINDOW_MS)
            .count(),
        1
    );
    let expired = tasks.expire(&task.id, 31 + DELIVERY_WINDOW_MS).unwrap();
    assert_eq!(expired.notice.unwrap().status, NoticeStatus::Expired);
    // 填满账本：每位用户一个已取消的任务；再派发时腾出最早了结的一条。
    let mut now = 100 + DELIVERY_WINDOW_MS;
    for index in 1..MAX_TASKS {
        let user = format!("user-{index}");
        let created = tasks
            .create(&owner(&user), spec("填充"), "fill", now)
            .unwrap();
        tasks.cancel(&user, &created.id, now).unwrap();
        now += 1;
    }
    assert_eq!(tasks.snapshot().unwrap().tasks.len(), MAX_TASKS);
    tasks
        .create(&owner("newcomer"), spec("新任务"), "new", now)
        .unwrap();
    let snapshot = tasks.snapshot().unwrap();
    assert_eq!(snapshot.tasks.len(), MAX_TASKS);
    assert!(snapshot.get(&task.id).is_none(), "最早了结的一条被腾出");
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn technical_wait_checkpoint_compare_does_not_resume_a_new_user_block() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let alice = owner("alice");
    let task = tasks
        .create_authorized(
            &alice,
            spec("条件竞态"),
            "wait-checkpoint-cas",
            vec!["等待构建后核对结果".into()],
            Some(TaskCompletionContract::Unspecified),
            Some(2000),
            10,
        )
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let mut old = checkpoint("alice");
    old.remaining = None;
    old.result = Some("构建仍在运行".into());
    tasks.checkpoint(&task.id, old.clone(), 1000, 30).unwrap();
    tasks
        .finish(
            &task.id,
            TaskEnd::Blocked {
                result: old.result.clone().unwrap(),
            },
            usage(0),
            40,
        )
        .unwrap();
    // 在技术监听者读取 old 后，另一可信入口先接续了原任务。
    let resumed = tasks
        .resume_blocked(&alice, &task.id, "用户已要求核对构建", 50)
        .unwrap();
    let mut current = resumed.execution.unwrap().checkpoint.unwrap();
    current.remaining = None;
    current.provider_requests += 1;
    current.result = Some("构建完成，但需要用户决定发布目标".into());
    tasks
        .checkpoint(&task.id, current.clone(), 1000, 60)
        .unwrap();
    let blocked = tasks
        .finish(
            &task.id,
            TaskEnd::Blocked {
                result: current.result.clone().unwrap(),
            },
            usage(0),
            70,
        )
        .unwrap();
    let saved = world.raw().unwrap();
    assert_eq!(
        tasks.resume_blocked_checkpoint(&alice, &task.id, &old, "旧文件条件满足", 80),
        Err(TaskError::InvalidTransition)
    );
    assert_eq!(world.raw().unwrap(), saved);
    assert_eq!(
        tasks.get_owned(&alice, &task.id).unwrap(),
        Some(blocked.clone())
    );
    // 新检查点的精确比较仍允许其自己的可信条件接续，不重置合同、授权、期限或用量。
    let resumed = tasks
        .resume_blocked_checkpoint(&alice, &task.id, &current, "用户已明确发布目标", 90)
        .unwrap();
    assert_eq!(resumed.status, TaskStatus::Running);
    let execution = resumed.execution.unwrap();
    assert_eq!(execution.recoveries, 2);
    assert_eq!(execution.deadline_at_ms, Some(1000));
    assert_eq!(execution.authorization_deadline_at_ms, Some(2000));
    assert_eq!(
        execution.review_messages,
        blocked.execution.unwrap().review_messages
    );
    assert_eq!(
        execution.checkpoint.unwrap().used_tool_calls,
        old.used_tool_calls
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn condition_resume_keeps_the_original_task_checkpoint_budget_and_deadline() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("等待构建"), "condition-resume", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let mut frame = checkpoint("alice");
    frame.result = Some("构建尚未完成".into());
    frame.remaining = None;
    tasks.checkpoint(&task.id, frame.clone(), 1000, 30).unwrap();
    tasks
        .finish(
            &task.id,
            TaskEnd::Blocked {
                result: "构建尚未完成".into(),
            },
            usage(0),
            40,
        )
        .unwrap();
    assert_eq!(
        tasks.resume_blocked(&owner("bob"), &task.id, "构建退出", 50),
        Err(TaskError::NotFound)
    );
    let resumed = tasks
        .resume_blocked(&owner("alice"), &task.id, "本地构建已退出，只检查日志", 50)
        .unwrap();
    assert_eq!(resumed.id, task.id);
    assert_eq!(resumed.spec.max_steps, task.spec.max_steps);
    assert_eq!(resumed.status, TaskStatus::Running);
    let execution = resumed.execution.unwrap();
    assert_eq!(execution.deadline_at_ms, Some(1000));
    assert_eq!(execution.resume_at_ms, Some(50));
    assert_eq!(execution.recoveries, 1);
    assert_eq!(
        execution.checkpoint.as_ref().unwrap().used_tool_calls,
        frame.used_tool_calls
    );
    assert_eq!(
        execution.checkpoint.as_ref().unwrap().provider_requests,
        frame.provider_requests
    );
    assert_eq!(execution.checkpoint.as_ref().unwrap().result, None);
    assert!(execution.checkpoint.as_ref().unwrap().messages.len() > frame.messages.len());
    assert_eq!(
        tasks.resume_blocked(&owner("alice"), &task.id, "重复事件", 60),
        Err(TaskError::InvalidTransition)
    );
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let snapshot = tasks.snapshot().unwrap();
    let restored = snapshot.get(&task.id).unwrap();
    assert_eq!(restored.id, task.id);
    assert_eq!(
        restored.execution.as_ref().unwrap().deadline_at_ms,
        Some(1000)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn condition_resume_text_contract_completes_without_a_progress_tool_or_extra_budget() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let contract = TaskCompletionContract::Text {
        required_fragments: vec!["交付文本已完成".into()],
        json_equals: None,
        valid_json: false,
        json_required_pointers: vec![],
    };
    let review_messages = vec!["等输入到位后完成这份文本".into()];
    let task = tasks
        .create_authorized(
            &owner("alice"),
            TaskSpec {
                max_steps: 1,
                ..spec("等待文本输入")
            },
            "text-condition-resume",
            review_messages.clone(),
            Some(contract.clone()),
            Some(1000),
            10,
        )
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let mut frame = checkpoint("alice");
    // 正在等待的检查点没有最终文本；阻塞答复由 TaskEnd 单独持久化。
    frame.remaining = Some("等待输入到位".into());
    tasks.checkpoint(&task.id, frame.clone(), 900, 30).unwrap();
    tasks
        .finish(
            &task.id,
            TaskEnd::Blocked {
                result: "等待输入到位".into(),
            },
            usage(0),
            40,
        )
        .unwrap();
    let resumed = tasks
        .resume_blocked(&owner("alice"), &task.id, "宿主已确认输入到位", 50)
        .unwrap();
    assert_eq!(resumed.id, task.id);
    assert_eq!(resumed.spec.max_steps, 1);
    assert_eq!(resumed.status, TaskStatus::Running);
    let execution = resumed.execution.as_ref().unwrap();
    assert!(execution.completion_contract.as_ref() == Some(&contract));
    assert_eq!(execution.review_messages, review_messages);
    assert_eq!(execution.recoveries, 1);
    assert_eq!(execution.deadline_at_ms, Some(900));
    assert_eq!(execution.authorization_deadline_at_ms, Some(1000));
    let mut continued = execution.checkpoint.clone().unwrap();
    assert_eq!(continued.remaining, None);
    assert_eq!(continued.result, None);
    assert_eq!(continued.used_tool_calls, frame.used_tool_calls);
    assert_eq!(continued.delegated_tool_calls, frame.delegated_tool_calls);
    assert_eq!(continued.provider_requests, frame.provider_requests);
    let event = serde_json::to_string(continued.messages.last().unwrap()).unwrap();
    assert!(event.contains("宿主已确认输入到位"));
    assert!(event.contains("等待输入到位"));
    assert!(event.contains("blocked_task_resumed"));
    assert!(!event.contains("task_progress"));
    // 零工具纯文本轮次保存实际结果，再由同一固定合同验收。
    continued.provider_requests += 1;
    continued.result = Some("交付文本已完成".into());
    tasks.checkpoint(&task.id, continued, 900, 60).unwrap();
    let completed = tasks
        .finish(
            &task.id,
            TaskEnd::Completed {
                result: "交付文本已完成".into(),
            },
            TaskUsage {
                tool_calls: 0,
                model_requests: 2,
            },
            70,
        )
        .unwrap();
    assert_eq!(completed.status, TaskStatus::Completed);
    assert_eq!(completed.result.as_deref(), Some("交付文本已完成"));
    assert_eq!(completed.usage.unwrap().tool_calls, 0);
    let final_execution = completed.execution.unwrap();
    assert_eq!(final_execution.recoveries, 1);
    assert_eq!(final_execution.deadline_at_ms, Some(900));
    assert_eq!(final_execution.authorization_deadline_at_ms, Some(1000));
    assert!(final_execution.completion_contract == Some(contract));
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn condition_resume_text_contract_still_rejects_unknown_effects_and_expired_deadlines() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    for (source, unknown, at) in [("text-unknown", true, 50), ("text-expired", false, 900)] {
        let task = tasks
            .create_authorized(
                &owner("alice"),
                spec("保留纯文本任务现场"),
                source,
                vec![],
                Some(TaskCompletionContract::Text {
                    required_fragments: vec!["交付文本已完成".into()],
                    json_equals: None,
                    valid_json: false,
                    json_required_pointers: vec![],
                }),
                Some(1000),
                10,
            )
            .unwrap();
        tasks.start(&task.id, 20).unwrap();
        let mut frame = checkpoint("alice");
        if unknown {
            frame.pending_calls.push(eve_llm_api::ToolCall {
                id: "unknown".into(),
                name: "write_file".into(),
                arguments: serde_json::json!({}),
            });
            frame.used_tool_calls = 1;
            frame.tool_batches = 1;
        }
        tasks.checkpoint(&task.id, frame, 900, 30).unwrap();
        tasks
            .finish(
                &task.id,
                TaskEnd::Blocked {
                    result: "等待确认".into(),
                },
                usage(0),
                40,
            )
            .unwrap();
        let before = tasks.snapshot().unwrap().get(&task.id).unwrap().clone();
        assert_eq!(
            tasks.resume_blocked(&owner("alice"), &task.id, "条件已满足", at),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(tasks.snapshot().unwrap().get(&task.id), Some(&before));
    }
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn condition_resume_waits_for_delivery_without_spending_budget_or_extending_deadlines() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create_bounded(
            &owner("alice"),
            spec("等待通知完成"),
            "delivery-race",
            1000,
            10,
        )
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let mut frame = checkpoint("alice");
    frame.result = Some("等待构建退出".into());
    frame.remaining = None;
    tasks.checkpoint(&task.id, frame.clone(), 900, 30).unwrap();
    tasks
        .finish(
            &task.id,
            TaskEnd::Blocked {
                result: "等待构建退出".into(),
            },
            usage(0),
            40,
        )
        .unwrap();
    let sending = tasks
        .claim(&task.id, DeliveryChannel::Proactive, 50)
        .unwrap();
    assert_eq!(
        tasks.resume_blocked(&owner("alice"), &task.id, "构建已退出", 60),
        Err(TaskError::Busy)
    );
    assert_eq!(tasks.snapshot().unwrap().get(&task.id), Some(&sending));
    assert_eq!(
        tasks.resume_blocked(&owner("alice"), &task.id, "原期限已过", 900),
        Err(TaskError::InvalidTransition),
        "通知发送中不能掩盖已经失效的原任务期限"
    );
    assert_eq!(tasks.snapshot().unwrap().get(&task.id), Some(&sending));
    tasks.record_delivery(&task.id, sent(), 70).unwrap();
    let resumed = tasks
        .resume_blocked(&owner("alice"), &task.id, "构建已退出", 80)
        .unwrap();
    assert_eq!(resumed.id, task.id);
    assert_eq!(resumed.spec.max_steps, task.spec.max_steps);
    let execution = resumed.execution.unwrap();
    assert_eq!(execution.recoveries, 1);
    assert_eq!(execution.deadline_at_ms, Some(900));
    assert_eq!(execution.authorization_deadline_at_ms, Some(1000));
    let restored = execution.checkpoint.unwrap();
    assert_eq!(restored.used_tool_calls, frame.used_tool_calls);
    assert_eq!(restored.provider_requests, frame.provider_requests);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn condition_resume_rejects_unknown_delivery_after_restart() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(
            &owner("alice"),
            spec("保留未知投递"),
            "unknown-delivery",
            10,
        )
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let mut frame = checkpoint("alice");
    frame.result = Some("等待条件".into());
    frame.remaining = None;
    tasks.checkpoint(&task.id, frame, u64::MAX, 30).unwrap();
    tasks
        .finish(
            &task.id,
            TaskEnd::Blocked {
                result: "等待条件".into(),
            },
            usage(0),
            40,
        )
        .unwrap();
    tasks
        .claim(&task.id, DeliveryChannel::Proactive, 50)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let saved = tasks.snapshot().unwrap().get(&task.id).unwrap().clone();
    assert_eq!(saved.notice.as_ref().unwrap().status, NoticeStatus::Unknown);
    assert_eq!(
        tasks.resume_blocked(&owner("alice"), &task.id, "条件已满足", 60),
        Err(TaskError::InvalidTransition)
    );
    assert_eq!(tasks.snapshot().unwrap().get(&task.id), Some(&saved));
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn condition_resume_rejects_unknown_side_effects_and_expired_deadlines() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    for (source, unknown, now) in [("unknown", true, 50), ("expired", false, 1000)] {
        let task = tasks
            .create(&owner("alice"), spec("保留现场"), source, 10)
            .unwrap();
        tasks.start(&task.id, 20).unwrap();
        let mut frame = checkpoint("alice");
        if unknown {
            frame.pending_calls.push(eve_llm_api::ToolCall {
                id: "unknown".into(),
                name: "write_file".into(),
                arguments: serde_json::json!({}),
            });
            frame.used_tool_calls = 1;
            frame.tool_batches = 1;
        }
        tasks.checkpoint(&task.id, frame, 1000, 30).unwrap();
        tasks
            .finish(
                &task.id,
                TaskEnd::Blocked {
                    result: "等待核对".into(),
                },
                usage(0),
                40,
            )
            .unwrap();
        let before = tasks.snapshot().unwrap().get(&task.id).unwrap().clone();
        assert_eq!(
            tasks.resume_blocked(&owner("alice"), &task.id, "条件成立", now),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(tasks.snapshot().unwrap().get(&task.id).unwrap(), &before);
    }
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn cancelling_a_blocked_task_prevents_future_condition_resume() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("等待后撤回"), "cancel-blocked", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let mut frame = checkpoint("alice");
    frame.remaining = None;
    frame.result = Some("等待条件".into());
    tasks.checkpoint(&task.id, frame, u64::MAX, 30).unwrap();
    tasks
        .finish(
            &task.id,
            TaskEnd::Blocked {
                result: "等待条件".into(),
            },
            usage(0),
            40,
        )
        .unwrap();
    let cancelled = tasks.cancel("alice", &task.id, 50).unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    assert_eq!(
        tasks.resume_blocked(&owner("alice"), &task.id, "旧条件满足", 60),
        Err(TaskError::InvalidTransition)
    );
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    assert_eq!(
        tasks.snapshot().unwrap().get(&task.id).unwrap().status,
        TaskStatus::Cancelled
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn bounded_create_keeps_original_authorization_deadline_atomically_across_restart() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let task = admin
        .create_bounded(&owner("a"), spec("原授权目标"), "bounded", 1000, 10)
        .unwrap();
    assert_eq!(
        task.execution
            .as_ref()
            .unwrap()
            .authorization_deadline_at_ms,
        Some(1000)
    );
    let duplicate = admin
        .create_bounded(&owner("a"), spec("不能刷新期限"), "bounded", 2000, 20)
        .unwrap();
    assert_eq!(
        duplicate
            .execution
            .as_ref()
            .unwrap()
            .authorization_deadline_at_ms,
        Some(1000)
    );
    assert!(
        admin
            .create_bounded(&owner("a"), spec("已过期"), "expired", 30, 30)
            .is_err()
    );
    let mut foreign = owner("a");
    foreign.session_id = "other-window".into();
    assert!(
        admin
            .create_bounded(&foreign, spec("不能跨范围"), "bounded", 2000, 20)
            .is_err()
    );
    admin.start(&task.id, 21).unwrap();
    assert!(
        admin
            .checkpoint(&task.id, checkpoint("a"), 2000, 22)
            .is_err()
    );
    admin
        .checkpoint(&task.id, checkpoint("a"), 1000, 22)
        .unwrap();
    kernel.stop_all().await.unwrap();
    drop(kernel);
    let (_kernel, restored) = world.open().await.unwrap();
    assert_eq!(
        restored
            .snapshot()
            .unwrap()
            .get(&task.id)
            .unwrap()
            .execution
            .as_ref()
            .unwrap()
            .authorization_deadline_at_ms,
        Some(1000)
    );
}

#[tokio::test]
async fn oversized_escaped_context_keeps_committed_tasks_and_handle_usable_after_rejection() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let large = |owner: &TaskOwner| {
        let mut task = spec("保留完整上下文");
        task.context = Some(DelegatedContext {
            owner: owner.clone(),
            messages_json: serde_json::to_string(&vec![eve_llm_api::ChatMessage::text(
                eve_llm_api::ChatRole::User,
                "\\\"".repeat(MAX_STATE_BYTES / 14 + 64 * 1024),
            )])
            .unwrap(),
        });
        task
    };
    let alice = owner("large-alice");
    let original = admin
        .create(&alice, large(&alice), "first-large", 100)
        .unwrap();
    let before = world.raw().unwrap();
    let bob = owner("large-bob");
    assert!(matches!(
        admin.create(&bob, large(&bob), "rejected-large", 101),
        Err(TaskError::LimitReached)
    ));
    assert_eq!(world.raw().unwrap(), before);
    assert!(admin.get_owned(&alice, &original.id).unwrap() == Some(original.clone()));
    let small = admin
        .create(&bob, spec("另一个小任务"), "small", 102)
        .unwrap();
    admin.start(&small.id, 103).unwrap();
    admin
        .finish(
            &small.id,
            TaskEnd::Completed {
                result: "已核对".into(),
            },
            usage(0),
            104,
        )
        .unwrap();
    assert!(admin.get_owned(&alice, &original.id).unwrap() == Some(original.clone()));
    kernel.stop_all().await.unwrap();
    let (kernel, restored) = world.open().await.unwrap();
    assert!(restored.get_owned(&alice, &original.id).unwrap() == Some(original));
    assert_eq!(
        restored.get_owned(&bob, &small.id).unwrap().unwrap().status,
        TaskStatus::Completed
    );
    assert_eq!(restored.summaries_for(&bob).unwrap().len(), 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn scoped_task_directory_omits_heavy_context_and_full_reads_keep_both_identity_fields() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let alice = owner("alice");
    let mut heavy = spec("自己的任务");
    heavy.context = Some(DelegatedContext {
        owner: alice.clone(),
        messages_json: serde_json::to_string(&vec![eve_llm_api::ChatMessage::text(
            eve_llm_api::ChatRole::User,
            "CONFIDENTIAL_CONTEXT_秘密".repeat(10_000),
        )])
        .unwrap(),
    });
    let own = admin.create(&alice, heavy, "own", 100).unwrap();
    let other_session = TaskOwner {
        session_id: "another-session".into(),
        ..alice.clone()
    };
    let other_user = TaskOwner {
        user_id: "bob".into(),
        ..alice.clone()
    };
    let foreign = admin
        .create(&other_session, spec("另一个会话"), "other-session", 101)
        .unwrap();
    admin
        .create(&other_user, spec("另一个用户"), "other-user", 102)
        .unwrap();
    let before = world.raw();
    let summaries = admin.summaries_for(&alice).unwrap();
    let directory = admin.scheduling_snapshot().unwrap();
    directory.validate().unwrap();
    assert_eq!(directory.tasks.len(), 3);
    let own_entry = directory
        .tasks
        .iter()
        .find(|entry| entry.id == own.id)
        .unwrap();
    assert!(own_entry.matches(&own));
    let delivery = admin.delivery_snapshot().unwrap();
    delivery.validate().unwrap();
    assert_eq!(delivery.tasks.len(), 3);
    assert!(
        delivery
            .tasks
            .iter()
            .find(|entry| entry.id == own.id)
            .unwrap()
            .matches(&own)
    );
    assert!(!format!("{delivery:?}").contains("CONFIDENTIAL_CONTEXT"));
    assert!(format!("{delivery:?}").len() < 4096);
    assert_eq!(world.raw(), before);
    let encoded_directory = format!("{directory:?}");
    assert!(!encoded_directory.contains("CONFIDENTIAL_CONTEXT"));
    assert!(encoded_directory.len() < 4096);
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, own.id);
    assert_eq!(summaries[0].instructions, own.spec.instructions);
    let encoded = serde_json::to_string(&summaries).unwrap();
    assert!(!encoded.contains("CONFIDENTIAL_CONTEXT"));
    assert!(encoded.len() < 1024);
    let body = admin.get_owned(&alice, &own.id).unwrap().unwrap();
    assert!(body == own);
    assert!(admin.get_owned(&alice, &foreign.id).unwrap().is_none());
    assert!(admin.get_owned(&other_session, &own.id).unwrap().is_none());
    assert!(admin.get_owned(&other_user, &own.id).unwrap().is_none());
    assert!(admin.get_owned(&alice, "unknown").unwrap().is_none());
    let invalid = TaskOwner {
        user_id: String::new(),
        ..alice.clone()
    };
    assert_eq!(admin.summaries_for(&invalid), Err(TaskError::InvalidInput));
    assert!(matches!(
        admin.get_owned(&invalid, &own.id),
        Err(TaskError::InvalidInput)
    ));
    assert!(matches!(
        admin.get_owned(&alice, ""),
        Err(TaskError::InvalidInput)
    ));
    assert_eq!(
        world.raw(),
        before,
        "只读目录、正文与错误不能写状态或新建任务"
    );
    let old = admin.clone();
    kernel.stop_all().await.unwrap();
    assert_eq!(old.summaries_for(&alice), Err(TaskError::Unavailable));
    assert!(matches!(
        old.get_owned(&alice, &own.id),
        Err(TaskError::Unavailable)
    ));
    let (kernel, restored) = world.open().await.unwrap();
    assert_eq!(restored.summaries_for(&alice).unwrap(), summaries);
    assert!(restored.get_owned(&alice, &own.id).unwrap().unwrap() == own);
    assert!(matches!(
        old.get_owned(&alice, &own.id),
        Err(TaskError::Unavailable)
    ));
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn selected_evidence_is_read_only_frozen_and_closed_after_ambiguous_commit() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let alice = tasks
        .create(&owner("alice"), spec("alice"), "alice-source", 1)
        .unwrap();
    let bob = tasks
        .create(&owner("bob"), spec("bob"), "bob-source", 2)
        .unwrap();
    let key = TaskEvidenceKey {
        id: alice.id.clone(),
        owner: alice.owner.clone(),
    };
    let before = world.raw();
    let view = tasks.evidence_snapshot(std::slice::from_ref(&key)).unwrap();
    assert_eq!(view.selected_len(), 1);
    assert_eq!(view.find(&alice.id).unwrap(), Some(&alice));
    assert!(matches!(view.find(&bob.id), Err(TaskError::Unavailable)));
    assert_eq!(
        world.raw(),
        before,
        "reading evidence does not save or claim a task"
    );
    assert!(matches!(
        tasks.evidence_snapshot(&[TaskEvidenceKey {
            id: alice.id.clone(),
            owner: bob.owner
        }]),
        Err(TaskError::CorruptState)
    ));
    let running = tasks.start(&alice.id, 3).unwrap();
    assert_eq!(
        view.find(&alice.id).unwrap().unwrap().status,
        TaskStatus::Queued
    );
    assert_eq!(
        tasks
            .evidence_snapshot(std::slice::from_ref(&key))
            .unwrap()
            .find(&alice.id)
            .unwrap(),
        Some(&running)
    );
    world.state.fail_after.store(true, Ordering::SeqCst);
    assert_eq!(tasks.cancel("alice", &alice.id, 4), Err(TaskError::Storage));
    world.state.fail_after.store(false, Ordering::SeqCst);
    assert!(matches!(
        tasks.evidence_snapshot(&[key]),
        Err(TaskError::Unavailable)
    ));
    kernel.stop_all().await.unwrap();
}

struct Archive {
    file: eve_task_plugin::FileTaskArchive,
    reads: std::sync::atomic::AtomicUsize,
    writes: std::sync::atomic::AtomicUsize,
    fail: AtomicBool,
    fail_after: AtomicBool,
    fail_compact: AtomicBool,
}
impl TaskArchiveStore for Archive {
    fn check_capacity(&self, plan: &TaskArchivePlan<'_>) -> TaskResult<()> {
        self.file.check_capacity(plan)
    }
    fn compact(&self, retained: &[String]) -> TaskResult<()> {
        if self.fail_compact.load(Ordering::SeqCst) {
            return Err(TaskError::Storage);
        }
        self.file.compact(retained)
    }
    fn get(&self, hash: &str) -> TaskResult<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.file.get(hash)
    }
    fn put(&self, hash: &str, bytes: &[u8]) -> TaskResult<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(TaskError::Storage);
        }
        self.file.put(hash, bytes)?;
        self.writes.fetch_add(1, Ordering::SeqCst);
        if self.fail_after.load(Ordering::SeqCst) {
            return Err(TaskError::Storage);
        }
        Ok(())
    }
}
impl World {
    fn archive(&self) -> Arc<Archive> {
        Arc::new(Archive {
            file: eve_task_plugin::FileTaskArchive::open(self._directory.path().join("archive"))
                .unwrap(),
            reads: Default::default(),
            writes: Default::default(),
            fail: AtomicBool::new(false),
            fail_after: AtomicBool::new(false),
            fail_compact: AtomicBool::new(false),
        })
    }
    async fn cold(&self, archive: Arc<Archive>) -> PluginResult<(Kernel, TaskController)> {
        let kernel = Kernel::with_services(KernelServices {
            state: self.state.clone(),
            ..KernelServices::default()
        });
        let plugin = TaskPlugin::new()?.with_archive(archive);
        let controller = plugin.controller();
        kernel.register(Box::new(plugin))?;
        kernel.start_all().await?;
        Ok((kernel, controller))
    }
    fn cold_path(&self, id: &str) -> std::path::PathBuf {
        let value: Value = serde_json::from_slice(&self.raw().unwrap()).unwrap();
        let entry = value["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["directory"]["id"] == id)
            .unwrap();
        self._directory.path().join("archive").join(format!(
            "{}.json",
            entry["record"]["hash"].as_str().unwrap()
        ))
    }
}
fn heavy_spec(user: &str) -> TaskSpec {
    let mut task = spec("保留完整上下文");
    task.context = Some(DelegatedContext {
        owner: owner(user),
        messages_json: serde_json::to_string(&vec![eve_llm_api::ChatMessage::text(
            eve_llm_api::ChatRole::User,
            "原始证据".repeat(20000),
        )])
        .unwrap(),
    });
    task
}
#[tokio::test]
async fn cold_ledger_keeps_catalog_light_and_hydrates_only_requested_owned_records() {
    let world = World::new();
    let archive = world.archive();
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    let first = tasks
        .create(&owner("alice"), heavy_spec("alice"), "cold-first", 10)
        .unwrap();
    let second = tasks
        .create(&owner("bob"), heavy_spec("bob"), "cold-second", 11)
        .unwrap();
    let raw = world.raw().unwrap();
    assert!(raw.len() < 5000, "索引不携带委派正文");
    let reads = archive.reads.load(Ordering::SeqCst);
    assert_eq!(tasks.scheduling_snapshot().unwrap().tasks.len(), 2);
    assert_eq!(tasks.delivery_snapshot().unwrap().tasks.len(), 2);
    assert_eq!(tasks.summaries_for(&owner("alice")).unwrap().len(), 1);
    assert_eq!(
        archive.reads.load(Ordering::SeqCst),
        reads,
        "轮询目录不读冷正文"
    );
    assert_eq!(tasks.get_owned(&owner("bob"), &first.id).unwrap(), None);
    assert_eq!(archive.reads.load(Ordering::SeqCst), reads);
    assert_eq!(
        tasks.get_owned(&owner("alice"), &first.id).unwrap(),
        Some(first.clone())
    );
    assert_eq!(archive.reads.load(Ordering::SeqCst), reads + 1);
    let writes = archive.writes.load(Ordering::SeqCst);
    let changed = tasks.start(&first.id, 20).unwrap();
    assert_eq!(
        archive.writes.load(Ordering::SeqCst),
        writes + 1,
        "变更不重写另一任务"
    );
    let view = tasks
        .evidence_snapshot(&[TaskEvidenceKey {
            id: first.id.clone(),
            owner: owner("alice"),
        }])
        .unwrap();
    assert_eq!(view.find(&first.id).unwrap(), Some(&changed));
    assert_eq!(view.find(&second.id), Err(TaskError::Unavailable));
    assert_eq!(tasks.snapshot().unwrap().tasks, vec![changed, second]);
    kernel.stop_all().await.unwrap();
}
#[tokio::test]
async fn legacy_task_migration_preserves_full_context_and_requires_archive_on_reopen() {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), heavy_spec("alice"), "migrate", 10)
        .unwrap();
    let old = world.raw().unwrap();
    kernel.stop_all().await.unwrap();
    let archive = world.archive();
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    assert_eq!(
        tasks.get_owned(&owner("alice"), &task.id).unwrap(),
        Some(task.clone())
    );
    assert!(world.raw().unwrap().len() < old.len() / 10);
    kernel.stop_all().await.unwrap();
    let before = world.raw();
    assert!(world.open().await.is_err());
    assert_eq!(world.raw(), before);
    let (kernel, tasks) = world.cold(archive).await.unwrap();
    assert_eq!(
        tasks.get_owned(&owner("alice"), &task.id).unwrap(),
        Some(task)
    );
    assert_eq!(world.raw(), before, "无恢复动作不重写索引");
    kernel.stop_all().await.unwrap();
}
#[tokio::test]
async fn cold_restart_validates_late_corruption_before_any_running_task_recovery_write() {
    let world = World::new();
    let archive = world.archive();
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    let first = tasks
        .create(&owner("alice"), spec("先恢复"), "first", 10)
        .unwrap();
    tasks.start(&first.id, 20).unwrap();
    tasks
        .checkpoint(&first.id, checkpoint("alice"), u64::MAX, 30)
        .unwrap();
    let second = tasks
        .create(&owner("bob"), spec("末页损坏"), "last", 10)
        .unwrap();
    let path = world.cold_path(&second.id);
    let bytes = std::fs::read(&path).unwrap();
    kernel.stop_all().await.unwrap();
    let before = world.raw();
    let writes = archive.writes.load(Ordering::SeqCst);
    std::fs::write(&path, b"damaged").unwrap();
    assert!(world.cold(archive.clone()).await.is_err());
    assert_eq!(world.raw(), before);
    assert_eq!(
        archive.writes.load(Ordering::SeqCst),
        writes,
        "未做部分恢复或写入"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"damaged");
    std::fs::write(&path, bytes).unwrap();
    let (kernel, tasks) = world.cold(archive).await.unwrap();
    let restored = tasks
        .get_owned(&owner("alice"), &first.id)
        .unwrap()
        .unwrap();
    let execution = restored.execution.unwrap();
    assert_eq!(execution.checkpoint, Some(checkpoint("alice")));
    assert_eq!(execution.recoveries, 1);
    assert_eq!(execution.deadline_at_ms, Some(u64::MAX));
    kernel.stop_all().await.unwrap();
}
#[tokio::test]
async fn cold_lost_index_acknowledgement_closes_old_handle_and_recovers_committed_body() {
    let world = World::new();
    let archive = world.archive();
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    world.state.fail_after.store(true, Ordering::SeqCst);
    assert_eq!(
        tasks.create(&owner("alice"), heavy_spec("alice"), "once", 10),
        Err(TaskError::Storage)
    );
    assert_eq!(tasks.snapshot().err(), Some(TaskError::Unavailable));
    world.state.fail_after.store(false, Ordering::SeqCst);
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.cold(archive).await.unwrap();
    let restored = tasks
        .create(&owner("alice"), spec("不得覆盖"), "once", 11)
        .unwrap();
    assert_eq!(restored.spec, heavy_spec("alice"));
    assert_eq!(tasks.snapshot().unwrap().tasks.len(), 1);
    kernel.stop_all().await.unwrap();
}
#[tokio::test]
async fn cold_migration_write_failure_never_replaces_legacy_root_or_claims_success() {
    for after in [false, true] {
        let world = World::new();
        let (kernel, tasks) = world.open().await.unwrap();
        tasks
            .create(&owner("alice"), heavy_spec("alice"), "keep", 10)
            .unwrap();
        kernel.stop_all().await.unwrap();
        let before = world.raw();
        let archive = world.archive();
        if after {
            archive.fail_after.store(true, Ordering::SeqCst);
        } else {
            archive.fail.store(true, Ordering::SeqCst);
        }
        assert!(world.cold(archive.clone()).await.is_err());
        assert_eq!(world.raw(), before);
        archive.fail_after.store(false, Ordering::SeqCst);
        archive.fail.store(false, Ordering::SeqCst);
        let (kernel, tasks) = world.cold(archive).await.unwrap();
        assert_eq!(tasks.snapshot().unwrap().tasks.len(), 1);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn cold_storage_does_not_raise_original_logical_capacity_or_write_rejected_payloads() {
    for cold in [false, true] {
        let world = World::new();
        let archive = world.archive();
        let (kernel, tasks) = if cold {
            world.cold(archive.clone()).await.unwrap()
        } else {
            world.open().await.unwrap()
        };
        let mut large = spec("原容量保持");
        large.context = Some(DelegatedContext {
            owner: owner("alice"),
            messages_json: serde_json::to_string(&vec![eve_llm_api::ChatMessage::text(
                eve_llm_api::ChatRole::User,
                "\"".repeat(MAX_STATE_BYTES / 8 + 64 * 1024),
            )])
            .unwrap(),
        });
        let first = tasks
            .create(&owner("alice"), large.clone(), "large-first", 10)
            .unwrap();
        let before = world.raw();
        let writes = archive.writes.load(Ordering::SeqCst);
        assert_eq!(
            tasks.create(&owner("alice"), large, "large-second", 11),
            Err(TaskError::LimitReached)
        );
        assert_eq!(world.raw(), before);
        assert_eq!(archive.writes.load(Ordering::SeqCst), writes);
        assert_eq!(
            tasks.get_owned(&owner("alice"), &first.id).unwrap(),
            Some(first)
        );
        kernel.stop_all().await.unwrap();
    }
}
#[tokio::test]
async fn ambiguous_cold_payload_write_closes_handle_without_publishing_new_index() {
    let world = World::new();
    let archive = world.archive();
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    let first = tasks
        .create(&owner("alice"), spec("保留旧结局"), "first", 10)
        .unwrap();
    let before = world.raw();
    archive.fail_after.store(true, Ordering::SeqCst);
    assert_eq!(tasks.start(&first.id, 20), Err(TaskError::Storage));
    assert_eq!(world.raw(), before);
    assert_eq!(tasks.snapshot().err(), Some(TaskError::Unavailable));
    assert_eq!(
        tasks.scheduling_snapshot().err(),
        Some(TaskError::Unavailable)
    );
    archive.fail_after.store(false, Ordering::SeqCst);
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.cold(archive).await.unwrap();
    assert_eq!(
        tasks.get_owned(&owner("alice"), &first.id).unwrap(),
        Some(first)
    );
    kernel.stop_all().await.unwrap();
}
#[tokio::test]
async fn missing_cold_record_never_becomes_missing_task_and_closes_cached_catalog() {
    let world = World::new();
    let archive = world.archive();
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    let first = tasks
        .create(&owner("alice"), spec("不能清空"), "first", 10)
        .unwrap();
    let before = world.raw();
    let path = world.cold_path(&first.id);
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(tasks.get_owned(&owner("alice"), &first.id).is_err());
    assert_eq!(
        tasks.scheduling_snapshot().err(),
        Some(TaskError::Unavailable)
    );
    assert_eq!(world.raw(), before);
    kernel.stop_all().await.unwrap();
    assert!(world.cold(archive.clone()).await.is_err());
    assert_eq!(world.raw(), before);
    std::fs::write(path, bytes).unwrap();
    let (kernel, tasks) = world.cold(archive).await.unwrap();
    assert_eq!(tasks.snapshot().unwrap().tasks, vec![first]);
    kernel.stop_all().await.unwrap();
}
#[tokio::test]
async fn cold_notice_inflight_recovery_remains_unknown_and_does_not_replay() {
    let world = World::new();
    let archive = world.archive();
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    let first = tasks
        .create(&owner("alice"), spec("结果保持"), "first", 10)
        .unwrap();
    tasks.start(&first.id, 20).unwrap();
    tasks
        .finish(
            &first.id,
            TaskEnd::Completed {
                result: "实际结果".into(),
            },
            usage(1),
            30,
        )
        .unwrap();
    tasks
        .claim(&first.id, DeliveryChannel::Proactive, 40)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.cold(archive).await.unwrap();
    let restored = tasks
        .get_owned(&owner("alice"), &first.id)
        .unwrap()
        .unwrap();
    assert_eq!(restored.result.as_deref(), Some("实际结果"));
    let notice = restored.notice.unwrap();
    assert_eq!(notice.status, NoticeStatus::Unknown);
    assert_eq!(notice.attempts[0].result, Some(DeliveryResult::Unknown));
    let before = world.raw();
    assert_eq!(
        tasks.claim(&first.id, DeliveryChannel::Proactive, 50),
        Err(TaskError::InvalidTransition)
    );
    assert_eq!(world.raw(), before);
    kernel.stop_all().await.unwrap();
}

#[test]
fn file_task_archive_is_exclusive_and_releases_its_lock_with_the_last_owner() {
    let root = tempfile::tempdir().unwrap();
    let archive = eve_task_plugin::FileTaskArchive::open(root.path()).unwrap();
    assert!(matches!(
        eve_task_plugin::FileTaskArchive::open(root.path()),
        Err(TaskError::Unavailable)
    ));
    drop(archive);
    assert!(eve_task_plugin::FileTaskArchive::open(root.path()).is_ok());
}
#[tokio::test]
async fn confirmed_checkpoint_replacement_reclaims_only_superseded_records() {
    let world = World::new();
    let archive = world.archive();
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    let task = tasks
        .create(&owner("alice"), heavy_spec("alice"), "checkpoints", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    let mut frame = checkpoint("alice");
    frame.messages.push(eve_llm_api::ChatMessage::text(
        eve_llm_api::ChatRole::User,
        "待核对正文".repeat(10000),
    ));
    let directory = world._directory.path().join("archive");
    let backup = directory.join("manual-backup.keep");
    std::fs::write(&backup, b"not managed").unwrap();
    for round in 0..24 {
        frame.provider_requests += 1;
        tasks
            .checkpoint(&task.id, frame.clone(), u64::MAX, 30 + round)
            .unwrap();
        let files = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .count();
        assert_eq!(files, 1, "只有当前已确认索引引用的任务正文留存");
    }
    assert_eq!(std::fs::read(backup).unwrap(), b"not managed");
    let expected = tasks.get_owned(&owner("alice"), &task.id).unwrap().unwrap();
    world.state.fail_after.store(true, Ordering::SeqCst);
    frame.provider_requests += 1;
    assert_eq!(
        tasks.checkpoint(&task.id, frame.clone(), u64::MAX, 60),
        Err(TaskError::Storage)
    );
    let files = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .count();
    assert_eq!(files, 2, "确认未知时保留旧、新两份，不能提前回收");
    world.state.fail_after.store(false, Ordering::SeqCst);
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.cold(archive).await.unwrap();
    let current = tasks.get_owned(&owner("alice"), &task.id).unwrap().unwrap();
    assert_eq!(current.spec, expected.spec);
    assert_eq!(current.execution.unwrap().checkpoint, Some(frame));
    let files = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .count();
    assert_eq!(files, 1);
    kernel.stop_all().await.unwrap();
}

#[test]
fn archive_quota_and_compaction_preserve_current_unknown_and_corrupt_files() {
    let directory = tempfile::tempdir().unwrap();
    let archive = eve_task_plugin::FileTaskArchive::open(directory.path()).unwrap();
    let hash = |data: &[u8]| {
        ring::digest::digest(&ring::digest::SHA256, data)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    let current = hash(b"current");
    let superseded = hash(b"superseded");
    archive.put(&current, b"current").unwrap();
    archive.put(&superseded, b"superseded").unwrap();
    let unknown = directory.path().join("backup.keep");
    std::fs::write(&unknown, b"keep").unwrap();
    let damaged = directory.path().join(format!("{superseded}.json"));
    std::fs::write(&damaged, b"damaged").unwrap();
    assert_eq!(
        archive.compact(std::slice::from_ref(&current)),
        Err(TaskError::CorruptState)
    );
    assert_eq!(std::fs::read(&damaged).unwrap(), b"damaged");
    assert_eq!(archive.get(&current).unwrap(), b"current");
    assert_eq!(std::fs::read(&unknown).unwrap(), b"keep");
    std::fs::write(&damaged, b"superseded").unwrap();
    archive.compact(std::slice::from_ref(&current)).unwrap();
    assert!(!damaged.exists());
    std::fs::OpenOptions::new()
        .write(true)
        .open(&unknown)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let next = hash(b"next");
    assert_eq!(archive.put(&next, b"next"), Err(TaskError::LimitReached));
    assert!(!directory.path().join(format!("{next}.json")).exists());
    assert_eq!(archive.get(&current).unwrap(), b"current");
}

#[tokio::test]
async fn step_only_checkpoints_resume_after_the_old_time_limit_without_losing_steps_or_authorization()
 {
    let world = World::new();
    let (kernel, tasks) = world.open().await.unwrap();
    let task = tasks
        .create(&owner("alice"), spec("按步骤恢复"), "step-only", 10)
        .unwrap();
    tasks.start(&task.id, 20).unwrap();
    tasks
        .checkpoint_with_deadline(&task.id, checkpoint("alice"), None, 30)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.open().await.unwrap();
    let recovered = tasks.get_owned(&owner("alice"), &task.id).unwrap().unwrap();
    let execution = recovered.execution.unwrap();
    assert_eq!(execution.deadline_at_ms, None);
    assert_eq!(execution.checkpoint, Some(checkpoint("alice")));
    // 三十分钟已经过去，但恢复不额外领步数，也不受旧总时长截止影响。
    tasks
        .checkpoint_with_deadline(&task.id, checkpoint("alice"), None, 3_600_000)
        .unwrap();
    tasks.retry(&task.id, 3_600_001).unwrap();
    let authorized = tasks
        .create_bounded(&owner("bob"), spec("有限授权"), "authorized", 100, 40)
        .unwrap();
    tasks.start(&authorized.id, 50).unwrap();
    assert!(
        tasks
            .checkpoint_with_deadline(&authorized.id, checkpoint("bob"), None, 60)
            .is_err()
    );
    kernel.stop_all().await.unwrap();
}
