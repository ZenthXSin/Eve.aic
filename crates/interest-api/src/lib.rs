//! 普通对话中用户明确表达的兴趣、经验与困难，以及由此派生学习目标的来源契约。
//!
//! 兴趣只记录能在已送达交互的用户原文中逐字核对的陈述；模型推断单独标注，
//! 不冒充用户指令。观察器、账本与目标派生器均可替换，宿主仍负责作用域绑定。
use eve_memory_api::{
    EvidenceSource, InteractionEvidence, MemoryError, MemoryScope, MemorySnapshot, validate_id,
    validate_text,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt, future::Future, pin::Pin};

pub const INTEREST_PLUGIN_ID: &str = "eve.interest";
pub const INTEREST_STATE_KEY: &str = "interests.v1";
/// 派生学习目标在认知状态中的来源通道；组合层据此授权反思准入。
pub const INTEREST_GOAL_CHANNEL: &str = "interest.learning";
/// 派生学习目标的验证标记；不同于用户待办的 `user-goal:v1`。
pub const INTEREST_GOAL_VERIFICATION: &str = "interest-learning:v1";
/// 观察批次总数，不自动淘汰；达到容量后保留原记录并停止新观察。
pub const MAX_JOBS: usize = 128;
/// 所有作用域的兴趣记录总数，不自动淘汰。
pub const MAX_INTERESTS: usize = 64;
/// 单条兴趣保存的用户陈述上限；超过时新陈述被拒绝并记入批次结果。
pub const MAX_STATEMENTS: usize = 16;
/// 单条兴趣保存的模型推断上限；超过时只保留已有推断。
pub const MAX_INFERENCES: usize = 8;
pub const MAX_BATCH_EVIDENCE: usize = 8;
/// 交给观察器用于关联与撤回的已有活动兴趣数量上限。
pub const MAX_KNOWN_INTERESTS: usize = 16;
pub const MAX_EVIDENCE_BYTES: usize = 8192;
pub const MAX_INPUT_BYTES: usize = 32768;
pub const MAX_OUTPUT_BYTES: usize = 8192;
pub const MAX_UPDATES: usize = 3;
pub const MAX_UPDATE_STATEMENTS: usize = 4;
pub const MAX_TOPIC_BYTES: usize = 128;
pub const MAX_QUOTE_BYTES: usize = 512;
pub const MAX_INFERENCE_BYTES: usize = 512;
pub const MAX_STATE_BYTES: usize = 4 * 1024 * 1024;
pub type InterestResult<T> = Result<T, InterestError>;
pub type InterestFuture<'a, T> = Pin<Box<dyn Future<Output = InterestResult<T>> + Send + 'a>>;

/// 低频观察节奏；默认每条新送达的交互都可触发，但同一作用域受冷却限制。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservationOptions {
    pub min_new_evidence: usize,
    pub max_batch_evidence: usize,
    pub cooldown_ms: u64,
}
impl Default for ObservationOptions {
    fn default() -> Self {
        Self {
            min_new_evidence: 1,
            max_batch_evidence: MAX_BATCH_EVIDENCE,
            cooldown_ms: 300_000,
        }
    }
}
impl ObservationOptions {
    pub fn validate(&self) -> InterestResult<()> {
        if self.min_new_evidence == 0
            || self.min_new_evidence > self.max_batch_evidence
            || self.max_batch_evidence > MAX_BATCH_EVIDENCE
            || self.cooldown_ms > 86_400_000
        {
            return Err(InterestError::InvalidInput);
        }
        Ok(())
    }
}

/// 观察器可用于关联或撤回的已有活动兴趣。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnownInterest {
    pub id: String,
    pub topic: String,
}

