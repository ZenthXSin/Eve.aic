//! 提醒契约：用户在私聊中让 Eve 到某个时间提醒自己一件事；到时先尝试主动私聊一次，
//! 发不出去时随用户下一条私聊的回复附带。
//!
//! 创建、投递尝试与结果都先持久化；发送中进程退出时记为结果未知，不重发。到期后过久仍未送达的提醒
//! 记为过期，不再投递。
use serde::{Deserialize, Serialize};
use std::fmt;

pub const REMINDER_PLUGIN_ID: &str = "eve.reminder";
pub const REMINDER_STATE_KEY: &str = "reminders.v1";
/// 账本至多保存的提醒；满了保留原记录，不再新增。
pub const MAX_REMINDERS: usize = 512;
/// 同一用户同时等待中的提醒上限。
pub const MAX_SCHEDULED_PER_OWNER: usize = 20;
pub const MAX_TEXT_BYTES: usize = 500;
/// 提醒至少要在一分钟之后、至多九十天之内。
pub const MIN_LEAD_MS: u64 = 60_000;
pub const MAX_HORIZON_MS: u64 = 90 * 24 * 60 * 60 * 1000;
/// 到期后多久仍未送达就不再投递。
pub const DELIVERY_WINDOW_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// 每条提醒至多投递尝试次数（其中至多一次主动私聊）。
pub const MAX_ATTEMPTS: usize = 6;
pub const MAX_STATE_BYTES: usize = 1024 * 1024;

