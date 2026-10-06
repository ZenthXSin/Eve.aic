//! 通过公开 StateStore 保存受控文档行动；管理能力只由宿主持有。
//!
//! begin 在返回前保存 Executing，finish 在保存成功后更新内存；启动时所有遗留
//! Executing 先保存为 Blocked/Interrupted。存储故障和损坏状态不会触发清空或重放。
mod execution;
mod strict_json;

pub use execution::{ActionCancellation, execute_document_action};

use eve_action_api::*;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

pub const ACTION_STATE_KEY: &str = "document-actions.v1";

struct Inner {
    snapshot: ActionSnapshot,
    context: Option<PluginContext>,
}

struct StoredActions {
    inner: Mutex<Inner>,
}

impl StoredActions {
    fn open(context: PluginContext, subject_id: &str) -> ActionResult<Self> {
        let bytes = context
            .state_get(ACTION_STATE_KEY)
            .map_err(|_| ActionError::Storage)?;
        let mut snapshot = match bytes {
            None => ActionSnapshot {
                schema_version: ACTION_SCHEMA_VERSION,
                subject_id: subject_id.into(),
                revision: 0,
                records: Vec::new(),
            },
            Some(bytes) => {
                if bytes.len() > MAX_ACTION_STATE_BYTES {
                    return Err(ActionError::LimitReached);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| ActionError::CorruptState)?;
                let snapshot: ActionSnapshot =
                    serde_json::from_value(value).map_err(|_| ActionError::CorruptState)?;
                snapshot.validate().map_err(|_| ActionError::CorruptState)?;
                if snapshot.subject_id != subject_id {
                    return Err(ActionError::SubjectMismatch);
                }
                snapshot
            }
        };

        let mut recovered = false;
        for record in &mut snapshot.records {
            if record.status == ActionStatus::Executing {
                record.status = ActionStatus::Blocked;
                record.failure = Some(ActionFailure::Interrupted);
                record.finished_at_ms = Some(now_ms()?);
                record.revision = record
                    .revision
                    .checked_add(1)
                    .ok_or(ActionError::LimitReached)?;
                recovered = true;
            }
        }
        if recovered {
            snapshot.revision = snapshot
                .revision
                .checked_add(1)
                .ok_or(ActionError::LimitReached)?;
            context
                .state_set(ACTION_STATE_KEY, encode(&snapshot)?)
                .map_err(|_| ActionError::Storage)?;
        }

        Ok(Self {
            inner: Mutex::new(Inner {
                snapshot,
                context: Some(context),
            }),
        })
    }

    fn lock(&self) -> ActionResult<MutexGuard<'_, Inner>> {
        let inner = self.inner.lock().map_err(|_| ActionError::Unavailable)?;
        if inner.context.is_none() {
            return Err(ActionError::Unavailable);
        }
        Ok(inner)
    }

    fn snapshot(&self) -> ActionResult<ActionSnapshot> {
        Ok(self.lock()?.snapshot.clone())
    }

    fn begin(&self, proposal: DocumentActionProposal) -> ActionResult<ActionBegin> {
        proposal.validate()?;
        let mut inner = self.lock()?;
        if proposal.subject_id != inner.snapshot.subject_id {
            return Err(ActionError::SubjectMismatch);
        }
        if let Some(record) = inner
            .snapshot
            .records
            .iter()
            .find(|record| record.proposal.action_id == proposal.action_id)
        {
            if !record.proposal.same_request(&proposal) {
                return Err(ActionError::Conflict);
            }
            return Ok(ActionBegin {
                record: record.clone(),
                duplicate: true,
            });
        }
        if inner.snapshot.records.len() >= MAX_ACTION_RECORDS {
            return Err(ActionError::LimitReached);
        }
        let record = ActionRecord {
            revision: 1,
            proposal,
            status: ActionStatus::Executing,
            finished_at_ms: None,
            receipt: None,
            failure: None,
        };
        let mut next = inner.snapshot.clone();
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or(ActionError::LimitReached)?;
        next.records.push(record.clone());
        persist(&mut inner, next)?;
        Ok(ActionBegin {
            record,
            duplicate: false,
        })
    }

    fn finish(
        &self,
        action_id: &str,
        expected_record_revision: u64,
        outcome: ActionOutcome,
    ) -> ActionResult<ActionRecord> {
        if !valid_id(action_id) || expected_record_revision == 0 {
            return Err(ActionError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let index = inner
            .snapshot
            .records
            .iter()
            .position(|record| record.proposal.action_id == action_id)
            .ok_or(ActionError::InvalidInput)?;
        let previous = &inner.snapshot.records[index];
        if previous.revision != expected_record_revision {
            return Err(ActionError::StaleRevision);
        }
        if previous.status != ActionStatus::Executing {
            return Err(ActionError::InvalidTransition);
        }
        let mut record = previous.clone();
        match outcome {
            ActionOutcome::Completed(receipt) => {
                receipt.validate()?;
                if !receipt.matches_proposal(&record.proposal) {
                    return Err(ActionError::InvalidInput);
                }
                record.status = ActionStatus::Completed;
                record.finished_at_ms = Some(receipt.verified_at_ms);
                record.receipt = Some(receipt);
            }
            ActionOutcome::Blocked {
                failure,
                finished_at_ms,
            } => {
                record.status = ActionStatus::Blocked;
                record.finished_at_ms = Some(finished_at_ms);
                record.failure = Some(failure);
            }
        }
        record.revision = record
            .revision
            .checked_add(1)
            .ok_or(ActionError::LimitReached)?;
        record.validate()?;
        let mut next = inner.snapshot.clone();
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or(ActionError::LimitReached)?;
        next.records[index] = record.clone();
        persist(&mut inner, next)?;
        Ok(record)
    }

    fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("行动状态锁不可用".into()))?
            .context
            .take();
        Ok(())
    }
}

