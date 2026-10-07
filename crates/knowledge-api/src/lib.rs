//! 受控研究与领域知识的来源契约。
//!
//! 知识属于 Eve 自身，与用户偏好分开保存。每条知识要么附带能在已抓取网页正文中逐字核对的
//! 引用（`SourceQuoted`：只说明来源原文这样写，不代表已经实践验证），要么明确标为未验证假设。
//! 研究只访问操作者配置的入口页面所在范围；网页正文始终是数据，不授予任何执行能力。
//! 账本、抓取器、来源选择器与知识提炼器均可替换，宿主负责绑定目标与可见范围。
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt, future::Future, pin::Pin};
use url::Url;

pub const KNOWLEDGE_PLUGIN_ID: &str = "eve.knowledge";
pub const KNOWLEDGE_STATE_KEY: &str = "knowledge.v1";
/// 操作者配置的入口页面数量上限。
pub const MAX_SEEDS: usize = 8;
pub const MAX_URL_BYTES: usize = 2048;
/// 单个页面保留的站内链接上限。
pub const MAX_PAGE_LINKS: usize = 64;
/// 一次研究交给来源选择器的候选链接上限。
pub const MAX_CANDIDATES: usize = 40;
/// 一次研究最多抓取并提炼的候选页面数。
pub const MAX_SELECTED: usize = 3;
/// 单次响应读取的原始字节上限；超过即放弃，不保存截断的原始内容。
pub const MAX_FETCH_BYTES: u64 = 1024 * 1024;
pub const MAX_REDIRECTS: usize = 3;
/// 保存并交给提炼器的正文上限；更长的页面标记为截断。
pub const MAX_DOCUMENT_TEXT_BYTES: usize = 8 * 1024;
pub const MAX_BRIEF_BYTES: usize = 4096;
/// 链接文字、页面标题与内容类型的上限。
pub const MAX_LABEL_BYTES: usize = 256;
pub const MAX_CLAIMS: usize = 12;
pub const MAX_HYPOTHESES: usize = 4;
pub const MAX_STATEMENT_BYTES: usize = 512;
pub const MAX_QUOTE_BYTES: usize = 512;
/// 引用至少包含的非空白字符数，避免用过短片段冒充来源。
pub const MIN_QUOTE_CHARS: usize = 8;
pub const MAX_VERSION_BYTES: usize = 64;
pub const MAX_SELECTION_OUTPUT_BYTES: usize = 2048;
pub const MAX_EXTRACTION_OUTPUT_BYTES: usize = 16 * 1024;
/// 研究记录总数，不自动淘汰；达到容量后保留原记录并停止新研究。
pub const MAX_RUNS: usize = 64;
pub const MAX_DOCUMENTS: usize = 96;
pub const MAX_ENTRIES: usize = 256;
/// 同一目标在所有修订中的研究次数上限；重启不会重置。
pub const MAX_RUNS_PER_GOAL: usize = 3;
pub const MAX_STATE_BYTES: usize = 4 * 1024 * 1024;

pub type KnowledgeResult<T> = Result<T, KnowledgeError>;
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type KnowledgeFuture<'a, T> = BoxFuture<'a, KnowledgeResult<T>>;

/// 操作者显式配置的入口页面。允许访问的范围是每个入口页面同源下的同一目录及其子路径；
/// 模型只能从这些范围内已发现的链接中选择，不能引入新的主机。
#[derive(Clone, Eq, PartialEq)]
pub struct SourcePolicy {
    seeds: Vec<String>,
}
impl SourcePolicy {
    /// 规范化并校验入口页面；不发起任何网络请求。
    pub fn new(seeds: &[String]) -> KnowledgeResult<Self> {
        if seeds.is_empty() || seeds.len() > MAX_SEEDS {
            return Err(KnowledgeError::InvalidInput);
        }
        let mut normalized = Vec::with_capacity(seeds.len());
        for seed in seeds {
            let url = parse_web_url(seed).ok_or(KnowledgeError::InvalidInput)?;
            let text = String::from(url);
            if text.len() > MAX_URL_BYTES || normalized.contains(&text) {
                return Err(KnowledgeError::InvalidInput);
            }
            normalized.push(text);
        }
        Ok(Self { seeds: normalized })
    }
    pub fn seeds(&self) -> &[String] {
        &self.seeds
    }
    pub fn allows(&self, url: &str) -> bool {
        url_allowed(&self.seeds, url)
    }
}

