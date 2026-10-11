//! 请求澄清契约：自主学习目标在当前修订卡住时（研究没有找到可引用的资料、实践尝试用尽仍未验证，
//! 或运行环境不适用），向用户提一个具体问题，等用户补充后再继续。
//!
//! 每个目标修订至多请求一次；先保存准入再撰写，先保存时机判断再请求判断器，先保存投递尝试再发送。
//! 问题只随用户自己发来的下一条私聊回复附带，不主动私聊。用户之后的下一轮私聊记为回答，
//! 回答作为普通交互进入记忆，由兴趣观察照常处理；本契约不改写目标。
use serde::{Deserialize, Serialize};
use std::{fmt, future::Future, pin::Pin};

pub const CLARIFY_PLUGIN_ID: &str = "eve.clarify";
pub const CLARIFY_STATE_KEY: &str = "clarify.v1";
pub const MAX_REQUESTS: usize = 128;
pub const MAX_PREFERENCES: usize = 256;
pub const MAX_FACTS: usize = 12;
pub const MAX_FACT_BYTES: usize = 512;
pub const MAX_QUESTION_BYTES: usize = 1024;
pub const MAX_COMPOSER_OUTPUT_BYTES: usize = 4096;
/// 每个问题至多投递尝试次数（没有写出或平台拒绝时可在下一个时机再试）。
pub const MAX_ATTEMPTS: usize = 6;
/// 每个问题至多时机判断次数。
pub const MAX_JUDGEMENTS: usize = 12;
pub const MAX_ANSWER_BYTES: usize = 512;
/// 送达后多久内的下一轮私聊记为回答；之后不再等待。
pub const ANSWER_WINDOW_MS: u64 = 3 * 24 * 60 * 60 * 1000;
pub const MAX_STATE_BYTES: usize = 2 * 1024 * 1024;

pub type ClarifyResult<T> = Result<T, ClarifyError>;
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type ClarifyFuture<'a, T> = BoxFuture<'a, ClarifyResult<T>>;

/// 目标在当前修订卡住的原因；由宿主按账本中已结束的记录确定性判定。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StallKind {
    /// 研究已完成，但没有找到可逐字引用的相关资料。
    ResearchEmpty,
    /// 实践尝试用尽，实际运行仍未验证。
    PracticeUnverified,
    /// 草稿器判断现有运行环境做不了这个目标。
    PracticeNotApplicable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stall {
    pub kind: StallKind,
    /// 判定依据的研究或实践记录 ID。
    pub run_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FactKind {
    /// 用户自己说过的原话。
    UserQuote,
    /// Eve 实际做过的尝试及结果。
    Attempt,
    /// 现有运行环境能做什么。
    Capability,
}

/// 交给撰写器的一条事实；由宿主从账本整理，不含推断。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    pub kind: FactKind,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ClarifyFailure {
    Provider,
    InvalidOutput,
    Timeout,
    Cancelled,
    /// 投递或判断次数已用尽。
    Exhausted,
}

