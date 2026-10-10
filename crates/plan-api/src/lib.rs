//! 有条件多步计划的公开契约：步骤依赖、预算、绑定输入与可核对的效果条件。
//!
//! 不包含存储、模型、文件或执行实现。计划引用能力 ID 不授予任何权限；宿主须登记能力上限，
//! 并在执行前重新核对绑定输入。步骤效果只由宿主独立读取得到的证据判定，调用方不能自报完成。
//! 模型只能提出建议：建议经宿主校验后保存为待确认计划，操作者确认前不准入任何步骤。
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    future::Future,
    pin::Pin,
};

pub const PLAN_PLUGIN_ID: &str = "eve.plan";
/// 当前账本版本。版本 1 仍可读取（没有建议记录与模型来源），下次写入时保存为当前版本。
pub const PLAN_SCHEMA_VERSION: u32 = 2;
pub const PLAN_SCHEMA_VERSION_V1: u32 = 1;
/// 每个主体的计划记录上限；达到后新建计划明确失败，不淘汰旧记录。
pub const MAX_PLANS: usize = 64;
/// 每个主体的建议请求记录上限；达到后不再请求模型，不淘汰旧记录。
pub const MAX_PROPOSALS: usize = 64;
/// 交给建议器的目标与草稿文本上限（各自的 UTF-8 字节）。
pub const MAX_PROPOSAL_TEXT_BYTES: usize = 8192;
pub const MAX_STEPS: usize = 8;
pub const MAX_STEP_ATTEMPTS: u8 = 3;
/// 任何步骤的期限上限；每种能力在登记时再给出自己的上限（文件观察与导出仍为 30 秒，
/// 研究与实践这类后台能力可到数分钟）。
pub const MAX_STEP_TIMEOUT_MS: u64 = 1_800_000;
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
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CapabilitySpec {
    pub id: String,
    /// 给操作者与建议器看的能力说明；只是数据，不扩大能力范围。
    pub description: String,
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
        Ok(format!(
            "plan:{}",
            derive(&[b"eve.plan:v1", subject_id.as_bytes(), &encoded])
        ))
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
    /// 模型建议经宿主校验后保存，等待操作者确认；不准入任何步骤。
    Proposed,
    Active,
    /// 全部步骤的效果条件都由独立证据满足；不代表父目标或现实任务已经完成。
    Completed,
    /// 有步骤失败、中断或被取消，计划不再准入新步骤。
    Blocked,
    /// 绑定输入已变化；保留历史，不再准入步骤。
    Stale,
    /// 操作者撤销待确认或活动计划；未开始的步骤阻塞，保留历史。
    Withdrawn,
}

/// 计划的提出者。来源不授予权限：模型建议须经操作者确认后才活动。
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanOrigin {
    /// 本地操作者提供，建立即活动。
    #[default]
    Operator,
    /// 模型建议；对应一条建议请求记录。
    Model { proposal_id: String },
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
    /// 版本 1 账本没有该字段，读取为操作者计划。
    #[serde(default)]
    pub origin: PlanOrigin,
    /// 操作者确认模型建议的时间；操作者计划为空。
    #[serde(default)]
    pub confirmed_at_ms: Option<u64>,
    #[serde(default)]
    pub withdrawn_at_ms: Option<u64>,
}
impl Plan {
    pub fn new(
        subject_id: &str,
        spec: PlanSpec,
        capabilities: &[CapabilitySpec],
        created_at_ms: u64,
    ) -> PlanResult<Self> {
        Self::build(
            subject_id,
            spec,
            capabilities,
            created_at_ms,
            PlanOrigin::Operator,
        )
    }

    /// 模型建议的待确认计划；与操作者计划同样按登记能力校验，但不准入步骤。
    pub fn proposed(
        subject_id: &str,
        spec: PlanSpec,
        capabilities: &[CapabilitySpec],
        created_at_ms: u64,
        proposal_id: &str,
    ) -> PlanResult<Self> {
        validate_id(proposal_id)?;
        Self::build(
            subject_id,
            spec,
            capabilities,
            created_at_ms,
            PlanOrigin::Model {
                proposal_id: proposal_id.into(),
            },
        )
    }

