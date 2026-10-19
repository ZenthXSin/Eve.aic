//! QQ 宿主的语义记忆召回：语义模型角色的装配、后台建索引与 /semantic 状态命令。
//!
//! 召回上下文在核心装配前组装，此时配置服务尚未启动；向量模型因此经 `DeferredEmbeddings`
//! 延后注入。宿主在通道开始收消息前读取配置并注入，配置无效时拒绝启动。
use crate::{AppError, config::OPENAI_NAMESPACE};
use eve_config_api::{
    CONFIG_SERVICE_ID, ConfigService, ConfigServiceHandle, MODELS_NAMESPACE, MODELS_SCHEMA_VERSION,
    ModelRole, ModelRolesConfig, SemanticOptions,
};
use eve_llm_openai::OpenAiEmbeddings;
use eve_plugin_api::{PluginError, PluginResult, ServiceId, ServiceRegistry};
use eve_qqbot_plugin::{QqCommandHandler, QqCommandInput};
use eve_semantic_api::{EmbedFuture, EmbeddingError, EmbeddingProfile, EmbeddingProvider};
use eve_semantic_plugin::{BackgroundEmbeddings, IndexError, IndexStepError, Indexer, MAX_ENTRIES};
use std::{
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::watch, task::JoinHandle};

/// 一次建索引请求失败后的等待；不在失败后立即重试。
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// 有待处理条目时两次请求之间的间隔，以及空闲时的检查间隔。
const BUSY_INTERVAL: Duration = Duration::from_millis(250);
const IDLE_INTERVAL: Duration = Duration::from_secs(2);
const DISABLED: &str = "语义召回未启用。";

/// 注入前没有可用模型：不匹配任何索引条目，请求一律失败（召回改用词项召回并标明）。
pub(crate) struct DeferredEmbeddings {
    inner: OnceLock<Arc<dyn EmbeddingProvider>>,
    unset: EmbeddingProfile,
}
impl DeferredEmbeddings {
    pub(crate) fn new() -> Self {
        Self {
            inner: OnceLock::new(),
            unset: EmbeddingProfile {
                model: "unconfigured".into(),
                dimensions: 1,
            },
        }
    }
    pub(crate) fn set(&self, provider: Arc<dyn EmbeddingProvider>) {
        let _ = self.inner.set(provider);
    }
}
impl EmbeddingProvider for DeferredEmbeddings {
    fn profile(&self) -> &EmbeddingProfile {
        self.inner
            .get()
            .map_or(&self.unset, |provider| provider.profile())
    }
    fn embed<'a>(&'a self, inputs: &'a [String]) -> EmbedFuture<'a> {
        match self.inner.get() {
            Some(provider) => provider.embed(inputs),
            None => Box::pin(async { Err(EmbeddingError::Provider) }),
        }
    }
}

/// 查询计时与可选的前台等待上限。后台查询、后台建索引沿用原模型超时。
struct QueryEmbeddings {
    inner: Arc<dyn EmbeddingProvider>,
    registry: Arc<dyn ServiceRegistry>,
    timing: bool,
    use_query_budget: bool,
}
impl QueryEmbeddings {
    fn settings(&self) -> Result<eve_config_api::ConfigSnapshot, EmbeddingError> {
        let unavailable = || EmbeddingError::Provider;
        let entry = self
            .registry
            .get(&ServiceId::new(CONFIG_SERVICE_ID).map_err(|_| unavailable())?)
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)?;
        let settings = entry
            .value
            .downcast::<ConfigServiceHandle>()
            .map_err(|_| unavailable())?;
        settings
            .0
            .snapshot(OPENAI_NAMESPACE, 1)
            .map_err(|_| unavailable())
    }
    fn budget(&self) -> Result<Duration, EmbeddingError> {
        let snapshot = self.settings()?;
        let millis: u64 = snapshot
            .get("semantic_query_timeout_ms")
            .map_err(|_| EmbeddingError::Provider)?;
        if !(1..=30_000).contains(&millis) {
            return Err(EmbeddingError::InvalidInput);
        }
        Ok(Duration::from_millis(millis))
    }
}

