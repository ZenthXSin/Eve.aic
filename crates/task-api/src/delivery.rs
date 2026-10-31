//! 通知轮询只复制投递元数据；正文在持久化认领后读取。
use crate::*;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskDeliveryEntry {
    pub id: String,
    pub owner: TaskOwner,
    pub source: String,
    pub created_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub notice: Option<Notice>,
}
impl From<&Task> for TaskDeliveryEntry {
    fn from(task: &Task) -> Self {
        Self {
            id: task.id.clone(),
            owner: task.owner.clone(),
            source: task.source.clone(),
            created_at_ms: task.created_at_ms,
            finished_at_ms: task.finished_at_ms,
            notice: task.notice.clone(),
        }
    }
}
impl TaskDeliveryEntry {
    /// 通知认领不得把候选换成另一身份、来源或创建记录。
    pub fn matches(&self, task: &Task) -> bool {
        self.id == task.id
            && self.owner == task.owner
            && self.source == task.source
            && self.created_at_ms == task.created_at_ms
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskDeliverySnapshot {
    pub tasks: Vec<TaskDeliveryEntry>,
}
impl TaskDeliverySnapshot {
    pub fn validate(&self) -> TaskResult<()> {
        if self.tasks.len() > MAX_TASKS {
            return Err(TaskError::CorruptState);
        }
        let mut ids = BTreeSet::new();
        for task in &self.tasks {
            validate_id(&task.id).map_err(|_| TaskError::CorruptState)?;
            validate_owner(&task.owner).map_err(|_| TaskError::CorruptState)?;
            validate_id(&task.source).map_err(|_| TaskError::CorruptState)?;
            if !ids.insert(&task.id) || task.created_at_ms == 0 {
                return Err(TaskError::CorruptState);
            }
            if let Some(notice) = &task.notice {
                validate_notice(notice, task.finished_at_ms.unwrap_or(0))?;
            }
        }
        Ok(())
    }
    pub fn due_proactive(&self) -> Option<&TaskDeliveryEntry> {
        due_proactive(&self.tasks)
    }
    pub fn due_passive(&self, user_id: &str, message_id: &str) -> Option<&TaskDeliveryEntry> {
        due_passive(&self.tasks, user_id, message_id)
    }
    pub fn overdue(&self, now_ms: u64) -> impl Iterator<Item = &TaskDeliveryEntry> {
        overdue(&self.tasks, now_ms)
    }
}

pub(super) trait NoticeRecord {
    fn id(&self) -> &str;
    fn user_id(&self) -> &str;
    fn finished_at_ms(&self) -> Option<u64>;
    fn notice(&self) -> Option<&Notice>;
}
macro_rules! notice_record {
    ($ty:ty) => {
        impl NoticeRecord for $ty {
            fn id(&self) -> &str {
                &self.id
            }
            fn user_id(&self) -> &str {
                &self.owner.user_id
            }
            fn finished_at_ms(&self) -> Option<u64> {
                self.finished_at_ms
            }
            fn notice(&self) -> Option<&Notice> {
                self.notice.as_ref()
            }
        }
    };
}
notice_record!(Task);
notice_record!(TaskDeliveryEntry);

fn pending(task: &impl NoticeRecord) -> bool {
    task.notice()
        .is_some_and(|notice| notice.status == NoticeStatus::Pending)
}
fn sending<T: NoticeRecord>(tasks: &[T], user_id: &str) -> bool {
    tasks.iter().any(|task| {
        task.user_id() == user_id
            && task
                .notice()
                .is_some_and(|n| n.status == NoticeStatus::Delivering)
    })
}
pub(super) fn due_proactive<T: NoticeRecord>(tasks: &[T]) -> Option<&T> {
    tasks
        .iter()
        .filter(|task| pending(*task))
        .filter(|task| task.notice().is_some_and(|n| n.proactive_attempts() == 0))
        .filter(|task| !sending(tasks, task.user_id()))
        .min_by(|a, b| (a.finished_at_ms(), a.id()).cmp(&(b.finished_at_ms(), b.id())))
}
pub(super) fn due_passive<'a, T: NoticeRecord>(
    tasks: &'a [T],
    user_id: &str,
    message_id: &str,
) -> Option<&'a T> {
    if sending(tasks, user_id) {
        return None;
    }
    tasks
        .iter()
        .filter(|task| task.user_id() == user_id && pending(*task))
        .filter(|task| task.notice().is_some_and(|n| !n.carried_by(message_id)))
        .min_by(|a, b| (a.finished_at_ms(), a.id()).cmp(&(b.finished_at_ms(), b.id())))
}
pub(super) fn overdue<T: NoticeRecord>(tasks: &[T], now_ms: u64) -> impl Iterator<Item = &T> {
    tasks.iter().filter(move |task| {
        pending(*task)
            && task
                .finished_at_ms()
                .is_some_and(|at| now_ms.saturating_sub(at) > DELIVERY_WINDOW_MS)
    })
}

