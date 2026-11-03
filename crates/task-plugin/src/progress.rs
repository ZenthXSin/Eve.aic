//! 进度通知与终态通知分账；身份、时间、序号、预算在同一任务账本锁内核对。
use crate::store::{Ledger, StoredTasks};
use eve_task_api::*;

fn scope_guard(ledger: &Ledger, owner: &TaskOwner, now_ms: u64) -> TaskResult<()> {
    let snapshot = TaskProgressSnapshot {
        tasks: ledger.tasks.iter().map(TaskProgressEntry::from).collect(),
    };
    if !snapshot.scope_due(owner, now_ms) {
        return Err(TaskError::InvalidTransition);
    }
    Ok(())
}

fn same_local(task: &Task, sequence: u64, notification_id: &str) -> bool {
    task.execution
        .as_ref()
        .and_then(|e| e.progress_notices.iter().find(|n| n.sequence == sequence))
        .is_some_and(|notice| {
            notice.attempt.channel == DeliveryChannel::LocalInbox
                && notice.attempt.result
                    == Some(DeliveryResult::StoredLocally {
                        notification_id: notification_id.into(),
                    })
        })
}

fn append(task: &mut Task, sequence: u64, attempt: DeliveryAttempt) -> TaskResult<()> {
    let status = task.status;
    let observed_finished_at_ms = task.finished_at_ms;
    let notices = &mut task
        .execution
        .as_mut()
        .ok_or(TaskError::InvalidTransition)?
        .progress_notices;
    if notices.len() == MAX_TASK_PROGRESS_NOTICES {
        notices.remove(0);
    }
    notices.push(TaskProgressNotice {
        sequence,
        status,
        observed_finished_at_ms,
        attempt,
    });
    Ok(())
}

impl StoredTasks {
    pub(super) fn claim_progress(
        &self,
        owner: &TaskOwner,
        id: &str,
        channel: DeliveryChannel,
        now_ms: u64,
    ) -> TaskResult<Task> {
        validate_owner(owner)?;
        validate_id(id)?;
        // 本地收件箱使用原子已存回执；被动消息不能绕过进度提醒间隔。
        if channel != DeliveryChannel::Proactive {
            return Err(TaskError::InvalidInput);
        }
        self.update_guarded(
            id,
            now_ms,
            |ledger| {
                if !ledger
                    .tasks
                    .iter()
                    .any(|task| task.id == id && &task.owner == owner)
                {
                    return Err(TaskError::NotFound);
                }
                scope_guard(ledger, owner, now_ms)
            },
            |task| {
                if &task.owner != owner {
                    return Err(TaskError::NotFound);
                }
                let entry = TaskProgressEntry::from(&*task);
                if !entry.due(now_ms) {
                    return Err(TaskError::InvalidTransition);
                }
                append(
                    task,
                    entry.next_sequence().ok_or(TaskError::LimitReached)?,
                    DeliveryAttempt {
                        channel,
                        started_at_ms: now_ms,
                        finished_at_ms: None,
                        result: None,
                    },
                )
            },
        )
    }

    pub(super) fn record_progress_delivery(
        &self,
        owner: &TaskOwner,
        id: &str,
        sequence: u64,
        result: DeliveryResult,
        now_ms: u64,
    ) -> TaskResult<Task> {
        validate_owner(owner)?;
        validate_id(id)?;
        if sequence == 0 {
            return Err(TaskError::InvalidInput);
        }
        let result = match result {
            DeliveryResult::Sent {
                platform_message_id: Some(id),
            } => {
                validate_id(&id)?;
                DeliveryResult::Sent {
                    platform_message_id: Some(id),
                }
            }
            // 没有平台消息编号不构成送达证据，保存未知且冻结该任务后续进度通知。
            DeliveryResult::Sent {
                platform_message_id: None,
            } => DeliveryResult::Unknown,
            DeliveryResult::StoredLocally { .. } => return Err(TaskError::InvalidInput),
            other => other,
        };
        self.update_guarded(
            id,
            now_ms,
            |_| Ok(()),
            |task| {
                if &task.owner != owner {
                    return Err(TaskError::NotFound);
                }
                let notice = task
                    .execution
                    .as_mut()
                    .and_then(|e| e.progress_notices.last_mut())
                    .filter(|n| {
                        n.sequence == sequence
                            && n.attempt.result.is_none()
                            && n.attempt.channel == DeliveryChannel::Proactive
                    })
                    .ok_or(TaskError::InvalidTransition)?;
                if now_ms < notice.attempt.started_at_ms {
                    return Err(TaskError::InvalidInput);
                }
                notice.attempt.finished_at_ms = Some(now_ms);
                notice.attempt.result = Some(result);
                Ok(())
            },
        )
    }

    pub(super) fn record_local_progress(
        &self,
        owner: &TaskOwner,
        id: &str,
        sequence: u64,
        notification_id: &str,
        at_ms: u64,
    ) -> TaskResult<Task> {
        validate_owner(owner)?;
        validate_id(id)?;
        validate_id(notification_id)?;
        if sequence == 0 || at_ms == 0 {
            return Err(TaskError::InvalidInput);
        }
        let task = self.get_owned(owner, id)?.ok_or(TaskError::NotFound)?;
        if let Some(notice) = task
            .execution
            .as_ref()
            .and_then(|e| e.progress_notices.iter().find(|n| n.sequence == sequence))
        {
            return if notice.attempt.channel == DeliveryChannel::LocalInbox
                && notice.attempt.result
                    == Some(DeliveryResult::StoredLocally {
                        notification_id: notification_id.into(),
                    })
            {
                Ok(task)
            } else {
                Err(TaskError::InvalidTransition)
            };
        }
        self.update_guarded(
            id,
            at_ms,
            |ledger| {
                let task = ledger
                    .tasks
                    .iter()
                    .find(|task| task.id == id && &task.owner == owner)
                    .ok_or(TaskError::NotFound)?;
                if same_local(task, sequence, notification_id) {
                    return Ok(());
                }
                scope_guard(ledger, owner, at_ms)
            },
            |task| {
                if &task.owner != owner {
                    return Err(TaskError::NotFound);
                }
                // 并发收件箱恢复可能都读到旧快照；在同一锁内再次确认幂等回执。
                if same_local(task, sequence, notification_id) {
                    return Ok(());
                }
                let entry = TaskProgressEntry::from(&*task);
                // 收件箱事件已在同范围持久化：任务在两次提交间结束不丢失已存事件的回执。
                if !entry.interval_due(at_ms) || entry.next_sequence() != Some(sequence) {
                    return Err(TaskError::InvalidTransition);
                }
                append(
                    task,
                    sequence,
                    DeliveryAttempt {
                        channel: DeliveryChannel::LocalInbox,
                        started_at_ms: at_ms,
                        finished_at_ms: Some(at_ms),
                        result: Some(DeliveryResult::StoredLocally {
                            notification_id: notification_id.into(),
                        }),
                    },
                )
            },
        )
    }
}

pub(super) fn recover(task: &mut Task) -> bool {
    let Some(notice) = task
        .execution
        .as_mut()
        .and_then(|e| e.progress_notices.last_mut())
    else {
        return false;
    };
    if notice.attempt.result.is_some() {
        return false;
    }
    notice.attempt.result = Some(DeliveryResult::Unknown);
    true
}