/// 宿主组合层热选择：默认后台精确查询缓存，显式关闭时沿用有限等待模式。
pub(crate) struct QueryRecallEmbeddings {
    foreground: Arc<QueryEmbeddings>,
    background: BackgroundEmbeddings,
}
impl QueryRecallEmbeddings {
    pub(crate) fn new(
        inner: Arc<dyn EmbeddingProvider>,
        registry: Arc<dyn ServiceRegistry>,
        timing: bool,
    ) -> Self {
        let foreground = Arc::new(QueryEmbeddings {
            inner: inner.clone(),
            registry: registry.clone(),
            timing,
            use_query_budget: true,
        });
        let background = BackgroundEmbeddings::new(Arc::new(QueryEmbeddings {
            inner,
            registry,
            timing,
            use_query_budget: false,
        }));
        Self {
            foreground,
            background,
        }
    }
    pub(crate) async fn close(&self) -> Result<(), EmbeddingError> {
        self.background.close().await
    }
}
impl EmbeddingProvider for QueryRecallEmbeddings {
    fn profile(&self) -> &EmbeddingProfile {
        self.foreground.profile()
    }
    fn embed<'a>(&'a self, inputs: &'a [String]) -> EmbedFuture<'a> {
        Box::pin(async move {
            let background: bool = self
                .foreground
                .settings()?
                .get("semantic_query_background")
                .map_err(|_| EmbeddingError::Provider)?;
            if background {
                self.background.embed(inputs).await
            } else {
                self.foreground.embed(inputs).await
            }
        })
    }
}
impl EmbeddingProvider for QueryEmbeddings {
    fn profile(&self) -> &EmbeddingProfile {
        self.inner.profile()
    }
    fn embed<'a>(&'a self, inputs: &'a [String]) -> EmbedFuture<'a> {
        Box::pin(async move {
            let budget = self.use_query_budget.then(|| self.budget()).transpose()?;
            let mut trace = QueryTiming::new(self.timing, budget);
            let request = self.inner.embed(inputs);
            let result = match budget {
                Some(budget) => tokio::time::timeout(budget, request)
                    .await
                    .unwrap_or(Err(EmbeddingError::Timeout)),
                None => request.await,
            };
            trace.finish(match &result {
                Ok(_) => "embedding_ready",
                Err(EmbeddingError::Timeout) => "embedding_timeout",
                Err(_) => "embedding_failed",
            });
            result
        })
    }
}

static QUERY_SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct QueryTiming {
    started: Option<Instant>,
    query: u64,
    budget_ms: Option<u128>,
}
impl QueryTiming {
    fn new(enabled: bool, budget: Option<Duration>) -> Self {
        let trace = Self {
            started: enabled.then(Instant::now),
            query: if enabled {
                QUERY_SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1
            } else {
                0
            },
            budget_ms: budget.map(|value| value.as_millis()),
        };
        trace.emit("embedding_begin");
        trace
    }
    fn emit(&self, phase: &str) {
        if let Some(started) = self.started {
            eprintln!(
                "EVE_RECALL_TIMING {}",
                serde_json::json!({
                    "query": self.query, "phase": phase, "at_ms": now_ms(),
                    "elapsed_ms": started.elapsed().as_millis(), "budget_ms": self.budget_ms,
                })
            );
        }
    }
    fn finish(&mut self, phase: &str) {
        self.emit(phase);
        self.started = None;
    }
}
impl Drop for QueryTiming {
    fn drop(&mut self) {
        self.emit("embedding_cancelled");
    }
}

/// 读取语义模型角色：须启用 embedding、提供者为 openai。
/// 默认沿用主模型端点和凭据；可显式选择语义端点及宿主注入的独立凭据。
/// 两个配置命名空间须来自同一配置修订；语义提供者在启动时捕获，不随主模型热修改。
pub(crate) fn configured(
    settings: &dyn ConfigService,
    api_key: &str,
    semantic_api_key: Option<&str>,
) -> Result<OpenAiEmbeddings, AppError> {
    for _ in 0..4 {
        let provider = settings.snapshot(OPENAI_NAMESPACE, 1)?;
        let roles = settings.snapshot(MODELS_NAMESPACE, MODELS_SCHEMA_VERSION)?;
        if provider.revision != roles.revision {
            continue;
        }
        let roles = ModelRolesConfig::try_from(&roles)?;
        let profile = roles
            .require(ModelRole::Semantic)
            .map_err(|_| "语义召回需要启用语义模型角色（EVE_MODELS_SEMANTIC_ENABLED 等）")?;
        let Some(SemanticOptions::Embedding { dimensions }) = profile.semantic else {
            return Err("语义召回需要语义模型角色的 operation 为 embedding，并给出维度".into());
        };
        if profile.provider != "openai" {
            return Err("语义模型角色的提供者须为 openai".into());
        }
        let key = match profile.credential_ref.as_deref() {
            None | Some("env:EVE_OPENAI_API_KEY") => api_key,
            Some("env:EVE_SEMANTIC_API_KEY") => semantic_api_key
                .filter(|key| !key.trim().is_empty())
                .ok_or("已选择独立语义凭据，但 EVE_SEMANTIC_API_KEY 为空")?,
            Some(_) => return Err("语义模型只支持宿主提供的主模型或独立语义凭据引用".into()),
        };
        let semantic_base: String = provider.get("semantic_base_url")?;
        let base = if semantic_base.is_empty() {
            provider.get("base_url")?
        } else {
            semantic_base
        };
        return OpenAiEmbeddings::new(
            &base,
            EmbeddingProfile {
                model: profile.model.clone(),
                dimensions,
            },
            key,
            Duration::from_millis(profile.timeout_ms),
        )
        .map_err(|_| "语义模型配置无效：模型、维度、端点或凭据不符合要求".into());
    }
    Err("模型配置持续变化，请在配置稳定后重新启动。".into())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(1)
}

/// 后台建索引的进展；只在内存中，重启后从索引账本与记忆重新计算。
#[derive(Clone, Default)]
struct Report {
    requests: u64,
    failures: u64,
    last_failure_at_ms: Option<u64>,
    full: bool,
}

