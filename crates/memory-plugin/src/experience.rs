//! 持久化经验账本。任何写入确认失败即关闭句柄，重启后读取原状态，不猜测提交结果。
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

pub const EXPERIENCE_STATE_KEY: &str = "experience.v1";
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    format_version: u32,
    state: ExperienceSnapshot,
}
struct Stored {
    document: Document,
    context: Option<PluginContext>,
}
#[derive(Clone, Default)]
pub struct ExperienceMemoryController {
    active: Arc<Mutex<Option<Arc<Mutex<Stored>>>>>,
}

fn visible(v: &ExperienceVisibility, c: &ExperienceReadContext) -> bool {
    match v {
        ExperienceVisibility::Subject => true,
        ExperienceVisibility::Principal { principal } => c.principal.as_ref() == Some(principal),
        ExperienceVisibility::Scope { scope } => scope == &c.scope,
    }
}
fn validate_context(c: &ExperienceReadContext) -> MemoryResult<()> {
    c.scope.validate()?;
    if let Some(p) = &c.principal {
        validate_id(p)?;
    }
    Ok(())
}
fn validate_version(v: &ExperienceVersion) -> MemoryResult<()> {
    validate_id(&v.operation_id)?;
    validate_text(&v.text, MAX_EXPERIENCE_TEXT_BYTES)?;
    if v.revision == 0 || v.expires_at_ms == Some(0) {
        return Err(MemoryError::InvalidInput);
    }
    let (reference, revision) = match &v.source {
        ExperienceSource::PublicArtifact {
            reference,
            revision,
        } => (reference, revision),
        ExperienceSource::ScopedEvidence {
            scope,
            reference,
            revision,
        } => {
            scope.validate()?;
            (reference, revision)
        }
    };
    validate_id(reference)?;
    if *revision == 0 {
        return Err(MemoryError::InvalidInput);
    }
    Ok(())
}
fn validate(s: &ExperienceSnapshot) -> MemoryResult<()> {
    let count: usize = s.records.iter().map(|r| r.history.len()).sum();
    let live = s
        .records
        .iter()
        .filter(|r| {
            r.history.last().is_some_and(|v| {
                !matches!(
                    v.status,
                    ExperienceStatus::Revoked | ExperienceStatus::Archived
                )
            })
        })
        .count();
    if s.records.len() > MAX_EXPERIENCES || count + live > MAX_EXPERIENCE_VERSIONS {
        return Err(MemoryError::LimitReached);
    }
    if s.revision != count as u64 {
        return Err(MemoryError::CorruptState);
    }
    let mut ids = BTreeSet::new();
    let mut operations = BTreeSet::new();
    for r in &s.records {
        validate_id(&r.id)?;
        if !ids.insert(&r.id) || r.history.is_empty() {
            return Err(MemoryError::CorruptState);
        }
        if let ExperienceVisibility::Principal { principal } = &r.visibility {
            validate_id(principal)?;
        }
        if let ExperienceVisibility::Scope { scope } = &r.visibility {
            scope.validate()?;
        }
        for (index, v) in r.history.iter().enumerate() {
            validate_version(v)?;
            if index == 0
                && matches!(
                    v.status,
                    ExperienceStatus::Revoked | ExperienceStatus::Archived
                )
            {
                return Err(MemoryError::InvalidInput);
            }
            if v.revision != (index + 1) as u64 || !operations.insert(&v.operation_id) {
                return Err(MemoryError::CorruptState);
            }
            match (&r.visibility, &v.source) {
                (ExperienceVisibility::Subject, ExperienceSource::PublicArtifact { .. }) => (),
                (
                    ExperienceVisibility::Scope { scope },
                    ExperienceSource::ScopedEvidence { scope: source, .. },
                ) if scope == source => (),
                (
                    ExperienceVisibility::Principal { .. },
                    ExperienceSource::ScopedEvidence { .. },
                ) => (),
                _ => return Err(MemoryError::InvalidInput),
            }
            if index > 0 {
                let prior = &r.history[index - 1];
                if matches!(
                    prior.status,
                    ExperienceStatus::Revoked | ExperienceStatus::Archived
                ) || (v.status == ExperienceStatus::Verified && v.text != prior.text)
                    || (matches!(
                        v.status,
                        ExperienceStatus::Revoked | ExperienceStatus::Archived
                    ) && v.text != prior.text)
                {
                    return Err(MemoryError::Conflict);
                }
            }
        }
    }
    // 每个活动条目预留一条终结修订与JSON余量，容量满仍可撤销。
    let reserve: usize = s
        .records
        .iter()
        .filter_map(|r| r.history.last())
        .filter(|v| {
            !matches!(
                v.status,
                ExperienceStatus::Revoked | ExperienceStatus::Archived
            )
        })
        .map(|v| {
            serde_json::to_vec(v)
                // 新操作ID、来源引用与principal新scope均可增长到合法上限；
                // 额外4KiB覆盖这些字段的JSON转义及数字长度，正文保持不变。
                .map(|b| b.len() + 4096)
                .unwrap_or(MAX_STATE_BYTES)
        })
        .sum();
    if serde_json::to_vec(s)
        .map_err(|_| MemoryError::Storage)?
        .len()
        .saturating_add(reserve)
        .saturating_add(128)
        > MAX_STATE_BYTES
    {
        return Err(MemoryError::LimitReached);
    }
    Ok(())
}
impl ExperienceMemoryController {
    fn stored(&self) -> MemoryResult<Arc<Mutex<Stored>>> {
        self.active
            .lock()
            .map_err(|_| MemoryError::Unavailable)?
            .clone()
            .ok_or(MemoryError::Unavailable)
    }
    fn change(
        &self,
        expected: u64,
        id: &str,
        visibility: Option<ExperienceVisibility>,
        version: ExperienceVersion,
        allowed: impl FnOnce(&ExperienceRecord) -> bool,
    ) -> MemoryResult<()> {
        validate_id(id)?;
        validate_version(&version)?;
        let stored = self.stored()?;
        let mut inner = stored.lock().map_err(|_| MemoryError::Unavailable)?;
        let context = inner.context.clone().ok_or(MemoryError::Unavailable)?;
        // 重放仅同一id、可访问记录、完全相同内容才成功，优先于CAS。
        for r in &inner.document.state.records {
            if let Some(old) = r
                .history
                .iter()
                .find(|v| v.operation_id == version.operation_id)
            {
                return if r.id == id
                    && *old == version
                    && visibility.as_ref().is_none_or(|v| v == &r.visibility)
                    && allowed(r)
                {
                    Ok(())
                } else {
                    Err(MemoryError::Conflict)
                };
            }
        }
        if inner.document.state.revision != expected {
            return Err(MemoryError::StaleRevision);
        }
        let mut next = inner.document.clone();
        if let Some(r) = next.state.records.iter_mut().find(|r| r.id == id) {
            if visibility.is_some() {
                return Err(MemoryError::Conflict);
            }
            if !allowed(r) {
                return Err(MemoryError::NotFound);
            }
            r.history.push(version);
        } else {
            let visibility = visibility.ok_or(MemoryError::NotFound)?;
            next.state.records.push(ExperienceRecord {
                id: id.into(),
                visibility,
                history: vec![version],
            });
        }
        next.state.revision = expected.checked_add(1).ok_or(MemoryError::LimitReached)?;
        validate(&next.state)?;
        let bytes = serde_json::to_vec(&next).map_err(|_| MemoryError::Storage)?;
        if context.state_set(EXPERIENCE_STATE_KEY, bytes).is_err() {
            inner.context = None;
            return Err(MemoryError::Storage);
        }
        inner.document = next;
        Ok(())
    }
}
impl ExperienceRecall for ExperienceMemoryController {
    fn recall(
        &self,
        context: &ExperienceReadContext,
        query: &str,
        limit: usize,
    ) -> MemoryResult<Vec<ExperienceHit>> {
        validate_context(context)?;
        MemoryRecallRequest {
            query: query.into(),
            limit,
        }
        .validate()?;
        let state = self.snapshot()?;
        let query_terms = terms(query);
        let mut hits = Vec::new();
        for r in state.records {
            if !visible(&r.visibility, context) {
                continue;
            }
            let version = r.history.last().ok_or(MemoryError::CorruptState)?;
            if version.is_expired()
                || matches!(
                    version.status,
                    ExperienceStatus::Revoked | ExperienceStatus::Archived
                )
            {
                continue;
            }
            let indexed = terms(&version.text);
            let score = query_terms.intersection(&indexed).count();
            if score > 0 {
                hits.push(ExperienceHit {
                    id: r.id.clone(),
                    visibility: r.visibility.clone(),
                    version: version.clone(),
                    score,
                });
            }
        }
        hits.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| b.version.at_ms.cmp(&a.version.at_ms))
                .then_with(|| a.id.cmp(&b.id))
        });
        hits.truncate(limit);
        Ok(hits)
    }
}
fn terms(text: &str) -> BTreeSet<String> {
    fn flush(result: &mut BTreeSet<String>, word: &mut String, cjk: bool) {
        if word.is_empty() {
            return;
        }
        result.insert(word.clone());
        if cjk {
            let chars: Vec<_> = word.chars().collect();
            for pair in chars.windows(2) {
                result.insert(pair.iter().collect());
            }
        }
        word.clear();
    }
    let mut result = BTreeSet::new();
    let mut word = String::new();
    let mut prior_cjk = false;
    for ch in text.to_lowercase().chars() {
        let cjk = ('\u{3400}'..='\u{9fff}').contains(&ch);
        if !ch.is_alphanumeric() || cjk != prior_cjk {
            flush(&mut result, &mut word, prior_cjk);
        }
        if ch.is_alphanumeric() {
            word.push(ch);
        }
        prior_cjk = cjk;
    }
    flush(&mut result, &mut word, prior_cjk);
    result
}

