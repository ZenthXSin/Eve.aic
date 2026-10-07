//! 用户确认与受限自主学习；决策意图和真实 Memory 历史分别核对。
use eve_learning_api::{
    AutoConfirmationPolicy, DecisionReason, JobStatus, LearningAdmin, LearningDecisionAction,
    LearningDecisionRecord, LearningError, LearningSnapshot, MAX_BATCH_EVIDENCE,
    MAX_CANDIDATE_BYTES, MAX_CANDIDATES, MAX_DECISIONS, MAX_JOBS, PreferenceCandidate,
    preference_id,
};
use eve_memory_api::{
    EvidenceSource, MAX_TEXT_BYTES, MemoryAdmin, MemoryError, MemoryScope, MemorySnapshot,
    Preference, PreferenceAction, PreferenceChange, PreferenceEvidence, PreferenceStatus,
    UserStatement, validate_id, validate_text,
};
use eve_plugin_api::{PluginError, PluginResult};
use eve_qqbot_plugin::{QqCommandHandler, QqCommandInput};
use ring::digest::{Context, SHA256};
use std::{
    collections::BTreeSet,
    fmt,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

const HELP: &str =
    "用法：/memory-candidates [页码]、/memory-decision 候选ID、/accept-memory 候选ID。";
const PAGE_SIZE: usize = 5;

pub(crate) struct Commands {
    services: Option<(Arc<dyn LearningAdmin>, Arc<dyn MemoryAdmin>)>,
    automatic: bool,
}
impl Commands {
    pub(crate) fn new(learning: Arc<dyn LearningAdmin>, memory: Arc<dyn MemoryAdmin>) -> Arc<Self> {
        Arc::new(Self {
            services: Some((learning, memory)),
            automatic: false,
        })
    }

    pub(crate) fn autonomous(
        learning: Arc<dyn LearningAdmin>,
        memory: Arc<dyn MemoryAdmin>,
    ) -> Arc<Self> {
        Arc::new(Self {
            services: Some((learning, memory)),
            automatic: true,
        })
    }

    pub(crate) fn disabled() -> Arc<Self> {
        Arc::new(Self {
            services: None,
            automatic: false,
        })
    }

    fn execute(&self, input: &QqCommandInput<'_>, command: Command<'_>) -> PluginResult<String> {
        let Some((learning, memory)) = &self.services else {
            return Ok("偏好提炼未启用。".into());
        };
        if command == Command::Help {
            return Ok(HELP.into());
        }
        if input.session.validate().is_err() || validate_id(input.message_id).is_err() {
            return Err(PluginError::Task("QQ 偏好候选命令输入无效。".into()));
        }
        if validate_text(input.text, MAX_TEXT_BYTES).is_err() {
            return Ok(HELP.into());
        }
        let scope = crate::qq_memory::scope(input.session);
        // 先固定 Memory CAS 基线，再读只追加的决策与候选。
        // 后台在此期间保存的新偏好会使手动命令 CAS 失效，不能漏掉关联后重复确认。
        let snapshot = match memory
            .reader(scope.clone())
            .and_then(|reader| reader.snapshot())
        {
            Ok(snapshot) if snapshot.scope == scope => snapshot,
            Ok(_) => return Err(failure()),
            Err(error) => return explain_memory(error),
        };
        let records = match learning.decisions(&scope) {
            Ok(records) => records,
            Err(error) => return explain_learning(error),
        };
        let learning_snapshot = match learning.snapshot(&scope) {
            Ok(snapshot) => snapshot,
            Err(error) => return explain_learning(error),
        };
        let candidates = candidates(&learning_snapshot, &scope)?;
        validate_records(&records, &candidates)?;
        // 先在当前可信作用域查候选，不能凭跨作用域的偏好 ID 进行确认。
        let selected = if let Command::Accept(id) | Command::Decision(id) = command {
            let Some(candidate) = candidates
                .iter()
                .copied()
                .find(|candidate| candidate.id == id)
            else {
                return Ok("当前会话没有这条候选。发送 /memory-candidates 查看候选 ID。".into());
            };
            Some(candidate)
        } else {
            None
        };
        let now_ms = now_ms()?;
        if command == Command::Status {
            let mut saved = 0;
            let mut pending = 0;
            for candidate in &candidates {
                if linked_candidate(&snapshot, candidate, &records)?.is_some() {
                    saved += 1;
                } else if candidate.expires_at_ms > now_ms {
                    pending += 1;
                }
            }
            return Ok(format!(
                "自主学习：{}。本会话已关联 {saved} 条候选，仍有 {pending} 条未确认且未过期。内置默认门槛：模型自评至少 80、至少两条真实交互引用；替换策略以决策版本为准。明确冲突优先保留手动修正和撤销。/memory-decision 候选ID 查看决策与实际保存；/memories 查看或纠正、撤销；/segment 查看有效节奏。{}",
                if self.automatic {
                    "开启"
                } else {
                    "关闭（提炼保留手动确认）"
                },
                if snapshot.preferences.len() >= eve_memory_api::MAX_PREFERENCES
                    || snapshot
                        .preferences
                        .iter()
                        .map(|p| p.history.len())
                        .sum::<usize>()
                        >= eve_memory_api::MAX_HISTORY
                {
                    "本会话记忆或历史容量已满，保留候选，不自动覆盖历史。"
                } else if records.len() >= MAX_DECISIONS {
                    "本会话学习决策容量已满，停止新增自动写入，保留全部历史。"
                } else if learning_snapshot.jobs.len() >= MAX_JOBS {
                    "本会话提炼容量已满，不自动删除旧批次。"
                } else {
                    "每范围至少 3 条新经历触发，按启动配置的冷却间隔执行；自主模式持续运行，不受四批启动额度限制。"
                }
            ));
        }
        if let Command::Candidates(page) = command {
            return list(&candidates, &snapshot, &records, page, now_ms);
        }
        let candidate = selected.ok_or_else(failure)?;
        if matches!(command, Command::Decision(_)) {
            return decision_details(candidate, &snapshot, &records);
        }
        let id = preference_id(&candidate.id);
        if let Some(preference) = linked_candidate(&snapshot, candidate, &records)? {
            // 过期只限制首次确认；重启和重复确认不能复活撤销或覆盖后续修改。
            return Ok(current_reply(preference));
        }
        if candidate.expires_at_ms <= now_ms {
            return Ok("候选已过期，不能首次确认。发送 /memory-candidates 查看当前候选。".into());
        }
        let digest = message_digest(input);
        let change = PreferenceChange {
            operation_id: format!("qq-learning-operation-{digest}"),
            at_ms: now_ms,
            evidence: PreferenceEvidence::Statement(UserStatement {
                evidence_id: format!("qq-learning-evidence-{digest}"),
                message_id: input.message_id.into(),
                // 确认证据是用户真实命令，候选正文保存在不可变 Learning 记录中。
                text: input.text.into(),
                at_ms: now_ms,
            }),
            action: PreferenceAction::Confirm {
                id,
                text: candidate.draft.text.clone(),
            },
        };
        match memory.update_preference(&scope, snapshot.revision, change) {
            Ok(saved) if saved.scope == scope => {
                let preference = confirmed(&saved, candidate)?.ok_or_else(failure)?;
                Ok(current_reply(preference))
            }
            Ok(_) => Err(failure()),
            // 不跨插件写 Accepted，也不在 CAS 冲突后自动重写。
            Err(error) => explain_memory(error),
        }
    }
}
impl QqCommandHandler for Commands {
    fn handle(&self, input: QqCommandInput<'_>) -> PluginResult<Option<String>> {
        let Some(command) = parse(input.text) else {
            return Ok(None);
        };
        self.execute(&input, command).map(Some)
    }
}

fn failure() -> PluginError {
    PluginError::State("偏好候选或记忆状态无法确认；服务已停止，请重新打开后查看持久状态。".into())
}

fn explain_memory(error: MemoryError) -> PluginResult<String> {
    match error {
        MemoryError::InvalidInput => Ok(HELP.into()),
        MemoryError::NotFound => Ok("当前会话没有这条偏好；请查看 /memory-candidates。".into()),
        MemoryError::Conflict => Ok(
            "确认操作与已有状态或消息记录冲突；请先查看 /memory-candidates，再使用新消息提交。"
                .into(),
        ),
        MemoryError::StaleRevision => {
            Ok("记忆刚刚发生变化；请重新查看 /memory-candidates 后发送新消息重试。".into())
        }
        MemoryError::LimitReached => Ok("记忆容量已满；原记录已保留，本次没有新增记录。".into()),
        MemoryError::Unavailable
        | MemoryError::CorruptState
        | MemoryError::UnsupportedVersion
        | MemoryError::Storage => Err(failure()),
    }
}

fn explain_learning(error: LearningError) -> PluginResult<String> {
    match error {
        LearningError::InvalidInput => Ok(HELP.into()),
        LearningError::Conflict => Ok("候选状态冲突；请重新查看 /memory-candidates。".into()),
        LearningError::LimitReached => Ok("候选容量已满；原记录已保留。".into()),
        LearningError::Memory(error) => explain_memory(error),
        LearningError::Unavailable
        | LearningError::CorruptState
        | LearningError::UnsupportedVersion
        | LearningError::Storage
        | LearningError::Extraction(_) => Err(failure()),
    }
}

fn candidates<'a>(
    snapshot: &'a LearningSnapshot,
    scope: &MemoryScope,
) -> PluginResult<Vec<&'a PreferenceCandidate>> {
    if &snapshot.scope != scope || snapshot.jobs.len() > MAX_JOBS {
        return Err(failure());
    }
    let mut result = Vec::new();
    let mut ids = BTreeSet::new();
    for job in &snapshot.jobs {
        if &job.batch.scope != scope
            || job.candidates.len() > MAX_CANDIDATES
            || (!job.candidates.is_empty() && job.status != JobStatus::Completed)
        {
            return Err(failure());
        }
        for candidate in &job.candidates {
            let evidence: BTreeSet<_> = candidate.draft.evidence_ids.iter().collect();
            if validate_id(&candidate.id).is_err()
                || validate_id(&preference_id(&candidate.id)).is_err()
                || candidate.id.chars().any(char::is_whitespace)
                || !ids.insert(candidate.id.as_str())
                || candidate.batch_id != job.batch.id
                || validate_text(&candidate.draft.text, MAX_CANDIDATE_BYTES).is_err()
                || candidate.draft.confidence > 100
                || candidate.expires_at_ms < candidate.created_at_ms
                || evidence.is_empty()
                || evidence.len() != candidate.draft.evidence_ids.len()
                || evidence.len() > MAX_BATCH_EVIDENCE
                || evidence.iter().any(|id| {
                    validate_id(id).is_err()
                        || !job.batch.evidence.iter().any(|source| {
                            &source.id == *id
                                && matches!(
                                    source.source,
                                    EvidenceSource::CompletedInteraction { .. }
                                )
                        })
                })
            {
                return Err(failure());
            }
            result.push(candidate);
        }
    }
    result.sort_by(|left, right| {
        right
            .created_at_ms
            .cmp(&left.created_at_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(result)
}

/// 固定关联键不足以证明已确认，还须验证首版正文与真实用户确认来源。
pub(crate) fn confirmed<'a>(
    snapshot: &'a MemorySnapshot,
    candidate: &PreferenceCandidate,
) -> PluginResult<Option<&'a Preference>> {
    let id = preference_id(&candidate.id);
    let mut matches = snapshot
        .preferences
        .iter()
        .filter(|preference| preference.id == id);
    let Some(preference) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        return Err(failure());
    }
    let first = preference.history.first().ok_or_else(failure)?;
    let current = preference.history.last().ok_or_else(failure)?;
    if first.revision != 1
        || first.status != PreferenceStatus::Confirmed
        || first.text != candidate.draft.text
        || current.revision != preference.revision
        || current.status != preference.status
        || current.text != preference.text
    {
        return Err(failure());
    }
    let mut evidence = snapshot
        .evidence
        .iter()
        .filter(|item| item.id == first.evidence_id);
    let source = evidence.next().ok_or_else(failure)?;
    if evidence.next().is_some()
        || !(matches!(
            &source.source,
            EvidenceSource::UserStatement { text, .. }
                if parse(text) == Some(Command::Accept(&candidate.id))
        ) || (matches!(source.source, EvidenceSource::CompletedInteraction { .. })
            && candidate.draft.evidence_ids.contains(&source.id)))
    {
        return Err(failure());
    }
    Ok(Some(preference))
}

