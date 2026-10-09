//! 主动交流的来源契约：学习目标取得实际验证的进展后，Eve 择机邀请用户一起创作。
//!
//! 邀请依据的事实全部来自账本（用户原话、实践证据、已启用技能），由撰写器写成一段话；
//! 宿主按固定策略决定何时投递：用户要求安静时不投递，同一用户在冷却期内至多送达一条，
//! 同一学习目标只邀请一次。投递只走两条路：用户下一次在私聊里找 Eve 时随被动回复附带
//! （先由时机判断器看过用户这条消息与 Eve 的回复，适合才附带），或在操作者开启时主动私聊。
//! 判断与投递都先保存再执行，以平台真实回执为准；结果未知时不重发，也不当作已送达。
//!
//! 送达之后，识别器看用户接下来的几轮对话，判断用户是否在回应邀请、怎样回应，并逐字引用
//! 用户原话作为依据。连续的负面反馈（不需要、时机不对）让同一用户的冷却期加倍并停止主动私聊，
//! 直到用户再次正面回应；之前的反馈也作为数据交给之后的时机判断。
//!
//! 用户在回应中提出具体想法时，派生器把这条原话确定性地变成一个后续创作目标：沿用学习目标的
//! 知识与已启用的技能去做，做成后再走同样的邀请流程告诉用户；最初的学习目标关闭时一并取消。
use serde::{Deserialize, Serialize};
use std::{cmp::Reverse, fmt, future::Future, pin::Pin};

pub const OUTREACH_PLUGIN_ID: &str = "eve.outreach";
pub const OUTREACH_STATE_KEY: &str = "outreach.v1";
/// 邀请总数，不自动淘汰；达到容量后保留原记录并停止新的邀请。
pub const MAX_INVITATIONS: usize = 128;
pub const MAX_PREFERENCES: usize = 256;
pub const MAX_FACTS: usize = 12;
pub const MAX_FACT_BYTES: usize = 512;
pub const MAX_TEXT_BYTES: usize = 1024;
/// 每条邀请的投递尝试上限；用尽后记为失败，不再尝试。
pub const MAX_ATTEMPTS: usize = 6;
pub const MAX_COMPOSER_OUTPUT_BYTES: usize = 4096;
/// 每条邀请的时机判断上限；用尽后只等操作者开启的主动私聊。
pub const MAX_JUDGEMENTS: usize = 12;
/// 交给判断器的用户消息与回复各截取的上限。
pub const MAX_MOMENT_BYTES: usize = 2048;
pub const MAX_JUDGE_OUTPUT_BYTES: usize = 1024;
/// 每条已送达邀请的回应识别上限；用尽后不再识别。
pub const MAX_RESPONSES: usize = 3;
/// 一次回应识别交给识别器的后续对话轮数上限。
pub const MAX_RESPONSE_TURNS: usize = 3;
/// 送达后多久之内的对话算作可能的回应。
pub const RESPONSE_WINDOW_MS: u64 = 3 * 24 * 60 * 60 * 1000;
/// 回应依据的用户原话引用上限。
pub const MAX_QUOTE_BYTES: usize = 512;
pub const MAX_RESPONSE_OUTPUT_BYTES: usize = 2048;
/// 连续负面反馈使冷却期加倍的次数上限（至多 8 倍）。
pub const MAX_COOLDOWN_DOUBLINGS: u32 = 3;
/// 交给时机判断器的先前反馈条数上限。
pub const MAX_FEEDBACK_NOTES: usize = 3;
/// 后续创作目标的来源通道与核对标记。
pub const REQUEST_GOAL_CHANNEL: &str = "outreach.request";
pub const REQUEST_GOAL_VERIFICATION: &str = "outreach-request:v1";
pub const REQUEST_MARKER_SCHEMA: &str = "outreach-request-goal:v1";
pub const MAX_STATE_BYTES: usize = 2 * 1024 * 1024;

pub type OutreachResult<T> = Result<T, OutreachError>;
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type OutreachFuture<'a, T> = BoxFuture<'a, OutreachResult<T>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FactKind {
    /// 用户自己说过的原话。
    UserQuote,
    /// 实际运行验证过的进展。
    Progress,
    /// 已启用、可以直接用于后续创作的技能。
    Skill,
}

/// 交给撰写器的一条事实；由宿主从账本整理，不含推断。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    pub kind: FactKind,
    pub text: String,
}