/// 只接受规范形式的 http/https URL：有主机、无用户信息与片段。
pub fn canonical_url(text: &str) -> Option<Url> {
    if text.len() > MAX_URL_BYTES {
        return None;
    }
    let url = parse_web_url(text)?;
    (url.as_str() == text).then_some(url)
}

fn parse_web_url(text: &str) -> Option<Url> {
    if text.is_empty() || text.len() > MAX_URL_BYTES || text.chars().any(char::is_whitespace) {
        return None;
    }
    let url = Url::parse(text).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none_or(str::is_empty)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    Some(url)
}

/// URL 是否落在某个入口页面的同源目录范围内；入口页面本身也允许。
pub fn url_allowed(seeds: &[String], url: &str) -> bool {
    let Some(target) = canonical_url(url) else {
        return false;
    };
    seeds.iter().any(|seed| {
        let Some(seed) = canonical_url(seed) else {
            return false;
        };
        let directory = match seed.path().rfind('/') {
            Some(end) => &seed.path()[..=end],
            None => "/",
        };
        seed.scheme() == target.scheme()
            && seed.host_str() == target.host_str()
            && seed.port_or_known_default() == target.port_or_known_default()
            && target.path().starts_with(directory)
    })
}

/// 由宿主从目标派生的研究主题。brief 是目标描述，可能含用户原话；只交给模型，
/// 不发送给任何网页。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResearchTopic {
    pub goal_id: String,
    pub goal_revision: u64,
    /// 目标可见的用户；知识只向该用户展示。
    pub owner: String,
    pub brief: String,
    pub brief_truncated: bool,
}
impl ResearchTopic {
    pub fn validate(&self) -> KnowledgeResult<()> {
        validate_id(&self.goal_id)?;
        validate_id(&self.owner)?;
        validate_text(&self.brief, MAX_BRIEF_BYTES)?;
        if self.goal_revision == 0 {
            return Err(KnowledgeError::InvalidInput);
        }
        Ok(())
    }
}

/// 抓取失败的原因；失败随研究记录保存，不重试。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FetchFailure {
    /// URL 或重定向目标不在允许范围内。
    NotAllowed,
    HttpStatus(u16),
    TooLarge,
    UnsupportedType,
    /// 页面没有可读正文。
    Empty,
    TooManyRedirects,
    Timeout,
    Network,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageLink {
    pub url: String,
    pub text: String,
}

/// 抓取器返回的一次实际 GET 结果；正文已转为纯文本并按上限截断。
#[derive(Clone, Eq, PartialEq)]
pub struct FetchedPage {
    /// 允许范围内跟随重定向后的最终 URL。
    pub final_url: String,
    pub content_type: String,
    /// 完整响应字节的 SHA-256，小写十六进制。
    pub sha256: String,
    pub byte_count: u64,
    pub title: String,
    pub text: String,
    pub text_truncated: bool,
    /// 页面中位于允许范围内的链接，按出现顺序去重。
    pub links: Vec<PageLink>,
}
impl FetchedPage {
    pub fn validate(&self, policy: &SourcePolicy) -> KnowledgeResult<()> {
        validate_source_fields(
            &self.final_url,
            &self.content_type,
            &self.sha256,
            self.byte_count,
            &self.title,
            &self.text,
        )?;
        if !policy.allows(&self.final_url) || self.links.len() > MAX_PAGE_LINKS {
            return Err(KnowledgeError::InvalidInput);
        }
        let mut seen = BTreeSet::new();
        for link in &self.links {
            validate_label(&link.text)?;
            if !policy.allows(&link.url) || !seen.insert(link.url.as_str()) {
                return Err(KnowledgeError::InvalidInput);
            }
        }
        Ok(())
    }
}

/// 可替换抓取器：只读 GET，不携带凭据或 Cookie，不执行脚本。
pub trait SourceFetcher: Send + Sync {
    fn fetch<'a>(
        &'a self,
        policy: &'a SourcePolicy,
        url: &'a str,
    ) -> BoxFuture<'a, Result<FetchedPage, FetchFailure>>;
}

