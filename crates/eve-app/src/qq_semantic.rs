//! QQ 宿主的语义记忆召回：语义模型角色的装配、后台建索引与 /semantic 状态命令。
//!
//! 召回上下文在核心装配前组装，此时配置服务尚未启动；向量模型因此经 `DeferredEmbeddings`
//! 延后注入。宿主在通道开始收消息前读取配置并注入，配置无效时拒绝启动。
use crate::{AppError, config::OPENAI_NAMESPACE};
use eve_config_api::{
    ConfigService, MODELS_NAMESPACE, MODELS_SCHEMA_VERSION, ModelRole, ModelRolesConfig,
    SemanticOptions,
};
use eve_llm_openai::OpenAiEmbeddings;
use eve_plugin_api::{PluginError, PluginResult};
use eve_qqbot_plugin::{QqCommandHandler, QqCommandInput};
use eve_semantic_api::{EmbedFuture, EmbeddingError, EmbeddingProfile, EmbeddingProvider};
use eve_semantic_plugin::{IndexError, IndexStepError, Indexer, MAX_ENTRIES};
use std::{
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
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

/// 读取语义模型角色：须启用 embedding、提供者为 openai，端点沿用主模型的 OpenAI 兼容地址，
/// 凭据沿用 EVE_OPENAI_API_KEY。两个配置命名空间须来自同一配置修订。
pub(crate) fn configured(
    settings: &dyn ConfigService,
    api_key: &str,
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
        if profile.provider != "openai"
            || profile
                .credential_ref
                .as_deref()
                .is_some_and(|reference| reference != "env:EVE_OPENAI_API_KEY")
        {
            return Err("语义模型角色的提供者须为 openai，凭据沿用 EVE_OPENAI_API_KEY".into());
        }
        let base: String = provider.get("base_url")?;
        return OpenAiEmbeddings::new(
            &base,
            EmbeddingProfile {
                model: profile.model.clone(),
                dimensions,
            },
            api_key,
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
