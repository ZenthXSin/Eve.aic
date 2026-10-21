//! 条件唤醒账本：先提交准入，再执行；崩溃和未知投递不重放。
mod strict_json;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
use eve_session_api::SessionKey;
use eve_trigger_api::*;
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    format_version: u32,
    triggers: Vec<Trigger>,
}
struct Inner {
    ledger: Ledger,
    context: PluginContext,
    available: bool,
}
#[derive(Clone, Default)]
pub struct TriggerController(Arc<Mutex<Option<Inner>>>);

fn valid(ledger: &Ledger) -> bool {
    let mut ids = BTreeSet::new();
    let mut sources = BTreeSet::new();
    ledger.format_version == 1
        && ledger.triggers.len() <= MAX_TRIGGERS
        && ledger.triggers.iter().all(|t| {
            let expected = identifier(&t.owner, &t.source);
            t.id == expected
                && ids.insert(&t.id)
                && sources.insert((&t.owner.session_id, &t.owner.user_id, &t.source))
                && t.owner.validate().is_ok()
                && t.spec.valid()
                && !t.source.is_empty()
                && t.source.len() <= 256
                && t.created_at_ms > 0
                && t.spec.expires_at_ms > t.created_at_ms
                && t.evidence.len() <= MAX_CONDITIONS
                && t.evidence.iter().all(|s| s.len() <= 512)
                && t.result
                    .as_ref()
                    .is_none_or(|s| s.len() <= 32768 && !s.trim().is_empty())
                && (t.status != Status::Completed || t.result.is_some())
                && (t.delivery.is_none() || t.status == Status::Completed)
                && (t.status != Status::Completed || t.delivery.is_some())
        })
}
fn identifier(owner: &SessionKey, source: &str) -> String {
    let data = serde_json::to_vec(&(&owner.session_id, &owner.user_id, source))
        .expect("strings serialize");
    let hex: String = digest(&SHA256, &data)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("wake-{hex}")
}
impl TriggerController {
    fn change<T>(&self, f: impl FnOnce(&mut Ledger) -> Result<T>) -> Result<T> {
        let mut guard = self.0.lock().map_err(|_| TriggerError::Unavailable)?;
        let inner = guard
            .as_mut()
            .filter(|i| i.available)
            .ok_or(TriggerError::Unavailable)?;
        let mut next = inner.ledger.clone();
        let result = f(&mut next)?;
        if !valid(&next) {
            return Err(TriggerError::Invalid);
        }
        let bytes = serde_json::to_vec(&next).map_err(|_| TriggerError::Invalid)?;
        if bytes.len() > MAX_BYTES {
            return Err(TriggerError::Limit);
        }
        if inner.context.state_set(STATE_KEY, bytes).is_err() {
            inner.available = false;
            return Err(TriggerError::Storage);
        }
        inner.ledger = next;
        Ok(result)
    }
}
impl TriggerAdmin for TriggerController {
    fn snapshot(&self) -> Result<Vec<Trigger>> {
        let guard = self.0.lock().map_err(|_| TriggerError::Unavailable)?;
        Ok(guard
            .as_ref()
            .filter(|i| i.available)
            .ok_or(TriggerError::Unavailable)?
            .ledger
            .triggers
            .clone())
    }
    fn create(
        &self,
        owner: SessionKey,
        source: String,
        spec: TriggerSpec,
        now: u64,
    ) -> Result<Trigger> {
        if owner.validate().is_err()
            || source.is_empty()
            || source.len() > 256
            || !spec.valid()
            || now == 0
            || spec.expires_at_ms <= now
            || spec.expires_at_ms.saturating_sub(now) > 90 * 86_400_000
        {
            return Err(TriggerError::Invalid);
        }
        self.change(|l| {
            let id = identifier(&owner, &source);
            if let Some(old) = l.triggers.iter().find(|t| t.id == id) {
                return Ok(old.clone());
            }
            if l.triggers.len() >= MAX_TRIGGERS
                || l.triggers
                    .iter()
                    .filter(|t| {
                        t.owner == owner
                            && matches!(t.status, Status::Armed | Status::Ready | Status::Running)
                    })
                    .count()
                    >= MAX_ACTIVE_PER_SESSION
                || l.triggers
                    .iter()
                    .filter(|t| {
                        t.owner == owner && now.saturating_sub(t.created_at_ms) < 86_400_000
                    })
                    .count()
                    >= 32
            {
                return Err(TriggerError::Limit);
            }
            let t = Trigger {
                id,
                owner,
                source,
                spec,
                created_at_ms: now,
                status: Status::Armed,
                evidence: vec![],
                result: None,
                delivery: None,
            };
            l.triggers.push(t.clone());
            Ok(t)
        })
    }
    fn ready(&self, id: &str, evidence: Vec<String>) -> Result<()> {
        self.change(|l| {
            let t = l
                .triggers
                .iter_mut()
                .find(|t| t.id == id)
                .ok_or(TriggerError::NotFound)?;
            if t.status != Status::Armed || evidence.is_empty() {
                return Err(TriggerError::Transition);
            }
            t.status = Status::Ready;
            t.evidence = evidence;
            Ok(())
        })
    }
    fn transition(&self, id: &str, from: Status, to: Status, result: Option<String>) -> Result<()> {
        self.change(|l| {
            let t = l
                .triggers
                .iter_mut()
                .find(|t| t.id == id)
                .ok_or(TriggerError::NotFound)?;
            let allowed = matches!(
                (from, to),
                (Status::Ready, Status::Running)
                    | (
                        Status::Running,
                        Status::Ready | Status::Completed | Status::Failed | Status::Interrupted
                    )
                    | (
                        Status::Armed | Status::Ready,
                        Status::Expired | Status::Failed
                    )
            );
            if t.status != from || !allowed {
                return Err(TriggerError::Transition);
            }
            t.status = to;
            t.result = result;
            if to == Status::Completed {
                t.delivery = Some(Delivery::Pending);
            }
            Ok(())
        })
    }
    fn cancel(&self, owner: &SessionKey, id: &str) -> Result<()> {
        self.change(|l| {
            let t = l
                .triggers
                .iter_mut()
                .find(|t| t.id == id && &t.owner == owner)
                .ok_or(TriggerError::NotFound)?;
            if !matches!(t.status, Status::Armed | Status::Ready | Status::Running) {
                return Err(TriggerError::Transition);
            }
            t.status = Status::Cancelled;
            Ok(())
        })
    }
    fn delivery(&self, id: &str, from: Delivery, to: Delivery) -> Result<()> {
        self.change(|l| {
            let t = l
                .triggers
                .iter_mut()
                .find(|t| t.id == id)
                .ok_or(TriggerError::NotFound)?;
            if t.delivery != Some(from)
                || !matches!(
                    (from, to),
                    (Delivery::Pending, Delivery::Sending | Delivery::Failed)
                        | (
                            Delivery::Sending,
                            Delivery::Sent | Delivery::Failed | Delivery::Unknown
                        )
                )
            {
                return Err(TriggerError::Transition);
            }
            t.delivery = Some(to);
            Ok(())
        })
    }
}
pub struct TriggerPlugin {
    manifest: PluginManifest,
    controller: TriggerController,
}
impl TriggerPlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: TriggerController::default(),
        })
    }
    pub fn controller(&self) -> TriggerController {
        self.controller.clone()
    }
}
impl Plugin for TriggerPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let fail = || PluginError::State("触发器账本损坏或不可用；保留原状态".into());
            let bytes = context.state_get(STATE_KEY)?;
            let mut ledger = match bytes {
                None => Ledger {
                    format_version: 1,
                    triggers: vec![],
                },
                Some(bytes) if bytes.len() <= MAX_BYTES => {
                    serde_json::from_value(strict_json::from_slice(&bytes).map_err(|_| fail())?)
                        .map_err(|_| fail())?
                }
                _ => return Err(fail()),
            };
            if !valid(&ledger) {
                return Err(fail());
            }
            let mut changed = false;
            for t in &mut ledger.triggers {
                if t.status == Status::Running {
                    t.status = Status::Interrupted;
                    changed = true;
                }
                if t.delivery == Some(Delivery::Sending) {
                    t.delivery = Some(Delivery::Unknown);
                    changed = true;
                }
            }
            if changed {
                context.state_set(STATE_KEY, serde_json::to_vec(&ledger).map_err(|_| fail())?)?;
            }
            *self.controller.0.lock().map_err(|_| fail())? = Some(Inner {
                ledger,
                context: context.clone(),
                available: true,
            });
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                *controller
                    .0
                    .lock()
                    .map_err(|_| PluginError::State("触发器收尾失败".into()))? = None;
                Ok(())
            }))?;
            Ok(None)
        })
    }
}