/// 已保存的来源文档；同一最终 URL 与内容只保存一次，fetched_at_ms 是首次抓到该内容的时间。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceDocument {
    pub id: String,
    pub url: String,
    pub fetched_at_ms: u64,
    pub content_type: String,
    pub sha256: String,
    pub byte_count: u64,
    pub title: String,
    pub text: String,
    pub text_truncated: bool,
}
impl SourceDocument {
    pub fn validate(&self) -> KnowledgeResult<()> {
        validate_source_fields(
            &self.url,
            &self.content_type,
            &self.sha256,
            self.byte_count,
            &self.title,
            &self.text,
        )?;
        if self.fetched_at_ms == 0 || self.id != document_id(&self.url, &self.sha256) {
            return Err(KnowledgeError::InvalidInput);
        }
        Ok(())
    }
}

/// 来源文档的稳定标识；长度前缀避免字段边界歧义。
pub fn document_id(url: &str, sha256: &str) -> String {
    format!("source-{}", digest(&["knowledge.source:v1", url, sha256]))
}

/// 同一目标修订的研究标识；重复准入得到同一 ID。
pub fn run_id(goal_id: &str, goal_revision: u64) -> String {
    format!(
        "research-{}",
        &digest(&["knowledge.run:v1", goal_id, &goal_revision.to_string()])[..32]
    )
}

/// 有来源结论的稳定标识：同一目标、同一来源文档的同一引用只保存一次。
pub fn claim_entry_id(goal_id: &str, document_id: &str, quote: &str) -> String {
    format!(
        "knowledge-{}",
        &digest(&["knowledge.claim:v1", goal_id, document_id, &compact(quote)])[..32]
    )
}

