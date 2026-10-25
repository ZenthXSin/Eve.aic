//! 主动提问训练的公开契约；启停权属于可信通道，不作为模型工具发布。
use eve_llm_api::{ContextScope, LlmFuture};
use eve_plugin_api::PluginResult;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const TRAINING_PLUGIN_ID: &str = "eve.training";
pub const TRAINING_SERVICE_ID: &str = "eve.training.mode";

/// 不含原文的去重与表达统计记录；active=false 保留去重凭据，重置不重新学习旧输入。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpressionRecord {
    pub scope: ContextScope,
    pub id: String,
    pub body: String,
    pub chars: usize,
    pub paragraphs: usize,
    pub question: bool,
    pub formal: bool,
    pub casual: bool,
    pub active: bool,
}
impl ExpressionRecord {
    pub fn valid(&self) -> bool {
        let identifier = |v: &str| {
            !v.is_empty() && v.len() <= 256 && v.trim() == v && !v.chars().any(char::is_control)
        };
        let hash = |v: &str| {
            v.len() == 64
                && v.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        identifier(&self.scope.session_id)
            && identifier(&self.scope.user_id)
            && hash(&self.id)
            && hash(&self.body)
            && ((1..=512).contains(&self.chars) && (1..=self.chars).contains(&self.paragraphs)
                || !self.active
                    && self.chars == 0
                    && self.paragraphs == 0
                    && !self.question
                    && !self.formal
                    && !self.casual)
    }
}

/// 可替换的表达统计持久存储；导入必须幂等且不得复活已重置记录，写入成功后才能缓冲学习样本。
pub trait ExpressionStore: Send + Sync {
    fn import_legacy(&self, rows: &[ExpressionRecord]) -> PluginResult<()>;
    fn get(&self, scope: &ContextScope, id: &str) -> PluginResult<Option<ExpressionRecord>>;
    /// 新插入返回 true；相同记录返回 false，ID 内容冲突必须拒绝。
    fn insert(&self, row: &ExpressionRecord) -> PluginResult<bool>;
    fn snapshot(&self, scope: Option<&ContextScope>) -> PluginResult<Option<ExpressionSnapshot>>;
    fn reset(&self, scope: &ContextScope) -> PluginResult<()>;
}

/// 用户原文的表达统计，不包含正文、身份、观点或已确认的长期偏好。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpressionSnapshot {
    pub revision: String,
    pub samples: usize,
    pub median_chars: usize,
    pub p75_chars: usize,
    pub short_percent: usize,
    pub single_paragraph_percent: usize,
    pub question_percent: usize,
    pub formal_percent: usize,
    pub casual_percent: usize,
}

/// 每批交给学习器的用户原话条数；攒够才学习。
pub const PHRASE_BATCH: usize = 12;
/// 每个作用域保存的已学说法上限。
pub const MAX_PHRASES: usize = 64;
/// 学习器单批至多返回的说法条数。
pub const MAX_PHRASE_DRAFTS: usize = 6;
/// 词语或场合的字符上限；词义、说法特点与原话摘录的字符上限。
pub const MAX_PHRASE_TEXT_CHARS: usize = 24;
pub const MAX_PHRASE_DETAIL_CHARS: usize = 60;

/// 从用户自己的话里学到的说法：词语（含圈内黑话、缩写、梗），或某种场合下的表达方式。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhraseKind {
    Term,
    Expression,
}

/// 学习器产出的候选；宿主核对摘录确实出自本批原话后才保存。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhraseDraft {
    pub kind: PhraseKind,
    /// 词语本身，或这种表达方式适用的场合。
    pub text: String,
    /// 词义，或这种场合下用户的说法特点。
    pub meaning: String,
    /// 本批某条原话中的原样摘录。
    pub example: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearnedPhrase {
    pub id: String,
    pub kind: PhraseKind,
    pub text: String,
    pub meaning: String,
    pub example: String,
    /// 在不同批次中被学到的次数。
    pub uses: u32,
    pub updated_at_ms: u64,
}