impl ExperienceMemoryAdmin for ExperienceMemoryController {
    fn snapshot(&self) -> MemoryResult<ExperienceSnapshot> {
        let stored = self.stored()?;
        let inner = stored.lock().map_err(|_| MemoryError::Unavailable)?;
        if inner.context.is_none() {
            return Err(MemoryError::Unavailable);
        }
        Ok(inner.document.state.clone())
    }
    fn save_local(
        &self,
        c: &ExperienceReadContext,
        rev: u64,
        id: &str,
        v: ExperienceVersion,
    ) -> MemoryResult<()> {
        validate_context(c)?;
        if !matches!(&v.source, ExperienceSource::ScopedEvidence { scope, .. } if scope == &c.scope)
        {
            return Err(MemoryError::InvalidInput);
        }
        self.change(
            rev,
            id,
            Some(ExperienceVisibility::Scope {
                scope: c.scope.clone(),
            }),
            v,
            |r| visible(&r.visibility, c),
        )
    }
    fn save_personal(
        &self,
        c: &ExperienceReadContext,
        rev: u64,
        id: &str,
        v: ExperienceVersion,
    ) -> MemoryResult<()> {
        validate_context(c)?;
        let principal = c.principal.clone().ok_or(MemoryError::InvalidInput)?;
        if !matches!(&v.source, ExperienceSource::ScopedEvidence { scope, .. } if scope == &c.scope)
        {
            return Err(MemoryError::InvalidInput);
        }
        self.change(
            rev,
            id,
            Some(ExperienceVisibility::Principal { principal }),
            v,
            |r| visible(&r.visibility, c),
        )
    }
    fn publish_public(&self, rev: u64, id: &str, v: ExperienceVersion) -> MemoryResult<()> {
        if !matches!(v.source, ExperienceSource::PublicArtifact { .. }) {
            return Err(MemoryError::InvalidInput);
        }
        self.change(rev, id, Some(ExperienceVisibility::Subject), v, |r| {
            r.visibility == ExperienceVisibility::Subject
        })
    }
    fn revise(
        &self,
        c: &ExperienceReadContext,
        rev: u64,
        id: &str,
        v: ExperienceVersion,
    ) -> MemoryResult<()> {
        validate_context(c)?;
        if !matches!(&v.source, ExperienceSource::ScopedEvidence { scope, .. } if scope == &c.scope)
        {
            return Err(MemoryError::InvalidInput);
        }
        self.change(rev, id, None, v, |r| {
            r.visibility != ExperienceVisibility::Subject && visible(&r.visibility, c)
        })
    }
    fn revise_public(&self, rev: u64, id: &str, v: ExperienceVersion) -> MemoryResult<()> {
        if !matches!(v.source, ExperienceSource::PublicArtifact { .. }) {
            return Err(MemoryError::InvalidInput);
        }
        self.change(rev, id, None, v, |r| {
            r.visibility == ExperienceVisibility::Subject
        })
    }
}
pub struct ExperienceMemoryPlugin {
    manifest: PluginManifest,
    controller: ExperienceMemoryController,
}
impl ExperienceMemoryPlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(EXPERIENCE_MEMORY_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: ExperienceMemoryController::default(),
        })
    }
    pub fn controller(&self) -> ExperienceMemoryController {
        self.controller.clone()
    }
}
impl Plugin for ExperienceMemoryPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let document = match context.state_get(EXPERIENCE_STATE_KEY)? {
                None => Document {
                    format_version: 1,
                    state: ExperienceSnapshot::default(),
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
                    document
                }
            };
            let stored = Arc::new(Mutex::new(Stored {
                document,
                context: Some(context.clone()),
            }));
            let closing = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                closing
                    .lock()
                    .map_err(|_| PluginError::State("经验状态锁不可用".into()))?
                    .context = None;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("经验句柄不可用".into()))?;
                if active.as_ref().is_some_and(|s| Arc::ptr_eq(s, &closing)) {
                    *active = None;
                }
                Ok(())
            }))?;
            *self
                .controller
                .active
                .lock()
                .map_err(|_| PluginError::State("经验句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
