//! 有限、确定的偏好冲突规则。只识别完整节奏声明；未知正文不推断语义关系。
use eve_learning_api::*;
use eve_memory_api::{
    EvidenceSource, InteractionEvidence, MemorySnapshot, Preference, PreferenceStatus, validate_id,
    validate_text,
};
use std::collections::BTreeSet;

pub const EVIDENCE_POLICY_VERSION: &str = "evidence-conflict-v2";

/// 宿主必须在提交任何可替换策略的结果前调用。此函数只收窄提议，不把新增授权
/// 升级成更正授权；宿主另须核对 `proposal.policy_version == policy.version()`。
/// 替换策略可以自行选择置信度及支持数量门槛；真实来源、时间和冲突屏障不可绕过。
/// 返回决定只绑定读取时快照，实际写入仍须执行 Memory CAS。
pub fn constrain_decision(
    candidate: &PreferenceCandidate,
    batch: &LearningBatch,
    memory: &MemorySnapshot,
    now_ms: u64,
    proposal: LearningDecision,
) -> LearningResult<LearningDecision> {
    proposal.validate()?;
    if proposal.candidate_id != candidate.id
        || proposal.batch_id != batch.id
        || proposal.memory_revision != memory.revision
        || proposal.evidence_ids != candidate.draft.evidence_ids
    {
        return Err(LearningError::InvalidInput);
    }
    let baseline = resolve_decision(
        candidate,
        batch,
        memory,
        now_ms,
        &proposal.policy_version,
        false,
    )?;
    if matches!(
        proposal.action,
        LearningDecisionAction::Defer | LearningDecisionAction::Reject
    ) {
        return Ok(proposal);
    }
    if matches!(
        baseline.action,
        LearningDecisionAction::Defer | LearningDecisionAction::Reject
    ) {
        return Ok(baseline);
    }
    if proposal.action == baseline.action {
        return Ok(proposal);
    }
    Ok(LearningDecision {
        action: LearningDecisionAction::Defer,
        reason: DecisionReason::PolicyDenied,
        ..proposal
    })
}

pub(super) fn evidence_decision(
    candidate: &PreferenceCandidate,
    batch: &LearningBatch,
    memory: &MemorySnapshot,
    now_ms: u64,
    policy_version: &str,
) -> LearningResult<LearningDecision> {
    resolve_decision(candidate, batch, memory, now_ms, policy_version, true)
}