fn validate_records(
    records: &[LearningDecisionRecord],
    candidates: &[&PreferenceCandidate],
) -> PluginResult<()> {
    if records.len() > MAX_DECISIONS {
        return Err(failure());
    }
    for (index, record) in records.iter().enumerate() {
        let decision = &record.decision;
        let candidate = candidates
            .iter()
            .find(|candidate| candidate.id == decision.candidate_id)
            .ok_or_else(failure)?;
        if record.sequence != index as u64 + 1
            || record.at_ms < candidate.created_at_ms
            || decision.batch_id != candidate.batch_id
            || decision.evidence_ids != candidate.draft.evidence_ids
            || decision.validate().is_err()
        {
            return Err(failure());
        }
    }
    Ok(())
}

/// 账本只是意图；必须在目标真实历史中找到指定后继版本及原始完成来源。
/// 遍历所有旧意图，使后来用户更正、撤销仍保留候选曾成功更新的事实。
fn linked_candidate<'a>(
    snapshot: &'a MemorySnapshot,
    candidate: &PreferenceCandidate,
    records: &[LearningDecisionRecord],
) -> PluginResult<Option<&'a Preference>> {
    if let Some(preference) = confirmed(snapshot, candidate)? {
        return Ok(Some(preference));
    }
    for record in records.iter().rev().filter(|record| {
        record.decision.candidate_id == candidate.id
            && record.decision.batch_id == candidate.batch_id
            && record.decision.evidence_ids == candidate.draft.evidence_ids
    }) {
        let LearningDecisionAction::Update {
            preference_id,
            expected_revision,
        } = &record.decision.action
        else {
            continue;
        };
        let Some(next_revision) = expected_revision.checked_add(1) else {
            return Err(failure());
        };
        let Some(preference) = snapshot.preferences.iter().find(|p| p.id == *preference_id) else {
            continue;
        };
        let Some(version) = preference
            .history
            .iter()
            .find(|h| h.revision == next_revision)
        else {
            continue;
        };
        if version.status != PreferenceStatus::Confirmed
            || version.text != candidate.draft.text
            || !candidate.draft.evidence_ids.contains(&version.evidence_id)
        {
            continue;
        }
        let source = snapshot
            .evidence
            .iter()
            .find(|e| e.id == version.evidence_id)
            .ok_or_else(failure)?;
        let current = preference.history.last().ok_or_else(failure)?;
        if !matches!(source.source, EvidenceSource::CompletedInteraction { .. })
            || current.revision != preference.revision
            || current.status != preference.status
            || current.text != preference.text
        {
            return Err(failure());
        }
        return Ok(Some(preference));
    }
    Ok(None)
}

