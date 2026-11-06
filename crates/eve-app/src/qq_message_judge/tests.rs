use super::*;
use crate::{config, http_support};
use eve_config_api::{
    ApplyMode, CONFIG_PLUGIN_ID, CONFIG_SERVICE_ID, ConfigAdmin, ConfigRequest, ConfigResult,
    ConfigServiceHandle, NamespaceValues, model_roles_schema,
};
use eve_config_plugin::{ConfigBootstrap, ConfigController, ConfigPlugin};
use eve_control_api::{ControlPhase, GenerationKey};
use eve_kernel::{Kernel, KernelServices};
use eve_message_api::{
    IncomingMessage, MessageIntent, RELATION_PLUGIN_ID, RELATION_SERVICE_ID, RelationServiceHandle,
    message_schema,
};
use eve_plugin_api::{PluginId, ServiceId, ServiceRegistry};
use eve_session_api::SessionKey;
use http_support::{Captured, Reply, Server, final_response};
use serde_json::Value;
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

const PRIMARY_SECRET: &str = "synthetic-primary-only-secret";
const JEV_SECRET: &str = "synthetic-jev-only-secret";

struct Harness {
    kernel: Kernel,
    registry: Arc<dyn ServiceRegistry>,
    admin: ConfigController,
    settings: Arc<dyn ConfigService>,
}

impl Harness {
    async fn start(path: &Path, primary_url: &str, jev_url: &str) -> Self {
        let backends = KernelServices::default();
        let registry = backends.registry.clone();
        let kernel = Kernel::with_services(backends);
        let plugin = ConfigPlugin::new(
            ConfigBootstrap::new(
                path,
                vec![
                    config::openai_schema(),
                    config::model_uses_schema(),
                    provider_schema(),
                    model_roles_schema(),
                    message_schema(),
                ],
            )
            .with_environment(BTreeMap::from([
                (
                    "EVE_OPENAI_BASE_URL".into(),
                    primary_url.trim_end_matches("/responses").into(),
                ),
                ("EVE_OPENAI_PROTOCOL".into(), "responses".into()),
                ("EVE_OPENAI_MODEL_ROLE".into(), "primary".into()),
                ("EVE_MODELS_PRIMARY_ENABLED".into(), "true".into()),
                ("EVE_MODELS_PRIMARY_PROVIDER".into(), "openai".into()),
                ("EVE_MODELS_PRIMARY_MODEL".into(), "primary-a".into()),
                ("EVE_MODELS_PRIMARY_TIMEOUT_MS".into(), "2000".into()),
                (
                    "EVE_JEV_BASE_URL".into(),
                    jev_url.trim_end_matches("/v1/responses").into(),
                ),
                ("EVE_MODELS_JEV_ENABLED".into(), "true".into()),
                ("EVE_MODELS_JEV_PROVIDER".into(), "jev".into()),
                ("EVE_MODELS_JEV_MODEL".into(), "jev-a".into()),
                ("EVE_MODELS_JEV_TIMEOUT_MS".into(), "2000".into()),
                ("EVE_MESSAGE_JUDGE_TIMEOUT_MS".into(), "4000".into()),
            ])),
        )
        .unwrap();
        let admin = plugin.controller();
        kernel.register(Box::new(plugin)).unwrap();
        kernel
            .start(&PluginId::new(CONFIG_PLUGIN_ID).unwrap())
            .await
            .unwrap();
        let settings = registry
            .get(&ServiceId::new(CONFIG_SERVICE_ID).unwrap())
            .unwrap()
            .unwrap()
            .value
            .downcast::<ConfigServiceHandle>()
            .unwrap()
            .0
            .clone();
        Self {
            kernel,
            registry,
            admin,
            settings,
        }
    }

    fn plugin(&self, mode: MessageJudgeMode) -> Result<RelationPlugin, AppError> {
        relation_plugin_with_key_reader(mode, self.settings.clone(), |name| match name {
            PRIMARY_KEY => Ok(PRIMARY_SECRET.into()),
            JEV_KEY => Ok(JEV_SECRET.into()),
            _ => panic!("不能解析任意环境变量"),
        })
    }

    async fn install(&self, plugin: RelationPlugin) -> Arc<dyn RelationJudge> {
        self.kernel.register(Box::new(plugin)).unwrap();
        self.kernel
            .start(&PluginId::new(RELATION_PLUGIN_ID).unwrap())
            .await
            .unwrap();
        self.registry
            .get(&ServiceId::new(RELATION_SERVICE_ID).unwrap())
            .unwrap()
            .unwrap()
            .value
            .downcast::<RelationServiceHandle>()
            .unwrap()
            .0
            .clone()
    }