fn resolve_decision(
    candidate: &PreferenceCandidate,
    batch: &LearningBatch,
    memory: &MemorySnapshot,
    now_ms: u64,
    policy_version: &str,
    enforce_default_threshold: bool,
) -> LearningResult<LearningDecision> {
    validate_inputs(candidate, batch, memory)?;
    let decide = |action, reason| {
        let decision = LearningDecision {
            candidate_id: candidate.id.clone(),
            batch_id: batch.id.clone(),
            policy_version: policy_version.into(),
            memory_revision: memory.revision,
            evidence_ids: candidate.draft.evidence_ids.clone(),
            action,
            reason,
        };
        decision.validate()?;
        Ok(decision)
    };
    let reject = |reason| decide(LearningDecisionAction::Reject, reason);
    let defer = |reason| decide(LearningDecisionAction::Defer, reason);

    if already_linked(candidate, memory)? {
        return reject(DecisionReason::AlreadyLinked);
    }
    if candidate.created_at_ms > now_ms || now_ms >= candidate.expires_at_ms {
        return reject(DecisionReason::Expired);
    }
    if enforce_default_threshold
        && (candidate.draft.confidence < 80 || candidate.draft.evidence_ids.len() < 2)
    {
        return defer(DecisionReason::EvidenceThreshold);
    }
    let text = &candidate.draft.text;
    if memory.preferences.iter().any(|preference| {
        preference.status == PreferenceStatus::Confirmed && equivalent(&preference.text, text)
    }) {
        return reject(DecisionReason::Duplicate);
    }
    if memory.preferences.iter().any(|preference| {
        preference.status == PreferenceStatus::Revoked
            && preference
                .history
                .iter()
                .any(|version| equivalent(&version.text, text))
    }) {
        return reject(DecisionReason::RevokedConflict);
    }
    // 明确更正过的旧正文不能换一个候选 ID 从历史复活。来源类型检查覆盖全部版本。
    for preference in &memory.preferences {
        if preference
            .history
            .iter()
            .any(|version| equivalent(&version.text, text))
            && has_manual_history(preference, memory)?
        {
            return defer(DecisionReason::ManualConflict);
        }
    }
    let Some((key, _)) = explicit_setting(text) else {
        let settings = blocked_settings(text);
        if settings.values.is_empty() {
            return decide(LearningDecisionAction::Confirm, DecisionReason::Eligible);
        }
        let related: Vec<_> = memory
            .preferences
            .iter()
            .filter(|preference| {
                preference.history.iter().any(|version| {
                    settings
                        .values
                        .iter()
                        .any(|(key, _)| contains_setting_key(&version.text, *key))
                })
            })
            .collect();
        if related
            .iter()
            .any(|preference| preference.status == PreferenceStatus::Revoked)
        {
            return defer(DecisionReason::RevokedConflict);
        }
        for preference in &related {
            if has_manual_history(preference, memory)? {
                return defer(DecisionReason::ManualConflict);
            }
        }
        // 保留已有的无冲突首次确认能力；宽写法和混合句永不获得更正权。
        return if related.is_empty() && settings.complete && settings.consistent {
            decide(LearningDecisionAction::Confirm, DecisionReason::Eligible)
        } else {
            defer(DecisionReason::AmbiguousConflict)
        };
    };
    let related: Vec<_> = memory
        .preferences
        .iter()
        .filter(|preference| {
            preference
                .history
                .iter()
                .any(|version| contains_setting_key(&version.text, key))
        })
        .collect();
    if related
        .iter()
        .any(|preference| preference.status == PreferenceStatus::Revoked)
    {
        return defer(DecisionReason::RevokedConflict);
    }
    for preference in &related {
        if has_manual_history(preference, memory)? {
            return defer(DecisionReason::ManualConflict);
        }
    }
    if related.len() > 1 {
        return defer(DecisionReason::AmbiguousConflict);
    }
    let Some(target) = related.first() else {
        return decide(LearningDecisionAction::Confirm, DecisionReason::Eligible);
    };
    // 当前正文已离开这个单键，不能以历史关联覆盖整个现有偏好。
    if !explicit_setting(&target.text).is_some_and(|(target_key, _)| target_key == key) {
        return defer(DecisionReason::AmbiguousConflict);
    }
    let last = target.history.last().ok_or(LearningError::InvalidInput)?;
    let target_source = source(memory, &last.evidence_id)?;
    let candidate_revision = candidate
        .draft
        .evidence_ids
        .iter()
        .map(|id| source(memory, id).map(|value| value.revision))
        .collect::<LearningResult<Vec<_>>>()?
        .into_iter()
        .max()
        .ok_or(LearningError::InvalidInput)?;
    if candidate_revision <= target_source.revision {
        return defer(DecisionReason::StaleEvidence);
    }
    decide(
        LearningDecisionAction::Update {
            preference_id: target.id.clone(),
            expected_revision: target.revision,
        },
        DecisionReason::ExplicitRevisionUpdate,
    )
}

