//! 有条件多步计划的公开契约：步骤依赖、预算、绑定输入与可核对的效果条件。
//!
//! 不包含存储、模型、文件或执行实现。计划引用能力 ID 不授予任何权限；宿主须登记能力上限，
//! 并在执行前重新核对绑定输入。步骤效果只由宿主独立读取得到的证据判定，调用方不能自报完成。
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

pub const PLAN_PLUGIN_ID: &str = "eve.plan";
pub const PLAN_SCHEMA_VERSION: u32 = 1;
/// 每个主体的计划记录上限；达到后新建计划明确失败，不淘汰旧记录。
pub const MAX_PLANS: usize = 64;
pub const MAX_STEPS: usize = 8;
pub const MAX_STEP_ATTEMPTS: u8 = 3;
pub const MAX_STEP_TIMEOUT_MS: u64 = 30_000;
pub const MAX_TITLE_BYTES: usize = 256;
pub const MAX_PLAN_JSON_BYTES: usize = 16_384;
pub const MAX_PLAN_STATE_BYTES: usize = 2_097_152;

pub type PlanResult<T> = Result<T, PlanError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanError {
    InvalidInput,
    /// 计划引用了宿主未登记的能力，或超出登记的上限。
    UnknownCapability,
    /// 同一目标已有活动计划，或同一 ID 的内容不同。
    Conflict,
    NotFound,
    /// 步骤依赖未满足、已在执行、计划非活动或尝试次数已用完。
    NotReady,
    StaleRevision,
    InvalidTransition,
    LimitReached,
    Storage,
    CorruptState,
    SubjectMismatch,
    Unavailable,
}
impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "计划输入无效",
            Self::UnknownCapability => "计划引用了宿主未登记或超出上限的能力",
            Self::Conflict => "计划与已有记录冲突",
            Self::NotFound => "计划或步骤不存在",
            Self::NotReady => "步骤当前不可执行",
            Self::StaleRevision => "计划记录已变化，请重新读取",
            Self::InvalidTransition => "计划状态转换无效",
            Self::LimitReached => "计划容量或计数已达上限",
            Self::Storage => "计划状态保存失败，可能已经提交，须重新打开核对",
            Self::CorruptState => "计划状态损坏，已保留原数据",
            Self::SubjectMismatch => "计划状态属于其他主体",
            Self::Unavailable => "计划服务不可用",
        })
    }
}
impl std::error::Error for PlanError {}

/// 计划假定成立的输入。任一项与当前目标不符时计划过时，未完成步骤失效。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanBinding {
    pub goal_id: String,
    pub goal_revision: u64,
    /// 计划依据的输入摘要（如最近一次文件观察的 SHA-256）；为空表示不依赖外部输入。
    pub input_sha256: Option<String>,
}
impl PlanBinding {
    pub fn validate(&self) -> PlanResult<()> {
        validate_id(&self.goal_id)?;
        if self.goal_revision == 0
            || self
                .input_sha256
                .as_deref()
                .is_some_and(|value| !is_sha256(value))
        {
            return Err(PlanError::InvalidInput);
        }
        Ok(())
    }
}

/// 宿主执行能力后独立读取得到的证据。只说明读到的字节，不说明父目标已经达成。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepEvidence {
    pub capability: String,
    /// 宿主绑定作用范围的稳定摘要标识；不是路径，也不是访问凭据。
    pub source_id: String,
    pub sha256: String,
    pub bytes: u64,
    pub verified_at_ms: u64,
}
impl StepEvidence {
    pub fn validate(&self) -> PlanResult<()> {
        validate_id(&self.capability)?;
        validate_id(&self.source_id)?;
        if !is_sha256(&self.sha256) || self.verified_at_ms == 0 {
            return Err(PlanError::InvalidInput);
        }
        Ok(())
    }
}