/// 邀请依据的进展：某次已验证的实践，以及由它固化的技能（若有）。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Milestone {
    pub practice_run_id: String,
    pub skill_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DeliveryChannel {
    /// 随用户这条私聊消息的被动回复附带发送。
    Passive { message_id: String },
    /// 主动私聊。
    Proactive,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AttemptResult {
    /// 平台确认发送成功。
    Sent { platform_message_id: Option<String> },
    /// 平台拒绝或发送失败，内容没有送达；仍可在下一个时机投递。
    Failed {
        http_status: Option<u16>,
        biz_code: Option<i64>,
    },
    /// 没有写出：前面的回复片段失败、回复已过期，或没有可用的私聊路由。
    NotSent,
    /// 写出前后进程退出，是否送达不确定；不重发。
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryAttempt {
    pub channel: DeliveryChannel,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub result: Option<AttemptResult>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OutreachFailure {
    Provider,
    InvalidOutput,
    Timeout,
    Cancelled,
    /// 投递尝试已用尽。
    Exhausted,
}

/// 时机判断的结论。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Verdict {
    /// 适合在这次回复后附带邀请。
    Invite,
    /// 此刻不合适，例如用户在表达不再感兴趣、忙碌或需要安慰；等下一个时机。
    NotNow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgementOutcome {
    Verdict(Verdict),
    Failed(OutreachFailure),
    /// 判断时进程退出；不重放。
    Interrupted,
}

/// 针对用户某条私聊消息的一次时机判断；先保存再请求判断器。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Judgement {
    pub message_id: String,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub outcome: Option<JudgementOutcome>,
}

/// 识别出的用户回应。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ResponseKind {
    /// 提出了具体的想法或需求。
    Request,
    /// 表示有兴趣，但还没有具体想法。
    Interested,
    /// 表示不需要或不感兴趣。
    Declined,
    /// 表示此刻不方便、被打扰或时机不对。
    BadTiming,
    /// 这些对话没有回应邀请。
    Unrelated,
}
impl ResponseKind {
    /// 负面反馈：之后放慢主动交流。
    pub fn is_negative(self) -> bool {
        matches!(self, Self::Declined | Self::BadTiming)
    }
    /// 正面反馈：清除之前连续的负面反馈。
    pub fn is_positive(self) -> bool {
        matches!(self, Self::Request | Self::Interested)
    }
}

/// 送达后用户的一轮对话；账本只保存引用，原文在交互记忆中。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseTurn {
    pub evidence_id: String,
    pub message_id: String,
    pub at_ms: u64,
}

/// 识别结论：除 Unrelated 外，须指明用户哪条消息，并逐字引用其中的原话作为依据。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseVerdict {
    pub kind: ResponseKind,
    pub message_id: Option<String>,
    pub quote: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseOutcome {
    Verdict(ResponseVerdict),
    Failed(OutreachFailure),
    /// 识别时进程退出；这批对话不重放。
    Interrupted,
}

