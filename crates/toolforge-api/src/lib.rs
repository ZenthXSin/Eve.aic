//! 工具锻造契约：把反复出现的能力缺口变成实践运行前可调用的检查工具。
//!
//! 工具是数据：一组只读草稿文件的规则（要求有匹配的文件、要求或禁止包含某段文本），由本契约的
//! 解释器 `evaluate` 执行，不执行任何生成的代码，因而不扩大宿主授信范围。锻造器（可替换，通常是
//! 一次无工具模型请求）依据缺口与实践账本中的真实草稿给出规格；宿主用同一批真实草稿回放验证：
//! 出现该问题的草稿必须全部被拦下，实际运行验证通过的草稿一个也不能误报，通过才成为新版本，
//! 并在用户没有停用时自动启用。工具可停用、启用某个已验证版本或回退；每次调用都留下记录。
//! 工具只用于来源用户、同一运行器的后续实践；账本与锻造器都可替换。
use eve_practice_api::{ArtifactFile, MAX_ISSUE_BYTES, RunnerProfile, is_relative_path};
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt, future::Future, pin::Pin};

pub const TOOLFORGE_PLUGIN_ID: &str = "eve.toolforge";
pub const TOOLFORGE_STATE_KEY: &str = "toolforge.v1";
/// 工具总数，不自动淘汰；满后保留原记录并停止新的锻造。
pub const MAX_TOOLS: usize = 32;
pub const MAX_VERSIONS: usize = 8;
/// 每个工具的启用变更记录上限；满后拒绝新的手动变更。
pub const MAX_CHANGES: usize = 64;
pub const MAX_FORGES: usize = 64;
/// 同一缺口的锻造次数上限；只有出现次数增加时才再次锻造。
pub const MAX_FORGES_PER_GAP: usize = 3;
/// 检查调用记录总数，不自动淘汰；满后宿主暂停使用锻造的检查并明确显示。
pub const MAX_CALLS: usize = 512;
pub const MAX_RULES: usize = 8;
pub const MAX_PATTERN_BYTES: usize = 128;
pub const MAX_RULE_TEXT_BYTES: usize = 256;
pub const MAX_NAME_BYTES: usize = 32;
pub const MAX_SUMMARY_BYTES: usize = 512;
pub const MAX_MESSAGE_BYTES: usize = 160;
pub const MAX_REASON_BYTES: usize = 512;
/// 交给锻造器、用于回放验证的每一侧草稿数上限。
pub const MAX_EXAMPLES: usize = 4;
pub const MAX_FINDINGS: usize = 8;
pub const MAX_FINDING_BYTES: usize = 384;
pub const MAX_FORGE_OUTPUT_BYTES: usize = 8 * 1024;
pub const MAX_STATE_BYTES: usize = 4 * 1024 * 1024;

pub type ToolResult<T> = Result<T, ToolError>;
pub type ToolFuture<'a, T> = Pin<Box<dyn Future<Output = ToolResult<T>> + Send + 'a>>;

/// 一条规则。`pattern` 是相对路径，或以 `*` 开头的路径后缀（例如 `*.hjson`）。
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckRule {
    /// 至少有一个匹配的文件。
    RequireFile { pattern: String },
    /// 至少有一个匹配的文件，且每个匹配的文件都包含这段文本。
    RequireText { pattern: String, text: String },
    /// 没有匹配的文件包含这段文本。
    ForbidText { pattern: String, text: String },
}
impl CheckRule {
    pub fn pattern(&self) -> &str {
        match self {
            Self::RequireFile { pattern }
            | Self::RequireText { pattern, .. }
            | Self::ForbidText { pattern, .. } => pattern,
        }
    }
}

/// 一个检查工具的规格；`message` 是拦下草稿时交给草稿器的说明。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckSpec {
    pub name: String,
    pub summary: String,
    pub message: String,
    pub rules: Vec<CheckRule>,
}
impl CheckSpec {
    pub fn validate(&self) -> ToolResult<()> {
        let invalid = || ToolError::InvalidInput;
        if !is_name(&self.name)
            || self.rules.is_empty()
            || self.rules.len() > MAX_RULES
            || !is_line(&self.message, MAX_MESSAGE_BYTES)
        {
            return Err(invalid());
        }
        validate_text(&self.summary, MAX_SUMMARY_BYTES)?;
        let mut seen = BTreeSet::new();
        for rule in &self.rules {
            if !is_pattern(rule.pattern()) || !seen.insert(rule) {
                return Err(invalid());
            }
            if let CheckRule::RequireText { text, .. } | CheckRule::ForbidText { text, .. } = rule
                && !is_line(text, MAX_RULE_TEXT_BYTES)
            {
                return Err(invalid());
            }
        }
        Ok(())
    }
}