/// 决策先持久保存，再用当前 Memory CAS 提交；重启依真实历史核对结果。
/// 故障时可能仅留意图，不能向用户声称已经保存。
pub(crate) fn auto_confirm(
    memory: &dyn MemoryAdmin,
    learning: &dyn LearningAdmin,
    policy: &dyn AutoConfirmationPolicy,
    scope: &MemoryScope,
    at_ms: u64,
) -> PluginResult<()> {
    let jobs = learning.snapshot(scope).map_err(|_| failure())?;
    let mut pending = candidates(&jobs, scope)?;
    let records = learning.decisions(scope).map_err(|_| failure())?;
    validate_records(&records, &pending)?;
    pending.reverse(); // 先确认旧证据，新的纠正具有更高来源修订。
    for candidate in pending {
        let snapshot = memory
            .reader(scope.clone())
            .and_then(|r| r.snapshot())
            .map_err(|_| failure())?;
        if snapshot.scope != *scope {
            return Err(failure());
        }
        if linked_candidate(&snapshot, candidate, &records)?.is_some() {
            continue;
        }
        let batch = jobs
            .jobs
            .iter()
            .find(|j| j.batch.id == candidate.batch_id)
            .ok_or_else(failure)?;
        if candidate.created_at_ms > at_ms {
            continue;
        }
        if candidate.draft.evidence_ids.iter().any(|id| {
            let source = batch.batch.evidence.iter().find(|e| e.id == *id);
            !snapshot
                .evidence
                .iter()
                .any(|current| Some(current) == source)
        }) {
            return Err(failure());
        }
        let decision = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let proposal = policy.decide(candidate, &batch.batch, &snapshot, at_ms)?;
            if proposal.policy_version != policy.version() {
                return Err(LearningError::InvalidInput);
            }
            eve_learning_plugin::constrain_decision(
                candidate,
                &batch.batch,
                &snapshot,
                at_ms,
                proposal,
            )
        }))
        .map_err(|_| failure())?
        .map_err(|_| failure())?;
        match learning.record_decision(scope, decision.clone(), at_ms) {
            Ok(record) if record.decision.same_outcome(&decision) => {}
            // 没有可持久审计意图时不得继续写 Memory。
            Err(LearningError::LimitReached) => return Ok(()),
            _ => return Err(failure()),
        }
        let action = match &decision.action {
            LearningDecisionAction::Confirm => PreferenceAction::Confirm {
                id: preference_id(&candidate.id),
                text: candidate.draft.text.clone(),
            },
            LearningDecisionAction::Update {
                preference_id,
                expected_revision,
            } => {
                let Some(target) = snapshot.preferences.iter().find(|p| p.id == *preference_id)
                else {
                    return Err(failure());
                };
                if target.revision != *expected_revision
                    || target.status != PreferenceStatus::Confirmed
                {
                    return Err(failure());
                }
                PreferenceAction::Correct {
                    id: preference_id.clone(),
                    text: candidate.draft.text.clone(),
                }
            }
            LearningDecisionAction::Defer | LearningDecisionAction::Reject => continue,
        };
        let evidence = candidate
            .draft
            .evidence_ids
            .iter()
            .filter_map(|id| snapshot.evidence.iter().find(|e| e.id == *id))
            .max_by_key(|e| e.revision)
            .ok_or_else(failure)?;
        let change = PreferenceChange {
            operation_id: preference_id(&candidate.id),
            at_ms,
            evidence: PreferenceEvidence::Existing(evidence.id.clone()),
            action,
        };
        match memory.update_preference(scope, snapshot.revision, change) {
            Ok(saved) if saved.scope == *scope => {
                let committed_records = learning.decisions(scope).map_err(|_| failure())?;
                linked_candidate(&saved, candidate, &committed_records)?.ok_or_else(failure)?;
            }
            // 新一轮扫描重新核对；冲突时不覆盖并发命令，容量满保留候选供查看。
            Err(MemoryError::StaleRevision | MemoryError::LimitReached) => return Ok(()),
            _ => return Err(failure()),
        }
    }
    Ok(())
}

fn status(preference: Option<&Preference>) -> &'static str {
    match preference {
        None => "待确认",
        Some(preference) if preference.status == PreferenceStatus::Revoked => "已撤销",
        Some(preference) if preference.revision > 1 => "后续修改",
        Some(_) => "已保存",
    }
}

fn current_reply(preference: &Preference) -> String {
    match status(Some(preference)) {
        "已撤销" => format!(
            "该候选曾确认，但偏好目前已撤销：{}。发送 /memories 查看当前记录。",
            preference.id
        ),
        "后续修改" => format!(
            "该候选已确认，偏好已有后续修改：{}。发送 /memories 查看当前记录。",
            preference.id
        ),
        _ => format!("候选偏好已确认：{}。", preference.id),
    }
}

