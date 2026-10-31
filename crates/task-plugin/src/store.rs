//! 后台任务账本：创建、开始、结束与通知尝试都先持久化。重启时仅从安全检查点接续，未知工具结果不重放；
//! 发送中的通知记为结果未知（不重发）。
use crate::strict_json;
use eve_plugin_api::{PluginContext, PluginError, PluginResult};
use eve_task_api::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    sync::{Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

const FORMAT_VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    format_version: u32,
    tasks: Vec<Task>,
}
struct Inner {
    ledger: Ledger,
    context: Option<PluginContext>,
}
pub(super) struct StoredTasks {
    inner: Mutex<Inner>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(1)
        .max(1)
}

impl StoredTasks {
    pub(super) fn open(context: PluginContext) -> TaskResult<Self> {
        let mut ledger = match context
            .state_get(TASK_STATE_KEY)
            .map_err(|_| TaskError::Storage)?
        {
            None => Ledger {
                format_version: FORMAT_VERSION,
                tasks: vec![],
            },
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(TaskError::CorruptState);
                }
                let value = strict_json::from_slice(&bytes).map_err(|_| TaskError::CorruptState)?;
                if !matches!(
                    value.get("format_version").and_then(|value| value.as_u64()),
                    Some(1..=3)
                ) {
                    return Err(if value.get("format_version").is_some() {
                        TaskError::UnsupportedVersion
                    } else {
                        TaskError::CorruptState
                    });
                }
                let mut ledger: Ledger =
                    serde_json::from_value(value).map_err(|_| TaskError::CorruptState)?;
                if ledger.format_version == 1
                    && ledger
                        .tasks
                        .iter()
                        .any(|task| task.spec.context.is_some() || task.execution.is_some())
                {
                    return Err(TaskError::CorruptState);
                }
                if ledger.format_version < 3
                    && ledger.tasks.iter().any(|t| t.status == TaskStatus::Blocked)
                {
                    return Err(TaskError::CorruptState);
                }
                validate_ledger(&ledger)?;
                ledger.format_version = FORMAT_VERSION;
                ledger
            }
        };
        let mut changed = false;
        let now = now_ms();
        for task in &mut ledger.tasks {
            if task.status == TaskStatus::Running {
                if !task.cancel_requested
                    && let Some(execution) = &mut task.execution
                {
                    // 最终结果已经持久化，只有任务结局尚未提交：直接了结，不请求模型或重放工具。
                    if let Some(checkpoint) = &execution.checkpoint
                        && let Some(result) = &checkpoint.result
                    {
                        if task.source.starts_with(FAILURE_RECOVERY_SOURCE_PREFIX)
                            && recovery_needs_step_approval(checkpoint)
                        {
                            task.status = TaskStatus::Failed;
                            task.failure = Some(
                                "本次步数预算不足；已有进度保留，等待原会话明确同意追加步数。"
                                    .into(),
                            );
                        } else if blocked_progress(&checkpoint.messages).is_some() {
                            task.status = TaskStatus::Blocked;
                            task.result = Some(bounded(result, MAX_RESULT_BYTES));
                        } else {
                            task.status = TaskStatus::Completed;
                            task.result = Some(bounded(result, MAX_RESULT_BYTES));
                        }
                        task.usage = Some(checkpoint_usage(checkpoint));
                        task.finished_at_ms = Some(now.max(last_time(task)));
                        task.notice = Some(Notice {
                            status: NoticeStatus::Pending,
                            attempts: vec![],
                            closed_at_ms: None,
                        });
                        changed = true;
                        continue;
                    }
                    // 在模型边界崩溃可接续；待执行工具批次或异常工具回执不可自动重放。
                    if can_retry(execution, now) {
                        schedule_retry(execution, now);
                        changed = true;
                        continue;
                    }
                }
                // 执行中进程退出：不知道做到了哪一步，不自动重跑。已请求取消的按取消结束。
                let at = now.max(last_time(task));
                task.finished_at_ms = Some(at);
                if task.cancel_requested {
                    task.status = TaskStatus::Cancelled;
                } else {
                    task.status = TaskStatus::Interrupted;
                    task.notice = Some(Notice {
                        status: NoticeStatus::Pending,
                        attempts: vec![],
                        closed_at_ms: None,
                    });
                }
                changed = true;
            }
            if let Some(notice) = &mut task.notice
                && notice.status == NoticeStatus::Delivering
            {
                // 发送前后进程退出：是否送达不确定，不重发，也不当作已送达。
                if let Some(attempt) = notice.attempts.last_mut() {
                    attempt.result = Some(DeliveryResult::Unknown);
                }
                notice.status = NoticeStatus::Unknown;
                changed = true;
            }
        }
        if changed {
            validate_ledger(&ledger)?;
            let bytes = encode(&ledger)?;
            context
                .state_set(TASK_STATE_KEY, bytes)
                .map_err(|_| TaskError::Storage)?;
        }
        Ok(Self {
            inner: Mutex::new(Inner {
                ledger,
                context: Some(context),
            }),
        })
    }

    fn lock(&self) -> TaskResult<MutexGuard<'_, Inner>> {
        let inner = self.inner.lock().map_err(|_| TaskError::Unavailable)?;
        if inner.context.is_none() {
            return Err(TaskError::Unavailable);
        }
        Ok(inner)
    }

    pub(super) fn snapshot(&self) -> TaskResult<TaskSnapshot> {
        Ok(TaskSnapshot {
            tasks: self.lock()?.ledger.tasks.clone(),
        })
    }

    pub(super) fn evidence_snapshot(
        &self,
        keys: &[TaskEvidenceKey],
    ) -> TaskResult<TaskEvidenceSnapshot> {
        TaskEvidenceSnapshot::select(&self.lock()?.ledger.tasks, keys)
    }

    pub(super) fn scheduling_snapshot(&self) -> TaskResult<TaskSchedulingSnapshot> {
        let directory = TaskSchedulingSnapshot {
            tasks: self
                .lock()?
                .ledger
                .tasks
                .iter()
                .map(TaskSchedulingEntry::from)
                .collect(),
        };
        directory.validate()?;
        Ok(directory)
    }

    pub(super) fn delivery_snapshot(&self) -> TaskResult<TaskDeliverySnapshot> {
        let directory = TaskDeliverySnapshot {
            tasks: self
                .lock()?
                .ledger
                .tasks
                .iter()
                .map(TaskDeliveryEntry::from)
                .collect(),
        };
        directory.validate()?;
        Ok(directory)
    }

    pub(super) fn summaries_for(&self, owner: &TaskOwner) -> TaskResult<Vec<TaskSummary>> {
        validate_owner(owner)?;
        Ok(self
            .lock()?
            .ledger
            .tasks
            .iter()
            .filter(|task| task.owner == *owner)
            .map(TaskSummary::from)
            .collect())
    }
    pub(super) fn get_owned(&self, owner: &TaskOwner, id: &str) -> TaskResult<Option<Task>> {
        validate_owner(owner)?;
        validate_id(id)?;
        Ok(self
            .lock()?
            .ledger
            .tasks
            .iter()
            .find(|task| task.id == id && task.owner == *owner)
            .cloned())
    }

    pub(super) fn create(
        &self,
        owner: &TaskOwner,
        spec: TaskSpec,
        source: &str,
        now_ms: u64,
    ) -> TaskResult<Task> {
        self.create_with_deadline(owner, spec, source, None, now_ms)
    }
    pub(super) fn create_with_deadline(
        &self,
        owner: &TaskOwner,
        spec: TaskSpec,
        source: &str,
        deadline_at_ms: Option<u64>,
        now_ms: u64,
    ) -> TaskResult<Task> {
        if deadline_at_ms.is_some_and(|d| d <= now_ms) {
            return Err(TaskError::InvalidInput);
        }
        validate_owner(owner)?;
        validate_id(source)?;
        validate_spec(&spec)?;
        if spec
            .context
            .as_ref()
            .is_some_and(|context| &context.owner != owner)
        {
            return Err(TaskError::InvalidInput);
        }
        if now_ms == 0 {
            return Err(TaskError::InvalidInput);
        }
        let id = task_id(&owner.user_id, source);
        let mut inner = self.lock()?;
        // 同一次派发重复执行：返回原记录。
        if let Some(existing) = inner.ledger.tasks.iter().find(|task| task.id == id) {
            if deadline_at_ms.is_some() && &existing.owner != owner {
                return Err(TaskError::NotFound);
            }
            return Ok(existing.clone());
        }
        let mine = || {
            inner
                .ledger
                .tasks
                .iter()
                .filter(|task| task.owner.user_id == owner.user_id)
        };
        let active = mine().filter(|task| !task.status.finished()).count();
        let recent = mine()
            .filter(|task| now_ms.saturating_sub(task.created_at_ms) < DAY_MS)
            .count();
        if active >= MAX_ACTIVE_PER_OWNER || recent >= MAX_CREATED_PER_OWNER_PER_DAY {
            return Err(TaskError::LimitReached);
        }
        // 满了时腾出最早了结的一条；进行中与通知未了结的任务从不删除。
        let evict = if inner.ledger.tasks.len() >= MAX_TASKS {
            let oldest = inner
                .ledger
                .tasks
                .iter()
                .enumerate()
                .filter_map(|(index, task)| task.settled_at().map(|at| (at, index)))
                .min()
                .ok_or(TaskError::LimitReached)?;
            Some(oldest.1)
        } else {
            None
        };
        let task = Task {
            id,
            owner: owner.clone(),
            spec,
            source: source.into(),
            created_at_ms: now_ms,
            status: TaskStatus::Queued,
            cancel_requested: false,
            started_at_ms: None,
            finished_at_ms: None,
            usage: None,
            result: None,
            failure: None,
            notice: None,
            execution: Some(TaskExecution {
                authorization_deadline_at_ms: deadline_at_ms,
                ..Default::default()
            }),
        };
        let tasks: Vec<_> = inner
            .ledger
            .tasks
            .iter()
            .enumerate()
            .filter(|(position, _)| Some(*position) != evict)
            .map(|(_, existing)| existing)
            .chain(std::iter::once(&task))
            .collect();
        let bytes = encode_tasks(&tasks)?;
        commit_bytes(&mut inner, bytes)?;
        if let Some(position) = evict {
            inner.ledger.tasks.remove(position);
        }
        inner.ledger.tasks.push(task.clone());
        Ok(task)
    }

    pub(super) fn start(&self, id: &str, now_ms: u64) -> TaskResult<Task> {
        self.update(id, now_ms, |task| {
            if task.status != TaskStatus::Queued {
                return Err(TaskError::InvalidTransition);
            }
            task.status = TaskStatus::Running;
            task.started_at_ms = Some(now_ms.max(last_time(task)));
            task.execution.get_or_insert_with(TaskExecution::default);
            Ok(())
        })
    }

    pub(super) fn finish(
        &self,
        id: &str,
        end: TaskEnd,
        mut usage: TaskUsage,
        now_ms: u64,
    ) -> TaskResult<Task> {
        self.update(id, now_ms, |task| {
            if task.status != TaskStatus::Running {
                return Err(TaskError::InvalidTransition);
            }
            if usage.tool_calls > task.spec.max_steps {
                return Err(TaskError::InvalidInput);
            }
            if let Some(checkpoint) = task.execution.as_ref().and_then(|e| e.checkpoint.as_ref()) {
                let saved = checkpoint_usage(checkpoint);
                usage.tool_calls = usage.tool_calls.max(saved.tool_calls);
                usage.model_requests = usage.model_requests.max(saved.model_requests);
            }
            let notice = Some(Notice {
                status: NoticeStatus::Pending,
                attempts: vec![],
                closed_at_ms: None,
            });
            // 取消与模型失败/最终答复并发到达时，以已持久化的取消请求为准。
            let end = if task.cancel_requested {
                TaskEnd::Cancelled
            } else {
                end
            };
            match end {
                TaskEnd::Blocked { result } => {
                    if result.trim().is_empty() {
                        return Err(TaskError::InvalidInput);
                    }
                    task.status = TaskStatus::Blocked;
                    task.result = Some(bounded(&result, MAX_RESULT_BYTES));
                    task.notice = notice;
                }
                TaskEnd::Completed { result } => {
                    if result.trim().is_empty() {
                        return Err(TaskError::InvalidInput);
                    }
                    task.status = TaskStatus::Completed;
                    task.result = Some(bounded(&result, MAX_RESULT_BYTES));
                    task.notice = notice;
                }
                TaskEnd::Failed { reason } => {
                    if reason.trim().is_empty() {
                        return Err(TaskError::InvalidInput);
                    }
                    task.status = TaskStatus::Failed;
                    task.failure = Some(bounded(&reason, MAX_FAILURE_BYTES));
                    task.notice = notice;
                }
                TaskEnd::Cancelled => {
                    if !task.cancel_requested {
                        return Err(TaskError::InvalidTransition);
                    }
                    task.status = TaskStatus::Cancelled;
                }
            }
            task.usage = Some(usage);
            task.finished_at_ms = Some(now_ms.max(last_time(task)));
            Ok(())
        })
    }

    pub(super) fn checkpoint(
        &self,
        id: &str,
        checkpoint: eve_llm_api::TurnCheckpoint,
        deadline_at_ms: u64,
        now_ms: u64,
    ) -> TaskResult<Task> {
        self.update(id, now_ms, |task| {
            if task.status != TaskStatus::Running || task.cancel_requested {
                return Err(TaskError::InvalidTransition);
            }
            checkpoint
                .validate(task.spec.max_steps as usize)
                .map_err(|_| TaskError::InvalidInput)?;
            if checkpoint.scope.session_id != task.owner.session_id
                || checkpoint.scope.user_id != task.owner.user_id
                || deadline_at_ms <= now_ms
            {
                return Err(TaskError::InvalidInput);
            }
            let execution = task
                .execution
                .as_mut()
                .ok_or(TaskError::InvalidTransition)?;
            if execution
                .authorization_deadline_at_ms
                .is_some_and(|deadline| deadline_at_ms > deadline)
                || execution
                    .deadline_at_ms
                    .is_some_and(|deadline| deadline != deadline_at_ms)
                || execution.checkpoint.as_ref().is_some_and(|old| {
                    checkpoint.used_tool_calls < old.used_tool_calls
                        || checkpoint.delegated_tool_calls < old.delegated_tool_calls
                        || checkpoint.delegated_model_requests < old.delegated_model_requests
                        || checkpoint.provider_requests < old.provider_requests
                        || old.result.is_some()
                })
            {
                return Err(TaskError::InvalidInput);
            }
            execution.deadline_at_ms = Some(deadline_at_ms);
            execution.resume_at_ms = None;
            execution.checkpoint = Some(checkpoint);
            Ok(())
        })
    }

    pub(super) fn resume_blocked(
        &self,
        owner: &TaskOwner,
        id: &str,
        evidence: &str,
        now_ms: u64,
    ) -> TaskResult<Task> {
        if evidence.trim().is_empty() || evidence.len() > MAX_INSTRUCTIONS_BYTES {
            return Err(TaskError::InvalidInput);
        }
        self.update_guarded(id, now_ms, |ledger| {
            if ledger.tasks.iter().filter(|task| task.owner.user_id == owner.user_id && !task.status.finished()).count() >= MAX_ACTIVE_PER_OWNER {
                return Err(TaskError::LimitReached);
            }
            Ok(())
        }, |task| {
            if &task.owner != owner { return Err(TaskError::NotFound); }
            if task.status != TaskStatus::Blocked || task.cancel_requested
                || task.notice.as_ref().is_some_and(|n| n.status == NoticeStatus::Delivering) {
                return Err(TaskError::InvalidTransition);
            }
            let execution = task.execution.as_mut().ok_or(TaskError::InvalidTransition)?;
            let frame = execution.checkpoint.as_mut().ok_or(TaskError::InvalidTransition)?;
            if !frame.resumable() || execution.recoveries >= MAX_AUTO_RECOVERIES || execution.deadline_at_ms.is_none_or(|at| at <= now_ms)
                || frame.used_tool_calls + frame.delegated_tool_calls >= task.spec.max_steps as usize
                || frame.closing {
                return Err(TaskError::InvalidTransition);
            }
            // 原问题、通知记录作为历史数据冻结；解除终局仅发生在显式可信接续入口。
            let data = serde_json::json!({"host_event":"blocked_task_resumed","evidence":evidence,
                "previous_result":task.result,"previous_notice":task.notice});
            // 最终文本原本单独保存在 result；接续前补成完整上一轮，让接续模型能看到上一轮完整答复。
            if let Some(result) = frame.result.take().or_else(|| task.result.clone()) {
                frame.messages.push(eve_llm_api::ChatMessage::text(eve_llm_api::ChatRole::Assistant, result));
            }
            frame.messages.push(eve_llm_api::ChatMessage::text(eve_llm_api::ChatRole::User,
                format!("宿主条件事件；以下 JSON 是观察数据，不增加原任务授权。继续原任务，核对条件并记录新的 task_progress：{data}")));
            frame.remaining = Some(bounded(evidence, 2048));
            frame.validate(task.spec.max_steps as usize).map_err(|_| TaskError::InvalidInput)?;
            execution.recoveries += 1;
            execution.resume_at_ms = Some(now_ms);
            task.status = TaskStatus::Running;
            task.finished_at_ms = None;
            task.usage = None;
            task.result = None;
            task.notice = None;
            Ok(())
        })
    }

    pub(super) fn retry(&self, id: &str, now_ms: u64) -> TaskResult<Task> {
        self.update(id, now_ms, |task| {
            if task.status != TaskStatus::Running || task.cancel_requested {
                return Err(TaskError::InvalidTransition);
            }
            let execution = task
                .execution
                .as_mut()
                .ok_or(TaskError::InvalidTransition)?;
            if !can_retry(execution, now_ms) || execution.resume_at_ms.is_some() {
                return Err(TaskError::InvalidTransition);
            }
            schedule_retry(execution, now_ms);
            Ok(())
        })
    }

    pub(super) fn cancel(&self, user_id: &str, id: &str, now_ms: u64) -> TaskResult<Task> {
        self.update(id, now_ms, |task| {
            // 其他用户的任务如同不存在。
            if task.owner.user_id != user_id {
                return Err(TaskError::NotFound);
            }
            match task.status {
                TaskStatus::Queued => {
                    task.status = TaskStatus::Cancelled;
                    task.finished_at_ms = Some(now_ms.max(last_time(task)));
                    Ok(())
                }
                TaskStatus::Running if !task.cancel_requested => {
                    task.cancel_requested = true;
                    Ok(())
                }
                TaskStatus::Blocked
                    if task
                        .notice
                        .as_ref()
                        .is_none_or(|notice| notice.status != NoticeStatus::Delivering) =>
                {
                    // 阻塞任务仍可显式撤回，之后条件触发器不得再次接续它。
                    task.status = TaskStatus::Cancelled;
                    task.cancel_requested = true;
                    task.finished_at_ms = Some(now_ms.max(last_time(task)));
                    task.result = None;
                    task.notice = None;
                    Ok(())
                }
                _ => Err(TaskError::InvalidTransition),
            }
        })
    }

    pub(super) fn claim(
        &self,
        id: &str,
        channel: DeliveryChannel,
        now_ms: u64,
    ) -> TaskResult<Task> {
        validate_channel(&channel)?;
        self.update(id, now_ms, |task| {
            let at = now_ms.max(last_time(task));
            let notice = task
                .notice
                .as_mut()
                .filter(|notice| notice.status == NoticeStatus::Pending)
                .ok_or(TaskError::InvalidTransition)?;
            if notice.attempts.len() >= MAX_ATTEMPTS
                || (channel == DeliveryChannel::Proactive && notice.proactive_attempts() > 0)
                || matches!(&channel, DeliveryChannel::Passive { message_id } if notice.carried_by(message_id))
            {
                return Err(TaskError::InvalidTransition);
            }
            notice.attempts.push(DeliveryAttempt {
                channel,
                started_at_ms: at,
                finished_at_ms: None,
                result: None,
            });
            notice.status = NoticeStatus::Delivering;
            Ok(())
        })
    }

    pub(super) fn record_delivery(
        &self,
        id: &str,
        result: DeliveryResult,
        now_ms: u64,
    ) -> TaskResult<Task> {
        validate_result(&result)?;
        if result == DeliveryResult::Unknown {
            // 结果未知只由重启核对写入。
            return Err(TaskError::InvalidInput);
        }
        self.update(id, now_ms, |task| {
            let at = now_ms.max(last_time(task));
            let notice = task
                .notice
                .as_mut()
                .filter(|notice| notice.status == NoticeStatus::Delivering)
                .ok_or(TaskError::InvalidTransition)?;
            let attempts = notice.attempts.len();
            let attempt = notice
                .attempts
                .last_mut()
                .filter(|attempt| attempt.result.is_none())
                .ok_or(TaskError::InvalidTransition)?;
            attempt.finished_at_ms = Some(at);
            attempt.result = Some(result.clone());
            match result {
                DeliveryResult::Sent { .. } => {
                    notice.status = NoticeStatus::Delivered;
                    notice.closed_at_ms = Some(at);
                }
                _ if attempts >= MAX_ATTEMPTS => {
                    notice.status = NoticeStatus::Expired;
                    notice.closed_at_ms = Some(at);
                }
                _ => notice.status = NoticeStatus::Pending,
            }
            Ok(())
        })
    }

    pub(super) fn expire(&self, id: &str, now_ms: u64) -> TaskResult<Task> {
        self.update(id, now_ms, |task| {
            let finished = task.finished_at_ms.ok_or(TaskError::InvalidTransition)?;
            let at = now_ms.max(last_time(task));
            let notice = task
                .notice
                .as_mut()
                .filter(|notice| notice.status == NoticeStatus::Pending)
                .ok_or(TaskError::InvalidTransition)?;
            if now_ms.saturating_sub(finished) <= DELIVERY_WINDOW_MS {
                return Err(TaskError::InvalidTransition);
            }
            notice.status = NoticeStatus::Expired;
            notice.closed_at_ms = Some(at);
            Ok(())
        })
    }

    fn update(
        &self,
        id: &str,
        now_ms: u64,
        apply: impl FnOnce(&mut Task) -> TaskResult<()>,
    ) -> TaskResult<Task> {
        self.update_guarded(id, now_ms, |_| Ok(()), apply)
    }

    fn update_guarded(
        &self,
        id: &str,
        now_ms: u64,
        guard: impl FnOnce(&Ledger) -> TaskResult<()>,
        apply: impl FnOnce(&mut Task) -> TaskResult<()>,
    ) -> TaskResult<Task> {
        if now_ms == 0 {
            return Err(TaskError::InvalidInput);
        }
        let mut inner = self.lock()?;
        guard(&inner.ledger)?;
        let position = inner
            .ledger
            .tasks
            .iter()
            .position(|task| task.id == id)
            .ok_or(TaskError::NotFound)?;
        let mut task = inner.ledger.tasks[position].clone();
        apply(&mut task)?;
        validate_task(&task).map_err(|_| TaskError::InvalidInput)?;
        let tasks: Vec<_> = inner
            .ledger
            .tasks
            .iter()
            .enumerate()
            .map(|(index, existing)| if index == position { &task } else { existing })
            .collect();
        let bytes = encode_tasks(&tasks)?;
        commit_bytes(&mut inner, bytes)?;
        inner.ledger.tasks[position] = task.clone();
        Ok(task)
    }

    pub(super) fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("后台任务状态锁不可用".into()))?
            .context = None;
        Ok(())
    }
}

