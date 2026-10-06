//! 通过公开 StateStore 持久化认知快照；不调用模型或执行工具。
mod goal_feedback;
mod strict_json;
use eve_cognition_api::*;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    ServiceId, cleanup,
};
pub use goal_feedback::UserGoalFeedback;
use std::sync::{Arc, Mutex, MutexGuard};

pub const COGNITION_STATE_KEY: &str = "cognition.v1";
struct Inner {
    snapshot: CognitiveSnapshot,
    context: Option<PluginContext>,
}
struct StoredCognition {
    inner: Mutex<Inner>,
}
impl StoredCognition {
    fn open(context: PluginContext, subject_id: &str) -> CognitionResult<Self> {
        let bytes = context
            .state_get(COGNITION_STATE_KEY)
            .map_err(|_| CognitionError::Storage)?;
        let mut snapshot = match bytes {
            None => CognitiveSnapshot {
                format_version: COGNITION_FORMAT_VERSION,
                subject_id: subject_id.into(),
                revision: 0,
                state: CognitiveState::default(),
            },
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(CognitionError::LimitReached);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| CognitionError::CorruptState)?;
                let doc: CognitiveSnapshot =
                    serde_json::from_value(value).map_err(|_| CognitionError::CorruptState)?;
                if doc.format_version != COGNITION_FORMAT_VERSION {
                    return Err(CognitionError::UnsupportedVersion);
                }
                if doc.subject_id != subject_id {
                    return Err(CognitionError::SubjectMismatch);
                }
                validate_id(&doc.subject_id).map_err(|_| CognitionError::CorruptState)?;
                doc.state
                    .validate()
                    .map_err(|_| CognitionError::CorruptState)?;
                if doc.revision == 0 && doc.state != CognitiveState::default() {
                    return Err(CognitionError::CorruptState);
                }
                if doc
                    .state
                    .goals
                    .values()
                    .any(|g| g.revision == 0 || g.revision > doc.revision)
                {
                    return Err(CognitionError::CorruptState);
                }
                doc
            }
        };
        let mut recovered = false;
        for goal in snapshot.state.goals.values_mut() {
            if goal.status == GoalStatus::Executing {
                goal.status = GoalStatus::Blocked;
                goal.block_reason = Some(BlockReason::Interrupted);
                goal.revision = goal
                    .revision
                    .checked_add(1)
                    .ok_or(CognitionError::LimitReached)?;
                recovered = true;
            }
        }
        if recovered {
            snapshot.revision = snapshot
                .revision
                .checked_add(1)
                .ok_or(CognitionError::LimitReached)?;
            if let Some(agenda) = &mut snapshot.state.agenda {
                agenda
                    .candidates
                    .retain(|id| snapshot.state.goals[id].status == GoalStatus::Ready);
                if agenda
                    .selected
                    .as_ref()
                    .is_some_and(|id| snapshot.state.goals[id].status == GoalStatus::Blocked)
                {
                    agenda.selected = None;
                }
            }
            snapshot
                .state
                .validate()
                .map_err(|_| CognitionError::CorruptState)?;
            context
                .state_set(COGNITION_STATE_KEY, encode(&snapshot)?)
                .map_err(|_| CognitionError::Storage)?;
        }
        Ok(Self {
            inner: Mutex::new(Inner {
                snapshot,
                context: Some(context),
            }),
        })
    }
    fn lock(&self) -> CognitionResult<MutexGuard<'_, Inner>> {
        let inner = self.inner.lock().map_err(|_| CognitionError::Unavailable)?;
        if inner.context.is_none() {
            return Err(CognitionError::Unavailable);
        }
        Ok(inner)
    }
    fn snapshot(&self) -> CognitionResult<CognitiveSnapshot> {
        Ok(self.lock()?.snapshot.clone())
    }
    fn replace(
        &self,
        expected: u64,
        mut state: CognitiveState,
    ) -> CognitionResult<CognitiveSnapshot> {
        let mut inner = self.lock()?;
        if inner.snapshot.revision != expected {
            return Err(CognitionError::StaleRevision);
        }
        state.validate()?;
        if !state.events.starts_with(&inner.snapshot.state.events) {
            return Err(CognitionError::InvalidTransition);
        }
        for (id, old) in &inner.snapshot.state.goals {
            let next = state
                .goals
                .get_mut(id)
                .ok_or(CognitionError::InvalidTransition)?;
            if next.revision != old.revision {
                return Err(CognitionError::StaleRevision);
            }
            if next != old {
                validate_transition(old, next)?;
                next.revision = old
                    .revision
                    .checked_add(1)
                    .ok_or(CognitionError::LimitReached)?;
            }
        }
        for (id, goal) in &mut state.goals {
            if !inner.snapshot.state.goals.contains_key(id) {
                if goal.revision != 0
                    || !matches!(goal.status, GoalStatus::Ready | GoalStatus::Waiting)
                {
                    return Err(CognitionError::InvalidTransition);
                }
                goal.revision = 1;
            }
        }
        let snapshot = CognitiveSnapshot {
            format_version: COGNITION_FORMAT_VERSION,
            subject_id: inner.snapshot.subject_id.clone(),
            revision: expected
                .checked_add(1)
                .ok_or(CognitionError::LimitReached)?,
            state,
        };
        inner
            .context
            .as_ref()
            .ok_or(CognitionError::Unavailable)?
            .state_set(COGNITION_STATE_KEY, encode(&snapshot)?)
            .map_err(|_| CognitionError::Storage)?;
        inner.snapshot = snapshot.clone();
        Ok(snapshot)
    }
    fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("认知状态锁不可用".into()))?
            .context
            .take();
        Ok(())
    }
}
fn encode(snapshot: &CognitiveSnapshot) -> CognitionResult<Vec<u8>> {
    let bytes = serde_json::to_vec(snapshot).map_err(|_| CognitionError::InvalidInput)?;
    if bytes.len() > MAX_STATE_BYTES {
        return Err(CognitionError::LimitReached);
    }
    Ok(bytes)
}
fn validate_transition(old: &Goal, next: &Goal) -> CognitionResult<()> {
    if old.source != next.source || old.visibility != next.visibility {
        return Err(CognitionError::AccessDenied);
    }
    let allowed = match old.status {
        GoalStatus::Ready => matches!(
            next.status,
            GoalStatus::Ready
                | GoalStatus::Waiting
                | GoalStatus::Executing
                | GoalStatus::Cancelled
                | GoalStatus::Blocked
        ),
        GoalStatus::Waiting => matches!(
            next.status,
            GoalStatus::Ready | GoalStatus::Waiting | GoalStatus::Cancelled | GoalStatus::Blocked
        ),
        GoalStatus::Executing => matches!(
            next.status,
            GoalStatus::Executing
                | GoalStatus::Completed
                | GoalStatus::Cancelled
                | GoalStatus::Blocked
        ),
        GoalStatus::Completed | GoalStatus::Cancelled | GoalStatus::Blocked => false,
    };
    if !allowed {
        return Err(CognitionError::InvalidTransition);
    }
    if old.status == GoalStatus::Executing {
        if old.description != next.description
            || old.verification != next.verification
            || old.budget != next.budget
            || old.priority != next.priority
            || old.stop_condition != next.stop_condition
            || old.expires_at_ms != next.expires_at_ms
        {
            return Err(CognitionError::InvalidTransition);
        }
        let before = old
            .execution
            .as_ref()
            .ok_or(CognitionError::InvalidTransition)?;
        let after = next
            .execution
            .as_ref()
            .ok_or(CognitionError::InvalidTransition)?;
        let mut comparable = before.clone();
        comparable.turn_id = after.turn_id;
        if &comparable != after || (before.turn_id.is_some() && before.turn_id != after.turn_id) {
            return Err(CognitionError::InvalidTransition);
        }
        if next.status == GoalStatus::Cancelled && next.feedback.is_none() {
            return Err(CognitionError::InvalidTransition);
        }
    }
    Ok(())
}

