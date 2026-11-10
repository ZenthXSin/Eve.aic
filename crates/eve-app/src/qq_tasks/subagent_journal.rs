//! 子代理持久执行证据与可替换存储契约。中断只保全，不重跑旧调用。
use super::*;
use eve_llm_api::{ModelSession, TurnCheckpoint, TurnJournal};
use eve_task_api::{
    TaskCompletionAssessment, TaskCompletionEvidenceSource, TaskFrozenNodeEvidence,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::Mutex};

#[cfg(test)]
mod evidence_tests;
#[cfg(test)]
mod frozen_node_tests;
mod sqlite;
pub(crate) use sqlite::SqliteRunStorage;
const MAX_RECORD_BYTES: usize = 34 * 1024 * 1024;
pub(crate) const LIST: &str = "list_subagent_runs";

fn storage() -> LlmError {
    LlmError::Context("子代理执行记录未确认保存；停止执行，原记录保留".into())
}
fn valid_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FrozenRun {
    pub owner: TaskOwner,
    pub model_session: ModelSession,
    pub parent_call_id: String,
    pub profile: eve_task_api::SubagentProfile,
    pub instructions: String,
    pub context: Option<DelegatedContext>,
    pub review_messages: Vec<String>,
    pub verification: TaskCompletionContract,
    pub steps: u32,
    pub deadline_at_ms: Option<u64>,
}
fn run_id(frozen: &FrozenRun) -> Result<String, LlmError> {
    let identity =
        serde_json::to_vec(&(&frozen.owner, &frozen.model_session, &frozen.parent_call_id))
            .map_err(|_| storage())?;
    Ok(ring::digest::digest(&SHA256, &identity)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Entry {
    id: String,
    owner: TaskOwner,
    instructions: String,
    created_at_ms: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum End {
    Completed { result: String },
    Blocked { result: String },
    Failed { reason: String },
    Cancelled,
}
impl End {
    fn capture(end: &TaskEnd) -> Result<Self, LlmError> {
        Ok(match end {
            TaskEnd::Completed { result } => Self::Completed {
                result: result.clone(),
            },
            TaskEnd::Blocked { result } => Self::Blocked {
                result: result.clone(),
            },
            TaskEnd::Failed { reason } => Self::Failed {
                reason: reason.clone(),
            },
            TaskEnd::Cancelled => Self::Cancelled,
            TaskEnd::SessionEnded { .. } => return Err(storage()),
        })
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    frozen: FrozenRun,
    checkpoint: Option<TurnCheckpoint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    frozen_node_evidence: Vec<TaskFrozenNodeEvidence>,
    end: Option<End>,
    usage: Option<TaskUsage>,
    verification: Option<eve_task_api::TaskCompletionAssessment>,
    finished_at_ms: Option<u64>,
}
impl Record {
    fn validate(&self) -> Result<(), LlmError> {
        let f = &self.frozen;
        if self.version != 1
            || f.steps == 0
            || f.steps > MAX_STEPS
            || f.instructions.trim().is_empty()
            || f.instructions.len() > 64 * 1024
            || f.parent_call_id.is_empty()
            || self.end.is_some() != self.finished_at_ms.is_some()
            || self.end.is_some() != self.usage.is_some()
        {
            return Err(storage());
        }
        f.model_session.validate_scope(Some(&ContextScope {
            session_id: f.owner.session_id.clone(),
            user_id: f.owner.user_id.clone(),
        }))?;
        f.profile.profile.validate().map_err(|_| storage())?;
        eve_task_api::validate_review_messages(&f.review_messages).map_err(|_| storage())?;
        f.verification.validate().map_err(|_| storage())?;
        if let Some(context) = &f.context {
            if context.owner != f.owner {
                return Err(storage());
            }
            eve_task_api::validate_delegated_context(context).map_err(|_| storage())?;
        }
        if let Some(frame) = &self.checkpoint {
            frame.validate(f.steps as usize)?;
            if frame.scope.session_id != f.owner.session_id
                || frame.scope.user_id != f.owner.user_id
                || frame.model_session.as_ref() != Some(&f.model_session)
                || frame.delegated_tool_calls != 0
                || frame.delegated_model_requests != 0
            {
                return Err(storage());
            }
            eve_task_api::validate_frozen_node_evidence(
                &f.owner,
                frame,
                &self.frozen_node_evidence,
            )
            .map_err(|_| storage())?;
        } else if !self.frozen_node_evidence.is_empty() {
            return Err(storage());
        }
        if !self.frozen_node_evidence.is_empty()
            && !matches!(
                f.verification,
                TaskCompletionContract::FrozenNodeTest { .. }
            )
        {
            return Err(storage());
        }
        if let (TaskCompletionContract::FrozenNodeTest { .. }, Some(End::Completed { result })) =
            (&f.verification, &self.end)
        {
            let frame = self.checkpoint.as_ref().ok_or_else(storage)?;
            if self.verification != Some(TaskCompletionAssessment::Verified)
                || !eve_task_plugin::evaluate_checkpoint_completion(
                    &f.verification,
                    &f.owner,
                    frame,
                    f.steps as usize,
                    result,
                    &self.frozen_node_evidence,
                )
                .verified()
                || self.usage != Some(usage_of(frame.provider_requests, frame.used_tool_calls))
            {
                return Err(storage());
            }
        }
        Ok(())
    }
}

/// 定义层仅承诺原子创建、更新与按身份分页读取；实现可替换，不假设主状态后端是文件。
pub(crate) type StoredRun = (Vec<u8>, Vec<u8>);
pub(crate) trait RunStorage: Send + Sync {
    fn create(
        &self,
        id: &str,
        owner: &TaskOwner,
        entry: &[u8],
        frozen: &[u8],
        state: &[u8],
    ) -> Result<bool, LlmError>;
    fn read(&self, id: &str) -> Result<Option<StoredRun>, LlmError>;
    fn update(&self, id: &str, state: &[u8]) -> Result<(), LlmError>;
    fn list(
        &self,
        owner: &TaskOwner,
        id: Option<&str>,
        offset: usize,
    ) -> Result<Vec<Vec<u8>>, LlmError>;
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedState {
    version: u32,
    checkpoint: Option<TurnCheckpoint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    frozen_node_evidence: Vec<TaskFrozenNodeEvidence>,
    end: Option<End>,
    usage: Option<TaskUsage>,
    verification: Option<eve_task_api::TaskCompletionAssessment>,
    finished_at_ms: Option<u64>,
}
fn state_bytes(record: &Record) -> Result<Vec<u8>, LlmError> {
    serde_json::to_vec(&SavedState {
        version: record.version,
        checkpoint: record.checkpoint.clone(),
        frozen_node_evidence: record.frozen_node_evidence.clone(),
        end: record.end.clone(),
        usage: record.usage,
        verification: record.verification.clone(),
        finished_at_ms: record.finished_at_ms,
    })
    .map_err(|_| storage())
}
pub(super) struct RunStore {
    backend: Arc<dyn RunStorage>,
    completion_evidence: Option<Arc<dyn TaskCompletionEvidenceSource>>,
    lock: Mutex<HashSet<String>>,
}
impl RunStore {
    pub(super) fn open(backend: Arc<dyn RunStorage>) -> Arc<Self> {
        Self::open_with_completion_evidence(backend, None)
    }
    pub(super) fn open_with_completion_evidence(
        backend: Arc<dyn RunStorage>,
        completion_evidence: Option<Arc<dyn TaskCompletionEvidenceSource>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            backend,
            completion_evidence,
            lock: Mutex::new(HashSet::new()),
        })
    }
    fn read(&self, id: &str) -> Result<Option<Record>, LlmError> {
        if !valid_id(id) {
            return Err(storage());
        }
        self.backend
            .read(id)?
            .map(|(frozen, state)| {
                if frozen.len().saturating_add(state.len()) > MAX_RECORD_BYTES {
                    return Err(storage());
                }
                let frozen: FrozenRun = serde_json::from_slice(&frozen).map_err(|_| storage())?;
                let state: SavedState = serde_json::from_slice(&state).map_err(|_| storage())?;
                let record = Record {
                    version: state.version,
                    frozen,
                    checkpoint: state.checkpoint,
                    frozen_node_evidence: state.frozen_node_evidence,
                    end: state.end,
                    usage: state.usage,
                    verification: state.verification,
                    finished_at_ms: state.finished_at_ms,
                };
                record.validate()?;
                if run_id(&record.frozen)? != id {
                    return Err(storage());
                }
                Ok(record)
            })
            .transpose()
    }
    fn write(&self, id: &str, record: &Record) -> Result<(), LlmError> {
        record.validate()?;
        let bytes = state_bytes(record)?;
        if bytes.len() > eve_llm_api::MAX_CHECKPOINT_BYTES + 128 * 1024 {
            return Err(storage());
        }
        self.backend.update(id, &bytes)
    }
    pub(super) fn begin(self: &Arc<Self>, frozen: FrozenRun) -> Result<Arc<RunJournal>, LlmError> {
        let id = run_id(&frozen)?;
        let mut active = self.lock.lock().map_err(|_| storage())?;
        if self.read(&id)?.is_some() {
            return Err(LlmError::Context(format!(
                "子代理原调用已持久化，禁止重新执行；用 list_subagent_runs 读取 run_id={id}"
            )));
        }
        if matches!(
            frozen.verification,
            TaskCompletionContract::FrozenNodeTest { .. }
        ) && self.completion_evidence.is_none()
        {
            return Err(LlmError::Context(
                "冻结测试证据来源未就绪，未启动子代理".into(),
            ));
        }
        let record = Record {
            version: 1,
            frozen,
            checkpoint: None,
            frozen_node_evidence: vec![],
            end: None,
            usage: None,
            verification: None,
            finished_at_ms: None,
        };
        record.validate()?;
        let entry = Entry {
            id: id.clone(),
            owner: record.frozen.owner.clone(),
            instructions: bounded(&record.frozen.instructions, 800),
            created_at_ms: now_ms(),
        };
        let frozen = serde_json::to_vec(&record.frozen).map_err(|_| storage())?;
        let state = state_bytes(&record)?;
        if frozen.len().saturating_add(state.len()) > MAX_RECORD_BYTES {
            return Err(storage());
        }
        if !self.backend.create(
            &id,
            &entry.owner,
            &serde_json::to_vec(&entry).map_err(|_| storage())?,
            &frozen,
            &state,
        )? {
            return Err(LlmError::Context(format!(
                "子代理原调用已持久化，禁止重新执行；run_id={id}"
            )));
        }
        active.insert(id.clone());
        Ok(Arc::new(RunJournal {
            store: self.clone(),
            id,
        }))
    }
    /// 直接读取原持久回执；不结算任务、不重放调用，也不把历史匹配当作当前产物证明。
    pub(super) fn evidence_view(&self, owner: &TaskOwner, id: &str) -> Result<Value, LlmError> {
        let active = self.lock.lock().map_err(|_| storage())?;
        let entries = self.backend.list(owner, Some(id), 0)?;
        if entries.len() > 1 {
            return Err(storage());
        }
        let Some(bytes) = entries.first() else {
            return Ok(json!({"runs":[],"automatic_resume":false}));
        };
        let entry: Entry = serde_json::from_slice(bytes).map_err(|_| storage())?;
        if entry.owner != *owner || entry.id != id {
            return Err(storage());
        }
        let record = self.read(id)?.ok_or_else(storage)?;
        if record.frozen.owner != *owner {
            return Err(storage());
        }
        let frame = record.checkpoint.as_ref();
        let run_status = match &record.end {
            Some(End::Completed { .. }) => "completed",
            Some(End::Blocked { .. }) => "blocked",
            Some(End::Failed { .. }) => "failed",
            Some(End::Cancelled) => "cancelled",
            None if active.contains(id) => "running",
            None if frame.is_some_and(|frame| !frame.resumable()) => "interrupted_unknown",
            None if frame.is_some_and(|frame| frame.result.is_some()) => "result_saved_unsettled",
            None => "interrupted_confirmed",
        };
        // 原验收原因可能包含 remaining 或模型文字；证据简表只显示固定状态。
        let verification = record.verification.as_ref().map(|value| match value {
            eve_task_api::TaskCompletionAssessment::Verified
                if matches!(&record.end, Some(End::Completed { result })
                    if frame.is_some_and(|frame| frame.resumable()
                        && frame.result.as_ref() == Some(result)
                        && frame.remaining.is_none())) =>
            {
                "verified"
            }
            eve_task_api::TaskCompletionAssessment::Verified => "inconsistent",
            eve_task_api::TaskCompletionAssessment::Mismatch { .. } => "mismatch",
            eve_task_api::TaskCompletionAssessment::Unverifiable { .. } => "unverifiable",
            eve_task_api::TaskCompletionAssessment::Blocked { .. } => "blocked",
        });
        let (report, reason) = match (&record.frozen.verification, frame) {
            (TaskCompletionContract::Receipts { contract }, Some(frame)) => (
                Some(eve_task_plugin::select_task_receipt_evidence(
                    contract,
                    &frame.messages,
                    &frame.pending_calls,
                )),
                None,
            ),
            (TaskCompletionContract::Receipts { .. }, None) => (None, Some("missing_checkpoint")),
            _ => (None, Some("no_frozen_receipt_contract")),
        };
        let mut row = json!({
            "run_id":id,
            "run_status":run_status,
            "evidence_kind":"historical_tool_receipts",
            "current_artifact_validity":"not_checked",
            "automatic_resume":false,
            "effects_unknown":frame.is_some_and(|frame| !frame.resumable()),
            "verification":verification,
            "report":report,
        });
        if let Some(reason) = reason {
            row["reason"] = json!(reason);
        }
        Ok(json!({"runs":[row],"automatic_resume":false}))
    }
    fn views(&self, owner: &TaskOwner, id: Option<&str>, offset: usize) -> Result<Value, LlmError> {
        let active = self.lock.lock().map_err(|_| storage())?;
        let entries = self
            .backend
            .list(owner, id, if id.is_none() { offset } else { 0 })?;
        let entries = entries
            .iter()
            .map(|bytes| serde_json::from_slice::<Entry>(bytes).map_err(|_| storage()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut rows = vec![];
        for entry in &entries {
            if entry.owner != *owner || id.is_some_and(|id| id != entry.id) {
                return Err(storage());
            }
            let record = self.read(&entry.id)?;
            if record
                .as_ref()
                .is_some_and(|r| r.frozen.owner != entry.owner)
            {
                return Err(storage());
            }
            let frame = record.as_ref().and_then(|r| r.checkpoint.as_ref());
            let status = match record.as_ref() {
                None => "not_started",
                Some(r) if r.end.is_some() => match (&r.end, &r.verification) {
                    (_, Some(eve_task_api::TaskCompletionAssessment::Unverifiable { .. })) => {
                        "returned_pending_verification"
                    }
                    (_, Some(eve_task_api::TaskCompletionAssessment::Mismatch { .. })) => {
                        "verification_mismatch"
                    }
                    (Some(End::Completed { .. }), _) => "completed",
                    (Some(End::Blocked { .. }), _) => "blocked",
                    (Some(End::Cancelled), _) => "cancelled",
                    _ => "failed",
                },
                Some(_) if active.contains(&entry.id) => "running",
                Some(_) if frame.is_some_and(|f| !f.resumable()) => "interrupted_unknown",
                Some(_) if frame.is_some_and(|f| f.result.is_some()) => "result_saved_unsettled",
                Some(_) => "interrupted_confirmed",
            };
            let mut row = json!({"run_id":entry.id,"status":status,"task":crate::qq_task_diagnostics::safe_detail(&entry.instructions, 800),"created_at_ms":entry.created_at_ms,"automatic_resume":false,"tool_calls":frame.map(|f|f.used_tool_calls),"model_requests":frame.map(|f|f.provider_requests),"pending_calls":frame.map(|f|f.pending_calls.len()),"verification":record.as_ref().and_then(|r|r.verification.as_ref())});
            row["effects_unknown"] = json!(frame.is_some_and(|f| !f.resumable()));
            if id.is_some() {
                row["original_task"] = json!(crate::qq_task_diagnostics::safe_detail(
                    record
                        .as_ref()
                        .map(|r| r.frozen.instructions.as_str())
                        .unwrap_or(&entry.instructions),
                    65536
                ));
                row["result"] = json!(record.as_ref().and_then(|r| r.end.as_ref()).map(|end| {
                    crate::qq_task_diagnostics::safe_detail(
                        &serde_json::to_string(end).unwrap_or_default(),
                        12000,
                    )
                }));
                row["saved_result"] = json!(
                    frame
                        .and_then(|f| f.result.as_deref())
                        .map(|result| crate::qq_task_diagnostics::safe_detail(result, 12000))
                );
                row["scope"] = json!(owner);
                row["model_session"] = json!(record.as_ref().map(|r| &r.frozen.model_session));
                row["frozen_context_bytes"] = json!(
                    record
                        .as_ref()
                        .and_then(|r| r.frozen.context.as_ref())
                        .map(|c| c.messages_json.len())
                );
                if let Some(frame) = frame {
                    let calls = frame
                        .messages
                        .iter()
                        .flat_map(|m| m.tool_calls.iter())
                        .chain(&frame.pending_calls)
                        .collect::<Vec<_>>();
                    row["steps_total"] = json!(calls.len());
                    row["steps_offset"] = json!(offset);
                    row["steps"] = json!(calls.into_iter().skip(offset).take(10).map(|call| {
                        let result = frame.messages.iter().flat_map(|m|&m.tool_results).find(|r|r.call_id == call.id);
                        json!({"call_id":call.id,"tool":call.name,"arguments":crate::qq_task_diagnostics::safe_detail(&call.arguments.to_string(), 4000),"result":result.map(|r|crate::qq_task_diagnostics::safe_detail(&serde_json::to_string(r).unwrap_or_default(), 8000)),"confirmed":result.is_some()})
                    }).collect::<Vec<_>>());
                }
            }
            rows.push(row);
        }
        Ok(json!({"runs":rows,"automatic_resume":false}))
    }
}

pub(super) struct RunJournal {
    store: Arc<RunStore>,
    pub(super) id: String,
}
impl RunJournal {
    pub(super) fn finish(&self, run: &mut WorkerRun) -> Result<(), LlmError> {
        let mut active = self.store.lock.lock().map_err(|_| storage())?;
        let mut record = self.store.read(&self.id)?.ok_or_else(storage)?;
        if record.end.is_some() {
            return Err(storage());
        }
        if let Some(checkpoint) = &record.checkpoint {
            let persisted = usage_of(checkpoint.provider_requests, checkpoint.used_tool_calls);
            run.usage.tool_calls = run.usage.tool_calls.max(persisted.tool_calls);
            run.usage.model_requests = run.usage.model_requests.max(persisted.model_requests);
        }
        if matches!(
            record.frozen.verification,
            TaskCompletionContract::FrozenNodeTest { .. }
        ) {
            let verification = match &run.end {
                TaskEnd::Completed { result } => match &record.checkpoint {
                    Some(frame)
                        if run.usage
                            == usage_of(frame.provider_requests, frame.used_tool_calls) =>
                    {
                        eve_task_plugin::evaluate_checkpoint_completion(
                            &record.frozen.verification,
                            &record.frozen.owner,
                            frame,
                            record.frozen.steps as usize,
                            result,
                            &record.frozen_node_evidence,
                        )
                    }
                    Some(_) => TaskCompletionAssessment::Unverifiable {
                        reason: "uncheckpointed_execution_usage".into(),
                    },
                    None => TaskCompletionAssessment::Unverifiable {
                        reason: "missing_checkpoint".into(),
                    },
                },
                TaskEnd::Cancelled => TaskCompletionAssessment::Blocked {
                    reason: "subagent_cancelled".into(),
                },
                TaskEnd::Blocked { .. } => TaskCompletionAssessment::Blocked {
                    reason: "subagent_execution_blocked".into(),
                },
                TaskEnd::Failed { .. } | TaskEnd::SessionEnded { .. } => {
                    TaskCompletionAssessment::Unverifiable {
                        reason: "subagent_execution_not_completed".into(),
                    }
                }
            };
            if !verification.verified()
                && let TaskEnd::Completed { result } = &run.end
            {
                run.end = TaskEnd::Blocked {
                    result: result.clone(),
                };
            }
            run.verification = Some(verification);
        }
        record.end = Some(End::capture(&run.end)?);
        record.usage = Some(run.usage);
        record.verification = run.verification.clone();
        record.finished_at_ms = Some(now_ms());
        self.store.write(&self.id, &record)?;
        active.remove(&self.id);
        Ok(())
    }
}
impl Drop for RunJournal {
    fn drop(&mut self) {
        if let Ok(mut active) = self.store.lock.lock() {
            active.remove(&self.id);
        }
    }
}
impl TurnJournal for RunJournal {
    fn load(&self) -> Result<Option<TurnCheckpoint>, LlmError> {
        let _lock = self.store.lock.lock().map_err(|_| storage())?;
        let record = self.store.read(&self.id)?.ok_or_else(storage)?;
        if record.end.is_some() {
            return Err(storage());
        }
        Ok(record.checkpoint)
    }
    fn save(&self, checkpoint: &TurnCheckpoint) -> Result<(), LlmError> {
        let _lock = self.store.lock.lock().map_err(|_| storage())?;
        let mut record = self.store.read(&self.id)?.ok_or_else(storage)?;
        if record.end.is_some()
            || record.checkpoint.as_ref().is_some_and(|old| {
                checkpoint.used_tool_calls < old.used_tool_calls
                    || checkpoint.provider_requests < old.provider_requests
                    || (matches!(
                        record.frozen.verification,
                        TaskCompletionContract::FrozenNodeTest { .. }
                    ) && old.result.is_some())
            })
        {
            return Err(storage());
        }
        record.checkpoint = Some(checkpoint.clone());
        if matches!(
            record.frozen.verification,
            TaskCompletionContract::FrozenNodeTest { .. }
        ) {
            // 先核可信身份与预算，再读运行器私有账本；checkpoint 与 sidecar 同次提交。
            record.validate()?;
            let captured = self
                .store
                .completion_evidence
                .as_ref()
                .map(|source| source.capture(&record.frozen.owner, checkpoint))
                .transpose()
                .map_err(|_| storage())?
                .unwrap_or_default();
            record.frozen_node_evidence = eve_task_api::merge_frozen_node_evidence(
                &record.frozen.owner,
                checkpoint,
                &record.frozen_node_evidence,
                captured,
            )
            .map_err(|_| storage())?;
        }
        self.store.write(&self.id, &record)
    }
}

pub(crate) struct ListRuns {
    pub(crate) late: Arc<Late>,
}
impl Tool for ListRuns {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition { name: LIST.into(), description: "读取当前会话子代理的持久记录；不传参数列最近20项，run_id读取原任务、结局和真实工具回执，offset用于列表按20条翻页、详情步骤按10条翻页。run_id配evidence=true只返回冻结验收项对应的历史回执简表，不传offset；它保留原调用编号与摘要，不含原文，也不证明当前源码、环境或整个目标已通过。中断未知不重跑，本工具只读不会恢复执行或补发消息。".into(), argument_schema: json!({"type":"object","properties":{"run_id":{"type":"string"},"offset":{"type":"integer","minimum":0},"evidence":{"type":"boolean"}},"additionalProperties":false}), required_permissions: vec![], concurrency: Some(ToolConcurrency::ParallelSafe) }
    }
    fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolValidationError> {
        let Some(object) = arguments.as_object() else {
            return Err(invalid("参数必须是对象"));
        };
        if object
            .keys()
            .any(|k| !matches!(k.as_str(), "run_id" | "offset" | "evidence"))
            || object
                .get("run_id")
                .is_some_and(|v| v.as_str().is_none_or(|s| !valid_id(s)))
            || object
                .get("offset")
                .is_some_and(|v| v.as_u64().is_none_or(|n| n > 1_000_000))
            || object.get("evidence").is_some_and(|v| !v.is_boolean())
            || (object.get("evidence") == Some(&json!(true))
                && (!object.contains_key("run_id") || object.contains_key("offset")))
        {
            return Err(invalid(
                "run_id须为有效记录ID，offset须为0至1000000整数；evidence=true须提供run_id且不传offset",
            ));
        }
        Ok(())
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            self.validate_arguments(&call.arguments)
                .map_err(|e| failed(e.message))?;
            let owner = owner(&context)?;
            let worker = self
                .late
                .worker
                .get()
                .ok_or_else(|| failed("子代理执行器尚未就绪"))?;
            if call.arguments.get("evidence") == Some(&json!(true)) {
                let id = call.arguments["run_id"]
                    .as_str()
                    .ok_or_else(|| failed("缺少子代理记录ID"))?;
                return worker.runs.evidence_view(&owner, id).map_err(failed);
            }
            worker
                .runs
                .views(
                    &owner,
                    call.arguments.get("run_id").and_then(Value::as_str),
                    call.arguments
                        .get("offset")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize,
                )
                .map_err(failed)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_llm_api::{ChatMessage, ChatRole, ToolResult};

    pub(super) fn open(path: &std::path::Path) -> Arc<RunStore> {
        RunStore::open(Arc::new(SqliteRunStorage::open(path).unwrap()))
    }
    pub(super) fn frozen() -> FrozenRun {
        let scope = ContextScope {
            session_id: "session".into(),
            user_id: "owner".into(),
        };
        FrozenRun {
            owner: TaskOwner {
                session_id: scope.session_id.clone(),
                user_id: scope.user_id.clone(),
            },
            model_session: ModelSession::scoped("main", &scope)
                .unwrap()
                .with_turn("7")
                .unwrap()
                .fork("subagent", Some("7:code"))
                .unwrap(),
            parent_call_id: "code".into(),
            profile: crate::qq_subagents::builtin("coding").unwrap(),
            instructions: "核验原任务，保留已有工作".into(),
            context: None,
            review_messages: vec!["真实用户授权".into()],
            verification: TaskCompletionContract::Unspecified,
            steps: 4,
            deadline_at_ms: Some(u64::MAX),
        }
    }
    pub(super) fn frame(frozen: &FrozenRun, pending: bool) -> TurnCheckpoint {
        let call = ToolCall {
            id: "once".into(),
            name: "read_file".into(),
            arguments: json!({"path":"README.md"}),
        };
        let mut messages = vec![
            ChatMessage::text(ChatRole::System, "系统"),
            ChatMessage::text(ChatRole::System, "工具"),
            ChatMessage::text(ChatRole::User, frozen.instructions.clone()),
        ];
        if !pending {
            messages.push(ChatMessage::assistant_tool_calls(vec![call.clone()]).unwrap());
            messages.push(
                ChatMessage::tool_results(vec![
                    ToolResult::success("once", json!({"data":"真实回执"})).unwrap(),
                ])
                .unwrap(),
            );
        }
        TurnCheckpoint {
            version: 1,
            scope: ContextScope {
                session_id: frozen.owner.session_id.clone(),
                user_id: frozen.owner.user_id.clone(),
            },
            model_session: Some(frozen.model_session.clone()),
            pending_model_request: None,
            pending_request_fingerprint: None,
            messages,
            pending_calls: if pending { vec![call] } else { vec![] },
            used_tool_calls: 1,
            delegated_tool_calls: 0,
            delegated_model_requests: 0,
            provider_requests: 1,
            tool_batches: 1,
            remaining: None,
            continuation_checks: 0,
            closing: false,
            result: None,
        }
    }
    #[tokio::test]
    async fn restart_keeps_original_task_authority_and_receipts_and_never_restarts_old_call() {
        for pending in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("runs.sqlite3");
            let store = open(&path);
            let original = frozen();
            let journal = store.begin(original.clone()).unwrap();
            let checkpoint = frame(&original, pending);
            journal.save(&checkpoint).unwrap();
            let id = journal.id.clone();
            assert_eq!(
                store.views(&original.owner, Some(&id), 0).unwrap()["runs"][0]["status"],
                "running"
            );
            drop(journal);
            drop(store);
            let store = open(&path);
            let saved = store.read(&id).unwrap().unwrap();
            assert!(saved.frozen == original);
            assert!(saved.checkpoint.as_ref() == Some(&checkpoint));
            assert_eq!(
                store.views(&original.owner, Some(&id), 0).unwrap()["runs"][0]["status"],
                if pending {
                    "interrupted_unknown"
                } else {
                    "interrupted_confirmed"
                }
            );
            assert!(store.begin(original.clone()).is_err());
            let stranger = TaskOwner {
                user_id: "other".into(),
                ..original.owner
            };
            assert!(
                store.views(&stranger, Some(&id), 0).unwrap()["runs"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            drop(store);
        }
    }
    #[test]
    fn restart_exposes_saved_result_without_settling_or_restarting_the_run() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runs.sqlite3");
        let store = open(&path);
        let original = frozen();
        let journal = store.begin(original.clone()).unwrap();
        let id = journal.id.clone();
        let mut checkpoint = frame(&original, false);
        checkpoint.result = Some("检查完成，实际输出已保存，但父调用尚未结算。".into());
        journal.save(&checkpoint).unwrap();
        drop(journal);
        drop(store);
        let store = open(&path);
        let detail = store.views(&original.owner, Some(&id), 0).unwrap();
        let row = &detail["runs"][0];
        assert_eq!(row["status"], "result_saved_unsettled");
        assert_eq!(row["saved_result"], checkpoint.result.as_deref().unwrap());
        assert!(row["result"].is_null());
        assert_eq!(row["automatic_resume"], false);
        assert!(store.begin(original).is_err());
        let record = store.read(&id).unwrap().unwrap();
        assert!(record.end.is_none());
        assert!(record.checkpoint.as_ref() == Some(&checkpoint));
    }
    #[test]
    fn timeout_settlement_keeps_leaf_usage_in_record_and_result_without_resetting_root_pool() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runs.sqlite3");
        let store = open(&path);
        let original = frozen();
        let journal = store.begin(original.clone()).unwrap();
        let id = journal.id.clone();
        let root_pool = eve_llm_api::SharedToolSteps::new(100).unwrap();
        root_pool.reserve(7).unwrap();
        root_pool.record_model_request().unwrap();
        root_pool.record_model_request().unwrap();
        let original_usage = root_pool.usage().unwrap();
        let mut checkpoint = frame(&original, true);
        checkpoint.pending_calls.push(ToolCall {
            id: "second".into(),
            name: "read_file".into(),
            arguments: json!({"path":"Cargo.toml"}),
        });
        checkpoint.used_tool_calls = 2;
        journal.save(&checkpoint).unwrap();
        let mut run = WorkerRun {
            end: TaskEnd::Failed {
                reason: "执行超时".into(),
            },
            usage: TaskUsage::default(),
            verification: None,
            retryable: false,
        };
        journal.finish(&mut run).unwrap();
        assert_eq!(run.usage.tool_calls, 2);
        assert_eq!(run.usage.model_requests, 1);
        assert_eq!(root_pool.usage().unwrap(), original_usage);
        drop(journal);
        drop(store);
        let store = open(&path);
        let saved = store.read(&id).unwrap().unwrap();
        assert_eq!(saved.usage.unwrap(), run.usage);
        assert!(saved.checkpoint.as_ref() == Some(&checkpoint));
        assert_eq!(root_pool.usage().unwrap(), original_usage);
        let detail = store.views(&original.owner, Some(&id), 0).unwrap();
        assert_eq!(detail["runs"][0]["status"], "failed");
        assert_eq!(detail["runs"][0]["effects_unknown"], true);
    }
    #[test]
    fn wrong_owner_columns_are_rejected_live_and_on_reopen_without_rewriting_rows() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runs.sqlite3");
        let store = open(&path);
        let original = frozen();
        let journal = store.begin(original.clone()).unwrap();
        let id = journal.id.clone();
        drop(journal);
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute("UPDATE runs SET user_id='stranger' WHERE id=?1", [&id])
            .unwrap();
        let stranger = TaskOwner {
            user_id: "stranger".into(),
            ..original.owner
        };
        assert!(store.views(&stranger, Some(&id), 0).is_err());
        assert!(store.views(&stranger, None, 0).is_err());
        drop(store);
        assert!(SqliteRunStorage::open(&path).is_err());
        let user: String = connection
            .query_row("SELECT user_id FROM runs WHERE id=?1", [&id], |r| r.get(0))
            .unwrap();
        assert_eq!(user, "stranger");
    }
    #[test]
    fn wrong_entry_id_is_rejected_live_and_on_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runs.sqlite3");
        let store = open(&path);
        let original = frozen();
        let journal = store.begin(original.clone()).unwrap();
        let id = journal.id.clone();
        drop(journal);
        let connection = rusqlite::Connection::open(&path).unwrap();
        let entry = serde_json::to_vec(&Entry {
            id: "0".repeat(64),
            owner: original.owner.clone(),
            instructions: "错误绑定".into(),
            created_at_ms: 0,
        })
        .unwrap();
        connection
            .execute(
                "UPDATE runs SET entry=?2 WHERE id=?1",
                rusqlite::params![id, entry],
            )
            .unwrap();
        assert!(store.views(&original.owner, Some(&id), 0).is_err());
        drop(store);
        assert!(SqliteRunStorage::open(&path).is_err());
    }
    #[test]
    fn corrupt_database_is_not_recreated_or_cleared() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runs.sqlite3");
        std::fs::write(&path, b"corrupt-original").unwrap();
        assert!(SqliteRunStorage::open(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"corrupt-original");
    }
}
