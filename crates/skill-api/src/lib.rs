//! 技能的来源契约：从 Eve 自己已验证的实践中提炼、可带参数复用的方法。
//!
//! 技能是参数化的产物模板。用原参数实例化必须逐字还原已验证的实践草稿（忠实性）；
//! 再由宿主为每个参数选取与原值不同的值实例化，并在同一运行器中实际运行通过（泛化验证），
//! 才成为可启用的版本。实例化只做受限的文本替换：参数值只能是标识、整数或固定选项，
//! 不执行任何代码；实例仍须通过实践草稿的通用检查与运行器的结构检查。
//! 技能只在来源用户的后续任务中复用；账本、提炼器与选择器都可替换。
use eve_practice_api::{
    MAX_FILES, MAX_ISSUE_BYTES, MAX_ISSUES, MAX_PROBES, PracticeDraft, PracticeTask, Probe,
    RunEvidence, RunnerProfile, validate_artifact,
};
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    future::Future,
    pin::Pin,
};

pub const SKILL_PLUGIN_ID: &str = "eve.skill";
pub const SKILL_STATE_KEY: &str = "skill.v1";
/// 技能总数，不自动淘汰；达到容量后保留原记录并停止新的提炼。
pub const MAX_SKILLS: usize = 32;
pub const MAX_VERSIONS: usize = 8;
/// 每个技能的启用变更记录上限；满后拒绝新的手动变更。
pub const MAX_CHANGES: usize = 64;
pub const MAX_DISTILLATIONS: usize = 32;
pub const MAX_SELECTIONS: usize = 64;
/// 对话中直接调用技能工具的记录总数，不自动淘汰；满后拒绝新的调用。
pub const MAX_TOOL_CALLS: usize = 256;
pub const MAX_PARAMETERS: usize = 8;
pub const MAX_OPTIONS: usize = 8;
/// 交给选择器的候选技能上限。
pub const MAX_CANDIDATES: usize = 8;
pub const MAX_NAME_BYTES: usize = 32;
pub const MAX_TITLE_BYTES: usize = 128;
pub const MAX_SUMMARY_BYTES: usize = 512;
pub const MAX_DESCRIPTION_BYTES: usize = 256;
pub const MAX_REASON_BYTES: usize = 512;
pub const MAX_PROPOSAL_OUTPUT_BYTES: usize = 48 * 1024;
pub const MAX_CHOICE_OUTPUT_BYTES: usize = 4 * 1024;
pub const MAX_STATE_BYTES: usize = 8 * 1024 * 1024;
/// 宿主为技能实例写入的说明前缀；实例不引用任务资料。
pub const INSTANCE_RATIONALE: &str = "由已验证技能的模板实例化";

