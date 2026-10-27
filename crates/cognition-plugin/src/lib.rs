//! 通过公开 StateStore 持久化认知快照；不调用模型或执行工具。
mod file_observation;
mod goal_feedback;
mod strict_json;
mod world_model;
use eve_cognition_api::*;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginId, PluginManifest,
    PluginResult, ServiceId, StateStore, cleanup,
};
pub use file_observation::UserGoalFileObservation;
pub use goal_feedback::UserGoalFeedback;
use std::sync::{Arc, Mutex, MutexGuard};
pub use world_model::{
    WorldModelController, WorldModelPlugin, capability_estimates, evaluate_world_receipt,
};

pub const COGNITION_STATE_KEY: &str = "cognition.v1";

/// 宿主读取完整持久快照，不启动插件、不恢复在途目标，也不写入状态。
///
/// 返回值包含所有可见范围的数据；宿主向用户展示时必须自行绑定读权限。
/// 缺失状态返回修订为零的空快照，损坏或不兼容的状态返回脱敏错误。
pub fn read_cognitive_snapshot(
    store: &dyn StateStore,
    subject_id: &str,
) -> CognitionResult<CognitiveSnapshot> {
    validate_id(subject_id)?;
    let namespace = PluginId::new(COGNITION_PLUGIN_ID).map_err(|_| CognitionError::InvalidInput)?;
    let bytes = store
        .get(&namespace, COGNITION_STATE_KEY)
        .map_err(|_| CognitionError::Storage)?;
    decode_snapshot(bytes.as_deref(), subject_id)
}