/// 校验通知持久状态；账本与通知目录共用，不授予发送权限。
pub fn validate_notice(notice: &Notice, finished: u64) -> TaskResult<()> {
    let invalid = Err(TaskError::CorruptState);
    if notice.attempts.len() > MAX_ATTEMPTS || notice.proactive_attempts() > 1 {
        return invalid;
    }
    let mut carried = BTreeSet::new();
    for (index, attempt) in notice.attempts.iter().enumerate() {
        if let DeliveryChannel::Passive { message_id } = &attempt.channel {
            validate_id(message_id)?;
        }
        if let Some(DeliveryResult::Sent {
            platform_message_id: Some(id),
        }) = &attempt.result
        {
            validate_id(id)?;
        }
        let last = index + 1 == notice.attempts.len();
        if let DeliveryChannel::Passive { message_id } = &attempt.channel
            && !carried.insert(message_id.as_str())
        {
            return invalid;
        }
        if attempt.started_at_ms < finished
            || (attempt.result.is_none() && !last)
            || (!last && matches!(attempt.result, Some(DeliveryResult::Sent { .. })))
            || (!last && attempt.result == Some(DeliveryResult::Unknown))
            || attempt
                .finished_at_ms
                .is_some_and(|at| at < attempt.started_at_ms)
            || attempt.finished_at_ms.is_some() != attempt.result.is_some()
                && attempt.result != Some(DeliveryResult::Unknown)
        {
            return invalid;
        }
    }
    let last_result = notice.attempts.last().map(|attempt| &attempt.result);
    let sent = matches!(last_result, Some(Some(DeliveryResult::Sent { .. })));
    let open_or_unknown = matches!(
        last_result,
        Some(None) | Some(Some(DeliveryResult::Unknown))
    );
    let consistent = match notice.status {
        NoticeStatus::Pending => !sent && !open_or_unknown && notice.closed_at_ms.is_none(),
        NoticeStatus::Delivering => {
            matches!(last_result, Some(None)) && notice.closed_at_ms.is_none()
        }
        NoticeStatus::Delivered => sent && notice.closed_at_ms.is_some(),
        NoticeStatus::Unknown => {
            matches!(last_result, Some(Some(DeliveryResult::Unknown)))
                && notice.closed_at_ms.is_none()
        }
        NoticeStatus::Expired => !sent && !open_or_unknown && notice.closed_at_ms.is_some(),
    };
    if !consistent {
        return invalid;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, user: &str, variant: usize) -> TaskDeliveryEntry {
        let (status, attempt, closed) = match variant {
            0 | 1 => (NoticeStatus::Pending, None, None),
            2 => (
                NoticeStatus::Pending,
                Some((DeliveryChannel::Proactive, Some(DeliveryResult::NotSent))),
                None,
            ),
            3 => (
                NoticeStatus::Pending,
                Some((
                    DeliveryChannel::Passive {
                        message_id: "message".into(),
                    },
                    Some(DeliveryResult::NotSent),
                )),
                None,
            ),
            4 => (
                NoticeStatus::Delivering,
                Some((DeliveryChannel::Proactive, None)),
                None,
            ),
            5 => (
                NoticeStatus::Delivered,
                Some((
                    DeliveryChannel::Proactive,
                    Some(DeliveryResult::Sent {
                        platform_message_id: Some("sent".into()),
                    }),
                )),
                Some(12),
            ),
            6 => (NoticeStatus::Expired, None, Some(12)),
            _ => (
                NoticeStatus::Unknown,
                Some((DeliveryChannel::Proactive, Some(DeliveryResult::Unknown))),
                None,
            ),
        };
        TaskDeliveryEntry {
            id: id.into(),
            owner: TaskOwner {
                session_id: "session".into(),
                user_id: user.into(),
            },
            source: format!("source:{id}"),
            created_at_ms: 1,
            finished_at_ms: Some(10),
            notice: (variant != 0).then(|| Notice {
                status,
                closed_at_ms: closed,
                attempts: attempt
                    .into_iter()
                    .map(|(channel, result)| DeliveryAttempt {
                        channel,
                        started_at_ms: 11,
                        finished_at_ms: result.as_ref().map(|_| 12),
                        result,
                    })
                    .collect(),
            }),
        }
    }
    // 独立保留旧快照选择公式，避免新旧接口共用实现后的循环对照。
    fn expected<'a>(
        tasks: &'a [TaskDeliveryEntry],
        passive: Option<(&str, &str)>,
    ) -> Option<&'a str> {
        tasks
            .iter()
            .filter(|task| {
                let Some(notice) = &task.notice else {
                    return false;
                };
                let sending = tasks.iter().any(|other| {
                    other.owner.user_id == task.owner.user_id
                        && other
                            .notice
                            .as_ref()
                            .is_some_and(|n| n.status == NoticeStatus::Delivering)
                });
                notice.status == NoticeStatus::Pending
                    && !sending
                    && match passive {
                        Some((user, message)) => {
                            task.owner.user_id == user && !notice.carried_by(message)
                        }
                        None => notice.proactive_attempts() == 0,
                    }
            })
            .min_by_key(|task| (task.finished_at_ms, task.id.clone()))
            .map(|task| task.id.as_str())
    }
    #[test]
    fn directory_matches_original_selection_for_pending_sending_settled_and_unknown() {
        for a in 0..8 {
            for b in 0..8 {
                for c in 0..8 {
                    let directory = TaskDeliverySnapshot {
                        tasks: vec![
                            entry("c", "alice", a),
                            entry("a", "alice", b),
                            entry("b", "bob", c),
                        ],
                    };
                    directory.validate().unwrap();
                    assert_eq!(
                        directory.due_proactive().map(|t| t.id.as_str()),
                        expected(&directory.tasks, None)
                    );
                    for user in ["alice", "bob", "foreign"] {
                        for message in ["", "message", "new-message"] {
                            assert_eq!(
                                directory.due_passive(user, message).map(|t| t.id.as_str()),
                                expected(&directory.tasks, Some((user, message)))
                            );
                        }
                    }
                    assert_eq!(directory.overdue(10 + DELIVERY_WINDOW_MS).count(), 0);
                    assert_eq!(directory.overdue(9).count(), 0);
                    let overdue: Vec<_> = directory
                        .overdue(11 + DELIVERY_WINDOW_MS)
                        .map(|t| t.id.as_str())
                        .collect();
                    let original: Vec<_> = directory
                        .tasks
                        .iter()
                        .filter(|t| {
                            t.notice
                                .as_ref()
                                .is_some_and(|n| n.status == NoticeStatus::Pending)
                        })
                        .map(|t| t.id.as_str())
                        .collect();
                    assert_eq!(overdue, original);
                }
            }
        }
    }
    #[test]
    fn malformed_directory_and_inconsistent_notice_fail_closed() {
        let valid = entry("a", "alice", 1);
        for variant in 0..7 {
            let mut changed = valid.clone();
            match variant {
                0 => changed.owner.session_id.clear(),
                1 => changed.source.clear(),
                2 => changed.created_at_ms = 0,
                3 => changed.notice.as_mut().unwrap().status = NoticeStatus::Delivering,
                4 => changed.notice.as_mut().unwrap().closed_at_ms = Some(12),
                5 => {
                    changed.notice = entry("b", "alice", 2).notice.map(|mut n| {
                        n.attempts[0].started_at_ms = 9;
                        n
                    })
                }
                _ => {
                    changed.notice = entry("b", "alice", 3).notice.map(|mut n| {
                        n.attempts[0].channel = DeliveryChannel::Passive {
                            message_id: String::new(),
                        };
                        n
                    })
                }
            }
            assert!(
                TaskDeliverySnapshot {
                    tasks: vec![changed]
                }
                .validate()
                .is_err()
            );
        }
        assert_eq!(
            TaskDeliverySnapshot {
                tasks: vec![valid.clone(), valid]
            }
            .validate(),
            Err(TaskError::CorruptState)
        );
    }
}
