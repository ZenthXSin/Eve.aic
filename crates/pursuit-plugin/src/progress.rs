use eve_cognition_api::{
    CognitionAdmin, CognitionError, CognitiveEvent, CognitiveEventKind, CognitiveState, GoalStatus,
    MAX_RECORDS, Source, SourceKind,
};
use eve_pursuit_api::{
    PROGRESS_SCHEMA, PURSUIT_CHANNEL, ProgressFacts, ProgressOutcome, ProgressRecord,
};
use std::{
    cmp::Reverse,
    sync::{Mutex, MutexGuard},
};

static ADMISSION: Mutex<()> = Mutex::new(());

/// 宿主在读取事实并保存暂停，以及最新检查点核对并 begin 的同步段共用此锁；禁止跨 await 持有。
pub fn progress_admission_guard() -> Result<MutexGuard<'static, ()>, CognitionError> {
    ADMISSION.lock().map_err(|_| CognitionError::Unavailable)
}

/// 容量已满无法持久化新进展时关闭目标准入，不能默认为未暂停。
pub fn progress_admitted(state: &CognitiveState, goal_id: &str) -> Result<bool, CognitionError> {
    let record = progress_record(state, goal_id)?;
    Ok(state.events.len() < MAX_RECORDS && !record.is_some_and(|record| record.paused))
}

fn event_id(goal_id: &str) -> String {
    format!(
        "pursuit-progress-{}",
        crate::stop_event_id(goal_id, 0).trim_start_matches("pursuit-stop-")
    )
}

/// 读取单目标追加事件链的最新检查点；损坏记录报错，不解释为首次运行。
pub fn progress_record(
    state: &CognitiveState,
    goal_id: &str,
) -> Result<Option<ProgressRecord>, CognitionError> {
    let prefix = format!("{}-", event_id(goal_id));
    let mut latest: Option<ProgressRecord> = None;
    for (sequence, event) in state
        .events
        .iter()
        .filter(|event| event.id.starts_with(&prefix))
        .enumerate()
    {
        let record: ProgressRecord =
            serde_json::from_str(&event.summary).map_err(|_| CognitionError::InvalidInput)?;
        if event.id != format!("{prefix}{sequence}")
            || record.schema != PROGRESS_SCHEMA
            || record.goal_id != goal_id
            || event.kind != CognitiveEventKind::StateChanged
            || record.last_progress_at_ms == 0
            || record.last_progress_at_ms > event.at_ms
            || state
                .goals
                .get(goal_id)
                .is_none_or(|goal| record.goal_revision > goal.revision)
            || latest.as_ref().is_some_and(|old| {
                record.evidence_revision < old.evidence_revision
                    || record.last_progress_at_ms < old.last_progress_at_ms
                    || record.goal_revision < old.goal_revision
                    || (old.paused
                        && !record.paused
                        && record.evidence_revision <= old.evidence_revision)
            })
            || event.source.kind != SourceKind::Internal
            || event.source.channel != PURSUIT_CHANNEL
            || event.source.reference != goal_id
            || state
                .goals
                .get(goal_id)
                .is_none_or(|goal| event.visibility != goal.visibility)
            || event.goal_id.as_deref() != Some(goal_id)
            || record.waiting_marker.is_empty()
        {
            return Err(CognitionError::InvalidInput);
        }
        latest = Some(record);
    }
    let Some(record) = latest else {
        return Ok(None);
    };
    Ok(Some(record))
}

/// 未服务或最久未服务者优先，同一服务时间按原优先级及 ID 排序。
/// 时间取实际持久化运行的 started_at_ms；重启不重新获得“从未服务”资格。
pub fn fair_order_key(
    last_started_at_ms: Option<u64>,
    priority: u8,
    goal_id: &str,
) -> (u64, Reverse<u8>, String) {
    (
        last_started_at_ms.unwrap_or(0),
        Reverse(priority),
        goal_id.into(),
    )
}

