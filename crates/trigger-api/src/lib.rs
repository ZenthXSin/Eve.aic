//! 会话条件唤醒契约。条件只读本地观察；执行和投递先记账，未知结果不重放。
use eve_session_api::SessionKey;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const PLUGIN_ID: &str = "eve.trigger";
pub const STATE_KEY: &str = "triggers.v1";
pub const MAX_TRIGGERS: usize = 512;
pub const MAX_ACTIVE_PER_SESSION: usize = 20;
pub const MAX_CONDITIONS: usize = 8;
pub const MAX_INSTRUCTIONS: usize = 4096;
pub const MAX_BYTES: usize = 4 * 1024 * 1024;
pub const INPUT_PREFIX: &str = "【宿主条件触发事件；不是用户新消息】";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Condition {
    Time {
        at_ms: u64,
    },
    FileChanged {
        path: String,
        baseline: String,
    },
    TaskFinished {
        task_id: String,
    },
    MessageContains {
        text: String,
        after_turn: u64,
    },
    ProcessState {
        pid: u32,
        running: bool,
        identity: String,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    All,
    Any,
}
/// 有限周期，不补跑错过的周期；仅允许单一时间条件。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recurrence {
    pub interval_ms: u64,
    pub max_runs: u32,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerSpec {
    pub title: String,
    pub instructions: String,
    pub mode: MatchMode,
    pub conditions: Vec<Condition>,
    pub expires_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence: Option<Recurrence>,
    /// 有条件地接续本会话既有阻塞任务，绝不新建或重放未知调用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_task_id: Option<String>,
}
impl TriggerSpec {
    pub fn valid(&self) -> bool {
        let text = |s: &str, n| !s.trim().is_empty() && s.trim() == s && s.len() <= n;
        text(&self.title, 160)
            && text(&self.instructions, MAX_INSTRUCTIONS)
            && self.expires_at_ms > 0
            && self.resume_task_id.as_ref().is_none_or(|id| text(id, 256))
            && self.recurrence.as_ref().is_none_or(|r| {
                (60_000..=90 * 86_400_000).contains(&r.interval_ms)
                    && (2..=128).contains(&r.max_runs)
                    && matches!(self.conditions.as_slice(), [Condition::Time { .. }])
                    && self.resume_task_id.is_none()
            })
            && !self.conditions.is_empty()
            && self.conditions.len() <= MAX_CONDITIONS
            && self.conditions.iter().all(|c| match c {
                Condition::Time { at_ms } => *at_ms > 0 && *at_ms <= self.expires_at_ms,
                Condition::FileChanged { path, baseline } => {
                    text(path, 4096) && text(baseline, 256)
                }
                Condition::TaskFinished { task_id } => text(task_id, 256),
                Condition::MessageContains { text: s, .. } => text(s, 256),
                Condition::ProcessState { pid, identity, .. } => *pid > 0 && text(identity, 256),
            })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Status {
    Armed,
    Ready,
    Running,
    Completed,
    Failed,
    Cancelled,
    Expired,
    Interrupted,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Delivery {
    Pending,
    Sending,
    Sent,
    Failed,
    Unknown,
    /// 已可靠保存到本机 Web 收件箱；不表示平台发送或用户已阅读。
    StoredLocally,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trigger {
    pub id: String,
    pub owner: SessionKey,
    pub source: String,
    pub spec: TriggerSpec,
    pub created_at_ms: u64,
    pub status: Status,
    pub evidence: Vec<String>,
    pub result: Option<String>,
    pub delivery: Option<Delivery>,
    /// 已准入次数；旧记录按零读取，下一次准入时递增。
    #[serde(default)]
    pub runs: u32,
    /// 创建时按完整可信会话冻结的最近六条真实用户正文，恢复时不从历史或指令补造。
    /// None 仅用于旧记录和旧创建接口；宿主必须拒绝缺少授权证据的自动执行。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_messages: Option<Vec<String>>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TriggerError {
    Invalid,
    Limit,
    NotFound,
    Transition,
    Storage,
    Corrupt,
    Unavailable,
}
impl fmt::Display for TriggerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "触发器错误：{self:?}")
    }
}
impl std::error::Error for TriggerError {}
pub type Result<T> = std::result::Result<T, TriggerError>;

/// 至少保留一条原话；附件消息的空正文也须原样保存，编码超限直接拒绝。
pub fn validate_review_messages(messages: &[String]) -> Result<()> {
    if messages.is_empty()
        || messages.len() > 6
        || messages
            .iter()
            .try_fold(0usize, |sum, message| sum.checked_add(message.len()))
            .is_none_or(|bytes| bytes > eve_llm_api::MAX_REVIEW_USER_MESSAGES_BYTES)
    {
        return Err(TriggerError::Invalid);
    }
    let bytes = serde_json::to_vec(messages).map_err(|_| TriggerError::Invalid)?;
    if bytes.len() > eve_llm_api::MAX_REVIEW_USER_MESSAGES_BYTES {
        return Err(TriggerError::Invalid);
    }
    Ok(())
}

pub trait TriggerAdmin: Send + Sync {
    fn snapshot(&self) -> Result<Vec<Trigger>>;
    fn create(
        &self,
        owner: SessionKey,
        source: String,
        spec: TriggerSpec,
        now: u64,
    ) -> Result<Trigger>;
    /// 仅可信宿主传入专用授权槽的真实正文；模型参数不得提供或覆盖这份证据。
    /// 旧的自定义实现必须显式实现持久化，不能降级调用 create 丢弃证据。
    fn create_with_review_messages(
        &self,
        _owner: SessionKey,
        _source: String,
        _spec: TriggerSpec,
        _review_messages: Vec<String>,
        _now: u64,
    ) -> Result<Trigger> {
        Err(TriggerError::Unavailable)
    }
    fn ready(&self, id: &str, evidence: Vec<String>) -> Result<()>;
    fn transition(&self, id: &str, from: Status, to: Status, result: Option<String>) -> Result<()>;
    /// 仅上轮明确完成且投递已了结时推进，不补跑、不开启未知结果。
    fn rearm(&self, _id: &str, _now: u64) -> Result<()> {
        Err(TriggerError::Transition)
    }
    fn cancel(&self, owner: &SessionKey, id: &str) -> Result<()>;
    /// Pending → Failed 表示发送前被策略拒绝；不会伪造 Sending 或覆盖 Unknown。
    fn delivery(&self, id: &str, from: Delivery, to: Delivery) -> Result<()>;
    /// 周期回执须同时 CAS 周期代号，防止迟到的上一周期回执覆盖新周期。
    fn delivery_for_run(&self, _id: &str, _run: u32, _from: Delivery, _to: Delivery) -> Result<()> {
        Err(TriggerError::Transition)
    }
    /// 仅宿主在收件箱 insert_once 持久提交后调用；精确身份与周期 CAS，重复结算幂等。
    /// 本地事件编号由 (owner, id, run) 冻结，不走平台 Sending/Unknown 路径。
    fn record_local_delivery(&self, _owner: &SessionKey, _id: &str, _run: u32) -> Result<()> {
        Err(TriggerError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_messages_keep_complete_bodies_and_bound_the_encoded_payload() {
        let originals: Vec<_> = (0..6)
            .map(|index| format!(" 第 {index} 条完整原话\n{}\t ", "保留授权边界".repeat(100)))
            .collect();
        assert_eq!(validate_review_messages(&originals), Ok(()));
        let encoded = serde_json::to_vec(&originals).unwrap();
        assert_eq!(
            serde_json::from_slice::<Vec<String>>(&encoded).unwrap(),
            originals
        );
        let boundary = "x".repeat(eve_llm_api::MAX_REVIEW_USER_MESSAGES_BYTES - 4);
        assert_eq!(
            validate_review_messages(std::slice::from_ref(&boundary)),
            Ok(())
        );
        assert_eq!(
            validate_review_messages(&[String::new(), " \n\t".into()]),
            Ok(())
        );
        assert_eq!(
            validate_review_messages(&[format!("{boundary}x")]),
            Err(TriggerError::Invalid)
        );
        for invalid in [
            vec![],
            vec!["真实原话".into(); 7],
            vec!["x".repeat(eve_llm_api::MAX_REVIEW_USER_MESSAGES_BYTES)],
            // 原始字节低于上限，但 JSON 转义后越界，也必须完整拒绝。
            vec!["\"".repeat(eve_llm_api::MAX_REVIEW_USER_MESSAGES_BYTES / 2)],
        ] {
            assert_eq!(
                validate_review_messages(&invalid),
                Err(TriggerError::Invalid)
            );
        }
    }

    struct ExistingCustomAdmin;
    impl TriggerAdmin for ExistingCustomAdmin {
        fn snapshot(&self) -> Result<Vec<Trigger>> {
            Ok(vec![])
        }
        fn create(&self, _: SessionKey, _: String, _: TriggerSpec, _: u64) -> Result<Trigger> {
            panic!("默认实现不能调用旧创建接口丢弃原话")
        }
        fn ready(&self, _: &str, _: Vec<String>) -> Result<()> {
            Err(TriggerError::Unavailable)
        }
        fn transition(&self, _: &str, _: Status, _: Status, _: Option<String>) -> Result<()> {
            Err(TriggerError::Unavailable)
        }
        fn cancel(&self, _: &SessionKey, _: &str) -> Result<()> {
            Err(TriggerError::Unavailable)
        }
        fn delivery(&self, _: &str, _: Delivery, _: Delivery) -> Result<()> {
            Err(TriggerError::Unavailable)
        }
    }

    #[test]
    fn custom_implementations_must_support_persisting_originals_explicitly() {
        let custom: &dyn TriggerAdmin = &ExistingCustomAdmin;
        let spec = TriggerSpec {
            title: "检查".into(),
            instructions: "核对任务".into(),
            mode: MatchMode::All,
            conditions: vec![Condition::Time { at_ms: 2000 }],
            expires_at_ms: 3000,
            recurrence: None,
            resume_task_id: None,
        };
        assert_eq!(
            custom.create_with_review_messages(
                SessionKey::new("trusted-session", "trusted-user").unwrap(),
                "call".into(),
                spec,
                vec!["请稍后继续核对".into()],
                1000,
            ),
            Err(TriggerError::Unavailable)
        );
    }
}