    fn update(&self, changes: &[(&str, &str, Value)], mode: ApplyMode) {
        let current = self.admin.current().unwrap();
        let mut namespaces = current.namespaces;
        for (namespace, field, value) in changes {
            namespaces
                .entry((*namespace).into())
                .or_insert_with(|| NamespaceValues {
                    schema_version: 1,
                    values: BTreeMap::new(),
                })
                .values
                .insert((*field).into(), value.clone());
        }
        self.admin
            .replace(current.revision, namespaces, mode)
            .unwrap();
    }

    async fn stop(self) {
        self.kernel.stop_all().await.unwrap();
    }
}

fn input(text: &str) -> RelationInput {
    RelationInput {
        message: IncomingMessage {
            message_id: "private-message".into(),
            target: GenerationKey {
                session: SessionKey::new("private-session", "private-user").unwrap(),
                task_id: "private-task".into(),
                controller_epoch: [29; 16],
                generation: 1,
            },
            text: text.into(),
            reply_to: None,
        },
        phase: ControlPhase::Generating,
        task_text: "原任务".into(),
        cancel_requested: false,
        started_tools: Some(0),
        clarification: None,
    }
}

fn jev_reply(confidence: f64) -> Reply {
    let probabilities: BTreeMap<_, _> = [
        "supplement",
        "correction",
        "answer",
        "new_task",
        "cancel",
        "continue",
        "unrelated",
        "ambiguous",
        "pause",
        "resume",
    ]
    .into_iter()
    .map(|label| (label, if label == "correction" { 1.0 } else { 0.0 }))
    .collect();
    Reply::json(json!({"answers": {
        "intent": {"type": "choice", "choice": "correction", "confidence": confidence,
            "probabilities": probabilities},
        "whole_message": {"type": "noul", "noul": 1.0}
    }}))
}

fn primary_reply(text: &str) -> Reply {
    Reply::json(final_response(
        &json!({"parts": [{"intent": "correction", "confidence": 95,
        "text": text}], "explanation": "当前任务的修订。"})
        .to_string(),
    ))
}

fn openrouter_reply() -> Reply {
    let mut body: Value = serde_json::from_slice(&jev_reply(0.99).body).unwrap();
    body["model"] = json!("typesafe/jev-1.13-20260917");
    body["provider"] = json!("TypeSafe");
    body["id"] = json!("synthetic-openrouter-decision");
    body["usage"] = json!({"input_tokens": 100, "output_tokens": 20, "cost": 0.0000042});
    Reply::json(body)
}

#[tokio::test]
async fn openrouter_typesafe_prefix_envelope_fallback_and_restart_use_independent_credentials() {
    let root = tempfile::tempdir().unwrap();
    let text = "把解释改成中文。";
    let mut primary = Server::start(vec![primary_reply(text)]).await;
    let mut unavailable = Reply::json(json!({"error": {"message": "synthetic unavailable"}}));
    unavailable.status = 502;
    let mut jev = Server::start(vec![openrouter_reply(), unavailable, openrouter_reply()]).await;
    let base = format!("{}/api", jev.url.trim_end_matches("/v1/responses"));
    let h = Harness::start(root.path(), &primary.url, &jev.url).await;
    h.update(
        &[
            (PROVIDER_NAMESPACE, "base_url", json!(base)),
            (MODELS_NAMESPACE, "jev_model", json!("typesafe/jev-1.13")),
            (
                MODELS_NAMESPACE,
                "jev_credential_ref",
                json!("env:EVE_JEV_API_KEY"),
            ),
        ],
        ApplyMode::Immediate,
    );
    let judge = h.install(h.plugin(MessageJudgeMode::Jev).unwrap()).await;
    // 启动预检不发送请求；明确命令同样不访问任一模型。
    assert!(primary.requests.try_recv().is_err());
    assert!(jev.requests.try_recv().is_err());
    assert_eq!(
        judge.judge(input("/cancel")).await.unwrap().parts[0].intent,
        MessageIntent::Cancel
    );
    assert!(primary.requests.try_recv().is_err());
    assert!(jev.requests.try_recv().is_err());
    assert_eq!(
        judge.judge(input(text)).await.unwrap().parts[0].confidence,
        99
    );
    let request = jev.next().await;
    assert_request(
        &request,
        "/api/v1/systemone",
        "typesafe/jev-1.13",
        JEV_SECRET,
        PRIMARY_SECRET,
    );
    assert_eq!(request.body["state"]["message"], text);
    assert_eq!(request.body["questions"]["intent"]["type"], "choice");
    assert_eq!(request.body["questions"]["whole_message"]["type"], "noul");
    assert!(request.body.get("messages").is_none());
    assert!(request.body.get("tools").is_none());
    assert!(primary.requests.try_recv().is_err());
    assert_eq!(
        judge.judge(input(text)).await.unwrap().parts[0].confidence,
        95
    );
    assert_request(
        &jev.next().await,
        "/api/v1/systemone",
        "typesafe/jev-1.13",
        JEV_SECRET,
        PRIMARY_SECRET,
    );
    assert_request(
        &primary.next().await,
        "/v1/responses",
        "primary-a",
        PRIMARY_SECRET,
        JEV_SECRET,
    );
    assert!(primary.requests.try_recv().is_err());
    assert!(jev.requests.try_recv().is_err());
    h.stop().await;
    let restored = Harness::start(root.path(), &primary.url, &jev.url).await;
    let judge = restored
        .install(restored.plugin(MessageJudgeMode::Jev).unwrap())
        .await;
    assert_eq!(
        restored
            .settings
            .snapshot(PROVIDER_NAMESPACE, 1)
            .unwrap()
            .get::<String>("base_url")
            .unwrap(),
        base
    );
    // 恢复普通配置，不重放之前的判断或失败请求。
    assert!(primary.requests.try_recv().is_err());
    assert!(jev.requests.try_recv().is_err());
    assert_eq!(
        judge.judge(input(text)).await.unwrap().parts[0].confidence,
        99
    );
    assert_request(
        &jev.next().await,
        "/api/v1/systemone",
        "typesafe/jev-1.13",
        JEV_SECRET,
        PRIMARY_SECRET,
    );
    assert!(primary.requests.try_recv().is_err());
    assert!(jev.requests.try_recv().is_err());
    restored.stop().await;
}