fn checkpoint_usage(checkpoint: &eve_llm_api::TurnCheckpoint) -> TaskUsage {
    TaskUsage {
        tool_calls: (checkpoint.used_tool_calls + checkpoint.delegated_tool_calls) as u32,
        model_requests: (checkpoint.provider_requests + checkpoint.delegated_model_requests) as u32,
    }
}

fn can_retry(execution: &TaskExecution, now_ms: u64) -> bool {
    execution.recoveries < MAX_AUTO_RECOVERIES
        && execution
            .deadline_at_ms
            .is_none_or(|deadline| now_ms.saturating_add(10_000) < deadline)
        && execution
            .checkpoint
            .as_ref()
            .is_none_or(|checkpoint| checkpoint.resumable() && checkpoint.result.is_none())
}

fn schedule_retry(execution: &mut TaskExecution, now_ms: u64) {
    execution.recoveries += 1;
    execution.resume_at_ms = Some(now_ms.saturating_add(1000_u64 << execution.recoveries));
}

/// 任务上最近一次已记录的时间；之后的记录不能更早。
fn last_time(task: &Task) -> u64 {
    let notice = task.notice.iter().flat_map(|notice| {
        notice
            .attempts
            .iter()
            .flat_map(|attempt| [Some(attempt.started_at_ms), attempt.finished_at_ms])
            .chain([notice.closed_at_ms])
    });
    [
        Some(task.created_at_ms),
        task.started_at_ms,
        task.finished_at_ms,
    ]
    .into_iter()
    .chain(notice)
    .flatten()
    .max()
    .unwrap_or(0)
}

