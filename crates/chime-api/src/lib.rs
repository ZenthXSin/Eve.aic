//! 群聊插话的公开契约：渠道把群里每条消息交给插话服务，服务决定要不要说话、
//! 说什么，再通过渠道提供的出口发出去。契约不绑定任何聊天平台。
use eve_llm_api::LlmFuture;
use serde::{Deserialize, Serialize};
mod digest;
mod recall;
mod relation;
pub use digest::*;
pub use recall::*;
pub use relation::*;

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
    /// 仅记录上下文，由渠道已有对话链路回复。
    Observed,
    /// 旧诊断记录的被叫到标记；新模型仍自行决定是否说话。
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
    /// Eve 选择等待，再到时结合最新事实审视。
    Waiting,
    /// 判断期间来源已被后续群消息替代，没有发送。
    Stale,
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

/// Eve 明确决定的一条回复。新路径仅一个完整段，不自动切句或添加停顿。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChimeReply {
    pub conversation: String,
    pub reply_to: String,
    pub text: String,
    pub parts: Vec<ChimePart>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
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
    /// 等待决定已经保存，没有生成发送尝试。
    Waiting {
        after_ms: u64,
    },
    /// 明确的发言决定已经保存，仍未认领发送，不表示送达。
    Queued {
        reply: ChimeReply,
        after_ms: u64,
    },
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

/// 本轮可信消息ID确定的序号窗口。消息按持久插入序号升序，绝不读取cutoff之后的后发消息。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupObservationWindow {
    pub cutoff_seq: u64,
    pub anchor_ids: Vec<String>,
    pub messages: Vec<ObservedMessage>,
}

/// 来自持久已确认问答的说话节奏，与当前原话窗口大小无关。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GroupReplyCadence {
    pub last_reply_ms: Option<u64>,
    pub replies_last_hour: usize,
}