/// 已持久保存为 Running 的观察输入；只含当前作用域已送达的完成交互。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationBatch {
    pub id: String,
    pub scope: MemoryScope,
    pub observer_version: String,
    pub started_at_ms: u64,
    pub evidence: Vec<InteractionEvidence>,
    pub known_interests: Vec<KnownInterest>,
}
impl ObservationBatch {
    /// 校验形状与容量；不读取记忆或账本。
    pub fn validate(&self) -> InterestResult<()> {
        let invalid = || InterestError::InvalidInput;
        self.scope.validate().map_err(|_| invalid())?;
        validate_id(&self.id).map_err(|_| invalid())?;
        validate_id(&self.observer_version).map_err(|_| invalid())?;
        if self.started_at_ms == 0
            || self.evidence.is_empty()
            || self.evidence.len() > MAX_BATCH_EVIDENCE
            || self.known_interests.len() > MAX_KNOWN_INTERESTS
        {
            return Err(invalid());
        }
        let mut ids = BTreeSet::new();
        let mut messages = BTreeSet::new();
        let mut previous = 0;
        for evidence in &self.evidence {
            validate_id(&evidence.id).map_err(|_| invalid())?;
            let EvidenceSource::CompletedInteraction {
                message_id,
                session_revision,
                turn_id,
                user_text,
                assistant_text,
            } = &evidence.source
            else {
                return Err(invalid());
            };
            validate_id(message_id).map_err(|_| invalid())?;
            validate_text(user_text, MAX_EVIDENCE_BYTES).map_err(|_| invalid())?;
            validate_text(assistant_text, MAX_EVIDENCE_BYTES).map_err(|_| invalid())?;
            if evidence.revision <= previous
                || *turn_id == 0
                || turn_id
                    .checked_mul(2)
                    .is_none_or(|minimum| *session_revision < minimum)
                || !ids.insert(evidence.id.as_str())
                || !messages.insert(message_id.as_str())
                || serde_json::to_vec(evidence).map_err(|_| invalid())?.len() > MAX_EVIDENCE_BYTES
            {
                return Err(invalid());
            }
            previous = evidence.revision;
        }
        let mut known = BTreeSet::new();
        for interest in &self.known_interests {
            validate_id(&interest.id).map_err(|_| invalid())?;
            validate_topic(&interest.topic)?;
            if !known.insert(interest.id.as_str()) {
                return Err(invalid());
            }
        }
        if serde_json::to_vec(self).map_err(|_| invalid())?.len() > MAX_INPUT_BYTES {
            return Err(invalid());
        }
        Ok(())
    }

    fn user_text(&self, evidence_id: &str) -> Option<&str> {
        self.evidence
            .iter()
            .find(|evidence| evidence.id == evidence_id)
            .and_then(|evidence| match &evidence.source {
                EvidenceSource::CompletedInteraction { user_text, .. } => Some(user_text.as_str()),
                EvidenceSource::UserStatement { .. } => None,
            })
    }
}

/// 用户在原文中明确表达的内容类别；Withdrawal 只能针对已有兴趣。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatementKind {
    Interest,
    Experience,
    Difficulty,
    Withdrawal,
}

/// 观察器提出的一条陈述；quote 必须能在所引交互的用户原文中逐字找到。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatementDraft {
    pub kind: StatementKind,
    pub quote: String,
    pub evidence_id: String,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum InterestTarget {
    New { topic: String },
    Existing { id: String },
}

/// 一次兴趣新增、补充或撤回。inferred_need 是模型推断，不是用户已下达的任务。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterestUpdateDraft {
    pub target: InterestTarget,
    pub statements: Vec<StatementDraft>,
    #[serde(default)]
    pub inferred_need: Option<String>,
}

