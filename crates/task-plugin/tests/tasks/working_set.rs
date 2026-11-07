//! 仅在自有临时目录构造完整旧账本，随后通过真实 TaskAdmin 和文件归档验收。
use super::*;
use std::collections::BTreeMap;

fn queued(source: &str, created: u64) -> Task {
    let owner = owner("alice");
    Task {
        id: task_id(&owner.user_id, source),
        owner,
        source: source.into(),
        spec: spec("保留任务现场"),
        created_at_ms: created,
        status: TaskStatus::Queued,
        cancel_requested: false,
        started_at_ms: None,
        finished_at_ms: None,
        usage: None,
        result: None,
        failure: None,
        notice: None,
        execution: Some(TaskExecution::default()),
    }
}

fn cancelled(source: &str, created: u64) -> Task {
    let mut task = queued(source, created);
    task.status = TaskStatus::Cancelled;
    task.finished_at_ms = Some(created + 1);
    task.execution.as_mut().unwrap().last_finished_at_ms = task.finished_at_ms;
    task
}

fn terminal(source: &str, created: u64, status: TaskStatus) -> Task {
    let mut task = queued(source, created);
    task.status = status;
    task.started_at_ms = Some(created + 1);
    task.finished_at_ms = Some(created + 2);
    task.usage = Some(usage(0));
    task.notice = Some(Notice {
        status: NoticeStatus::Delivered,
        attempts: vec![DeliveryAttempt {
            channel: DeliveryChannel::Proactive,
            started_at_ms: created + 3,
            finished_at_ms: Some(created + 4),
            result: Some(sent()),
        }],
        closed_at_ms: Some(created + 4),
    });
    if status == TaskStatus::Failed {
        task.failure = Some("合成的服务失败回执".into());
    } else {
        task.result = Some("合成的保留结果".into());
    }
    task.execution.as_mut().unwrap().last_finished_at_ms = task.finished_at_ms;
    task
}

fn ended_exploration(source: &str, created: u64, acknowledged: bool) -> Task {
    let mut task = terminal(source, created, TaskStatus::SessionEnded);
    task.notice = None;
    let mut frame = checkpoint("alice");
    frame.remaining = None;
    frame.result = task.result.clone();
    let execution = task.execution.as_mut().unwrap();
    execution.kind = TaskExecutionKind::Exploration;
    execution.session_end_reason = Some(TaskSessionEndReason::NaturalStop);
    execution.checkpoint = Some(frame);
    execution.exploration_acknowledged_at_ms = acknowledged.then_some(created + 3);
    task
}

fn sized(mut task: Task, bytes: usize) -> Task {
    let owner = task.owner.clone();
    let context = |length| DelegatedContext {
        owner: owner.clone(),
        messages_json: serde_json::to_string(&[eve_llm_api::ChatMessage::text(
            eve_llm_api::ChatRole::User,
            "x".repeat(length),
        )])
        .unwrap(),
    };
    task.spec.context = Some(context(1));
    let base = serde_json::to_vec(&task).unwrap().len();
    assert!(bytes >= base);
    task.spec.context = Some(context(bytes - base + 1));
    assert_eq!(serde_json::to_vec(&task).unwrap().len(), bytes);
    task
}

fn digest(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn seed(world: &World, archive: &Archive, tasks: &[Task], version: u32, archived: &[bool]) {
    let entries: Vec<_> = tasks
        .iter()
        .enumerate()
        .map(|(index, task)| {
            let bytes = serde_json::to_vec(task).unwrap();
            let hash = digest(&bytes);
            archive.put(&hash, &bytes).unwrap();
            let mut directory = serde_json::to_value(task).unwrap();
            directory["spec"].as_object_mut().unwrap().remove("context");
            directory["result"] = Value::Null;
            directory["failure"] = Value::Null;
            if let Some(execution) = directory.get_mut("execution") {
                execution["checkpoint"] = Value::Null;
                execution.as_object_mut().unwrap().remove("review_messages");
            }
            let mut entry = serde_json::json!({
                "directory": directory,
                "record": {"hash": hash, "bytes": bytes.len()},
            });
            if version == 5 {
                entry["archived"] = archived[index].into();
            }
            entry
        })
        .collect();
    world.write(
        serde_json::to_vec(&serde_json::json!({
            "format_version": version, "tasks": entries,
        }))
        .unwrap(),
    );
}

fn index(world: &World) -> Value {
    serde_json::from_slice(&world.raw().unwrap()).unwrap()
}

fn entry<'a>(index: &'a Value, id: &str) -> &'a Value {
    index["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["directory"]["id"] == id)
        .unwrap()
}

