use eve_cognition_api::*;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, MutexGuard};

/// 严格比较执行前条件；工具错误、过期、非最终批次均保留不确定性。
pub fn evaluate_world_receipt(
    prediction: &WorldPrediction,
    receipt: Option<&WorldReceipt>,
) -> CognitionResult<WorldEffect> {
    prediction.validate()?;
    let Some(receipt) = receipt else {
        return Ok(WorldEffect::Inconclusive);
    };
    receipt.validate()?;
    if receipt.execution_id != prediction.execution_id
        || receipt.tool != prediction.check.tool
        || receipt.arguments_sha256 != prediction.check.arguments_sha256
    {
        return Err(CognitionError::InvalidInput);
    }
    if !receipt.succeeded
        || receipt.environment_sha256 != prediction.scope.environment_sha256
        || !receipt.final_batch
        || receipt.at_ms < prediction.created_at_ms
        || receipt.at_ms > prediction.deadline_ms
    {
        return Ok(WorldEffect::Inconclusive);
    }
    let Some(actual) = receipt.output.pointer(&prediction.check.output_pointer) else {
        return Ok(WorldEffect::Inconclusive);
    };
    Ok(if actual == &prediction.check.expected {
        WorldEffect::Verified
    } else {
        WorldEffect::Failed
    })
}

/// 只按完整作用域聚合，不把用户私有能力证据混入公共画像。
pub fn capability_estimates(
    snapshot: &WorldModelSnapshot,
    access: &ReadAccess,
    environment: &str,
) -> CognitionResult<Vec<CapabilityEstimate>> {
    validate_snapshot(snapshot)?;
    access.validate()?;
    if !valid_world_digest(environment) {
        return Err(CognitionError::InvalidInput);
    }
    let mut estimates: Vec<(CapabilityEstimate, u32)> = Vec::new();
    for trial in &snapshot.trials {
        let scope = &trial.prediction.scope;
        if scope.environment_sha256 != environment || !scope.visibility.visible_to(access) {
            continue;
        }
        let index = if let Some(index) = estimates.iter().position(|(e, _)| &e.scope == scope) {
            index
        } else {
            estimates.push((
                CapabilityEstimate {
                    scope: scope.clone(),
                    verified: 0,
                    failed: 0,
                    inconclusive: 0,
                    pending: 0,
                    success_probability_percent: 50,
                    forecast_error_percent: None,
                    last_evidence_at_ms: None,
                },
                0,
            ));
            estimates.len() - 1
        };
        let (estimate, error) = &mut estimates[index];
        match &trial.resolution {
            None => estimate.pending += 1,
            Some(resolution) => {
                estimate.last_evidence_at_ms = Some(
                    estimate
                        .last_evidence_at_ms
                        .unwrap_or(0)
                        .max(resolution.at_ms),
                );
                match resolution.effect {
                    WorldEffect::Verified => {
                        estimate.verified += 1;
                        *error += 100 - u32::from(trial.prediction.success_probability_percent);
                    }
                    WorldEffect::Failed => {
                        estimate.failed += 1;
                        *error += u32::from(trial.prediction.success_probability_percent);
                    }
                    WorldEffect::Inconclusive => estimate.inconclusive += 1,
                }
            }
        }
    }
    Ok(estimates
        .into_iter()
        .map(|(mut e, error)| {
            let known = e.verified + e.failed;
            e.success_probability_percent = (((e.verified + 1) * 100) / (known + 2)) as u8;
            e.forecast_error_percent = (known != 0).then(|| (error / known) as u8);
            e
        })
        .collect())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    format_version: u32,
    subject_id: String,
    snapshot: WorldModelSnapshot,
}

fn validate_snapshot(snapshot: &WorldModelSnapshot) -> CognitionResult<()> {
    if snapshot.observations.len() + snapshot.trials.len() > MAX_WORLD_RECORDS {
        return Err(CognitionError::LimitReached);
    }
    let mut ids = std::collections::BTreeSet::new();
    for observation in &snapshot.observations {
        observation.validate()?;
        if !ids.insert(&observation.id) {
            return Err(CognitionError::InvalidInput);
        }
    }
    let mut executions = std::collections::BTreeSet::new();
    for trial in &snapshot.trials {
        let prediction = &trial.prediction;
        prediction.validate()?;
        if !ids.insert(&prediction.id)
            || !executions.insert((
                prediction.execution_id.clone(),
                world_digest(&prediction.check)?,
            ))
        {
            return Err(CognitionError::InvalidInput);
        }
        for id in &prediction.observation_ids {
            let observation = snapshot
                .observations
                .iter()
                .find(|o| &o.id == id)
                .ok_or(CognitionError::InvalidInput)?;
            if observation.scope.environment_sha256 != prediction.scope.environment_sha256
                || !prediction
                    .scope
                    .visibility
                    .restricts(&observation.scope.visibility)
                || observation.at_ms > prediction.created_at_ms
                || observation.expires_at_ms <= prediction.created_at_ms
            {
                return Err(CognitionError::AccessDenied);
            }
        }
        if let Some(resolution) = &trial.resolution
            && (resolution.at_ms < prediction.created_at_ms
                || resolution
                    .receipt
                    .as_ref()
                    .is_some_and(|r| r.at_ms > resolution.at_ms)
                || evaluate_world_receipt(prediction, resolution.receipt.as_ref())?
                    != resolution.effect)
        {
            return Err(CognitionError::InvalidInput);
        }
    }
    if snapshot.revision == 0 && (!snapshot.observations.is_empty() || !snapshot.trials.is_empty())
    {
        return Err(CognitionError::InvalidInput);
    }
    Ok(())
}

