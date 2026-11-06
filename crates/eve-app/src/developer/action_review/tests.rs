use super::*;
use crate::{
    config,
    http_support::{Captured, Reply, Server},
    qq_message_judge::JevRoleJudge,
};
use eve_config_api::{
    ApplyMode, CONFIG_PLUGIN_ID, ConfigAdmin, ConfigService, MODELS_NAMESPACE, NamespaceValues,
    model_roles_schema,
};
use eve_config_plugin::{ConfigBootstrap, ConfigController, ConfigPlugin};
use eve_kernel::{Kernel, KernelServices};
use eve_plugin_api::PluginId;
use serde_json::Value;
use std::{collections::BTreeMap, path::Path, time::Duration};

const JEV_SECRET: &str = "synthetic-jev-action-secret";
const LLM_SECRET: &str = "synthetic-llm-action-secret";

struct Harness {
    kernel: Kernel,
    registry: Arc<dyn ServiceRegistry>,
    settings: Arc<dyn ConfigService>,
    admin: ConfigController,
    health: Arc<eve_model_health::HealthBook>,
}
impl Harness {
    async fn start(path: &Path, llm: &Server, jev: &Server) -> Self {
        let backends = KernelServices::default();
        let registry = backends.registry.clone();
        let kernel = Kernel::with_services(backends);
        let plugin = ConfigPlugin::new(
            ConfigBootstrap::new(
                path,
                vec![
                    config::openai_schema(),
                    config::model_uses_schema(),
                    crate::qq_message_judge::provider_schema(),
                    model_roles_schema(),
                ],
            )
            .with_environment(BTreeMap::from([
                (
                    "EVE_OPENAI_BASE_URL".into(),
                    llm.url.trim_end_matches("/responses").into(),
                ),
                ("EVE_OPENAI_PROTOCOL".into(), "responses".into()),
                ("EVE_OPENAI_MODEL_ROLE".into(), "primary".into()),
                ("EVE_MODELS_PRIMARY_ENABLED".into(), "true".into()),
                ("EVE_MODELS_PRIMARY_PROVIDER".into(), "openai".into()),
                ("EVE_MODELS_PRIMARY_MODEL".into(), "llm-review-model".into()),
                ("EVE_MODELS_PRIMARY_TIMEOUT_MS".into(), "2000".into()),
                (
                    "EVE_JEV_BASE_URL".into(),
                    jev.url.trim_end_matches("/v1/responses").into(),
                ),
                ("EVE_MODELS_JEV_ENABLED".into(), "true".into()),
                ("EVE_MODELS_JEV_PROVIDER".into(), "jev".into()),
                ("EVE_MODELS_JEV_MODEL".into(), "jev-review-model".into()),
                ("EVE_MODELS_JEV_TIMEOUT_MS".into(), "2000".into()),
                ("EVE_MODELS_JEV_MAX_CONCURRENT_REQUESTS".into(), "1".into()),
            ])),
        )
        .unwrap();
        let admin = plugin.controller();
        kernel.register(Box::new(plugin)).unwrap();
        kernel
            .start(&PluginId::new(CONFIG_PLUGIN_ID).unwrap())
            .await
            .unwrap();
        let settings = crate::qq_tasks::config_service(registry.as_ref()).unwrap();
        Self {
            kernel,
            registry,
            settings,
            admin,
            health: Arc::new(eve_model_health::HealthBook::new()),
        }
    }
    fn reviewer(&self, jev: bool, llm: bool) -> Arc<dyn ActionReviewer> {
        action_reviewer_with_keys(
            self.registry.clone(),
            self.health.clone(),
            jev.then(|| JEV_SECRET.into()),
            llm.then(|| LLM_SECRET.into()),
        )
        .unwrap()
    }
    fn update(&self, changes: &[(&str, &str, Value)]) {
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
            .replace(current.revision, namespaces, ApplyMode::Immediate)
            .unwrap();
    }
    fn llm_requests(&self) -> u64 {
        self.health
            .snapshot()
            .iter()
            .find(|(role, _)| role == REVIEW_USE)
            .unwrap()
            .1
            .requests
    }
    async fn stop(self) {
        self.kernel.stop_all().await.unwrap();
    }
}
fn request(mode: ActionReviewMode) -> ActionReviewRequest {
    let mut request = ActionReviewRequest {
        model_session: Some(
            eve_llm_api::ModelSession::anchored("review", None, "persistent-synthetic-action")
                .unwrap()
                .with_turn("turn-1")
                .unwrap(),
        ),
        action: "具体变更".into(),
        program: "eve.restart_self".into(),
        arguments: vec!["完整 JSON、候选差异和参数".into()],
        workspace: "/workspace".into(),
        isolated: false,
        cwd: ".".into(),
        policy_reason: "核对直接系统影响".into(),
        claimed_confirmation: false,
        delegated: true,
        user_messages: vec![],
        recent_actions: vec![],
    };
    if mode == ActionReviewMode::IdentityBoundary {
        request.program = "eve.configure_memory_sharing".into();
        request.user_messages = vec!["允许本人的已配对窗口共享这部分记忆".into()];
    }
    request
}
async fn run(
    reviewer: &dyn ActionReviewer,
    mode: ActionReviewMode,
    request: ActionReviewRequest,
) -> Result<ActionReviewVerdict, String> {
    match mode {
        ActionReviewMode::IdentityBoundary => reviewer.review(request).await,
        ActionReviewMode::SystemSafety => reviewer.review_system_safety(request).await,
    }
}
fn choice(label: &str, labels: &[&str], confidence: f64) -> Value {
    let probabilities: BTreeMap<_, _> = labels
        .iter()
        .map(|item| (*item, if *item == label { 1.0 } else { 0.0 }))
        .collect();
    json!({"type":"choice","choice":label,"confidence":confidence,"probabilities":probabilities})
}
fn jev_reply(mode: ActionReviewMode, harm: &str, confidence: f64) -> Reply {
    let mut body = json!({"answers": {
        "direct_system_harm":choice(harm, &["none","bounded","high","critical","uncertain"], confidence),
        "grounded":{"type":"noul","noul":1.0}
    }});
    if mode == ActionReviewMode::IdentityBoundary {
        body["answers"]["boundary_intent"] = choice(
            "explicit",
            &["explicit", "absent", "revoked", "uncertain"],
            1.0,
        );
    }
    Reply::json(body)
}
fn llm_reply() -> Reply {
    Reply::json(json!({"status":"completed","error":null,"output":[{
        "type":"function_call","call_id":"synthetic-review-call","name":"submit_review",
        "arguments":json!({"risk":"low","authorization":"high","outcome":"allow","rationale":"真实本人意图明确，未发现直接系统危害"}).to_string()
    }]}))
}
fn assert_header(sent: &Captured, key: &str, path: &str, session: &eve_llm_api::ModelSession) {
    assert!(
        sent.headers
            .starts_with(&format!("POST {path} HTTP/1.1\r\n"))
    );
    assert!(sent.headers.contains(&format!("Bearer {key}")));
    let lower = sent.headers.to_ascii_lowercase();
    for (name, value) in [
        ("session-id", session.session_id()),
        ("thread-id", session.thread_id()),
        ("x-client-request-id", session.client_request_id()),
    ] {
        assert!(lower.contains(&format!("\r\n{name}: {value}\r\n")));
    }
    assert!(!sent.body.to_string().contains(JEV_SECRET));
    assert!(!sent.body.to_string().contains(LLM_SECRET));
}

