use crate::strict_json;
use eve_plugin_api::{PluginContext, PluginError, PluginResult};
use eve_practice_api::{PracticeDraft, RunEvidence, RunnerProfile, validate_artifact};
use eve_skill_api::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    sync::{Mutex, MutexGuard},
};

const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    format_version: u32,
    skills: Vec<Skill>,
    distillations: Vec<Distillation>,
    selections: Vec<Selection>,
    #[serde(default)]
    tool_calls: Vec<ToolCallRecord>,
}
impl Ledger {
    fn snapshot(&self) -> SkillSnapshot {
        SkillSnapshot {
            skills: self.skills.clone(),
            distillations: self.distillations.clone(),
            selections: self.selections.clone(),
            tool_calls: self.tool_calls.clone(),
        }
    }
}
struct Inner {
    ledger: Ledger,
    context: Option<PluginContext>,
}
pub(super) struct StoredSkills {
    inner: Mutex<Inner>,
}

impl StoredSkills {
    pub(super) fn open(context: PluginContext) -> SkillResult<Self> {
        let mut ledger = match context
            .state_get(SKILL_STATE_KEY)
            .map_err(|_| SkillError::Storage)?
        {
            None => Ledger {
                format_version: FORMAT_VERSION,
                skills: vec![],
                distillations: vec![],
                selections: vec![],
                tool_calls: vec![],
            },
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(SkillError::CorruptState);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| SkillError::CorruptState)?;
                if value.get("format_version").and_then(|value| value.as_u64())
                    != Some(u64::from(FORMAT_VERSION))
                {
                    return Err(if value.get("format_version").is_some() {
                        SkillError::UnsupportedVersion
                    } else {
                        SkillError::CorruptState
                    });
                }
                let ledger: Ledger =
                    serde_json::from_value(value).map_err(|_| SkillError::CorruptState)?;
                validate_ledger(&ledger).map_err(|_| SkillError::CorruptState)?;
                ledger
            }
        };
        let mut interrupted = false;
        for distillation in &mut ledger.distillations {
            if distillation.status == DistillStatus::Running {
                distillation.status = DistillStatus::Interrupted;
                interrupted = true;
            }
        }
        for selection in &mut ledger.selections {
            if selection.status == SelectionStatus::Running {
                selection.status = SelectionStatus::Interrupted;
                interrupted = true;
            }
        }
        for call in &mut ledger.tool_calls {
            if call.outcome.is_none() {
                call.outcome = Some(InvocationOutcome::Interrupted);
                interrupted = true;
            }
        }
        // 先保存中断结局再公开实例；不重放提炼、选择请求或验证运行，也不伪造完成时间。
        if interrupted {
            let bytes = encode(&ledger)?;
            context
                .state_set(SKILL_STATE_KEY, bytes)
                .map_err(|_| SkillError::Storage)?;
        }
        Ok(Self {
            inner: Mutex::new(Inner {
                ledger,
                context: Some(context),
            }),
        })
    }

    fn lock(&self) -> SkillResult<MutexGuard<'_, Inner>> {
        let inner = self.inner.lock().map_err(|_| SkillError::Unavailable)?;
        if inner.context.is_none() {
            return Err(SkillError::Unavailable);
        }
        Ok(inner)
    }

    pub(super) fn snapshot(&self) -> SkillResult<SkillSnapshot> {
        Ok(self.lock()?.ledger.snapshot())
    }

    pub(super) fn begin_distillation(
        &self,
        owner: &str,
        origin: SkillOrigin,
        source: PracticeDraft,
        runner: &RunnerProfile,
        distiller_version: &str,
        now_ms: u64,
    ) -> SkillResult<Option<Distillation>> {
        validate_id(owner)?;
        validate_origin(&origin)?;
        validate_id(distiller_version)?;
        runner.validate().map_err(|_| SkillError::InvalidInput)?;
        if now_ms == 0 || !source.applicable || validate_artifact(runner, &source).is_err() {
            return Err(SkillError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let id = distillation_id(&origin.practice_run_id);
        let ledger = &inner.ledger;
        // 同一次实践只提炼一次；同一时间只有一次提炼在进行。
        if ledger
            .distillations
            .iter()
            .any(|entry| entry.id == id || entry.status == DistillStatus::Running)
        {
            return Ok(None);
        }
        if ledger.distillations.len() >= MAX_DISTILLATIONS || ledger.skills.len() >= MAX_SKILLS {
            return Err(SkillError::LimitReached);
        }
        let distillation = Distillation {
            id,
            owner: owner.into(),
            origin,
            source,
            runner: runner.clone(),
            distiller_version: distiller_version.into(),
            started_at_ms: now_ms,
            finished_at_ms: None,
            stage: DistillStage::Proposing,
            status: DistillStatus::Running,
            proposal: None,
            issues: vec![],
            holdout: None,
            evidence: None,
            skill: None,
        };
        let mut next = inner.ledger.clone();
        next.distillations.push(distillation.clone());
        persist(&mut inner, next)?;
        Ok(Some(distillation))
    }

    pub(super) fn record_proposal(
        &self,
        id: &str,
        at_ms: u64,
        result: Result<Proposal, SkillFailure>,
        issues: Vec<String>,
        holdout: Option<Arguments>,
    ) -> SkillResult<Distillation> {
        validate_issues(&issues)?;
        self.update_distillation(id, at_ms, DistillStage::Proposing, |ledger, entry| {
            let rest = !issues.is_empty() || holdout.is_some();
            match result {
                Err(failure) => {
                    if rest {
                        return Err(SkillError::InvalidInput);
                    }
                    finish(entry, at_ms, DistillStatus::Failed(failure));
                }
                Ok(Proposal::NotReusable { reason }) => {
                    validate_reason(&reason)?;
                    if rest {
                        return Err(SkillError::InvalidInput);
                    }
                    entry.proposal = Some(Proposal::NotReusable { reason });
                    finish(entry, at_ms, DistillStatus::NotReusable);
                }
                Ok(Proposal::Skill(proposal)) => {
                    validate_proposal_shape(&proposal)?;
                    if !issues.is_empty() {
                        if holdout.is_some() {
                            return Err(SkillError::InvalidInput);
                        }
                        entry.issues = issues;
                        entry.proposal = Some(Proposal::Skill(proposal));
                        finish(entry, at_ms, DistillStatus::Rejected);
                    } else {
                        // 宿主报告没有问题时，账本按同样的规则复核，保证落盘的模板可以复验。
                        let (found, expected) =
                            proposal_issues(&proposal, &entry.source, &entry.runner);
                        let lineage = ledger.snapshot().lineage_issues(
                            &entry.owner,
                            &entry.runner.runner_id,
                            &proposal,
                        );
                        if !found.is_empty() || !lineage.is_empty() || expected != holdout {
                            return Err(SkillError::InvalidInput);
                        }
                        entry.proposal = Some(Proposal::Skill(proposal));
                        entry.holdout = holdout;
                        entry.stage = DistillStage::Verifying;
                    }
                }
            }
            Ok(())
        })
    }

    pub(super) fn record_verification(
        &self,
        id: &str,
        at_ms: u64,
        evidence: RunEvidence,
    ) -> SkillResult<Distillation> {
        self.update_distillation(id, at_ms, DistillStage::Verifying, |ledger, entry| {
            let draft = entry.holdout_draft().ok_or(SkillError::CorruptState)?;
            evidence
                .validate(&draft)
                .map_err(|_| SkillError::InvalidInput)?;
            let verified = evidence.verified(&draft);
            entry.evidence = Some(evidence);
            if !verified {
                finish(entry, at_ms, DistillStatus::Unverified);
                return Ok(());
            }
            let proposal = entry.template().ok_or(SkillError::CorruptState)?.clone();
            if !ledger
                .snapshot()
                .lineage_issues(&entry.owner, &entry.runner.runner_id, &proposal)
                .is_empty()
            {
                return Err(SkillError::Conflict);
            }
            let id = skill_id(&entry.owner, &entry.runner.runner_id, &proposal.name);
            let position = match ledger.skills.iter().position(|skill| skill.id == id) {
                Some(position) => position,
                None => {
                    ledger.skills.push(Skill {
                        id: id.clone(),
                        owner: entry.owner.clone(),
                        runner_id: entry.runner.runner_id.clone(),
                        name: proposal.name.clone(),
                        versions: vec![],
                        enabled: None,
                        changes: vec![],
                    });
                    ledger.skills.len() - 1
                }
            };
            let skill = &mut ledger.skills[position];
            let version = skill.latest().map_or(1, |latest| latest.version + 1);
            skill.versions.push(SkillVersion {
                version,
                distillation_id: entry.id.clone(),
                verified_at_ms: at_ms,
            });
            // 验证通过即在宿主既有授信范围内启用：技能只生成交给同一运行器的数据草稿。
            // 用户停用过的技能保持停用，直到用户重新启用。
            if !skill.held_by_owner() && skill.changes.len() < MAX_CHANGES {
                skill.enabled = Some(version);
                skill.changes.push(EnablementChange {
                    at_ms,
                    actor: Actor::Automatic,
                    enabled: Some(version),
                });
            }
            entry.skill = Some(SkillRef {
                skill_id: id,
                version,
            });
            finish(entry, at_ms, DistillStatus::Verified);
            Ok(())
        })
    }

    pub(super) fn abandon_distillation(
        &self,
        id: &str,
        at_ms: u64,
        failure: SkillFailure,
    ) -> SkillResult<Distillation> {
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let position = next
            .distillations
            .iter()
            .position(|entry| entry.id == id)
            .ok_or(SkillError::NotFound)?;
        let entry = &mut next.distillations[position];
        if entry.status != DistillStatus::Running {
            return Err(SkillError::Conflict);
        }
        if at_ms < entry.started_at_ms {
            return Err(SkillError::InvalidInput);
        }
        // 运行中的进程已由宿主终止，结果未知；只记录放弃原因，不补写证据。
        finish(entry, at_ms, DistillStatus::Failed(failure));
        let result = entry.clone();
        persist(&mut inner, next)?;
        Ok(result)
    }

    pub(super) fn set_enabled(
        &self,
        skill_id: &str,
        at_ms: u64,
        actor: Actor,
        enabled: Option<u32>,
    ) -> SkillResult<Skill> {
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let skill = next
            .skills
            .iter_mut()
            .find(|skill| skill.id == skill_id)
            .ok_or(SkillError::NotFound)?;
        if enabled.is_some_and(|version| skill.version(version).is_none()) {
            return Err(SkillError::InvalidInput);
        }
        if skill
            .changes
            .last()
            .is_some_and(|change| at_ms < change.at_ms)
            || at_ms == 0
        {
            return Err(SkillError::InvalidInput);
        }
        if skill.enabled == enabled {
            return Ok(skill.clone());
        }
        if skill.changes.len() >= MAX_CHANGES {
            return Err(SkillError::LimitReached);
        }
        skill.enabled = enabled;
        skill.changes.push(EnablementChange {
            at_ms,
            actor,
            enabled,
        });
        let result = skill.clone();
        persist(&mut inner, next)?;
        Ok(result)
    }

    pub(super) fn begin_selection(
        &self,
        id: &str,
        owner: &str,
        goal_id: &str,
        candidates: Vec<SkillRef>,
        selector_version: &str,
        now_ms: u64,
    ) -> SkillResult<Option<Selection>> {
        validate_id(id)?;
        validate_id(owner)?;
        validate_id(goal_id)?;
        validate_id(selector_version)?;
        if now_ms == 0 {
            return Err(SkillError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let ledger = &inner.ledger;
        if ledger
            .selections
            .iter()
            .any(|entry| entry.id == id || entry.status == SelectionStatus::Running)
        {
            return Ok(None);
        }
        let unique: BTreeSet<_> = candidates.iter().collect();
        if candidates.is_empty()
            || candidates.len() > MAX_CANDIDATES
            || unique.len() != candidates.len()
            || !candidates.iter().all(|candidate| {
                ledger.skills.iter().any(|skill| {
                    skill.id == candidate.skill_id
                        && skill.owner == owner
                        && skill.enabled == Some(candidate.version)
                })
            })
        {
            return Err(SkillError::InvalidInput);
        }
        if ledger.selections.len() >= MAX_SELECTIONS {
            return Err(SkillError::LimitReached);
        }
        let selection = Selection {
            id: id.into(),
            owner: owner.into(),
            goal_id: goal_id.into(),
            candidates,
            selector_version: selector_version.into(),
            started_at_ms: now_ms,
            finished_at_ms: None,
            status: SelectionStatus::Running,
            choice: None,
            issues: vec![],
            outcome: None,
            settled_at_ms: None,
        };
        let mut next = inner.ledger.clone();
        next.selections.push(selection.clone());
        persist(&mut inner, next)?;
        Ok(Some(selection))
    }

    pub(super) fn record_selection(
        &self,
        id: &str,
        at_ms: u64,
        result: Result<Option<Choice>, SkillFailure>,
        issues: Vec<String>,
    ) -> SkillResult<Selection> {
        validate_issues(&issues)?;
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let snapshot = next.snapshot();
        let entry = next
            .selections
            .iter_mut()
            .find(|entry| entry.id == id)
            .ok_or(SkillError::NotFound)?;
        if entry.status != SelectionStatus::Running {
            return Err(SkillError::Conflict);
        }
        if at_ms < entry.started_at_ms {
            return Err(SkillError::InvalidInput);
        }
        let status = match result {
            Err(failure) if issues.is_empty() => SelectionStatus::Failed(failure),
            Ok(None) if issues.is_empty() => SelectionStatus::Declined,
            Ok(Some(choice)) => {
                validate_choice_shape(&choice)?;
                let status = if issues.is_empty() {
                    if !chosen_is_usable(&snapshot, &entry.candidates, &choice) {
                        return Err(SkillError::InvalidInput);
                    }
                    SelectionStatus::Chosen
                } else {
                    SelectionStatus::Rejected
                };
                entry.choice = Some(choice);
                status
            }
            _ => return Err(SkillError::InvalidInput),
        };
        entry.issues = issues;
        entry.status = status;
        entry.finished_at_ms = Some(at_ms);
        let result = entry.clone();
        persist(&mut inner, next)?;
        Ok(result)
    }

    pub(super) fn settle(
        &self,
        id: &str,
        at_ms: u64,
        outcome: InvocationOutcome,
    ) -> SkillResult<Selection> {
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let entry = next
            .selections
            .iter_mut()
            .find(|entry| entry.id == id)
            .ok_or(SkillError::NotFound)?;
        if entry.status != SelectionStatus::Chosen || entry.outcome.is_some() {
            return Err(SkillError::Conflict);
        }
        if entry
            .finished_at_ms
            .is_some_and(|finished| at_ms < finished)
        {
            return Err(SkillError::InvalidInput);
        }
        entry.outcome = Some(outcome);
        entry.settled_at_ms = Some(at_ms);
        let result = entry.clone();
        persist(&mut inner, next)?;
        Ok(result)
    }

    pub(super) fn begin_tool_call(
        &self,
        id: &str,
        owner: &str,
        skill: SkillRef,
        arguments: Arguments,
        now_ms: u64,
    ) -> SkillResult<ToolCallRecord> {
        validate_id(id)?;
        validate_id(owner)?;
        if now_ms == 0 {
            return Err(SkillError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let snapshot = inner.ledger.snapshot();
        if inner
            .ledger
            .tool_calls
            .iter()
            .any(|call| call.id == id || call.outcome.is_none())
        {
            return Err(SkillError::Conflict);
        }
        let usable = snapshot
            .skill(&skill.skill_id)
            .is_some_and(|entry| entry.owner == owner && entry.enabled == Some(skill.version));
        let template = snapshot.source(&skill).and_then(|source| source.template());
        match template {
            Some(proposal)
                if usable && argument_issues(&proposal.template, &arguments).is_empty() => {}
            _ => return Err(SkillError::InvalidInput),
        }
        if inner.ledger.tool_calls.len() >= MAX_TOOL_CALLS {
            return Err(SkillError::LimitReached);
        }
        let call = ToolCallRecord {
            id: id.into(),
            owner: owner.into(),
            skill,
            arguments,
            started_at_ms: now_ms,
            finished_at_ms: None,
            outcome: None,
            evidence: None,
        };
        let mut next = inner.ledger.clone();
        next.tool_calls.push(call.clone());
        persist(&mut inner, next)?;
        Ok(call)
    }

    pub(super) fn record_tool_call(
        &self,
        id: &str,
        at_ms: u64,
        outcome: InvocationOutcome,
        evidence: Option<RunEvidence>,
    ) -> SkillResult<ToolCallRecord> {
        let mut inner = self.lock()?;
        let snapshot = inner.ledger.snapshot();
        let mut next = inner.ledger.clone();
        let call = next
            .tool_calls
            .iter_mut()
            .find(|call| call.id == id)
            .ok_or(SkillError::NotFound)?;
        if call.outcome.is_some() || at_ms < call.started_at_ms {
            return Err(SkillError::Conflict);
        }
        call.outcome = Some(outcome);
        call.evidence = evidence;
        call.finished_at_ms = Some(at_ms);
        validate_tool_call(call, &snapshot)?;
        let call = call.clone();
        persist(&mut inner, next)?;
        Ok(call)
    }

    fn update_distillation(
        &self,
        id: &str,
        at_ms: u64,
        stage: DistillStage,
        apply: impl FnOnce(&mut Ledger, &mut Distillation) -> SkillResult<()>,
    ) -> SkillResult<Distillation> {
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let position = next
            .distillations
            .iter()
            .position(|entry| entry.id == id)
            .ok_or(SkillError::NotFound)?;
        let mut entry = next.distillations[position].clone();
        if entry.status != DistillStatus::Running || entry.stage != stage {
            return Err(SkillError::Conflict);
        }
        if at_ms < entry.started_at_ms {
            return Err(SkillError::InvalidInput);
        }
        apply(&mut next, &mut entry)?;
        next.distillations[position] = entry.clone();
        persist(&mut inner, next)?;
        Ok(entry)
    }

    pub(super) fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("技能状态锁不可用".into()))?
            .context = None;
        Ok(())
    }
}

fn finish(entry: &mut Distillation, at_ms: u64, status: DistillStatus) {
    entry.status = status;
    entry.finished_at_ms = Some(at_ms);
}

fn validate_origin(origin: &SkillOrigin) -> SkillResult<()> {
    validate_id(&origin.practice_run_id)?;
    validate_id(&origin.goal_id)?;
    if origin.attempt == 0 || origin.attempt > eve_practice_api::MAX_ATTEMPTS {
        return Err(SkillError::InvalidInput);
    }
    Ok(())
}

/// 账本只接受有界的提案；内容是否可用由 `proposal_issues` 判断并作为问题记录。
fn validate_proposal_shape(proposal: &SkillProposal) -> SkillResult<()> {
    let bytes = serde_json::to_vec(proposal).map_err(|_| SkillError::InvalidInput)?;
    if bytes.len() > MAX_PROPOSAL_OUTPUT_BYTES
        || proposal
            .extends
            .as_ref()
            .is_some_and(|id| validate_id(id).is_err())
        || proposal.arguments.len() > MAX_PARAMETERS
    {
        return Err(SkillError::InvalidInput);
    }
    Ok(())
}

fn validate_choice_shape(choice: &Choice) -> SkillResult<()> {
    validate_id(&choice.skill.skill_id)?;
    validate_reason(&choice.reason)?;
    if choice.arguments.len() > MAX_PARAMETERS
        || choice.arguments.iter().any(|(key, value)| {
            key.len() > MAX_NAME_BYTES
                || value.len() > MAX_NAME_BYTES * 2
                || key.chars().chain(value.chars()).any(char::is_control)
        })
    {
        return Err(SkillError::InvalidInput);
    }
    Ok(())
}

/// 选定的版本在候选中，且参数可以实例化该版本的模板。
fn chosen_is_usable(snapshot: &SkillSnapshot, candidates: &[SkillRef], choice: &Choice) -> bool {
    candidates.contains(&choice.skill)
        && snapshot
            .source(&choice.skill)
            .and_then(Distillation::template)
            .is_some_and(|proposal| instantiate(&proposal.template, &choice.arguments).is_ok())
}

fn persist(inner: &mut Inner, next: Ledger) -> SkillResult<()> {
    let bytes = encode(&next)?;
    let context = inner.context.as_ref().ok_or(SkillError::Unavailable)?;
    // 失败也可能已经提交；关闭整个实例，禁止旧缓存继续读取或覆盖后端。
    if context.state_set(SKILL_STATE_KEY, bytes).is_err() {
        inner.context = None;
        return Err(SkillError::Storage);
    }
    inner.ledger = next;
    Ok(())
}

fn encode(ledger: &Ledger) -> SkillResult<Vec<u8>> {
    if ledger.skills.len() > MAX_SKILLS
        || ledger.distillations.len() > MAX_DISTILLATIONS
        || ledger.selections.len() > MAX_SELECTIONS
        || ledger.tool_calls.len() > MAX_TOOL_CALLS
    {
        return Err(SkillError::LimitReached);
    }
    let bytes = serde_json::to_vec(ledger).map_err(|_| SkillError::InvalidInput)?;
    // Interrupted 比 Running 多四个字节；写入时预留，保证下次启动能保存中断结局。
    let running = ledger
        .distillations
        .iter()
        .filter(|entry| entry.status == DistillStatus::Running)
        .count()
        + ledger
            .selections
            .iter()
            .filter(|entry| entry.status == SelectionStatus::Running)
            .count();
    // 运行中的工具调用重启时写入 "Interrupted"，比 null 多出的字节同样预留。
    let calling = ledger
        .tool_calls
        .iter()
        .filter(|call| call.outcome.is_none())
        .count();
    let recovery = running * ("Interrupted".len() - "Running".len())
        + calling * ("\"Interrupted\"".len() - "null".len());
    if bytes.len().saturating_add(recovery) > MAX_STATE_BYTES {
        return Err(SkillError::LimitReached);
    }
    Ok(bytes)
}

/// 启动时完整核对；任何不一致都拒绝打开并保留原字节。
fn validate_ledger(ledger: &Ledger) -> SkillResult<()> {
    let invalid = || SkillError::InvalidInput;
    if ledger.format_version != FORMAT_VERSION
        || ledger.skills.len() > MAX_SKILLS
        || ledger.distillations.len() > MAX_DISTILLATIONS
        || ledger.selections.len() > MAX_SELECTIONS
        || ledger.tool_calls.len() > MAX_TOOL_CALLS
    {
        return Err(invalid());
    }
    let snapshot = ledger.snapshot();
    let mut ids = BTreeSet::new();
    let mut running = 0;
    for entry in &ledger.distillations {
        validate_distillation(entry, &snapshot)?;
        if !ids.insert(entry.id.as_str()) {
            return Err(invalid());
        }
        running += usize::from(entry.status == DistillStatus::Running);
    }
    let mut skill_ids = BTreeSet::new();
    for skill in &ledger.skills {
        validate_skill(skill, &snapshot)?;
        if !skill_ids.insert(skill.id.as_str()) {
            return Err(invalid());
        }
    }
    let mut selection_ids = BTreeSet::new();
    let mut selecting = 0;
    for selection in &ledger.selections {
        validate_selection(selection, &snapshot)?;
        if !selection_ids.insert(selection.id.as_str()) {
            return Err(invalid());
        }
        selecting += usize::from(selection.status == SelectionStatus::Running);
    }
    let mut call_ids = BTreeSet::new();
    let mut calling = 0;
    for call in &ledger.tool_calls {
        validate_tool_call(call, &snapshot)?;
        if !call_ids.insert(call.id.as_str()) {
            return Err(invalid());
        }
        calling += usize::from(call.outcome.is_none());
    }
    if running > 1 || selecting > 1 || calling > 1 {
        return Err(invalid());
    }
    Ok(())
}

/// 工具调用：技能与版本属于该用户、参数合规；Verified 的证据满足验证条件，
/// 运行过的结论带证据，进行中或中断的没有完成时间与证据。
fn validate_tool_call(call: &ToolCallRecord, snapshot: &SkillSnapshot) -> SkillResult<()> {
    let invalid = || SkillError::InvalidInput;
    validate_id(&call.id)?;
    validate_id(&call.owner)?;
    let template = snapshot
        .skill(&call.skill.skill_id)
        .filter(|skill| skill.owner == call.owner)
        .and_then(|_| snapshot.source(&call.skill))
        .and_then(|source| source.template())
        .ok_or_else(invalid)?;
    let draft = instantiate(&template.template, &call.arguments).map_err(|_| invalid())?;
    if call.started_at_ms == 0 {
        return Err(invalid());
    }
    if let Some(evidence) = &call.evidence {
        evidence.validate(&draft).map_err(|_| invalid())?;
    }
    let consistent = match (call.outcome, call.finished_at_ms, &call.evidence) {
        (None | Some(InvocationOutcome::Interrupted), None, None) => true,
        (Some(InvocationOutcome::Verified), Some(at), Some(evidence)) => {
            at >= call.started_at_ms && evidence.verified(&draft)
        }
        (Some(InvocationOutcome::Failed), Some(at), Some(evidence)) => {
            at >= call.started_at_ms && !evidence.verified(&draft)
        }
        (Some(InvocationOutcome::Rejected | InvocationOutcome::Abandoned), Some(at), None) => {
            at >= call.started_at_ms
        }
        _ => false,
    };
    if !consistent {
        return Err(invalid());
    }
    Ok(())
}

fn validate_distillation(entry: &Distillation, snapshot: &SkillSnapshot) -> SkillResult<()> {
    let invalid = || SkillError::InvalidInput;
    validate_id(&entry.owner)?;
    validate_origin(&entry.origin)?;
    validate_id(&entry.distiller_version)?;
    validate_issues(&entry.issues)?;
    entry.runner.validate().map_err(|_| invalid())?;
    if entry.id != distillation_id(&entry.origin.practice_run_id)
        || entry.started_at_ms == 0
        || !entry.source.applicable
        || validate_artifact(&entry.runner, &entry.source).is_err()
    {
        return Err(invalid());
    }
    let open = matches!(
        entry.status,
        DistillStatus::Running | DistillStatus::Interrupted
    );
    match entry.finished_at_ms {
        None if open => {}
        Some(at) if !open && at >= entry.started_at_ms => {}
        _ => return Err(invalid()),
    }
    if let Some(proposal) = entry.template() {
        validate_proposal_shape(proposal)?;
    }
    let verifying = entry.stage == DistillStage::Verifying;
    let consistent = match entry.status {
        DistillStatus::NotReusable => {
            !verifying
                && matches!(&entry.proposal, Some(Proposal::NotReusable { reason }) if validate_reason(reason).is_ok())
                && entry.issues.is_empty()
                && entry.holdout.is_none()
        }
        DistillStatus::Rejected => {
            !verifying
                && entry.template().is_some()
                && !entry.issues.is_empty()
                && entry.holdout.is_none()
        }
        _ if !verifying => {
            entry.proposal.is_none() && entry.issues.is_empty() && entry.holdout.is_none()
        }
        _ => {
            // 进入验证的模板必须能复核：通过检查、逐字还原来源草稿、验证参数由宿主规则得出。
            let proposal = entry.template().ok_or_else(invalid)?;
            let (found, expected) = proposal_issues(proposal, &entry.source, &entry.runner);
            found.is_empty() && entry.issues.is_empty() && expected == entry.holdout
        }
    };
    let draft = entry.holdout_draft();
    let evidence_ok = match (&entry.evidence, entry.status) {
        (None, DistillStatus::Verified | DistillStatus::Unverified) => false,
        (None, _) => true,
        (Some(evidence), DistillStatus::Verified) => draft
            .as_ref()
            .is_some_and(|draft| evidence.validate(draft).is_ok() && evidence.verified(draft)),
        (Some(evidence), DistillStatus::Unverified) => draft
            .as_ref()
            .is_some_and(|draft| evidence.validate(draft).is_ok() && !evidence.verified(draft)),
        (Some(_), _) => false,
    };
    let skill_ok = match (&entry.skill, entry.status) {
        (Some(skill), DistillStatus::Verified) => snapshot
            .skill(&skill.skill_id)
            .and_then(|found| found.version(skill.version))
            .is_some_and(|version| version.distillation_id == entry.id),
        (None, DistillStatus::Verified) => false,
        (None, _) => true,
        (Some(_), _) => false,
    };
    if !consistent
        || !evidence_ok
        || !skill_ok
        || (matches!(
            entry.status,
            DistillStatus::Verified | DistillStatus::Unverified
        ) && !verifying)
    {
        return Err(invalid());
    }
    Ok(())
}

fn validate_skill(skill: &Skill, snapshot: &SkillSnapshot) -> SkillResult<()> {
    let invalid = || SkillError::InvalidInput;
    validate_id(&skill.owner)?;
    validate_id(&skill.runner_id)?;
    if !is_identifier(&skill.name)
        || skill.id != skill_id(&skill.owner, &skill.runner_id, &skill.name)
        || skill.versions.is_empty()
        || skill.versions.len() > MAX_VERSIONS
        || skill.changes.len() > MAX_CHANGES
    {
        return Err(invalid());
    }
    for (index, version) in skill.versions.iter().enumerate() {
        let source = snapshot
            .distillation(&version.distillation_id)
            .ok_or_else(invalid)?;
        let expected = SkillRef {
            skill_id: skill.id.clone(),
            version: version.version,
        };
        if version.version as usize != index + 1
            || source.status != DistillStatus::Verified
            || source.skill.as_ref() != Some(&expected)
            || source.owner != skill.owner
            || source.runner.runner_id != skill.runner_id
            || source
                .template()
                .is_none_or(|proposal| proposal.name != skill.name)
            || source.finished_at_ms != Some(version.verified_at_ms)
        {
            return Err(invalid());
        }
    }
    let mut previous = 0;
    for change in &skill.changes {
        if change.at_ms < previous
            || change
                .enabled
                .is_some_and(|version| skill.version(version).is_none())
        {
            return Err(invalid());
        }
        previous = change.at_ms;
    }
    if skill.changes.last().map(|change| change.enabled) != Some(skill.enabled)
        && !(skill.changes.is_empty() && skill.enabled.is_none())
    {
        return Err(invalid());
    }
    Ok(())
}

fn validate_selection(selection: &Selection, snapshot: &SkillSnapshot) -> SkillResult<()> {
    let invalid = || SkillError::InvalidInput;
    validate_id(&selection.id)?;
    validate_id(&selection.owner)?;
    validate_id(&selection.goal_id)?;
    validate_id(&selection.selector_version)?;
    validate_issues(&selection.issues)?;
    let unique: BTreeSet<_> = selection.candidates.iter().collect();
    if selection.started_at_ms == 0
        || selection.candidates.is_empty()
        || selection.candidates.len() > MAX_CANDIDATES
        || unique.len() != selection.candidates.len()
        || !selection.candidates.iter().all(|candidate| {
            snapshot.skill(&candidate.skill_id).is_some_and(|skill| {
                skill.owner == selection.owner && skill.version(candidate.version).is_some()
            })
        })
    {
        return Err(invalid());
    }
    let open = matches!(
        selection.status,
        SelectionStatus::Running | SelectionStatus::Interrupted
    );
    match selection.finished_at_ms {
        None if open => {}
        Some(at) if !open && at >= selection.started_at_ms => {}
        _ => return Err(invalid()),
    }
    if let Some(choice) = &selection.choice {
        validate_choice_shape(choice)?;
    }
    let consistent = match selection.status {
        SelectionStatus::Chosen => {
            selection.issues.is_empty()
                && selection
                    .choice
                    .as_ref()
                    .is_some_and(|choice| chosen_is_usable(snapshot, &selection.candidates, choice))
        }
        SelectionStatus::Rejected => selection.choice.is_some() && !selection.issues.is_empty(),
        _ => selection.choice.is_none() && selection.issues.is_empty(),
    };
    let settled = match (selection.outcome, selection.settled_at_ms) {
        (None, None) => true,
        (Some(_), Some(at)) => {
            selection.status == SelectionStatus::Chosen
                && selection
                    .finished_at_ms
                    .is_some_and(|finished| at >= finished)
        }
        _ => false,
    };
    if !consistent || !settled {
        return Err(invalid());
    }
    Ok(())
}
