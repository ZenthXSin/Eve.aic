//! 与任务账本分离的策略账本；不创建任务，不请求模型，不重新执行任何工具。
use crate::strict_json;
use eve_llm_api::ToolOutput;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
use eve_task_api::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

/// 输出摘要只关联宿主任务与回执，不保存正文到可跨任务传播的策略提示中。
pub fn evaluate_task_effect(
    task: &Task,
    contract: &TaskEvaluationContract,
) -> TaskResult<TaskStrategyObservation> {
    contract.validate()?;
    let observation = |effect, reason: &str| TaskStrategyObservation {
        effect,
        reason: reason.into(),
        evidence_sha256: effect_digest(task),
        usage: task.usage.unwrap_or_default(),
    };
    let unknown = |reason| observation(TaskEffect::Inconclusive, reason);
    if !task.status.finished() {
        return Err(TaskError::InvalidTransition);
    }
    if task.status != TaskStatus::Completed || task.cancel_requested {
        return Ok(unknown("task_not_completed"));
    }
    let Some(checkpoint) = task.execution.as_ref().and_then(|e| e.checkpoint.as_ref()) else {
        return Ok(unknown("missing_checkpoint"));
    };
    if checkpoint.validate(task.spec.max_steps as usize).is_err()
        || checkpoint.scope.user_id != task.owner.user_id
        || checkpoint.scope.session_id != task.owner.session_id
        || checkpoint.result.is_none()
        || checkpoint.remaining.is_some()
        || !checkpoint.resumable()
    {
        return Ok(unknown("unsettled_checkpoint"));
    }
    let Some(usage) = task.usage else {
        return Ok(unknown("missing_usage"));
    };
    if (usage.tool_calls as usize)
        < checkpoint
            .used_tool_calls
            .saturating_add(checkpoint.delegated_tool_calls)
        || (usage.model_requests as usize)
            < checkpoint
                .provider_requests
                .saturating_add(checkpoint.delegated_model_requests)
    {
        return Ok(unknown("incomplete_usage"));
    }
    let Some(batch) = checkpoint
        .messages
        .iter()
        .rev()
        .find(|m| !m.tool_calls.is_empty())
    else {
        return Ok(unknown("no_verification_receipt"));
    };
    // 同批次未声明的动作可能与检查交错，不能声称检查后的产物仍然有效。
    if batch.tool_calls.iter().any(|call| {
        !contract
            .checks
            .iter()
            .any(|check| call.name == check.tool && call.arguments == check.arguments)
    }) {
        return Ok(unknown("unbound_final_batch"));
    }
    let mut failed = false;
    for check in &contract.checks {
        let calls: Vec<_> = batch
            .tool_calls
            .iter()
            .filter(|call| call.name == check.tool && call.arguments == check.arguments)
            .collect();
        if calls.len() != 1 {
            return Ok(unknown("missing_or_ambiguous_check"));
        }
        let Some(receipt) = checkpoint
            .messages
            .iter()
            .flat_map(|m| &m.tool_results)
            .find(|result| result.call_id == calls[0].id)
        else {
            return Ok(unknown("missing_receipt"));
        };
        match &receipt.output {
            ToolOutput::Success(value) => match value.pointer(&check.output_pointer) {
                Some(actual) => failed |= actual != &check.expected,
                None => return Ok(unknown("missing_observation_field")),
            },
            ToolOutput::Failure { .. } => return Ok(unknown("verification_tool_failed")),
        }
    }
    Ok(observation(
        if failed {
            TaskEffect::Failed
        } else {
            TaskEffect::Verified
        },
        if failed {
            "effect_mismatch"
        } else {
            "contract_satisfied"
        },
    ))
}

