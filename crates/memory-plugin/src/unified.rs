use crate::strict_json;
use eve_memory_api::*;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

const STATE_KEY: &str = "unified.v1";
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    format_version: u32,
    state: UnifiedMemorySnapshot,
}
struct Inner {
    document: Document,
    context: Option<PluginContext>,
}
struct Stored {
    inner: Mutex<Inner>,
}

fn validate_binding(b: &UnifiedMemoryBinding) -> MemoryResult<()> {
    for id in [&b.id, &b.principal, &b.authorization_reference] {
        validate_id(id)?;
    }
    b.source.validate()?;
    b.target.validate()?;
    if b.revision == 0 || b.source.channel == b.target.channel {
        return Err(MemoryError::InvalidInput);
    }
    Ok(())
}
fn validate_holdout(h: &RecallHoldout) -> MemoryResult<()> {
    validate_id(&h.id)?;
    h.target.validate()?;
    MemoryRecallRequest {
        query: h.query.clone(),
        limit: 1,
    }
    .validate()?;
    if h.sources.len() > MAX_CONTINUITY_SOURCES || h.expected.len() > MAX_CONTINUITY_HITS {
        return Err(MemoryError::LimitReached);
    }
    let mut sources = BTreeSet::new();
    for source in &h.sources {
        source.scope.validate()?;
        if source.scope == h.target || !sources.insert(source.scope.clone()) {
            return Err(MemoryError::InvalidInput);
        }
    }
    let mut expected = BTreeSet::new();
    for e in &h.expected {
        validate_id(&e.evidence_id)?;
        if !sources.contains(&e.scope) || !expected.insert(e.clone()) {
            return Err(MemoryError::InvalidInput);
        }
    }
    Ok(())
}
fn validate(s: &UnifiedMemorySnapshot) -> MemoryResult<()> {
    if s.bindings.len() + s.holdouts.len() + s.evaluations.len() > MAX_UNIFIED_RECORDS
        || s.binding_revision > s.revision
    {
        return Err(MemoryError::LimitReached);
    }
    let total = s.bindings.len() + s.holdouts.len() + s.evaluations.len();
    if s.binding_revision != s.bindings.len() as u64 || s.revision != total as u64 {
        return Err(MemoryError::CorruptState);
    }
    // 每个仍有效授权预留一条撤销记录，容量满也必须能撤销。
    let live = s
        .bindings
        .iter()
        .filter(|binding| {
            !binding.revoked && !s.bindings.iter().any(|b| b.id == binding.id && b.revoked)
        })
        .count();
    if total + live > MAX_UNIFIED_RECORDS {
        return Err(MemoryError::LimitReached);
    }
    let mut revisions = BTreeSet::new();
    let mut bindings = BTreeSet::new();
    for b in &s.bindings {
        validate_binding(b)?;
        if b.revision > s.binding_revision
            || !bindings.insert((&b.id, b.revision))
            || !revisions.insert(b.revision)
        {
            return Err(MemoryError::CorruptState);
        }
    }
    // 每个授权ID只可绑定一次，再撤销一次；不能借重绑定静默改换身份。
    for id in s.bindings.iter().map(|b| &b.id).collect::<BTreeSet<_>>() {
        let history: Vec<_> = s.bindings.iter().filter(|b| &b.id == id).collect();
        if history.len() > 2 || history[0].revoked {
            return Err(MemoryError::CorruptState);
        }
        if history.len() == 2 {
            let mut expected = (*history[0]).clone();
            expected.revoked = true;
            expected.revision = history[1].revision;
            if !history[1].revoked
                || history[0].revision >= history[1].revision
                || &expected != history[1]
            {
                return Err(MemoryError::CorruptState);
            }
        }
    }
    let mut principals = std::collections::BTreeMap::new();
    for b in &s.bindings {
        for scope in [&b.source, &b.target] {
            if principals
                .insert(scope, &b.principal)
                .is_some_and(|p| p != &b.principal)
            {
                return Err(MemoryError::Conflict);
            }
        }
    }
    let mut holds = BTreeSet::new();
    for h in &s.holdouts {
        validate_holdout(h)?;
        if h.binding_revision > s.binding_revision || !holds.insert(&h.id) {
            return Err(MemoryError::CorruptState);
        }
    }
    let mut runs = BTreeSet::new();
    for e in &s.evaluations {
        validate_id(&e.implementation_version)?;
        let holdout = s
            .holdouts
            .iter()
            .find(|h| h.id == e.holdout_id)
            .ok_or(MemoryError::CorruptState)?;
        if !runs.insert((&e.holdout_id, &e.implementation_version))
            || e.expected != holdout.expected.len()
            || e.relevant > e.expected
            || e.relevant > e.retrieved
            || e.retrieved > MAX_CONTINUITY_HITS
        {
            return Err(MemoryError::CorruptState);
        }
        if e.status == RecallEvaluationStatus::Measured {
            if e.reason != "fixed_holdout"
                || (e.precision_bps, e.recall_bps) != metrics(e.relevant, e.retrieved, e.expected)
            {
                return Err(MemoryError::CorruptState);
            }
        } else if e.precision_bps != 0
            || e.recall_bps != 0
            || ![
                "training_overlap",
                "scope_changed",
                "recall_unavailable",
                "invalid_response",
                "invalid_holdout",
            ]
            .contains(&e.reason.as_str())
        {
            return Err(MemoryError::CorruptState);
        }
    }
    Ok(())
}
fn metrics(relevant: usize, retrieved: usize, expected: usize) -> (u32, u32) {
    let precision = if retrieved == 0 {
        if expected == 0 { 10000 } else { 0 }
    } else {
        relevant * 10000 / retrieved
    };
    let recall = if expected == 0 {
        if retrieved == 0 { 10000 } else { 0 }
    } else {
        relevant * 10000 / expected
    };
    (precision as u32, recall as u32)
}
impl Stored {
    fn snapshot(&self) -> MemoryResult<UnifiedMemorySnapshot> {
        let inner = self.inner.lock().map_err(|_| MemoryError::Unavailable)?;
        if inner.context.is_none() {
            return Err(MemoryError::Unavailable);
        }
        Ok(inner.document.state.clone())
    }
    fn change(
        &self,
        revision: u64,
        f: impl FnOnce(&mut UnifiedMemorySnapshot) -> MemoryResult<()>,
    ) -> MemoryResult<()> {
        let mut inner = self.inner.lock().map_err(|_| MemoryError::Unavailable)?;
        let context = inner.context.clone().ok_or(MemoryError::Unavailable)?;
        if inner.document.state.revision != revision {
            return Err(MemoryError::StaleRevision);
        }
        let mut next = inner.document.clone();
        f(&mut next.state)?;
        if next.state == inner.document.state {
            return Ok(());
        }
        next.state.revision = revision.checked_add(1).ok_or(MemoryError::LimitReached)?;
        validate(&next.state)?;
        let bytes = serde_json::to_vec(&next).map_err(|_| MemoryError::Storage)?;
        validate_bytes(&next.state, bytes.len())?;
        if context.state_set(STATE_KEY, bytes).is_err() {
            inner.context = None;
            return Err(MemoryError::Storage);
        }
        inner.document = next;
        Ok(())
    }
}
#[derive(Clone, Default)]
pub struct UnifiedMemoryController {
    active: Arc<Mutex<Option<Arc<Stored>>>>,
}
impl UnifiedMemoryController {
    fn stored(&self) -> MemoryResult<Arc<Stored>> {
        self.active
            .lock()
            .map_err(|_| MemoryError::Unavailable)?
            .clone()
            .ok_or(MemoryError::Unavailable)
    }
}
impl MemoryContinuityPolicy for UnifiedMemoryController {
    fn grants(
        &self,
        target: &MemoryScope,
        candidates: &[MemoryScope],
    ) -> MemoryResult<Vec<MemoryScopeGrant>> {
        target.validate()?;
        let state = self.snapshot()?;
        let mut grants = Vec::<MemoryScopeGrant>::new();
        for binding in &state.bindings {
            if binding.revoked
                || &binding.target != target
                || !candidates.contains(&binding.source)
                || state
                    .bindings
                    .iter()
                    .any(|b| b.id == binding.id && b.revoked)
            {
                continue;
            }
            if let Some(grant) = grants.iter_mut().find(|g| g.scope == binding.source) {
                grant.interactions |= binding.interactions;
            } else {
                grants.push(MemoryScopeGrant {
                    scope: binding.source.clone(),
                    interactions: binding.interactions,
                });
            }
        }
        grants.sort_by(|a, b| a.scope.cmp(&b.scope));
        Ok(grants)
    }
}
impl UnifiedMemoryAdmin for UnifiedMemoryController {
    fn snapshot(&self) -> MemoryResult<UnifiedMemorySnapshot> {
        self.stored()?.snapshot()
    }
    fn bind(&self, expected_revision: u64, binding: UnifiedMemoryBinding) -> MemoryResult<()> {
        validate_binding(&binding)?;
        if binding.revoked {
            return Err(MemoryError::InvalidInput);
        }
        self.stored()?.change(expected_revision, |s| {
            if let Some(existing) = s.bindings.iter().find(|b| b.id == binding.id) {
                return if existing == &binding {
                    Ok(())
                } else {
                    Err(MemoryError::Conflict)
                };
            }
            let next = s
                .binding_revision
                .checked_add(1)
                .ok_or(MemoryError::LimitReached)?;
            if binding.revision != next {
                return Err(MemoryError::StaleRevision);
            }
            s.binding_revision = next;
            s.bindings.push(binding);
            Ok(())
        })
    }
    fn revoke(&self, expected_revision: u64, binding_id: &str) -> MemoryResult<()> {
        validate_id(binding_id)?;
        self.stored()?.change(expected_revision, |s| {
            let mut b = s
                .bindings
                .iter()
                .rev()
                .find(|b| b.id == binding_id)
                .cloned()
                .ok_or(MemoryError::NotFound)?;
            if b.revoked {
                return Ok(());
            }
            b.revoked = true;
            b.revision = s
                .binding_revision
                .checked_add(1)
                .ok_or(MemoryError::LimitReached)?;
            s.binding_revision = b.revision;
            s.bindings.push(b);
            Ok(())
        })
    }
    fn register_holdout(&self, expected_revision: u64, holdout: RecallHoldout) -> MemoryResult<()> {
        validate_holdout(&holdout)?;
        self.stored()?.change(expected_revision, |s| {
            if let Some(existing) = s.holdouts.iter().find(|h| h.id == holdout.id) {
                return if existing == &holdout {
                    Ok(())
                } else {
                    Err(MemoryError::Conflict)
                };
            }
            if holdout.binding_revision != s.binding_revision {
                return Err(MemoryError::StaleRevision);
            }
            if s.holdouts.iter().any(|h| {
                h.target == holdout.target && normalized(&h.query) == normalized(&holdout.query)
            }) {
                return Err(MemoryError::Conflict);
            }
            s.holdouts.push(holdout);
            Ok(())
        })
    }
    fn evaluate(
        &self,
        holdout_id: &str,
        implementation_version: &str,
        training_queries: &[String],
        memory: &dyn MemoryAdmin,
        recall: &dyn MemoryContinuityFactory,
    ) -> MemoryResult<RecallEvaluation> {
        validate_id(implementation_version)?;
        if training_queries.len() > MAX_EVIDENCE
            || training_queries
                .iter()
                .any(|q| q.len() > MAX_RECALL_QUERY_BYTES)
        {
            return Err(MemoryError::LimitReached);
        }
        let state = self.snapshot()?;
        if let Some(old) = state.evaluations.iter().find(|e| {
            e.holdout_id == holdout_id && e.implementation_version == implementation_version
        }) {
            return Ok(old.clone());
        }
        let h = state
            .holdouts
            .iter()
            .find(|h| h.id == holdout_id)
            .ok_or(MemoryError::NotFound)?;
        let mut result = RecallEvaluation {
            holdout_id: h.id.clone(),
            implementation_version: implementation_version.into(),
            status: RecallEvaluationStatus::Inconclusive,
            relevant: 0,
            retrieved: 0,
            expected: h.expected.len(),
            precision_bps: 0,
            recall_bps: 0,
            reason: "scope_changed".into(),
        };
        let unchanged = || -> MemoryResult<bool> {
            if self.snapshot()?.binding_revision != h.binding_revision {
                return Ok(false);
            }
            let candidates = memory.scopes()?;
            let grants = self.grants(&h.target, &candidates)?;
            let actual: BTreeSet<_> = grants.iter().map(|g| &g.scope).collect();
            let expected: BTreeSet<_> = h.sources.iter().map(|s| &s.scope).collect();
            if actual != expected {
                return Ok(false);
            }
            for source in &h.sources {
                if memory.reader(source.scope.clone())?.snapshot()?.revision != source.revision {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        if training_queries
            .iter()
            .any(|q| normalized(q) == normalized(&h.query))
        {
            result.reason = "training_overlap".into();
        } else if unchanged()? && !expected_exists(h, memory)? {
            result.reason = "invalid_holdout".into();
        } else if unchanged()? {
            match recall
                .reader(h.target.clone())
                .and_then(|r| r.read(&h.query))
            {
                Err(_) => result.reason = "recall_unavailable".into(),
                Ok(view) => {
                    if view.validate_for(&h.target, &h.query).is_err()
                        || view.sources.iter().any(|s| {
                            !h.sources
                                .iter()
                                .any(|fixed| fixed.scope == s.scope && fixed.revision == s.revision)
                        })
                        || !authentic_hits(&view, memory)?
                    {
                        result.reason = "invalid_response".into();
                    } else if unchanged()? {
                        let actual: BTreeSet<_> = view
                            .sources
                            .iter()
                            .flat_map(|source| {
                                source.hits.iter().map(move |hit| {
                                    let evidence_id = match &hit.source {
                                        MemoryRecallSource::ConfirmedPreference {
                                            evidence_id,
                                            ..
                                        }
                                        | MemoryRecallSource::CompletedInteraction {
                                            evidence_id,
                                            ..
                                        } => evidence_id.clone(),
                                    };
                                    RecallExpectedEvidence {
                                        scope: source.scope.clone(),
                                        evidence_id,
                                    }
                                })
                            })
                            .collect();
                        result.retrieved = actual.len();
                        result.relevant = h.expected.iter().filter(|e| actual.contains(e)).count();
                        (result.precision_bps, result.recall_bps) =
                            metrics(result.relevant, result.retrieved, result.expected);
                        result.status = RecallEvaluationStatus::Measured;
                        result.reason = "fixed_holdout".into();
                    }
                }
            }
        }
        self.stored()?.change(state.revision, |s| {
            s.evaluations.push(result.clone());
            Ok(())
        })?;
        Ok(result)
    }
}
fn normalized(query: &str) -> String {
    query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub struct UnifiedMemoryPlugin {
    manifest: PluginManifest,
    controller: UnifiedMemoryController,
}
impl UnifiedMemoryPlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(UNIFIED_MEMORY_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: UnifiedMemoryController::default(),
        })
    }
    pub fn controller(&self) -> UnifiedMemoryController {
        self.controller.clone()
    }
}
impl Plugin for UnifiedMemoryPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let document = match context.state_get(STATE_KEY)? {
                None => Document {
                    format_version: 1,
                    state: UnifiedMemorySnapshot::default(),
                },
                Some(bytes) => {
                    if bytes.len() > MAX_STATE_BYTES {
                        return Err(PluginError::State(MemoryError::CorruptState.to_string()));
                    }
                    let value = strict_json::from_slice(&bytes)
                        .map_err(|_| PluginError::State(MemoryError::CorruptState.to_string()))?;
                    if value.get("format_version").and_then(|v| v.as_u64()) != Some(1) {
                        return Err(PluginError::State(
                            MemoryError::UnsupportedVersion.to_string(),
                        ));
                    }
                    let document: Document = serde_json::from_value(value)
                        .map_err(|_| PluginError::State(MemoryError::CorruptState.to_string()))?;
                    validate(&document.state)
                        .map_err(|_| PluginError::State(MemoryError::CorruptState.to_string()))?;
                    validate_bytes(&document.state, bytes.len())
                        .map_err(|_| PluginError::State(MemoryError::CorruptState.to_string()))?;
                    document
                }
            };
            let stored = Arc::new(Stored {
                inner: Mutex::new(Inner {
                    document,
                    context: Some(context.clone()),
                }),
            });
            let closing = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                closing
                    .inner
                    .lock()
                    .map_err(|_| PluginError::State("统一记忆状态锁不可用".into()))?
                    .context = None;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("统一记忆句柄不可用".into()))?;
                if active.as_ref().is_some_and(|s| Arc::ptr_eq(s, &closing)) {
                    *active = None;
                }
                Ok(())
            }))?;
            *self
                .controller
                .active
                .lock()
                .map_err(|_| PluginError::State("统一记忆句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}

fn expected_exists(holdout: &RecallHoldout, memory: &dyn MemoryAdmin) -> MemoryResult<bool> {
    for expected in &holdout.expected {
        let snapshot = memory.reader(expected.scope.clone())?.complete_snapshot()?;
        crate::recall::validate_snapshot(&snapshot, &expected.scope)?;
        if !snapshot
            .evidence
            .iter()
            .any(|e| e.id == expected.evidence_id)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

// 评价器独立核对可替换召回器的来源，不把格式正确当成证据真实。
fn authentic_hits(view: &MemoryContinuityView, memory: &dyn MemoryAdmin) -> MemoryResult<bool> {
    for source in &view.sources {
        let snapshot = memory.reader(source.scope.clone())?.complete_snapshot()?;
        crate::recall::validate_snapshot(&snapshot, &source.scope)?;
        if snapshot.revision != source.revision {
            return Ok(false);
        }
        for hit in &source.hits {
            let valid = match &hit.source {
                MemoryRecallSource::ConfirmedPreference {
                    preference_id,
                    preference_revision,
                    evidence_id,
                    evidence_revision,
                    at_ms,
                } => snapshot.preferences.iter().any(|p| {
                    p.id == *preference_id
                        && p.status == PreferenceStatus::Confirmed
                        && p.revision == *preference_revision
                        && authentic_excerpt(&p.text, hit)
                        && p.history
                            .last()
                            .is_some_and(|h| h.evidence_id == *evidence_id && h.at_ms == *at_ms)
                        && snapshot
                            .evidence
                            .iter()
                            .any(|e| e.id == *evidence_id && e.revision == *evidence_revision)
                }),
                MemoryRecallSource::CompletedInteraction {
                    evidence_id,
                    evidence_revision,
                    message_id,
                    session_revision,
                    turn_id,
                    at_ms,
                    field,
                } => snapshot.evidence.iter().any(|e| {
                    e.id == *evidence_id
                        && e.revision == *evidence_revision
                        && e.at_ms == *at_ms
                        && match &e.source {
                            EvidenceSource::CompletedInteraction {
                                message_id: actual_message,
                                session_revision: actual_revision,
                                turn_id: actual_turn,
                                user_text,
                                assistant_text,
                            } => {
                                actual_message == message_id
                                    && actual_revision == session_revision
                                    && actual_turn == turn_id
                                    && match field {
                                        RecallField::User => authentic_excerpt(user_text, hit),
                                        RecallField::Assistant => {
                                            authentic_excerpt(assistant_text, hit)
                                        }
                                    }
                            }
                            _ => false,
                        }
                }),
            };
            if !valid {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn validate_bytes(state: &UnifiedMemorySnapshot, encoded_bytes: usize) -> MemoryResult<()> {
    let revoke_reserve = state
        .bindings
        .iter()
        .filter(|b| {
            !b.revoked
                && !state
                    .bindings
                    .iter()
                    .any(|history| history.id == b.id && history.revoked)
        })
        .try_fold(0usize, |sum, binding| {
            // 撤销复用同一正文，只有修订长度可能增长；包含数组分隔符余量。
            serde_json::to_vec(binding)
                .map(|bytes| sum.saturating_add(bytes.len() + 32))
                .map_err(|_| MemoryError::Storage)
        })?;
    if encoded_bytes.saturating_add(revoke_reserve) > MAX_STATE_BYTES {
        return Err(MemoryError::LimitReached);
    }
    Ok(())
}

fn authentic_excerpt(text: &str, hit: &MemoryRecallHit) -> bool {
    if !hit.excerpt_truncated {
        return text == hit.excerpt;
    }
    let payload = hit.excerpt.strip_prefix('…').unwrap_or(&hit.excerpt);
    let payload = payload.strip_suffix('…').unwrap_or(payload);
    !payload.is_empty() && payload.len() < text.len() && text.contains(payload)
}