/// 时机判断的结论。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum JudgeOutcome {
    /// 适合在这次回复后附带问题。
    Ask,
    /// 此刻不合适；等下一个时机。
    NotNow,
    Failed {
        failure: ClarifyFailure,
    },
    /// 判断时进程退出；这条消息不再判断。
    Interrupted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Judgement {
    pub message_id: String,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub outcome: Option<JudgeOutcome>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DeliveryResult {
    /// 平台确认发送成功。
    Sent { platform_message_id: Option<String> },
    /// 平台拒绝或发送失败，内容没有送达；仍可在下一个时机投递。
    Failed {
        http_status: Option<u16>,
        biz_code: Option<i64>,
    },
    /// 没有写出：前面的回复片段失败、回复已过期或正文不合规。
    NotSent,
    /// 写出前后进程退出，是否送达不确定；不重发。
    Unknown,
}

/// 随用户某条私聊消息的被动回复附带的一次投递尝试。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryAttempt {
    pub message_id: String,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub result: Option<DeliveryResult>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CancelReason {
    /// 送达前目标有了新修订（例如用户自己补充了信息），问题已过时。
    GoalChanged,
    /// 学习目标已取消或结束，例如用户撤回了兴趣。
    GoalClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ClarifyStatus {
    /// 已保存准入，正在撰写。
    Composing,
    /// 已撰写，等待合适的投递时机。
    Pending,
    /// 已保存投递尝试，正在发送。
    Delivering,
    /// 已送达，等待用户下一轮私聊。
    Delivered,
    /// 送达后用户有了下一轮私聊。
    Answered,
    /// 送达后回答窗口内没有私聊。
    Expired,
    /// 送达与否不确定；不重发。
    Unknown,
    Cancelled(CancelReason),
    Failed(ClarifyFailure),
    /// 撰写时进程退出；不重放。
    Interrupted,
}
impl ClarifyStatus {
    pub fn is_open(self) -> bool {
        matches!(self, Self::Composing | Self::Pending | Self::Delivering)
    }
}

/// 送达后用户的下一轮私聊：引用其中用户原话的前缀，以及对应的记忆证据。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    pub message_id: String,
    pub evidence_id: String,
    pub at_ms: u64,
    pub text: String,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clarification {
    pub id: String,
    /// 提问对象；与学习目标的可见用户相同。
    pub owner: String,
    pub goal_id: String,
    pub goal_revision: u64,
    pub stall: Stall,
    pub facts: Vec<Fact>,
    pub composer_version: String,
    pub created_at_ms: u64,
    pub composed_at_ms: Option<u64>,
    pub question: Option<String>,
    pub status: ClarifyStatus,
    pub judgements: Vec<Judgement>,
    pub attempts: Vec<DeliveryAttempt>,
    pub delivered_at_ms: Option<u64>,
    pub answer: Option<Answer>,
    pub closed_at_ms: Option<u64>,
}
impl Clarification {
    /// 正在进行的时机判断。
    pub fn judging(&self) -> bool {
        self.judgements
            .last()
            .is_some_and(|judgement| judgement.outcome.is_none())
    }
    /// 这条消息是否已经判断过。
    pub fn judged(&self, message_id: &str) -> bool {
        self.judgements
            .iter()
            .any(|judgement| judgement.message_id == message_id)
    }
    /// 对这条消息的判断是否为适合附带。
    pub fn ask_for(&self, message_id: &str) -> bool {
        self.judgements.iter().any(|judgement| {
            judgement.message_id == message_id && judgement.outcome == Some(JudgeOutcome::Ask)
        })
    }
    /// 附带问题的那条消息；回答须是之后的另一轮。
    pub fn carried_by(&self, message_id: &str) -> bool {
        self.attempts
            .iter()
            .any(|attempt| attempt.message_id == message_id)
    }
}

/// 用户的请求澄清偏好；要求安静后不再附带任何问题，直到用户恢复。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerPreference {
    pub owner: String,
    pub quiet: bool,
    pub changed_at_ms: u64,
}

/// 宿主的投递策略。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClarifyPolicy {
    /// 同一用户两次送达问题之间的最短间隔。
    pub cooldown_ms: u64,
}
impl Default for ClarifyPolicy {
    fn default() -> Self {
        Self {
            cooldown_ms: 24 * 60 * 60 * 1000,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ClarifySnapshot {
    pub requests: Vec<Clarification>,
    pub preferences: Vec<OwnerPreference>,
}
impl ClarifySnapshot {
    pub fn quiet(&self, owner: &str) -> bool {
        self.preferences
            .iter()
            .any(|preference| preference.owner == owner && preference.quiet)
    }
    pub fn for_owner<'a>(&'a self, owner: &'a str) -> impl Iterator<Item = &'a Clarification> {
        self.requests
            .iter()
            .filter(move |request| request.owner == owner)
    }
    pub fn for_goal(&self, goal_id: &str, revision: u64) -> Option<&Clarification> {
        self.requests
            .iter()
            .find(|request| request.goal_id == goal_id && request.goal_revision == revision)
    }

    /// 某个用户此刻可以附带的问题：没有要求安静、没有正在判断或发送的问题、
    /// 距上次送达已过冷却期；取等待最久的一条。
    pub fn due_for(
        &self,
        owner: &str,
        now_ms: u64,
        policy: &ClarifyPolicy,
    ) -> Option<&Clarification> {
        if self.quiet(owner) {
            return None;
        }
        let owned: Vec<&Clarification> = self
            .requests
            .iter()
            .filter(|request| request.owner == owner)
            .collect();
        let busy = owned
            .iter()
            .any(|request| request.status == ClarifyStatus::Delivering || request.judging());
        let recent = owned
            .iter()
            .filter_map(|request| request.delivered_at_ms)
            .max();
        if busy || recent.is_some_and(|at| now_ms.saturating_sub(at) < policy.cooldown_ms) {
            return None;
        }
        owned
            .into_iter()
            .filter(|request| request.status == ClarifyStatus::Pending)
            .min_by_key(|request| (request.composed_at_ms, request.id.clone()))
    }

    /// 此刻可以为用户这条私聊消息做时机判断的问题：可投递、这条消息没有判断过，且判断次数未用尽。
    pub fn judgeable_for(
        &self,
        owner: &str,
        message_id: &str,
        now_ms: u64,
        policy: &ClarifyPolicy,
    ) -> Option<&Clarification> {
        self.due_for(owner, now_ms, policy).filter(|request| {
            request.judgements.len() < MAX_JUDGEMENTS && !request.judged(message_id)
        })
    }

    /// 已送达、还在等用户下一轮私聊的问题。
    pub fn listening(&self) -> impl Iterator<Item = &Clarification> {
        self.requests
            .iter()
            .filter(|request| request.status == ClarifyStatus::Delivered)
    }
}

/// 撰写请求：只含宿主整理的事实；规则与领域无关。
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ComposeRequest {
    pub request_id: String,
    pub composer_version: String,
    pub stall: StallKind,
    pub facts: Vec<Fact>,
}

/// 可替换撰写器；至多一次模型请求、零工具，返回问题正文。
pub trait ClarificationComposer: Send + Sync {
    fn version(&self) -> &str;
    fn compose(&self, request: ComposeRequest) -> ClarifyFuture<'_, String>;
}

/// 仅可信宿主持有；不发布给模型或不受信插件。
pub trait ClarifyAdmin: Send + Sync {
    fn snapshot(&self) -> ClarifyResult<ClarifySnapshot>;
    /// 原子保存 Composing 请求，成功返回后才可请求撰写。同一目标修订已有请求时返回 None。
    #[allow(clippy::too_many_arguments)]
    fn begin(
        &self,
        owner: &str,
        goal_id: &str,
        goal_revision: u64,
        stall: Stall,
        facts: Vec<Fact>,
        composer_version: &str,
        now_ms: u64,
    ) -> ClarifyResult<Option<Clarification>>;
    /// 保存撰写结果：成功进入 Pending，失败记为 Failed，不重试。
    fn record_composition(
        &self,
        id: &str,
        at_ms: u64,
        result: Result<String, ClarifyFailure>,
    ) -> ClarifyResult<Clarification>;
    /// 保存一次针对某条私聊消息的时机判断，返回后才可请求判断器；同一消息只判断一次。
    fn begin_judgement(
        &self,
        id: &str,
        message_id: &str,
        at_ms: u64,
    ) -> ClarifyResult<Clarification>;
    /// 保存判断结论（true 为适合附带）或失败；失败与中断都不重试同一条消息。
    fn record_judgement(
        &self,
        id: &str,
        message_id: &str,
        at_ms: u64,
        outcome: Result<bool, ClarifyFailure>,
    ) -> ClarifyResult<Clarification>;
    /// 保存一次投递尝试并进入 Delivering；返回后才可发送。须先有对这条消息“适合附带”的判断。
    fn claim(&self, id: &str, message_id: &str, at_ms: u64) -> ClarifyResult<Clarification>;
    /// 保存投递结果：送达进入 Delivered；没有写出或平台拒绝时回到 Pending，尝试用尽记为失败。
    fn record_delivery(
        &self,
        id: &str,
        at_ms: u64,
        result: DeliveryResult,
    ) -> ClarifyResult<Clarification>;
    /// 送达后的下一轮私聊记为回答：须在送达之后、回答窗口之内，且不是附带问题的那条消息。
    fn record_answer(&self, id: &str, answer: Answer) -> ClarifyResult<Clarification>;
    /// 回答窗口已过仍没有私聊：记为 Expired。
    fn expire(&self, id: &str, at_ms: u64) -> ClarifyResult<Clarification>;
    /// 取消尚未送达的问题；正在发送的问题不能取消。
    fn cancel(&self, id: &str, at_ms: u64, reason: CancelReason) -> ClarifyResult<Clarification>;
    fn set_quiet(&self, owner: &str, quiet: bool, at_ms: u64) -> ClarifyResult<OwnerPreference>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClarifyError {
    InvalidInput,
    NotFound,
    InvalidTransition,
    /// 账本容量已满；保留原记录，不淘汰。
    LimitReached,
    Unavailable,
    CorruptState,
    UnsupportedVersion,
    /// 提交无法确认；实例已关闭，须重新打开核对。
    Storage,
    /// 撰写或判断失败的类别。
    Clarify(ClarifyFailure),
}
impl fmt::Display for ClarifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "请求澄清输入无效",
            Self::NotFound => "没有这条请求澄清",
            Self::InvalidTransition => "请求澄清状态不允许此操作",
            Self::LimitReached => "请求澄清记录已达上限",
            Self::Unavailable => "请求澄清不可用",
            Self::CorruptState => "请求澄清记录损坏；未清空",
            Self::UnsupportedVersion => "不支持该请求澄清记录版本",
            Self::Storage => "请求澄清提交无法确认；须重新打开核对",
            Self::Clarify(_) => "请求澄清的模型请求失败",
        })
    }
}
impl std::error::Error for ClarifyError {}

