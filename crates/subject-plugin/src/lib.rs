//! 单一主体的有界持久账本；执行能力仍由宿主注入，不直接执行模型或外部副作用。
mod driver;
mod opportunity;
pub use opportunity::{Opportunity, decode_opportunity, encode_opportunity, select_opportunity};
mod strict_json;
pub use driver::{SubjectDispatch, SubjectDriver};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
use eve_subject_api::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DedupeEntry {
    scope: SubjectScope,
    source: EventSource,
    key: String,
    event_id: String,
    at_ms: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    format_version: u32,
    next: u64,
    generation: u64,
    events: Vec<SubjectEvent>,
    foci: Vec<SubjectFocus>,
    dedupe: Vec<DedupeEntry>,
}
impl Default for Ledger {
    fn default() -> Self {
        Self {
            format_version: 1,
            next: 0,
            generation: 0,
            events: vec![],
            foci: vec![],
            dedupe: vec![],
        }
    }
}
struct Store {
    ledger: Ledger,
    context: Option<PluginContext>,
}

/// 句柄生命周期跟随插件；持久提交失败后整个句柄关闭，避免旧缓存覆盖已提交的数据。
#[derive(Clone, Default)]
pub struct SubjectController {
    active: Arc<Mutex<Option<Arc<Mutex<Store>>>>>,
}
impl SubjectController {
    fn read<T>(&self, f: impl FnOnce(&Ledger) -> SubjectResult<T>) -> SubjectResult<T> {
        let active = self.active.lock().map_err(|_| SubjectError::Unavailable)?;
        let store = active
            .as_ref()
            .ok_or(SubjectError::Unavailable)?
            .lock()
            .map_err(|_| SubjectError::Unavailable)?;
        if store.context.is_none() {
            return Err(SubjectError::Unavailable);
        }
        f(&store.ledger)
    }
    fn update<T>(&self, f: impl FnOnce(&mut Ledger) -> SubjectResult<T>) -> SubjectResult<T> {
        let active = self.active.lock().map_err(|_| SubjectError::Unavailable)?;
        let mut store = active
            .as_ref()
            .ok_or(SubjectError::Unavailable)?
            .lock()
            .map_err(|_| SubjectError::Unavailable)?;
        if store.context.is_none() {
            return Err(SubjectError::Unavailable);
        }
        let mut next = store.ledger.clone();
        let result = f(&mut next)?;
        validate_ledger(&next)?;
        let bytes = serde_json::to_vec(&next).map_err(|_| SubjectError::InvalidInput)?;
        // 为所有活动事件的恢复状态预留固定空间，保证故障恢复不会因账本正好写满而失败。
        if bytes.len().saturating_add(next.events.len() * 128) > MAX_STATE_BYTES {
            return Err(SubjectError::LimitReached);
        }
        if store
            .context
            .as_ref()
            .ok_or(SubjectError::Unavailable)?
            .state_set(SUBJECT_STATE_KEY, bytes)
            .is_err()
        {
            store.context = None;
            return Err(SubjectError::Storage);
        }
        store.ledger = next;
        Ok(result)
    }
}