/// 从执行前冻结的来源中提炼候选，不复制工具参数、输出或模型自述。
/// 只识别真实的“检查不符 → 成功的后续工具操作 → 最终检查通过”。
/// 这只是相关性证据，候选仍必须通过独立留出对照才能晋升。
pub fn extract_task_strategy(
    task: &Task,
    source: &TaskStrategySource,
) -> TaskResult<Option<TaskStrategyProposal>> {
    source.scope.validate()?;
    source.contract.validate()?;
    if task.id != source.task_id
        || task.owner != source.scope.owner
        || task_strategy_input(task) != source.input_sha256
        || task.spec.max_steps != source.max_steps
    {
        return Err(TaskError::InvalidInput);
    }
    let effect = evaluate_task_effect(task, &source.contract)?;
    if source.observation.as_ref() != Some(&effect) {
        return Err(TaskError::InvalidInput);
    }
    if effect.effect != TaskEffect::Verified {
        return Ok(None);
    }
    let checkpoint = task
        .execution
        .as_ref()
        .and_then(|e| e.checkpoint.as_ref())
        .ok_or(TaskError::InvalidInput)?;
    let messages = &checkpoint.messages;
    let Some(final_index) = messages.iter().rposition(|m| !m.tool_calls.is_empty()) else {
        return Ok(None);
    };
    let mut failed_receipt = None;
    for (index, message) in messages.iter().enumerate().take(final_index) {
        for call in &message.tool_calls {
            for check in source
                .contract
                .checks
                .iter()
                .filter(|check| check.tool == call.name && check.arguments == call.arguments)
            {
                for (receipt_index, receipt_message) in messages
                    .iter()
                    .enumerate()
                    .take(final_index)
                    .skip(index + 1)
                {
                    if receipt_message.tool_results.iter().any(|receipt| {
                        receipt.call_id == call.id
                            && matches!(&receipt.output,
                            ToolOutput::Success(value) if value.pointer(&check.output_pointer)
                                .is_some_and(|actual| actual != &check.expected))
                    }) {
                        failed_receipt = Some(receipt_index);
                    }
                }
            }
        }
    }
    let Some(failed_index) = failed_receipt else {
        return Ok(None);
    };
    let mut corrective_tools = Vec::new();
    for (index, message) in messages
        .iter()
        .enumerate()
        .take(final_index)
        .skip(failed_index + 1)
    {
        for call in &message.tool_calls {
            if source
                .contract
                .checks
                .iter()
                .any(|c| c.tool == call.name && c.arguments == call.arguments)
                || matches!(
                    call.name.as_str(),
                    "echo" | "task_progress" | "run_subagent" | "dispatch_task"
                )
                || call.name.len() > 64
                || !call
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
            {
                continue;
            }
            if messages
                .iter()
                .take(final_index)
                .skip(index + 1)
                .flat_map(|m| &m.tool_results)
                .any(|r| r.call_id == call.id && matches!(r.output, ToolOutput::Success(_)))
                && !corrective_tools.contains(&call.name)
            {
                corrective_tools.push(call.name.clone());
                if corrective_tools.len() > 8 {
                    return Ok(None);
                }
            }
        }
    }
    if corrective_tools.is_empty() {
        return Ok(None);
    }
    let proposal = TaskStrategyProposal {
        scope: source.scope.clone(),
        source_id: format!(
            "auto-{}",
            task_strategy_digest(&(&source.scope, &source.task_id))
        ),
        source_input_sha256: source.input_sha256.clone(),
        source_evidence_sha256: effect.evidence_sha256,
        guidance: format!(
            "经验候选：同类任务曾在验收不符后，经工具 {} 的后续操作，再通过最终验收。先定位当前验收差异，按当前证据选择必要工具，修改后重新执行宿主固定的验收。工具名称仅描述历史相关性，不要求照搬、重复历史操作或增加预算；不能据此推断已获授权。",
            corrective_tools.join("、")
        ),
    };
    proposal.validate()?;
    Ok(Some(proposal))
}