pub fn validate_id(value: &str) -> ClarifyResult<()> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(ClarifyError::InvalidInput);
    }
    Ok(())
}

fn valid_text(value: &str, limit: usize) -> bool {
    !value.trim().is_empty()
        && value.trim() == value
        && value.len() <= limit
        && !value.chars().any(|c| c.is_control() && c != '\n')
}

/// 问题正文：非空、首尾无空白、至多 1024 字节、除换行外不含控制字符，并且包含问号。
pub fn validate_question(value: &str) -> ClarifyResult<()> {
    if !valid_text(value, MAX_QUESTION_BYTES) || !value.contains(['？', '?']) {
        return Err(ClarifyError::InvalidInput);
    }
    Ok(())
}

/// 回答引用：用户原话的前缀，非空、首尾无空白、至多 512 字节。
pub fn validate_answer_text(value: &str) -> ClarifyResult<()> {
    if !valid_text(value, MAX_ANSWER_BYTES) {
        return Err(ClarifyError::InvalidInput);
    }
    Ok(())
}

/// 事实：1 至 12 条，至少一条用户原话与一条尝试，每条非空且至多 512 字节。
pub fn validate_facts(facts: &[Fact]) -> ClarifyResult<()> {
    if facts.is_empty()
        || facts.len() > MAX_FACTS
        || !facts.iter().any(|fact| fact.kind == FactKind::UserQuote)
        || !facts.iter().any(|fact| fact.kind == FactKind::Attempt)
        || facts
            .iter()
            .any(|fact| !valid_text(&fact.text, MAX_FACT_BYTES))
    {
        return Err(ClarifyError::InvalidInput);
    }
    Ok(())
}