/// 步骤完成所需的可核对效果；只与证据字段比较，不解释正文语义。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EffectCondition {
    /// 能力返回了经独立读取的证据即满足。
    Verified,
    /// 证据摘要须等于给定值。
    DigestEquals { sha256: String },
    /// 证据摘要须不同于给定值，例如等待输入文件被修改。
    DigestDiffers { sha256: String },
}
impl EffectCondition {
    pub fn validate(&self) -> PlanResult<()> {
        match self {
            Self::Verified => Ok(()),
            Self::DigestEquals { sha256 } | Self::DigestDiffers { sha256 } if is_sha256(sha256) => {
                Ok(())
            }
            _ => Err(PlanError::InvalidInput),
        }
    }
    pub fn satisfied_by(&self, evidence: &StepEvidence) -> bool {
        match self {
            Self::Verified => true,
            Self::DigestEquals { sha256 } => &evidence.sha256 == sha256,
            Self::DigestDiffers { sha256 } => &evidence.sha256 != sha256,
        }
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepSpec {
    pub id: String,
    /// 给操作者看的说明；只是数据，不会被执行或交给能力。
    pub title: String,
    pub capability: String,
    pub depends_on: Vec<String>,
    pub max_attempts: u8,
    pub timeout_ms: u64,
    pub effect: EffectCondition,
}

/// 宿主登记的能力上限。计划只能在这些上限内引用能力，引用本身不授予权限。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilitySpec {
    pub id: String,
    pub max_attempts: u8,
    pub max_timeout_ms: u64,
    /// 需要计划绑定输入摘要，执行前由宿主核对当前输入。
    pub requires_input: bool,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanSpec {
    pub binding: PlanBinding,
    pub steps: Vec<StepSpec>,
}
impl PlanSpec {
    pub fn parse(text: &str) -> PlanResult<Self> {
        parse_json(text, MAX_PLAN_JSON_BYTES)
    }

    /// 结构校验：有界、ID 唯一、依赖存在且无环、能力已登记且预算在上限内。
    pub fn validate(&self, capabilities: &[CapabilitySpec]) -> PlanResult<()> {
        self.validate_shape()?;
        for step in &self.steps {
            let capability = capabilities
                .iter()
                .find(|capability| capability.id == step.capability)
                .ok_or(PlanError::UnknownCapability)?;
            if step.max_attempts > capability.max_attempts
                || step.timeout_ms > capability.max_timeout_ms
                || (capability.requires_input && self.binding.input_sha256.is_none())
            {
                return Err(PlanError::UnknownCapability);
            }
        }
        Ok(())
    }

    /// 不依赖能力登记的形状校验；持久记录用它核对，避免宿主能力变化使旧记录不可读。
    fn validate_shape(&self) -> PlanResult<()> {
        self.binding.validate()?;
        if self.steps.is_empty() || self.steps.len() > MAX_STEPS {
            return Err(PlanError::InvalidInput);
        }
        let mut ids = BTreeSet::new();
        for step in &self.steps {
            validate_id(&step.id)?;
            validate_id(&step.capability)?;
            if !ids.insert(step.id.as_str())
                || step.title.trim().is_empty()
                || step.title.len() > MAX_TITLE_BYTES
                || step.title.contains('\0')
                || !(1..=MAX_STEP_ATTEMPTS).contains(&step.max_attempts)
                || !(1..=MAX_STEP_TIMEOUT_MS).contains(&step.timeout_ms)
                || step.depends_on.len() >= MAX_STEPS
            {
                return Err(PlanError::InvalidInput);
            }
            step.effect.validate()?;
        }
        for step in &self.steps {
            let mut seen = BTreeSet::new();
            for dependency in &step.depends_on {
                if dependency == &step.id
                    || !ids.contains(dependency.as_str())
                    || !seen.insert(dependency.as_str())
                {
                    return Err(PlanError::InvalidInput);
                }
            }
        }
        // Kahn 拓扑排序；剩余节点说明存在环。
        let mut remaining: BTreeMap<&str, BTreeSet<&str>> = self
            .steps
            .iter()
            .map(|step| {
                (
                    step.id.as_str(),
                    step.depends_on.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        while let Some(next) = remaining
            .iter()
            .find(|(_, dependencies)| dependencies.is_empty())
            .map(|(id, _)| *id)
        {
            remaining.remove(next);
            for dependencies in remaining.values_mut() {
                dependencies.remove(next);
            }
        }
        if !remaining.is_empty() {
            return Err(PlanError::InvalidInput);
        }
        validate_encoded(self, MAX_PLAN_JSON_BYTES)
    }

    /// 内容寻址的计划 ID：同一主体的相同计划幂等，换任何步骤或绑定都是另一份计划。
    pub fn plan_id(&self, subject_id: &str) -> PlanResult<String> {
        validate_id(subject_id)?;
        let encoded = serde_json::to_vec(self).map_err(|_| PlanError::InvalidInput)?;
        let mut digest = Context::new(&SHA256);
        for part in [
            b"eve.plan:v1".as_slice(),
            subject_id.as_bytes(),
            encoded.as_slice(),
        ] {
            digest.update(&(part.len() as u64).to_be_bytes());
            digest.update(part);
        }
        Ok(format!("plan:{}", hex(digest.finish().as_ref())))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Executing,
    Satisfied,
    Failed,
    /// 执行中断、被取消，或计划因其他步骤失败而停止；不会自动重试。
    Blocked,
    /// 计划绑定的输入已变化，本步骤不再按原计划推进。
    Invalidated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepFailure {
    CapabilityFailed,
    EffectNotMet,
    Timeout,
    Cancelled,
    Interrupted,
    /// 执行前或执行中发现计划绑定的输入已变化。
    BindingChanged,
}
impl StepFailure {
    /// 可在剩余尝试内重试的失败；取消、中断与绑定变化需要新的判断。
    fn retryable(self) -> bool {
        matches!(
            self,
            Self::CapabilityFailed | Self::EffectNotMet | Self::Timeout
        )
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepAttempt {
    pub number: u8,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub evidence: Option<StepEvidence>,
    /// 证据满足效果条件；没有证据或计划已过时为 false。
    pub effect_met: bool,
    pub failure: Option<StepFailure>,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepState {
    pub spec: StepSpec,
    pub status: StepStatus,
    pub attempts: Vec<StepAttempt>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Active,
    /// 全部步骤的效果条件都由独立证据满足；不代表父目标或现实任务已经完成。
    Completed,
    /// 有步骤失败、中断或被取消，计划不再准入新步骤。
    Blocked,
    /// 绑定输入已变化；保留历史，不再准入步骤。
    Stale,
}

/// 步骤结束时宿主提交的结果；效果是否满足由计划按证据判定，不接受调用方结论。
pub enum StepOutcome {
    Evidence(StepEvidence),
    Failed { failure: StepFailure, at_ms: u64 },
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub id: String,
    /// 本计划记录的修订；每次状态变化加一，用于 CAS。
    pub revision: u64,
    pub binding: PlanBinding,
    pub created_at_ms: u64,
    pub status: PlanStatus,
    pub stale_at_ms: Option<u64>,
    pub steps: Vec<StepState>,
}
impl Plan {
    pub fn new(
        subject_id: &str,
        spec: PlanSpec,
        capabilities: &[CapabilitySpec],
        created_at_ms: u64,
    ) -> PlanResult<Self> {
        spec.validate(capabilities)?;
        if created_at_ms == 0 {
            return Err(PlanError::InvalidInput);
        }
        Ok(Self {
            id: spec.plan_id(subject_id)?,
            revision: 1,
            created_at_ms,
            status: PlanStatus::Active,
            stale_at_ms: None,
            steps: spec
                .steps
                .into_iter()
                .map(|spec| StepState {
                    spec,
                    status: StepStatus::Pending,
                    attempts: Vec::new(),
                })
                .collect(),
            binding: spec.binding,
        })
    }

    pub fn spec(&self) -> PlanSpec {
        PlanSpec {
            binding: self.binding.clone(),
            steps: self.steps.iter().map(|step| step.spec.clone()).collect(),
        }
    }

    pub fn step(&self, id: &str) -> Option<&StepState> {
        self.steps.iter().find(|step| step.spec.id == id)
    }

    /// 计划活动、无其他步骤执行中、依赖全部满足且仍有尝试次数的 Pending 步骤。
    pub fn ready_steps(&self) -> Vec<&str> {
        if self.status != PlanStatus::Active
            || self
                .steps
                .iter()
                .any(|step| step.status == StepStatus::Executing)
        {
            return Vec::new();
        }
        self.steps
            .iter()
            .filter(|step| {
                step.status == StepStatus::Pending
                    && step.attempts.len() < usize::from(step.spec.max_attempts)
                    && step.spec.depends_on.iter().all(|id| {
                        self.step(id)
                            .is_some_and(|dependency| dependency.status == StepStatus::Satisfied)
                    })
            })
            .map(|step| step.spec.id.as_str())
            .collect()
    }

    /// 先保存 Executing 再允许副作用；返回本次尝试序号。
    pub fn begin_step(&mut self, step_id: &str, at_ms: u64) -> PlanResult<u8> {
        // 时钟回拨会让记录无法通过一致性校验；明确拒绝，而不是写入矛盾时间。
        if at_ms < self.created_at_ms {
            return Err(PlanError::InvalidInput);
        }
        self.step(step_id).ok_or(PlanError::NotFound)?;
        if !self.ready_steps().contains(&step_id) {
            return Err(PlanError::NotReady);
        }
        let step = self.step_mut(step_id)?;
        let number = u8::try_from(step.attempts.len() + 1).map_err(|_| PlanError::LimitReached)?;
        step.attempts.push(StepAttempt {
            number,
            started_at_ms: at_ms,
            finished_at_ms: None,
            evidence: None,
            effect_met: false,
            failure: None,
        });
        step.status = StepStatus::Executing;
        self.bump()?;
        Ok(number)
    }

    /// 仅 Executing 可结束。计划已过时时只记录证据，不计为满足。
    pub fn finish_step(&mut self, step_id: &str, outcome: StepOutcome) -> PlanResult<()> {
        let stale = self.status == PlanStatus::Stale;
        let step = self.step_mut(step_id)?;
        if step.status != StepStatus::Executing {
            return Err(PlanError::InvalidTransition);
        }
        let (evidence, failure, at_ms) = match outcome {
            StepOutcome::Evidence(evidence) => {
                evidence.validate()?;
                if evidence.capability != step.spec.capability {
                    return Err(PlanError::InvalidInput);
                }
                let failure = (!step.spec.effect.satisfied_by(&evidence))
                    .then_some(StepFailure::EffectNotMet);
                let at_ms = evidence.verified_at_ms;
                (Some(evidence), failure, at_ms)
            }
            StepOutcome::Failed { failure, at_ms } => {
                if at_ms == 0 {
                    return Err(PlanError::InvalidInput);
                }
                (None, Some(failure), at_ms)
            }
        };
        let max_attempts = usize::from(step.spec.max_attempts);
        let attempts = step.attempts.len();
        let attempt = step.attempts.last_mut().ok_or(PlanError::CorruptState)?;
        if at_ms < attempt.started_at_ms {
            return Err(PlanError::InvalidInput);
        }
        attempt.finished_at_ms = Some(at_ms);
        attempt.effect_met = failure.is_none() && evidence.is_some() && !stale;
        attempt.evidence = evidence;
        attempt.failure = failure;
        step.status = match failure {
            _ if stale => StepStatus::Invalidated,
            None => StepStatus::Satisfied,
            Some(StepFailure::BindingChanged) => StepStatus::Invalidated,
            Some(failure) if failure.retryable() && attempts < max_attempts => StepStatus::Pending,
            Some(failure) if failure.retryable() => StepStatus::Failed,
            Some(_) => StepStatus::Blocked,
        };
        if failure == Some(StepFailure::BindingChanged) && !stale {
            self.mark_stale(at_ms);
        }
        self.settle();
        self.bump()
    }

    /// 绑定输入与当前不符时封存：未开始的步骤失效，执行中的步骤结束时也不计为满足。
    /// 已完成或已阻塞的计划保留原结论；返回是否发生变化。
    pub fn invalidate(&mut self, current: &PlanBinding, at_ms: u64) -> PlanResult<bool> {
        current.validate()?;
        if at_ms == 0 || current.goal_id != self.binding.goal_id {
            return Err(PlanError::InvalidInput);
        }
        if self.status != PlanStatus::Active || current == &self.binding {
            return Ok(false);
        }
        self.mark_stale(at_ms);
        self.bump()?;
        Ok(true)
    }

    /// 重启恢复：遗留 Executing 封存为 Blocked/Interrupted，不重放可能已发生的副作用。
    pub fn interrupt(&mut self, at_ms: u64) -> PlanResult<bool> {
        let mut changed = false;
        for step in &mut self.steps {
            if step.status != StepStatus::Executing {
                continue;
            }
            let attempt = step.attempts.last_mut().ok_or(PlanError::CorruptState)?;
            attempt.finished_at_ms = Some(at_ms.max(attempt.started_at_ms));
            attempt.failure = Some(StepFailure::Interrupted);
            step.status = if self.status == PlanStatus::Stale {
                StepStatus::Invalidated
            } else {
                StepStatus::Blocked
            };
            changed = true;
        }
        if changed {
            self.settle();
            self.bump()?;
        }
        Ok(changed)
    }

    fn mark_stale(&mut self, at_ms: u64) {
        self.status = PlanStatus::Stale;
        self.stale_at_ms = Some(at_ms);
        for step in &mut self.steps {
            if step.status == StepStatus::Pending {
                step.status = StepStatus::Invalidated;
            }
        }
    }

    /// 由步骤状态推出计划状态；失败使其余 Pending 步骤阻塞。
    fn settle(&mut self) {
        if self.status != PlanStatus::Active {
            return;
        }
        if self
            .steps
            .iter()
            .any(|step| matches!(step.status, StepStatus::Failed | StepStatus::Blocked))
        {
            self.status = PlanStatus::Blocked;
            for step in &mut self.steps {
                if step.status == StepStatus::Pending {
                    step.status = StepStatus::Blocked;
                }
            }
        } else if self
            .steps
            .iter()
            .all(|step| step.status == StepStatus::Satisfied)
        {
            self.status = PlanStatus::Completed;
        }
    }

    fn step_mut(&mut self, id: &str) -> PlanResult<&mut StepState> {
        self.steps
            .iter_mut()
            .find(|step| step.spec.id == id)
            .ok_or(PlanError::NotFound)
    }

    fn bump(&mut self) -> PlanResult<()> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or(PlanError::LimitReached)?;
        Ok(())
    }

    /// 持久记录的一致性校验，不依赖当前能力登记。
    pub fn validate(&self, subject_id: &str) -> PlanResult<()> {
        let spec = self.spec();
        spec.validate_shape()?;
        if self.id != spec.plan_id(subject_id)?
            || self.revision == 0
            || self.created_at_ms == 0
            || (self.status == PlanStatus::Stale) != self.stale_at_ms.is_some()
        {
            return Err(PlanError::InvalidInput);
        }
        let mut executing = 0;
        for step in &self.steps {
            if step.attempts.len() > usize::from(step.spec.max_attempts) {
                return Err(PlanError::InvalidInput);
            }
            for (index, attempt) in step.attempts.iter().enumerate() {
                let last = index + 1 == step.attempts.len();
                let open = attempt.finished_at_ms.is_none();
                if usize::from(attempt.number) != index + 1
                    || attempt.started_at_ms < self.created_at_ms
                    || attempt
                        .finished_at_ms
                        .is_some_and(|at| at < attempt.started_at_ms)
                    || (open && (!last || step.status != StepStatus::Executing))
                    || (open && (attempt.evidence.is_some() || attempt.failure.is_some()))
                    || (attempt.effect_met
                        && (attempt.failure.is_some()
                            || !attempt.evidence.as_ref().is_some_and(|evidence| {
                                evidence.capability == step.spec.capability
                                    && step.spec.effect.satisfied_by(evidence)
                            })))
                    || (!open && !attempt.effect_met && attempt.failure.is_none() && !last)
                {
                    return Err(PlanError::InvalidInput);
                }
                if let Some(evidence) = &attempt.evidence {
                    evidence.validate()?;
                }
            }
            let last = step.attempts.last();
            let consistent = match step.status {
                StepStatus::Pending => last.is_none_or(|a| a.failure.is_some()),
                StepStatus::Executing => last.is_some_and(|a| a.finished_at_ms.is_none()),
                StepStatus::Satisfied => last.is_some_and(|a| a.effect_met),
                StepStatus::Failed => last.is_some_and(|a| a.failure.is_some()),
                StepStatus::Blocked | StepStatus::Invalidated => {
                    last.is_none_or(|a| a.finished_at_ms.is_some() && !a.effect_met)
                }
            };
            if !consistent {
                return Err(PlanError::InvalidInput);
            }
            executing += usize::from(step.status == StepStatus::Executing);
            if step.status == StepStatus::Satisfied
                && step.spec.depends_on.iter().any(|id| {
                    self.step(id)
                        .is_none_or(|dependency| dependency.status != StepStatus::Satisfied)
                })
            {
                return Err(PlanError::InvalidInput);
            }
        }
        let all = |status| self.steps.iter().all(|step| step.status == status);
        let any = |status| self.steps.iter().any(|step| step.status == status);
        let status_ok = match self.status {
            PlanStatus::Active => {
                executing <= 1
                    && !any(StepStatus::Failed)
                    && !any(StepStatus::Blocked)
                    && !any(StepStatus::Invalidated)
                    && !all(StepStatus::Satisfied)
            }
            PlanStatus::Completed => all(StepStatus::Satisfied),
            PlanStatus::Blocked => {
                executing == 0
                    && (any(StepStatus::Failed) || any(StepStatus::Blocked))
                    && !any(StepStatus::Pending)
                    && !any(StepStatus::Invalidated)
            }
            PlanStatus::Stale => executing <= 1 && !any(StepStatus::Pending),
        };
        if !status_ok {
            return Err(PlanError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanSnapshot {
    pub schema_version: u32,
    pub subject_id: String,
    /// 整个账本的修订；任一计划变化加一。
    pub revision: u64,
    pub plans: Vec<Plan>,
}
impl PlanSnapshot {
    pub fn validate(&self) -> PlanResult<()> {
        validate_id(&self.subject_id)?;
        if self.schema_version != PLAN_SCHEMA_VERSION || self.plans.len() > MAX_PLANS {
            return Err(PlanError::InvalidInput);
        }
        let mut ids = BTreeSet::new();
        let mut active = BTreeSet::new();
        for plan in &self.plans {
            plan.validate(&self.subject_id)?;
            if !ids.insert(plan.id.as_str())
                || (plan.status == PlanStatus::Active
                    && !active.insert(plan.binding.goal_id.as_str()))
            {
                return Err(PlanError::InvalidInput);
            }
        }
        Ok(())
    }
}

pub struct PlanCreate {
    pub plan: Plan,
    pub duplicate: bool,
}

/// 计划账本。管理句柄只由宿主持有，不发布给模型或普通服务消费者。
/// 每次写入先持久化再在内存中发布；写入失败时旧状态保持不变，可能已提交须重新打开核对。
pub trait PlanJournal: Send + Sync {
    fn snapshot(&self) -> PlanResult<PlanSnapshot>;
    /// 相同内容幂等返回原计划；同一目标已有其他活动计划时冲突，先封存旧计划。
    fn create(
        &self,
        spec: PlanSpec,
        capabilities: &[CapabilitySpec],
        at_ms: u64,
    ) -> PlanResult<PlanCreate>;
    fn invalidate(
        &self,
        plan_id: &str,
        expected_revision: u64,
        current: &PlanBinding,
        at_ms: u64,
    ) -> PlanResult<Plan>;
    /// 保存 Executing 后才可执行能力；步骤必须就绪。
    fn begin_step(
        &self,
        plan_id: &str,
        step_id: &str,
        expected_revision: u64,
        at_ms: u64,
    ) -> PlanResult<Plan>;
    fn finish_step(
        &self,
        plan_id: &str,
        step_id: &str,
        expected_revision: u64,
        outcome: StepOutcome,
    ) -> PlanResult<Plan>;
}

pub fn validate_id(value: &str) -> PlanResult<()> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(PlanError::InvalidInput);
    }
    Ok(())
}
pub fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}
fn parse_json<T: DeserializeOwned>(text: &str, max_bytes: usize) -> PlanResult<T> {
    if text.is_empty() || text.len() > max_bytes {
        return Err(PlanError::InvalidInput);
    }
    serde_json::from_str(text).map_err(|_| PlanError::InvalidInput)
}
fn validate_encoded(value: &impl Serialize, max_bytes: usize) -> PlanResult<()> {
    if serde_json::to_vec(value)
        .map_err(|_| PlanError::InvalidInput)?
        .len()
        > max_bytes
    {
        return Err(PlanError::InvalidInput);
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
redacted!(
    PlanBinding,
    StepEvidence,
    EffectCondition,
    StepSpec,
    PlanSpec,
    StepAttempt,
    StepState,
    Plan,
    PlanSnapshot,
    PlanCreate,
    StepOutcome
);

#[cfg(test)]
mod tests {
    use super::*;

    const SUBJECT: &str = "eve";
    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn capabilities() -> Vec<CapabilitySpec> {
        vec![
            CapabilitySpec {
                id: "observe".into(),
                max_attempts: 3,
                max_timeout_ms: 30_000,
                requires_input: false,
            },
            CapabilitySpec {
                id: "export".into(),
                max_attempts: 1,
                max_timeout_ms: 30_000,
                requires_input: true,
            },
        ]
    }
    fn step(id: &str, capability: &str, depends_on: &[&str], effect: EffectCondition) -> StepSpec {
        StepSpec {
            id: id.into(),
            title: format!("步骤 {id}"),
            capability: capability.into(),
            depends_on: depends_on.iter().map(|id| id.to_string()).collect(),
            max_attempts: 1,
            timeout_ms: 1000,
            effect,
        }
    }
    fn spec() -> PlanSpec {
        let mut wait = step(
            "wait",
            "observe",
            &["export"],
            EffectCondition::DigestDiffers { sha256: A.into() },
        );
        wait.max_attempts = 2;
        PlanSpec {
            binding: PlanBinding {
                goal_id: "goal".into(),
                goal_revision: 2,
                input_sha256: Some(A.into()),
            },
            steps: vec![
                step(
                    "check",
                    "observe",
                    &[],
                    EffectCondition::DigestEquals { sha256: A.into() },
                ),
                step("export", "export", &["check"], EffectCondition::Verified),
                wait,
            ],
        }
    }
    fn plan() -> Plan {
        Plan::new(SUBJECT, spec(), &capabilities(), 10).unwrap()
    }
    fn evidence(capability: &str, sha256: &str, at: u64) -> StepOutcome {
        StepOutcome::Evidence(StepEvidence {
            capability: capability.into(),
            source_id: "source".into(),
            sha256: sha256.into(),
            bytes: 3,
            verified_at_ms: at,
        })
    }
    fn statuses(plan: &Plan) -> Vec<StepStatus> {
        plan.steps.iter().map(|step| step.status).collect()
    }

    #[test]
    fn steps_run_in_dependency_order_and_only_verified_effects_complete_the_plan() {
        let mut plan = plan();
        assert_eq!(plan.ready_steps(), ["check"]);
        assert_eq!(plan.begin_step("export", 11), Err(PlanError::NotReady));
        assert_eq!(plan.begin_step("check", 11), Ok(1));
        assert!(plan.ready_steps().is_empty(), "一次只执行一个步骤");
        plan.validate(SUBJECT).unwrap();
        plan.finish_step("check", evidence("observe", A, 12))
            .unwrap();
        assert_eq!(plan.ready_steps(), ["export"]);
        plan.begin_step("export", 13).unwrap();
        plan.finish_step("export", evidence("export", B, 14))
            .unwrap();
        // 输入还没变：DigestDiffers 不满足，可在剩余尝试内重试。
        plan.begin_step("wait", 15).unwrap();
        plan.finish_step("wait", evidence("observe", A, 16))
            .unwrap();
        assert_eq!(plan.step("wait").unwrap().status, StepStatus::Pending);
        assert_eq!(plan.status, PlanStatus::Active);
        plan.begin_step("wait", 17).unwrap();
        plan.finish_step("wait", evidence("observe", B, 18))
            .unwrap();
        assert_eq!(plan.status, PlanStatus::Completed);
        assert_eq!(statuses(&plan), [StepStatus::Satisfied; 3]);
        plan.validate(SUBJECT).unwrap();
        assert_eq!(plan.revision, 9);
    }

    #[test]
    fn unmet_effect_without_attempts_fails_and_blocks_remaining_steps() {
        let mut plan = plan();
        plan.begin_step("check", 11).unwrap();
        plan.finish_step("check", evidence("observe", B, 12))
            .unwrap();
        assert_eq!(plan.status, PlanStatus::Blocked);
        assert_eq!(
            statuses(&plan),
            [StepStatus::Failed, StepStatus::Blocked, StepStatus::Blocked]
        );
        let attempt = &plan.step("check").unwrap().attempts[0];
        assert_eq!(attempt.failure, Some(StepFailure::EffectNotMet));
        assert!(!attempt.effect_met);
        assert!(plan.ready_steps().is_empty());
        plan.validate(SUBJECT).unwrap();
    }

    #[test]
    fn evidence_from_another_capability_and_unfinished_steps_are_rejected() {
        let mut plan = plan();
        assert_eq!(
            plan.finish_step("check", evidence("observe", A, 12)),
            Err(PlanError::InvalidTransition)
        );
        plan.begin_step("check", 11).unwrap();
        assert_eq!(
            plan.finish_step("check", evidence("export", A, 12)),
            Err(PlanError::InvalidInput)
        );
        assert_eq!(
            plan.finish_step("check", evidence("observe", A, 5)),
            Err(PlanError::InvalidInput),
            "证据时间不能早于开始"
        );
        assert_eq!(plan.begin_step("check", 13), Err(PlanError::NotReady));
        let mut early = Plan::new(SUBJECT, spec(), &capabilities(), 10).unwrap();
        assert_eq!(early.begin_step("check", 9), Err(PlanError::InvalidInput));
    }

    #[test]
    fn changed_binding_invalidates_pending_work_and_executing_results_do_not_count() {
        let mut plan = plan();
        plan.begin_step("check", 11).unwrap();
        let mut current = plan.binding.clone();
        current.goal_revision = 3;
        assert!(plan.invalidate(&current, 12).unwrap());
        assert_eq!(plan.status, PlanStatus::Stale);
        assert_eq!(
            statuses(&plan),
            [
                StepStatus::Executing,
                StepStatus::Invalidated,
                StepStatus::Invalidated
            ]
        );
        plan.validate(SUBJECT).unwrap();
        plan.finish_step("check", evidence("observe", A, 13))
            .unwrap();
        let check = plan.step("check").unwrap();
        assert_eq!(check.status, StepStatus::Invalidated);
        assert!(!check.attempts[0].effect_met, "过时计划的证据不计为满足");
        assert!(check.attempts[0].evidence.is_some(), "证据仍保留");
        plan.validate(SUBJECT).unwrap();
        assert!(!plan.invalidate(&current, 14).unwrap(), "重复封存无变化");

        let mut changed_input = self::plan();
        let mut input = changed_input.binding.clone();
        input.input_sha256 = Some(B.into());
        assert!(changed_input.invalidate(&input, 12).unwrap());
        let mut other_goal = input.clone();
        other_goal.goal_id = "other".into();
        assert_eq!(
            changed_input.invalidate(&other_goal, 12),
            Err(PlanError::InvalidInput)
        );
    }

    #[test]
    fn completed_plans_keep_their_result_and_binding_change_found_at_execution_marks_stale() {
        let mut done = Plan::new(
            SUBJECT,
            PlanSpec {
                binding: spec().binding,
                steps: vec![step("only", "observe", &[], EffectCondition::Verified)],
            },
            &capabilities(),
            10,
        )
        .unwrap();
        done.begin_step("only", 11).unwrap();
        done.finish_step("only", evidence("observe", B, 12))
            .unwrap();
        assert_eq!(done.status, PlanStatus::Completed);
        let mut current = done.binding.clone();
        current.goal_revision = 9;
        assert!(!done.invalidate(&current, 13).unwrap());
        assert_eq!(done.status, PlanStatus::Completed);

        let mut plan = plan();
        plan.begin_step("check", 11).unwrap();
        plan.finish_step(
            "check",
            StepOutcome::Failed {
                failure: StepFailure::BindingChanged,
                at_ms: 12,
            },
        )
        .unwrap();
        assert_eq!(plan.status, PlanStatus::Stale);
        assert_eq!(plan.stale_at_ms, Some(12));
        assert_eq!(statuses(&plan), [StepStatus::Invalidated; 3]);
        plan.validate(SUBJECT).unwrap();
    }

    #[test]
    fn interruption_and_cancellation_block_without_retry() {
        let mut plan = plan();
        plan.begin_step("check", 11).unwrap();
        assert!(plan.interrupt(20).unwrap());
        assert_eq!(plan.status, PlanStatus::Blocked);
        let attempt = &plan.step("check").unwrap().attempts[0];
        assert_eq!(attempt.failure, Some(StepFailure::Interrupted));
        assert_eq!(attempt.finished_at_ms, Some(20));
        assert!(!plan.interrupt(21).unwrap());
        plan.validate(SUBJECT).unwrap();

        let mut cancelled = self::plan();
        cancelled.begin_step("check", 11).unwrap();
        cancelled
            .finish_step(
                "check",
                StepOutcome::Failed {
                    failure: StepFailure::Cancelled,
                    at_ms: 12,
                },
            )
            .unwrap();
        assert_eq!(cancelled.status, PlanStatus::Blocked);
        assert_eq!(cancelled.step("check").unwrap().status, StepStatus::Blocked);
    }

    #[test]
    fn plan_shape_capabilities_and_budgets_are_validated() {
        let caps = capabilities();
        assert!(spec().validate(&caps).is_ok());
        type Change = Box<dyn Fn(&mut PlanSpec)>;
        let cases: Vec<(&str, Change, PlanError)> = vec![
            (
                "empty",
                Box::new(|s| s.steps.clear()),
                PlanError::InvalidInput,
            ),
            (
                "too many",
                Box::new(|s| {
                    s.steps = (0..=MAX_STEPS)
                        .map(|i| step(&format!("s{i}"), "observe", &[], EffectCondition::Verified))
                        .collect()
                }),
                PlanError::InvalidInput,
            ),
            (
                "duplicate id",
                Box::new(|s| s.steps[1].id = "check".into()),
                PlanError::InvalidInput,
            ),
            (
                "unknown dependency",
                Box::new(|s| s.steps[1].depends_on = vec!["nope".into()]),
                PlanError::InvalidInput,
            ),
            (
                "self dependency",
                Box::new(|s| s.steps[0].depends_on = vec!["check".into()]),
                PlanError::InvalidInput,
            ),
            (
                "repeated dependency",
                Box::new(|s| s.steps[1].depends_on = vec!["check".into(), "check".into()]),
                PlanError::InvalidInput,
            ),
            (
                "cycle",
                Box::new(|s| s.steps[0].depends_on = vec!["wait".into()]),
                PlanError::InvalidInput,
            ),
            (
                "blank title",
                Box::new(|s| s.steps[0].title = " ".into()),
                PlanError::InvalidInput,
            ),
            (
                "long title",
                Box::new(|s| s.steps[0].title = "题".repeat(100)),
                PlanError::InvalidInput,
            ),
            (
                "zero attempts",
                Box::new(|s| s.steps[0].max_attempts = 0),
                PlanError::InvalidInput,
            ),
            (
                "zero timeout",
                Box::new(|s| s.steps[0].timeout_ms = 0),
                PlanError::InvalidInput,
            ),
            (
                "long timeout",
                Box::new(|s| s.steps[0].timeout_ms = MAX_STEP_TIMEOUT_MS + 1),
                PlanError::InvalidInput,
            ),
            (
                "bad digest",
                Box::new(|s| {
                    s.steps[0].effect = EffectCondition::DigestEquals {
                        sha256: "AB".into(),
                    }
                }),
                PlanError::InvalidInput,
            ),
            (
                "zero revision",
                Box::new(|s| s.binding.goal_revision = 0),
                PlanError::InvalidInput,
            ),
            (
                "bad input digest",
                Box::new(|s| s.binding.input_sha256 = Some("x".into())),
                PlanError::InvalidInput,
            ),
            (
                "unknown capability",
                Box::new(|s| s.steps[0].capability = "shell".into()),
                PlanError::UnknownCapability,
            ),
            (
                "attempts over capability",
                Box::new(|s| s.steps[1].max_attempts = 2),
                PlanError::UnknownCapability,
            ),
            (
                "input required",
                Box::new(|s| s.binding.input_sha256 = None),
                PlanError::UnknownCapability,
            ),
        ];
        for (name, change, expected) in cases {
            let mut candidate = spec();
            change(&mut candidate);
            assert_eq!(candidate.validate(&caps), Err(expected), "{name}");
        }
    }

    #[test]
    fn plan_ids_are_content_addressed_and_json_is_strict() {
        let id = spec().plan_id(SUBJECT).unwrap();
        assert!(id.starts_with("plan:") && is_sha256(&id[5..]));
        assert_eq!(id, spec().plan_id(SUBJECT).unwrap());
        let mut changed = spec();
        changed.steps[0].title = "另一个说明".into();
        assert_ne!(id, changed.plan_id(SUBJECT).unwrap());
        assert_ne!(id, spec().plan_id("other").unwrap());
        let text = serde_json::to_string(&spec()).unwrap();
        assert!(PlanSpec::parse(&text).is_ok());
        let unknown = text.replacen("\"steps\"", "\"extra\":1,\"steps\"", 1);
        assert_eq!(
            PlanSpec::parse(&unknown).unwrap_err(),
            PlanError::InvalidInput
        );
        let duplicate = text.replacen("\"steps\"", "\"binding\":null,\"steps\"", 1);
        assert_eq!(
            PlanSpec::parse(&duplicate).unwrap_err(),
            PlanError::InvalidInput
        );
        assert_eq!(
            PlanSpec::parse(&" ".repeat(MAX_PLAN_JSON_BYTES + 1)).unwrap_err(),
            PlanError::InvalidInput
        );
    }

    #[test]
    fn snapshots_reject_tampering_and_two_active_plans_for_one_goal() {
        let mut snapshot = PlanSnapshot {
            schema_version: PLAN_SCHEMA_VERSION,
            subject_id: SUBJECT.into(),
            revision: 1,
            plans: vec![plan()],
        };
        snapshot.validate().unwrap();
        let mut second = spec();
        second.steps[0].title = "另一份".into();
        snapshot
            .plans
            .push(Plan::new(SUBJECT, second, &capabilities(), 10).unwrap());
        assert_eq!(snapshot.validate(), Err(PlanError::InvalidInput));
        snapshot.plans.pop();
        for tamper in [
            |p: &mut Plan| p.id = "plan:forged".into(),
            |p: &mut Plan| p.steps[0].status = StepStatus::Satisfied,
            |p: &mut Plan| p.status = PlanStatus::Completed,
            |p: &mut Plan| p.stale_at_ms = Some(1),
        ] {
            let mut copy = snapshot.clone();
            tamper(&mut copy.plans[0]);
            assert_eq!(copy.validate(), Err(PlanError::InvalidInput));
        }
        let mut other = snapshot.clone();
        other.subject_id = "other".into();
        assert_eq!(other.validate(), Err(PlanError::InvalidInput));
    }

    #[test]
    fn debug_output_redacts_plan_contents() {
        let text = format!("{:?} {:?}", plan(), spec());
        assert_eq!(text, "Plan(<redacted>) PlanSpec(<redacted>)");
    }
}