pub struct SubjectPlugin {
    manifest: PluginManifest,
    controller: SubjectController,
}
impl SubjectPlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(SUBJECT_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: SubjectController::default(),
        })
    }
    pub fn controller(&self) -> SubjectController {
        self.controller.clone()
    }
}
impl Plugin for SubjectPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let mut ledger = match context.state_get(SUBJECT_STATE_KEY)? {
                None => Ledger::default(),
                Some(bytes) => {
                    if bytes.len() > MAX_STATE_BYTES {
                        return Err(PluginError::State(SubjectError::CorruptState.to_string()));
                    }
                    let value = strict_json::from_slice(&bytes)
                        .map_err(|_| PluginError::State(SubjectError::CorruptState.to_string()))?;
                    if value.get("format_version").and_then(|v| v.as_u64()) != Some(1) {
                        return Err(PluginError::State(
                            SubjectError::UnsupportedVersion.to_string(),
                        ));
                    }
                    let ledger: Ledger = serde_json::from_value(value)
                        .map_err(|_| PluginError::State(SubjectError::CorruptState.to_string()))?;
                    validate_ledger(&ledger)
                        .map_err(|_| PluginError::State(SubjectError::CorruptState.to_string()))?;
                    ledger
                }
            };
            // 新进程不能沿用旧租约；只有安全边界可以有限次恢复。
            let now = clock_ms();
            let recovered = recover(&mut ledger, now, true);
            if recovered {
                validate_ledger(&ledger).map_err(|e| PluginError::State(e.to_string()))?;
                let bytes =
                    serde_json::to_vec(&ledger).map_err(|e| PluginError::State(e.to_string()))?;
                context.state_set(SUBJECT_STATE_KEY, bytes)?;
            }
            let store = Arc::new(Mutex::new(Store {
                ledger,
                context: Some(context.clone()),
            }));
            let close = store.clone();
            let active = self.controller.active.clone();
            context.cleanup(cleanup(move || async move {
                let mut handle = active
                    .lock()
                    .map_err(|_| PluginError::State("主体句柄不可用".into()))?;
                close
                    .lock()
                    .map_err(|_| PluginError::State("主体账本不可用".into()))?
                    .context = None;
                if handle
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &close))
                {
                    *handle = None;
                }
                Ok(())
            }))?;
            *self
                .controller
                .active
                .lock()
                .map_err(|_| PluginError::State("主体句柄不可用".into()))? = Some(store);
            Ok(None)
        })
    }
}