/// 假设的稳定标识：同一目标的相同假设只保存一次。
pub fn hypothesis_entry_id(goal_id: &str, statement: &str) -> String {
    format!(
        "knowledge-{}",
        &digest(&["knowledge.hypothesis:v1", goal_id, &compact(statement)])[..32]
    )
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum ResearchStage {
    /// 已保存准入，正在抓取入口页面。
    Discovering,
    /// 已保存入口抓取与候选链接，正在请求来源选择。
    Selecting,
    /// 已保存选择结果，正在抓取所选页面。
    Fetching,
    /// 已保存所选页面，正在请求知识提炼。
    Extracting,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ResearchFailure {
    /// 入口或所选页面全部抓取失败。
    Fetch,
    Provider,
    InvalidOutput,
    Timeout,
    Cancelled,
    /// 来源文档容量已满，无法保存本次抓取；保留原记录，不淘汰。
    LimitReached,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RunStatus {
    Running,
    Completed,
    Failed(ResearchFailure),
    /// 进程在 Running 时退出；重启后记为中断，不重放。
    Interrupted,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum FetchOutcome {
    Fetched { document_id: String },
    Failed { failure: FetchFailure },
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchRecord {
    pub url: String,
    pub at_ms: u64,
    pub outcome: FetchOutcome,
}

/// 交给来源选择器的候选链接；只来自已抓取入口页面中的允许范围链接。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkCandidate {
    pub url: String,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimKind {
    /// 关于领域的事实性说明。
    Fact,
    /// 操作步骤或做法。
    Procedure,
    /// 适用版本、兼容性或变更说明。
    Version,
}

/// 提炼器给出的一条有来源结论；quote 必须是所引文档正文的连续片段。
/// statement 是模型对引用的概括，只有引用本身可由宿主核对。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimDraft {
    pub document_id: String,
    pub kind: ClaimKind,
    pub statement: String,
    pub quote: String,
    /// 适用版本；必须在所引文档正文中逐字出现，未提及时为 null。
    #[serde(default)]
    pub version: Option<String>,
}

/// 没有来源支持、需要后续实践验证的推测。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HypothesisDraft {
    pub statement: String,
}

#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractionOutput {
    pub claims: Vec<ClaimDraft>,
    pub hypotheses: Vec<HypothesisDraft>,
}
impl ExtractionOutput {
    pub fn is_empty(&self) -> bool {
        self.claims.is_empty() && self.hypotheses.is_empty()
    }
}

/// 校验提炼输出；只做纯检查。documents 是 (文档 ID, 正文)。
///
/// 每条结论的引用必须是所引文档正文的连续片段（忽略空白差异），版本须在正文中逐字出现；
/// 同一文档的同一引用、相同的假设都只能出现一次。
pub fn validate_extraction(
    documents: &[(&str, &str)],
    output: &ExtractionOutput,
) -> KnowledgeResult<()> {
    let invalid = || KnowledgeError::InvalidInput;
    if output.claims.len() > MAX_CLAIMS || output.hypotheses.len() > MAX_HYPOTHESES {
        return Err(invalid());
    }
    let mut quotes = BTreeSet::new();
    for claim in &output.claims {
        validate_text(&claim.statement, MAX_STATEMENT_BYTES)?;
        validate_text(&claim.quote, MAX_QUOTE_BYTES)?;
        let text = documents
            .iter()
            .find(|(id, _)| *id == claim.document_id)
            .map(|(_, text)| *text)
            .ok_or_else(invalid)?;
        let quote = compact(&claim.quote);
        if quote.chars().count() < MIN_QUOTE_CHARS
            || !compact(text).contains(&quote)
            || !quotes.insert((claim.document_id.as_str(), quote))
        {
            return Err(invalid());
        }
        if let Some(version) = &claim.version {
            validate_version(version)?;
            if !compact(text).contains(&compact(version)) {
                return Err(invalid());
            }
        }
    }
    let mut hypotheses = BTreeSet::new();
    for hypothesis in &output.hypotheses {
        validate_text(&hypothesis.statement, MAX_STATEMENT_BYTES)?;
        if !hypotheses.insert(compact(&hypothesis.statement)) {
            return Err(invalid());
        }
    }
    Ok(())
}

/// 忽略空白差异的比较形式；不做同义改写或模糊匹配。
pub fn compact(text: &str) -> String {
    text.chars()
        .filter(|value| !value.is_whitespace())
        .collect()
}

pub fn validate_version(version: &str) -> KnowledgeResult<()> {
    if version.trim() != version
        || version.is_empty()
        || version.len() > MAX_VERSION_BYTES
        || version.chars().any(char::is_control)
    {
        return Err(KnowledgeError::InvalidInput);
    }
    Ok(())
}

/// 一次研究的完整记录；先保存再执行对应阶段的外部动作，中断后不重放。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResearchRun {
    pub id: String,
    pub topic: ResearchTopic,
    pub researcher_version: String,
    /// 准入时生效的入口页面；本次研究的所有 URL 都须在其范围内。
    pub seeds: Vec<String>,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub stage: ResearchStage,
    pub status: RunStatus,
    /// 与 seeds 一一对应的入口抓取记录。
    pub discovery: Vec<FetchRecord>,
    pub candidates: Vec<LinkCandidate>,
    /// 选择器选中的候选下标。
    pub selected: Vec<usize>,
    /// 与 selected 一一对应的页面抓取记录。
    pub fetches: Vec<FetchRecord>,
    /// 只有 Completed 研究保存合规输出。
    pub output: Option<ExtractionOutput>,
    /// 先结论后假设，与输出逐项对应。
    pub results: Vec<EntryResult>,
}

/// 合规结论落入账本的结果；未能保存的原因随研究记录保存，不静默丢弃。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum EntryResult {
    Created {
        entry_id: String,
    },
    /// 同一目标已有同来源同引用的结论，或相同的假设。
    Duplicate {
        entry_id: String,
    },
    /// 知识条目总数已达上限。
    Rejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum KnowledgeKind {
    Fact,
    Procedure,
    Version,
    Hypothesis,
}
impl From<ClaimKind> for KnowledgeKind {
    fn from(kind: ClaimKind) -> Self {
        match kind {
            ClaimKind::Fact => Self::Fact,
            ClaimKind::Procedure => Self::Procedure,
            ClaimKind::Version => Self::Version,
        }
    }
}

/// 核实状态。SourceQuoted 只说明来源原文这样写；实践验证由后续切片的独立证据记录。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum KnowledgeStatus {
    SourceQuoted,
    Unverified,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeSource {
    pub document_id: String,
    pub quote: String,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeEntry {
    pub id: String,
    pub goal_id: String,
    pub owner: String,
    pub run_id: String,
    pub kind: KnowledgeKind,
    pub status: KnowledgeStatus,
    pub statement: String,
    pub version: Option<String>,
    pub source: Option<KnowledgeSource>,
    pub created_at_ms: u64,
}
impl KnowledgeEntry {
    pub fn validate(&self) -> KnowledgeResult<()> {
        for id in [&self.id, &self.goal_id, &self.owner, &self.run_id] {
            validate_id(id)?;
        }
        validate_text(&self.statement, MAX_STATEMENT_BYTES)?;
        let hypothesis = self.kind == KnowledgeKind::Hypothesis;
        if self.created_at_ms == 0
            || hypothesis != (self.status == KnowledgeStatus::Unverified)
            || hypothesis != self.source.is_none()
            || (hypothesis && self.version.is_some())
        {
            return Err(KnowledgeError::InvalidInput);
        }
        if let Some(version) = &self.version {
            validate_version(version)?;
        }
        if let Some(source) = &self.source {
            validate_id(&source.document_id)?;
            validate_text(&source.quote, MAX_QUOTE_BYTES)?;
        }
        Ok(())
    }
}

/// 一次抓取尝试；Ok 结果须通过 FetchedPage::validate。
pub struct FetchAttempt {
    pub url: String,
    pub at_ms: u64,
    pub result: Result<FetchedPage, FetchFailure>,
}

/// 研究阶段推进；每一步先保存，返回后才可执行下一阶段的外部动作。
pub enum ResearchProgress {
    /// Discovering → Selecting：保存与 seeds 一一对应的入口抓取及候选链接。
    Discovered {
        attempts: Vec<FetchAttempt>,
        candidates: Vec<LinkCandidate>,
    },
    /// Selecting → Fetching：保存选择器选中的候选下标。
    Selected { indices: Vec<usize> },
    /// Fetching → Extracting：保存与所选候选一一对应的页面抓取。
    Fetched { attempts: Vec<FetchAttempt> },
}

pub enum ResearchOutcome {
    /// 合规提炼结果；空输出表示没有找到相关来源或没有可引用的内容。
    Completed(ExtractionOutput),
    Failed(ResearchFailure),
}

#[derive(Clone, Eq, PartialEq)]
pub struct KnowledgeSnapshot {
    pub runs: Vec<ResearchRun>,
    pub documents: Vec<SourceDocument>,
    pub entries: Vec<KnowledgeEntry>,
}
impl KnowledgeSnapshot {
    pub fn document(&self, id: &str) -> Option<&SourceDocument> {
        self.documents.iter().find(|document| document.id == id)
    }
    /// 某个目标的知识，按保存顺序。
    pub fn entries_for<'a>(&'a self, goal_id: &'a str) -> impl Iterator<Item = &'a KnowledgeEntry> {
        self.entries
            .iter()
            .filter(move |entry| entry.goal_id == goal_id)
    }
    pub fn runs_for<'a>(&'a self, goal_id: &'a str) -> impl Iterator<Item = &'a ResearchRun> {
        self.runs
            .iter()
            .filter(move |run| run.topic.goal_id == goal_id)
    }
}

/// 仅可信宿主持有；不发布给模型或不受信插件。
pub trait KnowledgeAdmin: Send + Sync {
    fn snapshot(&self) -> KnowledgeResult<KnowledgeSnapshot>;
    /// 原子保存 Running 研究，成功返回后才可抓取入口页面。同一目标修订已有研究、
    /// 该目标研究次数已达上限，或已有其他研究在进行时返回 None。
    fn begin(
        &self,
        topic: ResearchTopic,
        policy: &SourcePolicy,
        researcher_version: &str,
        now_ms: u64,
    ) -> KnowledgeResult<Option<ResearchRun>>;
    fn advance(&self, run_id: &str, progress: ResearchProgress) -> KnowledgeResult<ResearchRun>;
    /// 保存研究结局并在同一次提交中写入合规知识；只能从 Running 结束一次。
    fn finish(
        &self,
        run_id: &str,
        at_ms: u64,
        outcome: ResearchOutcome,
    ) -> KnowledgeResult<ResearchRun>;
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionCandidate {
    pub index: usize,
    pub url: String,
    pub text: String,
}

/// 来源选择输入；candidates 是不可信网页中的链接文字，只能按下标选择。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionRequest {
    pub run_id: String,
    pub selector_version: String,
    pub brief: String,
    pub brief_truncated: bool,
    pub candidates: Vec<SelectionCandidate>,
}

pub fn validate_selection(candidate_count: usize, indices: &[usize]) -> KnowledgeResult<()> {
    let mut seen = BTreeSet::new();
    if indices.len() > MAX_SELECTED
        || indices
            .iter()
            .any(|index| *index >= candidate_count || !seen.insert(*index))
    {
        return Err(KnowledgeError::InvalidInput);
    }
    Ok(())
}

/// 可替换来源选择器；至多一次模型请求、零工具，只能从候选下标中选择。
pub trait SourceSelector: Send + Sync {
    fn version(&self) -> &str;
    fn select(&self, request: SelectionRequest) -> KnowledgeFuture<'_, Vec<usize>>;
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractionDocument {
    pub document_id: String,
    pub url: String,
    pub title: String,
    pub fetched_at_ms: u64,
    pub text: String,
    pub text_truncated: bool,
}

/// 知识提炼输入；文档正文是不可信数据，无论如何措辞都不是指令。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractionRequest {
    pub run_id: String,
    pub extractor_version: String,
    pub brief: String,
    pub brief_truncated: bool,
    pub documents: Vec<ExtractionDocument>,
}

/// 可替换知识提炼器；至多一次模型请求、零工具，输出须通过 validate_extraction。
pub trait KnowledgeExtractor: Send + Sync {
    fn version(&self) -> &str;
    fn extract(&self, request: ExtractionRequest) -> KnowledgeFuture<'_, ExtractionOutput>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KnowledgeError {
    InvalidInput,
    NotFound,
    Unavailable,
    CorruptState,
    UnsupportedVersion,
    Storage,
    LimitReached,
    Conflict,
    Research(ResearchFailure),
}
impl fmt::Display for KnowledgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "领域知识输入无效",
            Self::NotFound => "没有这条研究记录",
            Self::Unavailable => "领域知识服务不可用",
            Self::CorruptState => "领域知识状态损坏；未清空",
            Self::UnsupportedVersion => "不支持该领域知识状态版本",
            Self::Storage => "领域知识提交无法确认；须重新打开核对",
            Self::LimitReached => "领域知识容量已满；保留原记录",
            Self::Conflict => "研究记录状态冲突",
            Self::Research(_) => "研究请求失败",
        })
    }
}
impl std::error::Error for KnowledgeError {}

