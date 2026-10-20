//! 对话历史摘要的组合：热配置、对话宿主查找与后台增量生成。
//!
//! 摘要只在历史窗口省略较早轮次时使用；原始会话历史始终完整保存。后台每次只处理一个作用域，
//! 失败不重试同一批，等该作用域下一轮对话再次登记；结局写到标准错误的 `EVE_HISTORY_DIGEST` 行。
use crate::AppError;
use eve_config_api::{ConfigField, ConfigKind, ConfigSchema, ConfigService};
use eve_history_digest::{
    DigestError, DigestOptions, DigestPolicy, HistoryDigestService, HistoryDigester, generate,
};
use eve_llm_api::LlmError;
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::watch, task::JoinHandle};

pub(crate) const NAMESPACE: &str = "runtime.history_digest";
pub(crate) const MODEL_USE: &str = "digest";
/// 模型请求失败后暂停新请求的时间。
const PROVIDER_COOLDOWN: Duration = Duration::from_secs(60);

pub(crate) fn schema() -> ConfigSchema {
    let integer = |default, min, max, environment: &str| {
        let mut field = ConfigField::new(
            ConfigKind::Integer {
                minimum: Some(min),
                maximum: Some(max),
            },
            Some(json!(default)),
        );
        field.environment = Some(environment.into());
        field
    };
    let mut enabled = ConfigField::new(ConfigKind::Boolean, Some(json!(true)));
    enabled.environment = Some("EVE_HISTORY_DIGEST_ENABLED".into());
    ConfigSchema {
        namespace: NAMESPACE.into(),
        version: 1,
        fields: BTreeMap::from([
            ("enabled".into(), enabled),
            (
                "min_uncovered_turns".into(),
                integer(4, 1, 64, "EVE_HISTORY_DIGEST_MIN_UNCOVERED_TURNS"),
            ),
            (
                "max_input_bytes".into(),
                integer(49152, 4096, 262144, "EVE_HISTORY_DIGEST_MAX_INPUT_BYTES"),
            ),
        ]),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|v| u64::try_from(v.as_millis()).ok())
        .unwrap_or(1)
}

/// 配置服务在核心安装后才可用；之前一律视为关闭。
#[derive(Default)]
pub(crate) struct Policy {
    pub(crate) settings: OnceLock<Arc<dyn ConfigService>>,
}

impl Policy {
    fn read(&self) -> Result<Option<DigestOptions>, AppError> {
        let Some(settings) = self.settings.get() else {
            return Ok(None);
        };
        let snapshot = settings.snapshot(NAMESPACE, 1)?;
        if !snapshot.get::<bool>("enabled")? {
            return Ok(None);
        }
        Ok(Some(DigestOptions {
            min_uncovered_turns: snapshot.get("min_uncovered_turns")?,
            max_input_bytes: snapshot.get("max_input_bytes")?,
        }))
    }
}

pub(crate) struct SharedPolicy(pub(crate) Arc<Policy>);

impl DigestPolicy for SharedPolicy {
    fn options(&self) -> Result<Option<DigestOptions>, LlmError> {
        self.0
            .read()
            .map_err(|_| LlmError::Configuration("历史摘要配置无法读取".into()))
    }
}

fn report(outcome: &str, scope: &eve_llm_api::ContextScope, detail: serde_json::Value) {
    eprintln!(
        "EVE_HISTORY_DIGEST {}",
        json!({"outcome": outcome, "session_id": scope.session_id, "user_id": scope.user_id,
            "at_ms": now_ms(), "detail": detail})
    );
}

pub(crate) struct Background {
    active: watch::Sender<bool>,
    stop: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
    task: JoinHandle<()>,
}

impl Background {
    pub(crate) fn start(
        service: Arc<HistoryDigestService>,
        digester: Arc<dyn HistoryDigester>,
        policy: Arc<Policy>,
    ) -> Self {
        let (active, activated) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let (finished_sender, finished) = watch::channel(false);
        let task = tokio::spawn(async move {
            run(service, digester, policy, activated, stopped).await;
            let _ = finished_sender.send(true);
        });
        Self {
            active,
            stop,
            finished,
            task,
        }
    }
    pub(crate) fn activate(&self) {
        let _ = self.active.send(true);
    }
    pub(crate) fn request_stop(&self) {
        let _ = self.stop.send(true);
    }
    pub(crate) fn finished(&self) -> watch::Receiver<bool> {
        self.finished.clone()
    }
    pub(crate) async fn stop(self) -> Result<(), AppError> {
        self.request_stop();
        self.task
            .await
            .map_err(|_| "历史摘要后台异常；原始历史不受影响".into())
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
    service: Arc<HistoryDigestService>,
    digester: Arc<dyn HistoryDigester>,
    policy: Arc<Policy>,
    mut active: watch::Receiver<bool>,
    mut stopped: watch::Receiver<bool>,
) {
    while !*active.borrow_and_update() {
        tokio::select! { biased; _ = stop_requested(&mut stopped) => return, changed = active.changed() => if changed.is_err() { return; } }
    }
    let mut paused_until: Option<Instant> = None;
    loop {
        if *stopped.borrow() {
            return;
        }
        let ready = paused_until.is_none_or(|until| Instant::now() >= until);
        // 热停用在下一项生成前读取；读取失败按关闭处理，查找侧同样退回省略提示。
        let options = policy.read().ok().flatten();
        if let (true, Some(options)) = (ready, options)
            && let Some(pending) = service.take_pending()
        {
            let result = tokio::select! { biased;
                _ = stop_requested(&mut stopped) => Err(DigestError::Cancelled),
                result = generate(&service, digester.as_ref(), &pending, options, now_ms()) => result,
            };
            match result {
                Ok(Some(version)) => report(
                    "saved",
                    &pending.scope,
                    json!({"covered_turns": version.covered_turns, "points": version.points.len()}),
                ),
                Ok(None) => {}
                Err(DigestError::Cancelled) => return,
                Err(error) => {
                    if matches!(error, DigestError::Provider | DigestError::Timeout) {
                        paused_until = Some(Instant::now() + PROVIDER_COOLDOWN);
                    }
                    report("failed", &pending.scope, json!(error.to_string()));
                }
            }
            continue;
        }
        tokio::select! { biased; _ = stop_requested(&mut stopped) => return, _ = tokio::time::sleep(Duration::from_millis(200)) => {} }
    }
}