    fn build(
        subject_id: &str,
        spec: PlanSpec,
        capabilities: &[CapabilitySpec],
        created_at_ms: u64,
        origin: PlanOrigin,
    ) -> PlanResult<Self> {
        spec.validate(capabilities)?;
        if created_at_ms == 0 {
            return Err(PlanError::InvalidInput);
        }
        let status = match origin {
            PlanOrigin::Operator => PlanStatus::Active,
            PlanOrigin::Model { .. } => PlanStatus::Proposed,
        };
        Ok(Self {
            id: plan_id_for(&spec, subject_id, &origin)?,
            revision: 1,
            created_at_ms,
            status,
            stale_at_ms: None,
            origin,
            confirmed_at_ms: None,
            withdrawn_at_ms: None,
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
        if at_ms < self.admitted_at_ms() {
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
    /// 待确认的建议同样封存。已完成、已阻塞或已撤销的计划保留原结论；返回是否发生变化。
    pub fn invalidate(&mut self, current: &PlanBinding, at_ms: u64) -> PlanResult<bool> {
        current.validate()?;
        if at_ms == 0 || current.goal_id != self.binding.goal_id {
            return Err(PlanError::InvalidInput);
        }
        if !self.is_open() || current == &self.binding {
            return Ok(false);
        }
        self.mark_stale(at_ms);
        self.bump()?;
        Ok(true)
    }

    /// 操作者确认模型建议。绑定仍成立时转为 Active 并返回 true；已变化时封存为 Stale，
    /// 返回 false，任何步骤都不会因确认过时的建议而执行。
    pub fn confirm(&mut self, current: &PlanBinding, at_ms: u64) -> PlanResult<bool> {
        current.validate()?;
        if self.status != PlanStatus::Proposed {
            return Err(PlanError::InvalidTransition);
        }
        if at_ms < self.created_at_ms || current.goal_id != self.binding.goal_id {
            return Err(PlanError::InvalidInput);
        }
        if current != &self.binding {
            self.mark_stale(at_ms);
        } else {
            self.status = PlanStatus::Active;
            self.confirmed_at_ms = Some(at_ms);
        }
        self.bump()?;
        Ok(self.status == PlanStatus::Active)
    }

    /// 操作者撤销待确认或活动计划。有步骤执行中时拒绝，避免掩盖可能已发生的副作用；
    /// 未开始的步骤阻塞，已满足步骤的证据保留。
    pub fn withdraw(&mut self, at_ms: u64) -> PlanResult<()> {
        if !self.is_open() {
            return Err(PlanError::InvalidTransition);
        }
        if self
            .steps
            .iter()
            .any(|step| step.status == StepStatus::Executing)
        {
            return Err(PlanError::NotReady);
        }
        if at_ms < self.admitted_at_ms() {
            return Err(PlanError::InvalidInput);
        }
        self.status = PlanStatus::Withdrawn;
        self.withdrawn_at_ms = Some(at_ms);
        for step in &mut self.steps {
            if step.status == StepStatus::Pending {
                step.status = StepStatus::Blocked;
            }
        }
        self.bump()
    }

    /// 待确认或活动：同一目标同一时间至多一份，避免叠加执行预算或并行建议。
    pub fn is_open(&self) -> bool {
        matches!(self.status, PlanStatus::Proposed | PlanStatus::Active)
    }

    /// 计划开始准入步骤的时间：模型建议以确认时间为准。
    fn admitted_at_ms(&self) -> u64 {
        self.confirmed_at_ms.unwrap_or(self.created_at_ms)
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
        let model = match &self.origin {
            PlanOrigin::Operator => false,
            PlanOrigin::Model { proposal_id } => {
                validate_id(proposal_id)?;
                true
            }
        };
        // 模型建议只有经确认才可能执行过步骤或得出完成/阻塞结论。
        let confirmed_ok = match self.confirmed_at_ms {
            None if !model => self.status != PlanStatus::Proposed,
            None => matches!(
                self.status,
                PlanStatus::Proposed | PlanStatus::Stale | PlanStatus::Withdrawn
            ),
            Some(at) => model && self.status != PlanStatus::Proposed && at >= self.created_at_ms,
        };
        if self.id != plan_id_for(&spec, subject_id, &self.origin)?
            || self.revision == 0
            || self.created_at_ms == 0
            || !confirmed_ok
            || (self.status == PlanStatus::Stale) != self.stale_at_ms.is_some()
            || (self.status == PlanStatus::Withdrawn) != self.withdrawn_at_ms.is_some()
            || self
                .withdrawn_at_ms
                .is_some_and(|at| at < self.admitted_at_ms())
            || (model
                && self.confirmed_at_ms.is_none()
                && self.steps.iter().any(|step| !step.attempts.is_empty()))
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
                    || attempt.started_at_ms < self.admitted_at_ms()
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
            PlanStatus::Proposed => all(StepStatus::Pending),
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
            PlanStatus::Withdrawn => {
                executing == 0
                    && !any(StepStatus::Pending)
                    && !any(StepStatus::Failed)
                    && !any(StepStatus::Invalidated)
            }
        };
        if !status_ok {
            return Err(PlanError::InvalidInput);
        }
        Ok(())
    }
}

/// 内容寻址的计划 ID。操作者计划沿用版本 1 的派生方式；模型建议另含建议记录 ID，
/// 不会与内容相同的操作者计划混同。
fn plan_id_for(spec: &PlanSpec, subject_id: &str, origin: &PlanOrigin) -> PlanResult<String> {
    match origin {
        PlanOrigin::Operator => spec.plan_id(subject_id),
        PlanOrigin::Model { proposal_id } => {
            validate_id(proposal_id)?;
            let encoded = serde_json::to_vec(spec).map_err(|_| PlanError::InvalidInput)?;
            Ok(format!(
                "plan:{}",
                derive(&[
                    b"eve.plan.model:v1",
                    subject_id.as_bytes(),
                    proposal_id.as_bytes(),
                    &encoded,
                ])
            ))
        }
    }
}

fn derive(parts: &[&[u8]]) -> String {
    let mut digest = Context::new(&SHA256);
    for part in parts {
        digest.update(&(part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    hex(digest.finish().as_ref())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalFailure {
    Provider,
    Timeout,
    Cancelled,
    /// 输出不是严格的计划 JSON，或超出宿主登记的能力与上限。
    InvalidOutput,
    /// 请求期间进程退出；模型请求可能已经发生，不自动重放。
    Interrupted,
    /// 请求结束时目标修订或输入摘要已变化，建议不再对应当前目标。
    BindingChanged,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProposalStatus {
    /// 已保存请求记录，模型请求可能已经发出。
    Requested,
    /// 校验通过并保存为待确认计划。
    Proposed {
        plan_id: String,
    },
    /// 建议器认为登记能力不足以形成计划。
    Empty,
    Failed {
        failure: ProposalFailure,
    },
}

/// 一次建议请求的记录。每个计划绑定至多一条，用于限制模型请求次数。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalRecord {
    pub id: String,
    pub binding: PlanBinding,
    /// 建议器实现的稳定版本；不是模型名。
    pub proposer: String,
    pub requested_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub status: ProposalStatus,
}
impl ProposalRecord {
    pub fn new(
        subject_id: &str,
        binding: PlanBinding,
        proposer: &str,
        requested_at_ms: u64,
    ) -> PlanResult<Self> {
        let record = Self {
            id: proposal_id(subject_id, &binding)?,
            binding,
            proposer: proposer.into(),
            requested_at_ms,
            finished_at_ms: None,
            status: ProposalStatus::Requested,
        };
        record.validate(subject_id)?;
        Ok(record)
    }

    pub fn validate(&self, subject_id: &str) -> PlanResult<()> {
        self.binding.validate()?;
        validate_id(&self.proposer)?;
        let open = self.status == ProposalStatus::Requested;
        if self.id != proposal_id(subject_id, &self.binding)?
            || self.requested_at_ms == 0
            || open != self.finished_at_ms.is_none()
            || self
                .finished_at_ms
                .is_some_and(|at| at < self.requested_at_ms)
        {
            return Err(PlanError::InvalidInput);
        }
        if let ProposalStatus::Proposed { plan_id } = &self.status {
            validate_id(plan_id)?;
        }
        Ok(())
    }

    /// 重启恢复：遗留 Requested 记为 Interrupted，不重放可能已发生的模型请求。
    pub fn interrupt(&mut self, at_ms: u64) -> bool {
        if self.status != ProposalStatus::Requested {
            return false;
        }
        self.status = ProposalStatus::Failed {
            failure: ProposalFailure::Interrupted,
        };
        self.finished_at_ms = Some(at_ms.max(self.requested_at_ms));
        true
    }
}

/// 同一主体同一绑定只有一个建议记录 ID；目标修订或输入变化后才可再次请求。
pub fn proposal_id(subject_id: &str, binding: &PlanBinding) -> PlanResult<String> {
    validate_id(subject_id)?;
    binding.validate()?;
    let encoded = serde_json::to_vec(binding).map_err(|_| PlanError::InvalidInput)?;
    Ok(format!(
        "proposal:{}",
        derive(&[b"eve.plan.proposal:v1", subject_id.as_bytes(), &encoded])
    ))
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanSnapshot {
    pub schema_version: u32,
    pub subject_id: String,
    /// 整个账本的修订；任一计划变化加一。
    pub revision: u64,
    pub plans: Vec<Plan>,
    /// 版本 1 账本没有建议记录。
    #[serde(default)]
    pub proposals: Vec<ProposalRecord>,
}
impl PlanSnapshot {
    /// 把版本 1 账本升级为当前版本的内存形态；版本 1 不得含有建议、模型来源或新状态。
    pub fn upgrade(mut self) -> PlanResult<Self> {
        if self.schema_version == PLAN_SCHEMA_VERSION_V1 {
            if !self.proposals.is_empty()
                || self.plans.iter().any(|plan| {
                    plan.origin != PlanOrigin::Operator
                        || plan.confirmed_at_ms.is_some()
                        || plan.withdrawn_at_ms.is_some()
                        || matches!(plan.status, PlanStatus::Proposed | PlanStatus::Withdrawn)
                })
            {
                return Err(PlanError::InvalidInput);
            }
            self.schema_version = PLAN_SCHEMA_VERSION;
        }
        self.validate()?;
        Ok(self)
    }

    pub fn validate(&self) -> PlanResult<()> {
        validate_id(&self.subject_id)?;
        if self.schema_version != PLAN_SCHEMA_VERSION
            || self.plans.len() > MAX_PLANS
            || self.proposals.len() > MAX_PROPOSALS
        {
            return Err(PlanError::InvalidInput);
        }
        let mut ids = BTreeSet::new();
        // 同一目标至多一份待确认/活动计划或一条进行中的建议请求。
        let mut open = BTreeSet::new();
        for plan in &self.plans {
            plan.validate(&self.subject_id)?;
            if !ids.insert(plan.id.as_str())
                || (plan.is_open() && !open.insert(plan.binding.goal_id.as_str()))
            {
                return Err(PlanError::InvalidInput);
            }
        }
        let mut proposals = BTreeMap::new();
        for record in &self.proposals {
            record.validate(&self.subject_id)?;
            if proposals.insert(record.id.as_str(), record).is_some()
                || (record.status == ProposalStatus::Requested
                    && !open.insert(record.binding.goal_id.as_str()))
            {
                return Err(PlanError::InvalidInput);
            }
            // 建议记录与它保存的计划互相对应，绑定一致。
            if let ProposalStatus::Proposed { plan_id } = &record.status
                && !self.plans.iter().any(|plan| {
                    &plan.id == plan_id
                        && plan.binding == record.binding
                        && plan.created_at_ms == record.finished_at_ms.unwrap_or(0)
                        && matches!(&plan.origin, PlanOrigin::Model { proposal_id } if proposal_id == &record.id)
                })
            {
                return Err(PlanError::InvalidInput);
            }
        }
        for plan in &self.plans {
            if let PlanOrigin::Model { proposal_id } = &plan.origin
                && !proposals.get(proposal_id.as_str()).is_some_and(|record| {
                    matches!(&record.status, ProposalStatus::Proposed { plan_id } if plan_id == &plan.id)
                })
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

pub struct ProposalReservation {
    pub record: ProposalRecord,
    /// 该绑定已有记录：不得再次请求模型。
    pub duplicate: bool,
}

/// 建议器返回后宿主提交的结果。步骤由账本按记录绑定与登记能力重新校验。
pub enum ProposalOutcome {
    Steps(Vec<StepSpec>),
    Empty,
    Failed(ProposalFailure),
}

pub struct ProposalFinish {
    pub record: ProposalRecord,
    /// 校验通过时保存的待确认计划。
    pub plan: Option<Plan>,
}

/// 交给建议器的能力说明与上限。
#[derive(Clone, Serialize)]
pub struct ProposalCapability {
    pub id: String,
    pub description: String,
    pub max_attempts: u8,
    pub max_timeout_ms: u64,
    pub requires_input: bool,
}

/// 当前目标修订已验证的反思草稿；仍是未经验证的模型文本。
#[derive(Clone, Serialize)]
pub struct ProposalDraft {
    pub summary: String,
    pub next_step: String,
}

/// 交给建议器的全部数据。目标与草稿只是待分析的数据，不是指令，也不授予权限。
#[derive(Clone, Serialize)]
pub struct ProposalRequest {
    pub proposal_id: String,
    pub binding: PlanBinding,
    pub goal: String,
    pub draft: Option<ProposalDraft>,
    pub capabilities: Vec<ProposalCapability>,
    pub max_steps: usize,
}
impl ProposalRequest {
    pub fn new(
        subject_id: &str,
        binding: PlanBinding,
        goal: String,
        draft: Option<ProposalDraft>,
        capabilities: &[CapabilitySpec],
    ) -> PlanResult<Self> {
        let request = Self {
            proposal_id: proposal_id(subject_id, &binding)?,
            binding,
            goal,
            draft,
            capabilities: capabilities
                .iter()
                .map(|capability| ProposalCapability {
                    id: capability.id.clone(),
                    description: capability.description.clone(),
                    max_attempts: capability.max_attempts,
                    max_timeout_ms: capability.max_timeout_ms,
                    requires_input: capability.requires_input,
                })
                .collect(),
            max_steps: MAX_STEPS,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> PlanResult<()> {
        validate_id(&self.proposal_id)?;
        self.binding.validate()?;
        let text = |value: &str| {
            !value.trim().is_empty()
                && value.len() <= MAX_PROPOSAL_TEXT_BYTES
                && !value.contains('\0')
        };
        if !text(&self.goal)
            || self
                .draft
                .as_ref()
                .is_some_and(|draft| !text(&draft.summary) || !text(&draft.next_step))
            || self.capabilities.is_empty()
            || self.max_steps != MAX_STEPS
        {
            return Err(PlanError::InvalidInput);
        }
        for capability in &self.capabilities {
            validate_id(&capability.id)?;
            if !text(&capability.description) {
                return Err(PlanError::InvalidInput);
            }
        }
        Ok(())
    }
}

pub type ProposalFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<StepSpec>, ProposalFailure>> + Send + 'a>>;

/// 可替换的计划建议器。至多发起一次无工具请求；空列表表示没有可行计划。
/// 返回的步骤只是建议：宿主按登记能力校验后保存为待确认计划，由操作者决定是否确认。
pub trait PlanProposer: Send + Sync {
    fn version(&self) -> &str;
    fn propose(&self, request: ProposalRequest) -> ProposalFuture<'_>;
}

/// 解析建议器输出的严格计划步骤 JSON：`{"steps": [...]}`，拒绝未知字段与重复键。
pub fn parse_proposed_steps(text: &str) -> PlanResult<Vec<StepSpec>> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Steps {
        steps: Vec<StepSpec>,
    }
    let steps: Steps = parse_json(text, MAX_PLAN_JSON_BYTES)?;
    if steps.steps.len() > MAX_STEPS {
        return Err(PlanError::InvalidInput);
    }
    Ok(steps.steps)
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
    /// 先保存 Requested 再允许模型请求。同一绑定已有记录时幂等返回且不得再次请求；
    /// 同一目标已有待确认/活动计划或进行中的请求时冲突。
    fn reserve_proposal(
        &self,
        _binding: &PlanBinding,
        _proposer: &str,
        _at_ms: u64,
    ) -> PlanResult<ProposalReservation> {
        Err(PlanError::Unavailable)
    }
    /// 一次保存建议结果与待确认计划；步骤不合规时记为 InvalidOutput，不保存计划。
    fn finish_proposal(
        &self,
        _proposal_id: &str,
        _outcome: ProposalOutcome,
        _capabilities: &[CapabilitySpec],
        _at_ms: u64,
    ) -> PlanResult<ProposalFinish> {
        Err(PlanError::Unavailable)
    }
    /// 操作者确认建议；绑定已变化时封存为 Stale。
    fn confirm(
        &self,
        _plan_id: &str,
        _expected_revision: u64,
        _current: &PlanBinding,
        _at_ms: u64,
    ) -> PlanResult<Plan> {
        Err(PlanError::Unavailable)
    }
    fn withdraw(&self, _plan_id: &str, _expected_revision: u64, _at_ms: u64) -> PlanResult<Plan> {
        Err(PlanError::Unavailable)
    }
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
    StepOutcome,
    ProposalRecord,
    ProposalReservation,
    ProposalOutcome,
    ProposalFinish,
    ProposalRequest,
    ProposalDraft
);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SUBJECT: &str = "eve";
    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn capabilities() -> Vec<CapabilitySpec> {
        vec![
            CapabilitySpec {
                id: "observe".into(),
                description: "读取文件".into(),
                max_attempts: 3,
                max_timeout_ms: 30_000,
                requires_input: false,
            },
            CapabilitySpec {
                id: "export".into(),
                description: "导出草稿".into(),
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
            proposals: Vec::new(),
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

    /// 模型建议的账本：建议记录与它保存的待确认计划。
    fn proposed() -> (ProposalRecord, Plan) {
        let mut record = ProposalRecord::new(SUBJECT, spec().binding, "proposer:v1", 5).unwrap();
        let plan = Plan::proposed(SUBJECT, spec(), &capabilities(), 10, &record.id).unwrap();
        record.status = ProposalStatus::Proposed {
            plan_id: plan.id.clone(),
        };
        record.finished_at_ms = Some(10);
        (record, plan)
    }
    fn ledger(record: ProposalRecord, plan: Plan) -> PlanSnapshot {
        PlanSnapshot {
            schema_version: PLAN_SCHEMA_VERSION,
            subject_id: SUBJECT.into(),
            revision: 1,
            plans: vec![plan],
            proposals: vec![record],
        }
    }

    #[test]
    fn model_proposals_wait_for_confirmation_and_never_share_operator_ids() {
        let (record, mut plan) = proposed();
        assert_eq!(plan.status, PlanStatus::Proposed);
        assert_eq!(
            plan.origin,
            PlanOrigin::Model {
                proposal_id: record.id.clone()
            }
        );
        assert_ne!(
            plan.id,
            self::plan().id,
            "模型建议不与内容相同的操作者计划混同"
        );
        assert!(plan.ready_steps().is_empty(), "未确认的建议不准入步骤");
        assert_eq!(plan.begin_step("check", 11), Err(PlanError::NotReady));
        ledger(record.clone(), plan.clone()).validate().unwrap();

        assert_eq!(
            plan.confirm(&plan.binding.clone(), 9),
            Err(PlanError::InvalidInput)
        );
        assert!(plan.confirm(&plan.binding.clone(), 20).unwrap());
        assert_eq!(plan.status, PlanStatus::Active);
        assert_eq!(plan.confirmed_at_ms, Some(20));
        assert_eq!(plan.ready_steps(), ["check"]);
        assert_eq!(
            plan.begin_step("check", 15),
            Err(PlanError::InvalidInput),
            "步骤不能早于确认"
        );
        plan.begin_step("check", 21).unwrap();
        assert_eq!(
            plan.confirm(&plan.binding.clone(), 22),
            Err(PlanError::InvalidTransition)
        );
        ledger(record.clone(), plan).validate().unwrap();

        // 确认时绑定已变化：封存为过时，任何步骤都不执行。
        let (_, mut late) = proposed();
        let mut current = late.binding.clone();
        current.goal_revision = 3;
        assert!(!late.confirm(&current, 20).unwrap());
        assert_eq!(late.status, PlanStatus::Stale);
        assert_eq!(late.confirmed_at_ms, None);
        assert_eq!(statuses(&late), [StepStatus::Invalidated; 3]);
        ledger(record.clone(), late).validate().unwrap();

        // 待确认的建议同样随绑定变化封存。
        let (_, mut pending) = proposed();
        assert!(pending.invalidate(&current, 20).unwrap());
        assert_eq!(pending.status, PlanStatus::Stale);
        ledger(record, pending).validate().unwrap();
    }

    #[test]
    fn operators_withdraw_open_plans_without_hiding_running_steps() {
        let (record, mut plan) = proposed();
        plan.withdraw(12).unwrap();
        assert_eq!(plan.status, PlanStatus::Withdrawn);
        assert_eq!(statuses(&plan), [StepStatus::Blocked; 3]);
        assert_eq!(
            plan.confirm(&plan.binding.clone(), 13),
            Err(PlanError::InvalidTransition)
        );
        assert_eq!(plan.withdraw(13), Err(PlanError::InvalidTransition));
        ledger(record, plan).validate().unwrap();

        let mut active = self::plan();
        active.begin_step("check", 11).unwrap();
        assert_eq!(
            active.withdraw(12),
            Err(PlanError::NotReady),
            "执行中的步骤不能被撤销掩盖"
        );
        active
            .finish_step("check", evidence("observe", A, 12))
            .unwrap();
        active.withdraw(13).unwrap();
        assert_eq!(
            statuses(&active),
            [
                StepStatus::Satisfied,
                StepStatus::Blocked,
                StepStatus::Blocked
            ]
        );
        assert_eq!(active.withdrawn_at_ms, Some(13));
        active.validate(SUBJECT).unwrap();
        let mut current = active.binding.clone();
        current.goal_revision = 7;
        assert!(
            !active.invalidate(&current, 14).unwrap(),
            "撤销后保留原结论"
        );
    }

    #[test]
    fn proposal_records_bind_one_request_and_match_their_saved_plan() {
        let (record, plan) = proposed();
        assert!(record.id.starts_with("proposal:"));
        let mut other = spec().binding;
        other.goal_revision = 3;
        assert_ne!(
            record.id,
            proposal_id(SUBJECT, &other).unwrap(),
            "新修订才能再次请求"
        );
        assert_eq!(record.id, proposal_id(SUBJECT, &spec().binding).unwrap());

        let mut open = ProposalRecord::new(SUBJECT, other, "proposer:v1", 5).unwrap();
        assert!(open.interrupt(3));
        assert_eq!(open.finished_at_ms, Some(5));
        assert_eq!(
            open.status,
            ProposalStatus::Failed {
                failure: ProposalFailure::Interrupted
            }
        );
        assert!(!open.interrupt(9));
        open.validate(SUBJECT).unwrap();

        type Tamper = fn(&mut PlanSnapshot);
        let cases: [(&str, Tamper); 8] = [
            ("forged record id", |l| {
                l.proposals[0].id = "proposal:x".into()
            }),
            ("plan without record", |l| l.proposals.clear()),
            ("record without plan", |l| l.plans.clear()),
            ("operator origin", |l| {
                l.plans[0].origin = PlanOrigin::Operator
            }),
            ("unconfirmed attempts", |l| {
                l.plans[0].status = PlanStatus::Active;
            }),
            ("finished without time", |l| {
                l.proposals[0].finished_at_ms = None
            }),
            ("requested beside open plan", |l| {
                let mut record = ProposalRecord::new(SUBJECT, spec().binding, "p", 5).unwrap();
                record.binding.goal_revision = 9;
                record.id = proposal_id(SUBJECT, &record.binding).unwrap();
                l.proposals.push(record);
            }),
            ("duplicate record", |l| {
                let copy = l.proposals[0].clone();
                l.proposals.push(copy);
            }),
        ];
        for (name, tamper) in cases {
            let mut copy = ledger(record.clone(), plan.clone());
            tamper(&mut copy);
            assert_eq!(copy.validate(), Err(PlanError::InvalidInput), "{name}");
        }
    }

    #[test]
    fn version_one_ledgers_upgrade_only_without_new_fields() {
        let v1 = PlanSnapshot {
            schema_version: PLAN_SCHEMA_VERSION_V1,
            subject_id: SUBJECT.into(),
            revision: 3,
            plans: vec![plan()],
            proposals: Vec::new(),
        };
        let mut json = serde_json::to_value(&v1).unwrap();
        json.as_object_mut().unwrap().remove("proposals");
        for field in ["origin", "confirmed_at_ms", "withdrawn_at_ms"] {
            json["plans"][0].as_object_mut().unwrap().remove(field);
        }
        let parsed: PlanSnapshot = serde_json::from_value(json).unwrap();
        assert_eq!(
            parsed.validate(),
            Err(PlanError::InvalidInput),
            "旧版本不直接通过"
        );
        let upgraded = parsed.upgrade().unwrap();
        assert_eq!(upgraded.schema_version, PLAN_SCHEMA_VERSION);
        assert_eq!(upgraded.plans[0].origin, PlanOrigin::Operator);

        let (record, plan) = proposed();
        let mut forged = ledger(record, plan);
        forged.schema_version = PLAN_SCHEMA_VERSION_V1;
        assert_eq!(forged.upgrade().unwrap_err(), PlanError::InvalidInput);
        let mut future = PlanSnapshot {
            schema_version: 3,
            ..v1
        };
        assert_eq!(
            future.clone().upgrade().unwrap_err(),
            PlanError::InvalidInput
        );
        future.schema_version = PLAN_SCHEMA_VERSION;
        future.upgrade().unwrap();
    }

    #[test]
    fn proposal_requests_and_outputs_are_bounded_and_strict() {
        let request = ProposalRequest::new(
            SUBJECT,
            spec().binding,
            "整理材料".into(),
            Some(ProposalDraft {
                summary: "先核对".into(),
                next_step: "再导出".into(),
            }),
            &capabilities(),
        )
        .unwrap();
        assert_eq!(
            request.proposal_id,
            proposal_id(SUBJECT, &spec().binding).unwrap()
        );
        assert_eq!(request.capabilities.len(), 2);
        assert!(
            ProposalRequest::new(SUBJECT, spec().binding, " ".into(), None, &capabilities())
                .is_err()
        );
        assert!(
            ProposalRequest::new(
                SUBJECT,
                spec().binding,
                "x".repeat(MAX_PROPOSAL_TEXT_BYTES + 1),
                None,
                &capabilities()
            )
            .is_err()
        );
        assert!(ProposalRequest::new(SUBJECT, spec().binding, "目标".into(), None, &[]).is_err());

        let text = json!({"steps": spec().steps}).to_string();
        assert_eq!(parse_proposed_steps(&text).unwrap().len(), 3);
        assert!(parse_proposed_steps(r#"{"steps":[]}"#).unwrap().is_empty());
        for bad in [
            format!("```json\n{text}\n```"),
            text.replacen("{\"steps\"", "{\"note\":\"x\",\"steps\"", 1),
            text.replacen("\"title\"", "\"title\":\"a\",\"title\"", 1),
            r#"{"steps":[],"steps":[]}"#.into(),
            json!({"steps": (0..=MAX_STEPS).map(|i| step(&format!("s{i}"), "observe", &[], EffectCondition::Verified)).collect::<Vec<_>>()}).to_string(),
        ] {
            assert_eq!(parse_proposed_steps(&bad), Err(PlanError::InvalidInput), "{bad}");
        }
    }

    #[test]
    fn debug_output_redacts_plan_contents() {
        let text = format!("{:?} {:?}", plan(), spec());
        assert_eq!(text, "Plan(<redacted>) PlanSpec(<redacted>)");
    }
}