#[tokio::test]
async fn both_host_entries_use_jev_first_and_keep_llm_health_zero_without_primary_credentials() {
    let root = tempfile::tempdir().unwrap();
    let mut llm = Server::start(vec![]).await;
    let mut jev = Server::start(vec![
        jev_reply(ActionReviewMode::IdentityBoundary, "none", 0.99),
        jev_reply(ActionReviewMode::SystemSafety, "critical", 0.99),
        jev_reply(ActionReviewMode::SystemSafety, "uncertain", 0.99),
    ])
    .await;
    let h = Harness::start(root.path(), &llm, &jev).await;
    let reviewer = h.reviewer(true, false);
    for (mode, allow) in [
        (ActionReviewMode::IdentityBoundary, true),
        (ActionReviewMode::SystemSafety, false),
    ] {
        let input = request(mode);
        let session = input
            .model_session
            .as_ref()
            .unwrap()
            .fork("jev-action-review", None)
            .unwrap();
        assert_eq!(
            run(reviewer.as_ref(), mode, input.clone())
                .await
                .unwrap()
                .allow,
            allow
        );
        let sent = jev.next().await;
        assert_header(&sent, JEV_SECRET, "/v1/systemone", &session);
        assert_eq!(
            sent.body["state"]["request"],
            serde_json::to_value(input).unwrap()
        );
    }
    assert!(
        reviewer
            .review_system_safety(request(ActionReviewMode::SystemSafety))
            .await
            .unwrap_err()
            .contains("后备审查模型不可用")
    );
    jev.next().await;
    assert_eq!(h.llm_requests(), 0);
    assert!(llm.requests.try_recv().is_err());
    h.stop().await;
}