/// 针对送达后若干轮对话的一次回应识别；先保存确切输入再请求识别器。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseJudgement {
    pub turns: Vec<ResponseTurn>,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub outcome: Option<ResponseOutcome>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CancelReason {
    /// 学习目标已取消或结束，例如用户撤回了兴趣。
    GoalClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum InvitationStatus {
    /// 已保存准入，正在撰写。
    Composing,
    /// 已撰写，等待合适的投递时机。
    Pending,
    /// 已保存投递尝试，正在发送。
    Delivering,
    Delivered,
    /// 送达与否不确定；不重发。
    Unknown,
    Cancelled(CancelReason),
    Failed(OutreachFailure),
    /// 撰写时进程退出；不重放。
    Interrupted,
}
impl InvitationStatus {
    pub fn is_open(self) -> bool {
        matches!(self, Self::Composing | Self::Pending | Self::Delivering)
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invitation {
    pub id: String,
    /// 邀请对象；与学习目标的可见用户相同。
    pub owner: String,
    pub goal_id: String,
    pub milestone: Milestone,
    pub facts: Vec<Fact>,
    pub composer_version: String,
    pub created_at_ms: u64,
    pub composed_at_ms: Option<u64>,
    pub text: Option<String>,
    pub status: InvitationStatus,
    pub judgements: Vec<Judgement>,
    pub attempts: Vec<DeliveryAttempt>,
    pub delivered_at_ms: Option<u64>,
    pub closed_at_ms: Option<u64>,
    /// 送达后的回应识别，按时间先后。
    #[serde(default)]
    pub responses: Vec<ResponseJudgement>,
}
impl Invitation {
    /// 正在进行的时机判断。
    pub fn judging(&self) -> bool {
        self.judgements
            .last()
            .is_some_and(|judgement| judgement.outcome.is_none())
    }
    /// 某条消息的判断结论。
    pub fn verdict_for(&self, message_id: &str) -> Option<Verdict> {
        self.judgements
            .iter()
            .rev()
            .find(|judgement| judgement.message_id == message_id)
            .and_then(|judgement| match judgement.outcome {
                Some(JudgementOutcome::Verdict(verdict)) => Some(verdict),
                _ => None,
            })
    }
    pub fn proactive_attempts(&self) -> usize {
        self.attempts
            .iter()
            .filter(|attempt| attempt.channel == DeliveryChannel::Proactive)
            .count()
    }
    /// 撰写完成的时间；投递策略从此开始计时。
    pub fn pending_since_ms(&self) -> Option<u64> {
        self.composed_at_ms
    }
    /// 正在进行的回应识别。
    pub fn responding(&self) -> bool {
        self.responses
            .last()
            .is_some_and(|response| response.outcome.is_none())
    }
    /// 用户对这条邀请的回应：第一个不是 Unrelated 的识别结论。
    pub fn feedback(&self) -> Option<&ResponseVerdict> {
        self.responses
            .iter()
            .find_map(|response| match &response.outcome {
                Some(ResponseOutcome::Verdict(verdict))
                    if verdict.kind != ResponseKind::Unrelated =>
                {
                    Some(verdict)
                }
                _ => None,
            })
    }
    /// 还在等用户回应：已送达、没有结论、识别次数未用尽，且没有正在进行的识别。
    pub fn listening(&self) -> bool {
        self.status == InvitationStatus::Delivered
            && self.feedback().is_none()
            && self.responses.len() < MAX_RESPONSES
            && !self.responding()
    }
    /// 这轮对话是否已经交给过识别器，或是附带邀请的那条消息本身。
    pub fn heard(&self, message_id: &str) -> bool {
        self.responses
            .iter()
            .flat_map(|response| &response.turns)
            .any(|turn| turn.message_id == message_id)
            || self.attempts.iter().any(|attempt| {
                matches!(&attempt.channel, DeliveryChannel::Passive { message_id: id } if id == message_id)
            })
    }
}

/// 用户的主动交流偏好；要求安静后不投递任何邀请，直到用户恢复。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerPreference {
    pub owner: String,
    pub quiet: bool,
    pub changed_at_ms: u64,
}

/// 宿主的投递策略。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutreachPolicy {
    /// 同一用户两次送达之间的最短间隔。
    pub cooldown_ms: u64,
    /// 开启主动私聊时，邀请撰写完成后等待多久仍未在被动窗口送达才主动发送；None 表示不主动发送。
    pub proactive_after_ms: Option<u64>,
}
impl Default for OutreachPolicy {
    fn default() -> Self {
        Self {
            cooldown_ms: 24 * 60 * 60 * 1000,
            proactive_after_ms: None,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct OutreachSnapshot {
    pub invitations: Vec<Invitation>,
    pub preferences: Vec<OwnerPreference>,
}
impl OutreachSnapshot {
    pub fn quiet(&self, owner: &str) -> bool {
        self.preferences
            .iter()
            .any(|preference| preference.owner == owner && preference.quiet)
    }
    pub fn for_owner<'a>(&'a self, owner: &'a str) -> impl Iterator<Item = &'a Invitation> {
        self.invitations
            .iter()
            .filter(move |invitation| invitation.owner == owner)
    }
    pub fn for_goal(&self, goal_id: &str) -> Option<&Invitation> {
        self.invitations
            .iter()
            .find(|invitation| invitation.goal_id == goal_id)
    }

    /// 用户最近连续的负面反馈次数：从最近送达的邀请往前数，遇到正面反馈即停；
    /// 没有回应的邀请不计。
    pub fn negative_streak(&self, owner: &str) -> u32 {
        let mut delivered: Vec<&Invitation> = self
            .for_owner(owner)
            .filter(|invitation| invitation.delivered_at_ms.is_some())
            .collect();
        delivered.sort_by_key(|invitation| Reverse((invitation.delivered_at_ms, &invitation.id)));
        let mut streak = 0;
        for invitation in delivered {
            match invitation.feedback().map(|verdict| verdict.kind) {
                Some(kind) if kind.is_negative() => streak += 1,
                Some(kind) if kind.is_positive() => break,
                _ => {}
            }
        }
        streak
    }

    /// 这个用户当前的冷却期：每次连续负面反馈加倍，至多 8 倍。
    pub fn cooldown_for(&self, owner: &str, policy: &OutreachPolicy) -> u64 {
        let doublings = self.negative_streak(owner).min(MAX_COOLDOWN_DOUBLINGS);
        policy.cooldown_ms.saturating_mul(1 << doublings)
    }

    /// 用户对最近几条邀请的回应，新的在前；交给时机判断器作为数据。
    pub fn feedback_notes(&self, owner: &str) -> Vec<FeedbackNote> {
        let mut answered: Vec<(&Invitation, &ResponseVerdict)> = self
            .for_owner(owner)
            .filter_map(|invitation| invitation.feedback().map(|verdict| (invitation, verdict)))
            .collect();
        answered
            .sort_by_key(|(invitation, _)| Reverse((invitation.delivered_at_ms, &invitation.id)));
        answered
            .into_iter()
            .take(MAX_FEEDBACK_NOTES)
            .map(|(_, verdict)| FeedbackNote {
                kind: verdict.kind,
                quote: verdict.quote.clone().unwrap_or_default(),
            })
            .collect()
    }

    /// 还在等用户回应的邀请。
    pub fn listening(&self) -> impl Iterator<Item = &Invitation> {
        self.invitations
            .iter()
            .filter(|invitation| invitation.listening())
    }

    /// 某个用户此刻可以投递的邀请：没有要求安静、没有正在发送的邀请、距上次送达已过冷却期
    /// （连续负面反馈时加倍）；取等待最久的一条。
    pub fn due_for(
        &self,
        owner: &str,
        now_ms: u64,
        policy: &OutreachPolicy,
    ) -> Option<&Invitation> {
        if self.quiet(owner) {
            return None;
        }
        let owned: Vec<&Invitation> = self
            .invitations
            .iter()
            .filter(|invitation| invitation.owner == owner)
            .collect();
        let busy = owned.iter().any(|invitation| {
            invitation.status == InvitationStatus::Delivering || invitation.judging()
        });
        let recent = owned
            .iter()
            .filter_map(|invitation| invitation.delivered_at_ms)
            .max();
        let cooldown = self.cooldown_for(owner, policy);
        if busy || recent.is_some_and(|at| now_ms.saturating_sub(at) < cooldown) {
            return None;
        }
        owned
            .into_iter()
            .filter(|invitation| invitation.status == InvitationStatus::Pending)
            .min_by_key(|invitation| (invitation.composed_at_ms, invitation.id.clone()))
    }

    /// 此刻可以为用户这条私聊消息做时机判断的邀请：可投递，且判断次数未用尽。
    pub fn judgeable_for(
        &self,
        owner: &str,
        now_ms: u64,
        policy: &OutreachPolicy,
    ) -> Option<&Invitation> {
        self.due_for(owner, now_ms, policy)
            .filter(|invitation| invitation.judgements.len() < MAX_JUDGEMENTS)
    }

    /// 可以主动私聊的邀请：在被动窗口等待已超过设定时间，且这条邀请还没有主动发送过。
    /// 主动消息受平台配额限制，每条邀请至多主动尝试一次；用户最近的反馈是负面的，
    /// 只等用户自己找 Eve 时再看时机。
    pub fn due_proactive(&self, now_ms: u64, policy: &OutreachPolicy) -> Option<&Invitation> {
        let after = policy.proactive_after_ms?;
        let mut owners: Vec<&str> = self
            .invitations
            .iter()
            .map(|invitation| invitation.owner.as_str())
            .collect();
        owners.sort();
        owners.dedup();
        owners
            .into_iter()
            .filter(|owner| self.negative_streak(owner) == 0)
            .filter_map(|owner| self.due_for(owner, now_ms, policy))
            .filter(|invitation| {
                invitation.proactive_attempts() == 0
                    && invitation
                        .pending_since_ms()
                        .is_some_and(|since| now_ms.saturating_sub(since) >= after)
            })
            .min_by_key(|invitation| (invitation.composed_at_ms, invitation.id.clone()))
    }
}

/// 撰写请求：只含宿主整理的事实；规则与领域无关。
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ComposeRequest {
    pub invitation_id: String,
    pub composer_version: String,
    pub facts: Vec<Fact>,
}

/// 可替换撰写器；至多一次模型请求、零工具，返回邀请正文。
pub trait InvitationComposer: Send + Sync {
    fn version(&self) -> &str;
    fn compose(&self, request: ComposeRequest) -> OutreachFuture<'_, String>;
}

/// 用户对之前某条邀请的回应及原话。
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct FeedbackNote {
    pub kind: ResponseKind,
    pub quote: String,
}

/// 时机判断请求：邀请正文、用户这条私聊消息与 Eve 刚写好的回复（都已截断），
/// 以及用户对之前邀请的回应。
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct JudgeRequest {
    pub invitation_id: String,
    pub judge_version: String,
    pub invitation: String,
    pub user_message: String,
    pub reply: String,
    pub feedback: Vec<FeedbackNote>,
}

/// 可替换判断器；至多一次模型请求、零工具。
pub trait TimingJudge: Send + Sync {
    fn version(&self) -> &str;
    fn judge(&self, request: JudgeRequest) -> OutreachFuture<'_, Verdict>;
}

/// 送达后一轮对话的原文（都已截断）。
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ResponseTurnText {
    pub message_id: String,
    pub user_message: String,
    pub reply: String,
}

/// 回应识别请求：已送达的邀请正文与之后的几轮对话。
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ResponseRequest {
    pub invitation_id: String,
    pub judge_version: String,
    pub invitation: String,
    pub turns: Vec<ResponseTurnText>,
}

/// 可替换回应识别器；至多一次模型请求、零工具。结论须通过 [`validate_verdict`]。
pub trait ResponseJudge: Send + Sync {
    fn version(&self) -> &str;
    fn judge(&self, request: ResponseRequest) -> OutreachFuture<'_, ResponseVerdict>;
}

/// 后续创作目标等待原因中的机器可读标记。learning_goal_id 是最初由兴趣派生的学习目标，
/// 用户连续提出想法时保持不变；parent_goal_id 是这次邀请所属的目标。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestMarker {
    pub schema: String,
    pub invitation_id: String,
    pub parent_goal_id: String,
    pub learning_goal_id: String,
}
impl RequestMarker {
    pub fn parse(text: &str) -> Option<Self> {
        let marker: Self = serde_json::from_str(text).ok()?;
        (marker.schema == REQUEST_MARKER_SCHEMA).then_some(marker)
    }
}

/// 本次同步对后续创作目标做出的修改；暂缓的邀请下次重新核对。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RequestReport {
    pub created: Vec<String>,
    pub cancelled: Vec<String>,
    /// 修订冲突或认知容量不足而暂缓的邀请 ID。
    pub deferred: Vec<String>,
}

/// 可替换派生器：由识别为“提出想法”的回应确定性地同步后续创作目标；零模型请求，可重复调用。
/// 最初的学习目标关闭（例如用户撤回兴趣）时取消仍在等待的后续目标。
pub trait RequestGoalDeriver: Send + Sync {
    fn version(&self) -> &str;
    fn reconcile(&self, snapshot: &OutreachSnapshot, now_ms: u64) -> OutreachResult<RequestReport>;
}

/// 仅可信宿主持有；不发布给模型或不受信插件。
pub trait OutreachAdmin: Send + Sync {
    fn snapshot(&self) -> OutreachResult<OutreachSnapshot>;
    /// 原子保存 Composing 邀请，成功返回后才可请求撰写。同一学习目标已有邀请时返回 None。
    fn begin(
        &self,
        owner: &str,
        goal_id: &str,
        milestone: Milestone,
        facts: Vec<Fact>,
        composer_version: &str,
        now_ms: u64,
    ) -> OutreachResult<Option<Invitation>>;
    /// 保存撰写结果：成功进入 Pending，失败记为 Failed，不重试。
    fn record_composition(
        &self,
        id: &str,
        at_ms: u64,
        result: Result<String, OutreachFailure>,
    ) -> OutreachResult<Invitation>;
    /// 保存一次针对某条私聊消息的时机判断，返回后才可请求判断器；同一消息只判断一次。
    fn begin_judgement(&self, id: &str, message_id: &str, at_ms: u64)
    -> OutreachResult<Invitation>;
    /// 保存判断结论或失败；失败与中断都不重试同一条消息。
    fn record_judgement(
        &self,
        id: &str,
        message_id: &str,
        at_ms: u64,
        outcome: Result<Verdict, OutreachFailure>,
    ) -> OutreachResult<Invitation>;
    /// 保存一次投递尝试并进入 Delivering；返回后才可发送。被动附带须先有对这条消息的
    /// 邀请判断。
    fn claim(&self, id: &str, at_ms: u64, channel: DeliveryChannel) -> OutreachResult<Invitation>;
    /// 保存投递结果：送达进入 Delivered；没有写出或平台拒绝时回到 Pending，尝试用尽记为失败。
    fn record_delivery(
        &self,
        id: &str,
        at_ms: u64,
        result: AttemptResult,
    ) -> OutreachResult<Invitation>;
    /// 学习目标关闭时取消尚未送达的邀请；正在发送的邀请不能取消。
    fn cancel(&self, id: &str, at_ms: u64, reason: CancelReason) -> OutreachResult<Invitation>;
    fn set_quiet(&self, owner: &str, quiet: bool, at_ms: u64) -> OutreachResult<OwnerPreference>;
    /// 保存一次回应识别及其确切输入，返回后才可请求识别器。只针对仍在等待回应的已送达邀请；
    /// 对话须发生在送达之后、回应窗口之内，不是附带邀请的那条消息，且每轮只识别一次。
    fn begin_response(
        &self,
        id: &str,
        turns: Vec<ResponseTurn>,
        at_ms: u64,
    ) -> OutreachResult<Invitation>;
    /// 保存识别结论或失败；失败与中断都不重放同一批对话。
    fn record_response(
        &self,
        id: &str,
        at_ms: u64,
        outcome: Result<ResponseVerdict, OutreachFailure>,
    ) -> OutreachResult<Invitation>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutreachError {
    InvalidInput,
    NotFound,
    Unavailable,
    CorruptState,
    UnsupportedVersion,
    Storage,
    LimitReached,
    Conflict,
    Outreach(OutreachFailure),
}
impl fmt::Display for OutreachError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "主动交流输入无效",
            Self::NotFound => "没有这条邀请",
            Self::Unavailable => "主动交流服务不可用",
            Self::CorruptState => "主动交流状态损坏；未清空",
            Self::UnsupportedVersion => "不支持该主动交流状态版本",
            Self::Storage => "主动交流提交无法确认；须重新打开核对",
            Self::LimitReached => "邀请容量已满；保留原记录",
            Self::Conflict => "邀请状态冲突",
            Self::Outreach(_) => "邀请撰写失败",
        })
    }
}
impl std::error::Error for OutreachError {}

