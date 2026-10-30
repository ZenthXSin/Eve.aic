use eve_memory_api::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

/// 可替换的确定性词法召回实现；工厂只能由可信宿主持有。
///
/// 读取句柄在构造时绑定原始 `MemoryService`，不随插件重启重新取服务。
/// 召回只读当前快照，不调用模型，也不更新记忆或偏好。
pub struct LexicalMemoryRecall {
    memory: Arc<dyn MemoryAdmin>,
}

impl LexicalMemoryRecall {
    pub fn new(memory: Arc<dyn MemoryAdmin>) -> Self {
        Self { memory }
    }
}

impl MemoryRecallFactory for LexicalMemoryRecall {
    fn reader(&self, scope: MemoryScope) -> MemoryResult<Arc<dyn MemoryRecallService>> {
        scope.validate()?;
        let reader = self.memory.reader(scope.clone())?;
        Ok(Arc::new(ScopedLexicalRecall::new(scope, reader)?))
    }
}

/// 已绑定可信作用域的只读召回；每次调用都会校验可替换读取器的完整快照。
pub struct ScopedLexicalRecall {
    scope: MemoryScope,
    memory: Arc<dyn MemoryService>,
}

impl ScopedLexicalRecall {
    pub fn new(scope: MemoryScope, memory: Arc<dyn MemoryService>) -> MemoryResult<Self> {
        scope.validate()?;
        Ok(Self { scope, memory })
    }
}

impl MemoryRecallService for ScopedLexicalRecall {
    fn recall(&self, request: &MemoryRecallRequest) -> MemoryResult<MemoryRecallResponse> {
        self.read_view(request).map(|(_, response)| response)
    }
}
impl ScopedLexicalRecall {
    pub(crate) fn read_view(
        &self,
        request: &MemoryRecallRequest,
    ) -> MemoryResult<(MemoryScanSummary, MemoryRecallResponse)> {
        request.validate()?;
        // 保持同一修订的扫描视图；逐块核对全部证据，仅保留紧凑来源索引和最高排名候选。
        let mut cursor = self.memory.scan()?;
        let summary = cursor.summary().clone();
        summary
            .scope
            .validate()
            .map_err(|_| MemoryError::CorruptState)?;
        if summary.scope != self.scope || summary.preferences.len() > MAX_PREFERENCES {
            return Err(MemoryError::CorruptState);
        }
        let query = Query::new(&request.query);
        let mut index = ScanIndex::default();
        let mut candidates = Vec::new();
        loop {
            let page = cursor.next_page()?;
            if page.len() > MAX_ARCHIVE_PAGE {
                return Err(MemoryError::CorruptState);
            }
            if page.is_empty() {
                break;
            }
            for evidence in page {
                index.observe(&evidence, &summary)?;
                let EvidenceSource::CompletedInteraction {
                    message_id,
                    session_revision,
                    turn_id,
                    user_text,
                    assistant_text,
                } = &evidence.source
                else {
                    continue;
                };
                for (field, text) in [
                    (RecallField::User, user_text),
                    (RecallField::Assistant, assistant_text),
                ] {
                    if let Some(matched) = query.matches(text) {
                        keep_candidate(
                            &mut candidates,
                            Candidate::new(
                                matched,
                                text,
                                evidence.revision,
                                SourceKey::Interaction(evidence.id.clone(), field),
                                MemoryRecallSource::CompletedInteraction {
                                    evidence_id: evidence.id.clone(),
                                    evidence_revision: evidence.revision,
                                    message_id: message_id.clone(),
                                    session_revision: *session_revision,
                                    turn_id: *turn_id,
                                    at_ms: evidence.at_ms,
                                    field,
                                },
                            ),
                        );
                    }
                }
            }
        }
        index.finish(&summary)?;
        for preference in &summary.preferences {
            if preference.status != PreferenceStatus::Confirmed {
                continue;
            }
            let latest = preference.history.last().ok_or(MemoryError::CorruptState)?;
            let source = index
                .ids
                .get(&latest.evidence_id)
                .ok_or(MemoryError::CorruptState)?;
            if let Some(matched) = query.matches(&preference.text) {
                keep_candidate(
                    &mut candidates,
                    Candidate::new(
                        matched,
                        &preference.text,
                        *source,
                        SourceKey::Preference(preference.id.clone()),
                        MemoryRecallSource::ConfirmedPreference {
                            preference_id: preference.id.clone(),
                            preference_revision: preference.revision,
                            evidence_id: latest.evidence_id.clone(),
                            evidence_revision: *source,
                            at_ms: latest.at_ms,
                        },
                    ),
                );
            }
        }
        let mut response = MemoryRecallResponse {
            scope: summary.scope.clone(),
            revision: summary.revision,
            hits: vec![],
        };
        for candidate in candidates {
            if response.hits.len() == request.limit {
                break;
            }
            response.hits.push(candidate.hit);
            let encoded = serde_json::to_vec(&response).map_err(|_| MemoryError::CorruptState)?;
            if encoded.len() > MAX_RECALL_RESPONSE_BYTES {
                response.hits.pop();
            }
        }
        response
            .validate_for(&self.scope, request)
            .map_err(|_| MemoryError::CorruptState)?;
        Ok((summary, response))
    }
}

