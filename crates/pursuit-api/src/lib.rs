//! 长期学习目标的推进判断：接下来继续、请求澄清、等待还是停止。
//!
//! 宿主独立读取研究、实践、请求澄清、主动交流与兴趣记录，整理成 `PursuitFacts`；`PursuitPolicy`
//! 只按这些事实给出判断，不调用模型，不读写状态。停止由组合层执行：一次认知状态替换把仍在等待的目标
//! 标为已取消，并追加一条停止事件，事件摘要是 `StopRecord`。
use serde::{Deserialize, Serialize};

/// 默认规则的版本；写入停止事件，便于之后核对是哪套规则做的判断。
pub const POLICY_VERSION: &str = "pursuit-rules:v1";
/// 停止事件的来源通道。
pub const PURSUIT_CHANNEL: &str = "eve.pursuit";
pub const STOP_EVENT_SCHEMA: &str = "pursuit-stop:v1";
/// 停止事件 ID 的前缀；同一目标的同一版本只会有一条。
pub const STOP_EVENT_PREFIX: &str = "pursuit-stop-";

/// 某项能力在目标当前版本上的结局。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Running,
    /// 研究找到可逐字引用的资料；实践在运行环境中验证通过。
    Succeeded,
    /// 研究没有找到可引用的资料；实践尝试用尽仍未验证。
    Unsuccessful,
    /// 现有运行环境做不了这个目标（只用于实践）。
    NotApplicable,
    /// 模型、超时或服务停止等原因失败；同一版本不重试。
    Failed,
    /// 执行中进程退出；不重放。
    Interrupted,
}

/// 研究或实践对这个目标的事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Capability {
    /// 这个目标所有版本已经用掉的次数。
    pub runs_used: usize,
    /// 每个目标至多的次数；用完后任何版本都不再开始。
    pub max_runs: usize,
    /// 账本容量已满，不能再开始新的一次。
    pub ledger_full: bool,
    /// 当前版本的结局；还没开始时为空。
    pub current: Option<Outcome>,
    /// 任一版本曾经成功过。
    pub succeeded_ever: bool,
    /// 自主运行预算此刻够开始下一次；未启用预算时为真。
    pub admitted: bool,
}
impl Capability {
    /// 这个目标的次数已经用完。
    pub fn spent(&self) -> bool {
        self.runs_used >= self.max_runs
    }
}

/// 请求澄清或主动交流的进展。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Exchange {
    /// 未开启。
    Disabled,
    /// 已开启，还没有开始。
    NotStarted,
    /// 正在撰写、等待时机或发送。
    Preparing,
    /// 已送达，在回应窗口内等用户回复。
    AwaitingReply,
    /// 已结束：用户已回复、窗口已过、取消、失败或送达与否不确定。
    Closed,
}

/// 一个仍在等待的学习目标的事实；由可信宿主从各账本读取，不来自模型。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PursuitFacts {
    /// 未开启研究时为空。
    pub research: Option<Capability>,
    /// 未开启实践时为空。
    pub practice: Option<Capability>,
    /// 当前版本的请求澄清。
    pub clarify: Exchange,
    /// 这个目标的主动交流邀请；每个目标至多一次，不分版本。
    pub outreach: Exchange,
    /// 用户回应邀请时提出、仍在进行的后续创作目标数。
    pub open_follow_ups: usize,
    /// 兴趣还能记下用户新的陈述，从而产生新的目标版本。目标尚未同步到兴趣最新版本时也为真。
    pub room_for_input: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Research,
    Practice,
    /// 告诉用户学到的东西、邀请用户一起做。
    Invite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    /// 自主运行预算此刻不够开始下一步；窗口重置后继续。
    AutonomyBudget,
    /// 已经问了用户或发了邀请，在等用户回复。
    UserReply,
    /// 用户提出的后续创作还在进行。
    FollowUp,
    /// 研究或实践记录已满，需要操作者处理。
    Capacity,
    /// 当前版本能做的都做完了，等用户提到新的想法或情况。
    NewInput,
    /// 没有开启研究或实践。
    NoCapability,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// 研究与实践的次数都已用完，即学习目标的停止条件“学习预算耗尽”。
    LearningBudgetSpent,
    /// 兴趣已记不下新的陈述，剩下的次数再也用不上。
    InputClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    /// 后台会继续推进；`started` 表示这一步已经在进行。
    Continue {
        step: Step,
        started: bool,
    },
    /// 当前版本卡住了，正准备或正在问用户。
    Clarify,
    Wait {
        reason: WaitReason,
    },
    /// 再也没有可以做的事。
    Stop {
        reason: StopReason,
    },
}

/// 可替换的推进判断；实现必须是确定性的，只读 `facts`。
pub trait PursuitPolicy: Send + Sync {
    /// 写入停止事件的规则版本。
    fn version(&self) -> &str;
    fn evaluate(&self, facts: &PursuitFacts) -> Verdict;
}

/// 停止事件的摘要；不含用户原话。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopRecord {
    pub schema: String,
    pub reason: StopReason,
    /// 停止时目标的版本。
    pub goal_revision: u64,
    pub research_runs: usize,
    pub practice_runs: usize,
    pub policy_version: String,
    /// 总是 false：停止来自规则判断，不是用户指令。
    pub user_instruction: bool,
}