/// 先保存再发布新快照；失败时旧内存和既有执行记录保持不变。
fn persist(inner: &mut Inner, snapshot: ActionSnapshot) -> ActionResult<()> {
    inner
        .context
        .as_ref()
        .ok_or(ActionError::Unavailable)?
        .state_set(ACTION_STATE_KEY, encode(&snapshot)?)
        .map_err(|_| ActionError::Storage)?;
    inner.snapshot = snapshot;
    Ok(())
}

fn encode(snapshot: &ActionSnapshot) -> ActionResult<Vec<u8>> {
    snapshot.validate()?;
    let bytes = serde_json::to_vec(snapshot).map_err(|_| ActionError::InvalidInput)?;
    if bytes.len() > MAX_ACTION_STATE_BYTES {
        return Err(ActionError::LimitReached);
    }
    Ok(bytes)
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn now_ms() -> ActionResult<u64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ActionError::Unavailable)?;
    let value = u64::try_from(elapsed.as_millis()).map_err(|_| ActionError::LimitReached)?;
    if value == 0 {
        return Err(ActionError::Unavailable);
    }
    Ok(value)
}

/// 管理句柄不发布到服务目录；插件停止后所有方法返回 Unavailable。
#[derive(Clone, Default)]
pub struct ActionController {
    active: Arc<Mutex<Option<Arc<StoredActions>>>>,
}

impl ActionController {
    fn service(&self) -> ActionResult<Arc<StoredActions>> {
        self.active
            .lock()
            .map_err(|_| ActionError::Unavailable)?
            .clone()
            .ok_or(ActionError::Unavailable)
    }
}

impl ActionJournal for ActionController {
    fn snapshot(&self) -> ActionResult<ActionSnapshot> {
        self.service()?.snapshot()
    }

    fn begin(&self, proposal: DocumentActionProposal) -> ActionResult<ActionBegin> {
        self.service()?.begin(proposal)
    }

    fn finish(
        &self,
        action_id: &str,
        expected_record_revision: u64,
        outcome: ActionOutcome,
    ) -> ActionResult<ActionRecord> {
        self.service()?
            .finish(action_id, expected_record_revision, outcome)
    }
}

pub struct ActionPlugin {
    manifest: PluginManifest,
    subject_id: String,
    controller: ActionController,
}

impl ActionPlugin {
    pub fn new(subject_id: impl Into<String>) -> PluginResult<Self> {
        let subject_id = subject_id.into();
        if !valid_id(&subject_id) {
            return Err(PluginError::State(ActionError::InvalidInput.to_string()));
        }
        Ok(Self {
            manifest: PluginManifest::new(ACTION_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            subject_id,
            controller: ActionController::default(),
        })
    }

    pub fn controller(&self) -> ActionController {
        self.controller.clone()
    }
}

impl Plugin for ActionPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                StoredActions::open(context.clone(), &self.subject_id)
                    .map_err(|error| PluginError::State(error.to_string()))?,
            );
            let to_close = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("行动管理句柄不可用".into()))?;
                if active
                    .as_ref()
                    .is_some_and(|service| Arc::ptr_eq(service, &to_close))
                {
                    *active = None;
                }
                Ok(())
            }))?;
            *self
                .controller
                .active
                .lock()
                .map_err(|_| PluginError::State("行动管理句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