/// 校验观察器输出；只做纯检查，不修改账本。
///
/// 每条引用必须是所引交互用户原文的连续片段（忽略空白差异），不能取自助手回复；
/// 新兴趣至少含一条兴趣陈述；撤回只能针对批次中列出的已有活动兴趣，且不夹带推断。
pub fn validate_updates(
    batch: &ObservationBatch,
    updates: &[InterestUpdateDraft],
) -> InterestResult<()> {
    let invalid = || InterestError::InvalidInput;
    if updates.len() > MAX_UPDATES {
        return Err(invalid());
    }
    let known: BTreeSet<_> = batch
        .known_interests
        .iter()
        .map(|interest| interest.id.as_str())
        .collect();
    let mut existing = BTreeSet::new();
    let mut topics = BTreeSet::new();
    for update in updates {
        if update.statements.is_empty() || update.statements.len() > MAX_UPDATE_STATEMENTS {
            return Err(invalid());
        }
        let mut quotes = BTreeSet::new();
        for statement in &update.statements {
            validate_text(&statement.quote, MAX_QUOTE_BYTES).map_err(|_| invalid())?;
            let text = batch
                .user_text(&statement.evidence_id)
                .ok_or_else(invalid)?;
            if !quote_matches(text, &statement.quote)
                || !quotes.insert((statement.evidence_id.as_str(), compact(&statement.quote)))
            {
                return Err(invalid());
            }
        }
        let withdrawal = update
            .statements
            .iter()
            .any(|statement| statement.kind == StatementKind::Withdrawal);
        if withdrawal
            && (update
                .statements
                .iter()
                .any(|statement| statement.kind != StatementKind::Withdrawal)
                || update.inferred_need.is_some())
        {
            return Err(invalid());
        }
        if let Some(need) = &update.inferred_need {
            validate_text(need, MAX_INFERENCE_BYTES).map_err(|_| invalid())?;
        }
        match &update.target {
            InterestTarget::New { topic } => {
                validate_topic(topic)?;
                if withdrawal
                    || !update
                        .statements
                        .iter()
                        .any(|statement| statement.kind == StatementKind::Interest)
                    || !topics.insert(topic_key(topic))
                {
                    return Err(invalid());
                }
            }
            InterestTarget::Existing { id } => {
                if !known.contains(id.as_str()) || !existing.insert(id.as_str()) {
                    return Err(invalid());
                }
            }
        }
    }
    Ok(())
}

pub fn validate_topic(topic: &str) -> InterestResult<()> {
    validate_text(topic, MAX_TOPIC_BYTES).map_err(|_| InterestError::InvalidInput)?;
    if topic.trim() != topic || topic.chars().any(char::is_control) {
        return Err(InterestError::InvalidInput);
    }
    Ok(())
}

/// 主题比较键：忽略空白与大小写，用于把同名新主题并入已有活动兴趣。
pub fn topic_key(topic: &str) -> String {
    compact(topic).to_lowercase()
}

/// 忽略空白差异的逐字片段匹配；不做同义改写或模糊匹配。
pub fn quote_matches(text: &str, quote: &str) -> bool {
    let quote = compact(quote);
    !quote.is_empty() && compact(text).contains(&quote)
}

