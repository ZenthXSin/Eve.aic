//! 条件账本与取得流程：先把来源取到暂存目录，核对大小与摘要，交给宿主登记的验证器实际使用一次，
//! 全部通过后才移入条件目录并写账本；账本写盘成功才替换内存。失败的暂存文件删除，只记原因。
//! 账本损坏或格式未知时拒绝启动并保留原字节；条件目录里账本没有引用的文件不会被自动删除。
mod fetch;

use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
use eve_provision_api::*;
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const FORMAT_VERSION: u32 = 1;
const STAGING: &str = ".staging";

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ready: Option<Acquisition>,
    #[serde(default)]
    attempts: Vec<AttemptRecord>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    format_version: u32,
    conditions: BTreeMap<String, Entry>,
}

fn valid_acquisition(id: &str, a: &Acquisition) -> bool {
    a.condition == id
        && is_sha256(&a.sha256)
        && a.path
            == format!(
                "{id}/{}/{}",
                a.sha256,
                a.path.rsplit('/').next().unwrap_or("")
            )
        && a.path.rsplit('/').next().is_some_and(is_file_name)
        && a.source.text().len() <= MAX_SOURCE_BYTES
        && a.evidence.len() <= MAX_EVIDENCE_BYTES
        && a.reason.len() <= MAX_REASON_BYTES
}

fn valid(ledger: &Ledger) -> bool {
    ledger.format_version == FORMAT_VERSION
        && ledger.conditions.iter().all(|(id, entry)| {
            is_condition_id(id)
                && entry
                    .ready
                    .as_ref()
                    .is_none_or(|a| valid_acquisition(id, a))
                && entry.attempts.len() <= MAX_ATTEMPTS_KEPT
                && entry.attempts.iter().all(|attempt| {
                    attempt.source.text().len() <= MAX_SOURCE_BYTES
                        && attempt.detail.len() <= MAX_EVIDENCE_BYTES
                })
        })
}

struct Registered {
    spec: ConditionSpec,
    verifier: Arc<dyn ConditionVerifier>,
}

struct Inner {
    ledger: Ledger,
    context: Option<PluginContext>,
}

struct Stored {
    inner: Mutex<Inner>,
}
impl Stored {
    fn open(context: PluginContext) -> ProvisionResult<Self> {
        let ledger = match context
            .state_get(PROVISION_STATE_KEY)
            .map_err(|_| ProvisionError::Storage)?
        {
            None => Ledger {
                format_version: FORMAT_VERSION,
                conditions: BTreeMap::new(),
            },
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(ProvisionError::CorruptState);
                }
                let value: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(|_| ProvisionError::CorruptState)?;
                match value.get("format_version").and_then(|v| v.as_u64()) {
                    Some(v) if v == u64::from(FORMAT_VERSION) => {}
                    Some(_) => return Err(ProvisionError::UnsupportedVersion),
                    None => return Err(ProvisionError::CorruptState),
                }
                let ledger: Ledger =
                    serde_json::from_value(value).map_err(|_| ProvisionError::CorruptState)?;
                if !valid(&ledger) {
                    return Err(ProvisionError::CorruptState);
                }
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
    fn close(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.context = None;
        }
    }
    /// 在副本上修改，写盘成功后才替换内存；写入结果不能确认时关闭账本。
    fn update<T>(&self, change: impl FnOnce(&mut Ledger) -> T) -> ProvisionResult<T> {
        let mut inner = self.inner.lock().map_err(|_| ProvisionError::Unavailable)?;
        let context = inner.context.clone().ok_or(ProvisionError::Unavailable)?;
        let mut ledger = inner.ledger.clone();
        let result = change(&mut ledger);
        let bytes = serde_json::to_vec(&ledger).map_err(|_| ProvisionError::Storage)?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err(ProvisionError::Storage);
        }
        if context.state_set(PROVISION_STATE_KEY, bytes).is_err() {
            inner.context = None;
            return Err(ProvisionError::Storage);
        }
        inner.ledger = ledger;
        Ok(result)
    }
    fn read<T>(&self, read: impl FnOnce(&Ledger) -> T) -> ProvisionResult<T> {
        let inner = self.inner.lock().map_err(|_| ProvisionError::Unavailable)?;
        inner.context.as_ref().ok_or(ProvisionError::Unavailable)?;
        Ok(read(&inner.ledger))
    }
}

/// 管理能力只交给可信宿主，不发布到通用服务目录。
#[derive(Clone)]
pub struct ProvisionController {
    root: PathBuf,
    registry: Arc<BTreeMap<String, Registered>>,
    active: Arc<Mutex<Option<Arc<Stored>>>>,
    busy: Arc<Mutex<BTreeSet<String>>>,
    fetcher: Arc<fetch::Fetcher>,
    sequence: Arc<AtomicU64>,
}