fn decode_snapshot(bytes: Option<&[u8]>, subject_id: &str) -> CognitionResult<CognitiveSnapshot> {
    validate_id(subject_id)?;
    let Some(bytes) = bytes else {
        return Ok(CognitiveSnapshot {
            format_version: COGNITION_FORMAT_VERSION,
            subject_id: subject_id.into(),
            revision: 0,
            state: CognitiveState::default(),
        });
    };
    if bytes.len() > MAX_STATE_BYTES {
        return Err(CognitionError::LimitReached);
    }
    let value = strict_json::from_slice(bytes).map_err(|_| CognitionError::CorruptState)?;
    let snapshot: CognitiveSnapshot =
        serde_json::from_value(value).map_err(|_| CognitionError::CorruptState)?;
    if snapshot.format_version != COGNITION_FORMAT_VERSION {
        return Err(CognitionError::UnsupportedVersion);
    }
    if snapshot.subject_id != subject_id {
        return Err(CognitionError::SubjectMismatch);
    }
    validate_id(&snapshot.subject_id).map_err(|_| CognitionError::CorruptState)?;
    snapshot
        .state
        .validate()
        .map_err(|_| CognitionError::CorruptState)?;
    if snapshot.revision == 0 && snapshot.state != CognitiveState::default() {
        return Err(CognitionError::CorruptState);
    }
    if snapshot
        .state
        .goals
        .values()
        .any(|goal| goal.revision == 0 || goal.revision > snapshot.revision)
    {
        return Err(CognitionError::CorruptState);
    }
    Ok(snapshot)
}

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
        let mut snapshot = decode_snapshot(bytes.as_deref(), subject_id)?;
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
        for (id, old) in &inner.snapshot.state.autonomous_drives {
            let next = state
                .autonomous_drives
                .get(id)
                .ok_or(CognitionError::InvalidTransition)?;
            validate_autonomous_drive_transition(old, next)?;
        }
        for (id, drive) in &state.autonomous_drives {
            if !inner.snapshot.state.autonomous_drives.contains_key(id)
                && (!matches!(
                    drive.status,
                    AutonomousDriveStatus::Candidate | AutonomousDriveStatus::Ready
                ) || drive.attempts != 0)
            {
                return Err(CognitionError::InvalidTransition);
            }
        }
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
fn validate_autonomous_drive_transition(
    old: &AutonomousDrive,
    next: &AutonomousDrive,
) -> CognitionResult<()> {
    if old.source != next.source
        || old.visibility != next.visibility
        || old.description != next.description
        || old.evidence != next.evidence
        || old.priority != next.priority
        || old.budget != next.budget
        || old.stop_condition != next.stop_condition
    {
        return Err(CognitionError::AccessDenied);
    }
    if old.status == next.status {
        return Ok(());
    }
    let allowed = matches!(
        (&old.status, &next.status),
        (
            AutonomousDriveStatus::Candidate,
            AutonomousDriveStatus::Ready,
        ) | (
            AutonomousDriveStatus::Candidate,
            AutonomousDriveStatus::Waiting,
        ) | (
            AutonomousDriveStatus::Candidate,
            AutonomousDriveStatus::Blocked,
        ) | (
            AutonomousDriveStatus::Candidate,
            AutonomousDriveStatus::Cancelled,
        ) | (
            AutonomousDriveStatus::Ready,
            AutonomousDriveStatus::Active,
        ) | (
            AutonomousDriveStatus::Ready,
            AutonomousDriveStatus::Waiting,
        ) | (
            AutonomousDriveStatus::Ready,
            AutonomousDriveStatus::Blocked,
        ) | (
            AutonomousDriveStatus::Ready,
            AutonomousDriveStatus::Cancelled,
        ) | (
            AutonomousDriveStatus::Active,
            AutonomousDriveStatus::Waiting,
        ) | (
            AutonomousDriveStatus::Active,
            AutonomousDriveStatus::Blocked,
        ) | (
            AutonomousDriveStatus::Active,
            AutonomousDriveStatus::Completed,
        ) | (
            AutonomousDriveStatus::Waiting,
            AutonomousDriveStatus::Ready,
        ) | (
            AutonomousDriveStatus::Waiting,
            AutonomousDriveStatus::Blocked,
        ) | (
            AutonomousDriveStatus::Waiting,
            AutonomousDriveStatus::Cancelled,
    )
    );
    if allowed {
        Ok(())
    } else {
        Err(CognitionError::InvalidTransition)
    }
}
fn validate_transition(old: &Goal, next: &Goal) -> CognitionResult<()> {
    if old.source != next.source || old.visibility != next.visibility {
        return Err(CognitionError::AccessDenied);
    }
    // 用户确认完成：只能把等待中的目标一次性标为完成；确认的是当前版本，内容不变，没有执行记录。
    if old.completion.is_some() || next.completion.is_some() {
        let confirmed = old.status == GoalStatus::Waiting
            && old.completion.is_none()
            && next.status == GoalStatus::Completed
            && next
                .completion
                .as_ref()
                .is_some_and(|completion| completion.goal_revision == old.revision)
            && next.execution.is_none()
            && next.feedback.is_none()
            && old.description == next.description
            && old.verification == next.verification
            && old.budget == next.budget
            && old.priority == next.priority
            && old.stop_condition == next.stop_condition
            && old.expires_at_ms == next.expires_at_ms;
        return if confirmed {
            Ok(())
        } else {
            Err(CognitionError::InvalidTransition)
        };
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
    /// 由受信宿主运行固定自动审核；只推进候选生命周期并追加脱敏审计事件。
    pub fn review_autonomous_drives(
        &self,
        expected_revision: u64,
        now_ms: u64,
    ) -> CognitionResult<CognitiveSnapshot> {
        if now_ms == 0 {
            return Err(CognitionError::InvalidInput);
        }
        let mut snapshot = self.snapshot()?;
        if snapshot.revision != expected_revision {
            return Err(CognitionError::StaleRevision);
        }
        let mut changed = Vec::new();
        for drive in snapshot.state.autonomous_drives.values_mut() {
            if let Some(status) = drive.reviewed_status(now_ms)
                && status != drive.status
            {
                let previous = drive.status.clone();
                drive.status = status.clone();
                changed.push((drive.id.clone(), previous, status));
            }
        }
        if changed.is_empty() {
            return Ok(snapshot);
        }
        for (id, previous, status) in changed {
            let event_id = format!("autonomous-review-{expected_revision}-{id}");
            snapshot.state.events.push(CognitiveEvent {
                id: event_id,
                kind: CognitiveEventKind::StateChanged,
                source: Source {
                    kind: SourceKind::Internal,
                    channel: "cognition.autonomous.review".into(),
                    reference: id,
                },
                visibility: Visibility::Internal,
                goal_id: None,
                caused_by: None,
                at_ms: now_ms,
                summary: format!("自主驱动审核：{previous:?} -> {status:?}"),
            });
        }
        self.replace(expected_revision, snapshot.state)
    }

    /// 由受信宿主提交一个有来源的自主驱动候选；模型和普通通道不能直接调用。
    pub fn submit_autonomous_drive(
        &self,
        expected_revision: u64,
        drive: AutonomousDrive,
        at_ms: u64,
    ) -> CognitionResult<CognitiveSnapshot> {
        if at_ms == 0
            || !matches!(
                drive.status,
                AutonomousDriveStatus::Candidate | AutonomousDriveStatus::Ready
            )
            || drive.attempts != 0
        {
            return Err(CognitionError::InvalidInput);
        }
        drive.validate()?;
        let mut snapshot = self.snapshot()?;
        if snapshot.revision != expected_revision
            || snapshot.state.autonomous_drives.contains_key(&drive.id)
        {
            return Err(CognitionError::StaleRevision);
        }
        let id = format!("autonomous-drive-{expected_revision}");
        snapshot
            .state
            .autonomous_drives
            .insert(drive.id.clone(), drive.clone());
        snapshot.state.events.push(CognitiveEvent {
            id,
            kind: CognitiveEventKind::StateChanged,
            source: Source {
                kind: SourceKind::Internal,
                channel: "cognition.autonomous".into(),
                reference: drive.id.clone(),
            },
            visibility: drive.visibility.clone(),
            goal_id: None,
            caused_by: None,
            at_ms,
            summary: "自主驱动候选已提交，等待议程审核。".into(),
        });
        self.replace(expected_revision, snapshot.state)
    }

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