pub type SkillResult<T> = Result<T, SkillError>;
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type SkillFuture<'a, T> = BoxFuture<'a, SkillResult<T>>;
/// 参数名到参数值；值都是受限文本，不含引号、空白或括号。
pub type Arguments = BTreeMap<String, String>;

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ParameterKind {
    /// 小写字母开头，只含小写字母、数字与 `-`，不以 `-` 结尾，至多 32 字节。
    Identifier,
    /// 规范十进制整数，min < max，宿主用边界值验证。
    Integer { min: i64, max: i64 },
    /// 2 到 8 个固定选项，每项只含 `[A-Za-z0-9._-]`，至多 32 字节。
    Choice { options: Vec<String> },
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillParameter {
    /// 小写字母开头，只含小写字母、数字与 `_`，至多 32 字节。
    pub name: String,
    pub description: String,
    pub kind: ParameterKind,
}

/// 参数化的产物模板；文件路径、文件内容、探测对象与期望值中可以出现 `{{参数名}}`。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillTemplate {
    pub title: String,
    pub summary: String,
    pub parameters: Vec<SkillParameter>,
    pub files: Vec<eve_practice_api::ArtifactFile>,
    pub probes: Vec<Probe>,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillRef {
    pub skill_id: String,
    pub version: u32,
}

/// 技能来源：某次实践中已验证的那次尝试。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillOrigin {
    pub practice_run_id: String,
    pub attempt: usize,
    pub goal_id: String,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillProposal {
    /// 作为已有技能的新版本时给出该技能 ID；否则为新技能。
    pub extends: Option<String>,
    /// 技能名称：小写字母开头，只含小写字母、数字与 `-`，至多 32 字节；同一用户与运行器下唯一。
    pub name: String,
    pub template: SkillTemplate,
    /// 还原来源草稿的原参数。
    pub arguments: Arguments,
}

/// 提炼器的一次输出。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Proposal {
    /// 没有值得固化的通用方法，或已被已有技能覆盖。
    NotReusable {
        reason: String,
    },
    Skill(SkillProposal),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SkillFailure {
    Provider,
    InvalidOutput,
    Timeout,
    Cancelled,
    /// 账本容量已满。
    LimitReached,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DistillStage {
    /// 已保存准入，正在请求提炼。
    Proposing,
    /// 已保存通过检查的模板与宿主选取的验证参数，正在实际运行。
    Verifying,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DistillStatus {
    Running,
    NotReusable,
    /// 模板不合规或不能逐字还原来源草稿，没有运行。
    Rejected,
    /// 验证实例实际运行通过，已成为技能版本。
    Verified,
    /// 验证实例实际运行了，但证据不足以验证；不形成技能。
    Unverified,
    Failed(SkillFailure),
    /// 进程在 Running 时退出；重启后记为中断，不重放。
    Interrupted,
}

/// 一次提炼的完整记录；每一步先保存再执行对应外部动作。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Distillation {
    pub id: String,
    /// 来源实践可见的用户；技能只属于该用户。
    pub owner: String,
    pub origin: SkillOrigin,
    /// 来源草稿的副本，用于复核忠实性。
    pub source: PracticeDraft,
    pub runner: RunnerProfile,
    pub distiller_version: String,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub stage: DistillStage,
    pub status: DistillStatus,
    pub proposal: Option<Proposal>,
    pub issues: Vec<String>,
    /// 宿主选取的验证参数；验证实例由模板确定性生成。
    pub holdout: Option<Arguments>,
    pub evidence: Option<RunEvidence>,
    pub skill: Option<SkillRef>,
}
impl Distillation {
    pub fn template(&self) -> Option<&SkillProposal> {
        match &self.proposal {
            Some(Proposal::Skill(proposal)) => Some(proposal),
            _ => None,
        }
    }
    /// 验证实例：模板用宿主选取的参数实例化。
    pub fn holdout_draft(&self) -> Option<PracticeDraft> {
        let proposal = self.template()?;
        instantiate(&proposal.template, self.holdout.as_ref()?).ok()
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillVersion {
    pub version: u32,
    /// 生成并验证这个版本的提炼记录；模板、验证参数与证据都在其中。
    pub distillation_id: String,
    pub verified_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Actor {
    /// 新版本验证通过后由宿主自动启用。
    Automatic,
    /// 技能所属用户的停用、启用或回退。
    Owner,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnablementChange {
    pub at_ms: u64,
    pub actor: Actor,
    /// 变更后的启用版本；None 表示停用。
    pub enabled: Option<u32>,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Skill {
    pub id: String,
    pub owner: String,
    pub runner_id: String,
    pub name: String,
    /// 只包含已验证的版本，按版本号递增；版本不可修改。
    pub versions: Vec<SkillVersion>,
    pub enabled: Option<u32>,
    pub changes: Vec<EnablementChange>,
}
impl Skill {
    /// 用户最近一次停用后，新版本不再自动启用，直到用户重新启用。
    pub fn held_by_owner(&self) -> bool {
        self.changes
            .last()
            .is_some_and(|change| change.actor == Actor::Owner && change.enabled.is_none())
    }
    pub fn latest(&self) -> Option<&SkillVersion> {
        self.versions.last()
    }
    pub fn version(&self, version: u32) -> Option<&SkillVersion> {
        self.versions.iter().find(|entry| entry.version == version)
    }
}

/// 选择器选定的技能与参数。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub skill: SkillRef,
    pub arguments: Arguments,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SelectionStatus {
    Running,
    /// 选择器认为没有适用的技能；任务照常草稿。
    Declined,
    /// 选定的技能或参数不合规；任务照常草稿。
    Rejected,
    Failed(SkillFailure),
    /// 已选定并实例化；结果以实践账本中第一次尝试的证据为准。
    Chosen,
    Interrupted,
}

/// 技能调用的结果，取自实践账本中使用该实例的那次尝试。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum InvocationOutcome {
    Verified,
    /// 实际运行了，但证据不足以验证。
    Failed,
    /// 结构检查不通过，没有运行。
    Rejected,
    /// 因停止或超时放弃，结果未知。
    Abandoned,
    /// 进程退出前没有得到结果，不重放。
    Interrupted,
}

/// 一个后续任务的技能选择与调用；ID 即该任务的实践 ID。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub id: String,
    pub owner: String,
    pub goal_id: String,
    pub candidates: Vec<SkillRef>,
    pub selector_version: String,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub status: SelectionStatus,
    pub choice: Option<Choice>,
    pub issues: Vec<String>,
    pub outcome: Option<InvocationOutcome>,
    pub settled_at_ms: Option<u64>,
}

/// 对话中直接调用一个已启用技能的记录：先保存再运行，结果以实际运行证据为准。
/// 只能调用当前对话用户自己的技能；outcome 为 None 表示仍在运行，Interrupted 表示进程退出前没有结果。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallRecord {
    pub id: String,
    pub owner: String,
    pub skill: SkillRef,
    pub arguments: Arguments,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub outcome: Option<InvocationOutcome>,
    pub evidence: Option<RunEvidence>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SkillSummary {
    pub skill: SkillRef,
    pub name: String,
    pub title: String,
    pub summary: String,
    pub parameters: Vec<SkillParameter>,
}
impl Serialize for SkillSummary {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("SkillSummary", 6)?;
        state.serialize_field("skill_id", &self.skill.skill_id)?;
        state.serialize_field("version", &self.skill.version)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("title", &self.title)?;
        state.serialize_field("summary", &self.summary)?;
        state.serialize_field("parameters", &self.parameters)?;
        state.end()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct SkillSnapshot {
    pub skills: Vec<Skill>,
    pub distillations: Vec<Distillation>,
    pub selections: Vec<Selection>,
    pub tool_calls: Vec<ToolCallRecord>,
}
impl SkillSnapshot {
    pub fn skills_for<'a>(&'a self, owner: &'a str) -> impl Iterator<Item = &'a Skill> {
        self.skills.iter().filter(move |skill| skill.owner == owner)
    }
    pub fn skill(&self, skill_id: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.id == skill_id)
    }
    pub fn distillation(&self, id: &str) -> Option<&Distillation> {
        self.distillations.iter().find(|entry| entry.id == id)
    }
    pub fn selection(&self, id: &str) -> Option<&Selection> {
        self.selections.iter().find(|entry| entry.id == id)
    }
    /// 某个技能版本的提炼记录（模板、验证参数与证据）。
    pub fn source(&self, skill: &SkillRef) -> Option<&Distillation> {
        let version = self.skill(&skill.skill_id)?.version(skill.version)?;
        self.distillation(&version.distillation_id)
    }
    pub fn summary(&self, skill: &SkillRef) -> Option<SkillSummary> {
        let entry = self.skill(&skill.skill_id)?;
        let proposal = self.source(skill)?.template()?;
        Some(SkillSummary {
            skill: skill.clone(),
            name: entry.name.clone(),
            title: proposal.template.title.clone(),
            summary: proposal.template.summary.clone(),
            parameters: proposal.template.parameters.clone(),
        })
    }
    /// 某用户在某运行器下已启用的技能版本。
    pub fn enabled_for(&self, owner: &str, runner_id: &str) -> Vec<SkillRef> {
        self.skills_for(owner)
            .filter(|skill| skill.runner_id == runner_id)
            .filter_map(|skill| {
                skill.enabled.map(|version| SkillRef {
                    skill_id: skill.id.clone(),
                    version,
                })
            })
            .collect()
    }
    /// 提案与已有技能的关系：新技能不能与已有技能同名；新版本须指向同一用户与运行器下的
    /// 同名技能，且版本未满。复用已有技能时应作为新版本，而不是另造一个。
    pub fn lineage_issues(
        &self,
        owner: &str,
        runner_id: &str,
        proposal: &SkillProposal,
    ) -> Vec<String> {
        match &proposal.extends {
            None if self
                .skill(&skill_id(owner, runner_id, &proposal.name))
                .is_some() =>
            {
                vec!["已有同名技能；应作为该技能的新版本".into()]
            }
            None if self.skills.len() >= MAX_SKILLS => vec!["技能数量已满".into()],
            None => vec![],
            Some(id) => match self.skill(id) {
                Some(skill)
                    if skill.owner == owner
                        && skill.runner_id == runner_id
                        && skill.name == proposal.name =>
                {
                    if skill.versions.len() >= MAX_VERSIONS {
                        vec!["该技能的版本已满".into()]
                    } else {
                        vec![]
                    }
                }
                _ => vec!["extends 须是同一用户与运行器下的同名技能".into()],
            },
        }
    }
    /// 使用某个技能版本的调用。
    pub fn invocations<'a>(&'a self, skill_id: &'a str) -> impl Iterator<Item = &'a Selection> {
        self.selections.iter().filter(move |selection| {
            selection.status == SelectionStatus::Chosen
                && selection
                    .choice
                    .as_ref()
                    .is_some_and(|choice| choice.skill.skill_id == skill_id)
        })
    }
}

/// 提炼请求：只含已验证的产物、实际证据与已有技能，不含用户原话或学习目标描述。
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct DistillRequest {
    pub distillation_id: String,
    pub distiller_version: String,
    pub runner: RunnerProfile,
    pub source: PracticeDraft,
    pub evidence: RunEvidence,
    pub existing: Vec<SkillSummary>,
}

/// 可替换提炼器；至多一次模型请求、零工具，输出须是结构完整的提案。
pub trait SkillDistiller: Send + Sync {
    fn version(&self) -> &str;
    fn distill(&self, request: DistillRequest) -> SkillFuture<'_, Proposal>;
}

/// 选择请求：后续任务与该用户已启用的技能。
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct SelectRequest {
    pub selection_id: String,
    pub selector_version: String,
    pub task: PracticeTask,
    pub runner: RunnerProfile,
    pub candidates: Vec<SkillSummary>,
}

/// 可替换选择器；至多一次模型请求、零工具。None 表示没有适用的技能。
pub trait SkillSelector: Send + Sync {
    fn version(&self) -> &str;
    fn select(&self, request: SelectRequest) -> SkillFuture<'_, Option<Choice>>;
}

/// 仅可信宿主持有；不发布给模型或不受信插件。
pub trait SkillAdmin: Send + Sync {
    fn snapshot(&self) -> SkillResult<SkillSnapshot>;
    /// 原子保存 Running 提炼（Proposing），成功返回后才可请求提炼。
    /// 同一次实践已提炼过时返回 None；提炼记录或技能已满时返回 LimitReached。
    fn begin_distillation(
        &self,
        owner: &str,
        origin: SkillOrigin,
        source: PracticeDraft,
        runner: &RunnerProfile,
        distiller_version: &str,
        now_ms: u64,
    ) -> SkillResult<Option<Distillation>>;
    /// 保存提炼结果与宿主的检查。没有问题的技能提案须同时给出宿主选取的验证参数，
    /// 进入 Verifying，返回后才可实际运行；其余情况在同一次提交中写入结局。
    fn record_proposal(
        &self,
        id: &str,
        at_ms: u64,
        result: Result<Proposal, SkillFailure>,
        issues: Vec<String>,
        holdout: Option<Arguments>,
    ) -> SkillResult<Distillation>;
    /// 保存验证实例的实际证据；已验证时在同一次提交中写入技能版本，
    /// 除非用户停用了该技能，否则自动启用新版本。
    fn record_verification(
        &self,
        id: &str,
        at_ms: u64,
        evidence: RunEvidence,
    ) -> SkillResult<Distillation>;
    /// 因停止或超时放弃进行中的提炼；只能从 Running 结束一次。
    fn abandon_distillation(
        &self,
        id: &str,
        at_ms: u64,
        failure: SkillFailure,
    ) -> SkillResult<Distillation>;
    /// 用户停用（None）或启用某个已验证版本；记录变更，不删除任何版本。
    fn set_enabled(
        &self,
        skill_id: &str,
        at_ms: u64,
        actor: Actor,
        enabled: Option<u32>,
    ) -> SkillResult<Skill>;
    /// 原子保存 Running 选择，成功返回后才可请求选择器。候选必须是该用户当前启用的版本。
    /// 同一任务已选择过时返回 None。
    fn begin_selection(
        &self,
        id: &str,
        owner: &str,
        goal_id: &str,
        candidates: Vec<SkillRef>,
        selector_version: &str,
        now_ms: u64,
    ) -> SkillResult<Option<Selection>>;
    /// 保存选择结果；带问题的选定记为 Rejected 并保留原选择，任务照常草稿。
    fn record_selection(
        &self,
        id: &str,
        at_ms: u64,
        result: Result<Option<Choice>, SkillFailure>,
        issues: Vec<String>,
    ) -> SkillResult<Selection>;
    /// 依据实践账本写入调用结果；只能对 Chosen 的选择写一次。
    fn settle(&self, id: &str, at_ms: u64, outcome: InvocationOutcome) -> SkillResult<Selection>;
    /// 保存一次对话中的技能调用，返回后才可运行；技能必须属于该用户且该版本已启用，参数须合规。
    /// 同一时间只运行一次调用。
    fn begin_tool_call(
        &self,
        id: &str,
        owner: &str,
        skill: SkillRef,
        arguments: Arguments,
        now_ms: u64,
    ) -> SkillResult<ToolCallRecord>;
    /// 保存调用结果与运行证据；只能写一次。Verified 必须有满足验证条件的证据。
    fn record_tool_call(
        &self,
        id: &str,
        at_ms: u64,
        outcome: InvocationOutcome,
        evidence: Option<RunEvidence>,
    ) -> SkillResult<ToolCallRecord>;
}

/// 一次技能工具调用的 ID：由用户与对话中的调用 ID 确定。
pub fn tool_call_id(owner: &str, call_id: &str, started_at_ms: u64) -> String {
    let mut context = Context::new(&SHA256);
    for part in [
        "skill.tool-call:v1",
        owner,
        call_id,
        &started_at_ms.to_string(),
    ] {
        context.update(&(part.len() as u64).to_be_bytes());
        context.update(part.as_bytes());
    }
    let digest: String = context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("skill-call-{}", &digest[..32])
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SkillError {
    InvalidInput,
    NotFound,
    Unavailable,
    CorruptState,
    UnsupportedVersion,
    Storage,
    LimitReached,
    Conflict,
    Skill(SkillFailure),
}
impl fmt::Display for SkillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "技能输入无效",
            Self::NotFound => "没有这条技能记录",
            Self::Unavailable => "技能服务不可用",
            Self::CorruptState => "技能状态损坏；未清空",
            Self::UnsupportedVersion => "不支持该技能状态版本",
            Self::Storage => "技能提交无法确认；须重新打开核对",
            Self::LimitReached => "技能容量已满；保留原记录",
            Self::Conflict => "技能记录状态冲突",
            Self::Skill(_) => "技能请求失败",
        })
    }
}
impl std::error::Error for SkillError {}

fn digest(parts: &[&str]) -> String {
    let mut context = Context::new(&SHA256);
    for part in parts {
        context.update(&(part.len() as u64).to_be_bytes());
        context.update(part.as_bytes());
    }
    context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 同一次实践只提炼一次；重复准入得到同一 ID。
pub fn distillation_id(practice_run_id: &str) -> String {
    format!(
        "distill-{}",
        &digest(&["skill.distill:v1", practice_run_id])[..32]
    )
}

/// 同一用户、运行器与名称的技能是同一个技能。
pub fn skill_id(owner: &str, runner_id: &str, name: &str) -> String {
    format!(
        "skill-{}",
        &digest(&["skill.id:v1", owner, runner_id, name])[..24]
    )
}

fn is_lower_word(value: &str, extra: u8) -> bool {
    !value.is_empty()
        && value.len() <= MAX_NAME_BYTES
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == extra)
}

/// 参数名：小写字母开头，只含小写字母、数字与 `_`。
pub fn is_parameter_name(value: &str) -> bool {
    is_lower_word(value, b'_')
}

/// 技能名称与标识参数值：小写字母开头，只含小写字母、数字与 `-`，不以 `-` 结尾。
pub fn is_identifier(value: &str) -> bool {
    is_lower_word(value, b'-') && !value.ends_with('-')
}

fn is_option(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_NAME_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn is_text(value: &str, limit: usize) -> bool {
    !value.trim().is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}

/// 按出现顺序取出 `{{参数名}}`；不完整或名称不合规的 `{{` 返回 None。
fn placeholders(text: &str) -> Option<Vec<&str>> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let end = after.find("}}")?;
        let name = &after[..end];
        if !is_parameter_name(name) {
            return None;
        }
        found.push(name);
        rest = &after[end + 2..];
    }
    Some(found)
}

fn substitute(text: &str, arguments: &Arguments) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            output.push_str(&rest[start..]);
            return output;
        };
        match arguments.get(&after[..end]) {
            Some(value) => output.push_str(value),
            None => output.push_str(&rest[start..start + 4 + end]),
        }
        rest = &after[end + 2..];
    }
    output.push_str(rest);
    output
}