struct BusyGuard {
    busy: Arc<Mutex<BTreeSet<String>>>,
    id: String,
}
impl Drop for BusyGuard {
    fn drop(&mut self) {
        if let Ok(mut busy) = self.busy.lock() {
            busy.remove(&self.id);
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl ProvisionController {
    fn service(&self) -> ProvisionResult<Arc<Stored>> {
        self.active
            .lock()
            .map_err(|_| ProvisionError::Unavailable)?
            .clone()
            .ok_or(ProvisionError::Unavailable)
    }

    fn record_attempt(
        &self,
        stored: &Stored,
        id: &str,
        source: &AcquireSource,
        at_ms: u64,
        outcome: Result<&Acquisition, &ProvisionError>,
    ) -> ProvisionResult<()> {
        let attempt = AttemptRecord {
            source: source.clone(),
            at_ms,
            ready: outcome.is_ok(),
            detail: match outcome {
                Ok(a) => a.evidence.clone(),
                Err(e) => bounded(&e.to_string(), MAX_EVIDENCE_BYTES),
            },
        };
        stored.update(|ledger| {
            let entry = ledger.conditions.entry(id.to_string()).or_default();
            if let Ok(acquisition) = outcome {
                entry.ready = Some(acquisition.clone());
            }
            entry.attempts.insert(0, attempt);
            entry.attempts.truncate(MAX_ATTEMPTS_KEPT);
        })
    }

    /// 把来源写到暂存文件，边写边计算摘要并限制大小。
    async fn stage(
        &self,
        source: &AcquireSource,
        target: &Path,
        max_bytes: u64,
    ) -> ProvisionResult<(String, u64)> {
        let mut file = tokio::fs::File::create(target)
            .await
            .map_err(|_| ProvisionError::Storage)?;
        let mut digest = Context::new(&SHA256);
        let mut total = 0u64;
        match source {
            AcquireSource::Url(url) => {
                let mut response = self.fetcher.open(url, max_bytes).await?;
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|e| ProvisionError::Fetch(fetch::describe(&e)))?
                {
                    total += chunk.len() as u64;
                    if total > max_bytes {
                        return Err(ProvisionError::TooLarge);
                    }
                    digest.update(&chunk);
                    file.write_all(&chunk)
                        .await
                        .map_err(|_| ProvisionError::Storage)?;
                }
            }
            AcquireSource::LocalFile(path) => {
                let path = Path::new(path);
                if !path.is_absolute() {
                    return Err(ProvisionError::InvalidInput("本机文件须是绝对路径".into()));
                }
                let meta = tokio::fs::metadata(path)
                    .await
                    .map_err(|_| ProvisionError::Fetch("本机文件不存在或无法读取".into()))?;
                if !meta.is_file() {
                    return Err(ProvisionError::Fetch("本机来源不是普通文件".into()));
                }
                if meta.len() > max_bytes {
                    return Err(ProvisionError::TooLarge);
                }
                let mut input = tokio::fs::File::open(path)
                    .await
                    .map_err(|_| ProvisionError::Fetch("本机文件无法读取".into()))?;
                let mut buffer = vec![0u8; 64 * 1024];
                loop {
                    let read = input
                        .read(&mut buffer)
                        .await
                        .map_err(|_| ProvisionError::Fetch("本机文件读取中断".into()))?;
                    if read == 0 {
                        break;
                    }
                    total += read as u64;
                    if total > max_bytes {
                        return Err(ProvisionError::TooLarge);
                    }
                    digest.update(&buffer[..read]);
                    file.write_all(&buffer[..read])
                        .await
                        .map_err(|_| ProvisionError::Storage)?;
                }
            }
        }
        if total == 0 {
            return Err(ProvisionError::Fetch("来源内容为空".into()));
        }
        file.sync_all().await.map_err(|_| ProvisionError::Storage)?;
        Ok((hex(digest.finish().as_ref()), total))
    }

    async fn acquire_inner(
        &self,
        request: &AcquireRequest,
        registered: &Registered,
    ) -> ProvisionResult<Acquisition> {
        let id = &registered.spec.id;
        let file_name = &registered.spec.file_name;
        let staging = self.root.join(STAGING).join(format!(
            "{id}-{}-{}",
            request.now_ms,
            self.sequence.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::fs::create_dir_all(&staging)
            .await
            .map_err(|_| ProvisionError::Storage)?;
        let staged = staging.join(file_name);
        let outcome = async {
            let (sha256, bytes) = self
                .stage(&request.source, &staged, registered.spec.max_bytes)
                .await?;
            if let Some(expected) = &request.expected_sha256
                && *expected != sha256
            {
                return Err(ProvisionError::DigestMismatch { actual: sha256 });
            }
            let evidence = registered
                .verifier
                .verify(&staged)
                .await
                .map_err(|reason| {
                    ProvisionError::VerifyFailed(bounded(&reason, MAX_REASON_BYTES))
                })?;
            let relative = format!("{id}/{sha256}/{file_name}");
            let destination = self.root.join(&relative);
            if let Some(parent) = destination.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|_| ProvisionError::Storage)?;
            }
            tokio::fs::rename(&staged, &destination)
                .await
                .map_err(|_| ProvisionError::Storage)?;
            Ok(Acquisition {
                condition: id.clone(),
                source: request.source.clone(),
                sha256,
                bytes,
                path: relative,
                evidence: bounded(&evidence, MAX_EVIDENCE_BYTES),
                reason: bounded(&request.reason, MAX_REASON_BYTES),
                acquired_at_ms: request.now_ms,
            })
        }
        .await;
        // 暂存目录无论成败都删除；验证通过的文件已移走。
        let _ = tokio::fs::remove_dir_all(&staging).await;
        outcome
    }
}

impl ConditionProvider for ProvisionController {
    fn conditions(&self) -> ProvisionResult<Vec<ConditionView>> {
        let stored = self.service()?;
        stored.read(|ledger| {
            self.registry
                .values()
                .map(|registered| {
                    let entry = ledger
                        .conditions
                        .get(&registered.spec.id)
                        .cloned()
                        .unwrap_or_default();
                    ConditionView {
                        spec: registered.spec.clone(),
                        ready: entry.ready,
                        attempts: entry.attempts,
                    }
                })
                .collect()
        })
    }

    fn ready_path(&self, condition: &str) -> ProvisionResult<Option<PathBuf>> {
        let stored = self.service()?;
        stored.read(|ledger| {
            ledger
                .conditions
                .get(condition)
                .and_then(|entry| entry.ready.as_ref())
                .map(|a| self.root.join(&a.path))
                .filter(|path| path.is_file())
        })
    }

    fn acquire(&self, request: AcquireRequest) -> BoxFuture<'_, ProvisionResult<Acquisition>> {
        Box::pin(async move {
            let registered = self
                .registry
                .get(&request.condition)
                .ok_or(ProvisionError::UnknownCondition)?;
            if request.source.text().is_empty() || request.source.text().len() > MAX_SOURCE_BYTES {
                return Err(ProvisionError::InvalidInput("来源为空或过长".into()));
            }
            if request
                .expected_sha256
                .as_deref()
                .is_some_and(|value| !is_sha256(value))
            {
                return Err(ProvisionError::InvalidInput(
                    "sha256 须为 64 位小写十六进制".into(),
                ));
            }
            let stored = self.service()?;
            {
                let mut busy = self.busy.lock().map_err(|_| ProvisionError::Unavailable)?;
                if !busy.insert(request.condition.clone()) {
                    return Err(ProvisionError::Busy);
                }
            }
            let _guard = BusyGuard {
                busy: self.busy.clone(),
                id: request.condition.clone(),
            };
            let outcome = self.acquire_inner(&request, registered).await;
            // 账本只在验证通过后改为新版本；失败也记下原因，但不影响已启用的版本。
            self.record_attempt(
                &stored,
                &request.condition,
                &request.source,
                request.now_ms,
                outcome.as_ref(),
            )?;
            outcome
        })
    }
}

pub struct ProvisionPlugin {
    manifest: PluginManifest,
    controller: ProvisionController,
}
impl ProvisionPlugin {
    /// `root` 是条件目录；`conditions` 是宿主登记的条件与各自的验证器。
    pub fn new(
        root: PathBuf,
        conditions: Vec<(ConditionSpec, Arc<dyn ConditionVerifier>)>,
    ) -> PluginResult<Self> {
        let mut registry = BTreeMap::new();
        for (spec, verifier) in conditions {
            if !is_condition_id(&spec.id) || !is_file_name(&spec.file_name) || spec.max_bytes == 0 {
                return Err(PluginError::State(format!("条件登记无效：{}", spec.id)));
            }
            if registry
                .insert(spec.id.clone(), Registered { spec, verifier })
                .is_some()
            {
                return Err(PluginError::State("条件重复登记".into()));
            }
        }
        Ok(Self {
            manifest: PluginManifest::new(PROVISION_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: ProvisionController {
                root,
                registry: Arc::new(registry),
                active: Arc::new(Mutex::new(None)),
                busy: Arc::new(Mutex::new(BTreeSet::new())),
                fetcher: Arc::new(
                    fetch::Fetcher::new().map_err(|e| PluginError::State(e.to_string()))?,
                ),
                sequence: Arc::new(AtomicU64::new(0)),
            },
        })
    }
    pub fn controller(&self) -> ProvisionController {
        self.controller.clone()
    }
}
impl Plugin for ProvisionPlugin {
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
                to_close.close();
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("条件账本句柄不可用".into()))?;
                if active.as_ref().is_some_and(|v| Arc::ptr_eq(v, &to_close)) {
                    *active = None;
                }
                Ok(())
            }))?;
            *self
                .controller
                .active
                .lock()
                .map_err(|_| PluginError::State("条件账本句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