impl SubjectAdmin for SubjectController {
    fn enqueue(&self, input: SubjectEventInput, now_ms: u64) -> SubjectResult<SubjectEvent> {
        validate_input(&input)?;
        check_time(now_ms)?;
        self.update(|ledger| insert_event(ledger, input, now_ms, false))
    }
    fn observe_completed(
        &self,
        input: SubjectEventInput,
        now_ms: u64,
    ) -> SubjectResult<SubjectEvent> {
        validate_input(&input)?;
        check_time(now_ms)?;
        self.update(|ledger| insert_event(ledger, input, now_ms, true))
    }
    fn claim_next(
        &self,
        worker: &str,
        now_ms: u64,
        lease_ms: u64,
    ) -> SubjectResult<Option<SubjectEvent>> {
        self.claim_filtered(worker, &SubjectClaim::Any, now_ms, lease_ms)
    }
    fn claim_filtered(
        &self,
        worker: &str,
        filter: &SubjectClaim,
        now_ms: u64,
        lease_ms: u64,
    ) -> SubjectResult<Option<SubjectEvent>> {
        validate_id(worker)?;
        validate_lease_time(now_ms, lease_ms)?;
        self.update(|ledger| {
            recover(ledger, now_ms, false);
            if ledger
                .events
                .iter()
                .any(|e| e.status == EventStatus::Running)
            {
                return Ok(None);
            }
            let index = ledger
                .events
                .iter()
                .enumerate()
                .filter(|(_, e)| e.status == EventStatus::Queued)
                .min_by_key(|(_, e)| (e.input.priority, e.created_at_ms, e.id.clone()))
                .map(|(i, _)| i);
            let Some(index) = index else {
                return Ok(None);
            };
            let selected = &ledger.events[index];
            if match filter {
                SubjectClaim::Any => false,
                SubjectClaim::Event(id) => &selected.id != id,
                SubjectClaim::ExceptKinds(kinds) => kinds.contains(&selected.input.kind),
            } {
                return Ok(None);
            }
            ledger.generation = ledger
                .generation
                .checked_add(1)
                .ok_or(SubjectError::LimitReached)?;
            let event = &mut ledger.events[index];
            if now_ms < event.updated_at_ms {
                return Err(SubjectError::InvalidInput);
            }
            event.status = EventStatus::Running;
            event.updated_at_ms = now_ms;
            event.interrupt_requested = false;
            event.lease = Some(SubjectLease {
                token: LeaseToken {
                    event_id: event.id.clone(),
                    generation: ledger.generation,
                    worker: worker.into(),
                },
                expires_at_ms: now_ms
                    .checked_add(lease_ms)
                    .ok_or(SubjectError::InvalidInput)?,
            });
            Ok(Some(event.clone()))
        })
    }
    fn checkpoint(
        &self,
        token: &LeaseToken,
        checkpoint: SubjectCheckpoint,
        now_ms: u64,
    ) -> SubjectResult<SubjectEvent> {
        check_time(now_ms)?;
        if checkpoint.resume_data.len() > MAX_CHECKPOINT_BYTES {
            return Err(SubjectError::InvalidInput);
        }
        self.update(|ledger| {
            let event = leased(ledger, token, now_ms)?;
            event.checkpoint = checkpoint;
            event.updated_at_ms = now_ms;
            Ok(event.clone())
        })
    }
    fn renew(&self, token: &LeaseToken, now_ms: u64, lease_ms: u64) -> SubjectResult<SubjectEvent> {
        validate_lease_time(now_ms, lease_ms)?;
        self.update(|ledger| {
            let event = leased(ledger, token, now_ms)?;
            if event.interrupt_requested || event.cancel_requested {
                return Err(SubjectError::InvalidTransition);
            }
            event
                .lease
                .as_mut()
                .ok_or(SubjectError::LeaseLost)?
                .expires_at_ms = now_ms
                .checked_add(lease_ms)
                .ok_or(SubjectError::InvalidInput)?;
            event.updated_at_ms = now_ms;
            Ok(event.clone())
        })
    }
    fn yield_work(&self, token: &LeaseToken, now_ms: u64) -> SubjectResult<SubjectEvent> {
        check_time(now_ms)?;
        self.update(|ledger| {
            let event = leased(ledger, token, now_ms)?;
            if event.checkpoint.boundary != CheckpointBoundary::Safe {
                return Err(SubjectError::InvalidTransition);
            }
            event.status = if event.cancel_requested {
                EventStatus::Cancelled
            } else {
                EventStatus::Queued
            };
            event.lease = None;
            event.updated_at_ms = now_ms;
            Ok(event.clone())
        })
    }
    fn settle(
        &self,
        token: &LeaseToken,
        outcome: SubjectOutcome,
        now_ms: u64,
    ) -> SubjectResult<SubjectEvent> {
        check_time(now_ms)?;
        self.update(|ledger| {
            let event = leased(ledger, token, now_ms)?;
            if event.checkpoint.boundary == CheckpointBoundary::EffectPending {
                return Err(SubjectError::InvalidTransition);
            }
            apply_outcome(event, outcome, now_ms)?;
            Ok(event.clone())
        })
    }
    fn cancel_scope(&self, scope: &SubjectScope, now_ms: u64) -> SubjectResult<usize> {
        validate_scope(scope)?;
        check_time(now_ms)?;
        self.update(|ledger| {
            recover(ledger, now_ms, false);
            let mut count = 0;
            for event in &mut ledger.events {
                if &event.input.scope != scope || event.status.terminal() {
                    continue;
                }
                if now_ms < event.updated_at_ms {
                    return Err(SubjectError::InvalidInput);
                }
                count += 1;
                if event.status == EventStatus::Queued {
                    event.status = EventStatus::Cancelled;
                }
                event.interrupt_requested = true;
                event.cancel_requested = true;
                event.updated_at_ms = now_ms;
            }
            for focus in &mut ledger.foci {
                if &focus.scope == scope
                    && matches!(focus.status, FocusStatus::Active | FocusStatus::Blocked)
                {
                    focus.status = FocusStatus::Cancelled;
                    focus.updated_at_ms = now_ms.max(focus.updated_at_ms);
                }
            }
            Ok(count)
        })
    }
    fn abandon_pending_kind(&self, kind: &str, now_ms: u64) -> SubjectResult<usize> {
        validate_id(kind)?;
        check_time(now_ms)?;
        self.update(|ledger| {
            let mut count = 0;
            for event in &mut ledger.events {
                if event.input.kind == kind && event.status == EventStatus::Queued {
                    // 短准入已确认受理但尚未来得及settle时，只结算回执，不取消已执行工作。
                    event.status = match event.checkpoint.resume_data.as_str() {
                        "admission_started" => EventStatus::Completed,
                        "admission_rejected" => EventStatus::Failed,
                        "" => EventStatus::Cancelled,
                        _ => EventStatus::Unknown,
                    };
                    event.cancel_requested = event.status == EventStatus::Cancelled;
                    event.updated_at_ms = now_ms.max(event.updated_at_ms);
                    count += 1;
                }
            }
            Ok(count)
        })
    }
    fn rearm_busy_admission(
        &self,
        scope: &SubjectScope,
        id: &str,
        now_ms: u64,
    ) -> SubjectResult<SubjectEvent> {
        validate_scope(scope)?;
        validate_id(id)?;
        check_time(now_ms)?;
        self.update(|ledger| {
            let event = ledger
                .events
                .iter_mut()
                .find(|e| &e.input.scope == scope && e.id == id)
                .ok_or(SubjectError::NotFound)?;
            if event.status != EventStatus::Failed
                || event.input.kind != "work_admission"
                || event.outcome.as_deref() != Some("admission_busy")
                || event.checkpoint.boundary != CheckpointBoundary::Safe
                || event.cancel_requested
            {
                return Err(SubjectError::InvalidTransition);
            }
            event.status = EventStatus::Queued;
            event.outcome = None;
            event.updated_at_ms = now_ms.max(event.updated_at_ms);
            Ok(event.clone())
        })
    }
    fn cancel_queued_event(
        &self,
        scope: &SubjectScope,
        id: &str,
        now_ms: u64,
    ) -> SubjectResult<()> {
        validate_scope(scope)?;
        validate_id(id)?;
        check_time(now_ms)?;
        self.update(|ledger| {
            let event = ledger
                .events
                .iter_mut()
                .find(|e| &e.input.scope == scope && e.id == id)
                .ok_or(SubjectError::NotFound)?;
            if event.status == EventStatus::Queued {
                event.status = EventStatus::Cancelled;
                event.cancel_requested = true;
                event.updated_at_ms = now_ms.max(event.updated_at_ms);
            }
            Ok(())
        })
    }
    fn mark_unknown(
        &self,
        token: &LeaseToken,
        reason: &str,
        now_ms: u64,
    ) -> SubjectResult<SubjectEvent> {
        check_time(now_ms)?;
        if reason.is_empty() || reason.len() > MAX_PAYLOAD_BYTES {
            return Err(SubjectError::InvalidInput);
        }
        self.update(|ledger| {
            let event = leased(ledger, token, now_ms)?;
            if event.checkpoint.boundary != CheckpointBoundary::EffectPending {
                return Err(SubjectError::InvalidTransition);
            }
            event.status = EventStatus::Unknown;
            event.outcome = Some(reason.into());
            event.lease = None;
            event.updated_at_ms = now_ms;
            Ok(event.clone())
        })
    }
    fn snapshot_for(&self, scope: &SubjectScope) -> SubjectResult<SubjectSnapshot> {
        validate_scope(scope)?;
        self.read(|ledger| {
            Ok(SubjectSnapshot {
                events: ledger
                    .events
                    .iter()
                    .filter(|e| &e.input.scope == scope)
                    .cloned()
                    .collect(),
                foci: ledger
                    .foci
                    .iter()
                    .filter(|f| &f.scope == scope)
                    .cloned()
                    .collect(),
            })
        })
    }
    fn upsert_focus(&self, mut focus: SubjectFocus, now_ms: u64) -> SubjectResult<SubjectFocus> {
        check_time(now_ms)?;
        focus.updated_at_ms = now_ms;
        validate_focus(&focus)?;
        // 新建/修改关注都不能绕过半小时检查间隔。
        focus.next_check_at_ms = focus.next_check_at_ms.max(
            now_ms
                .checked_add(AUTONOMY_INTERVAL_MS)
                .ok_or(SubjectError::InvalidInput)?,
        );
        self.update(|ledger| {
            if let Some(existing) = ledger
                .foci
                .iter_mut()
                .find(|f| f.id == focus.id && f.scope == focus.scope)
            {
                if now_ms < existing.updated_at_ms {
                    return Err(SubjectError::InvalidInput);
                }
                *existing = focus.clone();
            } else {
                if ledger.foci.len() >= MAX_FOCI {
                    let oldest = ledger
                        .foci
                        .iter()
                        .enumerate()
                        .filter(|(_, f)| {
                            matches!(f.status, FocusStatus::Completed | FocusStatus::Cancelled)
                        })
                        .min_by_key(|(_, f)| f.updated_at_ms)
                        .map(|(i, _)| i)
                        .ok_or(SubjectError::LimitReached)?;
                    ledger.foci.remove(oldest);
                }
                ledger.foci.push(focus.clone());
            }
            Ok(focus)
        })
    }
    fn schedule_due(&self, now_ms: u64) -> SubjectResult<usize> {
        check_time(now_ms)?;
        self.update(|ledger| {
            let due: Vec<_> = ledger
                .foci
                .iter()
                .filter(|focus| {
                    focus.status == FocusStatus::Active && focus.next_check_at_ms <= now_ms
                })
                .cloned()
                .collect();
            let mut count = 0;
            for focus in due {
                // 未处理的检查不堆积；只更新下一次检查时间。检查不是通知。
                let pending = ledger.events.iter().any(|event| {
                    event.input.scope == focus.scope
                        && event.input.kind == "focus_check"
                        && event.input.payload == focus.id
                        && !event.status.terminal()
                });
                if !pending {
                    let key = format!("focus:{}:{}", focus.id, focus.next_check_at_ms);
                    insert_event(
                        ledger,
                        SubjectEventInput {
                            scope: focus.scope.clone(),
                            source: EventSource::AutonomyScheduler,
                            dedupe_key: key,
                            priority: EventPriority::Autonomy,
                            kind: "focus_check".into(),
                            payload: focus.id.clone(),
                        },
                        now_ms,
                        false,
                    )?;
                    count += 1;
                }
                let stored = ledger
                    .foci
                    .iter_mut()
                    .find(|f| f.id == focus.id && f.scope == focus.scope)
                    .ok_or(SubjectError::NotFound)?;
                stored.next_check_at_ms = now_ms
                    .checked_add(AUTONOMY_INTERVAL_MS)
                    .ok_or(SubjectError::InvalidInput)?;
                stored.updated_at_ms = now_ms;
            }
            Ok(count)
        })
    }
    fn resolve_unknown(
        &self,
        scope: &SubjectScope,
        id: &str,
        outcome: SubjectOutcome,
        now_ms: u64,
    ) -> SubjectResult<SubjectEvent> {
        validate_scope(scope)?;
        validate_id(id)?;
        check_time(now_ms)?;
        self.update(|ledger| {
            let event = ledger
                .events
                .iter_mut()
                .find(|e| e.id == id && &e.input.scope == scope)
                .ok_or(SubjectError::NotFound)?;
            if event.status != EventStatus::Unknown {
                return Err(SubjectError::InvalidTransition);
            }
            if now_ms < event.updated_at_ms {
                return Err(SubjectError::InvalidInput);
            }
            apply_outcome(event, outcome, now_ms)?;
            Ok(event.clone())
        })
    }
}

