//! 三十分钟进度通知：每次发送有独立序号与真实回执，不修改任务执行状态。
use crate::*;
use std::collections::BTreeSet;

pub const TASK_PROGRESS_INTERVAL_MS: u64 = 30 * 60 * 1000;
/// 同一范围滚动二十四小时最多四十八次进度通知；每个任务保留最近四十八条回执。
pub const MAX_TASK_PROGRESS_NOTICES: usize = 48;

/// 模型审视过的一份进度事实；只携带序号和摘要，不复制检查点或授予投递权限。
/// 源账本认领时须在同一锁内重新计算，不能先认领再发现事实已经变化。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskProgressToken {
    pub next_sequence: u64,
    pub facts_revision: String,
}

impl TaskProgressToken {
    pub fn from_task(task: &Task) -> TaskResult<Self> {
        validate_id(&task.id)?;
        validate_owner(&task.owner)?;
        let execution = task
            .execution
            .as_ref()
            .ok_or(TaskError::InvalidTransition)?;
        let last = execution.progress_notices.last();
        let next_sequence = last
            .map_or(Some(1), |notice| notice.sequence.checked_add(1))
            .ok_or(TaskError::LimitReached)?;
        let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
        digest.update(b"eve.task-progress-facts:v1\0");
        // 流式计算完整事实摘要，大小不随检查点增长；通知回执独立，只纳入最后序号/回执。
        let facts = (
            (&task.id, &task.owner, &task.source, task.created_at_ms),
            (
                &task.spec.title,
                task.spec.max_steps,
                task.status,
                task.cancel_requested,
            ),
            (task.started_at_ms, task.finished_at_ms, &task.usage),
            (&task.result, &task.failure, &task.notice),
            (
                &execution.checkpoint,
                execution.last_finished_at_ms,
                execution.recoveries,
                execution.resume_at_ms,
                execution.deadline_at_ms,
                execution.authorization_deadline_at_ms,
            ),
            last,
        );
        serde_json::to_writer(ProgressDigest(&mut digest), &facts)
            .map_err(|_| TaskError::InvalidInput)?;
        Ok(Self {
            next_sequence,
            facts_revision: digest
                .finish()
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        })
    }

    pub fn validate(&self) -> TaskResult<()> {
        if self.next_sequence == 0
            || self.facts_revision.len() != 64
            || !self
                .facts_revision
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(TaskError::InvalidInput);
        }
        Ok(())
    }
}

