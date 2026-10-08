//! 实践记录与真实运行验证的来源契约。
//!
//! 实践是 Eve 自己做出的可运行产物：草稿只能包含运行器声明允许的数据文件，
//! 由运行器在全新目录中实际运行，并按受限的探测项核对行为。草稿、编译或模型自述
//! 都不算验证；只有运行器给出的实际加载与探测结果才能把一次尝试记为已验证。
//! 账本、草稿器与运行器均可替换，宿主负责绑定目标、可见范围与工作目录。
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt, future::Future, path::Path, pin::Pin};

pub const PRACTICE_PLUGIN_ID: &str = "eve.practice";
pub const PRACTICE_STATE_KEY: &str = "practice.v1";
pub const MAX_BRIEF_BYTES: usize = 4096;
/// 交给草稿器的资料条数与单条上限。
pub const MAX_NOTES: usize = 12;
pub const MAX_NOTE_BYTES: usize = 1024;
pub const MAX_FILES: usize = 8;
pub const MAX_PATH_BYTES: usize = 128;
pub const MAX_FILE_BYTES: usize = 8 * 1024;
pub const MAX_ARTIFACT_BYTES: usize = 24 * 1024;
pub const MAX_PROBES: usize = 8;
pub const MAX_IDENTIFIER_BYTES: usize = 64;
pub const MAX_EXPECTED_BYTES: usize = 128;
pub const MAX_RATIONALE_BYTES: usize = 1024;
pub const MAX_PROFILE_BYTES: usize = 4096;
pub const MAX_ISSUES: usize = 16;
pub const MAX_ISSUE_BYTES: usize = 256;
pub const MAX_LOG_EXCERPT_BYTES: usize = 8 * 1024;
pub const MAX_DRAFT_OUTPUT_BYTES: usize = 32 * 1024;
/// 一次实践的尝试次数：首次草稿加至多两次依据证据的修正。
pub const MAX_ATTEMPTS: usize = 3;
/// 实践记录总数，不自动淘汰；达到容量后保留原记录并停止新实践。
pub const MAX_RUNS: usize = 64;
/// 同一目标在所有修订中的实践次数上限；重启不会重置。
pub const MAX_RUNS_PER_GOAL: usize = 2;
pub const MAX_STATE_BYTES: usize = 4 * 1024 * 1024;

pub type PracticeResult<T> = Result<T, PracticeError>;
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type PracticeFuture<'a, T> = BoxFuture<'a, PracticeResult<T>>;

/// 交给草稿器的已有资料，例如有来源的领域知识；只是数据，不是指令。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskNote {
    pub id: String,
    pub text: String,
    /// 来源网址或其他出处。
    pub source: Option<String>,
    pub version: Option<String>,
    /// 是否附有可核对的来源原文；未验证推测为 false。
    pub source_quoted: bool,
}

/// 由宿主从学习目标派生的实践任务。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PracticeTask {
    pub goal_id: String,
    pub goal_revision: u64,
    /// 目标可见的用户；实践记录只向该用户展示。
    pub owner: String,
    pub brief: String,
    pub brief_truncated: bool,
    pub notes: Vec<TaskNote>,
}
impl PracticeTask {
    pub fn validate(&self) -> PracticeResult<()> {
        validate_id(&self.goal_id)?;
        validate_id(&self.owner)?;
        validate_text(&self.brief, MAX_BRIEF_BYTES)?;
        if self.goal_revision == 0 || self.notes.len() > MAX_NOTES {
            return Err(PracticeError::InvalidInput);
        }
        let mut ids = BTreeSet::new();
        for note in &self.notes {
            validate_id(&note.id)?;
            validate_text(&note.text, MAX_NOTE_BYTES)?;
            for value in note.source.iter().chain(&note.version) {
                validate_text(value, MAX_NOTE_BYTES)?;
            }
            if !ids.insert(note.id.as_str()) {
                return Err(PracticeError::InvalidInput);
            }
        }
        Ok(())
    }
}

