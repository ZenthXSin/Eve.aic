//! 长期推进状态仅接受可信宿主从既有账本汇总的事实，不把模型判断当执行授权。
use serde::{Deserialize, Serialize};

pub const PROGRESS_SCHEMA: &str = "pursuit-progress:v1";
pub const DEFAULT_STALL_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressRecord {
    pub schema: String,
    pub goal_id: String,
    pub goal_revision: u64,
    /// 单调增加的外部证据版本；QQ 使用真实陈述数量，不能用模型推断修订替代。
    pub evidence_revision: u64,
    pub last_progress_at_ms: u64,
    pub paused: bool,
    /// 仅记录宿主原有的等待标记，恢复前由原派生器同步新证据，不修改执行预算。
    pub waiting_marker: String,
}

/// 时间和修订均来自持久化账本。新证据必须属于同一目标和拥有者。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgressFacts {
    pub goal_id: String,
    pub goal_revision: u64,
    /// 单调增加的外部证据版本；QQ 使用真实陈述数量，不能用模型推断修订替代。
    pub evidence_revision: u64,
    pub last_activity_at_ms: u64,
    pub waiting_for_new_input: bool,
    /// 只在仍有至少一次可用能力，且未运行、未等待用户、未未知提交时为真。
    pub can_resume: bool,
    pub now_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgressOutcome {
    Unchanged,
    Observed,
    Paused,
    Resumed,
    Deferred,
}
