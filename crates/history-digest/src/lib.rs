//! 对话历史摘要：历史窗口省略较早轮次时，用有轮次来源的摘要代替直接省略。
//!
//! `HistoryDigestService` 实现运行时的 `HistoryDigests` 契约，只做查找和登记，不发起模型请求；
//! 后台通过 `generate` 调用可替换的 `HistoryDigester` 增量生成新版本。原始会话历史始终完整保存。
mod model;
mod store;

pub use model::ModelHistoryDigester;
pub use store::{DigestStore, DigestVersion, MAX_SCOPE_FILES, MAX_VERSIONS};

use eve_llm_api::{
    ChatMessage, ChatRole, ContextScope, HistoryDigest, HistoryDigests, LlmError, LlmFuture,
    MAX_HISTORY_DIGEST_BYTES, history_turns,
};
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, sync::Mutex};

/// 一次摘要至多的要点数。
pub const MAX_POINTS: usize = 12;
/// 单个要点正文上限（UTF-8 字节）。
pub const MAX_POINT_BYTES: usize = 512;
/// 单个要点至多引用的轮次数。
pub const MAX_POINT_TURNS: usize = 8;
/// 等待生成的作用域上限；满时新作用域等下一轮再登记。
pub const MAX_PENDING: usize = 64;
/// 交给摘要器的单条消息文字上限（字符）；仅限摘要输入，原文不变。
pub const MAX_TURN_TEXT_CHARS: usize = 2000;
/// 交给摘要器的单个工具结果上限（字符）。
pub const MAX_TOOL_RESULT_CHARS: usize = 400;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DigestError {
    Storage,
    Corrupt,
    UnsupportedVersion,
    InvalidOutput,
    Provider,
    Timeout,
    Cancelled,
}

impl fmt::Display for DigestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Storage => "历史摘要存储不可用",
            Self::Corrupt => "历史摘要文件损坏；已保留原文件",
            Self::UnsupportedVersion => "历史摘要文件版本不受支持；已保留原文件",
            Self::InvalidOutput => "历史摘要输出无效",
            Self::Provider => "历史摘要模型请求失败",
            Self::Timeout => "历史摘要模型请求超时",
            Self::Cancelled => "历史摘要已取消",
        })
    }
}

impl std::error::Error for DigestError {}

/// 一个摘要要点及其来源轮次（从 1 起，按会话完整轮次计）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigestPoint {
    pub text: String,
    pub turns: Vec<usize>,
}

pub fn validate_points(points: &[DigestPoint], covered_turns: usize) -> Result<(), DigestError> {
    let valid = !points.is_empty()
        && points.len() <= MAX_POINTS
        && points.iter().all(|point| {
            !point.text.trim().is_empty()
                && point.text.len() <= MAX_POINT_BYTES
                && !point.text.chars().any(|c| c.is_control())
                && !point.turns.is_empty()
                && point.turns.len() <= MAX_POINT_TURNS
                && point.turns.windows(2).all(|pair| pair[0] < pair[1])
                && point
                    .turns
                    .iter()
                    .all(|turn| (1..=covered_turns).contains(turn))
        })
        && render(points).len() <= MAX_HISTORY_DIGEST_BYTES;
    if valid {
        Ok(())
    } else {
        Err(DigestError::InvalidOutput)
    }
}

