use super::*;
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_task_api::{
    DEFAULT_STEPS, DeliveryChannel, DeliveryResult, NoticeStatus, TASK_PROGRESS_INTERVAL_MS,
    TaskEnd, TaskSpec, TaskUsage,
};
use eve_task_plugin::{TaskController, TaskPlugin};
use eve_trigger_api::{Condition, Delivery, MatchMode, Recurrence, Trigger, TriggerSpec};
use eve_trigger_plugin::{TriggerController, TriggerPlugin};
use std::sync::Arc;

async fn open_plugins(root: &Path) -> (Kernel, TaskController, TriggerController) {
    let kernel = Kernel::with_services(KernelServices {
        state: Arc::new(FileStateStore::open(root.join("plugins")).unwrap()),
        ..KernelServices::default()
    });
    let tasks = TaskPlugin::new().unwrap();
    let task_controller = tasks.controller();
    let triggers = TriggerPlugin::new().unwrap();
    let trigger_controller = triggers.controller();
    kernel.register(Box::new(tasks)).unwrap();
    kernel.register(Box::new(triggers)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, task_controller, trigger_controller)
}

fn owner(chat: u32) -> TaskOwner {
    TaskOwner {
        session_id: format!("web:chat:{chat:032x}"),
        user_id: "web:owner".into(),
    }
}

fn session(owner: &TaskOwner) -> SessionKey {
    SessionKey::new(&owner.session_id, &owner.user_id).unwrap()
}

fn task_spec(title: &str) -> TaskSpec {
    TaskSpec {
        title: title.into(),
        instructions: "保存实际执行结果，完成前不得声称成功".into(),
        max_steps: DEFAULT_STEPS,
        context: None,
    }
}

fn completed_task(tasks: &TaskController, owner: &TaskOwner, source: &str) -> Task {
    let task = tasks
        .create(owner, task_spec("后台任务结果"), source, 1)
        .unwrap();
    tasks.start(&task.id, 2).unwrap();
    let task = tasks
        .finish(
            &task.id,
            TaskEnd::Completed {
                result: "已保存的真实任务结果".into(),
            },
            TaskUsage::default(),
            3,
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Completed);
    task
}

fn trigger_spec(recurrence: Option<Recurrence>) -> TriggerSpec {
    TriggerSpec {
        title: "后台提醒结果".into(),
        instructions: "报告实际条件检查结果".into(),
        mode: MatchMode::All,
        conditions: vec![Condition::Time { at_ms: 2 }],
        expires_at_ms: 10_000_000,
        recurrence,
        resume_task_id: None,
    }
}

fn finish_trigger(triggers: &TriggerController, id: &str, result: &str) -> Trigger {
    triggers.ready(id, vec!["时间条件已满足".into()]).unwrap();
    triggers
        .transition(id, TriggerStatus::Ready, TriggerStatus::Running, None)
        .unwrap();
    triggers
        .transition(
            id,
            TriggerStatus::Running,
            TriggerStatus::Completed,
            Some(result.into()),
        )
        .unwrap();
    triggers
        .snapshot()
        .unwrap()
        .into_iter()
        .find(|trigger| trigger.id == id)
        .unwrap()
}

fn stored_task(tasks: &TaskController, owner: &TaskOwner, id: &str) -> Task {
    tasks.get_owned(owner, id).unwrap().unwrap()
}

fn assert_local_task_receipt(task: &Task, event: &str) {
    let notice = task.notice.as_ref().unwrap();
    assert_eq!(notice.status, NoticeStatus::Delivered);
    assert_eq!(notice.attempts.len(), 1);
    assert_eq!(notice.attempts[0].channel, DeliveryChannel::LocalInbox);
    assert_eq!(
        notice.attempts[0].result,
        Some(DeliveryResult::StoredLocally {
            notification_id: event.into(),
        })
    );
    assert!(notice.attempts[0].finished_at_ms.is_some());
    assert_eq!(notice.proactive_attempts(), 0);
}

