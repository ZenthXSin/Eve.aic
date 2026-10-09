//! 主动交流的来源契约：学习目标取得实际验证的进展后，Eve 择机邀请用户一起创作。
//!
//! 邀请依据的事实全部来自账本（用户原话、实践证据、已启用技能），由撰写器写成一段话；
//! 宿主按固定策略决定何时投递：用户要求安静时不投递，同一用户在冷却期内至多送达一条，
//! 同一学习目标只邀请一次。投递只走两条路：用户下一次在私聊里找 Eve 时随被动回复附带
//! （先由时机判断器看过用户这条消息与 Eve 的回复，适合才附带），或在操作者开启时主动私聊。
//! 判断与投递都先保存再执行，以平台真实回执为准；结果未知时不重发，也不当作已送达。
use serde::{Deserialize, Serialize};
use std::{fmt, future::Future, pin::Pin};

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

    /// 某个用户此刻可以投递的邀请：没有要求安静、没有正在发送的邀请、距上次送达已过冷却期；
    /// 取等待最久的一条。
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
        if busy || recent.is_some_and(|at| now_ms.saturating_sub(at) < policy.cooldown_ms) {
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
    /// 主动消息受平台配额限制，每条邀请至多主动尝试一次。
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

/// 时机判断请求：邀请正文、用户这条私聊消息与 Eve 刚写好的回复（都已截断）。
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct JudgeRequest {
    pub invitation_id: String,
    pub judge_version: String,
    pub invitation: String,
    pub user_message: String,
    pub reply: String,
}

/// 可替换判断器；至多一次模型请求、零工具。
pub trait TimingJudge: Send + Sync {
    fn version(&self) -> &str;
    fn judge(&self, request: JudgeRequest) -> OutreachFuture<'_, Verdict>;
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

/// 同一学习目标只邀请一次；重复准入得到同一 ID。
pub fn invitation_id(goal_id: &str) -> String {
    use ring::digest::{Context, SHA256};
    let mut context = Context::new(&SHA256);
    for part in ["outreach.invitation:v1", goal_id] {
        context.update(&(part.len() as u64).to_be_bytes());
        context.update(part.as_bytes());
    }
    let digest: String = context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("invite-{}", &digest[..32])
}

macro_rules! redacted { ($($ty:ty),+ $(,)?) => { $(impl fmt::Debug for $ty { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(concat!(stringify!($ty), "(<redacted>)")) } })+ }; }
redacted!(
    Fact,
    Invitation,
    OutreachSnapshot,
    ComposeRequest,
    JudgeRequest
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
}