fn insert_event(
    ledger: &mut Ledger,
    input: SubjectEventInput,
    now_ms: u64,
    completed: bool,
) -> SubjectResult<SubjectEvent> {
    validate_input(&input)?;
    if let Some(existing) = ledger.events.iter().find(|e| {
        e.input.scope == input.scope
            && e.input.source == input.source
            && e.input.dedupe_key == input.dedupe_key
    }) {
        return Ok(existing.clone());
    }
    let retained: BTreeSet<_> = ledger.events.iter().map(|e| e.id.clone()).collect();
    ledger.dedupe.retain(|entry| {
        retained.contains(&entry.event_id) || now_ms.saturating_sub(entry.at_ms) < DEDUPE_WINDOW_MS
    });
    // 回收记录后仍保持去重；明确拒绝重建已处理的事件。
    if ledger
        .dedupe
        .iter()
        .any(|d| d.scope == input.scope && d.source == input.source && d.key == input.dedupe_key)
    {
        return Err(SubjectError::InvalidTransition);
    }
    if ledger.dedupe.len() >= MAX_DEDUPE_KEYS {
        return Err(SubjectError::LimitReached);
    }
    if ledger.events.len() >= MAX_EVENTS {
        let index = ledger
            .events
            .iter()
            .enumerate()
            .filter(|(_, e)| e.status.terminal() && e.status != EventStatus::Unknown)
            .min_by_key(|(_, e)| (e.updated_at_ms, e.id.clone()))
            .map(|(i, _)| i)
            .ok_or(SubjectError::LimitReached)?;
        ledger.events.remove(index);
    }
    ledger.next = ledger
        .next
        .checked_add(1)
        .ok_or(SubjectError::LimitReached)?;
    let event = SubjectEvent {
        id: format!("subject-{}", ledger.next),
        input,
        status: if completed {
            EventStatus::Completed
        } else {
            EventStatus::Queued
        },
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
        lease: None,
        checkpoint: SubjectCheckpoint::default(),
        recoveries: 0,
        interrupt_requested: false,
        cancel_requested: false,
        outcome: None,
    };
    if !completed {
        for running in &mut ledger.events {
            if running.status == EventStatus::Running
                && event.input.priority < running.input.priority
            {
                running.interrupt_requested = true;
            }
        }
    }
    ledger.dedupe.push(DedupeEntry {
        scope: event.input.scope.clone(),
        source: event.input.source,
        key: event.input.dedupe_key.clone(),
        event_id: event.id.clone(),
        at_ms: now_ms,
    });
    ledger.events.push(event.clone());
    Ok(event)
}