#[tokio::test]
async fn host_uncertain_fallback_uses_original_session_and_independent_llm_key_once_per_entry() {
    let root = tempfile::tempdir().unwrap();
    let mut llm = Server::start(vec![llm_reply(), llm_reply()]).await;
    let mut jev = Server::start(vec![
        jev_reply(ActionReviewMode::IdentityBoundary, "uncertain", 0.99),
        jev_reply(ActionReviewMode::SystemSafety, "none", 0.79),
    ])
    .await;
    let h = Harness::start(root.path(), &llm, &jev).await;
    let reviewer = h.reviewer(true, true);
    for mode in [
        ActionReviewMode::IdentityBoundary,
        ActionReviewMode::SystemSafety,
    ] {
        let input = request(mode);
        assert!(
            run(reviewer.as_ref(), mode, input.clone())
                .await
                .unwrap()
                .allow
        );
        let jev_sent = jev.next().await;
        assert_header(
            &jev_sent,
            JEV_SECRET,
            "/v1/systemone",
            &input
                .model_session
                .as_ref()
                .unwrap()
                .fork("jev-action-review", None)
                .unwrap(),
        );
        assert!(!jev_sent.headers.contains(LLM_SECRET));
        let llm_sent = llm.next().await;
        assert_header(
            &llm_sent,
            LLM_SECRET,
            "/v1/responses",
            input.model_session.as_ref().unwrap(),
        );
        assert!(!llm_sent.headers.contains(JEV_SECRET));
        assert_eq!(llm_sent.body["model"], "llm-review-model");
        assert_eq!(llm_sent.body["tools"][0]["name"], "submit_review");
        let text = llm_sent.body["input"][1]["content"].as_str().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(text).unwrap(),
            serde_json::to_value(input).unwrap()
        );
        let instructions = llm_sent.body["input"][0]["content"].as_str().unwrap();
        assert!(
            instructions.contains(if mode == ActionReviewMode::IdentityBoundary {
                "身份与访问边界审查者"
            } else {
                "系统安全审查者"
            })
        );
    }
    assert_eq!(h.llm_requests(), 2);
    assert!(llm.requests.try_recv().is_err());
    assert!(jev.requests.try_recv().is_err());
    h.stop().await;
}