pub fn validate_id(value: &str) -> OutreachResult<()> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(OutreachError::InvalidInput);
    }
    Ok(())
}

/// 邀请正文：非空、至多 1024 字节，除换行外不含控制字符。
pub fn validate_text(value: &str) -> OutreachResult<()> {
    if value.trim().is_empty()
        || value.trim() != value
        || value.len() > MAX_TEXT_BYTES
        || value.chars().any(|c| c.is_control() && c != '\n')
    {
        return Err(OutreachError::InvalidInput);
    }
    Ok(())
}

pub fn validate_facts(facts: &[Fact]) -> OutreachResult<()> {
    if facts.is_empty()
        || facts.len() > MAX_FACTS
        || !facts.iter().any(|fact| fact.kind == FactKind::Progress)
        || facts.iter().any(|fact| {
            fact.text.trim().is_empty()
                || fact.text.len() > MAX_FACT_BYTES
                || fact.text.chars().any(|c| c.is_control() && c != '\n')
        })
    {
        return Err(OutreachError::InvalidInput);
    }
    Ok(())
}

/// 用户原话引用：非空、首尾无空白、至多 512 字节，除换行外不含控制字符。
pub fn validate_quote(value: &str) -> OutreachResult<()> {
    if value.trim().is_empty()
        || value.trim() != value
        || value.len() > MAX_QUOTE_BYTES
        || value.chars().any(|c| c.is_control() && c != '\n')
    {
        return Err(OutreachError::InvalidInput);
    }
    Ok(())
}