fn validate_channel(channel: &DeliveryChannel) -> TaskResult<()> {
    match channel {
        DeliveryChannel::Passive { message_id } => validate_id(message_id),
        DeliveryChannel::Proactive => Ok(()),
    }
}

fn validate_result(result: &DeliveryResult) -> TaskResult<()> {
    match result {
        DeliveryResult::Sent {
            platform_message_id: Some(id),
        } => validate_id(id),
        _ => Ok(()),
    }
}

fn commit_bytes(inner: &mut Inner, bytes: Vec<u8>) -> TaskResult<()> {
    let context = inner.context.as_ref().ok_or(TaskError::Unavailable)?;
    // 失败也可能已经提交；关闭整个实例，禁止旧缓存继续读取或覆盖后端。
    if context.state_set(TASK_STATE_KEY, bytes).is_err() {
        inner.context = None;
        return Err(TaskError::Storage);
    }
    Ok(())
}

fn encode(ledger: &Ledger) -> TaskResult<Vec<u8>> {
    encode_tasks(&ledger.tasks.iter().collect::<Vec<_>>())
}

#[derive(Serialize)]
struct BorrowedLedger<'a> {
    format_version: u32,
    tasks: &'a [&'a Task],
}

fn encode_tasks(tasks: &[&Task]) -> TaskResult<Vec<u8>> {
    if tasks.len() > MAX_TASKS {
        return Err(TaskError::LimitReached);
    }
    let format_version = if tasks.iter().any(|t| t.status == TaskStatus::Blocked) {
        3
    } else {
        FORMAT_VERSION
    };
    // 重启时写入的中断与未知结局比原状态略长；为进行中的任务预留空间，保证能保存。
    let open = tasks
        .iter()
        .filter(|task| !task.status.finished() || task.notice.is_some())
        .count();
    // 进行中的任务结束时还要写入结果；按上限预留。
    let pending_results = tasks.iter().filter(|task| !task.status.finished()).count();
    let reserved = open
        .saturating_mul(160)
        .saturating_add(pending_results.saturating_mul(MAX_RESULT_BYTES * 2));
    let available = MAX_STATE_BYTES
        .checked_sub(reserved)
        .ok_or(TaskError::LimitReached)?;
    let mut writer = LimitedJson {
        bytes: vec![],
        available,
        exceeded: false,
    };
    if serde_json::to_writer(
        &mut writer,
        &BorrowedLedger {
            format_version,
            tasks,
        },
    )
    .is_err()
    {
        return Err(if writer.exceeded {
            TaskError::LimitReached
        } else {
            TaskError::InvalidInput
        });
    }
    Ok(writer.bytes)
}