struct ScopedReader {
    stored: Arc<StoredCognition>,
    access: ReadAccess,
}
impl CognitionReader for ScopedReader {
    fn snapshot(&self) -> CognitionResult<CognitiveView> {
        let snapshot = self.stored.snapshot()?;
        let mut state = snapshot.state;
        state
            .goals
            .retain(|_, g| g.visibility.visible_to(&self.access));
        state
            .drives
            .retain(|_, d| d.visibility.visible_to(&self.access));
        state
            .events
            .retain(|e| e.visibility.visible_to(&self.access));
        if state
            .agenda
            .as_ref()
            .is_some_and(|a| !a.visibility.visible_to(&self.access))
        {
            state.agenda = None;
        }
        Ok(CognitiveView {
            subject_id: snapshot.subject_id,
            revision: snapshot.revision,
            state,
        })
    }
}
/// 宿主保留管理句柄；不会发布到服务目录。每次启动绑定新的服务实例。
#[derive(Clone, Default)]
pub struct CognitionController {
    active: Arc<Mutex<Option<Arc<StoredCognition>>>>,
}
impl CognitionController {
    fn service(&self) -> CognitionResult<Arc<StoredCognition>> {
        self.active
            .lock()
            .map_err(|_| CognitionError::Unavailable)?
            .clone()
            .ok_or(CognitionError::Unavailable)
    }
}
impl CognitionAdmin for CognitionController {
    fn snapshot(&self) -> CognitionResult<CognitiveSnapshot> {
        self.service()?.snapshot()
    }
    fn reader(&self, access: ReadAccess) -> CognitionResult<Arc<dyn CognitionReader>> {
        access.validate()?;
        let stored = self.service()?;
        drop(stored.lock()?);
        Ok(Arc::new(ScopedReader { stored, access }))
    }
    fn replace(
        &self,
        expected_revision: u64,
        state: CognitiveState,
    ) -> CognitionResult<CognitiveSnapshot> {
        self.service()?.replace(expected_revision, state)
    }
}
pub struct CognitionPlugin {
    manifest: PluginManifest,
    subject_id: String,
    controller: CognitionController,
}
impl CognitionPlugin {
    pub fn new(subject_id: impl Into<String>) -> PluginResult<Self> {
        let subject_id = subject_id.into();
        validate_id(&subject_id).map_err(|e| PluginError::State(e.to_string()))?;
        Ok(Self {
            manifest: PluginManifest::new(COGNITION_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            subject_id,
            controller: CognitionController::default(),
        })
    }
    pub fn controller(&self) -> CognitionController {
        self.controller.clone()
    }
}
impl Plugin for CognitionPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                StoredCognition::open(context.clone(), &self.subject_id)
                    .map_err(|e| PluginError::State(e.to_string()))?,
            );
            let to_close = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("认知管理句柄不可用".into()))?;
                if active.as_ref().is_some_and(|s| Arc::ptr_eq(s, &to_close)) {
                    *active = None;
                }
                Ok(())
            }))?;
            *self
                .controller
                .active
                .lock()
                .map_err(|_| PluginError::State("认知管理句柄不可用".into()))? =
                Some(stored.clone());
            context.provide_service(
                ServiceId::new(COGNITION_READ_SERVICE_ID)?,
                CognitionReadHandle(Arc::new(ScopedReader {
                    stored,
                    access: ReadAccess::Public,
                })),
            )?;
            Ok(None)
        })
    }
}
