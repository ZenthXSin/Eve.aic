//! 提醒账本：创建、投递尝试与结果都先持久化；重启时把发送中的投递记为结果未知，不重发。
use crate::strict_json;
use eve_plugin_api::{PluginContext, PluginError, PluginResult};
use eve_reminder_api::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    sync::{Mutex, MutexGuard},
};

const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    format_version: u32,
    reminders: Vec<Reminder>,
}
struct Inner {
    ledger: Ledger,
    context: Option<PluginContext>,
}
pub(super) struct StoredReminders {
    inner: Mutex<Inner>,
}

impl StoredReminders {
    pub(super) fn open(context: PluginContext) -> ReminderResult<Self> {
        let mut ledger = match context
            .state_get(REMINDER_STATE_KEY)
            .map_err(|_| ReminderError::Storage)?
        {
            None => Ledger {
                format_version: FORMAT_VERSION,
                reminders: vec![],
            },
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(ReminderError::CorruptState);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| ReminderError::CorruptState)?;
                if value.get("format_version").and_then(|value| value.as_u64())
                    != Some(u64::from(FORMAT_VERSION))
                {
                    return Err(if value.get("format_version").is_some() {
                        ReminderError::UnsupportedVersion
                    } else {
                        ReminderError::CorruptState
                    });
                }
                let ledger: Ledger =
                    serde_json::from_value(value).map_err(|_| ReminderError::CorruptState)?;
                validate_ledger(&ledger).map_err(|_| ReminderError::CorruptState)?;
                ledger
            }
        };
        let mut changed = false;
        for reminder in &mut ledger.reminders {
            // 发送前后进程退出：是否送达不确定，不重发，也不当作已送达。
            if reminder.status == ReminderStatus::Delivering {
                if let Some(attempt) = reminder.attempts.last_mut() {
                    attempt.result = Some(DeliveryResult::Unknown);
                }
                reminder.status = ReminderStatus::Unknown;
                changed = true;
            }
        }
        if changed {
            let bytes = encode(&ledger)?;
            context
                .state_set(REMINDER_STATE_KEY, bytes)
                .map_err(|_| ReminderError::Storage)?;
        }
        Ok(Self {
            inner: Mutex::new(Inner {
                ledger,
                context: Some(context),
            }),
        })
    }

    fn lock(&self) -> ReminderResult<MutexGuard<'_, Inner>> {
        let inner = self.inner.lock().map_err(|_| ReminderError::Unavailable)?;
        if inner.context.is_none() {
            return Err(ReminderError::Unavailable);
        }
        Ok(inner)
    }

    pub(super) fn snapshot(&self) -> ReminderResult<ReminderSnapshot> {
        Ok(ReminderSnapshot {
            reminders: self.lock()?.ledger.reminders.clone(),
        })
    }

    pub(super) fn create(
        &self,
        owner: &str,
        text: &str,
        due_at_ms: u64,
        source_message_id: &str,
        now_ms: u64,
    ) -> ReminderResult<Reminder> {
        validate_id(owner)?;
        validate_id(source_message_id)?;
        validate_text(text)?;
        if now_ms == 0 {
            return Err(ReminderError::InvalidInput);
        }
        let id = reminder_id(owner, source_message_id);
        let mut inner = self.lock()?;
        // 渠道重复投递同一消息：返回原记录，不再校验时间。
        if let Some(existing) = inner.ledger.reminders.iter().find(|entry| entry.id == id) {
            return Ok(existing.clone());
        }
        validate_due(due_at_ms, now_ms)?;
        let scheduled = inner
            .ledger
            .reminders
            .iter()
            .filter(|entry| entry.owner == owner && entry.status == ReminderStatus::Scheduled)
            .count();
        if scheduled >= MAX_SCHEDULED_PER_OWNER {
            return Err(ReminderError::LimitReached);
        }
        let mut next = inner.ledger.clone();
        // 满了时腾出最早结束的一条；等待中、发送中与结果未知的记录从不删除。
        if next.reminders.len() >= MAX_REMINDERS {
            let oldest = next
                .reminders
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| entry.closed_at_ms.map(|at| (at, index)))
                .min()
                .ok_or(ReminderError::LimitReached)?;
            next.reminders.remove(oldest.1);
        }
        let reminder = Reminder {
            id,
            owner: owner.into(),
            text: text.into(),
            due_at_ms,
            created_at_ms: now_ms,
            source_message_id: source_message_id.into(),
            status: ReminderStatus::Scheduled,
            attempts: vec![],
            delivered_at_ms: None,
            closed_at_ms: None,
        };
        next.reminders.push(reminder.clone());
        persist(&mut inner, next)?;
        Ok(reminder)
    }

    pub(super) fn cancel(&self, owner: &str, id: &str, now_ms: u64) -> ReminderResult<Reminder> {
        self.update(id, now_ms, |entry| {
            // 其他用户的提醒如同不存在。
            if entry.owner != owner {
                return Err(ReminderError::NotFound);
            }
            if entry.status != ReminderStatus::Scheduled {
                return Err(ReminderError::InvalidTransition);
            }
            entry.status = ReminderStatus::Cancelled;
            entry.closed_at_ms = Some(now_ms.max(last_time(entry)));
            Ok(())
        })
    }

    pub(super) fn claim(
        &self,
        id: &str,
        channel: DeliveryChannel,
        now_ms: u64,
    ) -> ReminderResult<Reminder> {
        validate_channel(&channel)?;
        self.update(id, now_ms, |entry| {
            if !entry.due(now_ms)
                || entry.attempts.len() >= MAX_ATTEMPTS
                || (channel == DeliveryChannel::Proactive && entry.proactive_attempts() > 0)
                || matches!(&channel, DeliveryChannel::Passive { message_id } if entry.carried_by(message_id))
            {
                return Err(ReminderError::InvalidTransition);
            }
            entry.attempts.push(DeliveryAttempt {
                channel,
                started_at_ms: now_ms.max(last_time(entry)),
                finished_at_ms: None,
                result: None,
            });
            entry.status = ReminderStatus::Delivering;
            Ok(())
        })
    }

    pub(super) fn record_delivery(
        &self,
        id: &str,
        result: DeliveryResult,
        now_ms: u64,
    ) -> ReminderResult<Reminder> {
        validate_result(&result)?;
        self.update(id, now_ms, |entry| {
            if entry.status != ReminderStatus::Delivering {
                return Err(ReminderError::InvalidTransition);
            }
            let at = now_ms.max(last_time(entry));
            let attempts = entry.attempts.len();
            let attempt = entry
                .attempts
                .last_mut()
                .filter(|attempt| attempt.result.is_none())
                .ok_or(ReminderError::InvalidTransition)?;
            attempt.finished_at_ms = if result == DeliveryResult::Unknown {
                None
            } else {
                Some(at)
            };
            attempt.result = Some(result.clone());
            match result {
                DeliveryResult::Unknown => {
                    entry.status = ReminderStatus::Unknown;
                }
                DeliveryResult::Sent { .. } => {
                    entry.status = ReminderStatus::Delivered;
                    entry.delivered_at_ms = Some(at);
                    entry.closed_at_ms = Some(at);
                }
                _ if attempts >= MAX_ATTEMPTS => {
                    entry.status = ReminderStatus::Expired;
                    entry.closed_at_ms = Some(at);
                }
                _ => entry.status = ReminderStatus::Scheduled,
            }
            Ok(())
        })
    }

    pub(super) fn expire(&self, id: &str, now_ms: u64) -> ReminderResult<Reminder> {
        self.update(id, now_ms, |entry| {
            if entry.status != ReminderStatus::Scheduled
                || entry.due_at_ms > now_ms
                || now_ms - entry.due_at_ms <= DELIVERY_WINDOW_MS
            {
                return Err(ReminderError::InvalidTransition);
            }
            entry.status = ReminderStatus::Expired;
            entry.closed_at_ms = Some(now_ms.max(last_time(entry)));
            Ok(())
        })
    }

    fn update(
        &self,
        id: &str,
        now_ms: u64,
        apply: impl FnOnce(&mut Reminder) -> ReminderResult<()>,
    ) -> ReminderResult<Reminder> {
        if now_ms == 0 {
            return Err(ReminderError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let position = next
            .reminders
            .iter()
            .position(|entry| entry.id == id)
            .ok_or(ReminderError::NotFound)?;
        let mut entry = next.reminders[position].clone();
        apply(&mut entry)?;
        next.reminders[position] = entry.clone();
        persist(&mut inner, next)?;
        Ok(entry)
    }

    pub(super) fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("提醒状态锁不可用".into()))?
            .context = None;
        Ok(())
    }
}