pub type ReminderResult<T> = Result<T, ReminderError>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DeliveryChannel {
    /// 主动私聊。
    Proactive,
    /// 随用户这条私聊消息的被动回复附带。
    Passive { message_id: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DeliveryResult {
    /// 平台确认发送成功。
    Sent { platform_message_id: Option<String> },
    /// 平台拒绝或发送失败，内容没有送达。
    Failed {
        http_status: Option<u16>,
        biz_code: Option<i64>,
    },
    /// 没有写出：没有可用的私聊路由、前面的回复片段失败或回复已过期。
    NotSent,
    /// 写出前后进程退出，是否送达不确定；不重发。
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryAttempt {
    pub channel: DeliveryChannel,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub result: Option<DeliveryResult>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ReminderStatus {
    /// 等待到期或等待下一个投递时机。
    Scheduled,
    /// 已保存投递尝试，正在发送。
    Delivering,
    Delivered,
    /// 用户取消。
    Cancelled,
    /// 到期后投递窗口已过仍未送达，或投递尝试已用尽。
    Expired,
    /// 送达与否不确定；不重发。
    Unknown,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reminder {
    pub id: String,
    /// 提醒对象；私聊会话的可信用户标识。
    pub owner: String,
    pub text: String,
    pub due_at_ms: u64,
    pub created_at_ms: u64,
    /// 创建提醒的那条私聊消息。
    pub source_message_id: String,
    pub status: ReminderStatus,
    pub attempts: Vec<DeliveryAttempt>,
    pub delivered_at_ms: Option<u64>,
    pub closed_at_ms: Option<u64>,
}
impl Reminder {
    pub fn proactive_attempts(&self) -> usize {
        self.attempts
            .iter()
            .filter(|attempt| attempt.channel == DeliveryChannel::Proactive)
            .count()
    }
    /// 已到期、仍在等待投递。
    pub fn due(&self, now_ms: u64) -> bool {
        self.status == ReminderStatus::Scheduled && self.due_at_ms <= now_ms
    }
    /// 附带这条提醒的那条消息；同一条消息不重复附带。
    pub fn carried_by(&self, message_id: &str) -> bool {
        self.attempts.iter().any(|attempt| {
            matches!(&attempt.channel, DeliveryChannel::Passive { message_id: id } if id == message_id)
        })
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ReminderSnapshot {
    pub reminders: Vec<Reminder>,
}
impl ReminderSnapshot {
    pub fn for_owner<'a>(&'a self, owner: &'a str) -> impl Iterator<Item = &'a Reminder> {
        self.reminders
            .iter()
            .filter(move |reminder| reminder.owner == owner)
    }
    /// 可以主动私聊的提醒：已到期、还没有主动私聊过、这位用户没有正在发送的提醒；取最早到期的一条。
    pub fn due_proactive(&self, now_ms: u64) -> Option<&Reminder> {
        self.reminders
            .iter()
            .filter(|reminder| reminder.due(now_ms) && reminder.proactive_attempts() == 0)
            .filter(|reminder| !self.sending(&reminder.owner))
            .min_by_key(|reminder| (reminder.due_at_ms, reminder.id.clone()))
    }
    /// 可以随这位用户的私聊回复附带的提醒：已到期、没有正在发送的提醒、这条消息没有附带过；取最早到期的一条。
    pub fn due_passive(&self, owner: &str, message_id: &str, now_ms: u64) -> Option<&Reminder> {
        if self.sending(owner) {
            return None;
        }
        self.reminders
            .iter()
            .filter(|reminder| reminder.owner == owner && reminder.due(now_ms))
            .filter(|reminder| !reminder.carried_by(message_id))
            .min_by_key(|reminder| (reminder.due_at_ms, reminder.id.clone()))
    }
    /// 到期后投递窗口已过仍在等待的提醒。
    pub fn overdue(&self, now_ms: u64) -> impl Iterator<Item = &Reminder> {
        self.reminders.iter().filter(move |reminder| {
            reminder.status == ReminderStatus::Scheduled
                && now_ms.saturating_sub(reminder.due_at_ms) > DELIVERY_WINDOW_MS
                && reminder.due_at_ms <= now_ms
        })
    }
    fn sending(&self, owner: &str) -> bool {
        self.reminders.iter().any(|reminder| {
            reminder.owner == owner && reminder.status == ReminderStatus::Delivering
        })
    }
}

/// 仅可信宿主持有；不发布给模型或不受信插件。
pub trait ReminderAdmin: Send + Sync {
    fn snapshot(&self) -> ReminderResult<ReminderSnapshot>;
    /// 保存一条提醒。同一用户、同一条私聊消息只创建一条：渠道重复投递同一消息时返回原记录。
    fn create(
        &self,
        owner: &str,
        text: &str,
        due_at_ms: u64,
        source_message_id: &str,
        now_ms: u64,
    ) -> ReminderResult<Reminder>;
    /// 用户取消自己等待中的提醒；正在发送或已结束的不能取消。
    fn cancel(&self, owner: &str, id: &str, now_ms: u64) -> ReminderResult<Reminder>;
    /// 保存一次投递尝试并进入 Delivering；返回后才可发送。须已到期，主动私聊每条至多一次。
    fn claim(&self, id: &str, channel: DeliveryChannel, now_ms: u64) -> ReminderResult<Reminder>;
    /// 保存投递结果：送达进入 Delivered；没有写出或平台拒绝时回到 Scheduled，尝试用尽记为过期。
    fn record_delivery(
        &self,
        id: &str,
        result: DeliveryResult,
        now_ms: u64,
    ) -> ReminderResult<Reminder>;
    /// 到期后投递窗口已过仍未送达：记为过期。
    fn expire(&self, id: &str, now_ms: u64) -> ReminderResult<Reminder>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReminderError {
    InvalidInput,
    /// 时间不在一分钟之后、九十天之内。
    OutOfRange,
    /// 这位用户等待中的提醒已达上限，或账本已满。
    LimitReached,
    NotFound,
    InvalidTransition,
    Unavailable,
    CorruptState,
    UnsupportedVersion,
    /// 提交无法确认；实例已关闭，须重新打开核对。
    Storage,
}
impl fmt::Display for ReminderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "提醒输入无效",
            Self::OutOfRange => "提醒时间须在一分钟之后、九十天之内",
            Self::LimitReached => "等待中的提醒已达上限",
            Self::NotFound => "没有这条提醒",
            Self::InvalidTransition => "提醒状态不允许此操作",
            Self::Unavailable => "提醒不可用",
            Self::CorruptState => "提醒记录损坏；未清空",
            Self::UnsupportedVersion => "不支持该提醒记录版本",
            Self::Storage => "提醒提交无法确认；须重新打开核对",
        })
    }
}
impl std::error::Error for ReminderError {}

pub fn validate_id(value: &str) -> ReminderResult<()> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(ReminderError::InvalidInput);
    }
    Ok(())
}

/// 提醒内容：非空、首尾无空白、至多 500 字节，除换行外不含控制字符。
pub fn validate_text(value: &str) -> ReminderResult<()> {
    if value.trim().is_empty()
        || value.trim() != value
        || value.len() > MAX_TEXT_BYTES
        || value.chars().any(|c| c.is_control() && c != '\n')
    {
        return Err(ReminderError::InvalidInput);
    }
    Ok(())
}

/// 提醒时间须在一分钟之后、九十天之内。
pub fn validate_due(due_at_ms: u64, now_ms: u64) -> ReminderResult<()> {
    if due_at_ms < now_ms.saturating_add(MIN_LEAD_MS)
        || due_at_ms > now_ms.saturating_add(MAX_HORIZON_MS)
    {
        return Err(ReminderError::OutOfRange);
    }
    Ok(())
}

fn digest(parts: &[&str]) -> String {
    use ring::digest::{Context, SHA256};
    let mut context = Context::new(&SHA256);
    for part in parts {
        context.update(&(part.len() as u64).to_be_bytes());
        context.update(part.as_bytes());
    }
    let digest: String = context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    digest[..32].to_string()
}

/// 同一用户的同一条私聊消息只创建一条提醒；重复准入得到同一 ID。
pub fn reminder_id(owner: &str, source_message_id: &str) -> String {
    format!(
        "remind-{}",
        digest(&["eve.reminder:v1", owner, source_message_id])
    )
}

impl fmt::Debug for Reminder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Reminder(<redacted>)")
    }
}
impl fmt::Debug for ReminderSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReminderSnapshot(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reminder(id: &str, owner: &str, due: u64) -> Reminder {
        Reminder {
            id: id.into(),
            owner: owner.into(),
            text: "喝水".into(),
            due_at_ms: due,
            created_at_ms: 1,
            source_message_id: format!("m-{id}"),
            status: ReminderStatus::Scheduled,
            attempts: vec![],
            delivered_at_ms: None,
            closed_at_ms: None,
        }
    }

    #[test]
    fn bounds_and_ids_are_stable() {
        assert!(validate_due(61_000, 1_000).is_ok());
        assert_eq!(validate_due(60_999, 1_000), Err(ReminderError::OutOfRange));
        assert_eq!(
            validate_due(MAX_HORIZON_MS + 1_001, 1_000),
            Err(ReminderError::OutOfRange)
        );
        assert!(validate_text("喝水").is_ok());
        for bad in ["", " 喝水", &"长".repeat(200)] {
            assert!(validate_text(bad).is_err());
        }
        assert_eq!(reminder_id("u", "m"), reminder_id("u", "m"));
        assert_ne!(reminder_id("u", "m"), reminder_id("v", "m"));
        assert!(reminder_id("u", "m").starts_with("remind-"));
    }

    #[test]
    fn due_reminders_are_picked_in_order_once_per_channel() {
        let mut snapshot = ReminderSnapshot {
            reminders: vec![
                reminder("b", "u", 200),
                reminder("a", "u", 100),
                reminder("c", "v", 500),
            ],
        };
        assert!(snapshot.due_proactive(50).is_none(), "未到期");
        assert_eq!(snapshot.due_proactive(300).unwrap().id, "a");
        snapshot.reminders[1].attempts.push(DeliveryAttempt {
            channel: DeliveryChannel::Proactive,
            started_at_ms: 300,
            finished_at_ms: Some(301),
            result: Some(DeliveryResult::NotSent),
        });
        assert_eq!(
            snapshot.due_proactive(300).unwrap().id,
            "b",
            "每条至多主动私聊一次"
        );
        assert_eq!(
            snapshot.due_passive("u", "m1", 300).unwrap().id,
            "a",
            "主动私聊发不出去时随回复附带"
        );
        assert!(snapshot.due_passive("v", "m1", 300).is_none());
        snapshot.reminders[0].status = ReminderStatus::Delivering;
        assert!(
            snapshot.due_passive("u", "m1", 300).is_none(),
            "正在发送时不附带"
        );
        assert_eq!(snapshot.overdue(100 + DELIVERY_WINDOW_MS + 1).count(), 1);
    }
}