/// 结论本身的形状：Unrelated 不带消息与引用，其余两者都有且合规。
pub fn validate_verdict_shape(verdict: &ResponseVerdict) -> OutreachResult<()> {
    match (verdict.kind, &verdict.message_id, &verdict.quote) {
        (ResponseKind::Unrelated, None, None) => Ok(()),
        (ResponseKind::Unrelated, _, _) => Err(OutreachError::InvalidInput),
        (_, Some(message_id), Some(quote)) => {
            validate_id(message_id)?;
            validate_quote(quote)
        }
        _ => Err(OutreachError::InvalidInput),
    }
}

/// 核对识别结论与请求一致：引用必须是所指消息中用户原文的连续片段（忽略空白差异），
/// 不能取自 Eve 的回复或邀请。
pub fn validate_verdict(
    request: &ResponseRequest,
    verdict: &ResponseVerdict,
) -> OutreachResult<()> {
    validate_verdict_shape(verdict)?;
    let (Some(message_id), Some(quote)) = (&verdict.message_id, &verdict.quote) else {
        return Ok(());
    };
    let turn = request
        .turns
        .iter()
        .find(|turn| turn.message_id == *message_id)
        .ok_or(OutreachError::InvalidInput)?;
    if !quote_matches(&turn.user_message, quote) {
        return Err(OutreachError::InvalidInput);
    }
    Ok(())
}