/// 运行器对外声明的能力：适用领域、文件布局规则与可用探测属性。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerProfile {
    pub runner_id: String,
    /// 实际使用的运行环境标识，例如运行包文件的摘要；用于复验。
    pub runtime: String,
    pub domain: String,
    pub layout: String,
    pub properties: Vec<String>,
}
impl RunnerProfile {
    pub fn validate(&self) -> PracticeResult<()> {
        validate_id(&self.runner_id)?;
        validate_text(&self.runtime, MAX_PROFILE_BYTES)?;
        validate_text(&self.domain, MAX_PROFILE_BYTES)?;
        validate_text(&self.layout, MAX_PROFILE_BYTES)?;
        let mut seen = BTreeSet::new();
        if self.properties.is_empty()
            || self.properties.len() > 32
            || self
                .properties
                .iter()
                .any(|property| !is_property(property) || !seen.insert(property.as_str()))
        {
            return Err(PracticeError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactFile {
    /// 相对路径，段只含字母、数字、`.`、`_`、`-`。
    pub path: String,
    pub content: String,
}

/// 一项受限探测：运行器只按 subject 与 property 构造自己的查询，不执行草稿提供的代码。
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    pub subject: String,
    pub property: String,
    pub expected: String,
}

/// 草稿器的一次输出。applicable 为 false 表示运行器的领域与目标无关，不运行任何东西。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PracticeDraft {
    pub applicable: bool,
    pub files: Vec<ArtifactFile>,
    pub probes: Vec<Probe>,
    pub rationale: String,
    /// 草稿依据的资料 ID，须来自任务资料。
    pub notes_used: Vec<String>,
}

/// 校验草稿的通用形状；领域相关的结构检查由运行器的 `check` 负责。
pub fn validate_draft(
    task: &PracticeTask,
    profile: &RunnerProfile,
    draft: &PracticeDraft,
) -> PracticeResult<()> {
    let invalid = || PracticeError::InvalidInput;
    validate_text(&draft.rationale, MAX_RATIONALE_BYTES)?;
    let notes: BTreeSet<_> = task.notes.iter().map(|note| note.id.as_str()).collect();
    let mut used = BTreeSet::new();
    if draft
        .notes_used
        .iter()
        .any(|id| !notes.contains(id.as_str()) || !used.insert(id.as_str()))
    {
        return Err(invalid());
    }
    if !draft.applicable {
        return if draft.files.is_empty() && draft.probes.is_empty() {
            Ok(())
        } else {
            Err(invalid())
        };
    }
    if draft.files.is_empty()
        || draft.files.len() > MAX_FILES
        || draft.probes.is_empty()
        || draft.probes.len() > MAX_PROBES
    {
        return Err(invalid());
    }
    let mut paths = BTreeSet::new();
    let mut total = 0usize;
    for file in &draft.files {
        if !is_relative_path(&file.path)
            || !paths.insert(file.path.to_ascii_lowercase())
            || file.content.len() > MAX_FILE_BYTES
            || file.content.contains('\0')
        {
            return Err(invalid());
        }
        total += file.content.len();
    }
    if total > MAX_ARTIFACT_BYTES {
        return Err(invalid());
    }
    let mut probes = BTreeSet::new();
    for probe in &draft.probes {
        if !is_subject(&probe.subject)
            || !profile.properties.contains(&probe.property)
            || probe.expected.len() > MAX_EXPECTED_BYTES
            || probe.expected.chars().any(char::is_control)
            || probe.expected.trim() != probe.expected
            || !probes.insert((probe.subject.as_str(), probe.property.as_str()))
        {
            return Err(invalid());
        }
    }
    Ok(())
}

/// 相对路径：不以分隔符开头，段只含 `[A-Za-z0-9._-]`，不是 `.` 或 `..`。
pub fn is_relative_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH_BYTES
        && path.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
}

/// 探测对象：小写字母、数字与 `-`，以字母或数字开头。
pub fn is_subject(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_BYTES
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// 探测属性：以字母开头的字母数字标识。
pub fn is_property(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_BYTES
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic())
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RunExit {
    /// 运行器按协议完成探测并正常结束。
    Completed,
    /// 超过运行时限，进程已终止。
    Timeout,
    /// 无法准备目录或启动运行环境。
    StartFailed,
    /// 运行环境在完成探测前退出。
    Crashed,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeResult {
    pub probe: Probe,
    /// 运行环境实际给出的值；查询失败为 None。
    pub actual: Option<String>,
    pub passed: bool,
}

/// 运行器实际采集的证据。日志摘录只保留与产物相关的行和版本信息，完整日志以摘要核对。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunEvidence {
    pub runtime_version: String,
    pub exit: RunExit,
    /// 运行环境确认产物已加载并启用。
    pub loaded: bool,
    /// 运行环境对产物给出的警告或错误。
    pub warnings: Vec<String>,
    pub probes: Vec<ProbeResult>,
    pub log_excerpt: String,
    pub log_sha256: String,
    pub log_bytes: u64,
    pub duration_ms: u64,
}
impl RunEvidence {
    pub fn validate(&self, draft: &PracticeDraft) -> PracticeResult<()> {
        let invalid = || PracticeError::InvalidInput;
        if self.runtime_version.len() > MAX_ISSUE_BYTES
            || self.runtime_version.chars().any(char::is_control)
            || self.warnings.len() > MAX_ISSUES
            || self.log_excerpt.len() > MAX_LOG_EXCERPT_BYTES
            || self.log_excerpt.contains('\0')
            || !is_sha256(&self.log_sha256)
            || self.probes.len() > draft.probes.len()
        {
            return Err(invalid());
        }
        validate_issues(&self.warnings)?;
        for (result, probe) in self.probes.iter().zip(&draft.probes) {
            if result.probe != *probe
                || result.actual.as_ref().is_some_and(|actual| {
                    actual.len() > MAX_EXPECTED_BYTES * 2 || actual.chars().any(char::is_control)
                })
                || (result.passed && result.actual.is_none())
            {
                return Err(invalid());
            }
        }
        // 只有完整完成的运行才可能给出全部探测结果。
        if self.exit == RunExit::Completed && self.probes.len() != draft.probes.len() {
            return Err(invalid());
        }
        Ok(())
    }

    /// 已验证：正常结束、确认加载、没有针对产物的警告，且每项探测都通过。
    pub fn verified(&self, draft: &PracticeDraft) -> bool {
        self.exit == RunExit::Completed
            && self.loaded
            && self.warnings.is_empty()
            && !draft.probes.is_empty()
            && self.probes.len() == draft.probes.len()
            && self.probes.iter().all(|result| result.passed)
    }
}

/// 可替换运行器。`check` 不触碰文件系统；`run` 只在宿主给定的空目录中写入草稿文件并运行。
pub trait PracticeRunner: Send + Sync {
    fn profile(&self) -> &RunnerProfile;
    /// 运行前的结构检查，返回问题列表；空表示可以运行。
    fn check(&self, draft: &PracticeDraft) -> Vec<String>;
    fn run<'a>(
        &'a self,
        draft: &'a PracticeDraft,
        workspace: &'a Path,
    ) -> BoxFuture<'a, RunEvidence>;
}

/// 上一次尝试的结果，供草稿器依据实际证据修正。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviousAttempt {
    pub draft: PracticeDraft,
    pub issues: Vec<String>,
    pub evidence: Option<RunEvidence>,
}