fn validate_inputs(
    candidate: &PreferenceCandidate,
    batch: &LearningBatch,
    memory: &MemorySnapshot,
) -> LearningResult<()> {
    batch.scope.validate()?;
    validate_id(&candidate.id)?;
    validate_id(&batch.id)?;
    validate_id(&batch.extractor_version)?;
    if memory.scope != batch.scope
        || candidate.batch_id != batch.id
        || candidate.created_at_ms < batch.started_at_ms
        || candidate.expires_at_ms <= candidate.created_at_ms
        || memory.revision == 0
        || batch.evidence.is_empty()
        || batch.evidence.len() > MAX_BATCH_EVIDENCE
        || memory.evidence.len() > eve_memory_api::MAX_EVIDENCE
        || memory.preferences.len() > eve_memory_api::MAX_PREFERENCES
    {
        return Err(LearningError::InvalidInput);
    }
    crate::validate_drafts(batch, std::slice::from_ref(&candidate.draft))?;
    let mut ids = BTreeSet::new();
    let mut revisions = BTreeSet::new();
    let mut messages = BTreeSet::new();
    let mut turns = BTreeSet::new();
    for item in &memory.evidence {
        validate_id(&item.id)?;
        if !ids.insert(&item.id)
            || item.revision == 0
            || item.revision > memory.revision
            || !revisions.insert(item.revision)
        {
            return Err(LearningError::InvalidInput);
        }
        let message = match &item.source {
            EvidenceSource::UserStatement { message_id, text } => {
                validate_text(text, eve_memory_api::MAX_TEXT_BYTES)?;
                message_id
            }
            EvidenceSource::CompletedInteraction {
                message_id,
                session_revision,
                turn_id,
                user_text,
                assistant_text,
            } => {
                if *session_revision == 0 || *turn_id == 0 || !turns.insert(*turn_id) {
                    return Err(LearningError::InvalidInput);
                }
                validate_text(user_text, eve_memory_api::MAX_TEXT_BYTES)?;
                validate_text(assistant_text, eve_memory_api::MAX_TEXT_BYTES)?;
                message_id
            }
        };
        validate_id(message)?;
        if !messages.insert(message) {
            return Err(LearningError::InvalidInput);
        }
    }
    let mut previous_revision = 0;
    for item in &batch.evidence {
        if !matches!(item.source, EvidenceSource::CompletedInteraction { .. })
            || item.revision <= previous_revision
            || source(memory, &item.id)? != item
        {
            return Err(LearningError::InvalidInput);
        }
        previous_revision = item.revision;
    }
    let mut preference_ids = BTreeSet::new();
    let mut history_count = 0usize;
    for preference in &memory.preferences {
        validate_id(&preference.id)?;
        validate_text(&preference.text, eve_memory_api::MAX_PREFERENCE_BYTES)?;
        if !preference_ids.insert(&preference.id) || preference.history.is_empty() {
            return Err(LearningError::InvalidInput);
        }
        history_count += preference.history.len();
        if history_count > eve_memory_api::MAX_HISTORY {
            return Err(LearningError::InvalidInput);
        }
        for (index, version) in preference.history.iter().enumerate() {
            validate_text(&version.text, eve_memory_api::MAX_PREFERENCE_BYTES)?;
            if version.revision != index as u64 + 1 {
                return Err(LearningError::InvalidInput);
            }
            source(memory, &version.evidence_id)?;
        }
        let last = preference
            .history
            .last()
            .ok_or(LearningError::InvalidInput)?;
        if last.revision != preference.revision
            || last.text != preference.text
            || last.status != preference.status
        {
            return Err(LearningError::InvalidInput);
        }
    }
    Ok(())
}

fn source<'a>(memory: &'a MemorySnapshot, id: &str) -> LearningResult<&'a InteractionEvidence> {
    memory
        .evidence
        .iter()
        .find(|item| item.id == id)
        .ok_or(LearningError::InvalidInput)
}

