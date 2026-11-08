//! 从真实交流中学习接话方式；这些经验只是可修正的数据，不是事实或授权。
use eve_llm_api::{ContextScope, LlmFuture};
use eve_plugin_api::{PluginError, PluginResult};
use serde::{Deserialize, Serialize};

pub const CONVERSATION_BATCH_MESSAGES: usize = 4;
pub const MAX_CONVERSATION_MESSAGES: usize = 16;
/// 显式历史重访按相同字节预算合并更多短消息，普通实时批次不变。
pub const MAX_CONVERSATION_REVISIT_MESSAGES: usize = 128;
pub const MAX_CONVERSATION_BODY_BYTES: usize = 32_768;
pub const MAX_CONVERSATION_CONTEXT_BYTES: usize = 8_192;
pub const MAX_CONVERSATION_PROMPT_BYTES: usize = 65_536;
pub const MAX_CONVERSATION_LESSONS: usize = 24;
pub const MAX_CONVERSATION_DRAFTS: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationRole {
    Human,
    Eve,
}

/// 由通道提供已验证的身份与正文；speaker 是当前范围内的匿名标识。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationMessage {
    pub id: String,
    pub speaker: String,
    pub role: ConversationRole,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ConversationMetadata>,
}

/// 通道已经观察到的事实；未知时间不由恢复时间、正文或模型推测补齐。
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationMetadata {
    /// 历史来源只提供偏序，不能据数组相邻顺序推断真人反馈。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ordering_uncertain: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<ConversationActivity>,
    /// 仅持久存储分配；接收方留空。相同范围的事件重投不改变序号。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    /// 引用、附件描述等通道材料，不冒充说话者的正文或未读取的附件内容。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// 仅批次投影填写。原事件始终完整保存；区间是 UTF-8 字节偏移。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fragment: Option<ConversationFragment>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationActivity {
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationFragment {
    pub text_start: usize,
    pub text_end: usize,
    pub text_bytes: usize,
    pub context_start: usize,
    pub context_end: usize,
    pub context_bytes: usize,
}

/// 同一份验证供宿主和可替换持久层使用；接收正文不按词法或代码/引用类别过滤。
pub fn conversation_message_valid(message: &ConversationMessage) -> bool {
    let identifier = |text: &str, limit: usize| {
        !text.is_empty()
            && text.len() <= limit
            && text.trim() == text
            && !text.chars().any(char::is_control)
    };
    let body = |text: &str, limit: usize| {
        text.len() <= limit
            && !text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    };
    identifier(&message.id, 256)
        && identifier(&message.speaker, 256)
        && body(&message.text, MAX_CONVERSATION_BODY_BYTES)
        && (!message.text.trim().is_empty()
            || message
                .metadata
                .as_ref()
                .and_then(|m| m.context.as_ref())
                .is_some_and(|c| !c.trim().is_empty())
            || message.metadata.as_ref().is_some_and(|m| {
                m.fragment.is_some()
                    && (!message.text.is_empty()
                        || m.context.as_ref().is_some_and(|c| !c.is_empty()))
            }))
        && message.metadata.as_ref().is_none_or(|metadata| {
            metadata
                .reply_to
                .as_ref()
                .is_none_or(|v| identifier(v, 256))
                && metadata
                    .source_kind
                    .as_ref()
                    .is_none_or(|v| identifier(v, 80))
                && metadata
                    .context
                    .as_ref()
                    .is_none_or(|v| body(v, MAX_CONVERSATION_CONTEXT_BYTES))
                && metadata.activity.as_ref().is_none_or(|a| {
                    identifier(&a.phase, 80)
                        && a.tool_name.as_ref().is_none_or(|v| identifier(v, 256))
                })
                && metadata.fragment.as_ref().is_none_or(|f| {
                    f.text_start <= f.text_end
                        && f.text_end <= f.text_bytes
                        && f.context_start <= f.context_end
                        && f.context_end <= f.context_bytes
                        && f.text_end - f.text_start == message.text.len()
                        && f.context_end - f.context_start
                            == metadata.context.as_ref().map_or(0, String::len)
                })
        })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationEvidence {
    pub message_id: String,
    pub quote: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationAssessment {
    /// 不把出现过、无人反对或成功送达视为认可。
    Observation,
    /// 对 Eve 具体表达的直接纠正；必须有 Eve 发言之后的真人证据。
    Correction,
    /// 真实反例；停用旧经验，保留证据而不是删除历史。
    Counterexample,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationLessonDraft {
    pub lesson_id: Option<String>,
    pub situation: String,
    pub response: String,
    pub avoid: String,
    pub assessment: ConversationAssessment,
    pub evidence: Vec<ConversationEvidence>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationLesson {
    pub id: String,
    pub situation: String,
    pub response: String,
    pub avoid: String,
    pub observations: u32,
    pub corrections: u32,
    pub counterexamples: u32,
    pub active: bool,
    pub updated_at_ms: u64,
    pub evidence: Vec<ConversationEvidence>,
    /// 已用于支持/反例计数的真人消息编号，防滚动窗口重复强化同一证据。
    pub evaluated_message_ids: Vec<String>,
}

/// 相邻真人与 Eve 发言保持顺序；不同人不拼成同一个人的意见。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationBatch {
    pub id: String,
    pub scope: ContextScope,
    pub messages: Vec<ConversationMessage>,
    pub lessons: Vec<ConversationLesson>,
    /// 已归档的经验版本，按持久游标轮转；包含原证据，不充当新的真人反馈。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lesson_history: Vec<ConversationLessonRevision>,
    pub attempt: u8,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationLessonRevision {
    /// 存储在同范围内分配。待提交的新版本留 0。
    pub sequence: u64,
    pub lesson: ConversationLesson,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at_ms: Option<u64>,
}

/// 完整事件及不可变的范围内顺序；正文不按学习器的窗口大小裁剪。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationEvent {
    pub sequence: u64,
    pub message: ConversationMessage,
}

/// 运维受理的真实历史来源；原身份和原消息编号不改变。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationHistoryRecord {
    pub scope: ContextScope,
    pub event: ConversationEvent,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConversationSweepMessageCounts {
    /// 已消费完整原消息中，至少一片最终失败且没有未知片段的数量。
    pub failed: u64,
    /// 已消费完整原消息中，至少一片结果未知的数量；优先于失败。
    pub unknown: u64,
    /// 旧检查点没有逐片结果，不能证明成功或失败的已消费消息。
    pub unclassified: u64,
    /// 迁移前记录的 processed，仅供核对，不能与新成功数相加。
    pub legacy_processed: u64,
    /// 以下三个数都是 remaining 的子集，原消息越界后才计入终态。
    pub partial: u64,
    pub partial_failed: u64,
    pub partial_unknown: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConversationSweepStatus {
    pub id: String,
    pub scopes: u64,
    pub total: u64,
    /// message_counts 存在时，仅统计全部分片已确认成功的原消息。
    pub processed: u64,
    pub remaining: u64,
    pub updated: u64,
    pub failed_batches: u64,
    pub interrupted_batches: u64,
    pub model_requests: u64,
    /// 旧状态文件缺失此字段时保持未知，不能把旧 processed 当作全片成功。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_counts: Option<ConversationSweepMessageCounts>,
    /// 表示冻结队列已消费完，不代表每条消息均成功。
    pub completed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationSeen {
    pub id: String,
    pub body: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationAppend {
    pub inserted: bool,
    /// 暂停期间只存去重凭据，不采集正文，因此没有学习事件序号。
    pub sequence: Option<u64>,
}

/// 状态机的检查点采用版本化 JSON，存储实现不得改写其中的游标或批次。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationStoredScope {
    pub scope: ContextScope,
    pub revision: u64,
    pub next_sequence: u64,
    pub checkpoint: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationLegacyScope {
    pub scope: ContextScope,
    pub events: Vec<ConversationEvent>,
    pub seen: Vec<ConversationSeen>,
    pub checkpoint: Vec<u8>,
    pub revisions: Vec<ConversationLessonRevision>,
    /// 旧未知批次在同一导入事务中归档，不能按新模型预算裁剪或重新请求。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub batches: Vec<ConversationBatchArchive>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationBatchArchive {
    pub batch: ConversationBatch,
    /// completed / failed / paused / reset / interrupted；不是外部动作回执。
    /// budget_exhausted 仅结算已明确失败、但维护总额度不再允许重试的原批次，不代表新请求。
    pub outcome: String,
    pub recorded_at_ms: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationCommit {
    pub expected_revision: u64,
    pub checkpoint: Vec<u8>,
    pub revisions: Vec<ConversationLessonRevision>,
    pub batch: Option<ConversationBatchArchive>,
}

/// 可替换的交流日志存储。append 和 commit 各自原子；提交失败或未知不得退回空状态。
/// 去重使用范围、消息 id 与 speaker/role/text 的稳定摘要；元数据采用首次已保存版本。
pub trait ConversationStore: Send + Sync {
    /// 明确授权的历史导入补全已见但未保留正文的事件；原摘要冲突必须拒绝。
    fn import_history(
        &self,
        _scope: &ContextScope,
        _message: &ConversationMessage,
    ) -> PluginResult<ConversationAppend> {
        Err(PluginError::Task("当前存储不支持历史交流导入".into()))
    }
    /// 一次性导入旧 JSON 的可用材料。相同摘要幂等，来源变化必须拒绝；旧文件不改写。
    fn import_legacy(&self, digest: &str, scopes: &[ConversationLegacyScope]) -> PluginResult<()>;
    fn append(
        &self,
        scope: &ContextScope,
        message: &ConversationMessage,
        enabled: bool,
    ) -> PluginResult<ConversationAppend>;
    fn scopes(&self) -> PluginResult<Vec<ConversationStoredScope>>;
    fn scope(&self, scope: &ContextScope) -> PluginResult<Option<ConversationStoredScope>>;
    fn events(
        &self,
        scope: &ContextScope,
        from_sequence: u64,
        limit: usize,
    ) -> PluginResult<Vec<ConversationEvent>>;
    fn revisions(
        &self,
        scope: &ContextScope,
        after_sequence: u64,
        limit: usize,
    ) -> PluginResult<Vec<ConversationLessonRevision>>;
    fn latest_revision(
        &self,
        scope: &ContextScope,
        lesson_id: &str,
    ) -> PluginResult<Option<ConversationLessonRevision>>;
    /// 检查点、经验版本与批次结局同事务提交。并发版本冲突返回 false，不写任何内容。
    fn commit(&self, scope: &ContextScope, change: &ConversationCommit) -> PluginResult<bool>;
}

#[derive(Serialize)]
struct PromptLesson<'a> {
    id: &'a str,
    situation: &'a str,
    response: &'a str,
    avoid: &'a str,
    observations: u32,
    corrections: u32,
    counterexamples: u32,
    active: bool,
    updated_at_ms: u64,
    evidence: &'a [ConversationEvidence],
}
impl<'a> From<&'a ConversationLesson> for PromptLesson<'a> {
    fn from(lesson: &'a ConversationLesson) -> Self {
        Self {
            id: &lesson.id,
            situation: &lesson.situation,
            response: &lesson.response,
            avoid: &lesson.avoid,
            observations: lesson.observations,
            corrections: lesson.corrections,
            counterexamples: lesson.counterexamples,
            active: lesson.active,
            updated_at_ms: lesson.updated_at_ms,
            evidence: &lesson.evidence,
        }
    }
}

#[derive(Serialize)]
struct PromptRevision<'a> {
    sequence: u64,
    lesson: PromptLesson<'a>,
    reason: &'a str,
    batch_id: &'a Option<String>,
    recorded_at_ms: Option<u64>,
}
#[derive(Serialize)]
struct FeedbackPair<'a> {
    eve_message_id: &'a str,
    later_human_message_id: &'a str,
}
#[derive(Serialize)]
struct PromptBatch<'a> {
    scope: &'a ContextScope,
    messages: &'a [ConversationMessage],
    lessons: Vec<PromptLesson<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    lesson_history: Vec<PromptRevision<'a>>,
    feedback_pairs: Vec<FeedbackPair<'a>>,
    id: &'a str,
    attempt: u8,
}

/// 历史重访只有已有顺序事实才允许把后一句当作对前一句的反馈。
pub fn conversation_feedback_order_known(
    eve: &ConversationMessage,
    human: &ConversationMessage,
) -> bool {
    let (Some(eve), Some(human)) = (&eve.metadata, &human.metadata) else {
        return false;
    };
    if eve.ordering_uncertain || human.ordering_uncertain {
        return false;
    }
    match (eve.observed_at_ms, human.observed_at_ms) {
        (Some(first), Some(later)) => first <= later,
        _ => match (eve.source_at_ms, human.source_at_ms) {
            (Some(first), Some(later)) => first <= later,
            _ => false,
        },
    }
}

/// 队列与模型学习器共用的精确序列化；不传内部支持计数去重表，不裁剪原文。
pub fn conversation_prompt_json(batch: &ConversationBatch) -> PluginResult<String> {
    let feedback_pairs = batch
        .messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role == ConversationRole::Human)
        .filter_map(|(index, message)| {
            batch.messages[..index]
                .iter()
                .rev()
                .find(|previous| previous.role == ConversationRole::Eve)
                .filter(|previous| {
                    !previous
                        .metadata
                        .as_ref()
                        .is_some_and(|m| m.ordering_uncertain)
                        && !message
                            .metadata
                            .as_ref()
                            .is_some_and(|m| m.ordering_uncertain)
                        && (!batch.id.starts_with("conversation-revisit-v2-")
                            || conversation_feedback_order_known(previous, message))
                })
                .map(|previous| FeedbackPair {
                    eve_message_id: &previous.id,
                    later_human_message_id: &message.id,
                })
        })
        .collect();
    serde_json::to_string(&PromptBatch {
        scope: &batch.scope,
        messages: &batch.messages,
        lessons: batch.lessons.iter().map(PromptLesson::from).collect(),
        lesson_history: batch
            .lesson_history
            .iter()
            .map(|revision| PromptRevision {
                sequence: revision.sequence,
                lesson: PromptLesson::from(&revision.lesson),
                reason: &revision.reason,
                batch_id: &revision.batch_id,
                recorded_at_ms: revision.recorded_at_ms,
            })
            .collect(),
        feedback_pairs,
        id: &batch.id,
        attempt: batch.attempt,
    })
    .map_err(|_| PluginError::State("交流学习批次编码失败".into()))
}

pub trait ConversationLearner: Send + Sync {
    fn learn(&self, batch: ConversationBatch) -> LlmFuture<'_, Vec<ConversationLessonDraft>>;
}