/// 工具名称：小写字母开头，只含小写字母、数字和 `-`，不以 `-` 结尾。
pub fn is_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_NAME_BYTES
        && value.starts_with(|c: char| c.is_ascii_lowercase())
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// 相对路径，或 `*` 加至多 64 字节的后缀（只含字母、数字、`.`、`_`、`-`、`/`，不含 `..`）。
pub fn is_pattern(pattern: &str) -> bool {
    match pattern.strip_prefix('*') {
        Some(suffix) => {
            !suffix.is_empty()
                && suffix.len() <= 64
                && !suffix.contains("..")
                && suffix.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/')
                })
        }
        None => pattern.len() <= MAX_PATTERN_BYTES && is_relative_path(pattern),
    }
}

pub fn pattern_matches(pattern: &str, path: &str) -> bool {
    match pattern.strip_prefix('*') {
        Some(suffix) => path.ends_with(suffix),
        None => path == pattern,
    }
}

/// 只读执行规格：返回发现的问题，空表示通过。不触碰文件系统，不执行文件内容。
pub fn evaluate(spec: &CheckSpec, files: &[ArtifactFile]) -> Vec<String> {
    let mut findings = Vec::new();
    for rule in &spec.rules {
        let pattern = rule.pattern();
        let matched: Vec<&ArtifactFile> = files
            .iter()
            .filter(|file| pattern_matches(pattern, &file.path))
            .collect();
        match rule {
            CheckRule::RequireFile { .. } | CheckRule::RequireText { .. } if matched.is_empty() => {
                findings.push(format!("缺少匹配 {pattern} 的文件"));
            }
            CheckRule::RequireFile { .. } => {}
            CheckRule::RequireText { text, .. } => {
                for file in matched {
                    if !file.content.contains(text.as_str()) {
                        findings.push(format!("{} 缺少“{text}”", file.path));
                    }
                }
            }
            CheckRule::ForbidText { text, .. } => {
                for file in matched {
                    if file.content.contains(text.as_str()) {
                        findings.push(format!("{} 含有“{text}”", file.path));
                    }
                }
            }
        }
    }
    findings.truncate(MAX_FINDINGS);
    findings
}

/// 实践账本中的一份真实草稿，只含文件；不含任务描述等用户内容。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExampleFiles {
    pub run_id: String,
    pub attempt: usize,
    pub files: Vec<ArtifactFile>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExampleResult {
    pub run_id: String,
    pub attempt: usize,
    /// 这份草稿出现过该问题，应当被拦下。
    pub expect_flag: bool,
    pub findings: Vec<String>,
}
impl ExampleResult {
    pub fn passed(&self) -> bool {
        self.expect_flag != self.findings.is_empty()
    }
}

/// 用真实草稿回放的验证证据。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verification {
    pub examples: Vec<ExampleResult>,
}
impl Verification {
    /// 两侧都有草稿，且每份都符合预期。
    pub fn passed(&self) -> bool {
        self.examples.iter().any(|example| example.expect_flag)
            && self.examples.iter().any(|example| !example.expect_flag)
            && self.examples.iter().all(ExampleResult::passed)
    }
    pub fn validate(&self) -> ToolResult<()> {
        if self.examples.len() > 2 * MAX_EXAMPLES {
            return Err(ToolError::InvalidInput);
        }
        let mut seen = BTreeSet::new();
        for example in &self.examples {
            validate_id(&example.run_id)?;
            validate_findings(&example.findings)?;
            if example.attempt == 0 || !seen.insert((&example.run_id, example.attempt)) {
                return Err(ToolError::InvalidInput);
            }
        }
        Ok(())
    }
}

/// 回放验证：出现问题的草稿必须被拦下，验证通过的草稿不能被拦下。
pub fn verify(
    spec: &CheckSpec,
    failing: &[ExampleFiles],
    passing: &[ExampleFiles],
) -> Verification {
    let result = |example: &ExampleFiles, expect_flag| ExampleResult {
        run_id: example.run_id.clone(),
        attempt: example.attempt,
        expect_flag,
        findings: evaluate(spec, &example.files),
    };
    Verification {
        examples: failing
            .iter()
            .map(|example| result(example, true))
            .chain(passing.iter().map(|example| result(example, false)))
            .collect(),
    }
}

