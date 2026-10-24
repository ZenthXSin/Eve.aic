//! 约定账本：先写盘再更新内存；写入结果不能确认时关闭账本，重启后以磁盘为准。
//! 损坏或未知格式拒绝启动并保留原字节，不自动清空。
use eve_commitment_api::*;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    format_version: u32,
    /// 已分配的最大编号。
    next: u64,
    commitments: Vec<Commitment>,
}

fn numbered(id: &str, next: u64) -> bool {
    id.strip_prefix('c')
        .and_then(|n| n.parse::<u64>().ok())
        .is_some_and(|n| n > 0 && n <= next && id == format!("c{n}"))
}

fn valid(ledger: &Ledger) -> bool {
    let mut ids = BTreeSet::new();
    let mut sources = BTreeSet::new();
    let mut open: BTreeMap<&str, usize> = BTreeMap::new();
    ledger.commitments.len() <= MAX_COMMITMENTS
        && ledger.commitments.iter().all(|c| {
            let settled = match c.status {
                CommitmentStatus::Open => {
                    *open.entry(&c.owner).or_default() += 1;
                    c.settled_at_ms.is_none() && c.note.is_none()
                }
                CommitmentStatus::Done | CommitmentStatus::Dropped => {
                    c.settled_at_ms.is_some_and(|at| at >= c.created_at_ms)
                }
            };
            settled
                && numbered(&c.id, ledger.next)
                && ids.insert(c.id.as_str())
                && sources.insert((c.owner.as_str(), c.source.as_str()))
                && validate_owner(&c.owner).is_ok()
                && validate_owner(&c.source).is_ok()
                && validate_text(&c.text).is_ok()
                && c.note.as_deref().is_none_or(|n| validate_text(n).is_ok())
        })
        && open.values().all(|&n| n <= MAX_OPEN_PER_OWNER)
}

struct Inner {
    ledger: Ledger,
    context: Option<PluginContext>,
}
struct Stored(Mutex<Inner>);
impl Stored {
    fn open(context: PluginContext) -> CommitmentResult<Self> {
        let ledger = match context
            .state_get(COMMITMENT_STATE_KEY)
            .map_err(|_| CommitmentError::Storage)?
        {
            None => Ledger {
                format_version: FORMAT_VERSION,
                next: 0,
                commitments: vec![],
            },
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(CommitmentError::CorruptState);
                }
                let value: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(|_| CommitmentError::CorruptState)?;
                match value.get("format_version").and_then(|v| v.as_u64()) {
                    Some(v) if v == u64::from(FORMAT_VERSION) => {}
                    Some(_) => return Err(CommitmentError::UnsupportedVersion),
                    None => return Err(CommitmentError::CorruptState),
                }
                let ledger: Ledger =
                    serde_json::from_value(value).map_err(|_| CommitmentError::CorruptState)?;
                if !valid(&ledger) {
                    return Err(CommitmentError::CorruptState);
                }
                ledger
            }
        };
        Ok(Self(Mutex::new(Inner {
            ledger,
            context: Some(context),
        })))
    }
    fn close(&self) -> PluginResult<()> {
        self.0
            .lock()
            .map_err(|_| PluginError::State(CommitmentError::Unavailable.to_string()))?
            .context = None;
        Ok(())
    }
    /// 在副本上修改，写盘成功后才替换内存。
    fn update<T>(
        &self,
        change: impl FnOnce(&mut Ledger) -> CommitmentResult<T>,
    ) -> CommitmentResult<T> {
        let mut inner = self.0.lock().map_err(|_| CommitmentError::Unavailable)?;
        let context = inner.context.clone().ok_or(CommitmentError::Unavailable)?;
        let mut ledger = inner.ledger.clone();
        let result = change(&mut ledger)?;
        let bytes = serde_json::to_vec(&ledger).map_err(|_| CommitmentError::Storage)?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err(CommitmentError::LimitReached);
        }
        if context.state_set(COMMITMENT_STATE_KEY, bytes).is_err() {
            inner.context = None;
            return Err(CommitmentError::Storage);
        }
        inner.ledger = ledger;
        Ok(result)
    }
    fn read<T>(&self, read: impl FnOnce(&Ledger) -> T) -> CommitmentResult<T> {
        let inner = self.0.lock().map_err(|_| CommitmentError::Unavailable)?;
        inner.context.as_ref().ok_or(CommitmentError::Unavailable)?;
        Ok(read(&inner.ledger))
    }
}

