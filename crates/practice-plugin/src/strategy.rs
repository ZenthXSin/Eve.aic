//! 策略事件账本：先保存选择再草稿，效果只按实践账本结算，投影可重建晋升与回退。
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, cleanup,
};
use eve_practice_api::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

fn invalid<T>() -> PracticeResult<T> {
    Err(PracticeError::CorruptState)
}
fn hash_valid(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|v| v.is_ascii_hexdigit())
}

fn source_valid(run: &PracticeRun) -> PracticeResult<()> {
    super::store::validate_run(run)?;
    if run.status != PracticeStatus::Verified
        || run.attempts.len() < 2
        || !run.attempts[..run.attempts.len() - 1].iter().any(|a| {
            matches!(
                a.outcome,
                Some(AttemptOutcome::Rejected | AttemptOutcome::Failed)
            )
        })
    {
        return invalid();
    }
    Ok(())
}

/// 只从不可变事件重建状态，持久文件不能单独改一个 active 字段绕过验证。
pub fn strategy_views(snapshot: &StrategySnapshot) -> PracticeResult<Vec<StrategyView>> {
    let mut views: BTreeMap<u64, StrategyView> = BTreeMap::new();
    let mut uses: BTreeMap<&str, &StrategyUse> = BTreeMap::new();
    let mut observed = BTreeSet::new();
    let mut costs: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    if snapshot.events.len() > MAX_STRATEGY_EVENTS {
        return invalid();
    }
    for event in &snapshot.events {
        match event {
            StrategyEvent::Proposed(candidate) => {
                source_valid(&candidate.source)?;
                let scope = strategy_scope(&candidate.source);
                let same = |v: &&StrategyView| strategy_scope(&v.candidate.source) == scope;
                if candidate.version != views.len() as u64 + 1
                    || views
                        .values()
                        .filter(same)
                        .any(|v| v.status == StrategyStatus::Candidate)
                    || views
                        .values()
                        .any(|v| v.candidate.source.id == candidate.source.id)
                {
                    return invalid();
                }
                let parent = views
                    .values()
                    .filter(same)
                    .find(|v| v.status == StrategyStatus::Active);
                let ceiling = parent.map_or(candidate.source.attempts.len(), |v| {
                    v.attempt_ceiling.min(candidate.source.attempts.len())
                });
                if candidate.parent != parent.map(|v| v.candidate.version)
                    || candidate.attempt_ceiling != ceiling
                {
                    return invalid();
                }
                views.insert(
                    candidate.version,
                    StrategyView {
                        candidate: candidate.as_ref().clone(),
                        status: StrategyStatus::Candidate,
                        attempt_ceiling: ceiling,
                        verified_goals: BTreeSet::new(),
                        verified_briefs: BTreeSet::new(),
                    },
                );
            }
            StrategyEvent::Selected(usage) => {
                let Some(view) = views.get(&usage.version) else {
                    return invalid();
                };
                let source = &view.candidate.source;
                for id in [&usage.run_id, &usage.owner, &usage.goal_id] {
                    validate_id(id)?;
                }
                if usage.run_id != run_id(&usage.goal_id, usage.goal_revision)
                    || usage.goal_revision == 0
                    || !hash_valid(&usage.brief_sha256)
                    || usage.scope_sha256 != strategy_scope(source)
                    || usage.owner != source.task.owner
                    || usage.goal_id == source.task.goal_id
                    || usage.brief_sha256 == strategy_brief(&source.task.brief)
                    || !matches!(
                        view.status,
                        StrategyStatus::Candidate | StrategyStatus::Active
                    )
                    || uses.contains_key(usage.run_id.as_str())
                    || uses.values().any(|u| {
                        u.version == usage.version
                            && (u.goal_id == usage.goal_id || u.brief_sha256 == usage.brief_sha256)
                    })
                    || (view.status == StrategyStatus::Candidate
                        && uses.values().any(|u| {
                            u.version == usage.version && !observed.contains(u.run_id.as_str())
                        }))
                {
                    return invalid();
                }
                uses.insert(&usage.run_id, usage);
            }
            StrategyEvent::Observed {
                run_id,
                evidence_sha256,
                effect,
            } => {
                let Some(usage) = uses.get(run_id.as_str()) else {
                    return invalid();
                };
                if !observed.insert(run_id.as_str()) || !hash_valid(evidence_sha256) {
                    return invalid();
                }
                if matches!(effect,StrategyEffect::Verified { attempts } if *attempts==0 || *attempts>MAX_ATTEMPTS)
                {
                    return invalid();
                }
                let version = usage.version;
                let view = views.get_mut(&version).ok_or(PracticeError::CorruptState)?;
                let regressed = matches!(effect, StrategyEffect::Failed)
                    || matches!(effect,StrategyEffect::Verified { attempts } if *attempts>view.attempt_ceiling);
                if regressed
                    && matches!(
                        view.status,
                        StrategyStatus::Candidate | StrategyStatus::Active
                    )
                {
                    let active = view.status == StrategyStatus::Active;
                    view.status = StrategyStatus::Rejected;
                    let mut parent = view.candidate.parent;
                    if active {
                        while let Some(id) = parent {
                            let old = views.get_mut(&id).ok_or(PracticeError::CorruptState)?;
                            if old.status == StrategyStatus::Retired {
                                old.status = StrategyStatus::Active;
                                break;
                            }
                            parent = old.candidate.parent;
                        }
                    }
                } else if let StrategyEffect::Verified { attempts } = effect {
                    let view = views.get_mut(&version).ok_or(PracticeError::CorruptState)?;
                    if view.status != StrategyStatus::Candidate {
                        continue;
                    }
                    view.verified_goals.insert(usage.goal_id.clone());
                    view.verified_briefs.insert(usage.brief_sha256.clone());
                    costs.entry(version).or_default().push(*attempts);
                    if view.verified_goals.len() >= 2 && view.verified_briefs.len() >= 2 {
                        view.status = StrategyStatus::Active;
                        view.attempt_ceiling = *costs[&version]
                            .iter()
                            .max()
                            .ok_or(PracticeError::CorruptState)?;
                        if let Some(parent) = view.candidate.parent {
                            let old = views.get_mut(&parent).ok_or(PracticeError::CorruptState)?;
                            if old.status == StrategyStatus::Active {
                                old.status = StrategyStatus::Retired;
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(views.into_values().collect())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    format_version: u32,
    snapshot: StrategySnapshot,
}
struct Inner {
    ledger: Ledger,
    context: Option<PluginContext>,
}
struct Stored {
    inner: Mutex<Inner>,
}
impl Stored {
    fn open(context: PluginContext) -> PracticeResult<Self> {
        let ledger = match context
            .state_get(STRATEGY_STATE_KEY)
            .map_err(|_| PracticeError::Storage)?
        {
            None => Ledger {
                format_version: 1,
                snapshot: StrategySnapshot::default(),
            },
            Some(bytes) => {
                if bytes.len() > MAX_STRATEGY_BYTES {
                    return invalid();
                }
                let value = crate::strict_json::from_slice(&bytes)
                    .map_err(|_| PracticeError::CorruptState)?;
                if value.get("format_version").and_then(|v| v.as_u64()) != Some(1) {
                    return Err(PracticeError::UnsupportedVersion);
                }
                let ledger: Ledger =
                    serde_json::from_value(value).map_err(|_| PracticeError::CorruptState)?;
                strategy_views(&ledger.snapshot).map_err(|_| PracticeError::CorruptState)?;
                ledger
            }
        };
        Ok(Self {
            inner: Mutex::new(Inner {
                ledger,
                context: Some(context),
            }),
        })
    }
    fn update<T>(
        &self,
        action: impl FnOnce(&mut StrategySnapshot) -> PracticeResult<T>,
    ) -> PracticeResult<T> {
        let mut inner = self.inner.lock().map_err(|_| PracticeError::Unavailable)?;
        if inner.context.is_none() {
            return Err(PracticeError::Unavailable);
        }
        let mut next = inner.ledger.clone();
        let result = action(&mut next.snapshot)?;
        if next.snapshot == inner.ledger.snapshot {
            return Ok(result);
        }
        strategy_views(&next.snapshot)?;
        let bytes = serde_json::to_vec(&next).map_err(|_| PracticeError::InvalidInput)?;
        // 预留每个尚未结算选择的结局空间，满后只停止新增候选和选择，不丢弃回执。
        let (selected, observed) = counts(&next.snapshot);
        if bytes.len() + (selected - observed) * 512 > MAX_STRATEGY_BYTES
            || next.snapshot.events.len() + selected - observed > MAX_STRATEGY_EVENTS
        {
            return Err(PracticeError::LimitReached);
        }
        if inner
            .context
            .as_ref()
            .ok_or(PracticeError::Unavailable)?
            .state_set(STRATEGY_STATE_KEY, bytes)
            .is_err()
        {
            inner.context = None;
            return Err(PracticeError::Storage);
        }
        inner.ledger = next;
        Ok(result)
    }
}
fn counts(s: &StrategySnapshot) -> (usize, usize) {
    (
        s.events
            .iter()
            .filter(|e| matches!(e, StrategyEvent::Selected(_)))
            .count(),
        s.events
            .iter()
            .filter(|e| matches!(e, StrategyEvent::Observed { .. }))
            .count(),
    )
}
fn guidance(candidate: &StrategyCandidate) -> PracticeResult<PracticeGuidance> {
    let source = &candidate.source;
    let attempt = source
        .verified_attempt()
        .ok_or(PracticeError::CorruptState)?;
    let previous_issues = source
        .attempts
        .iter()
        .take(source.attempts.len() - 1)
        .flat_map(|a| {
            a.issues
                .iter()
                .chain(a.evidence.iter().flat_map(|e| &e.warnings))
        })
        .take(MAX_ISSUES)
        .cloned()
        .collect();
    Ok(PracticeGuidance {
        version: candidate.version,
        source_run: source.id.clone(),
        source_sha256: strategy_digest(source),
        previous_issues,
        corrected_example: attempt.draft.clone().ok_or(PracticeError::CorruptState)?,
    })
}

fn measured(run: &PracticeRun) -> StrategyEffect {
    match run.status {
        PracticeStatus::Verified => StrategyEffect::Verified {
            attempts: run.attempts.len(),
        },
        PracticeStatus::Unverified
            if run.attempts.iter().all(|a| {
                a.outcome == Some(AttemptOutcome::Rejected)
                    || (a.outcome == Some(AttemptOutcome::Failed)
                        && a.evidence
                            .as_ref()
                            .is_some_and(|e| e.exit == RunExit::Completed))
            }) =>
        {
            StrategyEffect::Failed
        }
        _ => StrategyEffect::Inconclusive,
    }
}

#[derive(Clone, Default)]
pub struct StrategyController {
    active: Arc<Mutex<Option<Arc<Stored>>>>,
}
impl StrategyController {
    fn service(&self) -> PracticeResult<Arc<Stored>> {
        self.active
            .lock()
            .map_err(|_| PracticeError::Unavailable)?
            .clone()
            .ok_or(PracticeError::Unavailable)
    }
}
impl PracticeStrategy for StrategyController {
    fn snapshot(&self) -> PracticeResult<StrategySnapshot> {
        self.service()?.update(|s| Ok(s.clone()))
    }
    fn select(&self, run: &PracticeRun) -> PracticeResult<Option<PracticeGuidance>> {
        super::store::validate_run(run)?;
        if run.status != PracticeStatus::Running
            || run
                .current()
                .is_none_or(|a| a.stage != AttemptStage::Drafting)
        {
            return Err(PracticeError::InvalidInput);
        }
        let service = self.service()?;
        let result=service.update(|snapshot| {
            if let Some(usage)=snapshot.events.iter().find_map(|e|match e { StrategyEvent::Selected(u) if u.run_id==run.id=>Some(u), _=>None }) {
                if usage.scope_sha256!=strategy_scope(run) || usage.goal_id!=run.task.goal_id || usage.brief_sha256!=strategy_brief(&run.task.brief)
                    || snapshot.events.iter().any(|e|matches!(e,StrategyEvent::Observed { run_id,.. } if *run_id==run.id)) { return Err(PracticeError::InvalidInput); }
                let candidate=snapshot.events.iter().find_map(|e|match e { StrategyEvent::Proposed(c) if c.version==usage.version=>Some(c), _=>None }).ok_or(PracticeError::CorruptState)?;
                return guidance(candidate).map(Some);
            }
            // 只在本轮首次草稿前选择；不能把已执行的前几次尝试追记为策略调用。
            if run.attempts.len()!=1 || run.current().is_none_or(|a|a.stage!=AttemptStage::Drafting) { return Ok(None); }
            let views=strategy_views(snapshot)?;
            let scope=strategy_scope(run);let brief=strategy_brief(&run.task.brief);
            let eligible=|v: &&StrategyView| {
                let c=&v.candidate;
                strategy_scope(&c.source)==scope && c.source.task.goal_id!=run.task.goal_id && strategy_brief(&c.source.task.brief)!=brief
                && !snapshot.events.iter().any(|e|matches!(e,StrategyEvent::Selected(u) if u.version==c.version && (u.goal_id==run.task.goal_id || u.brief_sha256==brief)))
                && !(v.status==StrategyStatus::Candidate && snapshot.events.iter().any(|e|matches!(e,StrategyEvent::Selected(u) if u.version==c.version && !snapshot.events.iter().any(|o|matches!(o,StrategyEvent::Observed { run_id,.. } if *run_id==u.run_id)))))
            };
            let chosen=views.iter().filter(eligible).find(|v|v.status==StrategyStatus::Candidate).or_else(||views.iter().filter(eligible).find(|v|v.status==StrategyStatus::Active));
            let Some(chosen)=chosen else { return Ok(None) };
            snapshot.events.push(StrategyEvent::Selected(StrategyUse {run_id:run.id.clone(),owner:run.task.owner.clone(),goal_id:run.task.goal_id.clone(),goal_revision:run.task.goal_revision,brief_sha256:brief,scope_sha256:scope,version:chosen.candidate.version }));
            guidance(&chosen.candidate).map(Some)
        });
        match result {
            Err(PracticeError::LimitReached) => Ok(None),
            other => other,
        }
    }
    fn reconcile(&self, practice: &PracticeSnapshot) -> PracticeResult<()> {
        // 先结算所有既有选择，再从成功修正中派生；重复调用不增加事件。
        let service = self.service()?;
        service.update(|snapshot| {
            // 来源和既有结局逐一核对，缺失或被改写的原实践不能继续支撑策略。
            for event in &snapshot.events {
                match event {
                    StrategyEvent::Proposed(candidate) => {
                        if !practice
                            .runs
                            .iter()
                            .any(|r| r.id == candidate.source.id && *r == candidate.source)
                        {
                            return invalid();
                        }
                    }
                    StrategyEvent::Selected(usage) => {
                        if !practice.runs.iter().any(|r| {
                            r.id == usage.run_id
                                && strategy_scope(r) == usage.scope_sha256
                                && strategy_brief(&r.task.brief) == usage.brief_sha256
                        }) {
                            return invalid();
                        }
                    }
                    StrategyEvent::Observed {
                        run_id,
                        evidence_sha256,
                        effect,
                    } => {
                        if !practice.runs.iter().any(|r| {
                            r.id == *run_id
                                && strategy_digest(r) == *evidence_sha256
                                && measured(r) == *effect
                        }) {
                            return invalid();
                        }
                    }
                }
            }
            for run in &practice.runs {
                if run.status == PracticeStatus::Running {
                    continue;
                }
                let usage = snapshot.events.iter().find_map(|e| match e {
                    StrategyEvent::Selected(u) if u.run_id == run.id => Some(u),
                    _ => None,
                });
                let Some(usage) = usage else { continue };
                if snapshot
                    .events
                    .iter()
                    .any(|e| matches!(e,StrategyEvent::Observed {run_id,..} if *run_id==run.id))
                {
                    continue;
                }
                super::store::validate_run(run)?;
                if usage.scope_sha256 != strategy_scope(run)
                    || usage.brief_sha256 != strategy_brief(&run.task.brief)
                {
                    return invalid();
                }
                let effect = measured(run);
                snapshot.events.push(StrategyEvent::Observed {
                    run_id: run.id.clone(),
                    evidence_sha256: strategy_digest(run),
                    effect,
                });
            }
            Ok(())
        })?;
        for run in &practice.runs {
            if source_valid(run).is_err() {
                continue;
            }
            let result = service.update(|snapshot| {
                let views = strategy_views(snapshot)?;
                if views.iter().any(|v| {
                    v.candidate.source.id == run.id
                        || (strategy_scope(&v.candidate.source) == strategy_scope(run)
                            && v.status == StrategyStatus::Candidate)
                }) {
                    return Ok(());
                }
                // 策略自身失败的回执不能立即变成新策略，防止自我循环试验。
                if snapshot
                    .events
                    .iter()
                    .any(|e| matches!(e,StrategyEvent::Selected(u) if u.run_id==run.id))
                {
                    return Ok(());
                }
                let parent = views.iter().find(|v| {
                    strategy_scope(&v.candidate.source) == strategy_scope(run)
                        && v.status == StrategyStatus::Active
                });
                let ceiling = parent.map_or(run.attempts.len(), |v| {
                    v.attempt_ceiling.min(run.attempts.len())
                });
                snapshot
                    .events
                    .push(StrategyEvent::Proposed(Box::new(StrategyCandidate {
                        version: views.len() as u64 + 1,
                        parent: parent.map(|v| v.candidate.version),
                        source: run.clone(),
                        attempt_ceiling: ceiling,
                    })));
                Ok(())
            });
            if result == Err(PracticeError::LimitReached) {
                break;
            }
            result?;
        }
        Ok(())
    }
}

pub struct StrategyPlugin {
    manifest: PluginManifest,
    controller: StrategyController,
}
impl StrategyPlugin {
    pub fn new() -> Result<Self, PluginError> {
        Ok(Self {
            manifest: PluginManifest::new(STRATEGY_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: StrategyController::default(),
        })
    }
    pub fn controller(&self) -> StrategyController {
        self.controller.clone()
    }
}
impl Plugin for StrategyPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                Stored::open(context.clone()).map_err(|e| PluginError::State(e.to_string()))?,
            );
            let controller = self.controller.clone();
            let closing = stored.clone();
            context.cleanup(cleanup(move || async move {
                closing
                    .inner
                    .lock()
                    .map_err(|_| PluginError::State("策略句柄不可用".into()))?
                    .context = None;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("策略句柄不可用".into()))?;
                if active.as_ref().is_some_and(|s| Arc::ptr_eq(s, &closing)) {
                    *active = None;
                }
                Ok(())
            }))?;
            *self
                .controller
                .active
                .lock()
                .map_err(|_| PluginError::State("策略句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
