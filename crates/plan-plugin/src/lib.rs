//! 通过公开 StateStore 保存有条件多步计划；管理能力只由宿主持有。
//!
//! 每次写入先持久化、成功后才更新内存。启动时遗留 Executing 先封存为 Blocked/Interrupted，
//! 不重放可能已发生的副作用。存储故障和损坏状态明确报错，不清空、不淘汰旧记录。
mod strict_json;

use eve_plan_api::*;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

pub const PLAN_STATE_KEY: &str = "plans.v1";

struct Inner {
    snapshot: PlanSnapshot,
    context: Option<PluginContext>,
}

struct StoredPlans {
    inner: Mutex<Inner>,
}

impl StoredPlans {
    fn open(context: PluginContext, subject_id: &str, now_ms: u64) -> PlanResult<Self> {
        let bytes = context
            .state_get(PLAN_STATE_KEY)
            .map_err(|_| PlanError::Storage)?;
        let mut snapshot = match bytes {
            None => PlanSnapshot {
                schema_version: PLAN_SCHEMA_VERSION,
                subject_id: subject_id.into(),
                revision: 0,
                plans: Vec::new(),
            },
            Some(bytes) => {
                if bytes.len() > MAX_PLAN_STATE_BYTES {
                    return Err(PlanError::CorruptState);
                }
                let value = strict_json::from_slice(&bytes).map_err(|_| PlanError::CorruptState)?;
                let snapshot: PlanSnapshot =
                    serde_json::from_value(value).map_err(|_| PlanError::CorruptState)?;
                if snapshot.schema_version != PLAN_SCHEMA_VERSION {
                    return Err(PlanError::CorruptState);
                }
                if snapshot.subject_id != subject_id {
                    return Err(PlanError::SubjectMismatch);
                }
                snapshot.validate().map_err(|_| PlanError::CorruptState)?;
                snapshot
            }
        };
        // 恢复先保存再公开实例；从不重放已经开始的步骤。
        let mut recovered = false;
        for plan in &mut snapshot.plans {
            recovered |= plan.interrupt(now_ms)?;
        }
        if recovered {
            snapshot.revision = snapshot
                .revision
                .checked_add(1)
                .ok_or(PlanError::LimitReached)?;
            context
                .state_set(PLAN_STATE_KEY, encode(&snapshot)?)
                .map_err(|_| PlanError::Storage)?;
        }
        Ok(Self {
            inner: Mutex::new(Inner {
                snapshot,
                context: Some(context),
            }),
        })
    }

    fn lock(&self) -> PlanResult<MutexGuard<'_, Inner>> {
        let inner = self.inner.lock().map_err(|_| PlanError::Unavailable)?;
        if inner.context.is_none() {
            return Err(PlanError::Unavailable);
        }
        Ok(inner)
    }

    fn snapshot(&self) -> PlanResult<PlanSnapshot> {
        Ok(self.lock()?.snapshot.clone())
    }

    fn create(
        &self,
        spec: PlanSpec,
        capabilities: &[CapabilitySpec],
        at_ms: u64,
    ) -> PlanResult<PlanCreate> {
        let mut inner = self.lock()?;
        let plan = Plan::new(&inner.snapshot.subject_id, spec, capabilities, at_ms)?;
        if let Some(existing) = inner.snapshot.plans.iter().find(|p| p.id == plan.id) {
            return Ok(PlanCreate {
                plan: existing.clone(),
                duplicate: true,
            });
        }
        // 每个目标同一时间只有一份活动计划，避免用多份计划叠加执行预算。
        if inner.snapshot.plans.iter().any(|existing| {
            existing.status == PlanStatus::Active
                && existing.binding.goal_id == plan.binding.goal_id
        }) {
            return Err(PlanError::Conflict);
        }
        if inner.snapshot.plans.len() >= MAX_PLANS {
            return Err(PlanError::LimitReached);
        }
        let mut next = inner.snapshot.clone();
        next.plans.push(plan.clone());
        persist(&mut inner, next)?;
        Ok(PlanCreate {
            plan,
            duplicate: false,
        })
    }

    /// 在副本上做 CAS 与状态转换，保存成功后才替换内存。
    fn update(
        &self,
        plan_id: &str,
        expected_revision: u64,
        change: impl FnOnce(&mut Plan) -> PlanResult<bool>,
    ) -> PlanResult<Plan> {
        validate_id(plan_id)?;
        if expected_revision == 0 {
            return Err(PlanError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let index = inner
            .snapshot
            .plans
            .iter()
            .position(|plan| plan.id == plan_id)
            .ok_or(PlanError::NotFound)?;
        let mut plan = inner.snapshot.plans[index].clone();
        if plan.revision != expected_revision {
            return Err(PlanError::StaleRevision);
        }
        if !change(&mut plan)? {
            return Ok(plan);
        }
        let mut next = inner.snapshot.clone();
        next.plans[index] = plan.clone();
        persist(&mut inner, next)?;
        Ok(plan)
    }

    fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("计划状态锁不可用".into()))?
            .context
            .take();
        Ok(())
    }
}