fn files(world: &World) -> BTreeMap<String, String> {
    std::fs::read_dir(world._directory.path().join("archive"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .map(|path| {
            let name = path.file_name().unwrap().to_str().unwrap().to_owned();
            (name, digest(&std::fs::read(path).unwrap()))
        })
        .collect()
}

fn full_history(count: usize, total: usize) -> Vec<Task> {
    (0..count)
        .map(|number| {
            let source = format!("history-{number}");
            let created = number as u64 * DAY_MS + 10;
            let task = if count == 40 && number < 39 {
                terminal(
                    &source,
                    created,
                    if number < 12 {
                        TaskStatus::Completed
                    } else {
                        TaskStatus::Failed
                    },
                )
            } else {
                cancelled(&source, created)
            };
            sized(task, total / count + usize::from(number < total % count))
        })
        .collect()
}

#[tokio::test]
async fn forty_terminal_records_allow_a_new_idle_review_without_losing_old_bodies_or_sources() {
    let world = World::new();
    let archive = world.archive();
    let old = full_history(40, MAX_STATE_BYTES - 39 * 160 - 40_000);
    seed(&world, &archive, &old, 4, &[]);
    let before = index(&world);
    let raw = world.raw();
    let old_files = files(&world);
    let writes = archive.writes.load(Ordering::SeqCst);
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    assert_eq!(world.raw(), raw, "只读旧格式不提前改写为格式 5");
    assert_eq!(archive.writes.load(Ordering::SeqCst), writes);
    let created = tasks
        .create_authorized(
            &owner("alice"),
            sized(queued("review", 1), 16 * 1024).spec,
            &format!("{IDLE_TASK_SOURCE_PREFIX}working-set"),
            vec!["保留自由阅读和回顾的会话语境".into()],
            None,
            None,
            40 * DAY_MS + 10,
        )
        .unwrap();
    let current = index(&world);
    assert_eq!(current["format_version"], 5);
    assert_eq!(current["tasks"].as_array().unwrap().len(), 41);
    for (number, task) in old.iter().enumerate() {
        assert_eq!(
            entry(&current, &task.id)["archived"],
            number == 0,
            "只需要最早结算的一条即可释放本次工作集空间"
        );
    }
    assert_eq!(entry(&current, &created.id)["archived"], false);
    let current_files = files(&world);
    for task in &old {
        assert_eq!(
            entry(&current, &task.id)["directory"],
            entry(&before, &task.id)["directory"]
        );
        assert_eq!(
            entry(&current, &task.id)["record"],
            entry(&before, &task.id)["record"]
        );
        assert_eq!(
            tasks.get_owned(&task.owner, &task.id).unwrap().as_ref(),
            Some(task)
        );
        assert_eq!(
            tasks
                .create(
                    &task.owner,
                    spec("重复来源不覆盖历史"),
                    &task.source,
                    40 * DAY_MS + 11
                )
                .unwrap(),
            *task,
        );
    }
    for (name, hash) in old_files {
        assert_eq!(current_files.get(&name), Some(&hash));
    }
    let keys: Vec<_> = old
        .iter()
        .map(|task| TaskEvidenceKey {
            id: task.id.clone(),
            owner: task.owner.clone(),
        })
        .collect();
    let evidence = tasks.evidence_snapshot(&keys).unwrap();
    for task in &old {
        assert_eq!(evidence.find(&task.id).unwrap(), Some(task));
    }
    assert_eq!(tasks.get_owned(&owner("bob"), &old[0].id).unwrap(), None);
    assert_eq!(tasks.summaries_for(&owner("alice")).unwrap().len(), 41);
    assert_eq!(tasks.scheduling_snapshot().unwrap().tasks.len(), 41);
    assert_eq!(tasks.delivery_snapshot().unwrap().tasks.len(), 41);
    assert_eq!(tasks.progress_snapshot().unwrap().tasks.len(), 41);
    let committed = world.raw();
    kernel.stop_all().await.unwrap();
    assert!(
        world.open().await.is_err(),
        "没有冷档案能力的宿主不能降级读格式 5"
    );
    assert_eq!(world.raw(), committed);
    let (kernel, tasks) = world.cold(archive).await.unwrap();
    assert_eq!(world.raw(), committed);
    assert_eq!(
        tasks.get_owned(&created.owner, &created.id).unwrap(),
        Some(created)
    );
    for task in old {
        assert_eq!(tasks.get_owned(&task.owner, &task.id).unwrap(), Some(task));
    }
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn rejected_single_record_never_publishes_tentative_archival_or_writes_a_body() {
    let world = World::new();
    let archive = world.archive();
    let old = sized(cancelled("old", 10), 64 * 1024);
    seed(&world, &archive, std::slice::from_ref(&old), 4, &[]);
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    let before = world.raw();
    let writes = archive.writes.load(Ordering::SeqCst);
    let file_state = files(&world);
    // 单条正文没有越过 16 MiB；加上自己的收尾预留后仍不能装入工作集。
    let oversized = sized(queued("oversized", DAY_MS), MAX_STATE_BYTES - 1024);
    assert_eq!(
        tasks.create(
            &oversized.owner,
            oversized.spec,
            &oversized.source,
            oversized.created_at_ms
        ),
        Err(TaskError::LimitReached),
    );
    assert_eq!(world.raw(), before);
    assert_eq!(archive.writes.load(Ordering::SeqCst), writes);
    assert_eq!(files(&world), file_state);
    assert_eq!(tasks.get_owned(&old.owner, &old.id).unwrap(), Some(old));
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn version_five_keeps_the_original_active_task_limit() {
    let world = World::new();
    let archive = world.archive();
    let active_only: Vec<_> = (0..4)
        .map(|number| sized(queued(&format!("active-{number}"), 10), 2 * 1024))
        .collect();
    seed(&world, &archive, &active_only, 5, &[false; 4]);
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    let before = world.raw();
    let writes = archive.writes.load(Ordering::SeqCst);
    assert_eq!(
        tasks.create(&owner("alice"), spec("原并发限制"), "fifth", 20),
        Err(TaskError::LimitReached)
    );
    assert_eq!(world.raw(), before);
    assert_eq!(archive.writes.load(Ordering::SeqCst), writes);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn pending_tasks_notices_progress_and_unacknowledged_exploration_cannot_be_archived() {
    let world = World::new();
    let archive = world.archive();
    let mut pending = terminal("notice", 10, TaskStatus::Completed);
    pending.notice = Some(Notice {
        status: NoticeStatus::Pending,
        attempts: vec![],
        closed_at_ms: None,
    });
    // 三条尚未结算记录还需保留完整转义结果与通知空间；本次进度认领也重新准入。
    let bytes = (MAX_STATE_BYTES - 1024 * 1024) / 4;
    let old = [
        sized(queued("queued", 10), bytes),
        sized(pending, bytes),
        sized(ended_exploration("unacknowledged", 10, false), bytes),
        sized(terminal("progress", 10, TaskStatus::Blocked), bytes),
    ];
    seed(&world, &archive, &old, 4, &[]);
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    let now = 10 + TASK_PROGRESS_INTERVAL_MS;
    let inflight = tasks
        .claim_progress(&old[3].owner, &old[3].id, DeliveryChannel::Proactive, now)
        .unwrap();
    assert!(inflight.settled_at().is_none());
    let before = world.raw();
    let writes = archive.writes.load(Ordering::SeqCst);
    let new = sized(queued("new", now + 10), 1024 * 1024);
    assert_eq!(
        tasks.create(&new.owner, new.spec.clone(), &new.source, new.created_at_ms),
        Err(TaskError::LimitReached)
    );
    assert_eq!(world.raw(), before);
    assert_eq!(archive.writes.load(Ordering::SeqCst), writes);
    let settled = tasks
        .record_progress_delivery(&old[3].owner, &old[3].id, 1, sent(), now + 20)
        .unwrap();
    assert!(settled.settled_at().is_some());
    tasks
        .create(&new.owner, new.spec, &new.source, new.created_at_ms)
        .unwrap();
    let current = index(&world);
    assert_eq!(current["format_version"], 5);
    for task in &old[..3] {
        assert_eq!(entry(&current, &task.id)["archived"], false);
        assert_eq!(
            tasks.get_owned(&task.owner, &task.id).unwrap().as_ref(),
            Some(task)
        );
    }
    assert_eq!(entry(&current, &settled.id)["archived"], true);
    assert_eq!(
        tasks.get_owned(&settled.owner, &settled.id).unwrap(),
        Some(settled)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn exploration_acknowledgement_allows_history_archival_without_changing_its_experience() {
    let world = World::new();
    let archive = world.archive();
    let old = sized(
        ended_exploration("experience", 10, false),
        MAX_STATE_BYTES - 40_000,
    );
    seed(&world, &archive, std::slice::from_ref(&old), 4, &[]);
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    let before = world.raw();
    let writes = archive.writes.load(Ordering::SeqCst);
    let new = sized(queued("new", DAY_MS + 10), 16 * 1024);
    assert_eq!(
        tasks.create(&new.owner, new.spec.clone(), &new.source, new.created_at_ms),
        Err(TaskError::LimitReached)
    );
    assert_eq!(world.raw(), before);
    assert_eq!(archive.writes.load(Ordering::SeqCst), writes);
    let confirmed = tasks
        .acknowledge_exploration(&old.owner, &old.id, old.finished_at_ms.unwrap(), 20)
        .unwrap();
    tasks
        .create(&new.owner, new.spec, &new.source, new.created_at_ms)
        .unwrap();
    assert_eq!(entry(&index(&world), &old.id)["archived"], true);
    assert_eq!(
        tasks.get_owned(&old.owner, &old.id).unwrap(),
        Some(confirmed.clone())
    );
    assert_eq!(confirmed.result, old.result);
    assert_eq!(
        confirmed.execution.as_ref().unwrap().checkpoint,
        old.execution.as_ref().unwrap().checkpoint
    );
    assert!(confirmed.notice.is_none());
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.cold(archive).await.unwrap();
    assert_eq!(
        tasks.get_owned(&old.owner, &old.id).unwrap(),
        Some(confirmed)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn archived_blocked_resume_reenters_capacity_before_any_write_and_can_archive_other_settled_work()
 {
    let world = World::new();
    let archive = world.archive();
    let mut blocked = terminal("blocked", 10, TaskStatus::Blocked);
    let mut frame = checkpoint("alice");
    frame.result = blocked.result.clone();
    frame.remaining = None;
    blocked.execution.as_mut().unwrap().checkpoint = Some(frame);
    let blocked = sized(blocked, 6 * 1024 * 1024);
    let active = sized(queued("active", 20), 11 * 1024 * 1024);
    seed(
        &world,
        &archive,
        &[blocked.clone(), active.clone()],
        5,
        &[true, false],
    );
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    let before = world.raw();
    let writes = archive.writes.load(Ordering::SeqCst);
    assert_eq!(
        tasks.resume_blocked(&blocked.owner, &blocked.id, "条件已解除", 100),
        Err(TaskError::LimitReached),
    );
    assert_eq!(world.raw(), before);
    assert_eq!(archive.writes.load(Ordering::SeqCst), writes);
    assert_eq!(
        tasks.get_owned(&blocked.owner, &blocked.id).unwrap(),
        Some(blocked.clone())
    );
    let cancelled = tasks
        .cancel(&active.owner.user_id, &active.id, 110)
        .unwrap();
    let resumed = tasks
        .resume_blocked(&blocked.owner, &blocked.id, "条件已解除", 120)
        .unwrap();
    assert_eq!(resumed.status, TaskStatus::Running);
    assert_eq!(resumed.spec, blocked.spec);
    assert_eq!(resumed.execution.as_ref().unwrap().recoveries, 1);
    assert_eq!(entry(&index(&world), &blocked.id)["archived"], false);
    assert_eq!(entry(&index(&world), &cancelled.id)["archived"], true);
    assert_eq!(
        tasks.get_owned(&cancelled.owner, &cancelled.id).unwrap(),
        Some(cancelled)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn total_history_limit_rejects_before_payload_write_and_keeps_every_archived_reference() {
    let world = World::new();
    let archive = world.archive();
    let old = full_history(4, MAX_ARCHIVE_BYTES - 1024);
    seed(&world, &archive, &old, 5, &[true; 4]);
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    let before = world.raw();
    let writes = archive.writes.load(Ordering::SeqCst);
    let large = sized(queued("over-total", 4 * DAY_MS), 8 * 1024);
    assert_eq!(
        tasks.create(&large.owner, large.spec, &large.source, large.created_at_ms),
        Err(TaskError::LimitReached),
    );
    assert_eq!(world.raw(), before);
    assert_eq!(archive.writes.load(Ordering::SeqCst), writes);
    for task in &old {
        assert_eq!(
            tasks.get_owned(&task.owner, &task.id).unwrap().as_ref(),
            Some(task)
        );
    }
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn physical_archive_limit_cannot_publish_archival_even_when_the_working_set_would_fit() {
    let world = World::new();
    let archive = world.archive();
    let old = full_history(2, MAX_STATE_BYTES - 64 * 1024);
    seed(&world, &archive, &old, 4, &[]);
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    let backup = world
        ._directory
        .path()
        .join("archive")
        .join("manual-backup.keep");
    let file = std::fs::File::create(&backup).unwrap();
    file.set_len(MAX_ARCHIVE_BYTES as u64).unwrap();
    let before = world.raw();
    let writes = archive.writes.load(Ordering::SeqCst);
    let file_state = files(&world);
    let new = sized(queued("new", 2 * DAY_MS), 256 * 1024);
    assert_eq!(
        tasks.create(&new.owner, new.spec.clone(), &new.source, new.created_at_ms),
        Err(TaskError::LimitReached),
    );
    assert_eq!(world.raw(), before);
    assert_eq!(archive.writes.load(Ordering::SeqCst), writes);
    assert_eq!(files(&world), file_state);
    assert_eq!(
        std::fs::metadata(&backup).unwrap().len(),
        MAX_ARCHIVE_BYTES as u64
    );
    for task in &old {
        assert_eq!(
            tasks.get_owned(&task.owner, &task.id).unwrap().as_ref(),
            Some(task)
        );
    }
    file.set_len(0).unwrap();
    drop(file);
    assert_eq!(
        tasks
            .create(&new.owner, new.spec.clone(), &new.source, new.created_at_ms)
            .unwrap(),
        new,
    );
    assert_eq!(index(&world)["format_version"], 5);
    assert!(backup.exists());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn physical_closeout_reserve_rejects_later_admission_and_keeps_finish_and_restart_available()
{
    let world = World::new();
    let archive = world.archive();
    let old = full_history(4, 61 * 1024 * 1024);
    seed(&world, &archive, &old, 5, &[true; 4]);
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    let now = 4 * DAY_MS + 10;
    let task = sized(queued("active-closeout", now), 1024 * 1024);
    let task = tasks
        .create(&task.owner, task.spec, &task.source, now)
        .unwrap();
    let running = tasks.start(&task.id, now + 1).unwrap();
    let next = sized(queued("next", now + 2), 1536 * 1024);
    let before = world.raw();
    let file_state = files(&world);
    let writes = archive.writes.load(Ordering::SeqCst);
    assert_eq!(
        tasks.create(&next.owner, next.spec, &next.source, now + 2),
        Err(TaskError::LimitReached),
    );
    assert_eq!(world.raw(), before);
    assert_eq!(archive.writes.load(Ordering::SeqCst), writes);
    assert_eq!(files(&world), file_state);
    assert_eq!(
        tasks.get_owned(&running.owner, &running.id).unwrap(),
        Some(running.clone())
    );
    kernel.stop_all().await.unwrap();
    // 先验收已接纳 Running 的重启替换，再写入最坏转义结果与通知回执。
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    let recovered = tasks
        .get_owned(&running.owner, &running.id)
        .unwrap()
        .unwrap();
    assert_eq!(recovered.status, TaskStatus::Running);
    assert_eq!(recovered.execution.as_ref().unwrap().recoveries, 1);
    let result = "\0".repeat(MAX_RESULT_BYTES);
    let finished = tasks
        .finish(
            &task.id,
            TaskEnd::Completed {
                result: result.clone(),
            },
            usage(0),
            now + 3,
        )
        .unwrap();
    assert_eq!(finished.result, Some(result));
    tasks
        .claim(
            &task.id,
            DeliveryChannel::Passive {
                message_id: "\\".repeat(256),
            },
            now + 4,
        )
        .unwrap();
    tasks
        .record_delivery(
            &task.id,
            DeliveryResult::Sent {
                platform_message_id: Some("\\".repeat(256)),
            },
            now + 5,
        )
        .unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.cold(archive).await.unwrap();
    let saved = tasks.get_owned(&task.owner, &task.id).unwrap().unwrap();
    assert_eq!(saved.status, TaskStatus::Completed);
    assert_eq!(saved.notice.unwrap().status, NoticeStatus::Delivered);
    for task in &old {
        assert_eq!(
            tasks.get_owned(&task.owner, &task.id).unwrap().as_ref(),
            Some(task)
        );
    }
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn closeout_reserve_covers_one_index_commit_for_all_recovered_tasks() {
    let world = World::new();
    let archive = world.archive();
    let old = full_history(4, 54 * 1024 * 1024);
    seed(&world, &archive, &old, 5, &[true; 4]);
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    let mut running = Vec::new();
    let now = 5 * DAY_MS;
    for number in 0..4 {
        let new = sized(queued(&format!("recover-{number}"), now), 1024 * 1024);
        let task = tasks
            .create(&new.owner, new.spec, &new.source, now)
            .unwrap();
        running.push(tasks.start(&task.id, now + 1).unwrap());
    }
    let before = world.raw();
    let writes = archive.writes.load(Ordering::SeqCst);
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    assert_ne!(world.raw(), before);
    assert_eq!(archive.writes.load(Ordering::SeqCst), writes + 4);
    for task in &running {
        let recovered = tasks.get_owned(&task.owner, &task.id).unwrap().unwrap();
        assert_eq!(recovered.spec, task.spec);
        assert_eq!(recovered.execution.as_ref().unwrap().recoveries, 1);
        assert_eq!(recovered.status, TaskStatus::Running);
        tasks
            .cancel(&task.owner.user_id, &task.id, now + 3)
            .unwrap();
    }
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.cold(archive).await.unwrap();
    for task in &running {
        let stopped = tasks.get_owned(&task.owner, &task.id).unwrap().unwrap();
        assert_eq!(stopped.status, TaskStatus::Cancelled);
        assert!(stopped.notice.is_none());
    }
    for task in &old {
        assert_eq!(
            tasks.get_owned(&task.owner, &task.id).unwrap().as_ref(),
            Some(task)
        );
    }
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn restart_reclaims_confirmed_retired_bodies_after_compaction_or_commit_acknowledgement_failure()
 {
    for failure in [
        "compact",
        "index-after",
        "index-before",
        "read-unconfirmed",
        "corrupt-orphan",
    ] {
        let world = World::new();
        let archive = world.archive();
        let old = full_history(4, 59 * 1024 * 1024);
        seed(&world, &archive, &old, 5, &[true; 4]);
        let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
        let now = 5 * DAY_MS;
        let task = sized(queued("crash-before-compact", now), 2 * 1024 * 1024);
        let task = tasks
            .create(&task.owner, task.spec, &task.source, now)
            .unwrap();
        tasks.start(&task.id, now + 1).unwrap();
        let before = world.raw();
        let old_body = world.cold_path(&task.id);
        let old_bytes = std::fs::read(&old_body).unwrap();
        let writes = archive.writes.load(Ordering::SeqCst);
        match failure {
            "index-before" => world.state.fail.store(true, Ordering::SeqCst),
            "index-after" | "read-unconfirmed" => {
                world.state.fail_after.store(true, Ordering::SeqCst)
            }
            _ => archive.fail_compact.store(true, Ordering::SeqCst),
        }
        let mut frame = checkpoint("alice");
        frame.messages.push(eve_llm_api::ChatMessage::text(
            eve_llm_api::ChatRole::User,
            "保留已保存检查点",
        ));
        let updated = tasks.checkpoint(&task.id, frame.clone(), u64::MAX, now + 2);
        if matches!(failure, "compact" | "corrupt-orphan") {
            assert!(updated.is_ok());
            assert!(tasks.get_owned(&task.owner, &task.id).unwrap().is_some());
        } else {
            assert_eq!(updated, Err(TaskError::Storage));
            assert_eq!(
                tasks.scheduling_snapshot().err(),
                Some(TaskError::Unavailable)
            );
        }
        assert_eq!(archive.writes.load(Ordering::SeqCst), writes + 1);
        assert_eq!(std::fs::read(&old_body).unwrap(), old_bytes);
        assert_eq!(files(&world).len(), old.len() + 2, "新旧正文必须共存");
        if failure == "index-before" {
            assert_eq!(world.raw(), before);
        } else {
            assert_ne!(world.raw(), before);
        }
        let committed = world.raw();
        archive.fail_compact.store(false, Ordering::SeqCst);
        world.state.fail.store(false, Ordering::SeqCst);
        world.state.fail_after.store(false, Ordering::SeqCst);
        kernel.stop_all().await.unwrap();
        if failure == "read-unconfirmed" {
            world.state.hide_ledger.store(true, Ordering::SeqCst);
            let saved_files = files(&world);
            assert!(world.cold(archive.clone()).await.is_err());
            assert_eq!(files(&world), saved_files);
            assert_eq!(archive.writes.load(Ordering::SeqCst), writes + 1);
            world.state.hide_ledger.store(false, Ordering::SeqCst);
            assert_eq!(world.raw(), committed);
        }
        if failure == "corrupt-orphan" {
            // 保持文件长度，确保仍走物理峰值不足后的验证回收分支。
            let mut damaged = old_bytes.clone();
            damaged[0] ^= 1;
            std::fs::write(&old_body, &damaged).unwrap();
            let saved_files = files(&world);
            assert!(world.cold(archive.clone()).await.is_err());
            assert_eq!(world.raw(), committed);
            assert_eq!(files(&world), saved_files, "损坏孤儿不能提前回收");
            assert_eq!(archive.writes.load(Ordering::SeqCst), writes + 1);
            std::fs::write(&old_body, &old_bytes).unwrap();
        }
        // 残留正文约 63 MiB；直接再写约 2 MiB 恢复正文会超限。确认当前索引后
        // 先补回收上次退役/未提交正文，整批预检通过才写恢复记录。
        let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
        let recovered = tasks.get_owned(&task.owner, &task.id).unwrap().unwrap();
        assert_eq!(recovered.status, TaskStatus::Running);
        assert_eq!(recovered.execution.as_ref().unwrap().recoveries, 1);
        assert_eq!(
            recovered.execution.as_ref().unwrap().checkpoint,
            if failure == "index-before" {
                None
            } else {
                Some(frame)
            }
        );
        assert_eq!(archive.writes.load(Ordering::SeqCst), writes + 2);
        assert_eq!(files(&world).len(), old.len() + 1);
        for task in &old {
            assert_eq!(
                tasks.get_owned(&task.owner, &task.id).unwrap().as_ref(),
                Some(task)
            );
        }
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn strict_result_repair_reuses_reserved_space_across_restart_and_escaped_closeout() {
    let world = World::new();
    let archive = world.archive();
    let old = full_history(4, 60 * 1024 * 1024);
    seed(&world, &archive, &old, 5, &[true; 4]);
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    let now = 5 * DAY_MS;
    let contract = TaskCompletionContract::Text {
        required_fragments: vec!["verified".into()],
        json_equals: None,
        valid_json: false,
        json_required_pointers: vec![],
    };
    let requested = sized(queued("strict-space", now), 1024 * 1024);
    let task = tasks
        .create_authorized(
            &requested.owner,
            requested.spec,
            &requested.source,
            vec!["继续完成原文本".into()],
            Some(contract.clone()),
            Some(u64::MAX),
            now,
        )
        .unwrap();
    tasks.start(&task.id, now + 1).unwrap();
    let mut frame = checkpoint("alice");
    frame.remaining = None;
    frame.result = Some("\0".repeat(MAX_RESULT_BYTES));
    frame.closing = true;
    tasks
        .checkpoint(&task.id, frame.clone(), u64::MAX, now + 2)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    let repaired = tasks.get_owned(&task.owner, &task.id).unwrap().unwrap();
    assert_eq!(repaired.status, TaskStatus::Running);
    let execution = repaired.execution.unwrap();
    assert_eq!(execution.recoveries, 1);
    assert!(execution.completion_contract == Some(contract));
    assert_eq!(execution.review_messages, vec!["继续完成原文本"]);
    let mut continued = execution.checkpoint.unwrap();
    assert!(continued.messages.starts_with(&frame.messages));
    assert_eq!(continued.messages.len(), frame.messages.len() + 3);
    assert_eq!(continued.messages[frame.messages.len()].text, frame.result);
    assert!(continued.result.is_none());
    assert!(!continued.closing);
    assert_eq!(continued.provider_requests, frame.provider_requests);
    let mut result = "verified".to_owned();
    result.push_str(&"\0".repeat(MAX_RESULT_BYTES - result.len()));
    continued.result = Some(result.clone());
    continued.provider_requests += 1;
    tasks
        .checkpoint(&task.id, continued, u64::MAX, now + 3)
        .unwrap();
    let finished = tasks
        .finish(
            &task.id,
            TaskEnd::Completed {
                result: result.clone(),
            },
            TaskUsage {
                tool_calls: 0,
                model_requests: 2,
            },
            now + 4,
        )
        .unwrap();
    assert_eq!(finished.status, TaskStatus::Completed);
    assert_eq!(finished.result, Some(result));
    tasks
        .claim(&task.id, DeliveryChannel::Proactive, now + 5)
        .unwrap();
    tasks.record_delivery(&task.id, sent(), now + 6).unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.cold(archive).await.unwrap();
    let restored = tasks.get_owned(&task.owner, &task.id).unwrap().unwrap();
    assert_eq!(restored.status, TaskStatus::Completed);
    assert_eq!(restored.notice.unwrap().status, NoticeStatus::Delivered);
    for task in &old {
        assert_eq!(
            tasks.get_owned(&task.owner, &task.id).unwrap().as_ref(),
            Some(task)
        );
    }
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn overfull_restore_rejects_the_entire_recovery_batch_before_any_body_write() {
    let world = World::new();
    let archive = world.archive();
    let mut seeded = full_history(4, 60 * 1024 * 1024);
    for number in 0..2 {
        let mut task = sized(
            queued(&format!("overfull-{number}"), 4 * DAY_MS),
            1024 * 1024,
        );
        task.status = TaskStatus::Running;
        task.started_at_ms = Some(task.created_at_ms + 1);
        seeded.push(task);
    }
    seed(
        &world,
        &archive,
        &seeded,
        5,
        &[true, true, true, true, false, false],
    );
    let before = world.raw();
    let saved = files(&world);
    let writes = archive.writes.load(Ordering::SeqCst);
    assert!(world.cold(archive.clone()).await.is_err());
    assert_eq!(world.raw(), before);
    assert_eq!(files(&world), saved);
    assert_eq!(archive.writes.load(Ordering::SeqCst), writes);
}

#[tokio::test]
async fn v5_pressure_preserves_unknown_effects_notice_receipts_and_source_identity() {
    let world = World::new();
    let archive = world.archive();
    let old = full_history(2, MAX_STATE_BYTES - 64 * 1024);
    seed(&world, &archive, &old, 4, &[]);
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    let now = 3 * DAY_MS;
    let task = tasks
        .create(&owner("alice"), spec("保留未知效果"), "unknown-v5", now)
        .unwrap();
    assert_eq!(index(&world)["format_version"], 5);
    tasks.start(&task.id, now + 1).unwrap();
    let mut frame = checkpoint("alice");
    frame.pending_calls.push(eve_llm_api::ToolCall {
        id: "unknown-effect".into(),
        name: "write_file".into(),
        arguments: serde_json::json!({}),
    });
    frame.used_tool_calls = 1;
    frame.tool_batches = 1;
    tasks
        .checkpoint(&task.id, frame.clone(), u64::MAX, now + 2)
        .unwrap();
    tasks
        .finish(
            &task.id,
            TaskEnd::Blocked {
                result: "保留未确认现场".into(),
            },
            usage(1),
            now + 3,
        )
        .unwrap();
    tasks
        .claim(&task.id, DeliveryChannel::Proactive, now + 4)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = world.cold(archive).await.unwrap();
    let restored = tasks.get_owned(&task.owner, &task.id).unwrap().unwrap();
    assert_eq!(restored.status, TaskStatus::Blocked);
    assert_eq!(restored.execution.as_ref().unwrap().checkpoint, Some(frame));
    assert_eq!(
        restored.notice.as_ref().unwrap().status,
        NoticeStatus::Unknown
    );
    assert_eq!(
        restored.notice.as_ref().unwrap().attempts[0].result,
        Some(DeliveryResult::Unknown)
    );
    assert_eq!(
        tasks.resume_blocked(&task.owner, &task.id, "继续", now + 5),
        Err(TaskError::InvalidTransition)
    );
    assert_eq!(
        tasks.claim(&task.id, DeliveryChannel::Proactive, now + 6),
        Err(TaskError::InvalidTransition)
    );
    assert_eq!(
        tasks
            .create(&task.owner, spec("幂等保留"), &task.source, now + 7)
            .unwrap(),
        restored
    );
    let key = TaskEvidenceKey {
        id: task.id.clone(),
        owner: task.owner.clone(),
    };
    assert_eq!(
        tasks
            .evidence_snapshot(&[key])
            .unwrap()
            .find(&task.id)
            .unwrap(),
        Some(&restored)
    );
    kernel.stop_all().await.unwrap();
}

#[test]
fn physical_preflight_deduplicates_hashes_and_counts_unknown_files_without_writing() {
    let directory = tempfile::tempdir().unwrap();
    let archive = eve_task_plugin::FileTaskArchive::open(directory.path()).unwrap();
    let current = TaskArchiveRecord {
        hash: digest(b"current"),
        bytes: 7,
    };
    let next = TaskArchiveRecord {
        hash: digest(b"next"),
        bytes: 4,
    };
    archive.put(&current.hash, b"current").unwrap();
    let backup = directory.path().join("manual-backup.keep");
    let file = std::fs::File::create(&backup).unwrap();
    file.set_len((MAX_ARCHIVE_BYTES - current.bytes - next.bytes - 9) as u64)
        .unwrap();
    let check = |retained: &[TaskArchiveRecord], reserve_bytes| {
        archive.check_capacity(&TaskArchivePlan {
            current: &[current.clone(), current.clone()],
            retained,
            writes: &[next.clone(), next.clone()],
            reserve_bytes,
            reserve_files: 0,
        })
    };
    // 本次新 hash 只计一次；已确认旧 hash 也只能抵扣一次。
    assert_eq!(check(std::slice::from_ref(&next), 16), Ok(()));
    assert_eq!(
        check(std::slice::from_ref(&next), 17),
        Err(TaskError::LimitReached)
    );
    // 仍被候选引用的共享正文不能作为回收空间。
    assert_eq!(
        check(&[current.clone(), next.clone()], 10),
        Err(TaskError::LimitReached)
    );
    assert_eq!(archive.get(&current.hash).unwrap(), b"current");
    assert!(
        !directory
            .path()
            .join(format!("{}.json", next.hash))
            .exists()
    );
    assert_eq!(
        std::fs::metadata(backup).unwrap().len(),
        (MAX_ARCHIVE_BYTES - 20) as u64
    );
}

#[tokio::test]
async fn invalid_oversized_v4_is_not_silently_migrated_to_history_on_open() {
    let world = World::new();
    let archive = world.archive();
    let old = full_history(2, MAX_STATE_BYTES + 1024);
    seed(&world, &archive, &old, 4, &[]);
    let before = world.raw();
    let file_state = files(&world);
    let writes = archive.writes.load(Ordering::SeqCst);
    assert!(world.cold(archive.clone()).await.is_err());
    assert_eq!(world.raw(), before);
    assert_eq!(archive.writes.load(Ordering::SeqCst), writes);
    assert_eq!(files(&world), file_state);
}

#[tokio::test]
async fn pressure_migration_failures_preserve_old_and_new_files_until_restart_checks_the_index() {
    for failure in [
        "payload-before",
        "payload-after",
        "index-before",
        "index-after",
    ] {
        let world = World::new();
        let archive = world.archive();
        let old = full_history(2, MAX_STATE_BYTES - 64 * 1024);
        seed(&world, &archive, &old, 4, &[]);
        let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
        let before = world.raw();
        let old_files = files(&world);
        match failure {
            "payload-before" => archive.fail.store(true, Ordering::SeqCst),
            "payload-after" => archive.fail_after.store(true, Ordering::SeqCst),
            "index-before" => world.state.fail.store(true, Ordering::SeqCst),
            _ => world.state.fail_after.store(true, Ordering::SeqCst),
        }
        let new = sized(queued("new", 2 * DAY_MS), 256 * 1024);
        assert_eq!(
            tasks.create(&new.owner, new.spec.clone(), &new.source, new.created_at_ms),
            Err(TaskError::Storage),
            "{failure}",
        );
        assert_eq!(
            tasks.scheduling_snapshot().err(),
            Some(TaskError::Unavailable)
        );
        if failure == "index-after" {
            assert_eq!(index(&world)["format_version"], 5);
            assert_eq!(index(&world)["tasks"].as_array().unwrap().len(), 3);
        } else {
            assert_eq!(world.raw(), before);
        }
        let after_files = files(&world);
        assert_eq!(
            after_files.len(),
            if failure == "payload-before" { 2 } else { 3 }
        );
        for (name, hash) in old_files {
            assert_eq!(
                after_files.get(&name),
                Some(&hash),
                "提交未知时不回收旧文件"
            );
        }
        archive.fail.store(false, Ordering::SeqCst);
        archive.fail_after.store(false, Ordering::SeqCst);
        world.state.fail.store(false, Ordering::SeqCst);
        world.state.fail_after.store(false, Ordering::SeqCst);
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.cold(archive).await.unwrap();
        assert_eq!(
            tasks
                .create(&new.owner, new.spec.clone(), &new.source, new.created_at_ms)
                .unwrap(),
            new,
        );
        assert_eq!(tasks.scheduling_snapshot().unwrap().tasks.len(), 3);
        for task in &old {
            assert_eq!(
                tasks.get_owned(&task.owner, &task.id).unwrap().as_ref(),
                Some(task)
            );
        }
        assert_eq!(files(&world).len(), 3);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn v5_invalid_archival_fields_identity_and_oversized_index_preserve_original_files() {
    let world = World::new();
    let archive = world.archive();
    let history = cancelled("archived", 10);
    let active = queued("active", 20);
    seed(
        &world,
        &archive,
        &[history.clone(), active],
        5,
        &[true, false],
    );
    let (kernel, _) = world.cold(archive.clone()).await.unwrap();
    kernel.stop_all().await.unwrap();
    let good = index(&world);
    let file_state = files(&world);
    let writes = archive.writes.load(Ordering::SeqCst);
    for variant in 0..9 {
        let mut bad = good.clone();
        match variant {
            0 => {
                bad["tasks"][0].as_object_mut().unwrap().remove("archived");
            }
            1 => bad["tasks"][0]["archived"] = Value::Null,
            2 => bad["tasks"][0]["archived"] = "true".into(),
            3 => bad["tasks"][1]["archived"] = true.into(),
            4 => bad["format_version"] = 4.into(),
            5 => bad["tasks"][0]["record"]["bytes"] = 0.into(),
            6 => bad["tasks"][0]["directory"]["owner"]["session_id"] = "other".into(),
            7 => bad["tasks"][0]["record"]["extra"] = true.into(),
            _ => bad["format_version"] = 6.into(),
        }
        world.write(serde_json::to_vec(&bad).unwrap());
        let bad_bytes = world.raw();
        assert!(
            world.cold(archive.clone()).await.is_err(),
            "variant {variant}"
        );
        assert_eq!(world.raw(), bad_bytes);
        assert_eq!(archive.writes.load(Ordering::SeqCst), writes);
        assert_eq!(files(&world), file_state);
    }
    let mut oversized = serde_json::to_vec(&good).unwrap();
    oversized.resize(MAX_STATE_BYTES + 1, b' ');
    world.write(oversized);
    let before = world.raw();
    let reads = archive.reads.load(Ordering::SeqCst);
    assert!(world.cold(archive.clone()).await.is_err());
    assert_eq!(world.raw(), before);
    assert_eq!(archive.reads.load(Ordering::SeqCst), reads);
    assert_eq!(files(&world), file_state);
    world.write(serde_json::to_vec(&good).unwrap());
    let path = world.cold_path(&history.id);
    let body = std::fs::read(&path).unwrap();
    for missing in [false, true] {
        if missing {
            std::fs::remove_file(&path).unwrap();
        } else {
            std::fs::write(&path, b"changed").unwrap();
        }
        let before = world.raw();
        assert!(world.cold(archive.clone()).await.is_err());
        assert_eq!(world.raw(), before);
        assert_eq!(archive.writes.load(Ordering::SeqCst), writes);
        if !missing {
            assert_eq!(std::fs::read(&path).unwrap(), b"changed");
        }
        std::fs::write(&path, &body).unwrap();
    }
    let (kernel, tasks) = world.cold(archive).await.unwrap();
    assert_eq!(
        tasks.get_owned(&history.owner, &history.id).unwrap(),
        Some(history)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn archived_tasks_still_count_toward_the_original_daily_creation_limit() {
    let world = World::new();
    let archive = world.archive();
    let old: Vec<_> = (0..MAX_CREATED_PER_OWNER_PER_DAY)
        .map(|number| cancelled(&format!("daily-{number}"), 10))
        .collect();
    seed(&world, &archive, &old, 5, &vec![true; old.len()]);
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    let before = world.raw();
    let writes = archive.writes.load(Ordering::SeqCst);
    assert_eq!(
        tasks.create(&owner("alice"), spec("当日额满"), "over-day", 100),
        Err(TaskError::LimitReached)
    );
    assert_eq!(world.raw(), before);
    assert_eq!(archive.writes.load(Ordering::SeqCst), writes);
    assert_eq!(
        tasks
            .create(&old[0].owner, spec("来源仍幂等"), &old[0].source, 100)
            .unwrap(),
        old[0]
    );
    assert!(
        tasks
            .create(&owner("alice"), spec("下一日"), "next-day", DAY_MS + 10)
            .is_ok()
    );
    assert_eq!(
        tasks.scheduling_snapshot().unwrap().tasks.len(),
        old.len() + 1
    );
    kernel.stop_all().await.unwrap();
}

fn progress(task: &mut Task, count: usize, first: u64) {
    task.execution.as_mut().unwrap().progress_notices = (0..count)
        .map(|number| {
            let at = first + number as u64 * TASK_PROGRESS_INTERVAL_MS;
            TaskProgressNotice {
                sequence: number as u64 + 1,
                status: TaskStatus::Running,
                observed_finished_at_ms: None,
                attempt: DeliveryAttempt {
                    channel: DeliveryChannel::Proactive,
                    started_at_ms: at,
                    finished_at_ms: Some(at + 1),
                    result: Some(sent()),
                },
            }
        })
        .collect();
    // 回执发生于取消之前，保留真实可发生的执行与结束时间形状。
    let last = first + count.saturating_sub(1) as u64 * TASK_PROGRESS_INTERVAL_MS;
    task.started_at_ms = Some(task.created_at_ms + 1);
    task.cancel_requested = true;
    task.finished_at_ms = Some(last + 2);
    task.usage = Some(usage(0));
    task.execution.as_mut().unwrap().last_finished_at_ms = task.finished_at_ms;
}

#[tokio::test]
async fn archived_progress_receipts_keep_scope_budget_and_count_eviction_protection() {
    let world = World::new();
    let archive = world.archive();
    let now = 2 * DAY_MS;
    let mut first = cancelled("progress-first", 10);
    let mut second = cancelled("progress-second", 10);
    let count = MAX_TASK_PROGRESS_NOTICES / 2;
    let since = now - count as u64 * TASK_PROGRESS_INTERVAL_MS;
    progress(&mut first, count, since);
    progress(&mut second, count, since);
    let active = queued("active", 10);
    seed(
        &world,
        &archive,
        &[first.clone(), second, active.clone()],
        5,
        &[true, true, false],
    );
    let (kernel, tasks) = world.cold(archive.clone()).await.unwrap();
    assert!(
        !tasks
            .progress_snapshot()
            .unwrap()
            .scope_due(&first.owner, now)
    );
    let before = world.raw();
    assert_eq!(
        tasks.claim_progress(&active.owner, &active.id, DeliveryChannel::Proactive, now),
        Err(TaskError::InvalidTransition)
    );
    assert_eq!(world.raw(), before);
    kernel.stop_all().await.unwrap();

    let mut full: Vec<_> = (0..MAX_TASKS)
        .map(|number| cancelled(&format!("count-{number}"), 10 + number as u64))
        .collect();
    progress(&mut full[0], 1, now - TASK_PROGRESS_INTERVAL_MS);
    for (number, task) in full.iter_mut().enumerate().skip(2) {
        task.finished_at_ms = Some(now - TASK_PROGRESS_INTERVAL_MS + 10 + number as u64);
        task.execution.as_mut().unwrap().last_finished_at_ms = task.finished_at_ms;
    }
    full[1] = ended_exploration("unacknowledged", 10, false);
    let mut archived = vec![true; MAX_TASKS];
    archived[1] = false;
    seed(&world, &archive, &full, 5, &archived);
    let (kernel, tasks) = world.cold(archive).await.unwrap();
    let created = tasks
        .create(&owner("alice"), spec("原条数淘汰"), "new-at-limit", now)
        .unwrap();
    assert_eq!(tasks.scheduling_snapshot().unwrap().tasks.len(), MAX_TASKS);
    assert!(
        tasks
            .get_owned(&full[0].owner, &full[0].id)
            .unwrap()
            .is_some()
    );
    assert!(
        tasks
            .get_owned(&full[1].owner, &full[1].id)
            .unwrap()
            .is_some()
    );
    assert!(
        tasks
            .get_owned(&full[2].owner, &full[2].id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        tasks.get_owned(&created.owner, &created.id).unwrap(),
        Some(created)
    );
    kernel.stop_all().await.unwrap();
}