fn leased<'a>(
    ledger: &'a mut Ledger,
    token: &LeaseToken,
    now_ms: u64,
) -> SubjectResult<&'a mut SubjectEvent> {
    let event = ledger
        .events
        .iter_mut()
        .find(|e| e.id == token.event_id)
        .ok_or(SubjectError::LeaseLost)?;
    if event.status != EventStatus::Running
        || !event
            .lease
            .as_ref()
            .is_some_and(|lease| lease.token == *token && now_ms < lease.expires_at_ms)
    {
        return Err(SubjectError::LeaseLost);
    }
    if now_ms < event.updated_at_ms {
        return Err(SubjectError::InvalidInput);
    }
    Ok(event)
}

fn apply_outcome(
    event: &mut SubjectEvent,
    outcome: SubjectOutcome,
    now_ms: u64,
) -> SubjectResult<()> {
    let (status, text) = match outcome {
        SubjectOutcome::Completed(text) => (EventStatus::Completed, Some(text)),
        SubjectOutcome::Failed(text) => (EventStatus::Failed, Some(text)),
        SubjectOutcome::Cancelled => (EventStatus::Cancelled, None),
    };
    if text.as_ref().is_some_and(|s| s.len() > MAX_PAYLOAD_BYTES) {
        return Err(SubjectError::InvalidInput);
    }
    event.status = status;
    event.outcome = text;
    event.lease = None;
    event.updated_at_ms = now_ms;
    Ok(())
}

