//! 实践策略的独立版本、选择与效果证据；不产生新的权限或执行次数。
use crate::*;

pub const STRATEGY_PLUGIN_ID: &str = "eve.practice-strategy";
pub const STRATEGY_STATE_KEY: &str = "strategies.v1";
pub const MAX_STRATEGY_EVENTS: usize = 256;
pub const MAX_STRATEGY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PracticeGuidance {
    pub version: u64,
    pub source_run: String,
    pub source_sha256: String,
    pub previous_issues: Vec<String>,
    pub corrected_example: PracticeDraft,
}

impl PracticeGuidance {
    pub fn validate(&self, runner: &RunnerProfile) -> PracticeResult<()> {
        validate_id(&self.source_run)?;
        if self.version == 0
            || self.source_sha256.len() != 64
            || !self.source_sha256.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(PracticeError::InvalidInput);
        }
        validate_issues(&self.previous_issues)?;
        validate_artifact(runner, &self.corrected_example)
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategyCandidate {
    pub version: u64,
    pub parent: Option<u64>,
    /// 只从实际修正并验证通过的实践提炼；保留完整来源，恢复时重新校验。
    pub source: PracticeRun,
    /// 以尝试次数衡量的冻结比较基线，不因重启或后来的失败放宽。
    pub attempt_ceiling: usize,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategyUse {
    pub run_id: String,
    pub owner: String,
    pub goal_id: String,
    pub goal_revision: u64,
    pub brief_sha256: String,
    pub scope_sha256: String,
    pub version: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum StrategyEffect {
    Verified { attempts: usize },
    Failed,
    Inconclusive,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum StrategyEvent {
    Proposed(Box<StrategyCandidate>),
    Selected(StrategyUse),
    Observed {
        run_id: String,
        evidence_sha256: String,
        effect: StrategyEffect,
    },
}

#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategySnapshot {
    pub events: Vec<StrategyEvent>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum StrategyStatus {
    Candidate,
    Active,
    Retired,
    Rejected,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct StrategyView {
    pub candidate: StrategyCandidate,
    pub status: StrategyStatus,
    pub attempt_ceiling: usize,
    pub verified_goals: BTreeSet<String>,
    pub verified_briefs: BTreeSet<String>,
}

/// 只有可信实践执行器持有管理接口；外部消息不能直接提交“验证成功”。
pub trait PracticeStrategy: Send + Sync {
    fn snapshot(&self) -> PracticeResult<StrategySnapshot>;
    fn select(&self, run: &PracticeRun) -> PracticeResult<Option<PracticeGuidance>>;
    /// 对账只读取既有实践结果，不重新请求模型或运行产物。
    fn reconcile(&self, practice: &PracticeSnapshot) -> PracticeResult<()>;
}

pub fn strategy_digest(value: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(value).expect("strategy serializable data");
    let mut context = Context::new(&SHA256);
    context.update(&bytes);
    context
        .finish()
        .as_ref()
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect()
}

pub fn strategy_scope(run: &PracticeRun) -> String {
    strategy_digest(&(&run.task.owner, &run.runner, &run.drafter_version))
}

/// 去掉空白、大小写差异，防止换目标 ID 重复拿原题给自己验证。
pub fn strategy_brief(brief: &str) -> String {
    strategy_digest(&brief.split_whitespace().collect::<String>().to_lowercase())
}