/// 把要点渲染成交给对话模型的文字，每点附来源轮次。
pub fn render(points: &[DigestPoint]) -> String {
    points
        .iter()
        .map(|point| {
            let turns: Vec<String> = point.turns.iter().map(usize::to_string).collect();
            format!("- {}（第 {} 轮）", point.text.trim(), turns.join("、"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 最早 `turns` 个完整轮次原文的 SHA-256；轮次不足时为 None。
pub fn fingerprint(messages: &[ChatMessage], turns: usize) -> Option<String> {
    let groups = history_turns(messages);
    if turns == 0 || turns > groups.len() {
        return None;
    }
    let end: usize = groups[..turns].iter().map(|turn| turn.len()).sum();
    let bytes = serde_json::to_vec(&messages[..end]).ok()?;
    Some(store::hex(digest(&SHA256, &bytes).as_ref()))
}

/// 交给摘要器的一轮内容；文字按字符截断并标明，原文不变。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigestTurn {
    pub number: usize,
    pub user: String,
    pub assistant: Vec<String>,
    pub tools: Vec<DigestTool>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigestTool {
    pub name: String,
    pub result: Option<String>,
}

/// 一次增量摘要请求：上次摘要（覆盖前 `previous_turns` 轮）加上紧随其后的若干新轮次。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigestRequest {
    pub previous_turns: usize,
    pub previous: Vec<DigestPoint>,
    pub turns: Vec<DigestTurn>,
}

impl DigestRequest {
    pub fn covered_turns(&self) -> usize {
        self.previous_turns + self.turns.len()
    }
}

/// 可替换的摘要生成器；输出须通过 `validate_points` 且只引用请求覆盖的轮次。
pub trait HistoryDigester: Send + Sync {
    fn version(&self) -> &str;
    fn digest(&self, request: DigestRequest) -> LlmFuture<'_, Vec<DigestPoint>>;
}

/// 每次登记时读取的设置；`None` 表示关闭（不提供也不登记摘要）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DigestOptions {
    /// 被省略且尚未覆盖的轮次达到此数才登记生成。
    pub min_uncovered_turns: usize,
    /// 单次请求新轮次的 JSON 字节预算；至少包含一轮。
    pub max_input_bytes: usize,
}

pub trait DigestPolicy: Send + Sync {
    fn options(&self) -> Result<Option<DigestOptions>, LlmError>;
}

/// 等待后台生成的作用域及其被省略的轮次原文。
#[derive(Clone, Debug)]
pub struct PendingDigest {
    pub scope: ContextScope,
    pub omitted: Vec<ChatMessage>,
    pub base: Option<DigestVersion>,
}

pub struct HistoryDigestService {
    store: DigestStore,
    policy: Box<dyn DigestPolicy>,
    pending: Mutex<BTreeMap<(String, String), PendingDigest>>,
}

impl HistoryDigestService {
    pub fn new(store: DigestStore, policy: Box<dyn DigestPolicy>) -> Self {
        Self {
            store,
            policy,
            pending: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn store(&self) -> &DigestStore {
        &self.store
    }

    /// 取出一个待生成作用域；没有时为 None。
    pub fn take_pending(&self) -> Option<PendingDigest> {
        let mut pending = self.pending.lock().ok()?;
        let key = pending.keys().next()?.clone();
        pending.remove(&key)
    }

    pub fn pending_count(&self) -> usize {
        self.pending.lock().map_or(0, |pending| pending.len())
    }

    fn enqueue(&self, entry: PendingDigest) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        let key = (entry.scope.session_id.clone(), entry.scope.user_id.clone());
        if pending.len() < MAX_PENDING || pending.contains_key(&key) {
            pending.insert(key, entry);
        }
    }

    /// 与原文指纹一致、且不超出被省略轮次的覆盖最多的版本。
    fn best(
        &self,
        scope: &ContextScope,
        omitted: &[ChatMessage],
    ) -> Result<Option<DigestVersion>, DigestError> {
        let turns = history_turns(omitted).len();
        Ok(self
            .store
            .versions(scope)?
            .into_iter()
            .rev()
            .find(|version| {
                version.covered_turns <= turns
                    && fingerprint(omitted, version.covered_turns).as_deref()
                        == Some(version.fingerprint.as_str())
            }))
    }
}

impl HistoryDigests for HistoryDigestService {
    fn digest(
        &self,
        scope: Option<&ContextScope>,
        omitted: &[ChatMessage],
    ) -> Result<Option<HistoryDigest>, LlmError> {
        let Some(scope) = scope else {
            return Ok(None);
        };
        let Some(options) = self.policy.options()? else {
            return Ok(None);
        };
        if omitted
            .first()
            .is_none_or(|message| message.role != ChatRole::User)
        {
            return Ok(None);
        }
        let best = self
            .best(scope, omitted)
            .map_err(|error| LlmError::Context(error.to_string()))?;
        let covered = best.as_ref().map_or(0, |version| version.covered_turns);
        if history_turns(omitted).len() - covered >= options.min_uncovered_turns.max(1) {
            self.enqueue(PendingDigest {
                scope: scope.clone(),
                omitted: omitted.to_vec(),
                base: best.clone(),
            });
        }
        Ok(best.map(|version| HistoryDigest {
            covered_turns: version.covered_turns,
            text: render(&version.points),
        }))
    }
}

fn clip(text: &str, limit: usize) -> (String, bool) {
    let mut chars = text.chars();
    let clipped: String = chars.by_ref().take(limit).collect();
    (clipped, chars.next().is_some())
}

/// 把一轮原文整理成摘要器输入。
pub fn digest_turn(number: usize, messages: &[ChatMessage]) -> DigestTurn {
    let mut truncated = false;
    let mut text = |value: &str, limit| {
        let (value, cut) = clip(value, limit);
        truncated |= cut;
        value
    };
    let mut turn = DigestTurn {
        number,
        user: String::new(),
        assistant: vec![],
        tools: vec![],
        truncated: false,
    };
    let mut names = BTreeMap::new();
    for message in messages {
        match message.role {
            ChatRole::User => {
                turn.user = text(message.text.as_deref().unwrap_or(""), MAX_TURN_TEXT_CHARS)
            }
            ChatRole::Assistant => {
                if let Some(value) = message.text.as_deref().filter(|v| !v.trim().is_empty()) {
                    turn.assistant.push(text(value, MAX_TURN_TEXT_CHARS));
                }
                for call in &message.tool_calls {
                    names.insert(call.id.clone(), turn.tools.len());
                    turn.tools.push(DigestTool {
                        name: call.name.clone(),
                        result: None,
                    });
                }
            }
            ChatRole::Tool => {
                for result in &message.tool_results {
                    if let Some(&index) = names.get(&result.call_id) {
                        let output = serde_json::to_string(&result.output).unwrap_or_default();
                        turn.tools[index].result = Some(text(&output, MAX_TOOL_RESULT_CHARS));
                    }
                }
            }
            ChatRole::System => {}
        }
    }
    turn.truncated = truncated;
    turn
}

/// 根据待生成项构造下一次增量请求；没有可摘要的新轮次时为 None。
pub fn plan(pending: &PendingDigest, options: DigestOptions) -> Option<DigestRequest> {
    let groups = history_turns(&pending.omitted);
    let (previous_turns, previous) = match &pending.base {
        Some(base) => (base.covered_turns, base.points.clone()),
        None => (0, vec![]),
    };
    let mut request = DigestRequest {
        previous_turns,
        previous,
        turns: vec![],
    };
    let mut bytes = 0usize;
    for (index, messages) in groups.iter().enumerate().skip(previous_turns) {
        let turn = digest_turn(index + 1, messages);
        let size = serde_json::to_vec(&turn).map_or(usize::MAX, |encoded| encoded.len());
        if !request.turns.is_empty() && size > options.max_input_bytes.saturating_sub(bytes) {
            break;
        }
        bytes = bytes.saturating_add(size);
        request.turns.push(turn);
    }
    (!request.turns.is_empty()).then_some(request)
}

/// 生成并保存一个新版本；返回保存的版本。任何失败都不改动已有文件。
pub async fn generate(
    service: &HistoryDigestService,
    digester: &dyn HistoryDigester,
    pending: &PendingDigest,
    options: DigestOptions,
    now_ms: u64,
) -> Result<Option<DigestVersion>, DigestError> {
    let Some(request) = plan(pending, options) else {
        return Ok(None);
    };
    let covered = request.covered_turns();
    let fingerprint = fingerprint(&pending.omitted, covered).ok_or(DigestError::InvalidOutput)?;
    let points = digester
        .digest(request)
        .await
        .map_err(|error| match error {
            LlmError::ProviderTimeout => DigestError::Timeout,
            LlmError::Cancelled => DigestError::Cancelled,
            LlmError::Protocol(_) => DigestError::InvalidOutput,
            _ => DigestError::Provider,
        })?;
    validate_points(&points, covered)?;
    let version = DigestVersion {
        covered_turns: covered,
        fingerprint,
        points,
        generator: digester.version().to_owned(),
        created_at_ms: now_ms.max(1),
    };
    service.store.put(&pending.scope, version.clone())?;
    Ok(Some(version))
}