/// 工具针对的能力缺口：某个运行器下按 `gap_key` 归类的反复问题。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GapRef {
    pub runner_id: String,
    pub key: String,
    /// 最近一次出现时的原文。
    pub summary: String,
}
impl GapRef {
    pub fn validate(&self) -> ToolResult<()> {
        validate_id(&self.runner_id)?;
        validate_text(&self.key, MAX_ISSUE_BYTES)?;
        validate_text(&self.summary, MAX_ISSUE_BYTES)?;
        if self.key.is_empty() {
            return Err(ToolError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRef {
    pub tool_id: String,
    pub version: u32,
}

/// 锻造请求；草稿文件与问题原文都是数据，不是指令。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgeRequest {
    pub forge_id: String,
    pub forger_version: String,
    pub gap: GapRef,
    pub runner: RunnerProfile,
    /// 出现该问题的草稿。
    pub failing: Vec<ExampleFiles>,
    /// 实际运行验证通过的草稿。
    pub passing: Vec<ExampleFiles>,
    /// 同一缺口当前的工具规格；改进时在它之上修改。
    pub current: Option<CheckSpec>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ForgeOutput {
    Check(CheckSpec),
    /// 这个问题无法用只读规则在运行前识别。
    NotForgeable {
        reason: String,
    },
}

/// 可替换锻造器；每次至多一次无工具模型请求，输出须通过 `CheckSpec::validate`。
pub trait ToolForger: Send + Sync {
    fn version(&self) -> &str;
    fn forge(&self, request: ForgeRequest) -> ToolFuture<'_, ForgeOutput>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ForgeFailure {
    Provider,
    InvalidOutput,
    Timeout,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ForgeStatus {
    /// 已保存准入，正在请求锻造器。
    Running,
    /// 回放验证通过，成为工具的新版本。
    Verified,
    /// 回放验证未通过，没有成为工具。
    Rejected,
    /// 已有工具回放验证即覆盖这个缺口，没有请求锻造器。
    Reused,
    NotForgeable,
    Failed(ForgeFailure),
    /// 进程在 Running 时退出；重启后记为中断，不重放。
    Interrupted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgeAttempt {
    pub id: String,
    pub owner: String,
    pub gap: GapRef,
    /// 准入时该缺口的出现次数。
    pub occurrences: usize,
    pub forger_version: String,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub status: ForgeStatus,
    pub output: Option<ForgeOutput>,
    pub verification: Option<Verification>,
    /// 成为的工具版本，或复用的工具版本。
    pub tool: Option<ToolRef>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Actor {
    /// 验证通过后自动启用。
    Automatic,
    Owner,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolChange {
    pub at_ms: u64,
    pub actor: Actor,
    pub enabled: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolVersion {
    pub version: u32,
    pub forge_id: String,
    pub spec: CheckSpec,
    pub verification: Verification,
    pub created_at_ms: u64,
}

/// 一个锻造出的工具；每个用户、运行器与缺口一个，版本递增，不删除。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgedTool {
    pub id: String,
    pub owner: String,
    pub runner_id: String,
    pub gap_key: String,
    pub versions: Vec<ToolVersion>,
    pub enabled: Option<u32>,
    pub changes: Vec<ToolChange>,
}
impl ForgedTool {
    pub fn version(&self, version: u32) -> Option<&ToolVersion> {
        self.versions.iter().find(|entry| entry.version == version)
    }
    pub fn latest(&self) -> Option<&ToolVersion> {
        self.versions.last()
    }
    pub fn current(&self) -> Option<&ToolVersion> {
        self.enabled.and_then(|version| self.version(version))
    }
    /// 用户是否停用过、且之后没有重新启用；此时新版本不自动启用。
    pub fn disabled_by_owner(&self) -> bool {
        self.changes
            .last()
            .is_some_and(|change| change.actor == Actor::Owner && change.enabled.is_none())
    }
}

/// 一次检查调用；`findings` 非空表示拦下了这份草稿。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub id: String,
    pub tool: ToolRef,
    pub owner: String,
    pub run_id: String,
    pub attempt: usize,
    pub at_ms: u64,
    pub findings: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolSnapshot {
    pub tools: Vec<ForgedTool>,
    pub forges: Vec<ForgeAttempt>,
    pub calls: Vec<ToolCall>,
}
impl ToolSnapshot {
    pub fn tool(&self, id: &str) -> Option<&ForgedTool> {
        self.tools.iter().find(|tool| tool.id == id)
    }
    pub fn tools_for<'a>(&'a self, owner: &'a str) -> impl Iterator<Item = &'a ForgedTool> {
        self.tools.iter().filter(move |tool| tool.owner == owner)
    }
    pub fn forges_for<'a>(&'a self, owner: &'a str) -> impl Iterator<Item = &'a ForgeAttempt> {
        self.forges.iter().filter(move |forge| forge.owner == owner)
    }
    pub fn calls_for<'a>(&'a self, tool_id: &'a str) -> impl Iterator<Item = &'a ToolCall> {
        self.calls
            .iter()
            .filter(move |call| call.tool.tool_id == tool_id)
    }
    /// 某个用户在某个运行器下当前启用的工具版本。
    pub fn enabled_for<'a>(
        &'a self,
        owner: &'a str,
        runner_id: &'a str,
    ) -> impl Iterator<Item = (&'a ForgedTool, &'a ToolVersion)> {
        self.tools
            .iter()
            .filter(move |tool| tool.owner == owner && tool.runner_id == runner_id)
            .filter_map(|tool| tool.current().map(|version| (tool, version)))
    }
}

/// 仅可信宿主持有；不发布给模型或不受信插件。
pub trait ToolAdmin: Send + Sync {
    fn snapshot(&self) -> ToolResult<ToolSnapshot>;
    /// 原子保存 Running 锻造，成功返回后才可请求锻造器。同一缺口同一出现次数只锻造一次，
    /// 每个缺口至多 `MAX_FORGES_PER_GAP` 次，同一时间只有一次锻造；不满足时返回 None。
    fn begin_forge(
        &self,
        owner: &str,
        gap: GapRef,
        occurrences: usize,
        forger_version: &str,
        now_ms: u64,
    ) -> ToolResult<Option<ForgeAttempt>>;
    /// 保存锻造结果与回放验证。验证通过时同一提交写入工具新版本，用户没有停用时自动启用；
    /// 没有规格时不得附带验证。
    fn record_forge(
        &self,
        id: &str,
        at_ms: u64,
        result: Result<ForgeOutput, ForgeFailure>,
        verification: Option<Verification>,
    ) -> ToolResult<ForgeAttempt>;
    /// 已启用的工具回放验证即覆盖这个缺口：只记录复用，不请求锻造器；去重规则同 `begin_forge`。
    fn record_reuse(
        &self,
        owner: &str,
        gap: GapRef,
        occurrences: usize,
        tool: ToolRef,
        verification: Verification,
        now_ms: u64,
    ) -> ToolResult<Option<ForgeAttempt>>;
    /// 因停止或超时放弃进行中的锻造；只能从 Running 结束一次。
    fn abandon_forge(
        &self,
        id: &str,
        at_ms: u64,
        failure: ForgeFailure,
    ) -> ToolResult<ForgeAttempt>;
    /// 停用（None）或启用某个已验证版本；只改变启用版本并留下记录，不删除版本。
    fn set_enabled(
        &self,
        tool_id: &str,
        at_ms: u64,
        actor: Actor,
        enabled: Option<u32>,
    ) -> ToolResult<ForgedTool>;
    /// 保存一次检查调用；成功返回后才可把结果交给实践。调用的须是当时启用的版本。
    fn record_call(&self, call: ToolCall) -> ToolResult<ToolCall>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolError {
    InvalidInput,
    NotFound,
    Unavailable,
    CorruptState,
    UnsupportedVersion,
    Storage,
    LimitReached,
    Conflict,
    Forge(ForgeFailure),
}
impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "工具锻造输入无效",
            Self::NotFound => "没有这个工具或锻造记录",
            Self::Unavailable => "工具锻造服务不可用",
            Self::CorruptState => "工具锻造状态损坏；未清空",
            Self::UnsupportedVersion => "不支持该工具锻造状态版本",
            Self::Storage => "工具锻造提交无法确认；须重新打开核对",
            Self::LimitReached => "工具锻造容量已满；保留原记录",
            Self::Conflict => "工具锻造记录状态冲突",
            Self::Forge(_) => "工具锻造请求失败",
        })
    }
}
impl std::error::Error for ToolError {}

pub fn validate_id(value: &str) -> ToolResult<()> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(ToolError::InvalidInput);
    }
    Ok(())
}

pub fn validate_text(value: &str, limit: usize) -> ToolResult<()> {
    if value.len() > limit || value.contains('\0') {
        return Err(ToolError::InvalidInput);
    }
    Ok(())
}

pub fn validate_findings(findings: &[String]) -> ToolResult<()> {
    if findings.len() > MAX_FINDINGS
        || findings
            .iter()
            .any(|finding| !is_line(finding, MAX_FINDING_BYTES))
    {
        return Err(ToolError::InvalidInput);
    }
    Ok(())
}

/// 非空单行文本：无控制字符，首尾无空白。
pub fn is_line(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

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

/// 每个用户、运行器与缺口一个工具。
pub fn tool_id(owner: &str, runner_id: &str, gap_key: &str) -> String {
    format!(
        "tool-{}",
        &digest(&["toolforge.tool:v1", owner, runner_id, gap_key])[..24]
    )
}

/// 同一缺口同一出现次数只锻造一次；重复准入得到同一 ID。
pub fn forge_id(owner: &str, runner_id: &str, gap_key: &str, occurrences: usize) -> String {
    format!(
        "forge-{}",
        &digest(&[
            "toolforge.forge:v1",
            owner,
            runner_id,
            gap_key,
            &occurrences.to_string()
        ])[..32]
    )
}

/// 同一工具对同一次尝试只调用一次。
pub fn call_id(tool_id: &str, run_id: &str, attempt: usize) -> String {
    format!(
        "call-{}",
        &digest(&["toolforge.call:v1", tool_id, run_id, &attempt.to_string()])[..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, content: &str) -> ArtifactFile {
        ArtifactFile {
            path: path.into(),
            content: content.into(),
        }
    }

    fn spec(rules: Vec<CheckRule>) -> CheckSpec {
        CheckSpec {
            name: "block-type".into(),
            summary: "方块类型必须是已知类型".into(),
            message: "方块类型拼写不对".into(),
            rules,
        }
    }

    #[test]
    fn rules_read_only_matching_files() {
        let files = [
            file("mod.hjson", "name: x"),
            file("content/blocks/a.hjson", "type: Walll"),
            file("content/blocks/b.hjson", "type: Wall"),
        ];
        let forbid = spec(vec![CheckRule::ForbidText {
            pattern: "*.hjson".into(),
            text: "type: Walll".into(),
        }]);
        assert_eq!(
            evaluate(&forbid, &files),
            vec!["content/blocks/a.hjson 含有“type: Walll”"]
        );
        let require = spec(vec![CheckRule::RequireText {
            pattern: "mod.hjson".into(),
            text: "minGameVersion".into(),
        }]);
        assert_eq!(
            evaluate(&require, &files),
            vec!["mod.hjson 缺少“minGameVersion”"]
        );
        let missing = spec(vec![CheckRule::RequireFile {
            pattern: "*.json".into(),
        }]);
        assert_eq!(evaluate(&missing, &files), vec!["缺少匹配 *.json 的文件"]);
        assert!(evaluate(&missing, &[file("a.json", "{}")]).is_empty());
    }

    #[test]
    fn verification_needs_both_sides_and_no_false_positive() {
        let check = spec(vec![CheckRule::ForbidText {
            pattern: "*.hjson".into(),
            text: "Walll".into(),
        }]);
        let example = |run: &str, content: &str| ExampleFiles {
            run_id: run.into(),
            attempt: 1,
            files: vec![file("a.hjson", content)],
        };
        let failing = [example("r1", "type: Walll"), example("r2", "type: Walll")];
        let passing = [example("r3", "type: Wall")];
        assert!(verify(&check, &failing, &passing).passed());
        assert!(!verify(&check, &failing, &[]).passed());
        assert!(!verify(&check, &[], &passing).passed());
        let loose = spec(vec![CheckRule::ForbidText {
            pattern: "*.hjson".into(),
            text: "Wall".into(),
        }]);
        assert!(
            !verify(&loose, &failing, &passing).passed(),
            "误报通过的草稿"
        );
    }

    #[test]
    fn spec_and_pattern_shapes_are_bounded() {
        let ok = spec(vec![CheckRule::RequireFile {
            pattern: "*.hjson".into(),
        }]);
        assert!(ok.validate().is_ok());
        for pattern in ["*", "*../x", "/abs", "a/../b", "*a b"] {
            assert!(!is_pattern(pattern), "{pattern}");
        }
        let mut bad = ok.clone();
        bad.rules.push(bad.rules[0].clone());
        assert!(bad.validate().is_err(), "重复规则");
        let mut bad = ok.clone();
        bad.message = "两行\n说明".into();
        assert!(bad.validate().is_err());
        let mut bad = ok;
        bad.name = "Bad".into();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn owner_disable_blocks_automatic_enable() {
        let mut tool = ForgedTool {
            id: "t".into(),
            owner: "u".into(),
            runner_id: "r".into(),
            gap_key: "k".into(),
            versions: vec![],
            enabled: None,
            changes: vec![ToolChange {
                at_ms: 1,
                actor: Actor::Automatic,
                enabled: Some(1),
            }],
        };
        assert!(!tool.disabled_by_owner());
        tool.changes.push(ToolChange {
            at_ms: 2,
            actor: Actor::Owner,
            enabled: None,
        });
        assert!(tool.disabled_by_owner());
    }
}