fn compact(text: &str) -> String {
    text.chars()
        .filter(|value| !value.is_whitespace())
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum InterestStatus {
    Active,
    Withdrawn,
}

/// 陈述来自哪类可信来源；观察来源引用批次，命令来源是用户在当前会话的原始命令。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum StatementOrigin {
    Observation { batch_id: String },
    UserCommand,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterestStatement {
    pub kind: StatementKind,
    pub quote: String,
    pub evidence_id: String,
    pub message_id: String,
    /// 原始交互或命令的时间，不是观察器处理时间。
    pub observed_at_ms: u64,
    pub origin: StatementOrigin,
}

/// 模型对用户可能需要什么帮助的推断；未经用户确认，不能当作任务或事实。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferredNeed {
    pub text: String,
    pub batch_id: String,
    pub interest_revision: u64,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterestRecord {
    pub id: String,
    pub scope: MemoryScope,
    pub topic: String,
    pub status: InterestStatus,
    /// 创建为 1；每次补充陈述、推断或撤回加一。
    pub revision: u64,
    pub statements: Vec<InterestStatement>,
    pub inferences: Vec<InferredNeed>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}
impl InterestRecord {
    pub fn validate(&self) -> InterestResult<()> {
        let invalid = || InterestError::InvalidInput;
        validate_id(&self.id).map_err(|_| invalid())?;
        self.scope.validate().map_err(|_| invalid())?;
        validate_topic(&self.topic)?;
        if self.revision == 0
            || self.statements.is_empty()
            || self.statements.len() > MAX_STATEMENTS
            || self.inferences.len() > MAX_INFERENCES
            || self.created_at_ms == 0
            || self.updated_at_ms < self.created_at_ms
            || !self
                .statements
                .iter()
                .any(|statement| statement.kind == StatementKind::Interest)
        {
            return Err(invalid());
        }
        let mut sources = BTreeSet::new();
        for statement in &self.statements {
            validate_text(&statement.quote, MAX_QUOTE_BYTES).map_err(|_| invalid())?;
            validate_id(&statement.evidence_id).map_err(|_| invalid())?;
            validate_id(&statement.message_id).map_err(|_| invalid())?;
            if let StatementOrigin::Observation { batch_id } = &statement.origin {
                validate_id(batch_id).map_err(|_| invalid())?;
            }
            if statement.observed_at_ms == 0
                || !sources.insert((statement.evidence_id.as_str(), compact(&statement.quote)))
            {
                return Err(invalid());
            }
        }
        let withdrawn = self
            .statements
            .iter()
            .any(|statement| statement.kind == StatementKind::Withdrawal);
        if withdrawn != (self.status == InterestStatus::Withdrawn) {
            return Err(invalid());
        }
        for inference in &self.inferences {
            validate_text(&inference.text, MAX_INFERENCE_BYTES).map_err(|_| invalid())?;
            validate_id(&inference.batch_id).map_err(|_| invalid())?;
            if inference.interest_revision == 0 || inference.interest_revision > self.revision {
                return Err(invalid());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ObservationFailure {
    Provider,
    InvalidOutput,
    Timeout,
    Cancelled,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum JobStatus {
    Running,
    Completed,
    Failed(ObservationFailure),
    Interrupted,
}
/// 合规输出仍可能因当前账本状态不能落地；原因随批次保存，不静默丢弃。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RejectReason {
    /// 关联的兴趣已在保存结果前被撤回。
    NotActive,
    /// 该兴趣陈述数量已达上限。
    StatementLimit,
    /// 兴趣记录总数已达上限。
    InterestLimit,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum UpdateResult {
    Created { interest_id: String },
    Updated { interest_id: String, revision: u64 },
    Withdrawn { interest_id: String, revision: u64 },
    Rejected { reason: RejectReason },
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationJob {
    pub batch: ObservationBatch,
    pub status: JobStatus,
    pub finished_at_ms: Option<u64>,
    pub updates: Vec<InterestUpdateDraft>,
    /// 与 updates 一一对应；只有 Completed 批次有结果。
    pub results: Vec<UpdateResult>,
}
pub enum ObservationOutcome {
    Completed(Vec<InterestUpdateDraft>),
    Failed(ObservationFailure),
}
#[derive(Clone, Eq, PartialEq)]
pub struct InterestSnapshot {
    pub scope: MemoryScope,
    pub interests: Vec<InterestRecord>,
    pub jobs: Vec<ObservationJob>,
}

/// 用户在当前会话发出的明确撤回命令；宿主负责绑定作用域与消息来源。
#[derive(Clone, Eq, PartialEq)]
pub struct WithdrawalRequest {
    pub evidence_id: String,
    pub message_id: String,
    pub text: String,
    pub at_ms: u64,
}

/// 仅可信宿主持有；不发布给模型或不受信插件。
pub trait InterestAdmin: Send + Sync {
    /// 已有兴趣或观察批次的作用域，按作用域排序。
    fn scopes(&self) -> InterestResult<Vec<MemoryScope>>;
    fn snapshot(&self, scope: &MemoryScope) -> InterestResult<InterestSnapshot>;
    /// 原子保存 Running 与确切输入，成功返回后才可发起一次观察请求。
    /// 只消费新增 CompletedInteraction；失败、空结果、中断都不再观察同一证据。
    fn reserve(
        &self,
        memory: &MemorySnapshot,
        now_ms: u64,
        observer_version: &str,
        options: &ObservationOptions,
    ) -> InterestResult<Option<ObservationBatch>>;
    /// 保存观察结局并在同一次提交中应用合规更新；同一结局重放零写入。
    fn finish(
        &self,
        batch: &ObservationBatch,
        at_ms: u64,
        outcome: ObservationOutcome,
    ) -> InterestResult<Vec<UpdateResult>>;
    /// 用户明确撤回；同一命令证据重放返回当前记录且零写入。
    fn withdraw(
        &self,
        scope: &MemoryScope,
        interest_id: &str,
        request: WithdrawalRequest,
    ) -> InterestResult<InterestRecord>;
}

/// 可替换观察器；至多一次模型请求、零工具，不读取批次之外的记忆。
pub trait InterestObserver: Send + Sync {
    fn version(&self) -> &str;
    fn observe(&self, batch: ObservationBatch) -> InterestFuture<'_, Vec<InterestUpdateDraft>>;
}

/// 本次同步对派生目标做出的修改；未能落地的兴趣下次重新核对，不调用模型。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DerivationReport {
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub cancelled: Vec<String>,
    /// 修订冲突或认知容量不足而暂缓的兴趣 ID。
    pub deferred: Vec<String>,
}

/// 可替换目标派生器：由兴趣记录确定性地同步学习目标，零模型请求，可重复调用。
pub trait InterestGoalDeriver: Send + Sync {
    fn version(&self) -> &str;
    fn reconcile(
        &self,
        interests: &[InterestRecord],
        now_ms: u64,
    ) -> InterestResult<DerivationReport>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InterestError {
    InvalidInput,
    NotFound,
    Unavailable,
    CorruptState,
    UnsupportedVersion,
    Storage,
    LimitReached,
    Conflict,
    Memory(MemoryError),
    Observation(ObservationFailure),
    /// 目标派生器发现不属于兴趣来源的同名目标，或认知状态不可用。
    Derivation,
}
impl From<MemoryError> for InterestError {
    fn from(error: MemoryError) -> Self {
        Self::Memory(error)
    }
}
impl fmt::Display for InterestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "兴趣观察输入无效",
            Self::NotFound => "当前会话没有这条兴趣",
            Self::Unavailable => "兴趣观察服务不可用",
            Self::CorruptState => "兴趣观察状态损坏；未清空",
            Self::UnsupportedVersion => "不支持该兴趣观察状态版本",
            Self::Storage => "兴趣观察提交无法确认；须重新打开核对",
            Self::LimitReached => "兴趣观察容量已满；保留原记录",
            Self::Conflict => "兴趣观察批次冲突",
            Self::Memory(_) => "兴趣观察来源不可用",
            Self::Observation(_) => "兴趣观察请求失败",
            Self::Derivation => "学习目标派生失败；未覆盖已有目标",
        })
    }
}
impl std::error::Error for InterestError {}

macro_rules! redacted { ($($ty:ty),+ $(,)?) => { $(impl fmt::Debug for $ty { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(concat!(stringify!($ty), "(<redacted>)")) } })+ }; }
redacted!(
    KnownInterest,
    ObservationBatch,
    StatementDraft,
    InterestTarget,
    InterestUpdateDraft,
    StatementOrigin,
    InterestStatement,
    InferredNeed,
    InterestRecord,
    ObservationJob,
    ObservationOutcome,
    InterestSnapshot,
    WithdrawalRequest,
);