fn list(
    candidates: &[&PreferenceCandidate],
    snapshot: &MemorySnapshot,
    records: &[LearningDecisionRecord],
    page: usize,
    now_ms: u64,
) -> PluginResult<String> {
    if candidates.is_empty() {
        return Ok("当前会话没有偏好候选。".into());
    }
    let pages = candidates.len().div_ceil(PAGE_SIZE);
    if page > pages {
        return Ok(format!(
            "当前偏好候选共有 {pages} 页；发送 /memory-candidates 1 查看第一页。"
        ));
    }
    let mut reply = format!("当前会话偏好候选（第 {page}/{pages} 页，模型自评并非事实概率）：");
    for candidate in candidates
        .iter()
        .skip((page - 1) * PAGE_SIZE)
        .take(PAGE_SIZE)
    {
        let preference = linked_candidate(snapshot, candidate, records)?;
        let expiry = if candidate.expires_at_ms <= now_ms {
            "已过期；仅限制首次确认".into()
        } else {
            format!(
                "剩余 {} 秒",
                (candidate.expires_at_ms - now_ms).div_ceil(1000)
            )
        };
        reply.push_str(&format!(
            "\n候选 ID：{}\n状态：{}；模型自评：{}%\n首次确认有效期：{}（Unix 毫秒，{}）\n来源证据：{}\n候选正文：\n{}\n确认：/accept-memory {}",
            candidate.id,
            if preference.is_some_and(|p| p.revision == 1 && snapshot.evidence.iter().any(|e|
                p.history.first().is_some_and(|h| h.evidence_id == e.id)
                    && matches!(e.source, EvidenceSource::CompletedInteraction { .. }))) {
                "已自动保存"
            } else { status(preference) },
            candidate.draft.confidence,
            candidate.expires_at_ms,
            expiry,
            candidate.draft.evidence_ids.join("、"),
            candidate.draft.text,
            candidate.id,
        ));
        if let Some(preference) = preference {
            reply.push_str(&format!("\n已关联偏好：{}", preference.id));
        }
        if let Some(record) = records
            .iter()
            .rev()
            .find(|r| r.decision.candidate_id == candidate.id)
        {
            reply.push_str(&format!(
                "\n最近学习决策：{}；{}\n详情：/memory-decision {}",
                action_text(&record.decision.action),
                reason_text(&record.decision.reason),
                candidate.id,
            ));
        }
    }
    if reply.len() >= MAX_TEXT_BYTES {
        return Err(failure());
    }
    Ok(reply)
}

fn action_text(action: &LearningDecisionAction) -> String {
    match action {
        LearningDecisionAction::Confirm => "confirm：新增确认".into(),
        LearningDecisionAction::Update {
            preference_id,
            expected_revision,
        } => format!("update：更新偏好 {preference_id}，目标偏好版本 {expected_revision}",),
        LearningDecisionAction::Defer => "defer：暂缓自动保存".into(),
        LearningDecisionAction::Reject => "reject：拒绝自动保存".into(),
    }
}

fn reason_text(reason: &DecisionReason) -> &'static str {
    match reason {
        DecisionReason::Eligible => "满足来源门槛",
        DecisionReason::EvidenceThreshold => "自评或真实来源数量未达门槛",
        DecisionReason::Expired => "已过首次确认期限",
        DecisionReason::PolicyDenied => "替换策略未授权此动作",
        DecisionReason::Duplicate => "已有规范化等价偏好",
        DecisionReason::RevokedConflict => "与用户撤销记录冲突，不自动恢复",
        DecisionReason::ManualConflict => "与用户手动确认或更正冲突，保留手动选择",
        DecisionReason::AmbiguousConflict => "存在多个或不明确的更新目标",
        DecisionReason::StaleEvidence => "候选来源修订未晚于当前偏好来源",
        DecisionReason::ExplicitRevisionUpdate => "明确偏好键已有更新的真实来源",
        DecisionReason::AlreadyLinked => "候选已有可核对的保存历史",
    }
}

fn decision_details(
    candidate: &PreferenceCandidate,
    snapshot: &MemorySnapshot,
    records: &[LearningDecisionRecord],
) -> PluginResult<String> {
    let history: Vec<_> = records
        .iter()
        .filter(|r| r.decision.candidate_id == candidate.id)
        .collect();
    let mut reply = format!(
        "候选 ID：{}\n学习决策共 {} 条（最多显示最近 8 条）。决策记录是提交意图，实际保存另按 Memory 历史核对。",
        candidate.id,
        history.len(),
    );
    if history.is_empty() {
        reply.push_str("\n尚无自动学习决策；手动确认不伪造自动决策。");
    }
    for record in history.iter().rev().take(8) {
        let decision = &record.decision;
        reply.push_str(&format!(
            "\n决策序号：{}；时间：{}（Unix 毫秒）\n策略版本：{}；读取记忆版本：{}\n动作：{}\n理由：{}\n来源证据：{}",
            record.sequence, record.at_ms, decision.policy_version, decision.memory_revision,
            action_text(&decision.action), reason_text(&decision.reason), decision.evidence_ids.join("、"),
        ));
    }
    if let Some(preference) = linked_candidate(snapshot, candidate, records)? {
        reply.push_str(&format!(
            "\n实际保存：偏好 {}；当前版本 {}；状态：{}。",
            preference.id,
            preference.revision,
            status(Some(preference)),
        ));
    } else {
        reply.push_str("\n实际保存：尚无该候选对应的确认或更新历史。");
    }
    if reply.len() >= MAX_TEXT_BYTES {
        return Err(failure());
    }
    Ok(reply)
}

fn now_ms() -> PluginResult<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .ok_or_else(failure)
}

fn message_digest(input: &QqCommandInput<'_>) -> String {
    let mut hash = Context::new(&SHA256);
    for part in [
        b"qq".as_slice(),
        input.session.session_id.as_bytes(),
        input.session.user_id.as_bytes(),
        input.message_id.as_bytes(),
    ] {
        hash.update(&(part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    hash.finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Eq, PartialEq)]
enum Command<'a> {
    Status,
    Candidates(usize),
    Decision(&'a str),
    Accept(&'a str),
    Help,
}
impl fmt::Debug for Command<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Status => "Status",
            Self::Candidates(_) => "Candidates(<redacted>)",
            Self::Decision(_) => "Decision(<redacted>)",
            Self::Accept(_) => "Accept(<redacted>)",
            Self::Help => "Help",
        })
    }
}