/// 草稿请求；任务资料与上一次证据都是数据，不是指令。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftRequest {
    pub run_id: String,
    pub drafter_version: String,
    pub attempt: usize,
    pub task: PracticeTask,
    pub runner: RunnerProfile,
    pub previous: Option<PreviousAttempt>,
}

/// 可替换草稿器；至多一次模型请求、零工具，输出须通过 `validate_draft`。
pub trait PracticeDrafter: Send + Sync {
    fn version(&self) -> &str;
    fn draft(&self, request: DraftRequest) -> PracticeFuture<'_, PracticeDraft>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PracticeFailure {
    Provider,
    InvalidOutput,
    Timeout,
    Cancelled,
    /// 账本容量已满。
    LimitReached,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum AttemptStage {
    /// 已保存准入，正在请求草稿。
    Drafting,
    /// 已保存通过结构检查的草稿，正在实际运行。
    Running,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AttemptOutcome {
    /// 草稿器判断运行器与目标无关，没有运行。
    NotApplicable,
    /// 结构检查不通过，没有运行。
    Rejected,
    Verified,
    /// 实际运行了，但证据不足以验证。
    Failed,
    /// 草稿请求失败。
    DraftFailed {
        failure: PracticeFailure,
    },
    /// 因停止或超时放弃；运行中的进程已终止，结果未知。
    Abandoned {
        failure: PracticeFailure,
    },
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PracticeAttempt {
    pub number: usize,
    pub stage: AttemptStage,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub draft: Option<PracticeDraft>,
    pub issues: Vec<String>,
    pub evidence: Option<RunEvidence>,
    pub outcome: Option<AttemptOutcome>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PracticeStatus {
    Running,
    Verified,
    NotApplicable,
    /// 尝试用尽仍未验证。
    Unverified,
    Failed(PracticeFailure),
    /// 进程在 Running 时退出；重启后记为中断，不重放。
    Interrupted,
}

/// 一次实践的完整记录；每一步先保存再执行对应外部动作。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PracticeRun {
    pub id: String,
    pub task: PracticeTask,
    pub runner: RunnerProfile,
    pub drafter_version: String,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub status: PracticeStatus,
    pub attempts: Vec<PracticeAttempt>,
}
impl PracticeRun {
    pub fn current(&self) -> Option<&PracticeAttempt> {
        self.attempts.last()
    }
    /// 最近一次已验证的尝试。
    pub fn verified_attempt(&self) -> Option<&PracticeAttempt> {
        self.attempts
            .iter()
            .rev()
            .find(|attempt| attempt.outcome == Some(AttemptOutcome::Verified))
    }
}

/// 同一目标修订的实践标识；重复准入得到同一 ID。
pub fn run_id(goal_id: &str, goal_revision: u64) -> String {
    let mut context = Context::new(&SHA256);
    for part in ["practice.run:v1", goal_id, &goal_revision.to_string()] {
        context.update(&(part.len() as u64).to_be_bytes());
        context.update(part.as_bytes());
    }
    let digest: String = context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("practice-{}", &digest[..32])
}

#[derive(Clone, Eq, PartialEq)]
pub struct PracticeSnapshot {
    pub runs: Vec<PracticeRun>,
}
impl PracticeSnapshot {
    pub fn runs_for<'a>(&'a self, goal_id: &'a str) -> impl Iterator<Item = &'a PracticeRun> {
        self.runs
            .iter()
            .filter(move |run| run.task.goal_id == goal_id)
    }
}

/// 仅可信宿主持有；不发布给模型或不受信插件。
pub trait PracticeAdmin: Send + Sync {
    fn snapshot(&self) -> PracticeResult<PracticeSnapshot>;
    /// 原子保存 Running 实践与第一次尝试（Drafting），成功返回后才可请求草稿。
    /// 同一目标修订已有实践、该目标次数已达上限，或已有其他实践在进行时返回 None。
    fn begin(
        &self,
        task: PracticeTask,
        runner: &RunnerProfile,
        drafter_version: &str,
        now_ms: u64,
    ) -> PracticeResult<Option<PracticeRun>>;
    /// 保存草稿结果。通过结构检查的草稿进入 Running，返回后才可运行；
    /// 不适用、请求失败或尝试用尽时同一次提交写入实践结局；被拒绝且仍有次数时开启下一次尝试。
    fn record_draft(
        &self,
        run_id: &str,
        at_ms: u64,
        result: Result<PracticeDraft, PracticeFailure>,
        issues: Vec<String>,
    ) -> PracticeResult<PracticeRun>;
    /// 保存实际运行证据；已验证即结束，否则在仍有次数时开启下一次尝试。
    fn record_evidence(
        &self,
        run_id: &str,
        at_ms: u64,
        evidence: RunEvidence,
    ) -> PracticeResult<PracticeRun>;
    /// 因停止或超时放弃进行中的实践；只能从 Running 结束一次。
    fn abandon(
        &self,
        run_id: &str,
        at_ms: u64,
        failure: PracticeFailure,
    ) -> PracticeResult<PracticeRun>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PracticeError {
    InvalidInput,
    NotFound,
    Unavailable,
    CorruptState,
    UnsupportedVersion,
    Storage,
    LimitReached,
    Conflict,
    Practice(PracticeFailure),
}
impl fmt::Display for PracticeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "实践输入无效",
            Self::NotFound => "没有这条实践记录",
            Self::Unavailable => "实践服务不可用",
            Self::CorruptState => "实践状态损坏；未清空",
            Self::UnsupportedVersion => "不支持该实践状态版本",
            Self::Storage => "实践提交无法确认；须重新打开核对",
            Self::LimitReached => "实践容量已满；保留原记录",
            Self::Conflict => "实践记录状态冲突",
            Self::Practice(_) => "实践请求失败",
        })
    }
}
impl std::error::Error for PracticeError {}