fn keep_candidate(candidates: &mut Vec<Candidate>, candidate: Candidate) {
    candidates.push(candidate);
    candidates.sort_by(|left, right| {
        right
            .hit
            .score
            .cmp(&left.hit.score)
            .then_with(|| right.evidence_revision.cmp(&left.evidence_revision))
            .then_with(|| left.key.cmp(&right.key))
    });
    candidates.truncate(MAX_RECALL_RESULTS);
}

/// 仅保存唯一性与来源引用核验所需元数据，不保留扫描过的原文。
#[derive(Default)]
struct ScanIndex {
    ids: BTreeMap<String, u64>,
    messages: BTreeSet<String>,
    turns: BTreeSet<u64>,
    statements: BTreeSet<String>,
    last_revision: u64,
}
impl ScanIndex {
    fn observe(
        &mut self,
        evidence: &InteractionEvidence,
        summary: &MemoryScanSummary,
    ) -> MemoryResult<()> {
        let invalid = || MemoryError::CorruptState;
        validate_id(&evidence.id).map_err(|_| invalid())?;
        if self.ids.len() as u64 >= summary.evidence_count
            || evidence.revision <= self.last_revision
            || evidence.revision > summary.revision
            || self
                .ids
                .insert(evidence.id.clone(), evidence.revision)
                .is_some()
        {
            return Err(invalid());
        }
        self.last_revision = evidence.revision;
        let message = match &evidence.source {
            EvidenceSource::UserStatement { message_id, text } => {
                validate_text(text, MAX_TEXT_BYTES).map_err(|_| invalid())?;
                self.statements.insert(evidence.id.clone());
                message_id
            }
            EvidenceSource::CompletedInteraction {
                message_id,
                session_revision,
                turn_id,
                user_text,
                assistant_text,
            } => {
                validate_text(user_text, MAX_TEXT_BYTES).map_err(|_| invalid())?;
                validate_text(assistant_text, MAX_TEXT_BYTES).map_err(|_| invalid())?;
                if *turn_id == 0
                    || turn_id.checked_mul(2).is_none_or(|n| *session_revision < n)
                    || !self.turns.insert(*turn_id)
                {
                    return Err(invalid());
                }
                message_id
            }
        };
        validate_id(message).map_err(|_| invalid())?;
        if !self.messages.insert(message.clone()) {
            return Err(invalid());
        }
        Ok(())
    }
    fn finish(&self, summary: &MemoryScanSummary) -> MemoryResult<()> {
        if self.ids.len() as u64 != summary.evidence_count {
            return Err(MemoryError::CorruptState);
        }
        let mut preferences = BTreeSet::new();
        let mut referenced = BTreeSet::new();
        let mut count = 0usize;
        for preference in &summary.preferences {
            validate_id(&preference.id).map_err(|_| MemoryError::CorruptState)?;
            validate_text(&preference.text, MAX_PREFERENCE_BYTES)
                .map_err(|_| MemoryError::CorruptState)?;
            count = count
                .checked_add(preference.history.len())
                .ok_or(MemoryError::CorruptState)?;
            if count > MAX_HISTORY
                || !preferences.insert(&preference.id)
                || preference.revision == 0
                || preference.revision > summary.revision
                || preference.revision != preference.history.len() as u64
            {
                return Err(MemoryError::CorruptState);
            }
            for (index, version) in preference.history.iter().enumerate() {
                validate_id(&version.evidence_id).map_err(|_| MemoryError::CorruptState)?;
                validate_text(&version.text, MAX_PREFERENCE_BYTES)
                    .map_err(|_| MemoryError::CorruptState)?;
                if version.revision != index as u64 + 1
                    || !self.ids.contains_key(&version.evidence_id)
                {
                    return Err(MemoryError::CorruptState);
                }
                referenced.insert(&version.evidence_id);
                if version.status == PreferenceStatus::Revoked
                    && (index == 0
                        || index + 1 != preference.history.len()
                        || version.text != preference.history[index - 1].text)
                {
                    return Err(MemoryError::CorruptState);
                }
            }
            let last = preference.history.last().ok_or(MemoryError::CorruptState)?;
            if last.revision != preference.revision
                || last.text != preference.text
                || last.status != preference.status
            {
                return Err(MemoryError::CorruptState);
            }
        }
        if summary.revision != (self.turns.len() + count) as u64
            || self.statements.iter().any(|id| !referenced.contains(id))
        {
            return Err(MemoryError::CorruptState);
        }
        Ok(())
    }
}

