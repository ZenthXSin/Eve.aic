//! 公开判断观察契约的有界、单次调用实现；无全局计数、正文或持久化。
use eve_message_api::{
    RelationAttempt, RelationObservation, RelationObserver, RelationOperation, RelationOutcome,
    RelationStage, observe_relation,
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Instant,
};

pub const MAX_OBSERVATIONS: usize = 64;

/// 在实际动作开始时创建；取消或 panic 丢弃 Future 时仍产生一次 Dropped。
pub struct RelationObservationGuard {
    observer: Arc<dyn RelationObserver>,
    operation: RelationOperation,
    started: Instant,
    finished: bool,
}
impl RelationObservationGuard {
    pub fn stage(observer: Arc<dyn RelationObserver>, stage: RelationStage) -> Self {
        Self::new(observer, RelationOperation::Stage(stage))
    }
    pub fn attempt(observer: Arc<dyn RelationObserver>, attempt: RelationAttempt) -> Self {
        Self::new(observer, RelationOperation::Attempt(attempt))
    }
    fn new(observer: Arc<dyn RelationObserver>, operation: RelationOperation) -> Self {
        let guard = Self {
            observer,
            operation,
            started: Instant::now(),
            finished: false,
        };
        observe_relation(
            guard.observer.as_ref(),
            RelationObservation::Started { operation },
        );
        guard
    }
    pub fn finish(mut self, outcome: RelationOutcome) {
        self.emit_finish(outcome);
    }
    fn emit_finish(&mut self, outcome: RelationOutcome) {
        self.finished = true;
        observe_relation(
            self.observer.as_ref(),
            RelationObservation::Finished {
                operation: self.operation,
                outcome,
                elapsed_micros: self.started.elapsed().as_micros().min(u64::MAX as u128) as u64,
            },
        );
    }
}
impl Drop for RelationObservationGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.emit_finish(RelationOutcome::Dropped);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCoverage {
    Complete,
    Unsupported,
    Overflow,
    Invalid,
    Unreported,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct RelationDiagnosticCounts {
    pub rules_started: u64,
    pub auxiliary_started: u64,
    pub primary_started: u64,
    pub classifier_calls: u64,
    pub model_provider_calls: u64,
    pub fallbacks: u64,
}

impl RelationDiagnosticCounts {
    pub fn accumulate(&mut self, other: &Self) {
        self.rules_started += other.rules_started;
        self.auxiliary_started += other.auxiliary_started;
        self.primary_started += other.primary_started;
        self.classifier_calls += other.classifier_calls;
        self.model_provider_calls += other.model_provider_calls;
        self.fallbacks += other.fallbacks;
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RelationDiagnostics {
    pub coverage: DiagnosticCoverage,
    /// 不支持观察、事件溢出或缺少终态时为 null，绝不填伪造的零。
    pub counts: Option<RelationDiagnosticCounts>,
    pub events: Vec<RelationObservation>,
}

#[derive(Default)]
struct State {
    events: Vec<RelationObservation>,
    overflow: bool,
}

/// 每次判断单独构造，最多保留 64 个事件；不阻塞异步等待，不持有业务输入。
#[derive(Default)]
pub struct BoundedRelationDiagnostics {
    state: Mutex<State>,
}
impl RelationObserver for BoundedRelationDiagnostics {
    fn observe(&self, observation: RelationObservation) {
        if let Ok(mut state) = self.state.lock() {
            if state.events.len() < MAX_OBSERVATIONS {
                state.events.push(observation);
            } else {
                state.overflow = true;
            }
        }
    }
}
impl BoundedRelationDiagnostics {
    /// 应在判断完成或 Future 已丢弃之后读取。
    pub fn snapshot(&self) -> RelationDiagnostics {
        let Ok(state) = self.state.lock() else {
            return RelationDiagnostics {
                coverage: DiagnosticCoverage::Invalid,
                counts: None,
                events: vec![],
            };
        };
        let mut counts = RelationDiagnosticCounts::default();
        let mut outstanding = BTreeMap::<RelationOperation, u64>::new();
        let mut supported = false;
        let mut unsupported = false;
        let mut invalid = false;
        for event in &state.events {
            match *event {
                RelationObservation::Supported => supported = true,
                RelationObservation::Unsupported => unsupported = true,
                RelationObservation::Started { operation } => {
                    *outstanding.entry(operation).or_default() += 1;
                    match operation {
                        RelationOperation::Stage(RelationStage::Rules) => counts.rules_started += 1,
                        RelationOperation::Stage(RelationStage::Auxiliary) => {
                            counts.auxiliary_started += 1
                        }
                        RelationOperation::Stage(RelationStage::Primary) => {
                            counts.primary_started += 1
                        }
                        RelationOperation::Attempt(RelationAttempt::ClassifierCall) => {
                            counts.classifier_calls += 1
                        }
                        RelationOperation::Attempt(RelationAttempt::ModelProviderCall) => {
                            counts.model_provider_calls += 1
                        }
                    }
                }
                RelationObservation::Finished { operation, .. } => {
                    let pending = outstanding.entry(operation).or_default();
                    if *pending == 0 {
                        invalid = true;
                    } else {
                        *pending -= 1;
                    }
                }
                RelationObservation::Fallback { .. } => counts.fallbacks += 1,
            }
        }
        let coverage = if state.overflow {
            DiagnosticCoverage::Overflow
        } else if unsupported {
            DiagnosticCoverage::Unsupported
        } else if invalid || outstanding.values().any(|count| *count != 0) {
            DiagnosticCoverage::Invalid
        } else if !supported {
            DiagnosticCoverage::Unreported
        } else {
            DiagnosticCoverage::Complete
        };
        RelationDiagnostics {
            counts: (coverage == DiagnosticCoverage::Complete).then_some(counts),
            coverage,
            events: state.events.clone(),
        }
    }
}

#[cfg(test)]
mod tests;