fn assert_request(request: &Captured, path: &str, model: &str, own_key: &str, other_key: &str) {
    assert!(
        request
            .headers
            .starts_with(&format!("POST {path} HTTP/1.1\r\n"))
    );
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {own_key}\r\n"))
    );
    assert!(!request.headers.contains(other_key));
    assert_eq!(request.body["model"], model);
    let body = request.body.to_string();
    for secret in [
        own_key,
        other_key,
        "private-message",
        "private-session",
        "private-user",
        "private-task",
    ] {
        assert!(!body.contains(secret));
    }
}

struct ForbiddenSettings;
impl ConfigService for ForbiddenSettings {
    fn snapshot(&self, _: &str, _: u32) -> ConfigResult<ConfigSnapshot> {
        panic!("关闭时不能读取模型配置")
    }
    fn begin_request(&self, _: &str, _: u32) -> ConfigResult<ConfigRequest> {
        panic!("关闭时不能读取模型配置")
    }
    fn read_request(&self, _: &ConfigRequest) -> ConfigResult<ConfigSnapshot> {
        panic!("关闭时不能读取模型配置")
    }
}

#[tokio::test]
async fn off_mode_reads_neither_configuration_nor_credentials() {
    assert_eq!(MessageJudgeMode::default(), MessageJudgeMode::Off);
    let plugin =
        relation_plugin_with_key_reader(MessageJudgeMode::Off, Arc::new(ForbiddenSettings), |_| {
            panic!("关闭时不能读取任何凭据")
        })
        .unwrap();
    let backends = KernelServices::default();
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    kernel.register(Box::new(plugin)).unwrap();
    kernel
        .start(&PluginId::new(RELATION_PLUGIN_ID).unwrap())
        .await
        .unwrap();
    let judge = registry
        .get(&ServiceId::new(RELATION_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<RelationServiceHandle>()
        .unwrap();
    assert_eq!(
        judge.0.judge(input("/cancel")).await.unwrap().parts[0].intent,
        MessageIntent::Cancel
    );
    assert_eq!(
        judge
            .0
            .judge(input("把解释改成中文。"))
            .await
            .unwrap()
            .parts[0]
            .intent,
        MessageIntent::Ambiguous
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn primary_mode_never_reads_jev_key_and_resolves_each_request() {
    let root = tempfile::tempdir().unwrap();
    let text = "把解释改成中文。";
    let mut primary = Server::start(vec![primary_reply(text), primary_reply(text)]).await;
    let mut jev = Server::start(vec![]).await;
    let h = Harness::start(root.path(), &primary.url, &jev.url).await;
    h.update(
        &[(MODELS_NAMESPACE, "jev_enabled", json!(false))],
        ApplyMode::Immediate,
    );
    let plugin =
        relation_plugin_with_key_reader(MessageJudgeMode::Primary, h.settings.clone(), |name| {
            assert_eq!(name, PRIMARY_KEY);
            Ok(PRIMARY_SECRET.into())
        })
        .unwrap();
    let judge = h.install(plugin).await;
    for (model, mode) in [
        ("primary-a", ApplyMode::Immediate),
        ("primary-b", ApplyMode::NewRequests),
    ] {
        h.update(&[(MODELS_NAMESPACE, "primary_model", json!(model))], mode);
        assert_eq!(
            judge.judge(input(text)).await.unwrap().parts[0].intent,
            MessageIntent::Correction
        );
        assert_request(
            &primary.next().await,
            "/v1/responses",
            model,
            PRIMARY_SECRET,
            JEV_SECRET,
        );
    }
    assert!(jev.requests.try_recv().is_err());
    h.stop().await;
}

#[tokio::test]
async fn jev_chain_keeps_keys_separate_and_falls_back_after_live_configuration_errors() {
    let root = tempfile::tempdir().unwrap();
    let text = "把解释改成中文。";
    let mut primary = Server::start(vec![primary_reply(text), primary_reply(text)]).await;
    let mut jev = Server::start(vec![jev_reply(0.99), jev_reply(0.2), jev_reply(0.99)]).await;
    let h = Harness::start(root.path(), &primary.url, &jev.url).await;
    let judge = h.install(h.plugin(MessageJudgeMode::Jev).unwrap()).await;
    assert_eq!(
        judge.judge(input("/cancel")).await.unwrap().parts[0].intent,
        MessageIntent::Cancel
    );
    assert!(primary.requests.try_recv().is_err());
    assert!(jev.requests.try_recv().is_err());
    assert_eq!(
        judge.judge(input(text)).await.unwrap().parts[0].intent,
        MessageIntent::Correction
    );
    assert_request(
        &jev.next().await,
        "/v1/systemone",
        "jev-a",
        JEV_SECRET,
        PRIMARY_SECRET,
    );
    assert!(primary.requests.try_recv().is_err());
    h.update(
        &[
            (MODELS_NAMESPACE, "jev_model", json!("jev-b")),
            (
                MODELS_NAMESPACE,
                "jev_credential_ref",
                json!("env:EVE_JEV_API_KEY"),
            ),
            (MODELS_NAMESPACE, "primary_model", json!("primary-b")),
        ],
        ApplyMode::NewRequests,
    );
    assert_eq!(
        judge.judge(input(text)).await.unwrap().parts[0].confidence,
        95
    );
    assert_request(
        &jev.next().await,
        "/v1/systemone",
        "jev-b",
        JEV_SECRET,
        PRIMARY_SECRET,
    );
    assert_request(
        &primary.next().await,
        "/v1/responses",
        "primary-b",
        PRIMARY_SECRET,
        JEV_SECRET,
    );
    h.update(
        &[(
            MODELS_NAMESPACE,
            "jev_provider",
            json!("unsupported-private-provider"),
        )],
        ApplyMode::Immediate,
    );
    assert_eq!(
        judge.judge(input(text)).await.unwrap().parts[0].confidence,
        95
    );
    assert_request(
        &primary.next().await,
        "/v1/responses",
        "primary-b",
        PRIMARY_SECRET,
        JEV_SECRET,
    );
    assert!(jev.requests.try_recv().is_err());
    h.update(
        &[(MODELS_NAMESPACE, "jev_provider", json!("jev"))],
        ApplyMode::Immediate,
    );
    h.stop().await;
    assert_eq!(
        judge.judge(input(text)).await,
        Err(RelationError::Unavailable)
    );
    let reopened = Harness::start(root.path(), &primary.url, &jev.url).await;
    let restored = reopened
        .install(reopened.plugin(MessageJudgeMode::Jev).unwrap())
        .await;
    assert!(primary.requests.try_recv().is_err());
    assert!(jev.requests.try_recv().is_err());
    assert_eq!(
        restored.judge(input(text)).await.unwrap().parts[0].intent,
        MessageIntent::Correction
    );
    assert_request(
        &jev.next().await,
        "/v1/systemone",
        "jev-b",
        JEV_SECRET,
        PRIMARY_SECRET,
    );
    assert!(primary.requests.try_recv().is_err());
    let persisted = serde_json::to_string(&reopened.admin.current().unwrap()).unwrap();
    assert!(!persisted.contains(PRIMARY_SECRET));
    assert!(!persisted.contains(JEV_SECRET));
    reopened.stop().await;
}

#[tokio::test]
async fn startup_rejects_unsupported_jev_configuration_without_network_or_secret_output() {
    let root = tempfile::tempdir().unwrap();
    let mut primary = Server::start(vec![]).await;
    let mut jev = Server::start(vec![]).await;
    let h = Harness::start(root.path(), &primary.url, &jev.url).await;
    for (namespace, field, invalid, valid) in [
        (MODELS_NAMESPACE, "jev_enabled", json!(false), json!(true)),
        (
            MODELS_NAMESPACE,
            "jev_provider",
            json!("private-provider-value"),
            json!("jev"),
        ),
        (
            MODELS_NAMESPACE,
            "jev_credential_ref",
            json!("env:PRIVATE_SECRET_VALUE"),
            json!(""),
        ),
        (
            MODELS_NAMESPACE,
            "jev_timeout_ms",
            json!(60001),
            json!(2000),
        ),
        (
            MODELS_NAMESPACE,
            "jev_max_concurrent_requests",
            json!(33),
            json!(1),
        ),
        (
            MODELS_NAMESPACE,
            "jev_max_output_tokens",
            json!(32),
            json!(0),
        ),
        (
            PROVIDER_NAMESPACE,
            "base_url",
            json!("https://private-user:private-password@example.invalid"),
            json!(jev.url.trim_end_matches("/v1/responses")),
        ),
    ] {
        h.update(&[(namespace, field, invalid)], ApplyMode::Immediate);
        let error = h.plugin(MessageJudgeMode::Jev).err().unwrap().to_string();
        for private in [
            "private-provider-value",
            "PRIVATE_SECRET_VALUE",
            "private-user",
            "private-password",
            JEV_SECRET,
            PRIMARY_SECRET,
        ] {
            assert!(!error.contains(private));
        }
        h.update(&[(namespace, field, valid)], ApplyMode::Immediate);
    }
    assert!(
        relation_plugin_with_key_reader(MessageJudgeMode::Jev, h.settings.clone(), |name| {
            Ok(if name == PRIMARY_KEY {
                PRIMARY_SECRET.into()
            } else {
                String::new()
            })
        })
        .is_err()
    );
    assert!(primary.requests.try_recv().is_err());
    assert!(jev.requests.try_recv().is_err());
    h.stop().await;
}

struct ChangingRevision {
    settings: Arc<dyn ConfigService>,
    changing: AtomicBool,
}
impl ConfigService for ChangingRevision {
    fn snapshot(&self, namespace: &str, version: u32) -> ConfigResult<ConfigSnapshot> {
        let mut snapshot = self.settings.snapshot(namespace, version)?;
        if namespace == PROVIDER_NAMESPACE && self.changing.load(Ordering::SeqCst) {
            snapshot.revision += 1;
        }
        Ok(snapshot)
    }
    fn begin_request(&self, namespace: &str, version: u32) -> ConfigResult<ConfigRequest> {
        self.settings.begin_request(namespace, version)
    }
    fn read_request(&self, request: &ConfigRequest) -> ConfigResult<ConfigSnapshot> {
        self.settings.read_request(request)
    }
}

#[tokio::test]
async fn inconsistent_revisions_cannot_reuse_previous_jev_client() {
    let root = tempfile::tempdir().unwrap();
    let mut jev = Server::start(vec![jev_reply(0.99)]).await;
    let h = Harness::start(root.path(), &jev.url, &jev.url).await;
    let settings = Arc::new(ChangingRevision {
        settings: h.settings.clone(),
        changing: AtomicBool::new(false),
    });
    let judge = JevRoleJudge::new(settings.clone(), JEV_SECRET.into());
    judge.resolve().unwrap();
    settings.changing.store(true, Ordering::SeqCst);
    assert_eq!(
        judge.judge(input("把解释改成中文。")).await,
        Err(RelationError::Unavailable)
    );
    assert_eq!(
        eve_developer_api::ActionReviewClassifier::classify(
            &judge,
            eve_developer_api::ActionReviewMode::SystemSafety,
            action_request(),
        )
        .await,
        eve_developer_api::ActionClassification::Uncertain(
            eve_developer_api::ActionReviewUncertainty::Unavailable,
        ),
    );
    assert!(jev.requests.try_recv().is_err());
    settings.changing.store(false, Ordering::SeqCst);
    assert_eq!(
        judge.judge(input("把解释改成中文。")).await.unwrap().parts[0].intent,
        MessageIntent::Correction
    );
    assert_request(
        &jev.next().await,
        "/v1/systemone",
        "jev-a",
        JEV_SECRET,
        PRIMARY_SECRET,
    );
    h.stop().await;
}

fn action_request() -> eve_developer_api::ActionReviewRequest {
    eve_developer_api::ActionReviewRequest {
        model_session: None,
        action: "具体系统影响".into(),
        program: "eve.restart_self".into(),
        arguments: vec!["{\"target\":\"current-running-binary\",\"retain_state\":true}".into()],
        workspace: "/workspace".into(),
        isolated: false,
        cwd: ".".into(),
        policy_reason: "核对具体系统危害".into(),
        claimed_confirmation: false,
        delegated: true,
        user_messages: vec![],
        recent_actions: vec![],
    }
}

fn action_reply() -> Reply {
    Reply::json(json!({"answers":{
        "direct_system_harm":{"type":"choice","choice":"bounded","confidence":0.99,
            "probabilities":{"none":0.0,"bounded":1.0,"high":0.0,"critical":0.0,"uncertain":0.0}},
        "grounded":{"type":"noul","noul":0.99}
    }}))
}

#[tokio::test]
async fn relation_and_action_share_http_slots_and_new_credentials_do_not_reuse_old_client() {
    use eve_developer_api::{
        ActionClassification, ActionReviewClassifier, ActionReviewMode, ActionReviewUncertainty,
    };
    let root = tempfile::tempdir().unwrap();
    let mut slow = jev_reply(0.99);
    slow.header_delay = Duration::from_secs(60);
    let mut jev = Server::start(vec![slow, action_reply(), action_reply()]).await;
    let h = Harness::start(root.path(), &jev.url, &jev.url).await;
    h.update(
        &[
            (MODELS_NAMESPACE, "jev_max_concurrent_requests", json!(1)),
            (MODELS_NAMESPACE, "jev_timeout_ms", json!(60000)),
        ],
        ApplyMode::Immediate,
    );
    let judge = JevRoleJudge::shared(h.settings.clone(), JEV_SECRET.into()).unwrap();
    let action_judge = JevRoleJudge::shared(h.settings.clone(), JEV_SECRET.into()).unwrap();
    assert!(Arc::ptr_eq(&judge, &action_judge));
    let in_flight = {
        let judge = judge.clone();
        tokio::spawn(async move { judge.judge(input("把解释改成中文。")).await })
    };
    jev.next().await;
    assert_eq!(
        action_judge
            .classify(ActionReviewMode::SystemSafety, action_request())
            .await,
        ActionClassification::Uncertain(ActionReviewUncertainty::Busy)
    );
    assert!(jev.requests.try_recv().is_err());
    let replacement_key = "synthetic-replaced-jev-action-secret";
    let replacement = JevRoleJudge::shared(h.settings.clone(), replacement_key.into()).unwrap();
    assert!(!Arc::ptr_eq(&judge, &replacement));
    assert!(!Arc::ptr_eq(
        &judge.resolve().unwrap(),
        &replacement.resolve().unwrap()
    ));
    assert_eq!(
        replacement
            .classify(ActionReviewMode::SystemSafety, action_request())
            .await,
        ActionClassification::Uncertain(ActionReviewUncertainty::Busy)
    );
    assert!(jev.requests.try_recv().is_err());
    in_flight.abort();
    assert!(in_flight.await.unwrap_err().is_cancelled());
    assert!(matches!(
        action_judge
            .classify(ActionReviewMode::SystemSafety, action_request())
            .await,
        ActionClassification::Decided { .. }
    ));
    assert_request(
        &jev.next().await,
        "/v1/systemone",
        "jev-a",
        JEV_SECRET,
        PRIMARY_SECRET,
    );
    assert!(matches!(
        replacement
            .classify(ActionReviewMode::SystemSafety, action_request())
            .await,
        ActionClassification::Decided { .. }
    ));
    assert_request(
        &jev.next().await,
        "/v1/systemone",
        "jev-a",
        replacement_key,
        JEV_SECRET,
    );
    h.stop().await;
}

#[tokio::test]
async fn jev_hot_configuration_keeps_in_flight_admission_across_unrelated_and_provider_updates() {
    use eve_developer_api::{
        ActionClassification, ActionReviewClassifier, ActionReviewMode, ActionReviewUncertainty,
    };
    let root = tempfile::tempdir().unwrap();
    let mut slow = jev_reply(0.99);
    slow.header_delay = Duration::from_secs(60);
    let mut old = Server::start(vec![slow]).await;
    let mut next = Server::start(vec![action_reply()]).await;
    let h = Harness::start(root.path(), &old.url, &old.url).await;
    h.update(
        &[
            (MODELS_NAMESPACE, "jev_max_concurrent_requests", json!(1)),
            (MODELS_NAMESPACE, "jev_timeout_ms", json!(60000)),
        ],
        ApplyMode::Immediate,
    );
    let judge = JevRoleJudge::shared(h.settings.clone(), JEV_SECRET.into()).unwrap();
    let in_flight = {
        let judge = judge.clone();
        tokio::spawn(async move { judge.judge(input("把解释改成中文。")).await })
    };
    assert_request(
        &old.next().await,
        "/v1/systemone",
        "jev-a",
        JEV_SECRET,
        PRIMARY_SECRET,
    );
    h.update(
        &[(
            MODELS_NAMESPACE,
            "primary_model",
            json!("primary-unrelated"),
        )],
        ApplyMode::Immediate,
    );
    assert_eq!(
        judge
            .classify(ActionReviewMode::SystemSafety, action_request())
            .await,
        ActionClassification::Uncertain(ActionReviewUncertainty::Busy)
    );
    assert!(!in_flight.is_finished());
    assert!(old.requests.try_recv().is_err());
    h.update(
        &[
            (MODELS_NAMESPACE, "jev_model", json!("jev-b")),
            (MODELS_NAMESPACE, "jev_timeout_ms", json!(59000)),
            (
                PROVIDER_NAMESPACE,
                "base_url",
                json!(next.url.trim_end_matches("/v1/responses")),
            ),
        ],
        ApplyMode::NewRequests,
    );
    assert_eq!(
        judge
            .classify(ActionReviewMode::SystemSafety, action_request())
            .await,
        ActionClassification::Uncertain(ActionReviewUncertainty::Busy)
    );
    assert!(!in_flight.is_finished());
    assert!(old.requests.try_recv().is_err());
    assert!(next.requests.try_recv().is_err());
    let status = crate::model_concurrency::status(&h.settings, ModelRole::Jev)
        .unwrap()
        .unwrap();
    assert_eq!((status.limit, status.active, status.waiting), (1, 1, 0));
    in_flight.abort();
    assert!(in_flight.await.unwrap_err().is_cancelled());
    assert!(matches!(
        judge
            .classify(ActionReviewMode::SystemSafety, action_request())
            .await,
        ActionClassification::Decided { .. }
    ));
    assert_request(
        &next.next().await,
        "/v1/systemone",
        "jev-b",
        JEV_SECRET,
        PRIMARY_SECRET,
    );
    h.stop().await;
}

#[tokio::test]
async fn jev_invalid_hot_candidate_keeps_limit_and_lowering_blocks_old_and_new_clients() {
    use eve_developer_api::{
        ActionClassification, ActionReviewClassifier, ActionReviewMode, ActionReviewUncertainty,
    };
    let root = tempfile::tempdir().unwrap();
    let slow = || {
        let mut reply = jev_reply(0.99);
        reply.header_delay = Duration::from_secs(60);
        reply
    };
    let mut server = Server::start(vec![slow(), slow(), action_reply(), action_reply()]).await;
    let h = Harness::start(root.path(), &server.url, &server.url).await;
    h.update(
        &[
            (MODELS_NAMESPACE, "jev_max_concurrent_requests", json!(2)),
            (MODELS_NAMESPACE, "jev_timeout_ms", json!(60000)),
        ],
        ApplyMode::Immediate,
    );
    let judge = JevRoleJudge::shared(h.settings.clone(), JEV_SECRET.into()).unwrap();
    let original = judge.resolve().unwrap();
    let first = {
        let judge = judge.clone();
        tokio::spawn(async move { judge.judge(input("把解释改成中文。")).await })
    };
    server.next().await;
    let second = {
        let judge = judge.clone();
        tokio::spawn(async move { judge.judge(input("再补一条解释。")).await })
    };
    server.next().await;
    let before = crate::model_concurrency::status(&h.settings, ModelRole::Jev)
        .unwrap()
        .unwrap();
    assert_eq!((before.limit, before.active), (2, 2));
    h.update(
        &[
            (MODELS_NAMESPACE, "jev_max_concurrent_requests", json!(32)),
            (PROVIDER_NAMESPACE, "base_url", json!("invalid://synthetic")),
        ],
        ApplyMode::Immediate,
    );
    assert_eq!(
        judge
            .classify(ActionReviewMode::SystemSafety, action_request())
            .await,
        ActionClassification::Uncertain(ActionReviewUncertainty::Unavailable)
    );
    assert_eq!(
        crate::model_concurrency::status(&h.settings, ModelRole::Jev)
            .unwrap()
            .unwrap(),
        before
    );
    h.update(
        &[
            (MODELS_NAMESPACE, "jev_max_concurrent_requests", json!(1)),
            (MODELS_NAMESPACE, "jev_model", json!("jev-b")),
            (
                PROVIDER_NAMESPACE,
                "base_url",
                json!(server.url.trim_end_matches("/v1/responses")),
            ),
        ],
        ApplyMode::Immediate,
    );
    assert_eq!(
        judge
            .classify(ActionReviewMode::SystemSafety, action_request())
            .await,
        ActionClassification::Uncertain(ActionReviewUncertainty::Busy)
    );
    assert!(!first.is_finished() && !second.is_finished());
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert_eq!(
        ActionReviewClassifier::classify(
            original.as_ref(),
            ActionReviewMode::SystemSafety,
            action_request(),
        )
        .await,
        ActionClassification::Uncertain(ActionReviewUncertainty::Busy)
    );
    assert!(!second.is_finished());
    assert!(server.requests.try_recv().is_err());
    second.abort();
    assert!(second.await.unwrap_err().is_cancelled());
    assert!(matches!(
        ActionReviewClassifier::classify(
            original.as_ref(),
            ActionReviewMode::SystemSafety,
            action_request(),
        )
        .await,
        ActionClassification::Decided { .. }
    ));
    assert_request(
        &server.next().await,
        "/v1/systemone",
        "jev-a",
        JEV_SECRET,
        PRIMARY_SECRET,
    );
    assert!(matches!(
        judge
            .classify(ActionReviewMode::SystemSafety, action_request())
            .await,
        ActionClassification::Decided { .. }
    ));
    assert_request(
        &server.next().await,
        "/v1/systemone",
        "jev-b",
        JEV_SECRET,
        PRIMARY_SECRET,
    );
    h.stop().await;
}

#[tokio::test]
async fn relation_fallback_routes_auxiliary_per_request_without_sending_to_primary_on_invalid_role()
{
    let root = tempfile::tempdir().unwrap();
    let text = "把解释改成中文。";
    let mut primary = Server::start(vec![primary_reply(text)]).await;
    let mut auxiliary = Server::start(vec![primary_reply(text)]).await;
    let mut jev = Server::start(vec![]).await;
    let h = Harness::start(root.path(), &primary.url, &jev.url).await;
    let judge = h
        .install(h.plugin(MessageJudgeMode::Primary).unwrap())
        .await;
    assert!(primary.requests.try_recv().is_err());
    assert_eq!(
        judge.judge(input(text)).await.unwrap().parts[0].intent,
        MessageIntent::Correction
    );
    assert_request(
        &primary.next().await,
        "/v1/responses",
        "primary-a",
        PRIMARY_SECRET,
        JEV_SECRET,
    );
    h.update(
        &[
            (MODELS_NAMESPACE, "auxiliary_small_enabled", json!(true)),
            (
                MODELS_NAMESPACE,
                "auxiliary_small_provider",
                json!("openai"),
            ),
            (
                MODELS_NAMESPACE,
                "auxiliary_small_model",
                json!("relation-aux"),
            ),
            (MODELS_NAMESPACE, "auxiliary_small_timeout_ms", json!(2000)),
            (
                MODELS_NAMESPACE,
                "auxiliary_small_max_output_tokens",
                json!(128),
            ),
            (
                config::MODEL_USES_NAMESPACE,
                "message_relation_role",
                json!("auxiliary_small"),
            ),
            (
                config::MODEL_USES_NAMESPACE,
                "message_relation_max_output_tokens",
                json!(77),
            ),
            // 此测试显式复用启动凭据的辅助用途绑定；不选择独立端点或任意环境变量。
            (
                config::OPENAI_NAMESPACE,
                "base_url",
                json!(auxiliary.url.trim_end_matches("/responses")),
            ),
        ],
        ApplyMode::NewRequests,
    );
    assert_eq!(
        judge.judge(input(text)).await.unwrap().parts[0].intent,
        MessageIntent::Correction
    );
    let request = auxiliary.next().await;
    assert_request(
        &request,
        "/v1/responses",
        "relation-aux",
        PRIMARY_SECRET,
        JEV_SECRET,
    );
    assert_eq!(request.body["max_output_tokens"], 77);
    h.update(
        &[(MODELS_NAMESPACE, "auxiliary_small_enabled", json!(false))],
        ApplyMode::Immediate,
    );
    assert!(judge.judge(input(text)).await.is_err());
    assert!(primary.requests.try_recv().is_err());
    assert!(auxiliary.requests.try_recv().is_err());
    assert!(jev.requests.try_recv().is_err());
    h.stop().await;
}
