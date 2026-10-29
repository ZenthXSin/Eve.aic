use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use eve_reminder_api::*;
use eve_reminder_plugin::{ReminderController, ReminderPlugin};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const MINUTE: u64 = 60_000;

struct Store {
    inner: FileStateStore,
    fail: AtomicBool,
}
impl StateStore for Store {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.inner.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(PluginError::State("private-storage-detail".into()));
        }
        self.inner.set(namespace, key, value)
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
        });
        Self {
            _directory: directory,
            state,
        }
    }
    async fn open(&self) -> PluginResult<(Kernel, ReminderController)> {
        let kernel = Kernel::with_services(KernelServices {
            state: self.state.clone(),
            ..KernelServices::default()
        });
        let plugin = ReminderPlugin::new()?;
        let controller = plugin.controller();
        kernel.register(Box::new(plugin))?;
        kernel.start_all().await?;
        Ok((kernel, controller))
    }
    fn raw(&self) -> Option<Vec<u8>> {
        self.state
            .get(
                &PluginId::new(REMINDER_PLUGIN_ID).unwrap(),
                REMINDER_STATE_KEY,
            )
            .unwrap()
    }
    fn write(&self, bytes: Vec<u8>) {
        self.state
            .inner
            .set(
                &PluginId::new(REMINDER_PLUGIN_ID).unwrap(),
                REMINDER_STATE_KEY.into(),
                bytes,
            )
            .unwrap();
    }
}

fn sent() -> DeliveryResult {
    DeliveryResult::Sent {
        platform_message_id: Some("out-1".into()),
    }
}

