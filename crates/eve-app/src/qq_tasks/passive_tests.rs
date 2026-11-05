use super::*;
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_task_api::NoticeStatus;
use eve_task_plugin::{TaskController, TaskPlugin};

async fn open(path: &std::path::Path) -> (Kernel, Arc<TaskController>, ChannelAdapter) {
    let kernel = Kernel::with_services(KernelServices {
        state: Arc::new(FileStateStore::open(path).unwrap()),
        ..KernelServices::default()
    });
    let plugin = TaskPlugin::new().unwrap();
    let tasks = Arc::new(plugin.controller());
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let adapter = ChannelAdapter {
        tasks: tasks.clone(),
    };
    (kernel, tasks, adapter)
}
fn completed(tasks: &dyn TaskAdmin, owner: &TaskOwner, source: &str, at: u64) -> Task {
    let task = tasks
        .create(
            owner,
            TaskSpec {
                title: "本地回归".into(),
                instructions: "保留任务结果".into(),
                max_steps: 50,
                context: None,
            },
            source,
            at,
        )
        .unwrap();
    tasks.start(&task.id, at + 1).unwrap();
    tasks
        .finish(
            &task.id,
            TaskEnd::Completed {
                result: "已有实际结果".into(),
            },
            TaskUsage::default(),
            at + 2,
        )
        .unwrap()
}
#[tokio::test]
async fn failed_proactive_becomes_authored_passive_only_for_a_new_anchor_and_settles_once() {
    let directory = tempfile::tempdir().unwrap();
    let (kernel, tasks, channel) = open(directory.path()).await;
    let session = SessionKey::new("qq:owner", "qq:owner").unwrap();
    let owner = TaskOwner {
        session_id: session.session_id.clone(),
        user_id: session.user_id.clone(),
    };
    let at = now_ms() - 100;
    let task = completed(tasks.as_ref(), &owner, "failed-proactive", at);
    let candidate = channel.candidates().unwrap().remove(0);
    let first = channel
        .claim_candidate(&candidate, "Eve 的第一次表达", "old-input")
        .unwrap()
        .unwrap();
    channel
        .delivered(
            &first.message.id,
            QqOutreachResult::Failed {
                http_status: Some(400),
                biz_code: Some(1),
            },
        )
        .unwrap();
    assert!(
        channel.candidates().unwrap().is_empty(),
        "此前 live 只能扫描这里，通知因此永远遗漏"
    );
    let candidate = channel
        .passive_candidates(&session, "fresh-input")
        .unwrap()
        .remove(0);
    let foreign = SessionKey::new("qq:other-session", "qq:owner").unwrap();
    assert!(
        channel
            .passive_candidates(&foreign, "fresh-input")
            .unwrap()
            .is_empty()
    );
    assert!(
        channel
            .claim_candidate(&candidate, "不能变主动", "fresh-input")
            .unwrap()
            .is_none()
    );
    assert!(
        channel
            .claim_passive_candidate(&candidate, "旧审视不能换锚点", "other-input")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        tasks
            .get_owned(&owner, &task.id)
            .unwrap()
            .unwrap()
            .notice
            .unwrap()
            .attempts
            .len(),
        1
    );
    let second = channel
        .claim_passive_candidate(&candidate, "Eve 自己写出的补充", "fresh-input")
        .unwrap()
        .unwrap();
    assert_eq!(second.message.text, "Eve 自己写出的补充");
    assert!(
        channel
            .claim_passive_candidate(&candidate, "重复", "fresh-input")
            .unwrap()
            .is_none()
    );
    channel
        .delivered(&second.message.id, QqOutreachResult::NotSent)
        .unwrap();
    assert!(
        channel
            .passive_candidates(&session, "fresh-input")
            .unwrap()
            .is_empty()
    );
    let next = channel
        .passive_candidates(&session, "next-input")
        .unwrap()
        .remove(0);
    let third = channel
        .claim_passive_candidate(&next, "重新审视后的补充", "next-input")
        .unwrap()
        .unwrap();
    channel
        .delivered(
            &second.message.id,
            QqOutreachResult::Sent {
                platform_message_id: Some("stale-receipt".into()),
            },
        )
        .unwrap();
    assert_eq!(
        tasks
            .get_owned(&owner, &task.id)
            .unwrap()
            .unwrap()
            .notice
            .unwrap()
            .status,
        NoticeStatus::Delivering
    );
    channel
        .delivered(
            &third.message.id,
            QqOutreachResult::Sent {
                platform_message_id: Some("actual-message".into()),
            },
        )
        .unwrap();
    channel
        .delivered(
            &third.message.id,
            QqOutreachResult::Sent {
                platform_message_id: Some("actual-message".into()),
            },
        )
        .unwrap();
    assert_eq!(
        tasks
            .get_owned(&owner, &task.id)
            .unwrap()
            .unwrap()
            .notice
            .unwrap()
            .status,
        NoticeStatus::Delivered
    );
    kernel.stop_all().await.unwrap();
    let (kernel, _, channel) = open(directory.path()).await;
    assert!(
        channel
            .passive_candidates(&session, "after-restart")
            .unwrap()
            .is_empty()
    );
    kernel.stop_all().await.unwrap();
}
#[tokio::test]
async fn passive_candidates_preserve_expiry_attempt_budget_and_unknown_across_restart() {
    let directory = tempfile::tempdir().unwrap();
    let (kernel, tasks, channel) = open(directory.path()).await;
    let session = SessionKey::new("qq:owner", "qq:owner").unwrap();
    let owner = TaskOwner {
        session_id: session.session_id.clone(),
        user_id: session.user_id.clone(),
    };
    let expired = completed(
        tasks.as_ref(),
        &owner,
        "expired",
        now_ms() - eve_task_api::DELIVERY_WINDOW_MS - 100,
    );
    assert!(
        channel
            .passive_candidates(&session, "input")
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        tasks
            .get_owned(&owner, &expired.id)
            .unwrap()
            .unwrap()
            .notice
            .unwrap()
            .status,
        NoticeStatus::Expired
    );
    let bounded = completed(tasks.as_ref(), &owner, "bounded", now_ms() - 100);
    for i in 0..eve_task_api::MAX_ATTEMPTS {
        let anchor = format!("input-{i}");
        let candidate = channel
            .passive_candidates(&session, &anchor)
            .unwrap()
            .remove(0);
        let push = channel
            .claim_passive_candidate(&candidate, "Eve 的决定", &anchor)
            .unwrap()
            .unwrap();
        channel
            .delivered(&push.message.id, QqOutreachResult::NotSent)
            .unwrap();
        assert!(
            channel.candidates().unwrap().is_empty(),
            "被动尝试未发出后仍等待下次真实输入，不能改走首次主动发送"
        );
    }
    assert_eq!(
        tasks
            .get_owned(&owner, &bounded.id)
            .unwrap()
            .unwrap()
            .notice
            .unwrap()
            .status,
        NoticeStatus::Expired
    );
    let unknown = completed(tasks.as_ref(), &owner, "unknown", now_ms() - 100);
    let candidate = channel
        .passive_candidates(&session, "uncertain-input")
        .unwrap()
        .remove(0);
    let push = channel
        .claim_passive_candidate(&candidate, "Eve 的决定", "uncertain-input")
        .unwrap()
        .unwrap();
    channel
        .delivered(
            &push.message.id,
            QqOutreachResult::Sent {
                platform_message_id: None,
            },
        )
        .unwrap();
    assert_eq!(
        tasks
            .get_owned(&owner, &unknown.id)
            .unwrap()
            .unwrap()
            .notice
            .unwrap()
            .status,
        NoticeStatus::Unknown
    );
    kernel.stop_all().await.unwrap();
    let (kernel, _, channel) = open(directory.path()).await;
    assert!(
        channel
            .passive_candidates(&session, "after-restart")
            .unwrap()
            .is_empty()
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn passive_candidates_only_settle_the_exact_session() {
    let directory = tempfile::tempdir().unwrap();
    let (kernel, tasks, channel) = open(directory.path()).await;
    let session = SessionKey::new("qq:owner", "qq:owner").unwrap();
    let owner = TaskOwner {
        session_id: session.session_id.clone(),
        user_id: session.user_id.clone(),
    };
    let foreign = TaskOwner {
        session_id: "web:owner".into(),
        user_id: session.user_id.clone(),
    };
    let old = completed(
        tasks.as_ref(),
        &foreign,
        "foreign-expired",
        now_ms() - eve_task_api::DELIVERY_WINDOW_MS - 100,
    );
    let sending = completed(tasks.as_ref(), &foreign, "foreign-sending", now_ms() - 100);
    tasks
        .claim(&sending.id, DeliveryChannel::Proactive, now_ms())
        .unwrap();
    let own = completed(tasks.as_ref(), &owner, "own-pending", now_ms() - 100);
    let candidates = channel.passive_candidates(&session, "fresh-input").unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].id, own.id);
    assert_eq!(
        tasks
            .get_owned(&foreign, &old.id)
            .unwrap()
            .unwrap()
            .notice
            .unwrap()
            .status,
        NoticeStatus::Pending
    );
    assert_eq!(
        tasks
            .get_owned(&foreign, &sending.id)
            .unwrap()
            .unwrap()
            .notice
            .unwrap()
            .status,
        NoticeStatus::Delivering
    );
    kernel.stop_all().await.unwrap();
}