fn parse(text: &str) -> Option<Command<'_>> {
    let text = text.trim();
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    let (name, tail) = text.split_at(end);
    let tail = tail.trim();
    Some(match name {
        "/self-learning" if tail.is_empty() || tail == "status" => Command::Status,
        "/self-learning" => Command::Help,
        "/memory-candidates" if tail.is_empty() => Command::Candidates(1),
        "/memory-candidates" => tail
            .parse::<usize>()
            .ok()
            .filter(|page| *page > 0)
            .map(Command::Candidates)
            .unwrap_or(Command::Help),
        "/accept-memory" if validate_id(tail).is_ok() && !tail.chars().any(char::is_whitespace) => {
            Command::Accept(tail)
        }
        "/accept-memory" => Command::Help,
        "/memory-decision"
            if validate_id(tail).is_ok() && !tail.chars().any(char::is_whitespace) =>
        {
            Command::Decision(tail)
        }
        "/memory-decision" => Command::Help,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
    use eve_learning_api::{
        CandidateDraft, LearningBatch, LearningDecision, LearningDecisionRecord, LearningJob,
        LearningOptions, LearningOutcome, LearningResult,
    };
    use eve_memory_api::{CompletedInteraction, InteractionEvidence, MemoryResult, MemoryService};
    use eve_memory_plugin::{MemoryController, MemoryPlugin};
    use eve_plugin_api::{PluginId, StateStore};
    use eve_session_api::SessionKey;
    use std::{
        collections::BTreeMap,
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    #[derive(Default)]
    pub(super) struct FixedLearning {
        pub(super) snapshots: Mutex<BTreeMap<MemoryScope, LearningSnapshot>>,
        pub(super) error: Mutex<Option<LearningError>>,
        pub(super) record_error: Mutex<Option<LearningError>>,
        pub(super) records: Mutex<BTreeMap<MemoryScope, Vec<LearningDecisionRecord>>>,
        pub(super) record_writes: AtomicUsize,
    }
    impl LearningAdmin for FixedLearning {
        fn snapshot(&self, scope: &MemoryScope) -> LearningResult<LearningSnapshot> {
            if let Some(error) = self.error.lock().unwrap().clone() {
                return Err(error);
            }
            Ok(self
                .snapshots
                .lock()
                .unwrap()
                .get(scope)
                .cloned()
                .unwrap_or(LearningSnapshot {
                    scope: scope.clone(),
                    jobs: vec![],
                }))
        }
        fn reserve(
            &self,
            _: &MemorySnapshot,
            _: u64,
            _: &str,
            _: &LearningOptions,
        ) -> LearningResult<Option<LearningBatch>> {
            panic!("显式 QQ 命令不得触发模型提炼或修改 Learning")
        }
        fn finish(&self, _: &LearningBatch, _: u64, _: LearningOutcome) -> LearningResult<()> {
            panic!("确认只写一次 Memory，不跨插件写 Accepted")
        }
        fn decisions(&self, scope: &MemoryScope) -> LearningResult<Vec<LearningDecisionRecord>> {
            if let Some(error) = self.error.lock().unwrap().clone() {
                return Err(error);
            }
            Ok(self
                .records
                .lock()
                .unwrap()
                .get(scope)
                .cloned()
                .unwrap_or_default())
        }
        fn record_decision(
            &self,
            scope: &MemoryScope,
            decision: LearningDecision,
            at_ms: u64,
        ) -> LearningResult<LearningDecisionRecord> {
            if let Some(error) = self.error.lock().unwrap().clone() {
                return Err(error);
            }
            if let Some(error) = self.record_error.lock().unwrap().clone() {
                return Err(error);
            }
            decision.validate()?;
            let mut records = self.records.lock().unwrap();
            let records = records.entry(scope.clone()).or_default();
            if let Some(previous) = records
                .iter()
                .rev()
                .find(|record| record.decision.candidate_id == decision.candidate_id)
                .filter(|record| record.decision.same_outcome(&decision))
            {
                return Ok(previous.clone());
            }
            let record = LearningDecisionRecord {
                sequence: records.len() as u64 + 1,
                at_ms,
                decision,
            };
            records.push(record.clone());
            self.record_writes.fetch_add(1, Ordering::SeqCst);
            Ok(record)
        }
    }

    #[derive(Default)]
    pub(super) struct RecordingStore {
        inner: MemoryStateStore,
        pub(super) writes: AtomicUsize,
        fail_after_commit: AtomicBool,
    }
    impl StateStore for RecordingStore {
        fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
            self.inner.get(namespace, key)
        }
        fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
            self.inner.set(namespace, key, value)?;
            self.writes.fetch_add(1, Ordering::SeqCst);
            if self.fail_after_commit.load(Ordering::SeqCst) {
                return Err(PluginError::State("模拟提交成功但确认丢失".into()));
            }
            Ok(())
        }
    }

    struct MemoryFaults {
        inner: MemoryController,
        read_error: Mutex<Option<MemoryError>>,
        write_error: Mutex<Option<MemoryError>>,
        writes: AtomicUsize,
    }
    impl MemoryAdmin for MemoryFaults {
        fn reader(&self, scope: MemoryScope) -> MemoryResult<Arc<dyn MemoryService>> {
            if let Some(error) = self.read_error.lock().unwrap().clone() {
                return Err(error);
            }
            self.inner.reader(scope)
        }
        fn import_completed(
            &self,
            _: &MemoryScope,
            _: u64,
            _: CompletedInteraction,
        ) -> MemoryResult<MemorySnapshot> {
            panic!("用户确认命令不得伪造完成交互")
        }
        fn update_preference(
            &self,
            scope: &MemoryScope,
            revision: u64,
            change: PreferenceChange,
        ) -> MemoryResult<MemorySnapshot> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = self.write_error.lock().unwrap().clone() {
                return Err(error);
            }
            self.inner.update_preference(scope, revision, change)
        }
    }

    pub(super) fn session() -> SessionKey {
        SessionKey::new("qq-full-app-group-session-hash", "qq-full-user-hash").unwrap()
    }
    pub(super) fn run(
        commands: &Commands,
        session: &SessionKey,
        message_id: &str,
        text: &str,
    ) -> PluginResult<Option<String>> {
        commands.handle(QqCommandInput {
            message_id,
            session,
            text,
        })
    }
    pub(super) async fn memory(store: Arc<RecordingStore>) -> (Kernel, MemoryController) {
        let kernel = Kernel::with_services(KernelServices {
            state: store,
            ..KernelServices::default()
        });
        let plugin = MemoryPlugin::new().unwrap();
        let controller = plugin.controller();
        kernel.register(Box::new(plugin)).unwrap();
        kernel.start_all().await.unwrap();
        (kernel, controller)
    }
    fn fixture(scope: MemoryScope, count: usize) -> LearningSnapshot {
        let jobs = (0..count)
            .map(|index| {
                let batch_id = format!("batch-{index}");
                let evidence_id = format!("completed-{index}");
                LearningJob {
                    batch: LearningBatch {
                        id: batch_id.clone(),
                        scope: scope.clone(),
                        extractor_version: "fixture-v1".into(),
                        started_at_ms: 1,
                        evidence: vec![InteractionEvidence {
                            id: evidence_id.clone(),
                            revision: 1,
                            at_ms: 1,
                            source: EvidenceSource::CompletedInteraction {
                                message_id: format!("source-{index}"),
                                session_revision: 1,
                                turn_id: 1,
                                user_text: "先说结论".into(),
                                assistant_text: "好的".into(),
                            },
                        }],
                    },
                    status: JobStatus::Completed,
                    finished_at_ms: Some(2),
                    candidates: vec![PreferenceCandidate {
                        id: format!("candidate-{index}"),
                        batch_id,
                        draft: CandidateDraft {
                            text: format!("第 {index} 条：先说结论\n然后给必要细节"),
                            confidence: 80,
                            evidence_ids: vec![evidence_id],
                        },
                        created_at_ms: 2,
                        expires_at_ms: u64::MAX,
                    }],
                }
            })
            .collect();
        LearningSnapshot { scope, jobs }
    }
    fn learning(session: &SessionKey, count: usize) -> Arc<FixedLearning> {
        let learning = Arc::new(FixedLearning::default());
        let scope = crate::qq_memory::scope(session);
        learning
            .snapshots
            .lock()
            .unwrap()
            .insert(scope.clone(), fixture(scope, count));
        learning
    }
    pub(super) fn snapshot(admin: &dyn MemoryAdmin, session: &SessionKey) -> MemorySnapshot {
        admin
            .reader(crate::qq_memory::scope(session))
            .unwrap()
            .snapshot()
            .unwrap()
    }

    #[test]
    fn exact_command_parser_recognizes_bad_arguments_without_exposing_body() {
        for text in [
            "/memory-candidates-extra",
            "正文 /accept-memory id",
            "/accept-memories id",
        ] {
            assert_eq!(parse(text), None);
        }
        for text in [
            "/memory-candidates 0",
            "/memory-candidates -1",
            "/memory-candidates two",
            "/memory-candidates 1 extra",
            "/memory-candidates 999999999999999999999999999999999999",
            "/accept-memory",
            "/accept-memory id extra",
            "/accept-memory id\0",
        ] {
            assert_eq!(parse(text), Some(Command::Help));
        }
        assert_eq!(parse(" /memory-candidates "), Some(Command::Candidates(1)));
        assert_eq!(parse("/memory-candidates 2"), Some(Command::Candidates(2)));
        assert_eq!(parse("  /accept-memory\tid  "), Some(Command::Accept("id")));
        assert!(
            !format!("{:?}", Command::Accept("private-candidate")).contains("private-candidate")
        );
        let commands = Commands::disabled();
        for text in ["/memory-candidates", "/accept-memory id", "/accept-memory"] {
            assert_eq!(
                run(&commands, &session(), "message", text)
                    .unwrap()
                    .as_deref(),
                Some("偏好提炼未启用。")
            );
        }
        assert!(
            run(&commands, &session(), "message", "普通聊天")
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn one_atomic_confirmation_keeps_original_command_and_replays_read_only_after_restart() {
        let store = Arc::new(RecordingStore::default());
        let (kernel, admin) = memory(store.clone()).await;
        let session = session();
        let learning = learning(&session, 1);
        let commands = Commands::new(learning.clone(), Arc::new(admin.clone()));
        let text = "  /accept-memory\tcandidate-0  ";
        let message_id = "x".repeat(253);
        let reply = run(&commands, &session, &message_id, text)
            .unwrap()
            .unwrap();
        assert_eq!(reply, "候选偏好已确认：learned-candidate-0。");
        let saved = snapshot(&admin, &session);
        assert_eq!(saved.revision, 1);
        assert_eq!(saved.preferences.len(), 1);
        assert_eq!(saved.evidence.len(), 1);
        assert!(matches!(
            &saved.evidence[0].source,
            EvidenceSource::UserStatement { text: original, message_id: original_id }
                if original == text && original_id == &message_id
        ));
        assert_eq!(
            saved.preferences[0].history[0].evidence_id,
            saved.evidence[0].id
        );
        assert!(saved.evidence[0].id.len() <= 256);
        assert_eq!(store.writes.load(Ordering::SeqCst), 1);
        for id in [message_id.as_str(), "another-message"] {
            assert_eq!(run(&commands, &session, id, text).unwrap().unwrap(), reply);
        }
        assert_eq!(snapshot(&admin, &session), saved);
        kernel.stop_all().await.unwrap();
        let (kernel, reopened) = memory(store.clone()).await;
        let commands = Commands::new(learning, Arc::new(reopened.clone()));
        assert_eq!(
            run(&commands, &session, "after-restart", text)
                .unwrap()
                .unwrap(),
            reply
        );
        assert_eq!(snapshot(&reopened, &session), saved);
        assert_eq!(store.writes.load(Ordering::SeqCst), 1);
        kernel.stop_all().await.unwrap();
    }

    #[tokio::test]
    async fn correction_and_revocation_join_current_memory_without_resurrecting_preferences() {
        let store = Arc::new(RecordingStore::default());
        let (kernel, admin) = memory(store.clone()).await;
        let session = session();
        let commands = Commands::new(learning(&session, 1), Arc::new(admin.clone()));
        run(&commands, &session, "accept", "/accept-memory candidate-0").unwrap();
        let ordinary = crate::qq_memory::Commands::new(Arc::new(admin.clone()));
        run_memory(
            &ordinary,
            &session,
            "correct",
            "/correct-memory learned-candidate-0 新正文",
        );
        let reply = run(&commands, &session, "again", "/accept-memory candidate-0")
            .unwrap()
            .unwrap();
        assert!(reply.contains("后续修改"));
        assert!(
            run(&commands, &session, "list", "/memory-candidates")
                .unwrap()
                .unwrap()
                .contains("状态：后续修改")
        );
        run_memory(&ordinary, &session, "revoke", "/forget learned-candidate-0");
        let saved = snapshot(&admin, &session);
        let reply = run(&commands, &session, "again-2", "/accept-memory candidate-0")
            .unwrap()
            .unwrap();
        assert!(reply.contains("目前已撤销"));
        assert!(
            run(&commands, &session, "list-2", "/memory-candidates")
                .unwrap()
                .unwrap()
                .contains("状态：已撤销")
        );
        assert_eq!(snapshot(&admin, &session), saved);
        assert_eq!(store.writes.load(Ordering::SeqCst), 3);
        kernel.stop_all().await.unwrap();
    }
    pub(super) fn run_memory(
        commands: &crate::qq_memory::Commands,
        session: &SessionKey,
        id: &str,
        text: &str,
    ) {
        commands
            .handle(QqCommandInput {
                session,
                message_id: id,
                text,
            })
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn candidates_and_acceptance_stay_in_full_app_group_and_user_scope() {
        let store = Arc::new(RecordingStore::default());
        let (kernel, admin) = memory(store.clone()).await;
        let owner = session();
        let commands = Commands::new(learning(&owner, 1), Arc::new(admin.clone()));
        for foreign in [
            SessionKey::new("other-app", &owner.user_id).unwrap(),
            SessionKey::new("other-group", &owner.user_id).unwrap(),
            SessionKey::new(&owner.session_id, "other-user").unwrap(),
        ] {
            let list = run(&commands, &foreign, "list", "/memory-candidates")
                .unwrap()
                .unwrap();
            assert!(!list.contains("candidate-0"));
            assert!(
                run(&commands, &foreign, "accept", "/accept-memory candidate-0")
                    .unwrap()
                    .unwrap()
                    .contains("没有这条候选")
            );
            assert!(snapshot(&admin, &foreign).preferences.is_empty());
        }
        assert_eq!(store.writes.load(Ordering::SeqCst), 0);
        kernel.stop_all().await.unwrap();
    }

    #[tokio::test]
    async fn list_keeps_full_candidate_text_metadata_and_five_item_pages_without_writes() {
        let store = Arc::new(RecordingStore::default());
        let (kernel, admin) = memory(store.clone()).await;
        let session = session();
        let learning = learning(&session, 11);
        let scope = crate::qq_memory::scope(&session);
        let full = "完整正文\n".to_owned() + &"好".repeat(330);
        for job in &mut learning
            .snapshots
            .lock()
            .unwrap()
            .get_mut(&scope)
            .unwrap()
            .jobs
        {
            job.candidates[0].draft.text = full.clone();
        }
        let commands = Commands::new(learning, Arc::new(admin));
        for (page, count) in [(1, 5), (2, 5), (3, 1)] {
            let reply = run(
                &commands,
                &session,
                "list",
                &format!("/memory-candidates {page}"),
            )
            .unwrap()
            .unwrap();
            assert_eq!(reply.matches("候选 ID：").count(), count);
            assert_eq!(reply.matches(&full).count(), count);
            assert_eq!(reply.matches("模型自评：80%").count(), count);
            assert_eq!(reply.matches("Unix 毫秒").count(), count);
            assert_eq!(reply.matches("来源证据：completed-").count(), count);
            assert_eq!(reply.matches("状态：待确认").count(), count);
            assert!(reply.len() < MAX_TEXT_BYTES);
        }
        assert!(
            run(
                &commands,
                &session,
                "huge",
                &format!("/memory-candidates {}", usize::MAX)
            )
            .unwrap()
            .unwrap()
            .contains("共有 3 页")
        );
        assert_eq!(store.writes.load(Ordering::SeqCst), 0);
        kernel.stop_all().await.unwrap();
    }

    fn seed_confirmation(admin: &MemoryController, scope: &MemoryScope, text: &str, command: &str) {
        admin
            .update_preference(
                scope,
                0,
                PreferenceChange {
                    operation_id: "historical-confirmation".into(),
                    at_ms: 3,
                    evidence: PreferenceEvidence::Statement(UserStatement {
                        evidence_id: "historical-command".into(),
                        message_id: "historical-message".into(),
                        text: command.into(),
                        at_ms: 3,
                    }),
                    action: PreferenceAction::Confirm {
                        id: "learned-candidate-0".into(),
                        text: text.into(),
                    },
                },
            )
            .unwrap();
    }

    #[tokio::test]
    async fn expiry_only_rejects_first_confirmation_and_keeps_existing_confirmation_readable() {
        for preconfirmed in [false, true] {
            let store = Arc::new(RecordingStore::default());
            let (kernel, admin) = memory(store.clone()).await;
            let session = session();
            let scope = crate::qq_memory::scope(&session);
            let learning = learning(&session, 1);
            {
                let mut snapshots = learning.snapshots.lock().unwrap();
                let candidate = &mut snapshots.get_mut(&scope).unwrap().jobs[0].candidates[0];
                candidate.expires_at_ms = 4;
                if preconfirmed {
                    seed_confirmation(
                        &admin,
                        &scope,
                        &candidate.draft.text,
                        "/accept-memory candidate-0",
                    );
                }
            }
            let commands = Commands::new(learning, Arc::new(admin));
            let reply = run(
                &commands,
                &session,
                "accept-now",
                "/accept-memory candidate-0",
            )
            .unwrap()
            .unwrap();
            assert!(reply.contains(if preconfirmed {
                "已确认"
            } else {
                "已过期"
            }));
            let listing = run(&commands, &session, "list", "/memory-candidates")
                .unwrap()
                .unwrap();
            assert!(listing.contains("已过期"));
            assert!(listing.contains(if preconfirmed {
                "状态：已保存"
            } else {
                "状态：待确认"
            }));
            assert_eq!(
                store.writes.load(Ordering::SeqCst),
                usize::from(preconfirmed)
            );
            kernel.stop_all().await.unwrap();
        }
    }

    #[tokio::test]
    async fn fixed_preference_id_cannot_spoof_candidate_confirmation_or_cross_scope_snapshot() {
        for (text, command) in [
            ("不同正文", "/accept-memory candidate-0"),
            (
                "第 0 条：先说结论\n然后给必要细节",
                "/remember 第 0 条：先说结论",
            ),
            (
                "第 0 条：先说结论\n然后给必要细节",
                "/accept-memory another-candidate",
            ),
        ] {
            let store = Arc::new(RecordingStore::default());
            let (kernel, admin) = memory(store.clone()).await;
            let session = session();
            seed_confirmation(&admin, &crate::qq_memory::scope(&session), text, command);
            let commands = Commands::new(learning(&session, 1), Arc::new(admin));
            for command in ["/memory-candidates", "/accept-memory candidate-0"] {
                assert!(matches!(
                    run(&commands, &session, "read", command),
                    Err(PluginError::State(_))
                ));
            }
            assert_eq!(store.writes.load(Ordering::SeqCst), 1);
            kernel.stop_all().await.unwrap();
        }
        let store = Arc::new(RecordingStore::default());
        let (kernel, admin) = memory(store.clone()).await;
        let session = session();
        let learning = learning(&session, 1);
        learning
            .snapshots
            .lock()
            .unwrap()
            .get_mut(&crate::qq_memory::scope(&session))
            .unwrap()
            .scope
            .user_id = "foreign".into();
        let commands = Commands::new(learning, Arc::new(admin));
        assert!(matches!(
            run(&commands, &session, "accept", "/accept-memory candidate-0"),
            Err(PluginError::State(_))
        ));
        assert_eq!(store.writes.load(Ordering::SeqCst), 0);
        kernel.stop_all().await.unwrap();
    }

    #[tokio::test]
    async fn cas_conflict_is_not_retried_and_uncertain_memory_state_fails_closed() {
        let store = Arc::new(RecordingStore::default());
        let (kernel, admin) = memory(store.clone()).await;
        let session = session();
        for error in [
            MemoryError::StaleRevision,
            MemoryError::Conflict,
            MemoryError::LimitReached,
            MemoryError::Storage,
            MemoryError::Unavailable,
            MemoryError::CorruptState,
            MemoryError::UnsupportedVersion,
        ] {
            for read in [false, true] {
                let faults = Arc::new(MemoryFaults {
                    inner: admin.clone(),
                    read_error: Mutex::new(read.then(|| error.clone())),
                    write_error: Mutex::new((!read).then(|| error.clone())),
                    writes: AtomicUsize::new(0),
                });
                let commands = Commands::new(learning(&session, 1), faults.clone());
                let result = run(&commands, &session, "accept", "/accept-memory candidate-0");
                if matches!(
                    error,
                    MemoryError::StaleRevision | MemoryError::Conflict | MemoryError::LimitReached
                ) {
                    let reply = result.unwrap().unwrap();
                    assert!(!reply.contains("已确认"));
                    assert!(reply.contains("新消息") || reply.contains("容量已满"));
                } else {
                    assert!(matches!(result, Err(PluginError::State(_))));
                }
                assert_eq!(faults.writes.load(Ordering::SeqCst), usize::from(!read));
            }
        }
        assert_eq!(store.writes.load(Ordering::SeqCst), 0);
        kernel.stop_all().await.unwrap();
    }

    #[tokio::test]
    async fn committed_but_unacknowledged_confirmation_recovers_by_join_without_rewriting() {
        let store = Arc::new(RecordingStore::default());
        let (kernel, admin) = memory(store.clone()).await;
        let session = session();
        let learning = learning(&session, 1);
        let commands = Commands::new(learning.clone(), Arc::new(admin));
        store.fail_after_commit.store(true, Ordering::SeqCst);
        assert!(matches!(
            run(&commands, &session, "accept", "/accept-memory candidate-0"),
            Err(PluginError::State(_))
        ));
        assert!(matches!(
            run(&commands, &session, "read", "/memory-candidates"),
            Err(PluginError::State(_))
        ));
        assert_eq!(store.writes.load(Ordering::SeqCst), 1);
        kernel.stop_all().await.unwrap();
        store.fail_after_commit.store(false, Ordering::SeqCst);
        let (kernel, reopened) = memory(store.clone()).await;
        let commands = Commands::new(learning, Arc::new(reopened.clone()));
        assert!(
            run(
                &commands,
                &session,
                "accept-after-recovery",
                "/accept-memory candidate-0"
            )
            .unwrap()
            .unwrap()
            .contains("已确认")
        );
        assert_eq!(snapshot(&reopened, &session).revision, 1);
        assert_eq!(store.writes.load(Ordering::SeqCst), 1);
        kernel.stop_all().await.unwrap();
    }

    #[tokio::test]
    async fn one_message_cannot_confirm_different_candidates_by_reusing_its_evidence() {
        let store = Arc::new(RecordingStore::default());
        let (kernel, admin) = memory(store.clone()).await;
        let session = session();
        let commands = Commands::new(learning(&session, 2), Arc::new(admin.clone()));
        run(
            &commands,
            &session,
            "same-message",
            "/accept-memory candidate-0",
        )
        .unwrap();
        let reply = run(
            &commands,
            &session,
            "same-message",
            "/accept-memory candidate-1",
        )
        .unwrap()
        .unwrap();
        assert!(reply.contains("冲突"));
        assert!(reply.contains("新消息"));
        assert_eq!(snapshot(&admin, &session).preferences.len(), 1);
        assert_eq!(store.writes.load(Ordering::SeqCst), 1);
        assert!(
            run(
                &commands,
                &session,
                "new-message",
                "/accept-memory candidate-1"
            )
            .unwrap()
            .unwrap()
            .contains("已确认")
        );
        assert_eq!(snapshot(&admin, &session).preferences.len(), 2);
        assert_eq!(store.writes.load(Ordering::SeqCst), 2);
        kernel.stop_all().await.unwrap();
    }

    #[tokio::test]
    async fn learning_read_failures_and_invalid_candidate_evidence_never_write_memory() {
        let store = Arc::new(RecordingStore::default());
        let (kernel, admin) = memory(store.clone()).await;
        let session = session();
        let learning = learning(&session, 1);
        let commands = Commands::new(learning.clone(), Arc::new(admin.clone()));
        for error in [
            LearningError::Storage,
            LearningError::Unavailable,
            LearningError::CorruptState,
            LearningError::UnsupportedVersion,
            LearningError::Memory(MemoryError::Storage),
        ] {
            *learning.error.lock().unwrap() = Some(error);
            for text in ["/memory-candidates", "/accept-memory candidate-0"] {
                assert!(matches!(
                    run(&commands, &session, "read", text),
                    Err(PluginError::State(_))
                ));
            }
        }
        *learning.error.lock().unwrap() = None;
        let scope = crate::qq_memory::scope(&session);
        let valid = fixture(scope.clone(), 1);
        for invalid in 0..5 {
            let mut snapshot = valid.clone();
            match invalid {
                0 => {
                    snapshot.jobs[0].candidates[0].draft.evidence_ids =
                        vec!["foreign-evidence".into()]
                }
                1 => snapshot.jobs[0].candidates[0].draft.confidence = 101,
                2 => {
                    snapshot.jobs[0].candidates[0].draft.text = "a".repeat(MAX_CANDIDATE_BYTES + 1)
                }
                3 => snapshot.jobs[0].status = JobStatus::Interrupted,
                4 => snapshot.jobs.push(snapshot.jobs[0].clone()),
                _ => unreachable!(),
            }
            learning
                .snapshots
                .lock()
                .unwrap()
                .insert(scope.clone(), snapshot);
            assert!(matches!(
                run(&commands, &session, "read", "/memory-candidates"),
                Err(PluginError::State(_))
            ));
        }
        assert_eq!(store.writes.load(Ordering::SeqCst), 0);
        assert!(snapshot(&admin, &session).preferences.is_empty());
        kernel.stop_all().await.unwrap();
    }

    #[test]
    fn largest_valid_candidate_page_preserves_text_ids_and_stays_below_reply_limit() {
        let scope = crate::qq_memory::scope(&session());
        let mut learning = fixture(scope.clone(), PAGE_SIZE);
        for (index, job) in learning.jobs.iter_mut().enumerate() {
            let candidate = &mut job.candidates[0];
            candidate.id = format!("{index}{}", "i".repeat(247));
            candidate.draft.text = "t".repeat(MAX_CANDIDATE_BYTES);
            candidate.draft.evidence_ids.clear();
            job.batch.evidence.clear();
            for source in 0..MAX_BATCH_EVIDENCE {
                let id = format!("{source}{}", "e".repeat(255));
                candidate.draft.evidence_ids.push(id.clone());
                job.batch.evidence.push(InteractionEvidence {
                    id,
                    revision: source as u64 + 1,
                    at_ms: 1,
                    source: EvidenceSource::CompletedInteraction {
                        message_id: format!("source-{source}"),
                        session_revision: 1,
                        turn_id: source as u64 + 1,
                        user_text: "原话".into(),
                        assistant_text: "回复".into(),
                    },
                });
            }
        }
        let memory = MemorySnapshot {
            scope: scope.clone(),
            revision: 0,
            evidence: vec![],
            preferences: vec![],
        };
        let selected = candidates(&learning, &scope).unwrap();
        let reply = list(&selected, &memory, &[], 1, 3).unwrap();
        assert!(reply.len() < MAX_TEXT_BYTES);
        assert_eq!(
            reply.matches(&"t".repeat(MAX_CANDIDATE_BYTES)).count(),
            PAGE_SIZE
        );
        for candidate in selected {
            assert!(reply.contains(&candidate.id));
            for id in &candidate.draft.evidence_ids {
                assert!(reply.contains(id));
            }
        }
    }
}

#[cfg(test)]
#[path = "qq_learning_decision_tests.rs"]
mod decision_tests;
