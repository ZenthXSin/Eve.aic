//! 群话题关系与异步经验契约；不产生任务控制动作或权限。
use crate::GroupMessage;
use eve_llm_api::LlmFuture;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GroupExchange {
    pub source: GroupMessage,
    pub reply: GroupMessage,
}
impl GroupExchange {
    pub fn valid(&self, conversation: &str) -> bool {
        self.source.valid()
            && self.reply.valid()
            && !self.source.from_me
            && self.reply.from_me
            && self.source.conversation == conversation
            && self.reply.conversation == conversation
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupRelationKind {
    Related,
    MindustryQuestion,
    EveQuestion,
    Unrelated,
    Uncertain,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRelationDecision {
    pub kind: GroupRelationKind,
    pub needs_reply: bool,
    pub confidence: u8,
    /// 指向本群已参与问答的 source.id，不允许模型指定发送对象。
    pub topic: Option<String>,
    pub reason: String,
}
impl GroupRelationDecision {
    pub fn validate(&self, input: &GroupRelationInput) -> bool {
        self.confidence <= 100
            && self.reason.chars().count() <= 240
            && self
                .topic
                .as_ref()
                .is_none_or(|id| input.exchanges.iter().any(|e| &e.source.id == id))
            && (self.kind != GroupRelationKind::Related || self.topic.is_some())
            && (!matches!(
                self.kind,
                GroupRelationKind::Unrelated | GroupRelationKind::Uncertain
            ) || !self.needs_reply)
    }
    pub fn certain(&self) -> bool {
        self.kind != GroupRelationKind::Uncertain && self.confidence >= 80
    }
    pub fn uncertain(reason: &str) -> Self {
        Self {
            kind: GroupRelationKind::Uncertain,
            needs_reply: false,
            confidence: 0,
            topic: None,
            reason: reason.into(),
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GroupRelationExample {
    pub message: String,
    /// 完整的所关联问答；独立问题或无关消息为空。
    pub exchange: Option<GroupExchange>,
    pub decision: GroupRelationDecision,
    pub evidence: Vec<String>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GroupRelationInput {
    pub message: GroupMessage,
    pub exchanges: Vec<GroupExchange>,
    pub examples: Vec<GroupRelationExample>,
}
impl GroupRelationInput {
    pub fn valid(&self) -> bool {
        self.message.valid()
            && !self.message.from_me
            && self.examples.len() <= 4
            && self
                .exchanges
                .iter()
                .all(|e| e.valid(&self.message.conversation))
            && self.examples.iter().all(|e| {
                e.exchange
                    .as_ref()
                    .is_none_or(|x| x.valid(&self.message.conversation))
            })
    }
}
pub trait GroupRelationJudge: Send + Sync {
    fn classify(&self, input: GroupRelationInput) -> LlmFuture<'_, GroupRelationDecision>;
}
/// 整理只产生经验，不直接补发历史消息。
#[derive(Clone, Debug, Serialize)]
pub struct GroupRelationReview {
    pub input: GroupRelationInput,
    pub subsequent: Vec<GroupMessage>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRelationReviewResult {
    pub decision: GroupRelationDecision,
    pub evidence: Vec<String>,
}
pub trait GroupRelationReviewer: Send + Sync {
    fn review(&self, input: GroupRelationReview) -> LlmFuture<'_, GroupRelationReviewResult>;
}
pub struct PendingGroupRelation {
    pub message: GroupMessage,
    pub active_considered: bool,
}
/// 与在途任务经验分范围持久保存。pending 不因重启、判断失败或主动回复失败而删除。
pub trait GroupRelationStore: Send + Sync {
    fn stage(&self, message: &GroupMessage) -> Result<(), String>;
    fn suppress_active(&self, message: &GroupMessage) -> Result<(), String>;
    fn record(
        &self,
        message: &GroupMessage,
        decision: &GroupRelationDecision,
    ) -> Result<(), String>;
    fn recall(&self, message: &GroupMessage) -> Result<Vec<GroupRelationExample>, String>;
    /// 领取新消息形成的一批主动判断；历史 pending 重启后仅整理，不补发。
    fn claim_active(&self) -> Result<Option<GroupMessage>, String>;
    fn pending_groups(&self) -> Result<Vec<String>, String>;
    fn pending(
        &self,
        conversation: &str,
        revision: &str,
        limit: usize,
    ) -> Result<Vec<PendingGroupRelation>, String>;
    fn reviewed(
        &self,
        message: &GroupMessage,
        revision: &str,
        example: Option<&GroupRelationExample>,
    ) -> Result<(), String>;
}