#[tokio::test]
async fn over_jev_capacity_reaches_llm_complete_and_hot_invalid_jev_never_uses_old_client() {
    let root = tempfile::tempdir().unwrap();
    let mut llm = Server::start(vec![llm_reply(), llm_reply(), llm_reply()]).await;
    let mode = ActionReviewMode::SystemSafety;
    let mut jev = Server::start(vec![
        jev_reply(mode, "none", 0.99),
        jev_reply(mode, "none", 0.99),
    ])
    .await;
    let h = Harness::start(root.path(), &llm, &jev).await;
    let reviewer = h.reviewer(true, true);
    assert!(
        run(reviewer.as_ref(), mode, request(mode))
            .await
            .unwrap()
            .allow
    );
    jev.next().await;
    let mut large = request(mode);
    large.arguments.push("x".repeat(eve_jev::MAX_INPUT_BYTES));
    assert!(
        run(reviewer.as_ref(), mode, large.clone())
            .await
            .unwrap()
            .allow
    );
    let sent = llm.next().await;
    assert_eq!(
        serde_json::from_str::<Value>(sent.body["input"][1]["content"].as_str().unwrap()).unwrap(),
        serde_json::to_value(large).unwrap()
    );
    assert!(jev.requests.try_recv().is_err());
    for reference in ["env:EVE_OPENAI_API_KEY", "env:UNBOUND_ACTION_KEY"] {
        h.update(&[(MODELS_NAMESPACE, "jev_credential_ref", json!(reference))]);
        assert!(
            run(reviewer.as_ref(), mode, request(mode))
                .await
                .unwrap()
                .allow
        );
        llm.next().await;
        assert!(jev.requests.try_recv().is_err());
    }
    h.update(&[
        (
            MODELS_NAMESPACE,
            "jev_credential_ref",
            json!("env:EVE_JEV_API_KEY"),
        ),
        (
            MODELS_NAMESPACE,
            "jev_model",
            json!("jev-review-model-next"),
        ),
    ]);
    assert!(
        run(reviewer.as_ref(), mode, request(mode))
            .await
            .unwrap()
            .allow
    );
    assert_eq!(jev.next().await.body["model"], "jev-review-model-next");
    assert_eq!(h.llm_requests(), 3);
    assert!(llm.requests.try_recv().is_err());
    h.stop().await;
}

#[tokio::test]
async fn separately_wired_host_reviewers_share_jev_http_and_slots_with_other_jev_roles() {
    let root = tempfile::tempdir().unwrap();
    let mut llm = Server::start(vec![llm_reply()]).await;
    let mode = ActionReviewMode::SystemSafety;
    let mut slow = jev_reply(mode, "none", 0.99);
    slow.header_delay = Duration::from_secs(3);
    let mut jev = Server::start(vec![slow, jev_reply(mode, "none", 0.99)]).await;
    let h = Harness::start(root.path(), &llm, &jev).await;
    let shared = JevRoleJudge::shared(h.settings.clone(), JEV_SECRET.into()).unwrap();
    let other_role = JevRoleJudge::shared(h.settings.clone(), JEV_SECRET.into()).unwrap();
    assert!(Arc::ptr_eq(&shared, &other_role));
    assert!(Arc::ptr_eq(
        &shared.resolve().unwrap(),
        &other_role.resolve().unwrap()
    ));
    let first = h.reviewer(true, true);
    let second = h.reviewer(true, true);
    let running = tokio::spawn(async move { first.review_system_safety(request(mode)).await });
    jev.next().await;
    assert_eq!(
        shared.classify(mode, request(mode)).await,
        ActionClassification::Uncertain(ActionReviewUncertainty::Busy)
    );
    assert!(
        second
            .review(request(ActionReviewMode::IdentityBoundary))
            .await
            .unwrap()
            .allow
    );
    llm.next().await;
    assert!(jev.requests.try_recv().is_err());
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    assert!(
        second
            .review_system_safety(request(mode))
            .await
            .unwrap()
            .allow
    );
    jev.next().await;
    assert_eq!(h.llm_requests(), 1);
    h.stop().await;
}

#[tokio::test]
async fn missing_jev_key_falls_back_without_borrowing_the_llm_credential() {
    let root = tempfile::tempdir().unwrap();
    let mut llm = Server::start(vec![llm_reply()]).await;
    let mut jev = Server::start(vec![]).await;
    let h = Harness::start(root.path(), &llm, &jev).await;
    assert!(
        h.reviewer(false, true)
            .review_system_safety(request(ActionReviewMode::SystemSafety))
            .await
            .is_ok()
    );
    assert!(
        llm.next()
            .await
            .headers
            .contains(&format!("Bearer {LLM_SECRET}"))
    );
    assert!(jev.requests.try_recv().is_err());
    assert!(action_reviewer_with_keys(h.registry.clone(), h.health.clone(), None, None).is_none());
    h.stop().await;
}