// 预留每个未结算预测的完整回执空间，账本接近容量时仍可以结算，不静默丢弃。
fn encode(ledger: &Ledger) -> CognitionResult<Vec<u8>> {
    validate_snapshot(&ledger.snapshot)?;
    let bytes = serde_json::to_vec(ledger).map_err(|_| CognitionError::InvalidInput)?;
    let pending = ledger
        .snapshot
        .trials
        .iter()
        .filter(|t| t.resolution.is_none())
        .count();
    if bytes.len().saturating_add(pending * 12_288) > MAX_WORLD_BYTES {
        return Err(CognitionError::LimitReached);
    }
    Ok(bytes)
}

struct Inner {
    context: Option<PluginContext>,
    ledger: Ledger,
}
struct Stored {
    inner: Mutex<Inner>,
}
impl Stored {
    fn open(context: PluginContext, subject_id: &str) -> CognitionResult<Self> {
        let bytes = context
            .state_get(WORLD_MODEL_STATE_KEY)
            .map_err(|_| CognitionError::Storage)?;
        let ledger = if let Some(bytes) = bytes {
            if bytes.len() > MAX_WORLD_BYTES {
                return Err(CognitionError::LimitReached);
            }
            let value =
                crate::strict_json::from_slice(&bytes).map_err(|_| CognitionError::CorruptState)?;
            let ledger: Ledger =
                serde_json::from_value(value).map_err(|_| CognitionError::CorruptState)?;
            if ledger.format_version != 1 {
                return Err(CognitionError::UnsupportedVersion);
            }
            if ledger.subject_id != subject_id {
                return Err(CognitionError::SubjectMismatch);
            }
            encode(&ledger).map_err(|_| CognitionError::CorruptState)?;
            ledger
        } else {
            Ledger {
                format_version: 1,
                subject_id: subject_id.into(),
                snapshot: WorldModelSnapshot::default(),
            }
        };
        // 未结算预测原样保留。重启既不宣称失败，也不重放实际操作。
        Ok(Self {
            inner: Mutex::new(Inner {
                context: Some(context),
                ledger,
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
    fn snapshot(&self) -> CognitionResult<WorldModelSnapshot> {
        Ok(self.lock()?.ledger.snapshot.clone())
    }
    fn mutate<T>(
        &self,
        change: impl FnOnce(&mut WorldModelSnapshot) -> CognitionResult<T>,
    ) -> CognitionResult<T> {
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let result = change(&mut next.snapshot)?;
        if next.snapshot == inner.ledger.snapshot {
            return Ok(result);
        }
        next.snapshot.revision = inner
            .ledger
            .snapshot
            .revision
            .checked_add(1)
            .ok_or(CognitionError::LimitReached)?;
        let bytes = encode(&next)?;
        let context = inner.context.as_ref().ok_or(CognitionError::Unavailable)?;
        if context.state_set(WORLD_MODEL_STATE_KEY, bytes).is_err() {
            // 后端可能已提交；失效整个句柄，避免用旧内存覆盖未知提交。
            inner.context = None;
            return Err(CognitionError::Storage);
        }
        inner.ledger = next;
        Ok(result)
    }
    fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("环境模型状态锁不可用".into()))?
            .context = None;
        Ok(())
    }
}

struct ScopedReader {
    stored: Arc<Stored>,
    access: ReadAccess,
}
impl WorldModelReader for ScopedReader {
    fn snapshot(&self, environment_sha256: &str, now_ms: u64) -> CognitionResult<WorldModelView> {
        if !valid_world_digest(environment_sha256) || now_ms == 0 {
            return Err(CognitionError::InvalidInput);
        }
        let mut snapshot = self.stored.snapshot()?;
        snapshot.observations.retain(|o| {
            o.scope.environment_sha256 == environment_sha256
                && o.scope.visibility.visible_to(&self.access)
                && o.at_ms <= now_ms
                && now_ms < o.expires_at_ms
        });
        snapshot.trials.retain(|t| {
            t.prediction.scope.environment_sha256 == environment_sha256
                && t.prediction.scope.visibility.visible_to(&self.access)
        });
        Ok(WorldModelView {
            revision: snapshot.revision,
            observations: snapshot.observations,
            trials: snapshot
                .trials
                .into_iter()
                .map(|trial| {
                    let receipt_sha256 = trial
                        .resolution
                        .as_ref()
                        .and_then(|r| r.receipt.as_ref())
                        .map(world_digest)
                        .transpose()?;
                    Ok(WorldTrialView {
                        prediction: trial.prediction,
                        effect: trial.resolution.as_ref().map(|r| r.effect),
                        receipt_sha256,
                        resolved_at_ms: trial.resolution.as_ref().map(|r| r.at_ms),
                    })
                })
                .collect::<CognitionResult<Vec<_>>>()?,
        })
    }
    fn capabilities(&self, environment_sha256: &str) -> CognitionResult<Vec<CapabilityEstimate>> {
        capability_estimates(&self.stored.snapshot()?, &self.access, environment_sha256)
    }
}

#[derive(Clone, Default)]
pub struct WorldModelController {
    active: Arc<Mutex<Option<Arc<Stored>>>>,
}
impl WorldModelController {
    fn service(&self) -> CognitionResult<Arc<Stored>> {
        self.active
            .lock()
            .map_err(|_| CognitionError::Unavailable)?
            .clone()
            .ok_or(CognitionError::Unavailable)
    }
}
impl WorldModelAdmin for WorldModelController {
    fn snapshot(&self) -> CognitionResult<WorldModelSnapshot> {
        self.service()?.snapshot()
    }
    fn reader(&self, access: ReadAccess) -> CognitionResult<Arc<dyn WorldModelReader>> {
        access.validate()?;
        let stored = self.service()?;
        drop(stored.lock()?);
        Ok(Arc::new(ScopedReader { stored, access }))
    }
    fn observe(&self, observation: WorldObservation) -> CognitionResult<()> {
        observation.validate()?;
        self.service()?.mutate(|snapshot| {
            if let Some(old) = snapshot
                .observations
                .iter()
                .find(|o| o.id == observation.id)
            {
                return if old == &observation {
                    Ok(())
                } else {
                    Err(CognitionError::InvalidInput)
                };
            }
            snapshot.observations.push(observation);
            Ok(())
        })
    }
    fn predict(&self, prediction: WorldPrediction) -> CognitionResult<()> {
        prediction.validate()?;
        self.service()?.mutate(|snapshot| {
            if let Some(old) = snapshot
                .trials
                .iter()
                .find(|t| t.prediction.id == prediction.id)
            {
                return if old.prediction == prediction {
                    Ok(())
                } else {
                    Err(CognitionError::InvalidInput)
                };
            }
            snapshot.trials.push(WorldTrial {
                prediction,
                resolution: None,
            });
            Ok(())
        })
    }
    fn resolve(
        &self,
        prediction_id: &str,
        receipt: Option<WorldReceipt>,
        at_ms: u64,
    ) -> CognitionResult<WorldEffect> {
        validate_id(prediction_id)?;
        self.service()?.mutate(|snapshot| {
            let trial = snapshot
                .trials
                .iter_mut()
                .find(|t| t.prediction.id == prediction_id)
                .ok_or(CognitionError::InvalidInput)?;
            let effect = evaluate_world_receipt(&trial.prediction, receipt.as_ref())?;
            if let Some(old) = &trial.resolution {
                // 投递时间不参与幂等判断；固定回执和效果不能事后修改。
                return if old.receipt == receipt && old.effect == effect {
                    Ok(effect)
                } else {
                    Err(CognitionError::InvalidTransition)
                };
            }
            trial.resolution = Some(WorldResolution {
                receipt,
                effect,
                at_ms,
            });
            Ok(effect)
        })
    }
}

pub struct WorldModelPlugin {
    manifest: PluginManifest,
    subject_id: String,
    controller: WorldModelController,
}
impl WorldModelPlugin {
    pub fn new(subject_id: impl Into<String>) -> PluginResult<Self> {
        let subject_id = subject_id.into();
        validate_id(&subject_id).map_err(|e| PluginError::State(e.to_string()))?;
        Ok(Self {
            manifest: PluginManifest::new(WORLD_MODEL_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            subject_id,
            controller: WorldModelController::default(),
        })
    }
    pub fn controller(&self) -> WorldModelController {
        self.controller.clone()
    }
}
impl Plugin for WorldModelPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                Stored::open(context.clone(), &self.subject_id)
                    .map_err(|e| PluginError::State(e.to_string()))?,
            );
            let to_close = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("环境模型句柄不可用".into()))?;
                if active.as_ref().is_some_and(|s| Arc::ptr_eq(s, &to_close)) {
                    *active = None;
                }
                Ok(())
            }))?;
            *self
                .controller
                .active
                .lock()
                .map_err(|_| PluginError::State("环境模型句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
