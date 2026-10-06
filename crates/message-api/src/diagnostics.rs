//! 单次判断的脱敏观察契约；不规定 HTTP、存储或汇总实现。
use crate::RelationError;
use serde::Serialize;
use std::panic::{AssertUnwindSafe, catch_unwind};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationStage {
    Rules,
    Auxiliary,
    Primary,
}

/// 本地适配器调用边界，不证明服务收到 HTTP 请求或产生计费。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationAttempt {
    ClassifierCall,
    ModelProviderCall,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum RelationOperation {
    Stage(RelationStage),
    Attempt(RelationAttempt),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationOutcome {
    Completed,
    Unavailable,
    Protocol,
    Timeout,
    Panicked,
    Dropped,
}

impl From<RelationError> for RelationOutcome {
    fn from(error: RelationError) -> Self {
        match error {
            RelationError::Unavailable => Self::Unavailable,
            RelationError::Protocol => Self::Protocol,
            RelationError::Timeout => Self::Timeout,
            RelationError::Panicked => Self::Panicked,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason {
    Unavailable,
    Protocol,
    Timeout,
    Panicked,
    InvalidDecision,
    Ambiguous,
    LowConfidence,
}

impl From<RelationError> for FallbackReason {
    fn from(error: RelationError) -> Self {
        match error {
            RelationError::Unavailable => Self::Unavailable,
            RelationError::Protocol => Self::Protocol,
            RelationError::Timeout => Self::Timeout,
            RelationError::Panicked => Self::Panicked,
        }
    }
}

/// 不包含字符串、路由 ID、正文、模型解释、端点或凭据。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RelationObservation {
    Supported,
    Unsupported,
    Started {
        operation: RelationOperation,
    },
    Finished {
        operation: RelationOperation,
        outcome: RelationOutcome,
        elapsed_micros: u64,
    },
    Fallback {
        reason: FallbackReason,
    },
}

/// 同步、短时、非阻塞回调；消费者不得在回调内执行 I/O 或等待判断。
/// 每个 judge_observed 调用应注入独立观察器；不提供后台重放或持久化。
pub trait RelationObserver: Send + Sync {
    fn observe(&self, observation: RelationObservation);
}

/// 观察器 panic 不改变业务结果；不记录 panic payload。
pub fn observe_relation(observer: &dyn RelationObserver, observation: RelationObservation) {
    let _ = catch_unwind(AssertUnwindSafe(|| observer.observe(observation)));
}

pub struct DiscardRelationObservations;
impl RelationObserver for DiscardRelationObservations {
    fn observe(&self, _: RelationObservation) {}
}