fn line(issue: String) -> String {
    let mut end = issue.len().min(MAX_ISSUE_BYTES);
    while !issue.is_char_boundary(end) {
        end -= 1;
    }
    issue[..end].replace(['\r', '\n'], " ")
}

/// 模板的结构问题；空表示可以继续核对忠实性。只检查形状，不实例化。
pub fn template_issues(template: &SkillTemplate, profile: &RunnerProfile) -> Vec<String> {
    let mut issues = Vec::new();
    if !is_text(&template.title, MAX_TITLE_BYTES) {
        issues.push(format!("标题须非空且不超过 {MAX_TITLE_BYTES} 字节"));
    }
    if !is_text(&template.summary, MAX_SUMMARY_BYTES) {
        issues.push(format!("说明须非空且不超过 {MAX_SUMMARY_BYTES} 字节"));
    }
    if template.parameters.is_empty() || template.parameters.len() > MAX_PARAMETERS {
        issues.push(format!("参数须有 1 到 {MAX_PARAMETERS} 个"));
    }
    let mut names = BTreeSet::new();
    for parameter in &template.parameters {
        let name = &parameter.name;
        if !is_parameter_name(name) || !names.insert(name.as_str()) {
            issues.push(line(format!("参数名 {name} 不合规或重复")));
        }
        if !is_text(&parameter.description, MAX_DESCRIPTION_BYTES) {
            issues.push(line(format!("参数 {name} 缺少说明或说明过长")));
        }
        match &parameter.kind {
            ParameterKind::Identifier => {}
            ParameterKind::Integer { min, max } if min < max => {}
            ParameterKind::Integer { .. } => {
                issues.push(line(format!("参数 {name} 的取值范围须满足 min < max")));
            }
            ParameterKind::Choice { options } => {
                let unique: BTreeSet<_> = options.iter().collect();
                if options.len() < 2
                    || options.len() > MAX_OPTIONS
                    || unique.len() != options.len()
                    || !options.iter().all(|option| is_option(option))
                {
                    issues.push(line(format!(
                        "参数 {name} 须有 2 到 {MAX_OPTIONS} 个不同的合规选项"
                    )));
                }
            }
        }
    }
    if template.files.is_empty() || template.files.len() > MAX_FILES {
        issues.push(format!("文件须有 1 到 {MAX_FILES} 个"));
    }
    if template.probes.is_empty() || template.probes.len() > MAX_PROBES {
        issues.push(format!("探测须有 1 到 {MAX_PROBES} 项"));
    }
    let mut unknown = BTreeSet::new();
    let mut malformed = false;
    let mut collect = |text: &str| -> Vec<String> {
        match placeholders(text) {
            Some(found) => found
                .into_iter()
                .inspect(|name| {
                    if !names.contains(name) {
                        unknown.insert(name.to_string());
                    }
                })
                .map(str::to_string)
                .collect(),
            None => {
                malformed = true;
                vec![]
            }
        }
    };
    for file in &template.files {
        collect(&file.path);
        collect(&file.content);
    }
    let mut probed = BTreeSet::new();
    for probe in &template.probes {
        probed.extend(collect(&probe.subject));
        probed.extend(collect(&probe.expected));
        if !profile.properties.contains(&probe.property) {
            issues.push(line(format!(
                "探测属性 {} 不在运行器声明的属性中",
                probe.property
            )));
        }
    }
    if malformed {
        issues.push("存在不完整或名称不合规的 {{ 占位符".into());
    }
    for name in unknown {
        issues.push(line(format!("占位符 {name} 没有对应的参数")));
    }
    // 每个参数都要被某项探测检查，验证实例才能说明参数确实生效。
    for parameter in &template.parameters {
        if !probed.contains(&parameter.name) {
            issues.push(line(format!(
                "参数 {} 没有出现在任何探测的对象或期望值中",
                parameter.name
            )));
        }
    }
    issues.truncate(MAX_ISSUES);
    issues
}