/// 第一条原文包含引用的对话；用于把识别器给出的原话定位到消息。
pub fn locate_quote<'a>(turns: &'a [ResponseTurnText], quote: &str) -> Option<&'a str> {
    turns
        .iter()
        .find(|turn| quote_matches(&turn.user_message, quote))
        .map(|turn| turn.message_id.as_str())
}

fn quote_matches(text: &str, quote: &str) -> bool {
    let compact =
        |value: &str| -> String { value.chars().filter(|c| !c.is_whitespace()).collect() };
    let quote = compact(quote);
    !quote.is_empty() && compact(text).contains(&quote)
}

/// 同一主体与邀请固定映射到一个后续创作目标；重启或重复同步都不会另建目标。
pub fn request_goal_id(subject: &str, invitation_id: &str) -> String {
    format!(
        "eve.outreach.request.{}",
        digest(&["outreach.request.goal:v1", subject, invitation_id])
    )
}

fn digest(parts: &[&str]) -> String {
    use ring::digest::{Context, SHA256};
    let mut context = Context::new(&SHA256);
    for part in parts {
        context.update(&(part.len() as u64).to_be_bytes());
        context.update(part.as_bytes());
    }
    let digest: String = context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    digest[..32].to_string()
}

/// 同一学习目标只邀请一次；重复准入得到同一 ID。
pub fn invitation_id(goal_id: &str) -> String {
    format!("invite-{}", digest(&["outreach.invitation:v1", goal_id]))
}