/// 可信渠道观察记录与主动投递账本；保存原话不表示任务完成或管理员授权。
pub trait GroupObservationStore: Send + Sync {
    fn append(&self, message: &GroupMessage) -> Result<bool, String>;
    /// 原话与真实平台 ID 同一事务保存；不能由模型提供锚点。
    fn append_anchored(&self, _message: &GroupMessage, _platform_id: &str) -> Result<bool, String> {
        Err("群观察存储不支持持久平台锚点".into())
    }
    /// 不保存命令正文；只由渠道已核对的真实入站 ID 推进全群最新人类锚点。
    /// 同一平台事件重复调用不推进顺序，不能令旧候选复活。
    fn advance_anchor(&self, _conversation: &str, _platform_id: &str) -> Result<bool, String> {
        Err("群观察存储不支持独立入站锚点".into())
    }
    /// 原始观察、平台锚点、待审视事实同事务保存，堵住入队前进程中断的丢失窗口。
    fn append_speech(
        &self,
        _message: &GroupMessage,
        _platform_id: &str,
        _outbound_id: &str,
        _now_ms: u64,
    ) -> Result<bool, String> {
        Err("群观察存储不支持原子登记发言事实".into())
    }
    /// 只登记待审视事实，不认领发送；旧消息重复登记不覆盖已有决定。
    fn stage_speech(
        &self,
        _message: &GroupMessage,
        _outbound_id: &str,
        _now_ms: u64,
    ) -> Result<(), String> {
        Err("群观察存储不支持自主发言决定".into())
    }
    fn pending_speech(&self, _now_ms: u64, _limit: usize) -> Result<Vec<GroupMessage>, String> {
        Err("群观察存储不支持持久复审".into())
    }
    fn begin_speech(
        &self,
        _message: &GroupMessage,
        _now_ms: u64,
    ) -> Result<Option<GroupSpeechReview>, String> {
        Err("群观察存储不支持自主发言决定".into())
    }
    /// 原子保存模型三态决定；Speak 同时保存待发送正文，Wait/Silence 不创建发送行。
    /// None 表示模型失败，仅延后复审；比较失败不改写新消息或其他轮次。
    fn decide_speech(
        &self,
        _review: &GroupSpeechReview,
        _decision: Option<&ChimeSpeechDecision>,
        _now_ms: u64,
    ) -> Result<bool, String> {
        Err("群观察存储不支持自主发言决定".into())
    }
    fn next_authored(&self, _now_ms: u64) -> Result<Option<GroupAuthoredOutbound>, String> {
        Err("群观察存储不支持自主正文候选".into())
    }
    /// 按 outbound.id 升序只读分页，after 是排他的 ID 下界，最多返回 65 条。
    /// 多读的一条供宿主判断后续页；没有路由的群不能堵住其他群。
    fn authored_after(
        &self,
        _now_ms: u64,
        _after: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<GroupAuthoredOutbound>, String> {
        Err("群观察存储不支持自主正文分页".into())
    }
    fn authored(&self, _id: &str) -> Result<Option<GroupAuthoredOutbound>, String> {
        Err("群观察存储不支持自主正文候选".into())
    }
    /// 全群最新人类消息、正文、身份、决定版本和到期时间同锁比较后再认领。
    fn claim_authored(
        &self,
        _expected: &GroupAuthoredOutbound,
        _now_ms: u64,
    ) -> Result<bool, String> {
        Err("群观察存储不支持自主正文认领".into())
    }
    fn recent(&self, conversation: &str, limit: usize) -> Result<Vec<GroupMessage>, String>;
    fn message(&self, conversation: &str, id: &str) -> Result<Option<GroupMessage>, String>;
    /// 原子读取截至本轮主消息的最近窗口，必须保留全部合并批次ID。
    /// 缺失锚点/跨群/不支持的实现明确报错，不回退到全群最新消息，也不猜正文。
    fn window_ending_at(
        &self,
        _conversation: &str,
        _anchor_ids: &[String],
        _limit: usize,
    ) -> Result<GroupObservationWindow, String> {
        Err("群观察存储不支持可信轮次窗口".into())
    }

    /// 当前群的原话关键词检索，按序号降序；before_seq 是排他的向前翻页游标。
    /// 空关键词表示直接向前翻页；不支持的实现明确报错，不请求模型补造结果。
    fn search(
        &self,
        _conversation: &str,
        _query: &str,
        _before_seq: Option<u64>,
        _limit: usize,
    ) -> Result<Vec<ObservedMessage>, String> {
        Err("群观察存储不支持原话检索".into())
    }
    /// Eve 已确认送达的完整问答，不按最近窗口截断。
    fn exchanges(&self, conversation: &str) -> Result<Vec<GroupExchange>, String>;
    fn reply_cadence(
        &self,
        _conversation: &str,
        _now_ms: u64,
    ) -> Result<GroupReplyCadence, String> {
        Err("群观察存储不支持持久说话节奏".into())
    }
    fn link_reply(&self, conversation: &str, source: &str, reply: &str) -> Result<(), String>;
    fn queue(&self, id: &str, reply: &ChimeReply, speaker: &str) -> Result<(), String>;
    fn claim(&self) -> Result<Option<GroupOutbound>, String>;
    fn finish(&self, id: &str, sent: bool) -> Result<(), String>;
    /// 平台未确认实际发送结局；保留unknown，不能写成失败或成功，也不能补发。
    fn finish_unknown(&self, _id: &str) -> Result<(), String> {
        Err("群观察存储不支持未知结局".into())
    }

    fn abandon(&self, id: &str) -> Result<(), String>;
    fn outbound(&self, id: &str) -> Result<Option<GroupOutbound>, String>;
    /// 序号大于 `after_seq` 的消息，按序号升序，至多 `limit` 条；用于按需增量整理。
    /// 不支持增量读取的实现如实返回错误。
    fn after(
        &self,
        _conversation: &str,
        _after_seq: u64,
        _limit: usize,
    ) -> Result<Vec<ObservedMessage>, String> {
        Err("群观察存储不支持增量读取".into())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GroupOutbound {
    pub id: String,
    pub reply: ChimeReply,
    pub speaker: String,
}

/// 主对话模型的一次明确交流决定；普通文字 final 不构成发送指令。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChimeSpeechDecision {
    Speak { text: String, after_ms: u64 },
    Wait { after_ms: u64 },
    Silence {},
}
impl ChimeSpeechDecision {
    pub fn valid(&self) -> bool {
        match self {
            Self::Speak { text, after_ms } => {
                !text.trim().is_empty()
                    && text.trim() == text
                    && text.len() <= 2048
                    && *after_ms <= 60_000
                    && !text
                        .chars()
                        .any(|c| c.is_control() && c != '\n' && c != '\t')
            }
            Self::Wait { after_ms } => (1_000..=86_400_000).contains(after_ms),
            Self::Silence {} => true,
        }
    }
    pub fn reply(&self, message: &GroupMessage) -> Option<ChimeReply> {
        match self {
            Self::Speak { text, .. } => Some(ChimeReply {
                conversation: message.conversation.clone(),
                reply_to: message.id.clone(),
                text: text.clone(),
                parts: vec![ChimePart {
                    text: text.clone(),
                    pause_before_ms: 0,
                }],
            }),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupSpeechReview {
    pub message: GroupMessage,
    pub revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupAuthoredOutbound {
    pub outbound: GroupOutbound,
    pub anchor: String,
    pub revision: u64,
    pub due_at_ms: u64,
}