/// 提醒上最近一次已记录的时间；之后的记录不能更早。
fn last_time(entry: &Reminder) -> u64 {
    entry
        .attempts
        .iter()
        .flat_map(|attempt| [Some(attempt.started_at_ms), attempt.finished_at_ms])
        .flatten()
        .chain([entry.created_at_ms])
        .max()
        .unwrap_or(0)
}

fn validate_channel(channel: &DeliveryChannel) -> ReminderResult<()> {
    match channel {
        DeliveryChannel::Passive { message_id } => validate_id(message_id),
        DeliveryChannel::Proactive => Ok(()),
    }
}

fn validate_result(result: &DeliveryResult) -> ReminderResult<()> {
    match result {
        DeliveryResult::Sent {
            platform_message_id: Some(id),
        } => validate_id(id),
        _ => Ok(()),
    }
}

fn persist(inner: &mut Inner, next: Ledger) -> ReminderResult<()> {
    let bytes = encode(&next)?;
    let context = inner.context.as_ref().ok_or(ReminderError::Unavailable)?;
    // 失败也可能已经提交；关闭整个实例，禁止旧缓存继续读取或覆盖后端。
    if context.state_set(REMINDER_STATE_KEY, bytes).is_err() {
        inner.context = None;
        return Err(ReminderError::Storage);
    }
    inner.ledger = next;
    Ok(())
}