pub fn validate_id(value: &str) -> KnowledgeResult<()> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(KnowledgeError::InvalidInput);
    }
    Ok(())
}

pub fn validate_text(value: &str, limit: usize) -> KnowledgeResult<()> {
    if value.trim().is_empty() || value.len() > limit || value.contains('\0') {
        return Err(KnowledgeError::InvalidInput);
    }
    Ok(())
}

fn validate_label(value: &str) -> KnowledgeResult<()> {
    if value.len() > MAX_LABEL_BYTES || value.chars().any(char::is_control) {
        return Err(KnowledgeError::InvalidInput);
    }
    Ok(())
}

fn validate_source_fields(
    url: &str,
    content_type: &str,
    sha256: &str,
    byte_count: u64,
    title: &str,
    text: &str,
) -> KnowledgeResult<()> {
    validate_label(title)?;
    validate_label(content_type)?;
    validate_text(text, MAX_DOCUMENT_TEXT_BYTES)?;
    if canonical_url(url).is_none()
        || content_type.is_empty()
        || !is_sha256(sha256)
        || byte_count == 0
        || byte_count > MAX_FETCH_BYTES
    {
        return Err(KnowledgeError::InvalidInput);
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
    SourcePolicy,
    ResearchTopic,
    PageLink,
    FetchedPage,
    SourceDocument,
    FetchOutcome,
    FetchRecord,
    LinkCandidate,
    ClaimDraft,
    HypothesisDraft,
    ExtractionOutput,
    ResearchRun,
    EntryResult,
    KnowledgeSource,
    KnowledgeEntry,
    FetchAttempt,
    ResearchProgress,
    ResearchOutcome,
    KnowledgeSnapshot,
    SelectionCandidate,
    SelectionRequest,
    ExtractionDocument,
    ExtractionRequest,
);

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> SourcePolicy {
        SourcePolicy::new(&["https://docs.example/wiki/modding/index.html".into()]).unwrap()
    }

    #[test]
    fn policy_normalizes_seeds_and_limits_scope_to_the_seed_directory() {
        let policy = SourcePolicy::new(&["HTTPS://Docs.Example".into()]).unwrap();
        assert_eq!(policy.seeds(), ["https://docs.example/"]);
        let policy = self::policy();
        assert!(policy.allows("https://docs.example/wiki/modding/index.html"));
        assert!(policy.allows("https://docs.example/wiki/modding/blocks/wall.html"));
        for denied in [
            "https://docs.example/wiki/other.html",
            "http://docs.example/wiki/modding/a.html",
            "https://docs.example:8443/wiki/modding/a.html",
            "https://evil.example/wiki/modding/a.html",
            "https://user@docs.example/wiki/modding/a.html",
            "https://docs.example/wiki/modding/a.html#part",
            "https://docs.example/wiki/modding/../secret",
            "file:///wiki/modding/a.html",
            "https://docs.example/wiki/modding/a b.html",
        ] {
            assert!(!policy.allows(denied), "{denied}");
        }
        for invalid in [
            vec![],
            vec!["ftp://docs.example/".into()],
            vec!["https://docs.example/#x".into()],
            vec![
                "https://docs.example/".into(),
                "https://DOCS.example/".into(),
            ],
            vec!["https://docs.example/".into(); MAX_SEEDS + 1],
        ] {
            assert!(SourcePolicy::new(&invalid).is_err());
        }
    }

    #[test]
    fn extraction_requires_verbatim_quotes_and_versions_from_the_cited_document() {
        let text = "Mods are loaded from the mods folder.\nEach mod needs a mod.hjson file. Requires game version 146.";
        let documents = [("doc-a", text)];
        let claim = |quote: &str, version: Option<&str>| ClaimDraft {
            document_id: "doc-a".into(),
            kind: ClaimKind::Fact,
            statement: "模组需要 mod.hjson".into(),
            quote: quote.into(),
            version: version.map(Into::into),
        };
        let output = |claims| ExtractionOutput {
            claims,
            hypotheses: vec![],
        };
        assert!(
            validate_extraction(
                &documents,
                &output(vec![claim(
                    "Each mod needs   a mod.hjson file.",
                    Some("146")
                )])
            )
            .is_ok()
        );
        for bad in [
            claim("Each mod needs a mod.json file.", None),
            claim("mods", None),
            claim("Each mod needs a mod.hjson file.", Some("147")),
            claim("Each mod needs a mod.hjson file.", Some(" 146")),
            ClaimDraft {
                document_id: "doc-b".into(),
                ..claim("Each mod needs a mod.hjson file.", None)
            },
        ] {
            assert!(validate_extraction(&documents, &output(vec![bad])).is_err());
        }
        let duplicate = claim("Each mod needs a mod.hjson file.", None);
        assert!(
            validate_extraction(&documents, &output(vec![duplicate.clone(), duplicate])).is_err()
        );
        let hypothesis = HypothesisDraft {
            statement: "可能需要先安装 Java".into(),
        };
        assert!(
            validate_extraction(
                &documents,
                &ExtractionOutput {
                    claims: vec![],
                    hypotheses: vec![hypothesis.clone(), hypothesis]
                }
            )
            .is_err()
        );
    }

    #[test]
    fn identifiers_are_stable_and_selection_is_bounded() {
        assert_eq!(run_id("goal", 1), run_id("goal", 1));
        assert_ne!(run_id("goal", 1), run_id("goal", 2));
        assert_ne!(
            document_id("https://a/", "x"),
            document_id("https://a/x", "")
        );
        assert!(validate_selection(3, &[2, 0]).is_ok());
        assert!(validate_selection(3, &[3]).is_err());
        assert!(validate_selection(3, &[1, 1]).is_err());
        assert!(validate_selection(9, &[0, 1, 2, 3]).is_err());
    }

    #[test]
    fn entries_keep_hypotheses_and_quoted_claims_apart() {
        let entry = KnowledgeEntry {
            id: "k".into(),
            goal_id: "g".into(),
            owner: "u".into(),
            run_id: "r".into(),
            kind: KnowledgeKind::Fact,
            status: KnowledgeStatus::SourceQuoted,
            statement: "s".into(),
            version: Some("146".into()),
            source: Some(KnowledgeSource {
                document_id: "d".into(),
                quote: "quote text".into(),
            }),
            created_at_ms: 1,
        };
        assert!(entry.validate().is_ok());
        let hypothesis = KnowledgeEntry {
            kind: KnowledgeKind::Hypothesis,
            status: KnowledgeStatus::Unverified,
            version: None,
            source: None,
            ..entry.clone()
        };
        assert!(hypothesis.validate().is_ok());
        for invalid in [
            KnowledgeEntry {
                status: KnowledgeStatus::Unverified,
                ..entry.clone()
            },
            KnowledgeEntry {
                source: None,
                ..entry.clone()
            },
            KnowledgeEntry {
                version: Some("146".into()),
                ..hypothesis.clone()
            },
        ] {
            assert!(invalid.validate().is_err());
        }
    }
}