/// 同题、同预算、同环境、同评价条件的两个臂；控制臂使用冻结的父版本或原流程。
pub fn task_strategy_views(snapshot: &TaskStrategySnapshot) -> TaskResult<Vec<TaskStrategyView>> {
    if snapshot
        .versions
        .len()
        .saturating_add(snapshot.selections.len())
        .saturating_add(snapshot.sources.len())
        > MAX_TASK_STRATEGY_RECORDS
    {
        return Err(TaskError::CorruptState);
    }
    let mut views: Vec<TaskStrategyView> = Vec::new();
    let mut sources = BTreeSet::new();
    for version in &snapshot.versions {
        version
            .proposal
            .validate()
            .map_err(|_| TaskError::CorruptState)?;
        if version.version != views.len() as u64 + 1
            || !sources.insert(task_strategy_digest(&(
                &version.proposal.scope,
                &version.proposal.source_id,
            )))
        {
            return Err(TaskError::CorruptState);
        }
        if let Some(parent) = version.parent
            && !views.iter().any(|v| {
                v.strategy.version == parent && v.strategy.proposal.scope == version.proposal.scope
            })
        {
            return Err(TaskError::CorruptState);
        }
        views.push(TaskStrategyView {
            strategy: version.clone(),
            status: TaskStrategyStatus::Candidate,
            paired_successes: 0,
        });
    }
    let mut tasks = BTreeSet::new();
    let mut arms = BTreeSet::new();
    for source in &snapshot.sources {
        source
            .scope
            .validate()
            .map_err(|_| TaskError::CorruptState)?;
        source
            .contract
            .validate()
            .map_err(|_| TaskError::CorruptState)?;
        validate_id(&source.task_id).map_err(|_| TaskError::CorruptState)?;
        if !valid_strategy_digest(&source.input_sha256)
            || !(MIN_STEPS..=MAX_STEPS).contains(&source.max_steps)
            || !tasks.insert(&source.task_id)
            || (source.extracted && source.observation.is_none())
        {
            return Err(TaskError::CorruptState);
        }
        if let Some(observed) = &source.observation {
            validate_observation(observed, source.max_steps)?;
        }
    }
    for selection in &snapshot.selections {
        let candidate = views
            .iter()
            .find(|v| v.strategy.version == selection.assignment.arm.candidate())
            .ok_or(TaskError::CorruptState)?;
        selection
            .assignment
            .scope
            .validate()
            .map_err(|_| TaskError::CorruptState)?;
        selection
            .assignment
            .contract
            .validate()
            .map_err(|_| TaskError::CorruptState)?;
        validate_id(&selection.task_id).map_err(|_| TaskError::CorruptState)?;
        let expected = match selection.assignment.arm {
            TaskStrategyArm::Control { .. } => candidate.strategy.parent,
            TaskStrategyArm::Trial { candidate } | TaskStrategyArm::Apply { candidate } => {
                Some(candidate)
            }
        };
        if selection.assignment.scope != candidate.strategy.proposal.scope
            || selection.selected_version != expected
            || !valid_strategy_digest(&selection.input_sha256)
            || selection.input_sha256 == candidate.strategy.proposal.source_input_sha256
            || !(MIN_STEPS..=MAX_STEPS).contains(&selection.max_steps)
            || !tasks.insert(&selection.task_id)
            || (!matches!(selection.assignment.arm, TaskStrategyArm::Apply { .. })
                && !arms.insert(task_strategy_digest(&(
                    selection.assignment.arm,
                    &selection.input_sha256,
                ))))
        {
            return Err(TaskError::CorruptState);
        }
        if let Some(observed) = &selection.observation {
            validate_observation(observed, selection.max_steps)?;
        }
    }
    for view in &mut views {
        for trial in snapshot.selections.iter().filter(|s| {
            matches!(s.assignment.arm, TaskStrategyArm::Trial { candidate } | TaskStrategyArm::Apply { candidate }
                if candidate == view.strategy.version)
        }) {
            let Some(effect) = &trial.observation else { continue };
            if effect.effect == TaskEffect::Failed {
                view.status = TaskStrategyStatus::Rejected;
                break;
            }
            let control = snapshot.selections.iter().find(|s| {
                s.assignment.arm == (TaskStrategyArm::Control { candidate: view.strategy.version })
                    && s.input_sha256 == trial.input_sha256
                    && s.max_steps == trial.max_steps
                    && s.assignment.contract == trial.assignment.contract
            }).and_then(|s| s.observation.as_ref());
            let Some(control) = control.filter(|c| c.effect == TaskEffect::Verified) else { continue };
            if effect.effect != TaskEffect::Verified { continue }
            if effect.usage.tool_calls > control.usage.tool_calls
                || effect.usage.model_requests > control.usage.model_requests
            {
                view.status = TaskStrategyStatus::Rejected;
                break;
            }
            if matches!(trial.assignment.arm, TaskStrategyArm::Trial { .. }) {
                view.paired_successes += 1;
            }
        }
        if view.status != TaskStrategyStatus::Rejected && view.paired_successes >= 2 {
            view.status = TaskStrategyStatus::Active;
        }
    }
    let mut latest = BTreeMap::new();
    for view in &views {
        if view.status == TaskStrategyStatus::Active {
            latest.insert(
                task_strategy_digest(&view.strategy.proposal.scope),
                view.strategy.version,
            );
        }
    }
    for view in &mut views {
        if view.status == TaskStrategyStatus::Active
            && latest.get(&task_strategy_digest(&view.strategy.proposal.scope))
                != Some(&view.strategy.version)
        {
            view.status = TaskStrategyStatus::Retired;
        }
    }
    Ok(views)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    format_version: u32,
    snapshot: TaskStrategySnapshot,
}
struct Inner {
    ledger: Ledger,
    context: Option<PluginContext>,
}
struct Stored {
    inner: Mutex<Inner>,
    policy: Arc<dyn TaskStrategyPolicy>,
}
impl Stored {
    fn open(context: PluginContext, policy: Arc<dyn TaskStrategyPolicy>) -> TaskResult<Self> {
        let ledger = match context
            .state_get(TASK_STRATEGY_STATE_KEY)
            .map_err(|_| TaskError::Storage)?
        {
            None => Ledger {
                format_version: 1,
                snapshot: TaskStrategySnapshot::default(),
            },
            Some(bytes) => {
                if bytes.len() > MAX_TASK_STRATEGY_BYTES {
                    return Err(TaskError::CorruptState);
                }
                let value = strict_json::from_slice(&bytes).map_err(|_| TaskError::CorruptState)?;
                if value.get("format_version").and_then(|v| v.as_u64()) != Some(1) {
                    return Err(TaskError::UnsupportedVersion);
                }
                serde_json::from_value(value).map_err(|_| TaskError::CorruptState)?
            }
        };
        task_strategy_views(&ledger.snapshot)?;
        Ok(Self {
            inner: Mutex::new(Inner {
                ledger,
                context: Some(context),
            }),
            policy,
        })
    }
    fn mutate<T>(
        &self,
        f: impl FnOnce(&mut TaskStrategySnapshot) -> TaskResult<T>,
    ) -> TaskResult<T> {
        let mut inner = self.inner.lock().map_err(|_| TaskError::Unavailable)?;
        if inner.context.is_none() {
            return Err(TaskError::Unavailable);
        }
        let mut next = inner.ledger.clone();
        let result = f(&mut next.snapshot)?;
        task_strategy_views(&next.snapshot)?;
        if next.snapshot == inner.ledger.snapshot {
            return Ok(result);
        }
        let bytes = serde_json::to_vec(&next).map_err(|_| TaskError::InvalidInput)?;
        // 为每个尚未结算的选择留出终结记录空间，满后停止新选择，已有结果仍能保存。
        let reserved = next
            .snapshot
            .selections
            .iter()
            .filter(|s| s.observation.is_none())
            .count()
            .saturating_add(
                next.snapshot
                    .sources
                    .iter()
                    .filter(|s| s.observation.is_none())
                    .count(),
            )
            * 512;
        if bytes.len().saturating_add(reserved) > MAX_TASK_STRATEGY_BYTES {
            return Err(TaskError::LimitReached);
        }
        if inner
            .context
            .as_ref()
            .ok_or(TaskError::Unavailable)?
            .state_set(TASK_STRATEGY_STATE_KEY, bytes)
            .is_err()
        {
            inner.context = None;
            return Err(TaskError::Storage);
        }
        inner.ledger = next;
        Ok(result)
    }
    fn guidance(
        snapshot: &TaskStrategySnapshot,
        selected: &TaskStrategySelection,
    ) -> TaskResult<Option<TaskStrategyGuidance>> {
        let instructions = selected
            .selected_version
            .map(|version| {
                let row = snapshot
                    .versions
                    .iter()
                    .find(|v| v.version == version)
                    .ok_or(TaskError::CorruptState)?;
                Ok::<_, TaskError>(row.proposal.guidance.clone())
            })
            .transpose()?;
        Ok(Some(TaskStrategyGuidance {
            version: selected.selected_version,
            instructions,
            evaluation: selected.assignment.contract.clone(),
        }))
    }
}
impl TaskStrategyLearning for Stored {
    fn snapshot(&self) -> TaskResult<TaskStrategySnapshot> {
        let inner = self.inner.lock().map_err(|_| TaskError::Unavailable)?;
        if inner.context.is_none() {
            return Err(TaskError::Unavailable);
        }
        Ok(inner.ledger.snapshot.clone())
    }
    fn propose(&self, proposal: TaskStrategyProposal) -> TaskResult<TaskStrategyVersion> {
        proposal.validate()?;
        self.mutate(|snapshot| {
            if let Some(existing) = snapshot.versions.iter().find(|v| {
                v.proposal.scope == proposal.scope && v.proposal.source_id == proposal.source_id
            }) {
                if existing.proposal != proposal {
                    return Err(TaskError::InvalidInput);
                }
                return Ok(existing.clone());
            }
            if snapshot.versions.len() + snapshot.selections.len() + snapshot.sources.len()
                >= MAX_TASK_STRATEGY_RECORDS
            {
                return Err(TaskError::LimitReached);
            }
            let views = task_strategy_views(snapshot)?;
            if views.iter().any(|v| {
                v.strategy.proposal.scope == proposal.scope
                    && v.status == TaskStrategyStatus::Candidate
            }) {
                return Err(TaskError::InvalidTransition);
            }
            let parent = views
                .iter()
                .find(|v| {
                    v.strategy.proposal.scope == proposal.scope
                        && v.status == TaskStrategyStatus::Active
                })
                .map(|v| v.strategy.version);
            let version = TaskStrategyVersion {
                version: snapshot.versions.len() as u64 + 1,
                parent,
                proposal,
            };
            snapshot.versions.push(version.clone());
            Ok(version)
        })
    }
    fn prepare(&self, task: &Task) -> TaskResult<Option<TaskStrategyGuidance>> {
        if task.status != TaskStatus::Running || task.cancel_requested {
            return Err(TaskError::InvalidTransition);
        }
        let scope = self.policy.scope(task)?;
        let before = self.snapshot()?;
        if let Some(old) = before.sources.iter().find(|s| s.task_id == task.id) {
            if old.observation.is_some()
                || old.input_sha256 != task_strategy_input(task)
                || Some(&old.scope) != scope.as_ref()
            {
                return Err(TaskError::InvalidTransition);
            }
            return Ok(Some(TaskStrategyGuidance {
                version: None,
                instructions: None,
                evaluation: old.contract.clone(),
            }));
        }
        if let Some(old) = before.selections.iter().find(|s| s.task_id == task.id) {
            if old.observation.is_some()
                || old.input_sha256 != task_strategy_input(task)
                || Some(&old.assignment.scope) != scope.as_ref()
            {
                return Err(TaskError::InvalidTransition);
            }
            return Self::guidance(&before, old);
        }
        // 旧执行不能在安全恢复时中途加入新的试验。
        if task
            .execution
            .as_ref()
            .is_some_and(|e| e.checkpoint.is_some() || e.recoveries != 0)
        {
            return Ok(None);
        }
        if let Some(contract) = self.policy.source_contract(task)? {
            contract.validate()?;
            let scope = scope.ok_or(TaskError::InvalidInput)?;
            scope.validate()?;
            if scope.owner != task.owner {
                return Err(TaskError::InvalidInput);
            }
            return self
                .mutate(|snapshot| {
                    if *snapshot != before {
                        return Err(TaskError::InvalidTransition);
                    }
                    if snapshot.versions.len() + snapshot.selections.len() + snapshot.sources.len()
                        >= MAX_TASK_STRATEGY_RECORDS
                    {
                        return Err(TaskError::LimitReached);
                    }
                    snapshot.sources.push(TaskStrategySource {
                        task_id: task.id.clone(),
                        input_sha256: task_strategy_input(task),
                        scope,
                        contract: contract.clone(),
                        max_steps: task.spec.max_steps,
                        observation: None,
                        extracted: false,
                    });
                    Ok(Some(TaskStrategyGuidance {
                        version: None,
                        instructions: None,
                        evaluation: contract,
                    }))
                })
                .or_else(|e| {
                    if e == TaskError::LimitReached {
                        Ok(None)
                    } else {
                        Err(e)
                    }
                });
        }
        let Some(assignment) = self.policy.assign(task, &before)? else {
            return Ok(None);
        };
        if Some(&assignment.scope) != scope.as_ref() || assignment.scope.owner != task.owner {
            return Err(TaskError::InvalidInput);
        }
        assignment.scope.validate()?;
        assignment.contract.validate()?;
        self.mutate(|snapshot| {
            // 策略与选择在同一把锁内核对，避免并发选择同一对照臂。
            if *snapshot != before {
                return Err(TaskError::InvalidTransition);
            }
            if snapshot.versions.len() + snapshot.selections.len() + snapshot.sources.len()
                >= MAX_TASK_STRATEGY_RECORDS
            {
                return Err(TaskError::LimitReached);
            }
            let views = task_strategy_views(snapshot)?;
            let candidate = views
                .iter()
                .find(|v| v.strategy.version == assignment.arm.candidate())
                .ok_or(TaskError::NotFound)?;
            if candidate.strategy.proposal.scope != assignment.scope
                || !matches!(
                    candidate.status,
                    TaskStrategyStatus::Candidate | TaskStrategyStatus::Active
                )
                || (matches!(assignment.arm, TaskStrategyArm::Apply { .. })
                    && candidate.status != TaskStrategyStatus::Active)
                || candidate.strategy.proposal.source_input_sha256 == task_strategy_input(task)
                || candidate.strategy.parent.is_some_and(|parent| {
                    views.iter().any(|v| {
                        v.strategy.version == parent && v.status == TaskStrategyStatus::Rejected
                    })
                })
            {
                return Err(TaskError::InvalidTransition);
            }
            let selected_version = match assignment.arm {
                TaskStrategyArm::Control { .. } => candidate.strategy.parent,
                TaskStrategyArm::Trial { candidate } | TaskStrategyArm::Apply { candidate } => {
                    Some(candidate)
                }
            };
            let input = task_strategy_input(task);
            if !matches!(assignment.arm, TaskStrategyArm::Apply { .. })
                && snapshot
                    .selections
                    .iter()
                    .any(|s| s.assignment.arm == assignment.arm && s.input_sha256 == input)
            {
                return Err(TaskError::InvalidTransition);
            }
            if matches!(assignment.arm, TaskStrategyArm::Apply { .. })
                && !snapshot.selections.iter().any(|s| {
                    s.assignment.arm
                        == (TaskStrategyArm::Control {
                            candidate: assignment.arm.candidate(),
                        })
                        && s.input_sha256 == input
                        && s.assignment.contract == assignment.contract
                        && s.observation
                            .as_ref()
                            .is_some_and(|o| o.effect == TaskEffect::Verified)
                })
            {
                return Err(TaskError::InvalidTransition);
            }
            snapshot.selections.push(TaskStrategySelection {
                task_id: task.id.clone(),
                input_sha256: input,
                assignment,
                selected_version,
                max_steps: task.spec.max_steps,
                observation: None,
            });
            Self::guidance(
                snapshot,
                snapshot.selections.last().ok_or(TaskError::CorruptState)?,
            )
        })
        .or_else(|error| {
            if error == TaskError::LimitReached {
                Ok(None)
            } else {
                Err(error)
            }
        })
    }
    fn reconcile(&self, tasks: &TaskSnapshot) -> TaskResult<()> {
        self.reconcile_evidence(tasks)
    }
    fn reconcile_from(&self, tasks: &dyn TaskAdmin) -> TaskResult<()> {
        let keys = self.snapshot()?.evidence_keys()?;
        let evidence = tasks.evidence_snapshot(&keys)?;
        self.reconcile_evidence(&evidence)
    }
}