// 此验证器不接触存储实现。公开快照没有操作日志，故不能重放每次 CAS；
// 这里严格核查公开字段能表达的来源、修订、历史、容量及作用域不变量。
pub(crate) fn validate_snapshot(
    snapshot: &MemorySnapshot,
    scope: &MemoryScope,
) -> MemoryResult<()> {
    snapshot.scope.validate()?;
    // 完整记忆可以超过热层容量；修订与证据、历史条数的一致性在下面逐项核对。
    if &snapshot.scope != scope
        || snapshot.preferences.len() > MAX_PREFERENCES
        || snapshot.revision
            > (snapshot.evidence.len() + MAX_HISTORY) as u64 + snapshot.archived_evidence
    {
        return Err(MemoryError::CorruptState);
    }
    let mut evidence_ids = BTreeMap::new();
    let mut evidence_revisions = BTreeSet::new();
    let mut messages = BTreeSet::new();
    let mut turns = BTreeSet::new();
    let mut completed_count = 0;
    for evidence in &snapshot.evidence {
        validate_id(&evidence.id)?;
        if evidence.revision == 0
            || evidence.revision > snapshot.revision
            || evidence_ids
                .insert(evidence.id.as_str(), evidence)
                .is_some()
            || !evidence_revisions.insert(evidence.revision)
        {
            return Err(MemoryError::CorruptState);
        }
        let message_id = match &evidence.source {
            EvidenceSource::UserStatement { message_id, text } => {
                validate_text(text, MAX_TEXT_BYTES)?;
                message_id
            }
            EvidenceSource::CompletedInteraction {
                message_id,
                session_revision,
                turn_id,
                user_text,
                assistant_text,
            } => {
                validate_text(user_text, MAX_TEXT_BYTES)?;
                validate_text(assistant_text, MAX_TEXT_BYTES)?;
                if *turn_id == 0
                    || turn_id
                        .checked_mul(2)
                        .is_none_or(|minimum| *session_revision < minimum)
                    || !turns.insert(*turn_id)
                {
                    return Err(MemoryError::CorruptState);
                }
                completed_count += 1;
                message_id
            }
        };
        validate_id(message_id)?;
        if !messages.insert(message_id.as_str()) {
            return Err(MemoryError::CorruptState);
        }
    }

    let mut preference_ids = BTreeSet::new();
    let mut history_count = 0;
    let mut referenced_evidence = BTreeSet::new();
    for preference in &snapshot.preferences {
        validate_id(&preference.id)?;
        validate_text(&preference.text, MAX_PREFERENCE_BYTES)?;
        history_count += preference.history.len();
        if history_count > MAX_HISTORY
            || !preference_ids.insert(preference.id.as_str())
            || preference.revision == 0
            || preference.revision > snapshot.revision
            || preference.revision != preference.history.len() as u64
        {
            return Err(MemoryError::CorruptState);
        }
        for (index, version) in preference.history.iter().enumerate() {
            validate_id(&version.evidence_id)?;
            validate_text(&version.text, MAX_PREFERENCE_BYTES)?;
            if version.revision != index as u64 + 1
                || !evidence_ids.contains_key(version.evidence_id.as_str())
            {
                return Err(MemoryError::CorruptState);
            }
            referenced_evidence.insert(version.evidence_id.as_str());
            if version.status == PreferenceStatus::Revoked
                && (index == 0
                    || index + 1 != preference.history.len()
                    || version.text != preference.history[index - 1].text)
            {
                return Err(MemoryError::CorruptState);
            }
        }
        let current = preference.history.last().ok_or(MemoryError::CorruptState)?;
        if current.revision != preference.revision
            || current.text != preference.text
            || current.status != preference.status
        {
            return Err(MemoryError::CorruptState);
        }
    }
    // 已归档的完成交互仍占用修订；原文只在归档读取时逐项核对。
    // complete_snapshot 是热层与已验证分块的完整视图，不受热层文档的 4 MiB 上限限制。
    // 持久文档/分块仍各自有界；此处按项核对，召回传输另受 MAX_RECALL_RESPONSE_BYTES 限制，
    // 不再为验证完整视图额外序列化一份可能很大的历史。
    if snapshot.revision != (completed_count + history_count) as u64 + snapshot.archived_evidence
        || snapshot.evidence.iter().any(|evidence| {
            matches!(evidence.source, EvidenceSource::UserStatement { .. })
                && !referenced_evidence.contains(evidence.id.as_str())
        })
    {
        return Err(MemoryError::CorruptState);
    }
    Ok(())
}