/// 参数值是否符合声明；键集合须与参数完全一致。
pub fn argument_issues(template: &SkillTemplate, arguments: &Arguments) -> Vec<String> {
    let mut issues = Vec::new();
    let declared: BTreeSet<_> = template
        .parameters
        .iter()
        .map(|parameter| parameter.name.as_str())
        .collect();
    for name in arguments.keys() {
        if !declared.contains(name.as_str()) {
            issues.push(line(format!("参数 {name} 未声明")));
        }
    }
    for parameter in &template.parameters {
        let name = &parameter.name;
        let Some(value) = arguments.get(name) else {
            issues.push(line(format!("缺少参数 {name}")));
            continue;
        };
        let valid = match &parameter.kind {
            ParameterKind::Identifier => is_identifier(value),
            ParameterKind::Integer { min, max } => value.parse::<i64>().is_ok_and(|number| {
                number.to_string() == *value && (*min..=*max).contains(&number)
            }),
            ParameterKind::Choice { options } => options.contains(value),
        };
        if !valid {
            issues.push(line(format!("参数 {name} 的值不符合声明")));
        }
    }
    issues.truncate(MAX_ISSUES);
    issues
}

/// 用参数实例化模板；参数不合规时返回问题。实例仍须经 `validate_artifact` 与运行器检查。
pub fn instantiate(
    template: &SkillTemplate,
    arguments: &Arguments,
) -> Result<PracticeDraft, Vec<String>> {
    let issues = argument_issues(template, arguments);
    if !issues.is_empty() {
        return Err(issues);
    }
    Ok(PracticeDraft {
        applicable: true,
        files: template
            .files
            .iter()
            .map(|file| eve_practice_api::ArtifactFile {
                path: substitute(&file.path, arguments),
                content: substitute(&file.content, arguments),
            })
            .collect(),
        probes: template
            .probes
            .iter()
            .map(|probe| Probe {
                subject: substitute(&probe.subject, arguments),
                property: probe.property.clone(),
                expected: substitute(&probe.expected, arguments),
            })
            .collect(),
        rationale: INSTANCE_RATIONALE.into(),
        notes_used: vec![],
    })
}