macro_rules! redacted { ($($ty:ty),+ $(,)?) => { $(impl fmt::Debug for $ty { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(concat!(stringify!($ty), "(<redacted>)")) } })+ }; }
redacted!(
    Fact,
    Invitation,
    OutreachSnapshot,
    ComposeRequest,
    JudgeRequest,
    ResponseVerdict,
    FeedbackNote,
    ResponseTurnText,
    ResponseRequest
);

#[cfg(test)]
mod tests {
    use super::*;

    fn invitation(owner: &str, id: &str, status: InvitationStatus, composed: u64) -> Invitation {
        Invitation {
            id: id.into(),
            owner: owner.into(),
            goal_id: format!("goal-{id}"),
            milestone: Milestone {
                practice_run_id: "practice-1".into(),
                skill_id: None,
            },
            facts: vec![Fact {
                kind: FactKind::Progress,
                text: "已验证".into(),
            }],
            composer_version: "composer:v1".into(),
            created_at_ms: composed,
            composed_at_ms: Some(composed),
            text: Some("要不要一起做？".into()),
            status,
            judgements: vec![],
            attempts: vec![],
            delivered_at_ms: None,
            closed_at_ms: None,
            responses: vec![],
        }
    }

    #[test]
    fn delivery_waits_for_quiet_busy_and_cooldown() {
        let policy = OutreachPolicy {
            cooldown_ms: 1000,
            proactive_after_ms: Some(500),
        };
        let mut snapshot = OutreachSnapshot {
            invitations: vec![
                invitation("a", "1", InvitationStatus::Pending, 100),
                invitation("a", "2", InvitationStatus::Pending, 50),
            ],
            preferences: vec![],
        };
        assert_eq!(snapshot.due_for("a", 200, &policy).unwrap().id, "2");
        assert!(snapshot.due_for("b", 200, &policy).is_none());
        // 主动私聊要等被动窗口等待足够久。
        assert!(snapshot.due_proactive(200, &policy).is_none());
        assert_eq!(snapshot.due_proactive(600, &policy).unwrap().id, "2");

        snapshot.invitations[1].judgements.push(Judgement {
            message_id: "msg-1".into(),
            started_at_ms: 150,
            finished_at_ms: None,
            outcome: None,
        });
        assert!(
            snapshot.due_for("a", 200, &policy).is_none(),
            "判断中不再取出"
        );
        snapshot.invitations[1].judgements.clear();
        snapshot.invitations[1].status = InvitationStatus::Delivering;
        assert!(
            snapshot.due_for("a", 200, &policy).is_none(),
            "一次只发送一条"
        );
        snapshot.invitations[1].status = InvitationStatus::Delivered;
        snapshot.invitations[1].delivered_at_ms = Some(300);
        assert!(
            snapshot.due_for("a", 1200, &policy).is_none(),
            "冷却期内不再送达"
        );
        assert_eq!(snapshot.due_for("a", 1300, &policy).unwrap().id, "1");

        snapshot.preferences.push(OwnerPreference {
            owner: "a".into(),
            quiet: true,
            changed_at_ms: 400,
        });
        assert!(
            snapshot.due_for("a", 5000, &policy).is_none(),
            "要求安静时不投递"
        );

        snapshot.preferences.clear();
        snapshot.invitations[0].attempts.push(DeliveryAttempt {
            channel: DeliveryChannel::Proactive,
            started_at_ms: 1300,
            finished_at_ms: Some(1310),
            result: Some(AttemptResult::Failed {
                http_status: Some(400),
                biz_code: Some(22009),
            }),
        });
        assert!(
            snapshot.due_proactive(9000, &policy).is_none(),
            "每条至多主动尝试一次"
        );
        assert_eq!(
            snapshot.due_for("a", 9000, &policy).unwrap().id,
            "1",
            "仍可在被动窗口送达"
        );
        assert!(
            snapshot
                .due_proactive(9000, &OutreachPolicy::default())
                .is_none(),
            "默认不主动发送"
        );
    }