#[derive(Eq, PartialEq, Ord, PartialOrd)]
enum SourceKey {
    Preference(String),
    Interaction(String, RecallField),
}

struct Candidate {
    hit: MemoryRecallHit,
    evidence_revision: u64,
    key: SourceKey,
}

impl Candidate {
    fn new(
        matched: Matched,
        text: &str,
        evidence_revision: u64,
        key: SourceKey,
        source: MemoryRecallSource,
    ) -> Self {
        let (excerpt, excerpt_truncated) = excerpt(text, matched.start, matched.end);
        Self {
            hit: MemoryRecallHit {
                score: matched.score,
                source,
                excerpt,
                excerpt_truncated,
            },
            evidence_revision,
            key,
        }
    }
}

struct Query {
    folded: String,
    tokens: BTreeSet<String>,
}

impl Query {
    fn new(query: &str) -> Self {
        let folded: String = query.trim().chars().flat_map(char::to_lowercase).collect();
        let tokens = tokens(&folded).into_keys().collect();
        Self { folded, tokens }
    }

    fn matches(&self, text: &str) -> Option<Matched> {
        let folded = FoldedText::new(text);
        let document_tokens = tokens(&folded.text);
        let mut matched_count = 0;
        let mut first_match: Option<(usize, usize)> = None;
        for token in &self.tokens {
            if let Some(&(start, end)) = document_tokens.get(token) {
                matched_count += 1;
                if first_match.is_none_or(|(previous, _)| start < previous) {
                    first_match = Some((start, end));
                }
            }
        }
        let (start, end) = first_match?;
        let phrase = folded
            .text
            .match_indices(&self.folded)
            .find(|(start, value)| phrase_boundaries(&folded.text, *start, start + value.len()));
        // 一次短语命中高于全部词项交集；上限由 1024 字节查询约束。
        // 词项去重，因此重复出现的查询词不会凭次数提高分数。
        let score = matched_count * 100
            + matched_count * 100 / self.tokens.len() as u32
            + if phrase.is_some() { 1_000_000 } else { 0 };
        let (start, end) =
            phrase.map_or((start, end), |(start, value)| (start, start + value.len()));
        Some(Matched {
            score,
            start: folded.original_start(start),
            end: folded.original_end(end),
        })
    }
}