impl Stored {
    fn reconcile_evidence(&self, tasks: &dyn TaskEvidenceReader) -> TaskResult<()> {
        let snapshot = self.snapshot()?;
        let mut completed = Vec::new();
        for selected in &snapshot.selections {
            match tasks.find(&selected.task_id)? {
                Some(task) if selected.observation.is_none() && task.status.finished() => {
                    if task.id != selected.task_id
                        || task.owner != selected.assignment.scope.owner
                        || task_strategy_input(task) != selected.input_sha256
                    {
                        return Err(TaskError::CorruptState);
                    }
                    let mut observed = evaluate_task_effect(task, &selected.assignment.contract)?;
                    if self.policy.scope(task)?.as_ref() != Some(&selected.assignment.scope) {
                        observed.effect = TaskEffect::Inconclusive;
                        observed.reason = "runtime_scope_changed".into();
                    }
                    completed.push((selected.task_id.clone(), observed));
                }
                None if selected.observation.is_none() => {
                    // 任务已被原账本淘汰，证据不足，不能自动重建或重跑。
                    completed.push((
                        selected.task_id.clone(),
                        TaskStrategyObservation {
                            effect: TaskEffect::Inconclusive,
                            reason: "source_evicted".into(),
                            evidence_sha256: task_strategy_digest(&selected.task_id),
                            usage: TaskUsage::default(),
                        },
                    ));
                }
                Some(task) if selected.observation.is_some() => {
                    let old = selected
                        .observation
                        .as_ref()
                        .ok_or(TaskError::CorruptState)?;
                    // 通知回执后续会变化，不用整条 Task 摘要校验已完成任务。
                    if task.id != selected.task_id
                        || task.owner != selected.assignment.scope.owner
                        || task_strategy_input(task) != selected.input_sha256
                        || (old.reason != "source_evicted"
                            && old.evidence_sha256 != effect_digest(task))
                    {
                        return Err(TaskError::CorruptState);
                    }
                    if matches!(
                        old.reason.as_str(),
                        "runtime_scope_changed" | "source_evicted"
                    ) {
                        if old.effect != TaskEffect::Inconclusive {
                            return Err(TaskError::CorruptState);
                        }
                    } else if evaluate_task_effect(task, &selected.assignment.contract)? != *old {
                        return Err(TaskError::CorruptState);
                    }
                }
                _ => {}
            }
        }
        let mut source_completed = Vec::new();
        for source in &snapshot.sources {
            match tasks.find(&source.task_id)? {
                Some(task) => {
                    if task.id != source.task_id
                        || task.owner != source.scope.owner
                        || task_strategy_input(task) != source.input_sha256
                    {
                        return Err(TaskError::CorruptState);
                    }
                    if let Some(old) = &source.observation {
                        if old.reason != "source_evicted"
                            && old.evidence_sha256 != effect_digest(task)
                        {
                            return Err(TaskError::CorruptState);
                        }
                        if !matches!(
                            old.reason.as_str(),
                            "runtime_scope_changed" | "source_evicted"
                        ) && evaluate_task_effect(task, &source.contract)? != *old
                        {
                            return Err(TaskError::CorruptState);
                        }
                    } else if task.status.finished() {
                        let mut observed = evaluate_task_effect(task, &source.contract)?;
                        if self.policy.scope(task)?.as_ref() != Some(&source.scope) {
                            observed.effect = TaskEffect::Inconclusive;
                            observed.reason = "runtime_scope_changed".into();
                        }
                        source_completed.push((source.task_id.clone(), observed));
                    }
                }
                None if source.observation.is_none() => source_completed.push((
                    source.task_id.clone(),
                    TaskStrategyObservation {
                        effect: TaskEffect::Inconclusive,
                        reason: "source_evicted".into(),
                        evidence_sha256: task_strategy_digest(&source.task_id),
                        usage: TaskUsage::default(),
                    },
                )),
                None => {}
            }
        }
        self.mutate(|snapshot| {
            for (id, effect) in source_completed {
                let row = snapshot
                    .sources
                    .iter_mut()
                    .find(|s| s.task_id == id)
                    .ok_or(TaskError::CorruptState)?;
                if row.observation.is_none() {
                    row.observation = Some(effect);
                }
            }
            for (id, effect) in completed {
                let row = snapshot
                    .selections
                    .iter_mut()
                    .find(|s| s.task_id == id)
                    .ok_or(TaskError::CorruptState)?;
                if row.observation.is_none() {
                    row.observation = Some(effect)
                }
            }
            Ok(())
        })?;
        // 单独事务生成候选；崩溃后以不可变 source_id 去重，不重复生成版本。
        for source in self
            .snapshot()?
            .sources
            .iter()
            .filter(|s| !s.extracted && s.observation.is_some())
        {
            let proposal = match tasks.find(&source.task_id)? {
                Some(task)
                    if source
                        .observation
                        .as_ref()
                        .is_some_and(|o| o.effect == TaskEffect::Verified)
                        && self.policy.scope(task)?.as_ref() == Some(&source.scope) =>
                {
                    extract_task_strategy(task, source)?
                }
                _ => None,
            };
            if let Some(proposal) = proposal {
                match self.propose(proposal) {
                    Ok(_) => {}
                    Err(TaskError::InvalidTransition | TaskError::LimitReached) => continue,
                    Err(error) => return Err(error),
                }
            }
            self.mutate(|snapshot| {
                snapshot
                    .sources
                    .iter_mut()
                    .find(|s| s.task_id == source.task_id)
                    .ok_or(TaskError::CorruptState)?
                    .extracted = true;
                Ok(())
            })?;
        }
        Ok(())
    }
}