    fn answered(id: &str, delivered: u64, kind: ResponseKind) -> Invitation {
        let mut entry = invitation("a", id, InvitationStatus::Delivered, 10);
        entry.delivered_at_ms = Some(delivered);
        let verdict = if kind == ResponseKind::Unrelated {
            ResponseVerdict {
                kind,
                message_id: None,
                quote: None,
            }
        } else {
            ResponseVerdict {
                kind,
                message_id: Some(format!("reply-{id}")),
                quote: Some("原话".into()),
            }
        };
        entry.responses.push(ResponseJudgement {
            turns: vec![ResponseTurn {
                evidence_id: format!("evidence-{id}"),
                message_id: format!("reply-{id}"),
                at_ms: delivered + 1,
            }],
            started_at_ms: delivered + 2,
            finished_at_ms: Some(delivered + 3),
            outcome: Some(ResponseOutcome::Verdict(verdict)),
        });
        entry
    }

    #[test]
    fn negative_feedback_slows_outreach_until_a_positive_response() {
        let policy = OutreachPolicy {
            cooldown_ms: 1000,
            proactive_after_ms: Some(0),
        };
        let pending = invitation("a", "next", InvitationStatus::Pending, 100);
        let mut snapshot = OutreachSnapshot {
            invitations: vec![answered("1", 100, ResponseKind::BadTiming), pending],
            preferences: vec![],
        };
        assert_eq!(snapshot.negative_streak("a"), 1);
        assert_eq!(snapshot.cooldown_for("a", &policy), 2000);
        assert!(snapshot.due_for("a", 1500, &policy).is_none(), "冷却期加倍");
        assert_eq!(snapshot.due_for("a", 2100, &policy).unwrap().id, "next");
        assert!(
            snapshot.due_proactive(9000, &policy).is_none(),
            "负面反馈后不再主动私聊"
        );
        let [note] = snapshot.feedback_notes("a").try_into().ok().unwrap();
        assert_eq!(
            (note.kind, note.quote.as_str()),
            (ResponseKind::BadTiming, "原话")
        );

        // 没有回应的邀请不计入，也不打断连续计数；加倍有上限。
        for (index, kind) in [
            ResponseKind::Unrelated,
            ResponseKind::Declined,
            ResponseKind::BadTiming,
            ResponseKind::Declined,
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("later-{index}");
            snapshot
                .invitations
                .push(answered(&id, 200 + index as u64, kind));
        }
        assert_eq!(snapshot.negative_streak("a"), 4);
        assert_eq!(snapshot.cooldown_for("a", &policy), 8000);
        assert_eq!(snapshot.feedback_notes("a").len(), MAX_FEEDBACK_NOTES);

        // 用户再次正面回应后恢复原策略。
        snapshot
            .invitations
            .push(answered("positive", 300, ResponseKind::Interested));
        assert_eq!(snapshot.negative_streak("a"), 0);
        assert_eq!(snapshot.cooldown_for("a", &policy), 1000);
        assert_eq!(snapshot.due_proactive(9000, &policy).unwrap().id, "next");
        assert_eq!(snapshot.negative_streak("b"), 0);
    }

    #[test]
    fn verdicts_must_quote_the_user_and_name_the_message() {
        let request = ResponseRequest {
            invitation_id: "invite-1".into(),
            judge_version: "judge:v1".into(),
            invitation: "要不要一起做？".into(),
            turns: vec![
                ResponseTurnText {
                    message_id: "m1".into(),
                    user_message: "今天好累".into(),
                    reply: "辛苦了，要不要一起做？".into(),
                },
                ResponseTurnText {
                    message_id: "m2".into(),
                    user_message: "我在 上班，晚点说".into(),
                    reply: "好的".into(),
                },
            ],
        };
        let verdict = |kind, message: Option<&str>, quote: Option<&str>| ResponseVerdict {
            kind,
            message_id: message.map(Into::into),
            quote: quote.map(Into::into),
        };
        assert!(validate_verdict(&request, &verdict(ResponseKind::Unrelated, None, None)).is_ok());
        assert!(
            validate_verdict(
                &request,
                &verdict(ResponseKind::BadTiming, Some("m2"), Some("我在上班"))
            )
            .is_ok(),
            "忽略空白差异"
        );
        for (kind, message, quote) in [
            (ResponseKind::Unrelated, Some("m1"), None),
            (ResponseKind::BadTiming, None, Some("我在上班")),
            (ResponseKind::BadTiming, Some("m1"), Some("我在上班")),
            (ResponseKind::BadTiming, Some("m3"), Some("我在上班")),
            (ResponseKind::Interested, Some("m1"), Some("一起做")),
            (ResponseKind::Declined, Some("m2"), Some(" 我在上班")),
        ] {
            assert!(
                validate_verdict(&request, &verdict(kind, message, quote)).is_err(),
                "{kind:?} {message:?} {quote:?}"
            );
        }
        assert_eq!(locate_quote(&request.turns, "晚点说"), Some("m2"));
        assert_eq!(locate_quote(&request.turns, "一起做"), None, "不取自回复");
    }
}