fn has_manual_history(preference: &Preference, memory: &MemorySnapshot) -> LearningResult<bool> {
    for version in &preference.history {
        if matches!(
            source(memory, &version.evidence_id)?.source,
            EvidenceSource::UserStatement { .. }
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn already_linked(
    candidate: &PreferenceCandidate,
    memory: &MemorySnapshot,
) -> LearningResult<bool> {
    for preference in &memory.preferences {
        let mut linked = false;
        for version in &preference.history {
            if version.text != candidate.draft.text || version.status != PreferenceStatus::Confirmed
            {
                continue;
            }
            let evidence = source(memory, &version.evidence_id)?;
            linked |= match &evidence.source {
                EvidenceSource::CompletedInteraction { .. } => {
                    candidate.draft.evidence_ids.contains(&evidence.id)
                }
                EvidenceSource::UserStatement { text, .. } => {
                    let mut parts = text.split_whitespace();
                    parts.next() == Some("/accept-memory")
                        && parts.next() == Some(candidate.id.as_str())
                        && parts.next().is_none()
                }
            };
        }
        if preference.id == preference_id(&candidate.id) && !linked {
            // 固定跨插件 ID 已占用但来源/原文不符，不当作成功，也不能再新建覆盖。
            return Err(LearningError::InvalidInput);
        }
        if linked {
            return Ok(true);
        }
    }
    Ok(false)
}

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(['.', '。'])
        .trim_end()
        .to_owned()
}

fn equivalent(left: &str, right: &str) -> bool {
    normalize(left) == normalize(right)
        || explicit_setting(left)
            .zip(explicit_setting(right))
            .is_some_and(|(a, b)| a == b)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SettingKey {
    Enabled,
    MaxSegments,
    PausePercent,
}

struct BlockedSettings {
    values: Vec<(SettingKey, u16)>,
    complete: bool,
    consistent: bool,
}

fn blocked_settings(text: &str) -> BlockedSettings {
    let mut result = BlockedSettings {
        values: Vec::new(),
        complete: true,
        consistent: true,
    };
    let mut count = 0;
    for clause in text
        .split(['，', ',', '；', ';', '。', '.', '\n'])
        .filter(|part| !part.trim().is_empty())
    {
        count += 1;
        if count > 8 {
            result.complete = false;
        }
        let Some((key, value)) = blocking_setting(clause) else {
            result.complete = false;
            continue;
        };
        if let Some((_, previous)) = result.values.iter().find(|(existing, _)| *existing == key) {
            if *previous != value {
                result.consistent = false;
            }
        } else {
            result.values.push((key, value));
        }
    }
    result.complete &= count > 0;
    result
}

fn contains_setting_key(text: &str, key: SettingKey) -> bool {
    text.split(['，', ',', '；', ';', '。', '.', '\n'])
        .any(|clause| blocking_setting(clause).is_some_and(|(found, _)| found == key))
}

/// 消费节奏偏好的宿主可接受更多书写形式；这些形式扩大历史阻断范围。
/// 完整一致且没有任何相关历史时保留首次确认；不能作为自动更正依据。
fn blocking_setting(clause: &str) -> Option<(SettingKey, u16)> {
    let compact: String = clause
        .chars()
        .filter(|value| !value.is_whitespace())
        .collect();
    parse_setting(compact.trim_end_matches(['!', '！']), 8, true)
}

/// 仅完整、单一声明；不移除内部空白，不拆混合句，不归纳自由文本同义词。
fn explicit_setting(text: &str) -> Option<(SettingKey, u16)> {
    parse_setting(text, 5, false)
}

fn parse_setting(text: &str, max_segments: u16, allow_plus: bool) -> Option<(SettingKey, u16)> {
    let normalized = normalize(text);
    let text = normalized.strip_prefix('请').unwrap_or(&normalized);
    match text {
        "不要分段" | "回复不要分段" | "回复整条发送" | "整条发送回复" => {
            return Some((SettingKey::Enabled, 0));
        }
        "回复分段发送" | "回复按自然段分开发送" => {
            return Some((SettingKey::Enabled, 1));
        }
        "段间不要停顿" | "分段之间不要停顿" => {
            return Some((SettingKey::PausePercent, 0));
        }
        "段间停顿快一点" | "段间停顿短一点" => {
            return Some((SettingKey::PausePercent, 50));
        }
        "段间停顿慢一点" | "段间停顿长一点" => {
            return Some((SettingKey::PausePercent, 150));
        }
        _ => {}
    }
    for prefix in ["回复最多分成", "每条回复最多", "回复最多", "回复分成"] {
        if let Some(number) = text
            .strip_prefix(prefix)
            .and_then(|value| value.strip_suffix('段'))
        {
            let count = match number.trim() {
                "二" | "两" => 2,
                "三" => 3,
                "四" => 4,
                "五" => 5,
                value => decimal(value, allow_plus)?,
            };
            return (2..=max_segments)
                .contains(&count)
                .then_some((SettingKey::MaxSegments, count));
        }
    }
    for prefix in ["段间停顿为", "段间停顿设为", "段间停顿", "回复段间停顿"] {
        if let Some(number) = text
            .strip_prefix(prefix)
            .and_then(|value| value.strip_suffix(['%', '％']))
        {
            let number = number.trim();
            let percent = decimal(number, allow_plus)?;
            return (percent <= 200).then_some((SettingKey::PausePercent, percent));
        }
    }
    None
}

fn decimal(value: &str, allow_plus: bool) -> Option<u16> {
    let value = if allow_plus {
        value.strip_prefix('+').unwrap_or(value)
    } else {
        value
    };
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| value.parse().ok())
        .flatten()
}
