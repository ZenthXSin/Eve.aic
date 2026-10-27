//! 自主运行预算契约：后台自主工作（不是对用户消息的回复）在固定时间窗口内可发起的模型请求总数。
//!
//! 每次请求先持久化计数再发出；重启、重复事件或自造目标都不能放大一个窗口内的用量。
//! 进程在计数后、请求前退出时，该次计数不退还：预算宁可少用，不能多用。
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt};

pub const BUDGET_PLUGIN_ID: &str = "eve.budget";
pub const BUDGET_STATE_KEY: &str = "usage.v1";
pub const MIN_WINDOW_MS: u64 = 60_000;
pub const MAX_WINDOW_MS: u64 = 30 * 24 * 3_600_000;
pub const MAX_LIMIT: u32 = 1_000_000;
/// 保留的历史窗口数，供查看；更早的窗口摘要不再保存。
pub const MAX_HISTORY: usize = 14;
pub const MAX_CATEGORIES: usize = 16;
pub const MAX_CATEGORY_BYTES: usize = 32;

pub type BudgetResult<T> = Result<T, BudgetError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetError {
    /// 本窗口已用尽；`resets_at_ms` 是下一窗口开始时间。不写入任何计数。
    Exhausted {
        resets_at_ms: u64,
    },
    /// 此用途达到分项上限，其他用途仍可使用总余额。
    CategoryExhausted {
        resets_at_ms: u64,
    },
    InvalidInput,
    Unavailable,
    CorruptState,
    UnsupportedVersion,
    /// 提交无法确认；实例已关闭，须重新打开核对。
    Storage,
}
impl fmt::Display for BudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Exhausted { .. } => "自主运行预算已用尽",
            Self::CategoryExhausted { .. } => "此用途的自主运行预算已用尽",
            Self::InvalidInput => "自主运行预算输入无效",
            Self::Unavailable => "自主运行预算不可用",
            Self::CorruptState => "自主运行预算记录损坏；未清空",
            Self::UnsupportedVersion => "不支持该自主运行预算版本",
            Self::Storage => "自主运行预算提交无法确认；须重新打开核对",
        })
    }
}
impl std::error::Error for BudgetError {}

/// 窗口按 Unix 时间对齐：`start = now - now % window_ms`。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetPolicy {
    pub window_ms: u64,
    pub limit: u32,
}
impl BudgetPolicy {
    pub fn validate(&self) -> BudgetResult<()> {
        if !(MIN_WINDOW_MS..=MAX_WINDOW_MS).contains(&self.window_ms)
            || self.limit == 0
            || self.limit > MAX_LIMIT
        {
            return Err(BudgetError::InvalidInput);
        }
        Ok(())
    }
    pub fn window_start(&self, now_ms: u64) -> u64 {
        now_ms - now_ms % self.window_ms
    }
}

/// 用途标识：小写字母开头，只含小写字母、数字和 `-`。
pub fn validate_category(category: &str) -> BudgetResult<()> {
    if category.is_empty()
        || category.len() > MAX_CATEGORY_BYTES
        || !category.starts_with(|c: char| c.is_ascii_lowercase())
        || !category
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(BudgetError::InvalidInput);
    }
    Ok(())
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetWindow {
    pub start_ms: u64,
    pub used: u32,
    pub by_category: BTreeMap<String, u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BudgetSnapshot {
    pub policy: BudgetPolicy,
    /// 当前窗口；时钟回拨到已记录窗口之前时仍沿用已记录的窗口，不重新计数。
    pub current: BudgetWindow,
    /// 宿主配置的分项上限，仍同时受总上限约束；重启不重置已用计数。
    pub category_limits: BTreeMap<String, u32>,
    pub resets_at_ms: u64,
    /// 已结束的窗口，新的在前。
    pub history: Vec<BudgetWindow>,
}
impl BudgetSnapshot {
    pub fn remaining(&self) -> u32 {
        self.policy.limit.saturating_sub(self.current.used)
    }
}

/// 仅可信宿主持有；不发布给模型或不受信插件。
pub trait AutonomyBudget: Send + Sync {
    fn snapshot(&self, now_ms: u64) -> BudgetResult<BudgetSnapshot>;
    /// 先持久化计数再返回；余额不足时返回 Exhausted 且不写入。返回本窗口剩余额度。
    fn reserve(&self, category: &str, count: u32, now_ms: u64) -> BudgetResult<u32>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_align_and_policies_are_bounded() {
        let policy = BudgetPolicy {
            window_ms: 3_600_000,
            limit: 10,
        };
        assert!(policy.validate().is_ok());
        assert_eq!(policy.window_start(7_300_000), 7_200_000);
        for bad in [
            (1_000, 1),
            (MAX_WINDOW_MS + 1, 1),
            (60_000, 0),
            (60_000, MAX_LIMIT + 1),
        ] {
            assert!(
                BudgetPolicy {
                    window_ms: bad.0,
                    limit: bad.1
                }
                .validate()
                .is_err()
            );
        }
        assert!(validate_category("research").is_ok());
        for bad in ["", "Research", "a b", "1x", &"a".repeat(33)] {
            assert!(validate_category(bad).is_err(), "{bad}");
        }
    }
}