/// 两个草稿的产物是否相同：文件按路径、探测按内容比较，不看说明与资料引用。
pub fn same_artifact(left: &PracticeDraft, right: &PracticeDraft) -> bool {
    let files = |draft: &PracticeDraft| {
        let mut files: Vec<_> = draft
            .files
            .iter()
            .map(|file| (file.path.clone(), file.content.clone()))
            .collect();
        files.sort();
        files
    };
    let probes = |draft: &PracticeDraft| {
        let mut probes = draft.probes.clone();
        probes.sort();
        probes
    };
    left.applicable == right.applicable
        && files(left) == files(right)
        && probes(left) == probes(right)
}

/// 宿主为验证选取的参数：每个参数都取与原值不同的值。整数取边界值以检验声明的范围，
/// 标识追加后缀，选项取第一个不同的选项。结果确定，不由模型决定。
pub fn holdout_arguments(template: &SkillTemplate, original: &Arguments) -> Option<Arguments> {
    let mut holdout = Arguments::new();
    for parameter in &template.parameters {
        let value = original.get(&parameter.name)?;
        let chosen = match &parameter.kind {
            ParameterKind::Identifier => {
                if value.len() + 2 <= MAX_NAME_BYTES {
                    format!("{value}-b")
                } else {
                    let last = if value.ends_with('b') { 'c' } else { 'b' };
                    format!("{}{last}", &value[..value.len() - 1])
                }
            }
            ParameterKind::Integer { min, max } => {
                let number: i64 = value.parse().ok()?;
                (if number != *max { *max } else { *min }).to_string()
            }
            ParameterKind::Choice { options } => {
                options.iter().find(|option| *option != value)?.clone()
            }
        };
        holdout.insert(parameter.name.clone(), chosen);
    }
    Some(holdout)
}