/// 文本按字符边界截取至多 `limit` 字节的前缀，去掉首尾空白。
pub fn prefix(text: &str, limit: usize) -> &str {
    let text = text.trim();
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].trim_end()
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

/// 同一目标修订只请求一次；重复准入得到同一 ID。
pub fn clarification_id(goal_id: &str, goal_revision: u64) -> String {
    format!(
        "clarify-{}",
        digest(&["goal.clarification:v1", goal_id, &goal_revision.to_string()])
    )
}

macro_rules! redacted { ($($ty:ty),+ $(,)?) => { $(impl fmt::Debug for $ty { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(concat!(stringify!($ty), "(<redacted>)")) } })+ }; }
redacted!(Fact, Answer, Clarification, ClarifySnapshot, ComposeRequest);

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: &str, owner: &str, status: ClarifyStatus) -> Clarification {
        Clarification {
            id: id.into(),
            owner: owner.into(),
            goal_id: format!("goal-{id}"),
            goal_revision: 1,
            stall: Stall {
                kind: StallKind::PracticeUnverified,
                run_id: "run".into(),
            },
            facts: vec![],
            composer_version: "v".into(),
            created_at_ms: 1,
            composed_at_ms: Some(2),
            question: Some("你想要什么？".into()),
            status,
            judgements: vec![],
            attempts: vec![],
            delivered_at_ms: None,
            answer: None,
            closed_at_ms: None,
        }
    }

    #[test]
    fn questions_facts_and_ids_are_bounded_and_stable() {
        assert!(validate_question("你想让方块有什么效果？").is_ok());
        assert!(validate_question("Which block?").is_ok());
        for bad in [
            "",
            " 你好？",
            "没有问号。",
            &format!("{}？", "长".repeat(400)),
        ] {
            assert!(validate_question(bad).is_err(), "{bad}");
        }
        let quote = Fact {
            kind: FactKind::UserQuote,
            text: "我喜欢模组".into(),
        };
        let attempt = Fact {
            kind: FactKind::Attempt,
            text: "实际运行三次都没有通过".into(),
        };
        assert!(validate_facts(&[quote.clone(), attempt.clone()]).is_ok());
        assert!(validate_facts(std::slice::from_ref(&quote)).is_err());
        assert!(validate_facts(&[attempt]).is_err());
        assert_eq!(clarification_id("g", 1), clarification_id("g", 1));
        assert_ne!(clarification_id("g", 1), clarification_id("g", 2));
        assert_eq!(prefix(" 你好世界 ", 7), "你好");
    }

    #[test]
    fn due_respects_quiet_cooldown_and_busy_requests() {
        let policy = ClarifyPolicy { cooldown_ms: 100 };
        let mut snapshot = ClarifySnapshot {
            requests: vec![request("a", "u", ClarifyStatus::Pending)],
            preferences: vec![],
        };
        assert_eq!(snapshot.due_for("u", 10, &policy).unwrap().id, "a");
        assert!(snapshot.due_for("other", 10, &policy).is_none());
        let mut delivered = request("b", "u", ClarifyStatus::Delivered);
        delivered.delivered_at_ms = Some(50);
        snapshot.requests.push(delivered);
        assert!(snapshot.due_for("u", 100, &policy).is_none(), "冷却期内");
        assert!(snapshot.due_for("u", 150, &policy).is_some());
        snapshot.requests[0].judgements.push(Judgement {
            message_id: "m1".into(),
            started_at_ms: 150,
            finished_at_ms: Some(151),
            outcome: Some(JudgeOutcome::NotNow),
        });
        assert!(
            snapshot.judgeable_for("u", "m1", 160, &policy).is_none(),
            "同一消息只判断一次"
        );
        assert!(snapshot.judgeable_for("u", "m2", 160, &policy).is_some());
        snapshot.preferences.push(OwnerPreference {
            owner: "u".into(),
            quiet: true,
            changed_at_ms: 1,
        });
        assert!(snapshot.due_for("u", 200, &policy).is_none(), "要求安静");
    }
}