#[tokio::test]
async fn collects_only_exact_web_owners_and_restores_receipts_pages_and_read_state() {
    let root = tempfile::tempdir().unwrap();
    let (kernel, tasks, triggers) = open_plugins(root.path()).await;
    let web_owner = owner(1);
    let owners = [
        web_owner.clone(),
        TaskOwner {
            session_id: "qq:private:alice".into(),
            user_id: "alice".into(),
        },
        TaskOwner {
            session_id: web_owner.session_id.clone(),
            user_id: "web:guest".into(),
        },
        TaskOwner {
            session_id: "qq:private:web-owner".into(),
            user_id: web_owner.user_id.clone(),
        },
        TaskOwner {
            session_id: "web:chat:invalid".into(),
            user_id: web_owner.user_id.clone(),
        },
    ];
    let mut originals = Vec::new();
    for (index, owner) in owners.iter().enumerate() {
        let task = completed_task(&tasks, owner, &format!("task-{index}"));
        let trigger = triggers
            .create(
                session(owner),
                format!("trigger-{index}"),
                trigger_spec(None),
                1,
            )
            .unwrap();
        let trigger = finish_trigger(&triggers, &trigger.id, "已保存的真实提醒结果");
        originals.push((task, trigger));
    }
    let inbox = Inbox::open(root.path()).unwrap();
    inbox.collect(&tasks, &triggers, 10).unwrap();
    let page = inbox.list(None, 100).unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.unread, 2);
    assert_eq!(page.next_before, None);
    assert!(page.items.iter().all(|notification| {
        notification.session_id == web_owner.session_id
            && notification.status == "completed"
            && notification.read_at_ms.is_none()
    }));
    assert_eq!(page.items[0].kind, "trigger");
    assert_eq!(page.items[0].body, "已保存的真实提醒结果");
    assert_eq!(page.items[1].kind, "task");
    assert_eq!(page.items[1].body, "已保存的真实任务结果");
    let event = inbox
        .inner
        .lock()
        .unwrap()
        .ledger
        .records
        .iter()
        .find(|record| record.notification.kind == "task")
        .unwrap()
        .event
        .clone();
    let delivered = stored_task(&tasks, &web_owner, &originals[0].0.id);
    assert_local_task_receipt(&delivered, &event);
    assert_eq!(delivered.status, TaskStatus::Completed);
    assert_eq!(delivered.result, originals[0].0.result);
    let trigger_snapshot = triggers.snapshot().unwrap();
    assert_eq!(
        trigger_snapshot
            .iter()
            .find(|trigger| trigger.id == originals[0].1.id)
            .unwrap()
            .delivery,
        Some(Delivery::StoredLocally)
    );
    for (task, trigger) in &originals[1..] {
        assert!(stored_task(&tasks, &task.owner, &task.id) == *task);
        assert_eq!(
            trigger_snapshot.iter().find(|item| item.id == trigger.id),
            Some(trigger)
        );
    }

    let first = inbox.list(None, 1).unwrap();
    assert_eq!(first.items, page.items[..1]);
    assert_eq!(first.next_before, Some(page.items[0].id));
    let second = inbox.list(first.next_before, 1).unwrap();
    assert_eq!(second.items, page.items[1..]);
    assert_eq!(second.next_before, None);
    assert_eq!(second.unread, 2);
    assert!(
        inbox
            .list(Some(page.items[1].id), 1)
            .unwrap()
            .items
            .is_empty()
    );
    for (before, limit) in [(None, 0), (None, 101), (Some(0), 1)] {
        assert_eq!(
            inbox.list(before, limit).err(),
            Some(PanelError::InvalidInput)
        );
    }
    assert_eq!(inbox.mark_read(0, 10), Err(PanelError::InvalidInput));
    assert_eq!(inbox.mark_read(1, 0), Err(PanelError::InvalidInput));
    assert_eq!(inbox.mark_read(999, 10), Err(PanelError::NotFound));
    assert!(inbox.mark_read(page.items[0].id, 1).unwrap());
    assert!(!inbox.mark_read(page.items[0].id, 100).unwrap());
    let saved = inbox.list(None, 100).unwrap();
    assert_eq!(saved.unread, 1);
    assert_eq!(saved.items[0].read_at_ms, Some(10));
    let bytes = fs::read(inbox.directory.join("state.json")).unwrap();
    inbox.collect(&tasks, &triggers, 20).unwrap();
    assert_eq!(fs::read(inbox.directory.join("state.json")).unwrap(), bytes);
    kernel.stop_all().await.unwrap();
    drop((inbox, kernel, tasks, triggers));

    let (kernel, tasks, triggers) = open_plugins(root.path()).await;
    let inbox = Inbox::open(root.path()).unwrap();
    inbox.collect(&tasks, &triggers, 30).unwrap();
    let restored = inbox.list(None, 100).unwrap();
    assert_eq!(restored.items, saved.items);
    assert_eq!(restored.unread, 1);
    assert_local_task_receipt(&stored_task(&tasks, &web_owner, &originals[0].0.id), &event);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn local_receipt_allows_recurrence_and_each_cycle_keeps_its_own_notification() {
    let root = tempfile::tempdir().unwrap();
    let (kernel, tasks, triggers) = open_plugins(root.path()).await;
    let trigger = triggers
        .create(
            session(&owner(1)),
            "recurring".into(),
            trigger_spec(Some(Recurrence {
                interval_ms: 60_000,
                max_runs: 2,
            })),
            1,
        )
        .unwrap();
    finish_trigger(&triggers, &trigger.id, "第一轮结果");
    let inbox = Inbox::open(root.path()).unwrap();
    inbox.collect(&tasks, &triggers, 10).unwrap();
    assert_eq!(
        triggers.snapshot().unwrap()[0].delivery,
        Some(Delivery::StoredLocally)
    );
    triggers.rearm(&trigger.id, 20).unwrap();
    assert_eq!(triggers.snapshot().unwrap()[0].status, TriggerStatus::Armed);
    inbox.collect(&tasks, &triggers, 20).unwrap();
    assert_eq!(inbox.list(None, 100).unwrap().items.len(), 1);

    let second = finish_trigger(&triggers, &trigger.id, "第二轮结果");
    assert_eq!(second.runs, 2);
    inbox.collect(&tasks, &triggers, 60_021).unwrap();
    inbox.collect(&tasks, &triggers, 60_022).unwrap();
    let saved = inbox.list(None, 100).unwrap();
    assert_eq!(saved.items.len(), 2);
    assert_eq!(saved.items[0].body, "第二轮结果");
    assert_eq!(saved.items[1].body, "第一轮结果");
    assert_ne!(saved.items[0].id, saved.items[1].id);
    assert_eq!(saved.items[0].source_id, saved.items[1].source_id);
    assert_eq!(
        triggers.snapshot().unwrap()[0].delivery,
        Some(Delivery::StoredLocally)
    );
    assert_eq!(
        triggers.rearm(&trigger.id, 60_023),
        Err(eve_trigger_api::TriggerError::Transition)
    );
    kernel.stop_all().await.unwrap();
    drop((inbox, kernel, tasks, triggers));

    let (kernel, tasks, triggers) = open_plugins(root.path()).await;
    let inbox = Inbox::open(root.path()).unwrap();
    inbox.collect(&tasks, &triggers, 60_024).unwrap();
    assert_eq!(inbox.list(None, 100).unwrap().items, saved.items);
    assert_eq!(triggers.snapshot().unwrap()[0].runs, 2);
    assert_eq!(
        triggers.snapshot().unwrap()[0].delivery,
        Some(Delivery::StoredLocally)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn progress_obeys_thirty_minutes_survives_restart_and_does_not_complete_tasks() {
    assert_eq!(TASK_PROGRESS_INTERVAL_MS, 30 * 60 * 1000);
    let root = tempfile::tempdir().unwrap();
    let (kernel, tasks, triggers) = open_plugins(root.path()).await;
    let web_owner = owner(1);
    let task = tasks
        .create(&web_owner, task_spec("仍在排队的任务"), "progress", 1)
        .unwrap();
    let qq_owner = TaskOwner {
        session_id: "qq:private:alice".into(),
        user_id: "alice".into(),
    };
    let qq_task = tasks
        .create(&qq_owner, task_spec("QQ 排队任务"), "qq-progress", 1)
        .unwrap();
    let inbox = Inbox::open(root.path()).unwrap();
    let at = task.created_at_ms + TASK_PROGRESS_INTERVAL_MS;
    inbox.collect(&tasks, &triggers, at - 1).unwrap();
    assert!(inbox.list(None, 100).unwrap().items.is_empty());
    inbox.collect(&tasks, &triggers, at).unwrap();
    let first = inbox.list(None, 100).unwrap();
    assert_eq!(first.items.len(), 1);
    assert_eq!(first.items[0].kind, "progress");
    assert_eq!(first.items[0].status, "queued");
    assert!(first.items[0].body.contains("尚未确认完成"));
    assert!(first.items[0].body.contains("尚无可报告的新检查点"));
    let updated = stored_task(&tasks, &web_owner, &task.id);
    assert_eq!(updated.status, TaskStatus::Queued);
    assert_eq!(updated.finished_at_ms, None);
    assert_eq!(updated.result, None);
    assert_eq!(updated.notice, None);
    let notices = &updated.execution.as_ref().unwrap().progress_notices;
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].sequence, 1);
    assert_eq!(notices[0].status, TaskStatus::Queued);
    assert_eq!(notices[0].attempt.channel, DeliveryChannel::LocalInbox);
    assert!(matches!(
        notices[0].attempt.result,
        Some(DeliveryResult::StoredLocally { .. })
    ));
    assert_eq!(notices[0].attempt.started_at_ms, at);
    assert_eq!(notices[0].attempt.finished_at_ms, Some(at));
    assert!(stored_task(&tasks, &qq_owner, &qq_task.id) == qq_task);
    inbox.collect(&tasks, &triggers, at).unwrap();
    inbox
        .collect(&tasks, &triggers, at + TASK_PROGRESS_INTERVAL_MS - 1)
        .unwrap();
    assert_eq!(inbox.list(None, 100).unwrap().items, first.items);
    kernel.stop_all().await.unwrap();
    drop((inbox, kernel, tasks, triggers));

    let (kernel, tasks, triggers) = open_plugins(root.path()).await;
    let inbox = Inbox::open(root.path()).unwrap();
    inbox
        .collect(&tasks, &triggers, at + TASK_PROGRESS_INTERVAL_MS - 1)
        .unwrap();
    assert_eq!(inbox.list(None, 100).unwrap().items, first.items);
    assert!(stored_task(&tasks, &web_owner, &task.id) == updated);
    inbox
        .collect(&tasks, &triggers, at + TASK_PROGRESS_INTERVAL_MS)
        .unwrap();
    let second = inbox.list(None, 100).unwrap();
    assert_eq!(second.items.len(), 2);
    assert!(
        second
            .items
            .iter()
            .all(|item| item.kind == "progress" && item.status == "queued")
    );
    let updated = stored_task(&tasks, &web_owner, &task.id);
    let notices = &updated.execution.as_ref().unwrap().progress_notices;
    assert_eq!(notices.len(), 2);
    assert_eq!(notices[1].sequence, 2);
    assert_eq!(
        notices[1].attempt.started_at_ms,
        at + TASK_PROGRESS_INTERVAL_MS
    );
    assert_eq!(updated.status, TaskStatus::Queued);
    assert_eq!(updated.finished_at_ms, None);
    assert_eq!(updated.notice, None);
    kernel.stop_all().await.unwrap();
}