pub(crate) struct Background {
    active: watch::Sender<bool>,
    stop: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
    task: JoinHandle<Result<(), AppError>>,
    indexer: Arc<Indexer>,
    report: Arc<Mutex<Report>>,
}
impl Background {
    pub(crate) fn start(indexer: Indexer) -> Self {
        let indexer = Arc::new(indexer);
        let report = Arc::new(Mutex::new(Report::default()));
        let (active, activated) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let (finished_sender, finished) = watch::channel(false);
        let task = {
            let (indexer, report) = (indexer.clone(), report.clone());
            tokio::spawn(async move {
                let result = run(indexer, report, activated, stopped).await;
                let _ = finished_sender.send(true);
                result
            })
        };
        Self {
            active,
            stop,
            finished,
            task,
            indexer,
            report,
        }
    }
    pub(crate) fn activate(&self) {
        let _ = self.active.send(true);
    }
    pub(crate) fn finished(&self) -> watch::Receiver<bool> {
        self.finished.clone()
    }
    pub(crate) fn request_stop(&self) {
        let _ = self.stop.send(true);
    }
    pub(crate) async fn stop(self) -> Result<(), AppError> {
        self.request_stop();
        self.task
            .await
            .map_err(|_| "语义索引后台异常；已停止准入")?
    }
    pub(crate) fn commands(&self) -> Arc<dyn QqCommandHandler> {
        Arc::new(Commands {
            enabled: Some((self.indexer.clone(), self.report.clone())),
        })
    }
}

async fn stop_requested(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

async fn run(
    indexer: Arc<Indexer>,
    report: Arc<Mutex<Report>>,
    mut active: watch::Receiver<bool>,
    mut stopped: watch::Receiver<bool>,
) -> Result<(), AppError> {
    loop {
        if *active.borrow() {
            break;
        }
        tokio::select! {
            biased;
            _ = stop_requested(&mut stopped) => return Ok(()),
            changed = active.changed() => if changed.is_err() { return Ok(()); },
        }
    }
    loop {
        if *stopped.borrow() {
            return Ok(());
        }
        let wait = {
            let step = indexer.step(now_ms());
            tokio::pin!(step);
            let result = tokio::select! {
                biased;
                _ = stop_requested(&mut stopped) => return Ok(()),
                result = &mut step => result,
            };
            let mut state = report.lock().map_err(|_| "语义索引状态不可用")?;
            match result {
                Ok(0) => IDLE_INTERVAL,
                Ok(_) => {
                    state.requests += 1;
                    BUSY_INTERVAL
                }
                // 向量请求失败不写入索引；等待后再试，状态可用 /semantic 查看。
                Err(IndexStepError::Embedding(_)) => {
                    state.requests += 1;
                    state.failures += 1;
                    state.last_failure_at_ms = Some(now_ms());
                    RETRY_AFTER
                }
                // 索引已满：保留原记录，不再新增，状态可用 /semantic 查看。
                Err(IndexStepError::Index(IndexError::LimitReached)) => {
                    state.full = true;
                    IDLE_INTERVAL * 15
                }
                // 记忆或索引读写失败：停止后台，宿主随之停止准入并保留状态。
                Err(IndexStepError::Index(error)) => return Err(error.to_string().into()),
                Err(IndexStepError::Memory) => return Err("语义索引无法读取交互记忆".into()),
            }
        };
        tokio::select! {
            biased;
            _ = stop_requested(&mut stopped) => return Ok(()),
            _ = tokio::time::sleep(wait) => {},
        }
    }
}

/// /semantic：语义索引的进度与最近的失败；不显示正文、范围或向量。
pub(crate) struct Commands {
    enabled: Option<(Arc<Indexer>, Arc<Mutex<Report>>)>,
}
impl Commands {
    pub(crate) fn disabled() -> Arc<dyn QqCommandHandler> {
        Arc::new(Self { enabled: None })
    }
}
impl QqCommandHandler for Commands {
    fn handle(&self, input: QqCommandInput<'_>) -> PluginResult<Option<String>> {
        if input.text.trim() != "/semantic" {
            return Ok(None);
        }
        let Some((indexer, report)) = &self.enabled else {
            return Ok(Some(DISABLED.into()));
        };
        let status = indexer
            .status()
            .map_err(|_| PluginError::State("语义索引暂时不可用".into()))?;
        let report = report
            .lock()
            .map_err(|_| PluginError::State("语义索引状态不可用".into()))?
            .clone();
        let mut reply = format!(
            "语义召回已启用：已建索引 {} 条，待处理 {} 条（全部用户合计，索引上限 {} 条）。本次启动发出建索引请求 {} 次，失败 {} 次。",
            status.indexed, status.pending, MAX_ENTRIES, report.requests, report.failures
        );
        if report.last_failure_at_ms.is_some() {
            reply.push_str(
                " 最近一次建索引请求失败，30 秒后再试；召回期间改用词项召回并在资料中标明。",
            );
        }
        if report.full {
            reply.push_str(" 索引已满，保留原记录，不再新增。");
        }
        Ok(Some(reply))
    }
}