fn recover(ledger: &mut Ledger, now_ms: u64, restart: bool) -> bool {
    let mut changed = false;
    for event in &mut ledger.events {
        if event.status != EventStatus::Running
            || !event
                .lease
                .as_ref()
                .is_some_and(|lease| restart || lease.expires_at_ms <= now_ms)
        {
            continue;
        }
        changed = true;
        event.updated_at_ms = now_ms.max(event.updated_at_ms);
        event.lease = None;
        if event.checkpoint.boundary == CheckpointBoundary::EffectPending {
            event.status = EventStatus::Unknown;
            event.outcome = Some("执行结果未知；须人工核对，不自动重放".into());
        } else if event.cancel_requested {
            event.status = EventStatus::Cancelled;
        } else if event.recoveries < MAX_RECOVERIES {
            event.recoveries += 1;
            event.status = EventStatus::Queued;
        } else {
            event.status = EventStatus::Failed;
            event.outcome = Some("安全恢复次数已用尽；检查点保留".into());
        }
    }
    changed
}

fn check_time(now_ms: u64) -> SubjectResult<()> {
    if now_ms == 0 {
        Err(SubjectError::InvalidInput)
    } else {
        Ok(())
    }
}
fn validate_lease_time(now_ms: u64, lease_ms: u64) -> SubjectResult<()> {
    check_time(now_ms)?;
    if !(1..=MAX_LEASE_MS).contains(&lease_ms) || now_ms.checked_add(lease_ms).is_none() {
        return Err(SubjectError::InvalidInput);
    }
    Ok(())
}
fn clock_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(1)
        .max(1)
}