struct Matched {
    score: u32,
    start: usize,
    end: usize,
}

// Unicode 小写可能改变 UTF-8 字节数，甚至把一个字符展开为多个字符。
// 保存原字符边界，避免把小写后的匹配偏移误用于原文摘要。
struct FoldedText {
    text: String,
    spans: Vec<(usize, usize, usize, usize)>,
}

impl FoldedText {
    fn new(text: &str) -> Self {
        let mut result = Self {
            text: String::new(),
            spans: Vec::new(),
        };
        for (start, character) in text.char_indices() {
            let folded_start = result.text.len();
            result.text.extend(character.to_lowercase());
            result.spans.push((
                folded_start,
                result.text.len(),
                start,
                start + character.len_utf8(),
            ));
        }
        result
    }

    fn original_start(&self, offset: usize) -> usize {
        let index = self.spans.partition_point(|(_, end, _, _)| *end <= offset);
        self.spans[index].2
    }

    fn original_end(&self, offset: usize) -> usize {
        let index = self.spans.partition_point(|(_, end, _, _)| *end < offset);
        self.spans[index].3
    }
}

fn is_han(character: char) -> bool {
    matches!(character as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF
        | 0x20000..=0x2FA1F | 0x30000..=0x3347F)
}

fn is_word(character: char) -> bool {
    character.is_alphanumeric() && !is_han(character)
}

/// 非汉字按连续 Unicode 字母数字完整词匹配；汉字加入单字及连续双字。
/// 不做 Unicode NFC/NFKC、分词模型或同义词扩展，标点与下划线分隔词项。
fn tokens(text: &str) -> BTreeMap<String, (usize, usize)> {
    let mut result = BTreeMap::new();
    let mut word_start = None;
    let mut previous_han = None;
    for (index, character) in text.char_indices() {
        if is_word(character) {
            word_start.get_or_insert(index);
            previous_han = None;
            continue;
        }
        if let Some(start) = word_start.take() {
            result
                .entry(text[start..index].into())
                .or_insert((start, index));
        }
        if is_han(character) {
            let end = index + character.len_utf8();
            result
                .entry(text[index..end].into())
                .or_insert((index, end));
            if let Some(start) = previous_han {
                result
                    .entry(text[start..end].into())
                    .or_insert((start, end));
            }
            previous_han = Some(index);
        } else {
            previous_han = None;
        }
    }
    if let Some(start) = word_start {
        result
            .entry(text[start..].into())
            .or_insert((start, text.len()));
    }
    result
}

fn phrase_boundaries(text: &str, start: usize, end: usize) -> bool {
    let phrase = &text[start..end];
    !(phrase.chars().next().is_some_and(is_word)
        && text[..start].chars().next_back().is_some_and(is_word)
        || phrase.chars().next_back().is_some_and(is_word)
            && text[end..].chars().next().is_some_and(is_word))
}

fn excerpt(text: &str, matched_start: usize, matched_end: usize) -> (String, bool) {
    if text.len() <= MAX_RECALL_EXCERPT_BYTES {
        return (text.into(), false);
    }
    const MARK: &str = "…";
    let payload = MAX_RECALL_EXCERPT_BYTES - MARK.len() * 2;
    let matched_len = (matched_end - matched_start).min(payload);
    let mut start = matched_start.saturating_sub((payload - matched_len) / 2);
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + payload).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut result = String::new();
    if start > 0 {
        result.push_str(MARK);
    }
    result.push_str(&text[start..end]);
    if end < text.len() {
        result.push_str(MARK);
    }
    (result, true)
}
