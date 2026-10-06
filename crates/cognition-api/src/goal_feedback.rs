//! 用户提供的新事实或纠正，与执行器产生的 Goal.feedback 分离。
use crate::{CognitionError, CognitionResult, validate_id, validate_text};
use serde::{Deserialize, Serialize};
use std::{fmt, sync::Arc};

pub const MAX_GOAL_FEEDBACK_BYTES: usize = 4096;
pub const GOAL_USER_FEEDBACK_SCHEMA_VERSION: u32 = 1;

/// 用户和来源通道由宿主绑定到服务；调用方不能自报权限。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalFeedbackInput {
    pub goal_id: String,
    pub expected_goal_revision: u64,
    /// 宿主从路由与消息 ID 派生的稳定、有界标识，也是持久化事件 ID。
    pub feedback_id: String,
    pub text: String,
    pub at_ms: u64,
}
impl GoalFeedbackInput {
    pub fn validate(&self) -> CognitionResult<()> {
        validate_id(&self.goal_id)?;
        validate_id(&self.feedback_id)?;
        validate_feedback_text(&self.text)?;
        if self.expected_goal_revision == 0 || self.at_ms == 0 {
            return Err(CognitionError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalFeedbackReport {
    pub goal_id: String,
    /// 重复输入返回其原始提交后的目标修订；revision 是读取到的当前全局修订。
    pub goal_revision: u64,
    pub revision: u64,
    pub duplicate: bool,
}

pub trait GoalFeedbackService: Send + Sync {
    /// 一次 CAS 原子提交最新事实与来源事件；失败不会隐式重试或提升执行预算。
    /// 相同 feedback_id/目标/正文先作幂等判断；expected_goal_revision 仅是新提交的
    /// CAS 前置条件，和 at_ms 一样不参与已保存消息的重送比较。
    fn submit(&self, input: GoalFeedbackInput) -> CognitionResult<GoalFeedbackReport>;
}
#[derive(Clone)]
pub struct GoalFeedbackServiceHandle(pub Arc<dyn GoalFeedbackService>);

/// 用户反馈事件 summary 与 Waiting 父目标 wait_reason 的同一有界 JSON。
/// 这是用户提供的数据，不能当作工具回执、已验证事实或执行权限。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalUserFeedback {
    pub schema_version: u32,
    pub goal_id: String,
    pub previous_goal_revision: u64,
    pub goal_revision: u64,
    pub feedback_id: String,
    pub text: String,
}
impl GoalUserFeedback {
    pub fn validate(&self) -> CognitionResult<()> {
        validate_id(&self.goal_id)?;
        validate_id(&self.feedback_id)?;
        validate_feedback_text(&self.text)?;
        if self.schema_version != GOAL_USER_FEEDBACK_SCHEMA_VERSION
            || self.previous_goal_revision == 0
            || self.previous_goal_revision.checked_add(1) != Some(self.goal_revision)
        {
            return Err(CognitionError::InvalidInput);
        }
        Ok(())
    }
    pub fn parse(text: &str) -> CognitionResult<Self> {
        validate_text(text)?;
        let parsed: Self = serde_json::from_str(text).map_err(|_| CognitionError::InvalidInput)?;
        parsed.validate()?;
        Ok(parsed)
    }
    pub fn to_json(&self) -> CognitionResult<String> {
        self.validate()?;
        let text = serde_json::to_string(self).map_err(|_| CognitionError::InvalidInput)?;
        validate_text(&text)?;
        Ok(text)
    }
}

fn validate_feedback_text(text: &str) -> CognitionResult<()> {
    validate_text(text)?;
    if text.len() > MAX_GOAL_FEEDBACK_BYTES {
        return Err(CognitionError::InvalidInput);
    }
    Ok(())
}
macro_rules! redacted {
    ($($ty:ty),+) => { $(impl fmt::Debug for $ty {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(concat!(stringify!($ty), "(<redacted>)"))
        }
    })+ };
}
redacted!(GoalFeedbackInput, GoalFeedbackReport, GoalUserFeedback);

#[cfg(test)]
mod tests {
    use super::*;

    fn value() -> GoalUserFeedback {
        GoalUserFeedback {
            schema_version: 1,
            goal_id: "goal".into(),
            previous_goal_revision: 1,
            goal_revision: 2,
            feedback_id: "event".into(),
            text: "用户原文 📚".into(),
        }
    }
    #[test]
    fn strict_feedback_contract_preserves_utf8_and_rejects_forged_versions_or_ranges() {
        let value = value();
        let json = value.to_json().unwrap();
        assert_eq!(GoalUserFeedback::parse(&json).unwrap(), value);
        assert!(
            GoalUserFeedback::parse(&json.replacen(
                "\"schema_version\":1",
                "\"schema_version\":1,\"schema_version\":1",
                1
            ))
            .is_err()
        );
        assert!(
            GoalUserFeedback::parse(&json.replacen(
                "\"goal_revision\":2",
                "\"goal_revision\":3",
                1
            ))
            .is_err()
        );
        assert!(
            GoalUserFeedback::parse(&json.replacen(
                "\"schema_version\":1",
                "\"schema_version\":2",
                1
            ))
            .is_err()
        );
        let mut oversized = value.clone();
        oversized.text = "x".repeat(MAX_GOAL_FEEDBACK_BYTES + 1);
        assert!(oversized.to_json().is_err());
        let mut escaped = value;
        escaped.text = "\u{0001}".repeat(MAX_GOAL_FEEDBACK_BYTES);
        assert!(escaped.to_json().is_err());
    }
}