#[tokio::test]
async fn a_reminder_is_tried_proactively_once_then_rides_the_next_private_reply() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let now = 1_000_000;
    let reminder = admin
        .create("user-1", "喝水", now + 30 * MINUTE, "m1", now)
        .unwrap();
    assert_eq!(reminder.id, reminder_id("user-1", "m1"));
    // 渠道重复投递同一消息：返回原记录，不另建。
    let again = admin
        .create("user-1", "别的内容", now + 60 * MINUTE, "m1", now + 1)
        .unwrap();
    assert_eq!(
        (again.id.as_str(), again.text.as_str()),
        (reminder.id.as_str(), "喝水")
    );
    assert_eq!(
        admin
            .create("user-1", "太早", now + 1_000, "m2", now)
            .unwrap_err(),
        ReminderError::OutOfRange
    );
    assert_eq!(
        admin
            .create("user-1", "太远", now + MAX_HORIZON_MS + MINUTE, "m3", now)
            .unwrap_err(),
        ReminderError::OutOfRange
    );
    // 未到期不能投递。
    assert!(
        admin
            .claim(&reminder.id, DeliveryChannel::Proactive, now + MINUTE)
            .is_err()
    );
    let due = now + 30 * MINUTE;
    assert_eq!(
        admin.snapshot().unwrap().due_proactive(due).unwrap().id,
        reminder.id
    );
    admin
        .claim(&reminder.id, DeliveryChannel::Proactive, due)
        .unwrap();
    // 平台拒绝：回到等待；每条至多主动私聊一次。
    let back = admin
        .record_delivery(
            &reminder.id,
            DeliveryResult::Failed {
                http_status: Some(403),
                biz_code: Some(22009),
            },
            due + 1,
        )
        .unwrap();
    assert_eq!(back.status, ReminderStatus::Scheduled);
    assert!(admin.snapshot().unwrap().due_proactive(due + 2).is_none());
    assert!(
        admin
            .claim(&reminder.id, DeliveryChannel::Proactive, due + 2)
            .is_err()
    );
    // 随下一条私聊回复附带；同一条消息不重复附带。
    let passive = DeliveryChannel::Passive {
        message_id: "chat-1".into(),
    };
    assert_eq!(
        admin
            .snapshot()
            .unwrap()
            .due_passive("user-1", "chat-1", due + 3)
            .unwrap()
            .id,
        reminder.id
    );
    admin.claim(&reminder.id, passive.clone(), due + 3).unwrap();
    assert!(
        admin
            .snapshot()
            .unwrap()
            .due_passive("user-1", "chat-2", due + 3)
            .is_none(),
        "发送中"
    );
    admin
        .record_delivery(&reminder.id, DeliveryResult::NotSent, due + 4)
        .unwrap();
    assert!(admin.claim(&reminder.id, passive, due + 5).is_err());
    admin
        .claim(
            &reminder.id,
            DeliveryChannel::Passive {
                message_id: "chat-2".into(),
            },
            due + 5,
        )
        .unwrap();
    let delivered = admin
        .record_delivery(&reminder.id, sent(), due + 6)
        .unwrap();
    assert_eq!(
        (delivered.status, delivered.delivered_at_ms),
        (ReminderStatus::Delivered, Some(due + 6))
    );
    assert_eq!(delivered.attempts.len(), 3);
    assert!(
        admin.cancel("user-1", &reminder.id, due + 7).is_err(),
        "已送达不能取消"
    );
    kernel.stop_all().await.unwrap();
    let (kernel, admin) = world.open().await.unwrap();
    assert_eq!(
        admin.snapshot().unwrap().reminders[0].status,
        ReminderStatus::Delivered
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn owners_cancel_only_their_own_and_limits_and_expiry_hold() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let now = 1_000_000;
    let first = admin
        .create("user-1", "开会", now + MINUTE, "m1", now)
        .unwrap();
    assert_eq!(
        admin.cancel("user-2", &first.id, now).unwrap_err(),
        ReminderError::NotFound,
        "其他用户的提醒如同不存在"
    );
    let cancelled = admin.cancel("user-1", &first.id, now + 1).unwrap();
    assert_eq!(cancelled.status, ReminderStatus::Cancelled);
    assert!(
        admin
            .claim(&first.id, DeliveryChannel::Proactive, now + MINUTE)
            .is_err()
    );
    for index in 0..MAX_SCHEDULED_PER_OWNER {
        admin
            .create("user-1", "事项", now + MINUTE, &format!("n{index}"), now)
            .unwrap();
    }
    assert_eq!(
        admin
            .create("user-1", "再一个", now + MINUTE, "over", now)
            .unwrap_err(),
        ReminderError::LimitReached
    );
    assert!(
        admin
            .create("user-2", "别人的", now + MINUTE, "over", now)
            .is_ok()
    );
    let some = reminder_id("user-1", "n0");
    assert!(
        admin
            .expire(&some, now + MINUTE + DELIVERY_WINDOW_MS)
            .is_err(),
        "投递窗口未过"
    );
    let expired = admin
        .expire(&some, now + MINUTE + DELIVERY_WINDOW_MS + 1)
        .unwrap();
    assert_eq!(expired.status, ReminderStatus::Expired);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn a_full_ledger_makes_room_only_by_dropping_the_oldest_finished_reminder() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let now = 1_000_000;
    for index in 0..MAX_REMINDERS {
        let owner = format!("user-{}", index / MAX_SCHEDULED_PER_OWNER);
        admin
            .create(&owner, "事项", now + MINUTE, &format!("m{index}"), now)
            .unwrap();
    }
    let later = reminder_id("user-0", "m0");
    let earlier = reminder_id("user-1", &format!("m{MAX_SCHEDULED_PER_OWNER}"));
    admin.cancel("user-0", &later, now + 2).unwrap();
    admin.cancel("user-1", &earlier, now + 1).unwrap();
    admin
        .create("user-new", "新的", now + 2 * MINUTE, "n1", now + 3)
        .unwrap();
    let snapshot = admin.snapshot().unwrap();
    assert_eq!(snapshot.reminders.len(), MAX_REMINDERS);
    assert!(
        snapshot.reminders.iter().all(|entry| entry.id != earlier),
        "最早结束的一条腾出位置"
    );
    assert!(snapshot.reminders.iter().any(|entry| entry.id == later));
    admin
        .create("user-new", "再一个", now + 2 * MINUTE, "n2", now + 4)
        .unwrap();
    assert_eq!(
        admin
            .create("user-new", "没有可腾的", now + 2 * MINUTE, "n3", now + 5)
            .unwrap_err(),
        ReminderError::LimitReached,
        "等待中的提醒从不删除"
    );
    kernel.stop_all().await.unwrap();
    let (kernel, admin) = world.open().await.unwrap();
    let snapshot = admin.snapshot().unwrap();
    assert_eq!(snapshot.reminders.len(), MAX_REMINDERS);
    assert!(
        snapshot
            .reminders
            .iter()
            .all(|entry| entry.status == ReminderStatus::Scheduled)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn a_restart_while_sending_is_unknown_and_storage_failure_closes_the_instance() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let now = 1_000_000;
    let reminder = admin
        .create("user-1", "喝水", now + MINUTE, "m1", now)
        .unwrap();
    admin
        .claim(&reminder.id, DeliveryChannel::Proactive, now + MINUTE)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, admin) = world.open().await.unwrap();
    let snapshot = admin.snapshot().unwrap();
    assert_eq!(snapshot.reminders[0].status, ReminderStatus::Unknown);
    assert_eq!(
        snapshot.reminders[0].attempts[0].result,
        Some(DeliveryResult::Unknown)
    );
    assert!(
        snapshot.due_proactive(now + 2 * MINUTE).is_none(),
        "结果未知不重发"
    );
    assert!(
        snapshot
            .due_passive("user-1", "chat", now + 2 * MINUTE)
            .is_none()
    );
    assert!(
        admin
            .record_delivery(&reminder.id, sent(), now + 2 * MINUTE)
            .is_err()
    );

    world.state.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        admin
            .create("user-1", "开会", now + 2 * MINUTE, "m2", now)
            .unwrap_err(),
        ReminderError::Storage
    );
    world.state.fail.store(false, Ordering::SeqCst);
    assert_eq!(admin.snapshot().unwrap_err(), ReminderError::Unavailable);
    kernel.stop_all().await.unwrap();
    let (kernel, admin) = world.open().await.unwrap();
    assert_eq!(
        admin.snapshot().unwrap().reminders.len(),
        1,
        "未确认的提交不算数"
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn corrupt_or_unknown_ledgers_are_refused_and_preserved() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let now = 1_000_000;
    let reminder = admin
        .create("user-1", "喝水", now + MINUTE, "m1", now)
        .unwrap();
    admin
        .claim(&reminder.id, DeliveryChannel::Proactive, now + MINUTE)
        .unwrap();
    admin
        .record_delivery(&reminder.id, sent(), now + MINUTE + 1)
        .unwrap();
    admin
        .create("user-1", "开会", now + 2 * MINUTE, "m2", now)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let valid: serde_json::Value = serde_json::from_slice(&world.raw().unwrap()).unwrap();
    let mutate = |change: &dyn Fn(&mut serde_json::Value)| {
        let mut value = valid.clone();
        change(&mut value);
        serde_json::to_vec(&value).unwrap()
    };
    let cases = vec![
        b"{\"format_version\":1,\"format_version\":1,\"reminders\":[]}".to_vec(),
        serde_json::to_vec(&serde_json::json!({"format_version": 2, "reminders": []})).unwrap(),
        mutate(&|value| value["reminders"][0]["id"] = "remind-other".into()),
        mutate(&|value| value["reminders"][0]["text"] = "".into()),
        mutate(&|value| value["reminders"][0]["status"] = "Scheduled".into()),
        mutate(&|value| value["reminders"][0]["delivered_at_ms"] = serde_json::Value::Null),
        mutate(&|value| value["reminders"][0]["attempts"][0]["started_at_ms"] = 1.into()),
        mutate(&|value| value["reminders"][1]["due_at_ms"] = (now + 1).into()),
        mutate(&|value| {
            let first = value["reminders"][0].clone();
            value["reminders"].as_array_mut().unwrap().push(first)
        }),
        mutate(&|value| value["extra"] = 1.into()),
    ];
    for case in cases {
        world.write(case.clone());
        assert!(
            world.open().await.is_err(),
            "{}",
            String::from_utf8_lossy(&case)
        );
        assert_eq!(world.raw().unwrap(), case, "损坏的账本原样保留");
    }
    world.write(serde_json::to_vec(&valid).unwrap());
    let (kernel, admin) = world.open().await.unwrap();
    assert_eq!(admin.snapshot().unwrap().reminders.len(), 2);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn online_unknown_receipt_is_saved_and_never_retried_after_restart() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let now = 1_000_000;
    let reminder = admin
        .create("user-unknown", "核对回执", now + MINUTE, "m-unknown", now)
        .unwrap();
    admin
        .claim(&reminder.id, DeliveryChannel::Proactive, now + MINUTE)
        .unwrap();
    admin
        .record_delivery(&reminder.id, DeliveryResult::Unknown, now + MINUTE + 1)
        .unwrap();
    let snapshot = admin.snapshot().unwrap();
    assert_eq!(snapshot.reminders[0].status, ReminderStatus::Unknown);
    assert_eq!(snapshot.reminders[0].attempts[0].finished_at_ms, None);
    assert!(snapshot.due_proactive(now + 2 * MINUTE).is_none());
    kernel.stop_all().await.unwrap();
    let (kernel, admin) = world.open().await.unwrap();
    assert_eq!(
        admin.snapshot().unwrap().reminders[0].status,
        ReminderStatus::Unknown
    );
    assert!(
        admin
            .snapshot()
            .unwrap()
            .due_proactive(now + 2 * MINUTE)
            .is_none()
    );
    kernel.stop_all().await.unwrap();
}