fn persist(inner: &mut Inner, mut snapshot: PlanSnapshot) -> PlanResult<()> {
    snapshot.revision = snapshot
        .revision
        .checked_add(1)
        .ok_or(PlanError::LimitReached)?;
    inner
        .context
        .as_ref()
        .ok_or(PlanError::Unavailable)?
        .state_set(PLAN_STATE_KEY, encode(&snapshot)?)
        .map_err(|_| PlanError::Storage)?;
    inner.snapshot = snapshot;
    Ok(())
}

fn encode(snapshot: &PlanSnapshot) -> PlanResult<Vec<u8>> {
    snapshot.validate()?;
    let bytes = serde_json::to_vec(snapshot).map_err(|_| PlanError::InvalidInput)?;
    if bytes.len() > MAX_PLAN_STATE_BYTES {
        return Err(PlanError::LimitReached);
    }
    Ok(bytes)
}

fn now_ms() -> PlanResult<u64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| PlanError::Unavailable)?;
    let value = u64::try_from(elapsed.as_millis()).map_err(|_| PlanError::LimitReached)?;
    if value == 0 {
        return Err(PlanError::Unavailable);
    }
    Ok(value)
}

/// 管理句柄不发布到服务目录；插件停止后所有方法返回 Unavailable。
#[derive(Clone, Default)]
pub struct PlanController {
    active: Arc<Mutex<Option<Arc<StoredPlans>>>>,
}

impl PlanController {
    fn service(&self) -> PlanResult<Arc<StoredPlans>> {
        self.active
            .lock()
            .map_err(|_| PlanError::Unavailable)?
            .clone()
            .ok_or(PlanError::Unavailable)
    }
}

impl PlanJournal for PlanController {
    fn snapshot(&self) -> PlanResult<PlanSnapshot> {
        self.service()?.snapshot()
    }

    fn create(
        &self,
        spec: PlanSpec,
        capabilities: &[CapabilitySpec],
        at_ms: u64,
    ) -> PlanResult<PlanCreate> {
        self.service()?.create(spec, capabilities, at_ms)
    }

    fn invalidate(
        &self,
        plan_id: &str,
        expected_revision: u64,
        current: &PlanBinding,
        at_ms: u64,
    ) -> PlanResult<Plan> {
        self.service()?.update(plan_id, expected_revision, |plan| {
            plan.invalidate(current, at_ms)
        })
    }

    fn begin_step(
        &self,
        plan_id: &str,
        step_id: &str,
        expected_revision: u64,
        at_ms: u64,
    ) -> PlanResult<Plan> {
        self.service()?.update(plan_id, expected_revision, |plan| {
            plan.begin_step(step_id, at_ms).map(|_| true)
        })
    }

    fn finish_step(
        &self,
        plan_id: &str,
        step_id: &str,
        expected_revision: u64,
        outcome: StepOutcome,
    ) -> PlanResult<Plan> {
        self.service()?.update(plan_id, expected_revision, |plan| {
            plan.finish_step(step_id, outcome).map(|_| true)
        })
    }
}

pub struct PlanPlugin {
    manifest: PluginManifest,
    subject_id: String,
    controller: PlanController,
}

impl PlanPlugin {
    pub fn new(subject_id: impl Into<String>) -> PluginResult<Self> {
        let subject_id = subject_id.into();
        validate_id(&subject_id).map_err(|error| PluginError::State(error.to_string()))?;
        Ok(Self {
            manifest: PluginManifest::new(PLAN_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            subject_id,
            controller: PlanController::default(),
        })
    }

    pub fn controller(&self) -> PlanController {
        self.controller.clone()
    }
}

impl Plugin for PlanPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let now = now_ms().map_err(|error| PluginError::State(error.to_string()))?;
            let stored = Arc::new(
                StoredPlans::open(context.clone(), &self.subject_id, now)
                    .map_err(|error| PluginError::State(error.to_string()))?,
            );
            let to_close = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("计划管理句柄不可用".into()))?;
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
                .map_err(|_| PluginError::State("计划管理句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