/// 宿主对技能提案的完整检查：模板形状、原参数、逐字还原来源草稿、验证实例的通用形状。
/// 返回问题与验证参数；问题为空时验证参数一定存在。领域结构检查另由运行器负责。
pub fn proposal_issues(
    proposal: &SkillProposal,
    source: &PracticeDraft,
    profile: &RunnerProfile,
) -> (Vec<String>, Option<Arguments>) {
    let mut issues = Vec::new();
    if !is_identifier(&proposal.name) {
        issues.push("技能名称不合规".into());
    }
    issues.extend(template_issues(&proposal.template, profile));
    if !issues.is_empty() {
        issues.truncate(MAX_ISSUES);
        return (issues, None);
    }
    let instance = match instantiate(&proposal.template, &proposal.arguments) {
        Ok(instance) => instance,
        Err(found) => return (found, None),
    };
    if !same_artifact(&instance, source) {
        return (
            vec!["用原参数实例化后不能逐字还原已验证的草稿".into()],
            None,
        );
    }
    let Some(holdout) = holdout_arguments(&proposal.template, &proposal.arguments) else {
        return (vec!["无法为验证选取参数".into()], None);
    };
    match instantiate(&proposal.template, &holdout) {
        Ok(draft) if validate_artifact(profile, &draft).is_ok() => (vec![], Some(holdout)),
        _ => (vec!["验证实例不符合产物的通用限制".into()], None),
    }
}

