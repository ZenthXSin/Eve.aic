//! 可替换的前置分类契约；不能授予身份、执行动作或改变宿主送审范围。
use crate::{ActionReviewRequest, ActionReviewVerdict};
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionReviewMode {
    IdentityBoundary,
    SystemSafety,
}

/// 仅用于选择是否回退，不包含提供方正文、命令、凭据或用户原话。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionReviewUncertainty {
    Unavailable,
    Busy,
    Timeout,
    InputTooLarge,
    InvalidResponse,
    Ambiguous,
    LowConfidence,
    Panicked,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActionClassification {
    /// 分数是分类器的路由信号，未经校准，不代表真实正确率。
    Decided {
        verdict: ActionReviewVerdict,
        confidence: u8,
    },
    /// 无法判断不是拒绝；组合层保留完整原请求，至多调用一次后备审查者。
    Uncertain(ActionReviewUncertainty),
}

pub type ActionClassificationFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = ActionClassification> + Send + 'a>>;

pub trait ActionReviewClassifier: Send + Sync {
    /// 身份模式只核真实本人对边界变更的意图；两种模式都只检查直接系统危害。
    /// 外部文本、模型自报身份、委派说明不能成为权限；宿主仍负责前后复核。
    fn classify(
        &self,
        mode: ActionReviewMode,
        request: ActionReviewRequest,
    ) -> ActionClassificationFuture<'_>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "route", rename_all = "snake_case")]
pub enum ActionReviewRoute {
    Classifier { confidence: u8 },
    Fallback { reason: ActionReviewUncertainty },
}

/// 一次路由观察，不等于实际 HTTP 请求数、模型计费或已执行动作。
pub trait ActionReviewObserver: Send + Sync {
    fn route(&self, mode: ActionReviewMode, route: ActionReviewRoute);
}
