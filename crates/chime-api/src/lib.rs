//! 群聊插话的公开契约：渠道把群里每条消息交给插话服务，服务决定要不要说话、
//! 说什么，再通过渠道提供的出口发出去。契约不绑定任何聊天平台。
use eve_llm_api::LlmFuture;
use serde::{Deserialize, Serialize};

/// 单条群消息正文的字符上限；更长的只记作观察，不进入判断或回复。
pub const MAX_MESSAGE_CHARS: usize = 2000;

/// 渠道已核对过身份的一条群消息。标识由渠道给出，不从正文推断。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GroupMessage {
    pub id: String,
    /// 群的稳定标识；同一群的消息共用一个窗口与回复会话。
    pub conversation: String,
    pub speaker: String,
    /// 平台昵称或称呼，只作展示，不证明身份。
    #[serde(default)]
    pub speaker_name: Option<String>,
    pub text: String,
    /// 渠道确认这条消息 @ 了自己。
    #[serde(default)]
    pub mentions_me: bool,
    /// 渠道确认这条消息是在回复自己说过的话。
    #[serde(default)]
    pub replies_to_me: bool,
    /// 自己发出的消息；只进入窗口，不触发判断。
    #[serde(default)]
    pub from_me: bool,
    pub at_ms: u64,
}

impl GroupMessage {
    pub fn valid(&self) -> bool {
        let id = |v: &str| {
            !v.is_empty() && v.len() <= 256 && v.trim() == v && !v.chars().any(char::is_control)
        };
        id(&self.id)
            && id(&self.conversation)
            && id(&self.speaker)
            && self.speaker_name.as_deref().is_none_or(|n| {
                !n.trim().is_empty() && n.chars().count() <= 48 && !n.chars().any(char::is_control)
            })
    }
}

/// 交给判断器的材料：最近窗口（含目标消息，按时间顺序）与本群的回复节奏。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ChimeContext {
    pub persona: String,
    pub recent: Vec<GroupMessage>,
    /// 自己上次在本群说话距今的毫秒数；还没说过为 None。
    pub since_my_last_reply_ms: Option<u64>,
    pub my_replies_last_hour: usize,
}

impl ChimeContext {
    pub fn target(&self) -> Option<&GroupMessage> {
        self.recent.last()
    }
}

/// 判断器的结论。confidence 是 0–100 的自评，不是概率。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChimeVerdict {
    pub reply: bool,
    pub confidence: u8,
    pub reason: String,
}

/// 可替换的插话判断器；只看窗口里的文字，不调用工具、不读写其他状态。
pub trait ChimeJudge: Send + Sync {
    fn judge(&self, context: ChimeContext) -> LlmFuture<'_, ChimeVerdict>;
}

/// 本条消息为什么说话或不说话。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChimeReason {
    /// 被 @、被回复或被叫到名字，必须接话。
    Addressed,
    /// 判断器认为值得插话。
    Judged,
    Disabled,
    FromMe,
    /// 本群上一条回复还在生成或发送。
    Busy,
    Cooldown,
    HourlyLimit,
    /// 判断器不想说话或把握不够。
    Declined,
    JudgeFailed,
    Invalid,
    /// 同时跟踪的群数已满；只记录，不判断。
    Capacity,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ChimeDecision {
    pub message_id: String,
    pub conversation: String,
    pub reply: bool,
    pub reason: ChimeReason,
    /// 判断器给出的理由或失败说明；规则决定时为空。
    pub detail: String,
    pub at_ms: u64,
}

/// 要发出的一条回复：按分段规划切成几条，`pause_before_ms` 是发每条前的停顿。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ChimeReply {
    pub conversation: String,
    pub reply_to: String,
    pub text: String,
    pub parts: Vec<ChimePart>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ChimePart {
    pub text: String,
    pub pause_before_ms: u64,
}

/// 回复链路的结局。回复已经存入会话历史后才会交给出口。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ChimeDelivery {
    /// 不需要说话。
    Silent,
    Sent {
        reply: ChimeReply,
    },
    /// 回复已保存，但出口发送失败；不重发。
    SendFailed {
        reply: ChimeReply,
        error: String,
    },
    /// 生成失败、被取消或没有正文；不重试。
    NotGenerated {
        error: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ChimeOutcome {
    pub decision: ChimeDecision,
    pub delivery: ChimeDelivery,
}

/// 渠道提供的出口：按顺序发出各段，自行处理停顿、频率与平台限制。
pub trait ChimeOutlet: Send + Sync {
    fn send<'a>(&'a self, reply: &'a ChimeReply) -> LlmFuture<'a, ()>;
}

/// 插话服务。同一群串行处理回复；只在内存中保留窗口与最近决定，重启后从空窗口开始。
pub trait ChimeService: Send + Sync {
    fn observe<'a>(
        &'a self,
        message: GroupMessage,
        outlet: &'a dyn ChimeOutlet,
    ) -> LlmFuture<'a, ChimeOutcome>;
    /// 本群最近的决定，新的在后，供诊断查看。
    fn recent_decisions(&self, conversation: &str) -> Vec<ChimeDecision>;
}