fn pending_task(task: &Task, at: u64) -> Record {
    let source = source_key(
        &task.owner.session_id,
        &task.owner.user_id,
        &task.id,
        task.created_at_ms,
        "task",
    );
    let finished = task.finished_at_ms.unwrap();
    Record {
        event: hash(&(&source, finished, task.status)),
        source,
        notification: Notification {
            id: 0,
            session_id: task.owner.session_id.clone(),
            source_id: task.id.clone(),
            kind: "task".into(),
            status: "completed".into(),
            title: task.spec.title.clone(),
            body: task.result.clone().unwrap(),
            truncated: false,
            created_at_ms: at,
            read_at_ms: None,
        },
        pending: Some(Settlement::Task {
            owner: task.owner.clone(),
            id: task.id.clone(),
            source: task.source.clone(),
            created_at_ms: task.created_at_ms,
            finished_at_ms: finished,
        }),
        settlement: None,
    }
}

fn pending_trigger(trigger: &Trigger, at: u64) -> Record {
    let source = source_key(
        &trigger.owner.session_id,
        &trigger.owner.user_id,
        &trigger.id,
        trigger.created_at_ms,
        "trigger",
    );
    Record {
        event: hash(&(&source, trigger.runs, "completed")),
        source,
        notification: Notification {
            id: 0,
            session_id: trigger.owner.session_id.clone(),
            source_id: trigger.id.clone(),
            kind: "trigger".into(),
            status: "completed".into(),
            title: trigger.spec.title.clone(),
            body: trigger.result.clone().unwrap(),
            truncated: false,
            created_at_ms: at,
            read_at_ms: None,
        },
        pending: Some(Settlement::Trigger {
            owner: trigger.owner.clone(),
            id: trigger.id.clone(),
            source: trigger.source.clone(),
            created_at_ms: trigger.created_at_ms,
            run: trigger.runs,
        }),
        settlement: None,
    }
}

