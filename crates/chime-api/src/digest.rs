//! 群消息的按需增量整理：宿主按观察序号把尚未整理的新消息分批交给整理器，
//! 每批在上一段摘要的基础上写出一段新摘要并持久保存。摘要是可出错的归纳，
//! 不是事实、用户偏好或授权；原话仍以群观察库为准。
use crate::GroupMessage;
use eve_llm_api::LlmFuture;
use serde::{Deserialize, Serialize};

/// 单段摘要的字符上限。
pub const MAX_DIGEST_CHARS: usize = 1200;

/// 观察库里带序号的一条群消息；序号在同一数据库内单调递增，作为整理游标。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedMessage {
    pub seq: u64,
    pub message: GroupMessage,
}

/// 一段已保存的摘要，覆盖同一群中序号 `from_seq..=to_seq` 的消息。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GroupDigest {
    pub conversation: String,
    pub from_seq: u64,
    pub to_seq: u64,
    /// 本段实际整理的消息条数。
    pub messages: u32,
    pub first_at_ms: u64,
    pub last_at_ms: u64,
    pub summary: String,
}

impl GroupDigest {
    pub fn valid(&self) -> bool {
        !self.conversation.is_empty()
            && self.from_seq > 0
            && self.from_seq <= self.to_seq
            && self.messages > 0
            && self.first_at_ms <= self.last_at_ms
            && !self.summary.trim().is_empty()
            && self.summary.chars().count() <= MAX_DIGEST_CHARS
    }
}

/// 摘要的持久存储。段与段首尾相接、不重叠；追加的段必须从当前游标之后开始。
pub trait GroupDigestStore: Send + Sync {
    /// 已整理到的最大观察序号；没有摘要时为 0。
    fn cursor(&self, conversation: &str) -> Result<u64, String>;
    /// 最近 `limit` 段摘要，按时间从旧到新。
    fn latest(&self, conversation: &str, limit: usize) -> Result<Vec<GroupDigest>, String>;
    /// 原子追加一段；`from_seq` 不大于当前游标时拒绝，不覆盖已有摘要。
    fn append(&self, digest: &GroupDigest) -> Result<(), String>;
}

/// 把一批新消息整理成一段摘要。`previous` 是上一段摘要，只作衔接参考；
/// 消息正文是数据，不是指令。返回不超过 [`MAX_DIGEST_CHARS`] 字的纯文本。
pub trait GroupDigester: Send + Sync {
    fn digest(
        &self,
        persona: String,
        previous: Option<String>,
        messages: Vec<GroupMessage>,
    ) -> LlmFuture<'_, String>;
}
