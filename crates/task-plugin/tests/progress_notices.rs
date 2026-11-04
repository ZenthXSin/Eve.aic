use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use eve_task_api::*;
use eve_task_plugin::{FileTaskArchive, TaskController, TaskPlugin};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct Store {
    inner: FileStateStore,
    fail: AtomicBool,
    fail_after: AtomicBool,
}

impl StateStore for Store {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.inner.get(namespace, key)
    }

    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(PluginError::State("progress-commit-rejected".into()));
        }
        self.inner.set(namespace, key, value)?;
        if self.fail_after.load(Ordering::SeqCst) {
            return Err(PluginError::State("progress-commit-unconfirmed".into()));
        }
        Ok(())
    }
}

struct World {
    _directory: tempfile::TempDir,
    state: Arc<Store>,
    archive: Option<Arc<FileTaskArchive>>,
}

impl World {
    fn new(cold: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(Store {
            inner: FileStateStore::open(directory.path().join("state")).unwrap(),
            fail: AtomicBool::new(false),
            fail_after: AtomicBool::new(false),
        });
        let archive = cold
            .then(|| Arc::new(FileTaskArchive::open(directory.path().join("archive")).unwrap()));
        Self {
            _directory: directory,
            state,
            archive,
        }
    }

    async fn open(&self) -> PluginResult<(Kernel, TaskController)> {
        let kernel = Kernel::with_services(KernelServices {
            state: self.state.clone(),
            ..KernelServices::default()
        });
        let mut plugin = TaskPlugin::new()?;
        if let Some(archive) = &self.archive {
            plugin = plugin.with_archive(archive.clone());
        }
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

    fn fail_commit(&self, after: bool, fail: bool) {
        if after {
            self.state.fail_after.store(fail, Ordering::SeqCst);
        } else {
            self.state.fail.store(fail, Ordering::SeqCst);
        }
    }
}

fn owner(user: &str) -> TaskOwner {
    TaskOwner {
        session_id: format!("qq:session-{user}"),
        user_id: user.into(),
    }
}

fn spec() -> TaskSpec {
    TaskSpec {
        title: "后台核验".into(),
        instructions: "执行已授权的核验，保存实际结果".into(),
        max_steps: DEFAULT_STEPS,
        context: None,
    }
}

fn notices(task: &Task) -> &[TaskProgressNotice] {
    &task.execution.as_ref().unwrap().progress_notices
}

fn sent(id: &str) -> DeliveryResult {
    DeliveryResult::Sent {
        platform_message_id: Some(id.into()),
    }
}

fn is_due(tasks: &TaskController, id: &str, at: u64) -> bool {
    tasks
        .progress_snapshot()
        .unwrap()
        .due(at)
        .iter()
        .any(|entry| entry.id == id)
}

#[tokio::test]
async fn reviewed_progress_facts_are_compared_inside_the_claim_even_with_the_same_status_and_sequence()
 {
    for cold in [false, true] {
        let world = World::new(cold);
        let (kernel, tasks) = world.open().await.unwrap();
        let alice = owner("alice");
        let task = tasks
            .create(&alice, spec(), "reviewed-progress", 10)
            .unwrap();
        let queued = TaskProgressToken::from_task(&task).unwrap();
        tasks.start(&task.id, 20).unwrap();
        let mut checkpoint = eve_llm_api::TurnCheckpoint {
            version: 1,
            scope: eve_llm_api::ContextScope {
                session_id: alice.session_id.clone(),
                user_id: alice.user_id.clone(),
            },
            messages: vec![eve_llm_api::ChatMessage::text(
                eve_llm_api::ChatRole::User,
                "执行核验",
            )],
            pending_calls: vec![],
            used_tool_calls: 0,
            delegated_tool_calls: 0,
            delegated_model_requests: 0,
            provider_requests: 1,
            tool_batches: 0,
            remaining: Some("正在读取原始结果".into()),
            continuation_checks: 0,
            closing: false,
            result: None,
        };
        let first = tasks
            .checkpoint(&task.id, checkpoint.clone(), u64::MAX, 30)
            .unwrap();
        let old = TaskProgressToken::from_task(&first).unwrap();
        checkpoint.remaining = Some("已有结果，需要核对最新差异".into());
        let latest = tasks
            .checkpoint(&task.id, checkpoint, u64::MAX, 31)
            .unwrap();
        let expected = TaskProgressToken::from_task(&latest).unwrap();
        assert_eq!(old.next_sequence, expected.next_sequence);
        assert_ne!(old.facts_revision, expected.facts_revision);
        assert_eq!(first.status, latest.status);
        let at = task.created_at_ms + TASK_PROGRESS_INTERVAL_MS;
        let before = world.raw();
        for stale in [&queued, &old] {
            assert_eq!(
                tasks.claim_progress_expected(
                    &alice,
                    &task.id,
                    stale,
                    DeliveryChannel::Proactive,
                    at
                ),
                Err(TaskError::InvalidTransition)
            );
        }
        assert_eq!(world.raw(), before);
        assert!(notices(&tasks.get_owned(&alice, &task.id).unwrap().unwrap()).is_empty());
        let claimed = tasks
            .claim_progress_expected(&alice, &task.id, &expected, DeliveryChannel::Proactive, at)
            .unwrap();
        assert_eq!(notices(&claimed)[0].sequence, 1);
        assert_eq!(
            claimed.execution.as_ref().unwrap().checkpoint,
            latest.execution.as_ref().unwrap().checkpoint
        );
        let delivered = tasks
            .record_progress_delivery(&alice, &task.id, 1, sent("actual-receipt"), at)
            .unwrap();
        let current = TaskProgressToken::from_task(&delivered).unwrap();
        assert_eq!(current.next_sequence, 2);
        let mut stale_facts = expected;
        stale_facts.next_sequence = 2;
        let before = world.raw();
        assert_eq!(
            tasks.claim_progress_expected(
                &alice,
                &task.id,
                &stale_facts,
                DeliveryChannel::Proactive,
                at + TASK_PROGRESS_INTERVAL_MS
            ),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(world.raw(), before, "改序号不能让旧事实的判断重新生效");
        tasks.cancel(&alice.user_id, &task.id, at + 1).unwrap();
        let before = world.raw();
        assert_eq!(
            tasks.claim_progress_expected(
                &alice,
                &task.id,
                &current,
                DeliveryChannel::Proactive,
                at + TASK_PROGRESS_INTERVAL_MS
            ),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(world.raw(), before, "取消意图改变后必须重新决定内容");
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn unclaimed_progress_decision_survives_restart_without_consuming_a_notice() {
    for cold in [false, true] {
        let world = World::new(cold);
        let (kernel, tasks) = world.open().await.unwrap();
        let alice = owner("alice");
        let task = tasks
            .create(&alice, spec(), "unclaimed-candidate", 10)
            .unwrap();
        let before = world.raw();
        let expected = TaskProgressToken::from_task(&task).unwrap();
        let wire = serde_json::to_vec(&expected).unwrap();
        assert!(wire.len() < 128, "候选版本不复制任务上下文");
        assert_eq!(world.raw(), before);
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open().await.unwrap();
        let restored = tasks.get_owned(&alice, &task.id).unwrap().unwrap();
        assert_eq!(TaskProgressToken::from_task(&restored).unwrap(), expected);
        assert!(notices(&restored).is_empty());
        let at = task.created_at_ms + TASK_PROGRESS_INTERVAL_MS;
        tasks
            .claim_progress_expected(&alice, &task.id, &expected, DeliveryChannel::Proactive, at)
            .unwrap();
        let committed = world.raw();
        assert_eq!(
            tasks.claim_progress_expected(
                &alice,
                &task.id,
                &expected,
                DeliveryChannel::Proactive,
                at
            ),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(world.raw(), committed);
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open().await.unwrap();
        let restored = tasks.get_owned(&alice, &task.id).unwrap().unwrap();
        assert_eq!(notices(&restored).len(), 1);
        assert_eq!(
            notices(&restored)[0].attempt.result,
            Some(DeliveryResult::Unknown)
        );
        assert!(!is_due(&tasks, &task.id, at + DAY_MS));
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn thirty_minute_boundary_is_shared_by_scope_and_start_does_not_defer_it() {
    assert_eq!(TASK_PROGRESS_INTERVAL_MS, 30 * 60 * 1000);
    for cold in [false, true] {
        let world = World::new(cold);
        let (kernel, tasks) = world.open().await.unwrap();
        let alice = owner("alice");
        let other_session = TaskOwner {
            session_id: "qq:another-session".into(),
            ..alice.clone()
        };
        let bob = owner("bob");
        let first = tasks.create(&alice, spec(), "first", 10).unwrap();
        let second = tasks.create(&alice, spec(), "second", 10).unwrap();
        let other = tasks
            .create(&other_session, spec(), "other-session", 10)
            .unwrap();
        let independent = tasks.create(&bob, spec(), "bob", 10).unwrap();
        assert!(notices(&first).is_empty());
        let at = first.created_at_ms + TASK_PROGRESS_INTERVAL_MS;
        tasks.start(&first.id, at - 1).unwrap();
        assert!(tasks.progress_snapshot().unwrap().due(at - 1).is_empty());
        let before = world.raw();
        assert_eq!(
            tasks.claim_progress(&alice, &first.id, DeliveryChannel::Proactive, at - 1),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(world.raw(), before);

        let claimed = tasks
            .claim_progress(&alice, &first.id, DeliveryChannel::Proactive, at)
            .unwrap();
        assert_eq!(claimed.status, TaskStatus::Running);
        assert_eq!(notices(&claimed)[0].status, TaskStatus::Running);
        assert_eq!(notices(&claimed)[0].sequence, 1);
        assert_eq!(notices(&claimed)[0].attempt.started_at_ms, at);
        assert_eq!(notices(&claimed)[0].attempt.result, None);
        assert_eq!(notices(&claimed)[0].attempt.finished_at_ms, None);
        assert!(!is_due(&tasks, &first.id, at));
        assert!(!is_due(&tasks, &second.id, at));
        assert!(is_due(&tasks, &other.id, at));
        assert!(is_due(&tasks, &independent.id, at));
        assert_eq!(
            tasks.claim_progress(&alice, &second.id, DeliveryChannel::Proactive, at),
            Err(TaskError::InvalidTransition)
        );
        tasks
            .record_progress_delivery(&alice, &first.id, 1, sent("qq-first"), at)
            .unwrap();
        for (scope, task) in [(&other_session, &other), (&bob, &independent)] {
            tasks
                .claim_progress(scope, &task.id, DeliveryChannel::Proactive, at)
                .unwrap();
            tasks
                .record_progress_delivery(scope, &task.id, 1, sent("qq-independent"), at)
                .unwrap();
        }
        assert!(!is_due(
            &tasks,
            &second.id,
            at + TASK_PROGRESS_INTERVAL_MS - 1
        ));
        let claimed = tasks
            .claim_progress(
                &alice,
                &second.id,
                DeliveryChannel::Proactive,
                at + TASK_PROGRESS_INTERVAL_MS,
            )
            .unwrap();
        assert_eq!(claimed.status, TaskStatus::Queued);
        assert_eq!(notices(&claimed)[0].status, TaskStatus::Queued);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn receipts_require_the_full_owner_and_current_sequence_and_survive_restart() {
    for cold in [false, true] {
        let world = World::new(cold);
        let (kernel, tasks) = world.open().await.unwrap();
        let alice = owner("alice");
        let task = tasks.create(&alice, spec(), "receipt", 10).unwrap();
        let at = task.created_at_ms + TASK_PROGRESS_INTERVAL_MS;
        let wrong_session = TaskOwner {
            session_id: "qq:other-session".into(),
            ..alice.clone()
        };
        for wrong in [&wrong_session, &owner("bob")] {
            let before = world.raw();
            assert_eq!(
                tasks.claim_progress(wrong, &task.id, DeliveryChannel::Proactive, at),
                Err(TaskError::NotFound)
            );
            assert_eq!(world.raw(), before);
        }
        tasks
            .claim_progress(&alice, &task.id, DeliveryChannel::Proactive, at)
            .unwrap();
        let before = world.raw();
        for wrong in [&wrong_session, &owner("bob")] {
            assert_eq!(
                tasks.record_progress_delivery(wrong, &task.id, 1, sent("wrong-scope"), at),
                Err(TaskError::NotFound)
            );
        }
        assert_eq!(
            tasks.record_progress_delivery(&alice, &task.id, 0, sent("zero-sequence"), at),
            Err(TaskError::InvalidInput)
        );
        assert_eq!(
            tasks.record_progress_delivery(&alice, &task.id, 2, sent("stale"), at),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(world.raw(), before);
        let delivered = tasks
            .record_progress_delivery(&alice, &task.id, 1, sent("qq-platform-receipt-1"), at + 1)
            .unwrap();
        assert_eq!(
            notices(&delivered)[0].attempt.result,
            Some(sent("qq-platform-receipt-1"))
        );
        assert_eq!(notices(&delivered)[0].attempt.finished_at_ms, Some(at + 1));
        assert_eq!(delivered.status, TaskStatus::Queued);
        assert!(delivered.notice.is_none());
        let before = world.raw();
        assert_eq!(
            tasks.record_progress_delivery(&alice, &task.id, 1, sent("duplicate"), at + 2),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(world.raw(), before);
        kernel.stop_all().await.unwrap();

        let (kernel, tasks) = world.open().await.unwrap();
        let restored = tasks.get_owned(&alice, &task.id).unwrap().unwrap();
        assert_eq!(restored, delivered);
        let next = at + TASK_PROGRESS_INTERVAL_MS;
        let claimed = tasks
            .claim_progress(&alice, &task.id, DeliveryChannel::Proactive, next)
            .unwrap();
        assert_eq!(notices(&claimed).last().unwrap().sequence, 2);
        let before = world.raw();
        assert_eq!(
            tasks.record_progress_delivery(&alice, &task.id, 1, sent("late-first"), next),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(world.raw(), before);
        let failed = DeliveryResult::Failed {
            http_status: Some(503),
            biz_code: Some(12345),
        };
        let recorded = tasks
            .record_progress_delivery(&alice, &task.id, 2, failed.clone(), next)
            .unwrap();
        assert_eq!(
            notices(&recorded).last().unwrap().attempt.result,
            Some(failed)
        );
        assert!(!is_due(&tasks, &task.id, next));
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn a_claim_without_a_receipt_becomes_unknown_after_restart_and_is_never_resent() {
    for cold in [false, true] {
        let world = World::new(cold);
        let (kernel, tasks) = world.open().await.unwrap();
        let alice = owner("alice");
        let task = tasks.create(&alice, spec(), "unknown", 10).unwrap();
        let sibling = tasks.create(&alice, spec(), "sibling", 10).unwrap();
        let at = task.created_at_ms + TASK_PROGRESS_INTERVAL_MS;
        tasks
            .claim_progress(&alice, &task.id, DeliveryChannel::Proactive, at)
            .unwrap();
        kernel.stop_all().await.unwrap();

        let (kernel, tasks) = world.open().await.unwrap();
        let restored = tasks.get_owned(&alice, &task.id).unwrap().unwrap();
        assert_eq!(restored.status, TaskStatus::Queued);
        assert_eq!(notices(&restored).len(), 1);
        assert_eq!(notices(&restored)[0].sequence, 1);
        assert_eq!(
            notices(&restored)[0].attempt.result,
            Some(DeliveryResult::Unknown)
        );
        let later = at + 2 * DAY_MS;
        assert!(!is_due(&tasks, &task.id, later));
        assert!(is_due(&tasks, &sibling.id, later));
        let before = world.raw();
        assert_eq!(
            tasks.claim_progress(&alice, &task.id, DeliveryChannel::Proactive, later),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(
            tasks.record_progress_delivery(&alice, &task.id, 1, sent("late-receipt"), later),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(world.raw(), before);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn progress_preserves_actual_queued_running_and_blocked_states() {
    for cold in [false, true] {
        let world = World::new(cold);
        let (kernel, tasks) = world.open().await.unwrap();
        for (user, status) in [
            ("queued", TaskStatus::Queued),
            ("running", TaskStatus::Running),
            ("blocked", TaskStatus::Blocked),
        ] {
            let scope = owner(user);
            let mut baseline = tasks.create(&scope, spec(), user, 10).unwrap();
            if status != TaskStatus::Queued {
                baseline = tasks.start(&baseline.id, 11).unwrap();
            }
            if status == TaskStatus::Blocked {
                baseline = tasks
                    .finish(
                        &baseline.id,
                        TaskEnd::Blocked {
                            result: "缺少目标环境；已完成配置核对，等待真实授权证据".into(),
                        },
                        TaskUsage {
                            tool_calls: 2,
                            model_requests: 3,
                        },
                        12,
                    )
                    .unwrap();
            }
            let at = baseline.created_at_ms + TASK_PROGRESS_INTERVAL_MS;
            assert!(is_due(&tasks, &baseline.id, at));
            let claimed = tasks
                .claim_progress(&scope, &baseline.id, DeliveryChannel::Proactive, at)
                .unwrap();
            assert_eq!(notices(&claimed)[0].status, status);
            let mut without_progress = claimed.clone();
            without_progress
                .execution
                .as_mut()
                .unwrap()
                .progress_notices
                .clear();
            assert_eq!(without_progress, baseline);
            let mut delivered = tasks
                .record_progress_delivery(&scope, &baseline.id, 1, sent(user), at)
                .unwrap();
            delivered
                .execution
                .as_mut()
                .unwrap()
                .progress_notices
                .clear();
            assert_eq!(delivered, baseline);
            if status == TaskStatus::Blocked {
                assert!(!is_due(
                    &tasks,
                    &baseline.id,
                    at + TASK_PROGRESS_INTERVAL_MS
                ));
                assert_eq!(
                    tasks.claim_progress(
                        &scope,
                        &baseline.id,
                        DeliveryChannel::Proactive,
                        at + TASK_PROGRESS_INTERVAL_MS,
                    ),
                    Err(TaskError::InvalidTransition)
                );
            }
        }
        for (user, end) in [
            (
                "completed",
                TaskEnd::Completed {
                    result: "已核验完成".into(),
                },
            ),
            (
                "failed",
                TaskEnd::Failed {
                    reason: "后端拒绝执行".into(),
                },
            ),
        ] {
            let scope = owner(user);
            let task = tasks.create(&scope, spec(), user, 10).unwrap();
            tasks.start(&task.id, 11).unwrap();
            tasks
                .finish(&task.id, end, TaskUsage::default(), 12)
                .unwrap();
            let at = task.created_at_ms + TASK_PROGRESS_INTERVAL_MS;
            assert!(!is_due(&tasks, &task.id, at));
            assert_eq!(
                tasks.claim_progress(&scope, &task.id, DeliveryChannel::Proactive, at),
                Err(TaskError::InvalidTransition)
            );
        }
        let idle = tasks
            .create(
                &owner("idle"),
                spec(),
                &format!("{IDLE_TASK_SOURCE_PREFIX}review"),
                10,
            )
            .unwrap();
        assert!(!is_due(&tasks, &idle.id, 10 + TASK_PROGRESS_INTERVAL_MS));
        assert_eq!(
            tasks.claim_progress(
                &idle.owner,
                &idle.id,
                DeliveryChannel::Proactive,
                10 + TASK_PROGRESS_INTERVAL_MS
            ),
            Err(TaskError::InvalidTransition)
        );
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn task_completion_preserves_and_settles_an_inflight_progress_receipt() {
    for cold in [false, true] {
        let world = World::new(cold);
        let (kernel, tasks) = world.open().await.unwrap();
        let alice = owner("alice");
        let task = tasks.create(&alice, spec(), "completion-race", 10).unwrap();
        let at = task.created_at_ms + TASK_PROGRESS_INTERVAL_MS;
        tasks
            .claim_progress(&alice, &task.id, DeliveryChannel::Proactive, at)
            .unwrap();
        tasks.start(&task.id, at + 1).unwrap();
        let completed = tasks
            .finish(
                &task.id,
                TaskEnd::Completed {
                    result: "实际验收已经通过".into(),
                },
                TaskUsage {
                    tool_calls: 1,
                    model_requests: 2,
                },
                at + 2,
            )
            .unwrap();
        assert!(!is_due(&tasks, &task.id, at + TASK_PROGRESS_INTERVAL_MS));
        let recorded = tasks
            .record_progress_delivery(&alice, &task.id, 1, sent("qq-racing-receipt"), at + 3)
            .unwrap();
        assert_eq!(recorded.status, TaskStatus::Completed);
        assert_eq!(recorded.result, completed.result);
        assert_eq!(recorded.usage, completed.usage);
        assert_eq!(recorded.notice, completed.notice);
        assert!(recorded.notice_pending());
        assert_eq!(notices(&recorded)[0].status, TaskStatus::Queued);
        assert_eq!(
            notices(&recorded)[0].attempt.result,
            Some(sent("qq-racing-receipt"))
        );
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn retention_is_bounded_and_sequences_do_not_reset_when_old_receipts_are_removed() {
    assert_eq!(MAX_TASK_PROGRESS_NOTICES, 48);
    for cold in [false, true] {
        let world = World::new(cold);
        let (kernel, tasks) = world.open().await.unwrap();
        let alice = owner("alice");
        let task = tasks.create(&alice, spec(), "retention", 10).unwrap();
        for sequence in 1..=50 {
            let at = task.created_at_ms + sequence * TASK_PROGRESS_INTERVAL_MS;
            let claimed = tasks
                .claim_progress(&alice, &task.id, DeliveryChannel::Proactive, at)
                .unwrap();
            assert_eq!(notices(&claimed).last().unwrap().sequence, sequence);
            assert!(notices(&claimed).len() <= MAX_TASK_PROGRESS_NOTICES);
            tasks
                .record_progress_delivery(
                    &alice,
                    &task.id,
                    sequence,
                    sent(&format!("qq-{sequence}")),
                    at,
                )
                .unwrap();
            assert!(!is_due(
                &tasks,
                &task.id,
                at + TASK_PROGRESS_INTERVAL_MS - 1
            ));
        }
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open().await.unwrap();
        let restored = tasks.get_owned(&alice, &task.id).unwrap().unwrap();
        assert_eq!(notices(&restored).len(), MAX_TASK_PROGRESS_NOTICES);
        assert_eq!(notices(&restored).first().unwrap().sequence, 3);
        assert_eq!(notices(&restored).last().unwrap().sequence, 50);
        assert_eq!(
            notices(&restored).last().unwrap().attempt.result,
            Some(sent("qq-50"))
        );
        let claimed = tasks
            .claim_progress(
                &alice,
                &task.id,
                DeliveryChannel::Proactive,
                task.created_at_ms + 51 * TASK_PROGRESS_INTERVAL_MS,
            )
            .unwrap();
        assert_eq!(notices(&claimed).last().unwrap().sequence, 51);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn failed_or_unconfirmed_claim_commits_never_authorize_a_send() {
    for cold in [false, true] {
        for after in [false, true] {
            let world = World::new(cold);
            let (kernel, tasks) = world.open().await.unwrap();
            let alice = owner("alice");
            let task = tasks.create(&alice, spec(), "claim-failure", 10).unwrap();
            let expected = TaskProgressToken::from_task(&task).unwrap();
            let at = task.created_at_ms + TASK_PROGRESS_INTERVAL_MS;
            let before = world.raw();
            world.fail_commit(after, true);
            assert_eq!(
                tasks.claim_progress_expected(
                    &alice,
                    &task.id,
                    &expected,
                    DeliveryChannel::Proactive,
                    at
                ),
                Err(TaskError::Storage)
            );
            world.fail_commit(after, false);
            if !after {
                assert_eq!(world.raw(), before);
            }
            assert_eq!(
                tasks.progress_snapshot().unwrap_err(),
                TaskError::Unavailable
            );
            assert_eq!(
                tasks.record_progress_delivery(&alice, &task.id, 1, sent("must-not-send"), at),
                Err(TaskError::Unavailable)
            );
            kernel.stop_all().await.unwrap();
            let (kernel, tasks) = world.open().await.unwrap();
            let restored = tasks.get_owned(&alice, &task.id).unwrap().unwrap();
            if after {
                assert_eq!(
                    notices(&restored)[0].attempt.result,
                    Some(DeliveryResult::Unknown)
                );
                assert!(!is_due(&tasks, &task.id, at + DAY_MS));
            } else {
                assert!(notices(&restored).is_empty());
                assert!(is_due(&tasks, &task.id, at));
            }
            kernel.stop_all().await.unwrap();
        }
    }
}

#[tokio::test]
async fn unconfirmed_receipt_commits_are_reconciled_from_storage_without_fabricating_success() {
    for cold in [false, true] {
        for after in [false, true] {
            let world = World::new(cold);
            let (kernel, tasks) = world.open().await.unwrap();
            let alice = owner("alice");
            let task = tasks.create(&alice, spec(), "receipt-failure", 10).unwrap();
            let at = task.created_at_ms + TASK_PROGRESS_INTERVAL_MS;
            tasks
                .claim_progress(&alice, &task.id, DeliveryChannel::Proactive, at)
                .unwrap();
            world.fail_commit(after, true);
            assert_eq!(
                tasks.record_progress_delivery(
                    &alice,
                    &task.id,
                    1,
                    sent("qq-confirmed-by-platform"),
                    at
                ),
                Err(TaskError::Storage)
            );
            world.fail_commit(after, false);
            assert_eq!(
                tasks.progress_snapshot().unwrap_err(),
                TaskError::Unavailable
            );
            kernel.stop_all().await.unwrap();
            let (kernel, tasks) = world.open().await.unwrap();
            let restored = tasks.get_owned(&alice, &task.id).unwrap().unwrap();
            assert_eq!(notices(&restored).len(), 1);
            let expected = if after {
                sent("qq-confirmed-by-platform")
            } else {
                DeliveryResult::Unknown
            };
            assert_eq!(notices(&restored)[0].attempt.result, Some(expected));
            assert_eq!(restored.status, TaskStatus::Queued);
            assert_eq!(
                tasks.record_progress_delivery(&alice, &task.id, 1, sent("second-receipt"), at + 1),
                Err(TaskError::InvalidTransition)
            );
            kernel.stop_all().await.unwrap();
        }
    }
}

#[tokio::test]
async fn local_progress_receipts_are_atomic_idempotent_and_distinct_from_platform_delivery() {
    for cold in [false, true] {
        let world = World::new(cold);
        let (kernel, tasks) = world.open().await.unwrap();
        let alice = owner("alice");
        let task = tasks.create(&alice, spec(), "local-receipt", 10).unwrap();
        let at = task.created_at_ms + TASK_PROGRESS_INTERVAL_MS;
        let wrong_session = TaskOwner {
            session_id: "web:another-session".into(),
            ..alice.clone()
        };
        let before = world.raw();
        assert_eq!(
            tasks.record_local_progress(&wrong_session, &task.id, 1, "local-event-1", at),
            Err(TaskError::NotFound)
        );
        assert_eq!(
            tasks.record_local_progress(&alice, &task.id, 2, "local-event-2", at),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(
            tasks.record_local_progress(&alice, &task.id, 1, "local-event-1", at - 1),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(world.raw(), before);
        let recorded = tasks
            .record_local_progress(&alice, &task.id, 1, "local-event-1", at)
            .unwrap();
        assert_eq!(notices(&recorded).len(), 1);
        assert_eq!(notices(&recorded)[0].sequence, 1);
        assert_eq!(
            notices(&recorded)[0].attempt.channel,
            DeliveryChannel::LocalInbox
        );
        assert_eq!(notices(&recorded)[0].attempt.started_at_ms, at);
        assert_eq!(notices(&recorded)[0].attempt.finished_at_ms, Some(at));
        assert_eq!(
            notices(&recorded)[0].attempt.result,
            Some(DeliveryResult::StoredLocally {
                notification_id: "local-event-1".into()
            })
        );
        assert_eq!(recorded.status, TaskStatus::Queued);
        assert!(recorded.notice.is_none());
        assert_eq!(
            tasks
                .record_local_progress(&alice, &task.id, 1, "local-event-1", at)
                .unwrap(),
            recorded
        );
        let before = world.raw();
        assert_eq!(
            tasks.record_local_progress(&alice, &task.id, 1, "different-event", at),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(world.raw(), before);
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open().await.unwrap();
        assert_eq!(
            tasks.get_owned(&alice, &task.id).unwrap().unwrap(),
            recorded
        );
        assert_eq!(
            tasks
                .record_local_progress(&alice, &task.id, 1, "local-event-1", at)
                .unwrap(),
            recorded
        );
        assert!(!is_due(
            &tasks,
            &task.id,
            at + TASK_PROGRESS_INTERVAL_MS - 1
        ));
        assert!(is_due(&tasks, &task.id, at + TASK_PROGRESS_INTERVAL_MS));
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn local_receipt_commit_failures_can_reconcile_the_same_durable_event_without_duplication() {
    for cold in [false, true] {
        for after in [false, true] {
            let world = World::new(cold);
            let (kernel, tasks) = world.open().await.unwrap();
            let alice = owner("alice");
            let task = tasks
                .create(&alice, spec(), "local-commit-failure", 10)
                .unwrap();
            let at = task.created_at_ms + TASK_PROGRESS_INTERVAL_MS;
            world.fail_commit(after, true);
            assert_eq!(
                tasks.record_local_progress(&alice, &task.id, 1, "durable-local-event", at),
                Err(TaskError::Storage)
            );
            world.fail_commit(after, false);
            assert_eq!(
                tasks.progress_snapshot().unwrap_err(),
                TaskError::Unavailable
            );
            kernel.stop_all().await.unwrap();
            let (kernel, tasks) = world.open().await.unwrap();
            let restored = tasks.get_owned(&alice, &task.id).unwrap().unwrap();
            assert_eq!(notices(&restored).len(), usize::from(after));
            let recorded = tasks
                .record_local_progress(&alice, &task.id, 1, "durable-local-event", at)
                .unwrap();
            assert_eq!(notices(&recorded).len(), 1);
            assert_eq!(notices(&recorded)[0].sequence, 1);
            assert_eq!(
                notices(&recorded)[0].attempt.result,
                Some(DeliveryResult::StoredLocally {
                    notification_id: "durable-local-event".into()
                })
            );
            assert_eq!(recorded.status, TaskStatus::Queued);
            kernel.stop_all().await.unwrap();
        }
    }
}

#[tokio::test]
async fn progress_requires_an_actual_platform_receipt_and_rejects_other_delivery_channels() {
    for cold in [false, true] {
        let world = World::new(cold);
        let (kernel, tasks) = world.open().await.unwrap();
        let alice = owner("alice");
        let task = tasks
            .create(&alice, spec(), "receipt-evidence", 10)
            .unwrap();
        let at = task.created_at_ms + TASK_PROGRESS_INTERVAL_MS;
        let before = world.raw();
        for channel in [
            DeliveryChannel::Passive {
                message_id: "incoming-1".into(),
            },
            DeliveryChannel::LocalInbox,
        ] {
            assert_eq!(
                tasks.claim_progress(&alice, &task.id, channel, at),
                Err(TaskError::InvalidInput)
            );
        }
        assert_eq!(world.raw(), before);
        tasks
            .claim_progress(&alice, &task.id, DeliveryChannel::Proactive, at)
            .unwrap();
        let before = world.raw();
        for result in [
            DeliveryResult::Sent {
                platform_message_id: Some(String::new()),
            },
            DeliveryResult::StoredLocally {
                notification_id: "local-event-1".into(),
            },
        ] {
            assert_eq!(
                tasks.record_progress_delivery(&alice, &task.id, 1, result, at),
                Err(TaskError::InvalidInput)
            );
        }
        assert_eq!(world.raw(), before);
        let recorded = tasks
            .record_progress_delivery(&alice, &task.id, 1, DeliveryResult::NotSent, at)
            .unwrap();
        assert_eq!(
            notices(&recorded)[0].attempt.result,
            Some(DeliveryResult::NotSent)
        );
        assert!(!is_due(&tasks, &task.id, at));
        assert!(is_due(&tasks, &task.id, at + TASK_PROGRESS_INTERVAL_MS));
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn uncertain_platform_outcomes_freeze_progress_immediately_and_after_restart() {
    for cold in [false, true] {
        for result in [
            DeliveryResult::Unknown,
            DeliveryResult::Sent {
                platform_message_id: None,
            },
        ] {
            let world = World::new(cold);
            let (kernel, tasks) = world.open().await.unwrap();
            let alice = owner("alice");
            let task = tasks
                .create(&alice, spec(), "uncertain-platform", 10)
                .unwrap();
            let at = task.created_at_ms + TASK_PROGRESS_INTERVAL_MS;
            tasks
                .claim_progress(&alice, &task.id, DeliveryChannel::Proactive, at)
                .unwrap();
            let recorded = tasks
                .record_progress_delivery(&alice, &task.id, 1, result, at + 1)
                .unwrap();
            assert_eq!(
                notices(&recorded)[0].attempt.result,
                Some(DeliveryResult::Unknown)
            );
            assert_eq!(recorded.status, TaskStatus::Queued);
            let later = at + 2 * DAY_MS;
            assert!(!is_due(&tasks, &task.id, later));
            assert_eq!(
                tasks.claim_progress(&alice, &task.id, DeliveryChannel::Proactive, later),
                Err(TaskError::InvalidTransition)
            );
            assert_eq!(
                tasks.record_progress_delivery(&alice, &task.id, 1, sent("late"), later),
                Err(TaskError::InvalidTransition)
            );
            kernel.stop_all().await.unwrap();
            let (kernel, tasks) = world.open().await.unwrap();
            assert_eq!(
                tasks.get_owned(&alice, &task.id).unwrap().unwrap(),
                recorded
            );
            assert!(!is_due(&tasks, &task.id, later));
            assert_eq!(
                tasks.claim_progress(&alice, &task.id, DeliveryChannel::Proactive, later),
                Err(TaskError::InvalidTransition)
            );
            kernel.stop_all().await.unwrap();
        }
    }
}

#[tokio::test]
async fn legacy_tasks_without_execution_are_not_silently_enrolled_for_progress() {
    for cold in [false, true] {
        let mut world = World::new(false);
        let (kernel, tasks) = world.open().await.unwrap();
        let alice = owner("alice");
        let legacy = tasks.create(&alice, spec(), "legacy", 10).unwrap();
        let modern = tasks.create(&owner("bob"), spec(), "modern", 10).unwrap();
        kernel.stop_all().await.unwrap();
        let mut state: serde_json::Value = serde_json::from_slice(&world.raw().unwrap()).unwrap();
        for task in state["tasks"].as_array_mut().unwrap() {
            if task["id"].as_str() == Some(legacy.id.as_str()) {
                task.as_object_mut().unwrap().remove("execution");
            } else {
                task["execution"]
                    .as_object_mut()
                    .unwrap()
                    .remove("progress_notices");
            }
        }
        world
            .state
            .inner
            .set(
                &PluginId::new(TASK_PLUGIN_ID).unwrap(),
                TASK_STATE_KEY.into(),
                serde_json::to_vec(&state).unwrap(),
            )
            .unwrap();
        if cold {
            world.archive = Some(Arc::new(
                FileTaskArchive::open(world._directory.path().join("archive")).unwrap(),
            ));
        }
        let (kernel, tasks) = world.open().await.unwrap();
        let at = 10 + TASK_PROGRESS_INTERVAL_MS;
        assert!(
            tasks
                .get_owned(&alice, &legacy.id)
                .unwrap()
                .unwrap()
                .execution
                .is_none()
        );
        assert!(!is_due(&tasks, &legacy.id, at));
        assert_eq!(
            tasks.claim_progress(&alice, &legacy.id, DeliveryChannel::Proactive, at),
            Err(TaskError::InvalidTransition)
        );
        assert!(is_due(&tasks, &modern.id, at));
        assert!(notices(&tasks.get_owned(&modern.owner, &modern.id).unwrap().unwrap()).is_empty());
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn corrupt_progress_receipts_fail_closed_without_erasing_the_persisted_evidence() {
    use serde_json::{Value, json};

    let world = World::new(false);
    let (kernel, tasks) = world.open().await.unwrap();
    let alice = owner("alice");
    let task = tasks
        .create(&alice, spec(), "corrupt-progress", 10)
        .unwrap();
    let at = task.created_at_ms + TASK_PROGRESS_INTERVAL_MS;
    for sequence in 1..=2 {
        let started = task.created_at_ms + sequence * TASK_PROGRESS_INTERVAL_MS;
        tasks
            .claim_progress(&alice, &task.id, DeliveryChannel::Proactive, started)
            .unwrap();
        tasks
            .record_progress_delivery(&alice, &task.id, sequence, sent("qq-receipt"), started)
            .unwrap();
    }
    kernel.stop_all().await.unwrap();
    let baseline: Value = serde_json::from_slice(&world.raw().unwrap()).unwrap();
    let valid = baseline["tasks"][0]["execution"]["progress_notices"].clone();
    let mut cases: Vec<(&str, Value)> = Vec::new();

    for (name, channel, result) in [
        (
            "local inbox cannot claim platform delivery",
            json!(DeliveryChannel::LocalInbox),
            json!(sent("forged-platform-receipt")),
        ),
        (
            "local inbox must be durably settled",
            json!(DeliveryChannel::LocalInbox),
            Value::Null,
        ),
        (
            "local inbox cannot have an unknown external outcome",
            json!(DeliveryChannel::LocalInbox),
            json!(DeliveryResult::Unknown),
        ),
        (
            "local inbox cannot contain a platform rejection",
            json!(DeliveryChannel::LocalInbox),
            json!(DeliveryResult::Failed {
                http_status: Some(503),
                biz_code: None
            }),
        ),
        (
            "proactive delivery cannot borrow a local receipt",
            json!(DeliveryChannel::Proactive),
            json!(DeliveryResult::StoredLocally {
                notification_id: "local-event".into()
            }),
        ),
        (
            "passive delivery cannot bypass progress admission",
            json!(DeliveryChannel::Passive {
                message_id: "incoming-1".into()
            }),
            json!(sent("qq-receipt")),
        ),
        (
            "a persisted sent outcome needs a platform receipt",
            json!(DeliveryChannel::Proactive),
            json!(DeliveryResult::Sent {
                platform_message_id: None
            }),
        ),
        (
            "platform receipt identifiers cannot be empty",
            json!(DeliveryChannel::Proactive),
            json!(DeliveryResult::Sent {
                platform_message_id: Some(String::new())
            }),
        ),
        (
            "local receipt identifiers cannot be empty",
            json!(DeliveryChannel::LocalInbox),
            json!(DeliveryResult::StoredLocally {
                notification_id: String::new()
            }),
        ),
    ] {
        let mut malformed = valid.clone();
        malformed[1]["attempt"]["channel"] = channel;
        malformed[1]["attempt"]["result"] = result;
        if malformed[1]["attempt"]["result"].is_null() {
            malformed[1]["attempt"]["finished_at_ms"] = Value::Null;
        }
        cases.push((name, malformed));
    }

    for (name, sequence) in [
        ("zero sequence", 0),
        ("duplicate sequence", 1),
        ("skipped sequence", 3),
    ] {
        let mut malformed = valid.clone();
        malformed[1]["sequence"] = json!(sequence);
        cases.push((name, malformed));
    }

    let mut too_many = Vec::new();
    for sequence in 1..=(MAX_TASK_PROGRESS_NOTICES as u64 + 1) {
        let mut notice = valid[0].clone();
        let started = task.created_at_ms + sequence * TASK_PROGRESS_INTERVAL_MS;
        notice["sequence"] = json!(sequence);
        notice["attempt"]["started_at_ms"] = json!(started);
        notice["attempt"]["finished_at_ms"] = json!(started);
        too_many.push(notice);
    }
    cases.push(("retained history exceeds its bound", json!(too_many)));

    let mut too_early = valid.clone();
    too_early[0]["attempt"]["started_at_ms"] = json!(at - 1);
    cases.push(("first progress precedes thirty minutes", too_early));
    let mut too_close = valid.clone();
    too_close[1]["attempt"]["started_at_ms"] = json!(at + TASK_PROGRESS_INTERVAL_MS - 1);
    cases.push(("adjacent notices bypass their interval", too_close));
    let mut backwards = valid.clone();
    backwards[1]["attempt"]["finished_at_ms"] = json!(at);
    cases.push(("receipt predates its claim", backwards));
    let mut missing_finish = valid.clone();
    missing_finish[1]["attempt"]["finished_at_ms"] = Value::Null;
    cases.push(("confirmed receipt lacks a finish time", missing_finish));
    let mut unclaimed_result = valid.clone();
    unclaimed_result[1]["attempt"]["result"] = Value::Null;
    cases.push(("unfinished attempt carries a finish time", unclaimed_result));
    let mut unresolved_prior = valid.clone();
    unresolved_prior[0]["attempt"]["result"] = Value::Null;
    unresolved_prior[0]["attempt"]["finished_at_ms"] = Value::Null;
    cases.push(("earlier attempt is unresolved", unresolved_prior));
    let mut unknown_prior = valid.clone();
    unknown_prior[0]["attempt"]["result"] = json!(DeliveryResult::Unknown);
    unknown_prior[0]["attempt"]["finished_at_ms"] = Value::Null;
    cases.push(("progress continued after an unknown attempt", unknown_prior));

    for (reason, malformed) in cases {
        let mut corrupted = baseline.clone();
        corrupted["tasks"][0]["execution"]["progress_notices"] = malformed;
        let bytes = serde_json::to_vec(&corrupted).unwrap();
        world.write(bytes.clone());
        match world.open().await {
            Err(_) => {}
            Ok((kernel, _)) => {
                kernel.stop_all().await.unwrap();
                panic!("accepted corrupt progress: {reason}");
            }
        }
        assert_eq!(
            world.raw(),
            Some(bytes),
            "erased corrupt progress: {reason}"
        );
    }
}

#[tokio::test]
async fn concurrent_reconciliation_of_one_local_event_succeeds_without_duplicate_notices() {
    for cold in [false, true] {
        let world = World::new(cold);
        let (kernel, tasks) = world.open().await.unwrap();
        let alice = owner("alice");
        let task = tasks
            .create(&alice, spec(), "concurrent-local", 10)
            .unwrap();
        for sequence in 1..=8 {
            let at = task.created_at_ms + sequence * TASK_PROGRESS_INTERVAL_MS;
            let receipt = format!("local-concurrent-{sequence}");
            let barrier = std::sync::Barrier::new(3);
            std::thread::scope(|scope| {
                let reconcile = || {
                    barrier.wait();
                    tasks.record_local_progress(&alice, &task.id, sequence, &receipt, at)
                };
                let first = scope.spawn(reconcile);
                let second = scope.spawn(reconcile);
                barrier.wait();
                let first = first.join().unwrap().unwrap();
                let second = second.join().unwrap().unwrap();
                assert_eq!(first, second);
                assert_eq!(notices(&first).len(), sequence as usize);
                assert_eq!(notices(&first).last().unwrap().sequence, sequence);
                assert_eq!(
                    notices(&first).last().unwrap().attempt.result,
                    Some(DeliveryResult::StoredLocally {
                        notification_id: receipt.clone()
                    })
                );
            });
        }
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = world.open().await.unwrap();
        let restored = tasks.get_owned(&alice, &task.id).unwrap().unwrap();
        assert_eq!(notices(&restored).len(), 8);
        assert_eq!(notices(&restored).last().unwrap().sequence, 8);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn overflowing_progress_deadlines_never_become_due_or_allow_repeated_claims() {
    for cold in [false, true] {
        let world = World::new(cold);
        let (kernel, tasks) = world.open().await.unwrap();
        let overflow_owner = owner("overflow");
        let overflow = tasks
            .create(&overflow_owner, spec(), "overflow-created", u64::MAX)
            .unwrap();
        assert!(!is_due(&tasks, &overflow.id, u64::MAX));
        let before = world.raw();
        assert_eq!(
            tasks.claim_progress(
                &overflow_owner,
                &overflow.id,
                DeliveryChannel::Proactive,
                u64::MAX,
            ),
            Err(TaskError::InvalidTransition)
        );
        assert_eq!(world.raw(), before);

        let alice = owner("alice");
        let created = u64::MAX - TASK_PROGRESS_INTERVAL_MS;
        let task = tasks
            .create(&alice, spec(), "last-representable-claim", created)
            .unwrap();
        let sibling = tasks
            .create(&alice, spec(), "same-scope-overflow", created)
            .unwrap();
        assert!(!is_due(&tasks, &task.id, u64::MAX - 1));
        assert!(is_due(&tasks, &task.id, u64::MAX));
        tasks
            .claim_progress(&alice, &task.id, DeliveryChannel::Proactive, u64::MAX)
            .unwrap();
        let recorded = tasks
            .record_progress_delivery(&alice, &task.id, 1, sent("qq-last-time"), u64::MAX)
            .unwrap();
        assert_eq!(notices(&recorded).len(), 1);
        assert_eq!(notices(&recorded)[0].attempt.finished_at_ms, Some(u64::MAX));
        assert!(!is_due(&tasks, &task.id, u64::MAX));
        assert!(!is_due(&tasks, &sibling.id, u64::MAX));
        let before = world.raw();
        for id in [&task.id, &sibling.id] {
            assert_eq!(
                tasks.claim_progress(&alice, id, DeliveryChannel::Proactive, u64::MAX),
                Err(TaskError::InvalidTransition)
            );
        }
        assert_eq!(world.raw(), before);
        kernel.stop_all().await.unwrap();
    }
}