fn validate_ledger(ledger: &Ledger) -> SubjectResult<()> {
    if ledger.format_version != 1 {
        return Err(SubjectError::UnsupportedVersion);
    }
    if ledger.events.len() > MAX_EVENTS
        || ledger.foci.len() > MAX_FOCI
        || ledger.dedupe.len() > MAX_DEDUPE_KEYS
    {
        return Err(SubjectError::CorruptState);
    }
    let mut ids = BTreeSet::new();
    let mut keys = BTreeSet::new();
    let mut running = 0;
    for event in &ledger.events {
        validate_input(&event.input)?;
        let number = event
            .id
            .strip_prefix("subject-")
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or(SubjectError::CorruptState)?;
        if number == 0
            || event.id != format!("subject-{number}")
            || number > ledger.next
            || !ids.insert(event.id.clone())
            || event.created_at_ms == 0
            || event.updated_at_ms < event.created_at_ms
            || event.recoveries > MAX_RECOVERIES
            || event.checkpoint.resume_data.len() > MAX_CHECKPOINT_BYTES
            || event
                .outcome
                .as_ref()
                .is_some_and(|s| s.len() > MAX_PAYLOAD_BYTES)
        {
            return Err(SubjectError::CorruptState);
        }
        if event.status == EventStatus::Running {
            running += 1;
            let lease = event.lease.as_ref().ok_or(SubjectError::CorruptState)?;
            validate_id(&lease.token.worker)?;
            if lease.token.event_id != event.id
                || lease.token.generation == 0
                || lease.token.generation > ledger.generation
                || lease.expires_at_ms <= event.updated_at_ms
            {
                return Err(SubjectError::CorruptState);
            }
        } else if event.lease.is_some() {
            return Err(SubjectError::CorruptState);
        }
        if event.status == EventStatus::Queued
            && event.checkpoint.boundary != CheckpointBoundary::Safe
        {
            return Err(SubjectError::CorruptState);
        }
        if event.status == EventStatus::Unknown
            && event.checkpoint.boundary != CheckpointBoundary::EffectPending
        {
            return Err(SubjectError::CorruptState);
        }
        if !ledger.dedupe.iter().any(|d| {
            d.scope == event.input.scope
                && d.source == event.input.source
                && d.key == event.input.dedupe_key
                && d.event_id == event.id
                && d.at_ms == event.created_at_ms
        }) {
            return Err(SubjectError::CorruptState);
        }
    }
    if running > 1 {
        return Err(SubjectError::CorruptState);
    }
    for entry in &ledger.dedupe {
        validate_scope(&entry.scope)?;
        validate_id(&entry.key)?;
        let number = entry
            .event_id
            .strip_prefix("subject-")
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or(SubjectError::CorruptState)?;
        if entry.at_ms == 0
            || number == 0
            || entry.event_id != format!("subject-{number}")
            || number > ledger.next
            || !keys.insert((entry.scope.clone(), entry.source, entry.key.clone()))
        {
            return Err(SubjectError::CorruptState);
        }
        if let Some(event) = ledger
            .events
            .iter()
            .find(|event| event.id == entry.event_id)
            && (entry.scope != event.input.scope
                || entry.source != event.input.source
                || entry.key != event.input.dedupe_key
                || entry.at_ms != event.created_at_ms)
        {
            return Err(SubjectError::CorruptState);
        }
    }
    let mut foci = BTreeSet::new();
    for focus in &ledger.foci {
        validate_focus(focus)?;
        if !foci.insert((focus.scope.clone(), focus.id.clone())) || focus.next_check_at_ms == 0 {
            return Err(SubjectError::CorruptState);
        }
    }
    Ok(())
}
