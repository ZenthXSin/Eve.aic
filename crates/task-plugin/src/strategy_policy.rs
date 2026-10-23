//! 固定留出任务的自动配对策略；只使用真实已派发任务，不为了对照再执行一次副作用。
use eve_task_api::*;
use std::{collections::BTreeSet, fmt, sync::Arc};

pub struct PairedTaskStrategyPolicy {
    environment: Arc<dyn TaskStrategyEnvironment>,
    proposals: Vec<TaskStrategyProposal>,
    cases: Vec<TaskStrategyCase>,
    training: Vec<TaskStrategyCase>,
}
impl fmt::Debug for PairedTaskStrategyPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairedTaskStrategyPolicy")
            .field("proposals", &self.proposals.len())
            .field("cases", &self.cases.len())
            .finish_non_exhaustive()
    }
}
impl PairedTaskStrategyPolicy {
    pub fn new(
        environment: Arc<dyn TaskStrategyEnvironment>,
        proposals: Vec<TaskStrategyProposal>,
        cases: Vec<TaskStrategyCase>,
    ) -> TaskResult<Self> {
        if proposals.len() > 32 || cases.len() > 64 {
            return Err(TaskError::LimitReached);
        }
        for proposal in &proposals {
            proposal.validate()?
        }
        let mut keys = BTreeSet::new();
        for case in &cases {
            case.scope.validate()?;
            case.contract.validate()?;
            if !valid_strategy_digest(&case.input_sha256)
                || !keys.insert(task_strategy_digest(&(&case.scope, &case.input_sha256)))
                || proposals
                    .iter()
                    .any(|p| p.scope == case.scope && p.source_input_sha256 == case.input_sha256)
            {
                return Err(TaskError::InvalidInput);
            }
        }
        for proposal in &proposals {
            if cases.iter().filter(|c| c.scope == proposal.scope).count() < 2 {
                return Err(TaskError::InvalidInput);
            }
        }
        Ok(Self {
            environment,
            proposals,
            cases,
            training: Vec::new(),
        })
    }
    /// 自动提炼来源单独登记，不能把训练输入用作晋升的留出输入。
    pub fn with_training_cases(mut self, training: Vec<TaskStrategyCase>) -> TaskResult<Self> {
        if training.len() > 64 {
            return Err(TaskError::LimitReached);
        }
        let mut keys = BTreeSet::new();
        for case in &training {
            case.scope.validate()?;
            case.contract.validate()?;
            if !valid_strategy_digest(&case.input_sha256)
                || !keys.insert(task_strategy_digest(&(&case.scope, &case.input_sha256)))
                || self
                    .cases
                    .iter()
                    .any(|c| c.scope == case.scope && c.input_sha256 == case.input_sha256)
                || self.cases.iter().filter(|c| c.scope == case.scope).count() < 2
            {
                return Err(TaskError::InvalidInput);
            }
        }
        self.training = training;
        Ok(self)
    }
}
impl TaskStrategyPolicy for PairedTaskStrategyPolicy {
    fn proposals(&self) -> TaskResult<Vec<TaskStrategyProposal>> {
        Ok(self.proposals.clone())
    }
    fn source_contract(&self, task: &Task) -> TaskResult<Option<TaskEvaluationContract>> {
        let Some(scope) = self.scope(task)? else {
            return Ok(None);
        };
        let input = task_strategy_input(task);
        Ok(self
            .training
            .iter()
            .find(|c| c.scope == scope && c.input_sha256 == input)
            .map(|c| c.contract.clone()))
    }
    fn scope(&self, task: &Task) -> TaskResult<Option<TaskStrategyScope>> {
        let scope = self.environment.scope(task)?;
        if let Some(scope) = &scope {
            scope.validate()?;
            if scope.owner != task.owner {
                return Err(TaskError::InvalidInput);
            }
        }
        Ok(scope)
    }
    fn assign(
        &self,
        task: &Task,
        snapshot: &TaskStrategySnapshot,
    ) -> TaskResult<Option<TaskStrategyAssignment>> {
        let Some(scope) = self.scope(task)? else {
            return Ok(None);
        };
        let input = task_strategy_input(task);
        let Some(case) = self
            .cases
            .iter()
            .find(|c| c.scope == scope && c.input_sha256 == input)
        else {
            return Ok(None);
        };
        let views = crate::strategy::task_strategy_views(snapshot)?;
        // 优先试用同范围的候选；没有候选时继续观察有效的最新版本。
        let candidate = views
            .iter()
            .rev()
            .find(|v| {
                v.strategy.proposal.scope == scope && v.status == TaskStrategyStatus::Candidate
            })
            .or_else(|| {
                views.iter().rev().find(|v| {
                    v.strategy.proposal.scope == scope && v.status == TaskStrategyStatus::Active
                })
            });
        let Some(candidate) = candidate else {
            return Ok(None);
        };
        if candidate.strategy.parent.is_some_and(|parent| {
            views
                .iter()
                .any(|v| v.strategy.version == parent && v.status == TaskStrategyStatus::Rejected)
        }) {
            return Ok(None);
        }
        let version = candidate.strategy.version;
        let control = snapshot.selections.iter().find(|s| {
            s.assignment.arm == (TaskStrategyArm::Control { candidate: version })
                && s.input_sha256 == input
        });
        let arm = match control {
            None => TaskStrategyArm::Control { candidate: version },
            Some(control)
                if candidate.status == TaskStrategyStatus::Active
                    && control.assignment.contract == case.contract
                    && control
                        .observation
                        .as_ref()
                        .is_some_and(|o| o.effect == TaskEffect::Verified) =>
            {
                TaskStrategyArm::Apply { candidate: version }
            }
            Some(control)
                if control.assignment.contract == case.contract
                    && control
                        .observation
                        .as_ref()
                        .is_some_and(|o| o.effect == TaskEffect::Verified)
                    && !snapshot.selections.iter().any(|s| {
                        s.assignment.arm == (TaskStrategyArm::Trial { candidate: version })
                            && s.input_sha256 == input
                    }) =>
            {
                TaskStrategyArm::Trial { candidate: version }
            }
            _ => return Ok(None),
        };
        Ok(Some(TaskStrategyAssignment {
            scope,
            contract: case.contract.clone(),
            arm,
        }))
    }
}