fn encode(ledger: &Ledger) -> ReminderResult<Vec<u8>> {
    if ledger.reminders.len() > MAX_REMINDERS {
        return Err(ReminderError::LimitReached);
    }
    let bytes = serde_json::to_vec(ledger).map_err(|_| ReminderError::InvalidInput)?;
    // 重启时写入的未知结局比原状态略长；预留空间保证能保存。
    let open = ledger
        .reminders
        .iter()
        .filter(|entry| entry.status == ReminderStatus::Delivering)
        .count();
    if bytes.len().saturating_add(open * 32) > MAX_STATE_BYTES {
        return Err(ReminderError::LimitReached);
    }
    Ok(bytes)
}

/// 启动时完整核对；任何不一致都拒绝打开并保留原字节。
fn validate_ledger(ledger: &Ledger) -> ReminderResult<()> {
    if ledger.reminders.len() > MAX_REMINDERS {
        return Err(ReminderError::CorruptState);
    }
    let mut ids = BTreeSet::new();
    for entry in &ledger.reminders {
        validate_reminder(entry)?;
        if !ids.insert(entry.id.as_str()) {
            return Err(ReminderError::CorruptState);
        }
    }
    Ok(())
}

fn validate_reminder(entry: &Reminder) -> ReminderResult<()> {
    for id in [&entry.id, &entry.owner, &entry.source_message_id] {
        validate_id(id).map_err(|_| ReminderError::CorruptState)?;
    }
    validate_text(&entry.text).map_err(|_| ReminderError::CorruptState)?;
    if entry.id != reminder_id(&entry.owner, &entry.source_message_id)
        || entry.created_at_ms == 0
        || entry.due_at_ms < entry.created_at_ms.saturating_add(MIN_LEAD_MS)
        || entry.due_at_ms > entry.created_at_ms.saturating_add(MAX_HORIZON_MS)
        || entry.attempts.len() > MAX_ATTEMPTS
        || entry
            .attempts
            .iter()
            .filter(|attempt| attempt.channel == DeliveryChannel::Proactive)
            .count()
            > 1
    {
        return Err(ReminderError::CorruptState);
    }
    let mut carried = BTreeSet::new();
    for (index, attempt) in entry.attempts.iter().enumerate() {
        validate_channel(&attempt.channel).map_err(|_| ReminderError::CorruptState)?;
        if let Some(result) = &attempt.result {
            validate_result(result).map_err(|_| ReminderError::CorruptState)?;
        }
        let last = index + 1 == entry.attempts.len();
        if let DeliveryChannel::Passive { message_id } = &attempt.channel
            && !carried.insert(message_id.as_str())
        {
            return Err(ReminderError::CorruptState);
        }
        if attempt.started_at_ms < entry.due_at_ms
            || (attempt.result.is_none() && !last)
            || (!last && matches!(attempt.result, Some(DeliveryResult::Sent { .. })))
            || (!last && attempt.result == Some(DeliveryResult::Unknown))
            || attempt
                .finished_at_ms
                .is_some_and(|at| at < attempt.started_at_ms)
        {
            return Err(ReminderError::CorruptState);
        }
    }
    let last_result = entry.attempts.last().map(|attempt| &attempt.result);
    let sent = matches!(last_result, Some(Some(DeliveryResult::Sent { .. })));
    let consistent = match entry.status {
        ReminderStatus::Scheduled => {
            !sent
                && !matches!(
                    last_result,
                    Some(None) | Some(Some(DeliveryResult::Unknown))
                )
                && entry.closed_at_ms.is_none()
        }
        ReminderStatus::Delivering => {
            matches!(last_result, Some(None)) && entry.closed_at_ms.is_none()
        }
        ReminderStatus::Delivered => sent && entry.closed_at_ms.is_some(),
        ReminderStatus::Unknown => {
            matches!(last_result, Some(Some(DeliveryResult::Unknown)))
                && entry.closed_at_ms.is_none()
        }
        ReminderStatus::Cancelled | ReminderStatus::Expired => {
            !sent
                && !matches!(
                    last_result,
                    Some(None) | Some(Some(DeliveryResult::Unknown))
                )
                && entry.closed_at_ms.is_some()
        }
    };
    if !consistent || entry.delivered_at_ms.is_some() != (entry.status == ReminderStatus::Delivered)
    {
        return Err(ReminderError::CorruptState);
    }
    Ok(())
}