/// 保留原字节预算和收尾预留；超过可用载荷时停止编码，不先分配整份超额 JSON。
struct LimitedJson {
    bytes: Vec<u8>,
    available: usize,
    exceeded: bool,
}
impl std::io::Write for LimitedJson {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if buffer.len() > self.available.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(std::io::Error::other("后台任务编码超过保留预算"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 启动时完整核对；任何不一致都拒绝打开并保留原字节。
fn validate_ledger(ledger: &Ledger) -> TaskResult<()> {
    if ledger.tasks.len() > MAX_TASKS {
        return Err(TaskError::CorruptState);
    }
    let mut ids = BTreeSet::new();
    for task in &ledger.tasks {
        validate_task(task).map_err(|_| TaskError::CorruptState)?;
        if !ids.insert(task.id.as_str()) {
            return Err(TaskError::CorruptState);
        }
    }
    Ok(())
}

fn validate_task(task: &Task) -> TaskResult<()> {
    validate_id(&task.id)?;
    validate_owner(&task.owner)?;
    validate_id(&task.source)?;
    validate_spec(&task.spec)?;
    if task
        .spec
        .context
        .as_ref()
        .is_some_and(|context| context.owner != task.owner)
    {
        return Err(TaskError::CorruptState);
    }
    if let Some(execution) = &task.execution {
        if execution
            .authorization_deadline_at_ms
            .is_some_and(|d| d <= task.created_at_ms)
            || execution
                .deadline_at_ms
                .zip(execution.authorization_deadline_at_ms)
                .is_some_and(|(d, a)| d > a)
            || execution.recoveries > MAX_AUTO_RECOVERIES
            || execution.resume_at_ms == Some(0)
            || (execution.resume_at_ms.is_some() && execution.recoveries == 0)
            || execution.deadline_at_ms.is_some_and(|deadline| {
                deadline <= task.started_at_ms.unwrap_or(task.created_at_ms)
            })
            || execution.checkpoint.is_some() != execution.deadline_at_ms.is_some()
            || (task.status == TaskStatus::Queued
                && (execution.checkpoint.is_some()
                    || execution.recoveries != 0
                    || execution.resume_at_ms.is_some()))
        {
            return Err(TaskError::CorruptState);
        }
        if let Some(checkpoint) = &execution.checkpoint {
            checkpoint
                .validate(task.spec.max_steps as usize)
                .map_err(|_| TaskError::CorruptState)?;
            if checkpoint.scope.session_id != task.owner.session_id
                || checkpoint.scope.user_id != task.owner.user_id
                || task.usage.is_some_and(|usage| {
                    usage.tool_calls
                        < (checkpoint.used_tool_calls + checkpoint.delegated_tool_calls) as u32
                        || usage.model_requests
                            < (checkpoint.provider_requests + checkpoint.delegated_model_requests)
                                as u32
                })
            {
                return Err(TaskError::CorruptState);
            }
        }
    }
    let invalid = Err(TaskError::CorruptState);
    if task.id != task_id(&task.owner.user_id, &task.source) || task.created_at_ms == 0 {
        return invalid;
    }
    let started = task.started_at_ms;
    let finished = task.finished_at_ms;
    if started.is_some_and(|at| at < task.created_at_ms)
        || finished.is_some_and(|at| at < started.unwrap_or(task.created_at_ms))
        || task
            .usage
            .is_some_and(|usage| usage.tool_calls > task.spec.max_steps)
        || task
            .result
            .as_ref()
            .is_some_and(|text| text.trim().is_empty() || text.len() > MAX_RESULT_BYTES)
        || task
            .failure
            .as_ref()
            .is_some_and(|text| text.trim().is_empty() || text.len() > MAX_FAILURE_BYTES)
    {
        return invalid;
    }
    let shape = (
        started.is_some(),
        finished.is_some(),
        task.usage.is_some(),
        task.result.is_some(),
        task.failure.is_some(),
        task.notice.is_some(),
    );
    let consistent = match task.status {
        TaskStatus::Queued => {
            shape == (false, false, false, false, false, false) && !task.cancel_requested
        }
        TaskStatus::Running => shape == (true, false, false, false, false, false),
        TaskStatus::Completed | TaskStatus::Blocked => {
            shape == (true, true, true, true, false, true)
        }
        TaskStatus::Failed => shape == (true, true, true, false, true, true),
        TaskStatus::Interrupted => {
            shape == (true, true, false, false, false, true) && !task.cancel_requested
        }
        // 排队中取消没有开始与用量；执行中取消保存用量，重启时才发现的取消没有用量。
        TaskStatus::Cancelled => {
            shape.1
                && (shape.0 || !shape.2)
                && !shape.3
                && !shape.4
                && !shape.5
                && (shape.0 == task.cancel_requested)
        }
    };
    if !consistent {
        return invalid;
    }
    if let Some(notice) = &task.notice {
        validate_notice(notice, finished.unwrap_or(0))?;
    }
    Ok(())
}