/// 技能调用实例的说明：写明技能与版本，便于在实践记录中核对。
pub fn invocation_rationale(title: &str, skill: &SkillRef) -> String {
    let mut text = format!(
        "{INSTANCE_RATIONALE}：「{title}」第 {} 版（{}）",
        skill.version, skill.skill_id
    );
    let mut end = text.len().min(eve_practice_api::MAX_RATIONALE_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

pub fn validate_id(value: &str) -> SkillResult<()> {
    eve_practice_api::validate_id(value).map_err(|_| SkillError::InvalidInput)
}

pub fn validate_issues(issues: &[String]) -> SkillResult<()> {
    eve_practice_api::validate_issues(issues).map_err(|_| SkillError::InvalidInput)
}

pub fn validate_reason(value: &str) -> SkillResult<()> {
    if is_text(value, MAX_REASON_BYTES) {
        Ok(())
    } else {
        Err(SkillError::InvalidInput)
    }
}

macro_rules! redacted { ($($ty:ty),+ $(,)?) => { $(impl fmt::Debug for $ty { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(concat!(stringify!($ty), "(<redacted>)")) } })+ }; }
redacted!(
    ParameterKind,
    SkillParameter,
    SkillTemplate,
    SkillProposal,
    Proposal,
    Distillation,
    SkillVersion,
    Skill,
    Choice,
    Selection,
    SkillSummary,
    SkillSnapshot,
    DistillRequest,
    SelectRequest,
);

#[cfg(test)]
mod tests {
    use super::*;
    use eve_practice_api::ArtifactFile;

    fn profile() -> RunnerProfile {
        RunnerProfile {
            runner_id: "test-runner:v1".into(),
            runtime: "sha256:0".into(),
            domain: "测试".into(),
            layout: "任意".into(),
            properties: vec!["exists".into(), "health".into(), "class".into()],
        }
    }

    fn template() -> SkillTemplate {
        SkillTemplate {
            title: "带生命值的墙".into(),
            summary: "生成一个指定名称与生命值的墙方块".into(),
            parameters: vec![
                SkillParameter {
                    name: "block".into(),
                    description: "方块名称".into(),
                    kind: ParameterKind::Identifier,
                },
                SkillParameter {
                    name: "health".into(),
                    description: "生命值".into(),
                    kind: ParameterKind::Integer {
                        min: 100,
                        max: 4000,
                    },
                },
                SkillParameter {
                    name: "kind".into(),
                    description: "类型".into(),
                    kind: ParameterKind::Choice {
                        options: vec!["Wall".into(), "Door".into()],
                    },
                },
            ],
            files: vec![ArtifactFile {
                path: "content/blocks/{{block}}.hjson".into(),
                content: "type: {{kind}}\nhealth: {{health}}\nextra: {\"a\":{\"b\":1}}\n".into(),
            }],
            probes: vec![
                Probe {
                    subject: "m-{{block}}".into(),
                    property: "health".into(),
                    expected: "{{health}}".into(),
                },
                Probe {
                    subject: "m-{{block}}".into(),
                    property: "class".into(),
                    expected: "{{kind}}".into(),
                },
            ],
        }
    }

    fn arguments(block: &str, health: &str, kind: &str) -> Arguments {
        [("block", block), ("health", health), ("kind", kind)]
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn instantiation_substitutes_only_declared_placeholders() {
        let draft = instantiate(&template(), &arguments("wall", "800", "Wall")).unwrap();
        assert_eq!(draft.files[0].path, "content/blocks/wall.hjson");
        assert_eq!(
            draft.files[0].content,
            "type: Wall\nhealth: 800\nextra: {\"a\":{\"b\":1}}\n"
        );
        assert_eq!(draft.probes[0].subject, "m-wall");
        assert_eq!(draft.probes[1].expected, "Wall");
        assert!(draft.notes_used.is_empty());
        assert!(validate_artifact(&profile(), &draft).is_ok());
    }

    #[test]
    fn arguments_must_match_declared_kinds_exactly() {
        let template = template();
        for bad in [
            arguments("Wall", "800", "Wall"),
            arguments("wall-", "800", "Wall"),
            arguments("wall", "0800", "Wall"),
            arguments("wall", "+800", "Wall"),
            arguments("wall", "99", "Wall"),
            arguments("wall", "800", "Router"),
            arguments("wall\"", "800", "Wall"),
        ] {
            assert!(instantiate(&template, &bad).is_err());
        }
        let mut extra = arguments("wall", "800", "Wall");
        extra.insert("other".into(), "x".into());
        assert!(instantiate(&template, &extra).is_err());
        let mut missing = arguments("wall", "800", "Wall");
        missing.remove("kind");
        assert!(instantiate(&template, &missing).is_err());
    }

    #[test]
    fn templates_must_probe_every_parameter_and_use_known_placeholders() {
        assert!(template_issues(&template(), &profile()).is_empty());
        let mut unprobed = template();
        unprobed.probes.pop();
        assert!(!template_issues(&unprobed, &profile()).is_empty());
        let mut unknown = template();
        unknown.files[0].content.push_str("{{speed}}");
        assert!(!template_issues(&unknown, &profile()).is_empty());
        let mut malformed = template();
        malformed.files[0].content.push_str("{{ health }}");
        assert!(!template_issues(&malformed, &profile()).is_empty());
        let mut narrow = template();
        narrow.parameters[1].kind = ParameterKind::Integer { min: 5, max: 5 };
        assert!(!template_issues(&narrow, &profile()).is_empty());
        let mut property = template();
        property.probes[0].property = "speed".into();
        assert!(!template_issues(&property, &profile()).is_empty());
    }

    #[test]
    fn proposals_must_reproduce_the_verified_draft_and_get_host_chosen_holdout() {
        let original = arguments("wall", "800", "Wall");
        let source = instantiate(&template(), &original).unwrap();
        let proposal = SkillProposal {
            extends: None,
            name: "health-wall".into(),
            template: template(),
            arguments: original.clone(),
        };
        let (issues, holdout) = proposal_issues(&proposal, &source, &profile());
        assert!(issues.is_empty(), "{issues:?}");
        let holdout = holdout.unwrap();
        assert_eq!(holdout, arguments("wall-b", "4000", "Door"));
        assert_eq!(
            holdout_arguments(&template(), &arguments("w", "4000", "Door")).unwrap(),
            arguments("w-b", "100", "Wall")
        );

        let mut drifted = source.clone();
        drifted.files[0].content = drifted.files[0].content.replace("800", "801");
        let (issues, holdout) = proposal_issues(&proposal, &drifted, &profile());
        assert!(!issues.is_empty() && holdout.is_none());
    }

    #[test]
    fn identifiers_stay_valid_when_extended_for_holdout() {
        let long = "a".repeat(MAX_NAME_BYTES);
        let mut template = template();
        template.parameters.truncate(1);
        let holdout = holdout_arguments(
            &template,
            &[("block".to_string(), long.clone())].into_iter().collect(),
        )
        .unwrap();
        assert!(is_identifier(&holdout["block"]));
        assert_ne!(holdout["block"], long);
    }
}