/// 管理能力只交给可信宿主，不发布到通用服务目录。
#[derive(Clone, Default)]
pub struct CommitmentController {
    active: Arc<Mutex<Option<Arc<Stored>>>>,
}
impl CommitmentController {
    fn service(&self) -> CommitmentResult<Arc<Stored>> {
        self.active
            .lock()
            .map_err(|_| CommitmentError::Unavailable)?
            .clone()
            .ok_or(CommitmentError::Unavailable)
    }
}
impl CommitmentLedger for CommitmentController {
    fn record(
        &self,
        owner: &str,
        text: &str,
        source: &str,
        now_ms: u64,
    ) -> CommitmentResult<Commitment> {
        validate_owner(owner)?;
        validate_owner(source)?;
        validate_text(text)?;
        self.service()?.update(|ledger| {
            if let Some(existing) = ledger
                .commitments
                .iter()
                .find(|c| c.owner == owner && c.source == source)
            {
                return Ok(existing.clone());
            }
            let open = ledger
                .commitments
                .iter()
                .filter(|c| c.owner == owner && c.status == CommitmentStatus::Open)
                .count();
            if open >= MAX_OPEN_PER_OWNER {
                return Err(CommitmentError::LimitReached);
            }
            if ledger.commitments.len() >= MAX_COMMITMENTS {
                // 只淘汰最早了结的记录；未完成的约定一条也不丢。
                let oldest = ledger
                    .commitments
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.status != CommitmentStatus::Open)
                    .min_by_key(|(_, c)| (c.settled_at_ms, c.created_at_ms))
                    .map(|(i, _)| i)
                    .ok_or(CommitmentError::LimitReached)?;
                ledger.commitments.remove(oldest);
            }
            ledger.next = ledger
                .next
                .checked_add(1)
                .ok_or(CommitmentError::LimitReached)?;
            let commitment = Commitment {
                id: format!("c{}", ledger.next),
                owner: owner.into(),
                text: text.into(),
                source: source.into(),
                status: CommitmentStatus::Open,
                created_at_ms: now_ms,
                settled_at_ms: None,
                note: None,
            };
            ledger.commitments.push(commitment.clone());
            Ok(commitment)
        })
    }
    fn settle(
        &self,
        owners: &[String],
        id: &str,
        status: CommitmentStatus,
        note: Option<&str>,
        now_ms: u64,
    ) -> CommitmentResult<Commitment> {
        if status == CommitmentStatus::Open {
            return Err(CommitmentError::InvalidInput);
        }
        if let Some(note) = note {
            validate_text(note)?;
        }
        self.service()?.update(|ledger| {
            let commitment = ledger
                .commitments
                .iter_mut()
                .find(|c| c.id == id && owners.contains(&c.owner))
                .ok_or(CommitmentError::NotFound)?;
            if commitment.status != CommitmentStatus::Open {
                return Err(CommitmentError::AlreadySettled);
            }
            commitment.status = status;
            commitment.settled_at_ms = Some(now_ms.max(commitment.created_at_ms));
            commitment.note = note.map(Into::into);
            Ok(commitment.clone())
        })
    }
    fn list(&self, owners: &[String]) -> CommitmentResult<Vec<Commitment>> {
        self.service()?.read(|ledger| {
            let mut found: Vec<_> = ledger
                .commitments
                .iter()
                .filter(|c| owners.contains(&c.owner))
                .cloned()
                .collect();
            found.sort_by(|a, b| b.created_at_ms.cmp(&a.created_at_ms).then(b.id.cmp(&a.id)));
            found
        })
    }
    fn owners(&self) -> CommitmentResult<Vec<String>> {
        self.service()?.read(|ledger| {
            let owners: BTreeSet<_> = ledger.commitments.iter().map(|c| c.owner.clone()).collect();
            owners.into_iter().collect()
        })
    }
}

pub struct CommitmentPlugin {
    manifest: PluginManifest,
    controller: CommitmentController,
}
impl CommitmentPlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(COMMITMENT_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: CommitmentController::default(),
        })
    }
    pub fn controller(&self) -> CommitmentController {
        self.controller.clone()
    }
}
impl Plugin for CommitmentPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                Stored::open(context.clone())
                    .map_err(|error| PluginError::State(error.to_string()))?,
            );
            let to_close = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("约定账本句柄不可用".into()))?;
                if active.as_ref().is_some_and(|v| Arc::ptr_eq(v, &to_close)) {
                    *active = None;
                }
                Ok(())
            }))?;
            *self
                .controller
                .active
                .lock()
                .map_err(|_| PluginError::State("约定账本句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