struct ProgressDigest<'a>(&'a mut ring::digest::Context);
impl std::io::Write for ProgressDigest<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskProgressNotice {
    pub sequence: u64,
    /// 认领时实际观察的任务状态；通知不是任务结局。
    pub status: TaskStatus,
    pub observed_finished_at_ms: Option<u64>,
    pub attempt: DeliveryAttempt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskProgressEntry {
    pub id: String,
    pub owner: TaskOwner,
    pub source: String,
    pub created_at_ms: u64,
    pub status: TaskStatus,
    pub finished_at_ms: Option<u64>,
    pub cancel_requested: bool,
    pub resume_at_ms: Option<u64>,
    pub enabled: bool,
    pub notices: Vec<TaskProgressNotice>,
}
impl From<&Task> for TaskProgressEntry {
    fn from(task: &Task) -> Self {
        Self {
            id: task.id.clone(),
            owner: task.owner.clone(),
            source: task.source.clone(),
            created_at_ms: task.created_at_ms,
            status: task.status,
            finished_at_ms: task.finished_at_ms,
            cancel_requested: task.cancel_requested,
            resume_at_ms: task.execution.as_ref().and_then(|e| e.resume_at_ms),
            enabled: task.execution.is_some() && !task.source.starts_with(IDLE_TASK_SOURCE_PREFIX),
            notices: task
                .execution
                .as_ref()
                .map_or_else(Vec::new, |e| e.progress_notices.clone()),
        }
    }
}
impl TaskProgressEntry {
    pub fn matches(&self, task: &Task) -> bool {
        self.id == task.id
            && self.owner == task.owner
            && self.source == task.source
            && self.created_at_ms == task.created_at_ms
    }
    pub fn next_sequence(&self) -> Option<u64> {
        self.notices
            .last()
            .map_or(Some(1), |notice| notice.sequence.checked_add(1))
    }
    pub fn due(&self, now_ms: u64) -> bool {
        self.enabled
            && matches!(
                self.status,
                TaskStatus::Queued | TaskStatus::Running | TaskStatus::Blocked
            )
            && !(self.status == TaskStatus::Blocked
                && self.notices.last().is_some_and(|notice| {
                    notice.status == TaskStatus::Blocked
                        && notice.observed_finished_at_ms == self.finished_at_ms
                }))
            && self.interval_due(now_ms)
    }
    /// 本地收件箱的已提交事件允许在任务随后结束时补结算；不允许绕过原时钟或未知发送。
    pub fn interval_due(&self, now_ms: u64) -> bool {
        let last = self.notices.last();
        self.enabled
            && self.next_sequence().is_some()
            && last.is_none_or(|notice| {
                notice.attempt.result.is_some()
                    && notice.attempt.result != Some(DeliveryResult::Unknown)
            })
            && last
                .map_or(self.created_at_ms, |notice| notice.attempt.started_at_ms)
                .checked_add(TASK_PROGRESS_INTERVAL_MS)
                .is_some_and(|due| now_ms >= due)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskProgressSnapshot {
    pub tasks: Vec<TaskProgressEntry>,
}
impl TaskProgressSnapshot {
    pub fn validate(&self) -> TaskResult<()> {
        if self.tasks.len() > MAX_TASKS {
            return Err(TaskError::CorruptState);
        }
        let mut ids = BTreeSet::new();
        for task in &self.tasks {
            validate_id(&task.id)?;
            validate_owner(&task.owner)?;
            validate_id(&task.source)?;
            if !ids.insert(&task.id) || task.created_at_ms == 0 {
                return Err(TaskError::CorruptState);
            }
            validate_progress_notices(&task.notices, task.created_at_ms)?;
        }
        Ok(())
    }
    pub fn scope_due(&self, owner: &TaskOwner, now_ms: u64) -> bool {
        let attempts = self
            .tasks
            .iter()
            .filter(|task| &task.owner == owner)
            .flat_map(|task| &task.notices);
        let mut recent = 0;
        for notice in attempts {
            if notice
                .attempt
                .started_at_ms
                .checked_add(TASK_PROGRESS_INTERVAL_MS)
                .is_none_or(|due| now_ms < due)
            {
                return false;
            }
            if now_ms.saturating_sub(notice.attempt.started_at_ms) < DAY_MS {
                recent += 1;
            }
        }
        recent < MAX_TASK_PROGRESS_NOTICES
    }
    pub fn due(&self, now_ms: u64) -> Vec<&TaskProgressEntry> {
        let mut due: Vec<_> = self
            .tasks
            .iter()
            .filter(|task| task.due(now_ms) && self.scope_due(&task.owner, now_ms))
            .collect();
        due.sort_by_key(|task| {
            (
                task.notices
                    .last()
                    .map_or(task.created_at_ms, |n| n.attempt.started_at_ms),
                task.id.as_str(),
            )
        });
        due
    }
}

pub fn validate_progress_notices(
    notices: &[TaskProgressNotice],
    created_at_ms: u64,
) -> TaskResult<()> {
    if notices.len() > MAX_TASK_PROGRESS_NOTICES {
        return Err(TaskError::CorruptState);
    }
    let mut previous: Option<&TaskProgressNotice> = None;
    for (index, notice) in notices.iter().enumerate() {
        let attempt = &notice.attempt;
        if notice.sequence == 0
            || notice
                .observed_finished_at_ms
                .is_some_and(|at| at < created_at_ms)
            || notice.status.finished() != notice.observed_finished_at_ms.is_some()
            || created_at_ms
                .checked_add(TASK_PROGRESS_INTERVAL_MS)
                .is_none_or(|due| attempt.started_at_ms < due)
            || matches!(attempt.channel, DeliveryChannel::Passive { .. })
            || attempt
                .finished_at_ms
                .is_some_and(|at| at < attempt.started_at_ms)
            || (attempt.result.is_none()
                && (attempt.finished_at_ms.is_some() || index + 1 != notices.len()))
            || (attempt.result.is_some()
                && attempt.result != Some(DeliveryResult::Unknown)
                && attempt.finished_at_ms.is_none())
            || (attempt.result == Some(DeliveryResult::Unknown) && index + 1 != notices.len())
            || previous.is_some_and(|old| {
                old.sequence.checked_add(1) != Some(notice.sequence)
                    || old
                        .attempt
                        .started_at_ms
                        .checked_add(TASK_PROGRESS_INTERVAL_MS)
                        .is_none_or(|due| attempt.started_at_ms < due)
            })
        {
            return Err(TaskError::CorruptState);
        }
        match &attempt.result {
            Some(DeliveryResult::Sent {
                platform_message_id: Some(id),
            }) => {
                validate_id(id)?;
                if attempt.channel != DeliveryChannel::Proactive {
                    return Err(TaskError::CorruptState);
                }
            }
            Some(DeliveryResult::Sent {
                platform_message_id: None,
            }) => return Err(TaskError::CorruptState),
            Some(DeliveryResult::StoredLocally { notification_id }) => {
                validate_id(notification_id)?;
                if attempt.channel != DeliveryChannel::LocalInbox {
                    return Err(TaskError::CorruptState);
                }
            }
            _ if attempt.channel == DeliveryChannel::LocalInbox => {
                return Err(TaskError::CorruptState);
            }
            _ => {}
        }
        previous = Some(notice);
    }
    Ok(())
}
