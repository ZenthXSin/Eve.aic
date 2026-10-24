//! 群聊回复前的原话挑选：Eve 在群里被叫到时，宿主把当前消息、最近几条原话和
//! Eve 参与过的问答作为上下文，请挑选器从更早的全量群消息里选出对这次回答有用的原话。
//! 挑选只决定附上哪些原话，原话本身不改写；选错只多给或少给参考，不改变权限或任务。
use crate::{GroupExchange, GroupMessage};
use eve_llm_api::LlmFuture;

/// 单次交给挑选器的候选上限。
pub const MAX_RECALL_CANDIDATES: usize = 40;
/// 单次至多选中的原话条数。
pub const MAX_RECALL_SELECTED: usize = 8;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupRecallInput {
    pub persona: String,
    /// 当前要回答的消息。
    pub current: GroupMessage,
    /// 已经直接附上的最近原话，按时间顺序；只作上下文，不参与挑选。
    pub recent: Vec<GroupMessage>,
    /// Eve 在本群已确认送达的问答，按时间顺序。
    pub exchanges: Vec<GroupExchange>,
    /// 更早的候选原话，按时间顺序，至多 [`MAX_RECALL_CANDIDATES`] 条，ID 互不相同。
    pub candidates: Vec<GroupMessage>,
}

impl GroupRecallInput {
    pub fn valid(&self) -> bool {
        let conversation = &self.current.conversation;
        let mut ids = std::collections::BTreeSet::new();
        !self.candidates.is_empty()
            && self.candidates.len() <= MAX_RECALL_CANDIDATES
            && self
                .candidates
                .iter()
                .chain(&self.recent)
                .all(|m| &m.conversation == conversation)
            && self.exchanges.iter().all(|e| e.valid(conversation))
            && self.candidates.iter().all(|m| ids.insert(m.id.as_str()))
    }
}

/// 返回选中的候选 ID，按有用程度从高到低，至多 [`MAX_RECALL_SELECTED`] 个；
/// 只能是输入候选中的 ID。拿不准时返回空，不猜。
pub trait GroupRecallSelector: Send + Sync {
    fn select(&self, input: GroupRecallInput) -> LlmFuture<'_, Vec<String>>;
}