pub fn validate_id(value: &str) -> PracticeResult<()> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(PracticeError::InvalidInput);
    }
    Ok(())
}

pub fn validate_text(value: &str, limit: usize) -> PracticeResult<()> {
    if value.trim().is_empty() || value.len() > limit || value.contains('\0') {
        return Err(PracticeError::InvalidInput);
    }
    Ok(())
}

pub fn validate_issues(issues: &[String]) -> PracticeResult<()> {
    if issues.len() > MAX_ISSUES
        || issues
            .iter()
            .any(|issue| validate_text(issue, MAX_ISSUE_BYTES).is_err() || issue.contains('\n'))
    {
        return Err(PracticeError::InvalidInput);
    }
    Ok(())
}

pub fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

macro_rules! redacted { ($($ty:ty),+ $(,)?) => { $(impl fmt::Debug for $ty { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(concat!(stringify!($ty), "(<redacted>)")) } })+ }; }
redacted!(
    TaskNote,
    PracticeTask,
    RunnerProfile,
    ArtifactFile,
    Probe,
    PracticeDraft,
    ProbeResult,
    RunEvidence,
    PreviousAttempt,
    DraftRequest,
    AttemptOutcome,
    PracticeAttempt,
    PracticeRun,
    PracticeSnapshot,
);

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> PracticeTask {
        PracticeTask {
            goal_id: "goal".into(),
            goal_revision: 1,
            owner: "user".into(),
            brief: "学习目标".into(),
            brief_truncated: false,
            notes: vec![TaskNote {
                id: "knowledge-1".into(),
                text: "每个模组根目录需要 mod.hjson".into(),
                source: Some("https://docs.example/start.html".into()),
                version: Some("v146".into()),
                source_quoted: true,
            }],
        }
    }

    fn profile() -> RunnerProfile {
        RunnerProfile {
            runner_id: "runner:v1".into(),
            runtime: "runtime sha256:0".into(),
            domain: "某类数据文件".into(),
            layout: "根目录放 manifest".into(),
            properties: vec!["exists".into(), "health".into()],
        }
    }

    fn draft() -> PracticeDraft {
        PracticeDraft {
            applicable: true,
            files: vec![ArtifactFile {
                path: "content/blocks/wall.hjson".into(),
                content: "type: Wall".into(),
            }],
            probes: vec![Probe {
                subject: "eve-wall".into(),
                property: "health".into(),
                expected: "520".into(),
            }],
            rationale: "最小方块".into(),
            notes_used: vec!["knowledge-1".into()],
        }
    }

    #[test]
    fn drafts_are_limited_to_relative_data_files_and_declared_probes() {
        assert!(validate_draft(&task(), &profile(), &draft()).is_ok());
        let not_applicable = PracticeDraft {
            applicable: false,
            files: vec![],
            probes: vec![],
            notes_used: vec![],
            ..draft()
        };
        assert!(validate_draft(&task(), &profile(), &not_applicable).is_ok());
        let mut cases = Vec::new();
        for path in [
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "a/b c",
            "./a",
            "C:\\x",
        ] {
            let mut bad = draft();
            bad.files[0].path = path.into();
            cases.push(bad);
        }
        let mut bad = draft();
        bad.files.push(bad.files[0].clone());
        cases.push(bad);
        let mut bad = draft();
        bad.probes[0].property = "constructor".into();
        cases.push(bad);
        let mut bad = draft();
        bad.probes[0].subject = "Eve_Wall".into();
        cases.push(bad);
        let mut bad = draft();
        bad.probes[0].expected = "520\n".into();
        cases.push(bad);
        let mut bad = draft();
        bad.probes.clear();
        cases.push(bad);
        let mut bad = draft();
        bad.notes_used = vec!["knowledge-9".into()];
        cases.push(bad);
        let mut bad = not_applicable.clone();
        bad.files = draft().files;
        cases.push(bad);
        let mut bad = draft();
        bad.files[0].content = "x".repeat(MAX_FILE_BYTES + 1);
        cases.push(bad);
        for (index, bad) in cases.iter().enumerate() {
            assert!(validate_draft(&task(), &profile(), bad).is_err(), "{index}");
        }
    }

    #[test]
    fn evidence_is_verified_only_by_a_completed_run_with_all_probes_passing() {
        let draft = draft();
        let evidence = RunEvidence {
            runtime_version: "build 160.7".into(),
            exit: RunExit::Completed,
            loaded: true,
            warnings: vec![],
            probes: vec![ProbeResult {
                probe: draft.probes[0].clone(),
                actual: Some("520".into()),
                passed: true,
            }],
            log_excerpt: "1 mods loaded".into(),
            log_sha256: "0".repeat(64),
            log_bytes: 13,
            duration_ms: 5,
        };
        evidence.validate(&draft).unwrap();
        assert!(evidence.verified(&draft));
        for unverified in [
            RunEvidence {
                loaded: false,
                ..evidence.clone()
            },
            RunEvidence {
                warnings: vec!["defaulting to type 'Block'".into()],
                ..evidence.clone()
            },
            RunEvidence {
                probes: vec![ProbeResult {
                    passed: false,
                    actual: Some("400".into()),
                    ..evidence.probes[0].clone()
                }],
                ..evidence.clone()
            },
        ] {
            unverified.validate(&draft).unwrap();
            assert!(!unverified.verified(&draft));
        }
        let timeout = RunEvidence {
            exit: RunExit::Timeout,
            probes: vec![],
            ..evidence.clone()
        };
        timeout.validate(&draft).unwrap();
        assert!(!timeout.verified(&draft));
        // 完成的运行必须给出全部探测结果；通过的探测必须有实际值。
        assert!(
            RunEvidence {
                probes: vec![],
                ..evidence.clone()
            }
            .validate(&draft)
            .is_err()
        );
        assert!(
            RunEvidence {
                probes: vec![ProbeResult {
                    actual: None,
                    ..evidence.probes[0].clone()
                }],
                ..evidence
            }
            .validate(&draft)
            .is_err()
        );
    }

    #[test]
    fn identifiers_are_stable() {
        assert_eq!(run_id("g", 1), run_id("g", 1));
        assert_ne!(run_id("g", 1), run_id("g", 2));
        assert!(is_subject("eve-sample-wall") && !is_subject("-x") && !is_subject("a.b"));
        assert!(is_property("itemCapacity") && !is_property("__proto__"));
    }
}