fn pending_progress(task: &Task, at: u64) -> Record {
    let source = source_key(
        &task.owner.session_id,
        &task.owner.user_id,
        &task.id,
        task.created_at_ms,
        "task",
    );
    Record {
        event: hash(&(&source, "progress", 1_u64)),
        source,
        notification: Notification {
            id: 0,
            session_id: task.owner.session_id.clone(),
            source_id: task.id.clone(),
            kind: "progress".into(),
            status: "queued".into(),
            title: task.spec.title.clone(),
            body: "30 分钟进度提醒：任务仍在排队，尚未确认完成。".into(),
            truncated: false,
            created_at_ms: at,
            read_at_ms: None,
        },
        pending: Some(Settlement::Progress {
            owner: task.owner.clone(),
            id: task.id.clone(),
            source: task.source.clone(),
            created_at_ms: task.created_at_ms,
            sequence: 1,
            at_ms: at,
        }),
        settlement: None,
    }
}

#[tokio::test]
async fn crash_between_inbox_and_plugin_commits_recovers_all_three_receipts_once() {
    for plugin_receipt_committed in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (kernel, tasks, triggers) = open_plugins(root.path()).await;
        let terminal = completed_task(&tasks, &owner(1), "crash-task");
        let trigger = triggers
            .create(
                session(&owner(1)),
                "crash-trigger".into(),
                trigger_spec(None),
                1,
            )
            .unwrap();
        let trigger = finish_trigger(&triggers, &trigger.id, "崩溃前已保存的提醒正文");
        let progress = tasks
            .create(&owner(2), task_spec("仍在排队"), "crash-progress", 1)
            .unwrap();
        let at = progress.created_at_ms + TASK_PROGRESS_INTERVAL_MS;
        let records = [
            pending_task(&terminal, at),
            pending_trigger(&trigger, at),
            pending_progress(&progress, at),
        ];
        let inbox = Inbox::open(root.path()).unwrap();
        {
            let mut inner = inbox.inner.lock().unwrap();
            for record in &records {
                inbox.insert(&mut inner, record.clone()).unwrap();
            }
        }
        let saved = inbox.list(None, 100).unwrap();
        assert_eq!(saved.items.len(), 3);
        assert!(stored_task(&tasks, &terminal.owner, &terminal.id).notice_pending());
        assert_eq!(
            triggers.snapshot().unwrap()[0].delivery,
            Some(Delivery::Pending)
        );
        assert!(
            stored_task(&tasks, &progress.owner, &progress.id)
                .execution
                .unwrap()
                .progress_notices
                .is_empty()
        );
        if plugin_receipt_committed {
            tasks
                .record_local_delivery(
                    &terminal.owner,
                    &terminal.id,
                    terminal.finished_at_ms.unwrap(),
                    &records[0].event,
                    at,
                )
                .unwrap();
            triggers
                .record_local_delivery(&trigger.owner, &trigger.id, trigger.runs)
                .unwrap();
            tasks
                .record_local_progress(&progress.owner, &progress.id, 1, &records[2].event, at)
                .unwrap();
        }
        // 不调用 collect：重启时模拟正文已提交，但收件箱仍未确认跨账本结算。
        kernel.stop_all().await.unwrap();
        drop((inbox, kernel, tasks, triggers));

        let (kernel, tasks, triggers) = open_plugins(root.path()).await;
        let inbox = Inbox::open(root.path()).unwrap();
        assert_eq!(inbox.list(None, 100).unwrap().items, saved.items);
        assert!(
            inbox
                .inner
                .lock()
                .unwrap()
                .ledger
                .records
                .iter()
                .all(|record| record.pending.is_some())
        );
        inbox.collect(&tasks, &triggers, at + 1).unwrap();
        inbox.collect(&tasks, &triggers, at + 2).unwrap();
        assert_eq!(inbox.list(None, 100).unwrap().items, saved.items);
        assert!(
            inbox
                .inner
                .lock()
                .unwrap()
                .ledger
                .records
                .iter()
                .all(|record| {
                    record.pending.is_none() && record.settlement.as_deref() == Some("recorded")
                })
        );
        assert_local_task_receipt(
            &stored_task(&tasks, &terminal.owner, &terminal.id),
            &records[0].event,
        );
        assert_eq!(
            triggers.snapshot().unwrap()[0].delivery,
            Some(Delivery::StoredLocally)
        );
        let progress = stored_task(&tasks, &progress.owner, &progress.id);
        let notices = &progress.execution.as_ref().unwrap().progress_notices;
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].sequence, 1);
        assert_eq!(notices[0].attempt.started_at_ms, at);
        assert_eq!(
            notices[0].attempt.result,
            Some(DeliveryResult::StoredLocally {
                notification_id: records[2].event.clone()
            })
        );
        assert_eq!(progress.status, TaskStatus::Queued);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn progress_saved_before_completion_is_settled_without_rewriting_its_observed_status() {
    let root = tempfile::tempdir().unwrap();
    let (kernel, tasks, triggers) = open_plugins(root.path()).await;
    let task = tasks
        .create(
            &owner(1),
            task_spec("保存进度后完成"),
            "progress-completion-race",
            1,
        )
        .unwrap();
    let at = task.created_at_ms + TASK_PROGRESS_INTERVAL_MS;
    let pending = pending_progress(&task, at);
    let inbox = Inbox::open(root.path()).unwrap();
    {
        let mut inner = inbox.inner.lock().unwrap();
        inbox.insert(&mut inner, pending.clone()).unwrap();
    }
    let saved_progress = inbox.list(None, 1).unwrap().items.remove(0);
    // 模拟调度器在收件箱正文提交后、回执结算前完成同一任务。
    tasks.start(&task.id, at + 1).unwrap();
    let terminal = tasks
        .finish(
            &task.id,
            TaskEnd::Completed {
                result: "进度保存后才完成".into(),
            },
            TaskUsage::default(),
            at + 2,
        )
        .unwrap();
    assert_eq!(terminal.status, TaskStatus::Completed);
    kernel.stop_all().await.unwrap();
    drop((inbox, kernel, tasks, triggers));

    let (kernel, tasks, triggers) = open_plugins(root.path()).await;
    let inbox = Inbox::open(root.path()).unwrap();
    inbox.collect(&tasks, &triggers, at + 3).unwrap();
    inbox.collect(&tasks, &triggers, at + 4).unwrap();
    let page = inbox.list(None, 100).unwrap();
    assert_eq!(page.items.len(), 2);
    let progress = page
        .items
        .iter()
        .find(|item| item.kind == "progress")
        .unwrap();
    assert_eq!(progress, &saved_progress);
    assert_eq!(progress.status, "queued");
    assert!(progress.body.contains("尚未确认完成"));
    let completed = page.items.iter().find(|item| item.kind == "task").unwrap();
    assert_eq!(completed.status, "completed");
    assert_eq!(completed.body, "进度保存后才完成");
    let task = stored_task(&tasks, &task.owner, &task.id);
    assert_eq!(task.status, TaskStatus::Completed);
    assert_eq!(task.finished_at_ms, terminal.finished_at_ms);
    let notices = &task.execution.as_ref().unwrap().progress_notices;
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].status, TaskStatus::Completed);
    assert_eq!(notices[0].observed_finished_at_ms, terminal.finished_at_ms);
    assert_eq!(notices[0].attempt.started_at_ms, at);
    assert_eq!(
        notices[0].attempt.result,
        Some(DeliveryResult::StoredLocally {
            notification_id: pending.event
        })
    );
    assert_eq!(
        task.notice.as_ref().unwrap().status,
        NoticeStatus::Delivered
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn mismatched_pending_event_hash_is_rejected_without_modifying_any_ledger() {
    let root = tempfile::tempdir().unwrap();
    let (kernel, tasks, triggers) = open_plugins(root.path()).await;
    let terminal = completed_task(&tasks, &owner(1), "corrupt-pending-task");
    let trigger = triggers
        .create(
            session(&owner(1)),
            "corrupt-pending-trigger".into(),
            trigger_spec(None),
            1,
        )
        .unwrap();
    let trigger = finish_trigger(&triggers, &trigger.id, "已保存提醒结果");
    let progress = tasks
        .create(
            &owner(2),
            task_spec("未完成任务"),
            "corrupt-pending-progress",
            1,
        )
        .unwrap();
    let at = progress.created_at_ms + TASK_PROGRESS_INTERVAL_MS;
    let records = [
        pending_task(&terminal, at),
        pending_trigger(&trigger, at),
        pending_progress(&progress, at),
    ];
    let inbox = Inbox::open(root.path()).unwrap();
    {
        let mut inner = inbox.inner.lock().unwrap();
        for record in &records {
            inbox.insert(&mut inner, record.clone()).unwrap();
        }
    }
    let state = inbox.directory.join("state.json");
    let original = fs::read(&state).unwrap();
    drop(inbox);
    for index in 0..records.len() {
        let mut corrupted: Ledger = serde_json::from_slice(&original).unwrap();
        let event = "0".repeat(64);
        assert_ne!(corrupted.records[index].event, event);
        corrupted.records[index].event = event.clone();
        // 同时改去重索引，保证拒绝来自事件与回执内容不一致，而非悬空索引。
        corrupted.seen[index].events = vec![event];
        let bytes = serde_json::to_vec(&corrupted).unwrap();
        fs::write(&state, &bytes).unwrap();
        assert_eq!(
            Inbox::open(root.path()).err(),
            Some(PanelError::Unavailable)
        );
        assert_eq!(fs::read(&state).unwrap(), bytes);
        assert!(stored_task(&tasks, &terminal.owner, &terminal.id) == terminal);
        assert!(stored_task(&tasks, &progress.owner, &progress.id) == progress);
        assert_eq!(triggers.snapshot().unwrap(), vec![trigger.clone()]);
    }
    fs::write(&state, &original).unwrap();
    let inbox = Inbox::open(root.path()).unwrap();
    inbox.collect(&tasks, &triggers, at).unwrap();
    assert_eq!(inbox.list(None, 100).unwrap().items.len(), 3);
    kernel.stop_all().await.unwrap();
}

#[test]
fn exclusive_open_corrupt_state_and_missing_state_never_create_an_empty_inbox() {
    let root = tempfile::tempdir().unwrap();
    let inbox = Inbox::open(root.path()).unwrap();
    let state = inbox.directory.join("state.json");
    let original = fs::read(&state).unwrap();
    assert_eq!(
        Inbox::open(root.path()).err(),
        Some(PanelError::Unavailable)
    );
    assert_eq!(fs::read(&state).unwrap(), original);
    drop(inbox);
    drop(Inbox::open(root.path()).unwrap());
    for bytes in [
        b"broken".as_slice(),
        br#"{"version":1,"version":1,"next_id":1,"seen":[],"records":[]}"#.as_slice(),
        br#"{"version":1,"next_id":0,"seen":[],"records":[]}"#.as_slice(),
    ] {
        fs::write(&state, bytes).unwrap();
        assert_eq!(
            Inbox::open(root.path()).err(),
            Some(PanelError::Unavailable)
        );
        assert_eq!(fs::read(&state).unwrap(), bytes);
    }
    fs::write(&state, &original).unwrap();
    drop(Inbox::open(root.path()).unwrap());
    fs::remove_file(&state).unwrap();
    assert_eq!(
        Inbox::open(root.path()).err(),
        Some(PanelError::Unavailable)
    );
    assert!(!state.exists());
    fs::remove_dir_all(root.path().join("web-notifications")).unwrap();
    assert_eq!(
        Inbox::open(root.path()).err(),
        Some(PanelError::Unavailable)
    );
    assert!(!root.path().join("web-notifications").exists());
    let incomplete = tempfile::tempdir().unwrap();
    fs::create_dir(incomplete.path().join("web-notifications")).unwrap();
    assert_eq!(
        Inbox::open(incomplete.path()).err(),
        Some(PanelError::Unavailable)
    );
    assert!(
        !incomplete
            .path()
            .join("web-notifications/state.json")
            .exists()
    );
}

#[tokio::test]
async fn missing_persisted_file_rejects_read_receipt_and_closes_live_inbox() {
    let root = tempfile::tempdir().unwrap();
    let (kernel, tasks, triggers) = open_plugins(root.path()).await;
    let task = completed_task(&tasks, &owner(1), "live-storage-loss");
    let inbox = Inbox::open(root.path()).unwrap();
    inbox.collect(&tasks, &triggers, 10).unwrap();
    let item = inbox.list(None, 1).unwrap().items.remove(0);
    let delivered = stored_task(&tasks, &task.owner, &task.id);
    let state = inbox.directory.join("state.json");
    fs::remove_file(&state).unwrap();
    assert_eq!(inbox.mark_read(item.id, 11), Err(PanelError::Unavailable));
    assert_eq!(inbox.list(None, 1).err(), Some(PanelError::Unavailable));
    assert_eq!(
        inbox.collect(&tasks, &triggers, 12),
        Err(PanelError::Unavailable)
    );
    assert!(!state.exists());
    assert!(stored_task(&tasks, &task.owner, &task.id) == delivered);
    drop(inbox);
    assert_eq!(
        Inbox::open(root.path()).err(),
        Some(PanelError::Unavailable)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn optional_plugins_preserve_missing_service_receipts_and_collect_independently() {
    let root = tempfile::tempdir().unwrap();
    let (kernel, tasks, triggers) = open_plugins(root.path()).await;
    let terminal = completed_task(&tasks, &owner(1), "optional-task");
    let trigger = triggers
        .create(
            session(&owner(1)),
            "optional-trigger".into(),
            trigger_spec(None),
            1,
        )
        .unwrap();
    let trigger = finish_trigger(&triggers, &trigger.id, "可选插件的真实结果");
    let records = [pending_task(&terminal, 10), pending_trigger(&trigger, 10)];
    let inbox = Inbox::open(root.path()).unwrap();
    {
        let mut inner = inbox.inner.lock().unwrap();
        for record in &records {
            inbox.insert(&mut inner, record.clone()).unwrap();
        }
    }
    let state = inbox.directory.join("state.json");
    let original = fs::read(&state).unwrap();
    let saved = inbox.list(None, 100).unwrap().items;
    inbox.collect_available(None, None, 11).unwrap();
    assert_eq!(fs::read(&state).unwrap(), original);
    assert!(stored_task(&tasks, &terminal.owner, &terminal.id) == terminal);
    assert_eq!(triggers.snapshot().unwrap(), vec![trigger.clone()]);

    inbox.collect_available(Some(&tasks), None, 12).unwrap();
    assert_local_task_receipt(
        &stored_task(&tasks, &terminal.owner, &terminal.id),
        &records[0].event,
    );
    assert_eq!(triggers.snapshot().unwrap(), vec![trigger.clone()]);
    {
        let inner = inbox.inner.lock().unwrap();
        assert!(inner.ledger.records[0].pending.is_none());
        assert_eq!(
            inner.ledger.records[0].settlement.as_deref(),
            Some("recorded")
        );
        assert!(inner.ledger.records[1].pending.is_some());
        assert_eq!(inner.ledger.records[1].settlement, None);
        assert_eq!(inner.ledger.seen.len(), 2);
        assert!(inner.ledger.seen.iter().any(|seen| {
            seen.kind == "trigger"
                && seen.source == records[1].source
                && seen.events == vec![records[1].event.clone()]
        }));
    }
    let partially_settled = fs::read(&state).unwrap();
    inbox.collect_available(Some(&tasks), None, 13).unwrap();
    assert_eq!(fs::read(&state).unwrap(), partially_settled);
    inbox.collect_available(None, Some(&triggers), 14).unwrap();
    assert_eq!(
        triggers.snapshot().unwrap()[0].delivery,
        Some(Delivery::StoredLocally)
    );
    {
        let inner = inbox.inner.lock().unwrap();
        assert!(inner.ledger.records.iter().all(|record| {
            record.pending.is_none() && record.settlement.as_deref() == Some("recorded")
        }));
        assert_eq!(inner.ledger.seen.len(), 2);
        assert!(
            inner
                .ledger
                .seen
                .iter()
                .any(|seen| seen.kind == "task" && seen.source == records[0].source)
        );
    }
    inbox
        .collect_available(Some(&tasks), Some(&triggers), 15)
        .unwrap();
    assert_eq!(inbox.list(None, 100).unwrap().items, saved);

    // 新事件分别由单独的任务或触发器能力首次采集，不依赖另一插件在线。
    let fresh_task = completed_task(&tasks, &owner(1), "optional-fresh-task");
    let fresh_trigger = triggers
        .create(
            session(&owner(1)),
            "optional-fresh-trigger".into(),
            trigger_spec(None),
            1,
        )
        .unwrap();
    let fresh_trigger = finish_trigger(&triggers, &fresh_trigger.id, "独立采集提醒");
    inbox.collect_available(Some(&tasks), None, 16).unwrap();
    let task_only = inbox.list(None, 100).unwrap().items;
    assert_eq!(task_only.len(), 3);
    assert!(
        task_only
            .iter()
            .any(|item| item.source_id == fresh_task.id && item.kind == "task")
    );
    assert!(
        !task_only
            .iter()
            .any(|item| item.source_id == fresh_trigger.id)
    );
    assert_eq!(
        triggers
            .snapshot()
            .unwrap()
            .iter()
            .find(|item| item.id == fresh_trigger.id)
            .unwrap()
            .delivery,
        Some(Delivery::Pending)
    );
    inbox.collect_available(None, Some(&triggers), 17).unwrap();
    let both = inbox.list(None, 100).unwrap().items;
    assert_eq!(both.len(), 4);
    assert!(
        both.iter()
            .any(|item| item.source_id == fresh_trigger.id && item.kind == "trigger")
    );
    inbox
        .collect_available(Some(&tasks), Some(&triggers), 18)
        .unwrap();
    assert_eq!(inbox.list(None, 100).unwrap().items, both);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn progress_render_reports_wait_stop_and_uncertain_execution_truthfully() {
    let root = tempfile::tempdir().unwrap();
    let (kernel, tasks, _) = open_plugins(root.path()).await;
    let queued = tasks
        .create(&owner(1), task_spec("进度文案核验"), "progress-render", 1)
        .unwrap();
    let queued_body = progress_body(&queued, 100);
    assert!(queued_body.contains("仍在排队，尚未开始"));
    assert!(queued_body.contains("尚未确认完成"));
    let running = tasks.start(&queued.id, 2).unwrap();
    let running_body = progress_body(&running, 100);
    assert!(running_body.contains("执行器已接收任务"));
    assert!(running_body.contains("当前进程是否仍在执行尚无独立确认"));
    assert!(!running_body.contains("任务仍在执行"));

    let mut cancelling = running.clone();
    cancelling.cancel_requested = true;
    cancelling.execution.as_mut().unwrap().deadline_at_ms = Some(99);
    let cancelling_body = progress_body(&cancelling, 100);
    assert!(cancelling_body.contains("已收到停止请求，等待执行器确认停止"));
    assert!(!cancelling_body.contains("已确认停止"));

    for authorization_deadline in [false, true] {
        let mut expired = running.clone();
        let execution = expired.execution.as_mut().unwrap();
        if authorization_deadline {
            execution.authorization_deadline_at_ms = Some(100);
        } else {
            execution.deadline_at_ms = Some(100);
        }
        execution.resume_at_ms = Some(101);
        let expired_body = progress_body(&expired, 100);
        assert!(expired_body.contains("原授权或执行期限已到"));
        assert!(expired_body.contains("没有新的执行确认"));
        assert!(!expired_body.contains("恢复时点尚未到达"));
    }

    let mut recovering = running.clone();
    recovering.execution.as_mut().unwrap().resume_at_ms = Some(101);
    let waiting_body = progress_body(&recovering, 100);
    assert!(waiting_body.contains("等待自动恢复，恢复时点尚未到达"));
    let due_body = progress_body(&recovering, 101);
    assert!(due_body.contains("已到自动恢复时点，等待调度接续"));
    assert!(due_body.contains("尚无新的执行确认"));

    let mut blocked = running;
    blocked.status = TaskStatus::Blocked;
    blocked.finished_at_ms = Some(99);
    blocked.result = Some("等待磁盘恢复".into());
    let blocked_body = progress_body(&blocked, 100);
    assert!(blocked_body.contains("本次执行已停止，等待补充信息或解除阻塞"));
    assert!(blocked_body.contains("阻塞记录：等待磁盘恢复"));
    for body in [
        running_body,
        cancelling_body,
        waiting_body,
        due_body,
        blocked_body,
    ] {
        assert!(body.contains("尚未确认完成"));
        assert!(!body.contains("任务仍在执行"));
    }
    kernel.stop_all().await.unwrap();
}

#[test]
fn full_inbox_keeps_unread_items_and_deduplicates_events_after_read_item_eviction() {
    assert_eq!(MAX_ITEMS, 2048);
    let root = tempfile::tempdir().unwrap();
    let inbox = Inbox::open(root.path()).unwrap();
    let owner = owner(1);
    let source = source_key(
        &owner.session_id,
        &owner.user_id,
        "capacity-task",
        1,
        "task",
    );
    let template = Record {
        source: source.clone(),
        event: String::new(),
        pending: None,
        settlement: Some("recorded".into()),
        notification: Notification {
            id: 0,
            session_id: owner.session_id,
            source_id: "capacity-task".into(),
            kind: "task".into(),
            status: "completed".into(),
            title: "容量核验结果".into(),
            body: "已持久保存的正文".into(),
            truncated: false,
            created_at_ms: 1,
            read_at_ms: None,
        },
    };
    let records: Vec<_> = (1..=MAX_ITEMS)
        .map(|index| {
            let mut record = template.clone();
            record.notification.id = index as u64;
            record.event = hash(&(&source, index));
            record
        })
        .collect();
    let evicted = records[0].clone();
    let ledger = Ledger {
        version: 1,
        next_id: MAX_ITEMS as u64 + 1,
        seen: vec![Seen {
            source: source.clone(),
            kind: "task".into(),
            events: records.iter().map(|record| record.event.clone()).collect(),
        }],
        records,
    };
    inbox
        .commit(&mut inbox.inner.lock().unwrap(), ledger)
        .unwrap();
    let state = inbox.directory.join("state.json");
    let full_bytes = fs::read(&state).unwrap();
    let mut next = template;
    next.event = hash(&(&source, MAX_ITEMS + 1));
    assert_eq!(
        inbox.insert(&mut inbox.inner.lock().unwrap(), next.clone()),
        Err(PanelError::Capacity)
    );
    assert_eq!(fs::read(&state).unwrap(), full_bytes);
    assert_eq!(inbox.list(None, 1).unwrap().unread, MAX_ITEMS);
    assert!(inbox.mark_read(evicted.notification.id, 2).unwrap());
    inbox
        .insert(&mut inbox.inner.lock().unwrap(), next)
        .unwrap();
    let retained: Vec<_> = {
        let inner = inbox.inner.lock().unwrap();
        assert_eq!(inner.ledger.records.len(), MAX_ITEMS);
        assert_eq!(inner.ledger.records[0].notification.id, 2);
        assert_eq!(
            inner.ledger.records.last().unwrap().notification.id,
            MAX_ITEMS as u64 + 1
        );
        assert_eq!(inner.ledger.seen[0].events.len(), MAX_ITEMS + 1);
        assert!(inner.ledger.seen[0].events.contains(&evicted.event));
        inner
            .ledger
            .records
            .iter()
            .map(|record| record.notification.clone())
            .collect()
    };
    assert_eq!(inbox.list(None, 1).unwrap().unread, MAX_ITEMS);
    let committed = fs::read(&state).unwrap();
    inbox
        .insert(&mut inbox.inner.lock().unwrap(), evicted)
        .unwrap();
    assert_eq!(fs::read(&state).unwrap(), committed);
    drop(inbox);
    let inbox = Inbox::open(root.path()).unwrap();
    assert_eq!(inbox.list(None, 1).unwrap().unread, MAX_ITEMS);
    assert_eq!(
        inbox
            .inner
            .lock()
            .unwrap()
            .ledger
            .records
            .iter()
            .map(|record| record.notification.clone())
            .collect::<Vec<_>>(),
        retained
    );
    assert_eq!(fs::read(&state).unwrap(), committed);
}