fn validate_observation(observed: &TaskStrategyObservation, max_steps: u32) -> TaskResult<()> {
    if !valid_strategy_digest(&observed.evidence_sha256)
        || observed.usage.tool_calls > max_steps
        || observed.reason.is_empty()
        || observed.reason.len() > 128
        || (observed.effect == TaskEffect::Verified && observed.reason != "contract_satisfied")
        || (observed.effect == TaskEffect::Failed && observed.reason != "effect_mismatch")
        || (matches!(
            observed.reason.as_str(),
            "runtime_scope_changed" | "source_evicted"
        ) && observed.effect != TaskEffect::Inconclusive)
    {
        return Err(TaskError::CorruptState);
    }
    Ok(())
}

fn effect_digest(task: &Task) -> String {
    task_strategy_digest(&(
        &task.id,
        &task.owner,
        &task.spec,
        task.status,
        &task.result,
        &task.failure,
        &task.execution,
        task.usage,
        task.finished_at_ms,
    ))
}

#[derive(Clone, Default)]
pub struct TaskStrategyController {
    active: Arc<Mutex<Option<Arc<Stored>>>>,
}
impl TaskStrategyController {
    fn service(&self) -> TaskResult<Arc<Stored>> {
        self.active
            .lock()
            .map_err(|_| TaskError::Unavailable)?
            .clone()
            .ok_or(TaskError::Unavailable)
    }
}
impl TaskStrategyLearning for TaskStrategyController {
    fn snapshot(&self) -> TaskResult<TaskStrategySnapshot> {
        self.service()?.snapshot()
    }
    fn propose(&self, proposal: TaskStrategyProposal) -> TaskResult<TaskStrategyVersion> {
        self.service()?.propose(proposal)
    }
    fn prepare(&self, task: &Task) -> TaskResult<Option<TaskStrategyGuidance>> {
        self.service()?.prepare(task)
    }
    fn reconcile(&self, tasks: &TaskSnapshot) -> TaskResult<()> {
        self.service()?.reconcile(tasks)
    }
    fn reconcile_from(&self, tasks: &dyn TaskAdmin) -> TaskResult<()> {
        self.service()?.reconcile_from(tasks)
    }
}
pub struct TaskStrategyPlugin {
    manifest: PluginManifest,
    controller: TaskStrategyController,
    policy: Arc<dyn TaskStrategyPolicy>,
}
impl TaskStrategyPlugin {
    pub fn new(policy: Arc<dyn TaskStrategyPolicy>) -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(TASK_STRATEGY_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: TaskStrategyController::default(),
            policy,
        })
    }
    pub fn controller(&self) -> TaskStrategyController {
        self.controller.clone()
    }
}
impl Plugin for TaskStrategyPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                Stored::open(context.clone(), self.policy.clone())
                    .map_err(|e| PluginError::State(e.to_string()))?,
            );
            let closing = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                closing
                    .inner
                    .lock()
                    .map_err(|_| PluginError::State("任务策略不可用".into()))?
                    .context = None;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("任务策略不可用".into()))?;
                if active.as_ref().is_some_and(|v| Arc::ptr_eq(v, &closing)) {
                    *active = None
                }
                Ok(())
            }))?;
            for proposal in self
                .policy
                .proposals()
                .map_err(|e| PluginError::State(e.to_string()))?
            {
                match stored.propose(proposal) {
                    Ok(_) => {}
                    Err(TaskError::LimitReached) => break,
                    Err(error) => return Err(PluginError::State(error.to_string())),
                }
            }
            *self
                .controller
                .active
                .lock()
                .map_err(|_| PluginError::State("任务策略不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