/// 一批待学习的用户原话。取出即从待学缓冲中移除；学习失败或进程中断不重试。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhraseBatch {
    pub id: String,
    pub scope: ContextScope,
    pub messages: Vec<String>,
}

/// 可替换的说法学习器；只看本批原话，不读其他状态，不调用工具。
pub trait PhraseLearner: Send + Sync {
    fn learn(&self, batch: PhraseBatch) -> LlmFuture<'_, Vec<PhraseDraft>>;
}

pub trait TrainingService: Send + Sync {
    fn enabled(&self, scope: &ContextScope) -> PluginResult<bool>;
    /// 必须先持久化再确认；失败保留原状态。
    fn set_enabled(&self, scope: &ContextScope, enabled: bool) -> PluginResult<()>;
    /// 只接收可信宿主验证后的用户输入；重复 ID 不重复学习。
    fn observe_user_message(
        &self,
        _scope: &ContextScope,
        _id: &str,
        _text: &str,
    ) -> PluginResult<()> {
        Ok(())
    }
    fn expression_snapshot(
        &self,
        _scope: &ContextScope,
    ) -> PluginResult<Option<ExpressionSnapshot>> {
        Ok(None)
    }
    /// 只核对本会话仍有效的表达样本及原文摘要；停止、重置或未实现时不得提供新示例。
    fn is_expression_source(
        &self,
        _scope: &ContextScope,
        _message_id: &str,
        _text: &str,
    ) -> PluginResult<bool> {
        Ok(false)
    }
    /// 跨会话仅汇总数值特征，供样本不足时采用一般表达习惯。
    fn expression_baseline(&self) -> PluginResult<Option<ExpressionSnapshot>> {
        Ok(None)
    }
    /// 取出一批攒够的原话交给学习器；未装配说法学习或没有攒够时为 None。
    fn take_phrase_batch(&self, _now_ms: u64) -> PluginResult<Option<PhraseBatch>> {
        Ok(None)
    }
    /// 核对并保存学习结果，返回新增或更新的条数；任一候选不合格时整批拒绝，不保存。
    fn finish_phrase_batch(
        &self,
        _batch: &PhraseBatch,
        _drafts: Vec<PhraseDraft>,
        _now_ms: u64,
    ) -> PluginResult<usize> {
        Ok(0)
    }
    /// 当前作用域已学到的说法，按学到次数降序。
    fn phrases(&self, _scope: &ContextScope) -> PluginResult<Vec<LearnedPhrase>> {
        Ok(vec![])
    }
    /// 删除一条已学说法；不存在时返回 false。
    fn forget_phrase(&self, _scope: &ContextScope, _id: &str) -> PluginResult<bool> {
        Ok(false)
    }
    /// 保留去重凭据与原始历史，重置当前可信作用域的统计。
    fn reset_expression(&self, _scope: &ContextScope) -> PluginResult<()> {
        Err(eve_plugin_api::PluginError::State(
            "该训练实现不支持重置".into(),
        ))
    }
}
#[derive(Clone)]
pub struct TrainingServiceHandle(pub Arc<dyn TrainingService>);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TrainingCommand {
    Start,
    Stop,
    Status,
    Stats,
    Reset,
    Words,
    Forget(String),
    Help,
}
impl TrainingCommand {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "/train start" => Some(Self::Start),
            "/train stop" => Some(Self::Stop),
            "/train status" => Some(Self::Status),
            "/train stats" => Some(Self::Stats),
            "/train reset" => Some(Self::Reset),
            "/train words" => Some(Self::Words),
            text if text.starts_with("/train forget ") => {
                let id = text["/train forget ".len()..].trim();
                Some(if id.is_empty() || id.contains(char::is_whitespace) {
                    Self::Help
                } else {
                    Self::Forget(id.into())
                })
            }
            text if text == "/train" || text.starts_with("/train ") => Some(Self::Help),
            _ => None,
        }
    }
}