/// 只管理等待新资料的目标；暂停不取消运行中的操作，恢复不重放旧版本的执行。
/// 调用者必须验证目标来源、证据拥有者、兴趣仍有效，以及跨版本剩余次数。
/// 并持有 progress_admission_guard 覆盖事实读取与此调用，防止与新任务准入竞争。
pub fn reconcile_progress(
    admin: &dyn CognitionAdmin,
    facts: &ProgressFacts,
    stall_ms: u64,
) -> Result<ProgressOutcome, CognitionError> {
    if facts.now_ms == 0 || stall_ms == 0 || facts.last_activity_at_ms > facts.now_ms {
        return Err(CognitionError::InvalidInput);
    }
    let snapshot = admin.snapshot()?;
    let Some(goal) = snapshot.state.goals.get(&facts.goal_id) else {
        return Ok(ProgressOutcome::Unchanged);
    };
    if goal.revision != facts.goal_revision
        || goal.execution.is_some()
        || goal.feedback.is_some()
        || goal.block_reason.is_some()
        || goal.completion.is_some()
    {
        return Ok(ProgressOutcome::Unchanged);
    }
    let previous = progress_record(&snapshot.state, &goal.id)?;
    let mut record = match previous.clone() {
        Some(record) => record,
        None if goal.status == GoalStatus::Waiting => ProgressRecord {
            schema: PROGRESS_SCHEMA.into(),
            goal_id: goal.id.clone(),
            goal_revision: goal.revision,
            evidence_revision: facts.evidence_revision,
            last_progress_at_ms: if facts.last_activity_at_ms > 0 {
                facts.last_activity_at_ms
            } else {
                facts.now_ms
            },
            paused: false,
            waiting_marker: goal
                .wait_reason
                .clone()
                .ok_or(CognitionError::InvalidInput)?,
        },
        _ => return Ok(ProgressOutcome::Unchanged),
    };
    if facts.evidence_revision < record.evidence_revision {
        return Err(CognitionError::InvalidInput);
    }
    let mut state = snapshot.state.clone();
    let outcome;
    if record.paused {
        // 暂停只关闭调度，不把目标改成不可恢复的 Cancelled。
        // 新证据须先由原派生器同步，宿主 can_resume 检查既有预算和不确定提交。
        if goal.status != GoalStatus::Waiting
            || facts.evidence_revision <= record.evidence_revision
            || !facts.can_resume
        {
            return Ok(ProgressOutcome::Unchanged);
        }
        record.paused = false;
        record.evidence_revision = facts.evidence_revision;
        record.last_progress_at_ms = facts.now_ms;
        record.goal_revision = goal.revision;
        outcome = ProgressOutcome::Resumed;
    } else {
        if goal.status != GoalStatus::Waiting {
            return Ok(ProgressOutcome::Unchanged);
        }
        // 新证据或实际活动刷新进展，不因轮询、服务重启或预算窗口变化刷新。
        if facts.evidence_revision > record.evidence_revision
            || facts.last_activity_at_ms > record.last_progress_at_ms
        {
            record.last_progress_at_ms = facts.last_activity_at_ms.max(record.last_progress_at_ms);
            if facts.evidence_revision > record.evidence_revision {
                record.last_progress_at_ms = facts.now_ms;
            }
            record.evidence_revision = facts.evidence_revision;
        }
        record.goal_revision = goal.revision;
        record.waiting_marker = goal
            .wait_reason
            .clone()
            .ok_or(CognitionError::InvalidInput)?;
        if facts.waiting_for_new_input
            && facts.now_ms.saturating_sub(record.last_progress_at_ms) >= stall_ms
        {
            record.paused = true;
            outcome = ProgressOutcome::Paused;
        } else {
            outcome = ProgressOutcome::Observed;
        }
    }
    if previous.as_ref() == Some(&record) {
        return Ok(ProgressOutcome::Unchanged);
    }
    let prefix = format!("{}-", event_id(&facts.goal_id));
    let sequence = state
        .events
        .iter()
        .filter(|event| event.id.starts_with(&prefix))
        .count();
    let id = format!("{prefix}{sequence}");
    let summary = serde_json::to_string(&record).map_err(|_| CognitionError::InvalidInput)?;
    let event = CognitiveEvent {
        id: id.clone(),
        kind: CognitiveEventKind::StateChanged,
        source: Source {
            kind: SourceKind::Internal,
            channel: PURSUIT_CHANNEL.into(),
            reference: facts.goal_id.clone(),
        },
        visibility: goal.visibility.clone(),
        goal_id: Some(facts.goal_id.clone()),
        caused_by: None,
        at_ms: facts.now_ms,
        summary,
    };
    if state.events.len() >= MAX_RECORDS {
        return Ok(ProgressOutcome::Deferred);
    }
    state.events.push(event);
    match admin.replace(snapshot.revision, state) {
        Ok(_) => Ok(outcome),
        Err(CognitionError::StaleRevision | CognitionError::LimitReached) => {
            Ok(ProgressOutcome::Deferred)
        }
        Err(error) => Err(error),
    }
}
