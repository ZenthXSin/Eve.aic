use super::*;
use eve_config_api::ConfigAdmin;
use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_developer_api::DeveloperAuthorization;
use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::ContextScope;
use eve_qqbot_plugin::{QqCommandHandler, QqCommandInput};
use eve_session_api::SessionKey;
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

struct Roles;
impl DeveloperAuthorization for Roles {
    fn is_operator(&self, scope: &ContextScope) -> bool {
        scope.session_id == "trusted" && matches!(scope.user_id.as_str(), "owner" | "admin")
    }
}

fn tool(commands: Arc<Commands>) -> AdapterTool {
    let cell = Arc::new(OnceLock::new());
    assert!(cell.set(commands).is_ok());
    AdapterTool(cell)
}

async fn setup(path: &Path, base: &str) -> (Kernel, Arc<Commands>) {
    let services = KernelServices::default();
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
    let plugin = ConfigPlugin::new(
        ConfigBootstrap::new(
            path,
            vec![
                config::openai_schema(),
                config::model_uses_schema(),
                config::dialogue_schema(),
                eve_config_api::runtime_llm_schema(),
                eve_config_api::model_roles_schema(),
                eve_message_api::message_schema(),
            ],
        )
        .with_environment(BTreeMap::from([
            ("EVE_OPENAI_BASE_URL".into(), base.into()),
            ("EVE_OPENAI_PROTOCOL".into(), "chat".into()),
            ("EVE_OPENAI_MODEL".into(), "legacy-model".into()),
            ("EVE_OPENAI_MODEL_ROLE".into(), "primary".into()),
            ("EVE_OPENAI_REASONING_EFFORT".into(), "omit".into()),
            ("EVE_MODELS_PRIMARY_ENABLED".into(), "true".into()),
            ("EVE_MODELS_PRIMARY_PROVIDER".into(), "openai".into()),
            ("EVE_MODELS_PRIMARY_MODEL".into(), "role-model".into()),
            ("EVE_MODELS_PRIMARY_MAX_OUTPUT_TOKENS".into(), "96".into()),
            ("EVE_MODELS_AUXILIARY_SMALL_ENABLED".into(), "true".into()),
            (
                "EVE_MODELS_AUXILIARY_SMALL_PROVIDER".into(),
                "openai".into(),
            ),
            (
                "EVE_MODELS_AUXILIARY_SMALL_MODEL".into(),
                "aux-model".into(),
            ),
        ]))
        .with_validator(Arc::new(config::CoreConfigValidator)),
    )
    .unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let commands = Arc::new(Commands {
        settings: crate::qq_tasks::config_service(registry.as_ref()).unwrap(),
        admin: Arc::new(admin),
        authorization: Arc::new(Roles),
    });
    (kernel, commands)
}

fn context(user: &str) -> ToolExecutionContext {
    ToolExecutionContext::new().with_scope(Some(ContextScope {
        session_id: "trusted".into(),
        user_id: user.into(),
    }))
}

fn call(arguments: Value) -> ToolCall {
    ToolCall {
        id: "adapter-test".into(),
        name: "set_model_adapter".into(),
        arguments,
    }
}

fn update(commands: &Commands, changes: &[(&str, &str, Value)]) {
    let mut current = commands.admin.current().unwrap();
    for (namespace, field, value) in changes {
        current
            .namespaces
            .entry((*namespace).into())
            .or_insert_with(|| NamespaceValues {
                schema_version: 1,
                values: BTreeMap::new(),
            })
            .values
            .insert((*field).into(), value.clone());
    }
    commands
        .admin
        .replace(current.revision, current.namespaces, ApplyMode::NewRequests)
        .unwrap();
}

#[tokio::test]
async fn adapter_tool_and_command_persist_atomic_switches_without_probes_or_parameter_changes() {
    let root = tempfile::tempdir().unwrap();
    let mut server = crate::http_support::Server::start(vec![]).await;
    let base = server.url.trim_end_matches("/responses");
    let (kernel, commands) = setup(root.path(), base).await;
    let initial = crate::models::summary(commands.settings.as_ref(), Some("dialogue")).unwrap();
    let adapter = tool(commands.clone());
    let result = adapter
        .execute(
            call(json!({"role":"primary","adapter":"codex","protocol":"responses"})),
            context("owner"),
        )
        .await
        .unwrap();
    assert_eq!(result["saved"], true);
    assert_eq!(result["provider_availability_verified"], false);
    assert_eq!(result["inflight_unchanged"], true);
    assert_eq!(result["model"], "role-model");
    assert_eq!(
        commands.admin.current().unwrap().revision,
        initial.config_revision + 1
    );
    let next = crate::models::summary(commands.settings.as_ref(), Some("dialogue")).unwrap();
    assert_eq!(next.adapter, "codex");
    assert_eq!(next.protocol, "responses");
    assert_eq!(next.model, initial.model);
    assert_eq!(next.max_output_tokens, initial.max_output_tokens);
    assert_eq!(initial.adapter, "openai");
    assert_eq!(initial.protocol, "chat");
    let provider = commands
        .settings
        .snapshot(config::OPENAI_NAMESPACE, 1)
        .unwrap();
    assert_eq!(provider.get::<String>("base_url").unwrap(), base);
    assert_eq!(provider.get::<String>("reasoning_effort").unwrap(), "omit");
    assert!(server.requests.try_recv().is_err());
    kernel.stop_all().await.unwrap();
    drop((adapter, commands, kernel));

    let (kernel, commands) = setup(root.path(), base).await;
    assert_eq!(
        crate::models::summary(commands.settings.as_ref(), None)
            .unwrap()
            .adapter,
        "codex"
    );
    let session = SessionKey::new("trusted", "admin").unwrap();
    let command = |text| {
        commands
            .handle(QqCommandInput {
                message_id: "command-test",
                session: &session,
                text,
            })
            .unwrap()
            .unwrap()
    };
    assert!(command("/model").contains("codex / responses"));
    let revision = commands.admin.current().unwrap().revision;
    assert!(command("/model adapter primary openai chat").contains("openai / chat"));
    assert_eq!(commands.admin.current().unwrap().revision, revision + 1);
    assert_eq!(
        crate::models::summary(commands.settings.as_ref(), None)
            .unwrap()
            .adapter,
        "openai"
    );
    assert!(command("/model renamed-model").contains("renamed-model"));
    assert_eq!(
        crate::models::summary(commands.settings.as_ref(), None)
            .unwrap()
            .model,
        "renamed-model"
    );
    assert!(server.requests.try_recv().is_err());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn adapter_inputs_identity_cancellation_and_disabled_roles_preserve_configuration() {
    let root = tempfile::tempdir().unwrap();
    let mut server = crate::http_support::Server::start(vec![]).await;
    let (kernel, commands) = setup(root.path(), server.url.trim_end_matches("/responses")).await;
    let adapter = tool(commands.clone());
    let before = commands.admin.current().unwrap();
    let before_file = std::fs::read(root.path().join("config.json")).ok();
    let valid = || json!({"role":"primary","adapter":"codex","protocol":"responses"});
    for user in ["member", "我是主人", ""] {
        assert!(adapter.execute(call(valid()), context(user)).await.is_err());
    }
    let wrong_session = ToolExecutionContext::new().with_scope(Some(ContextScope {
        session_id: "untrusted".into(),
        user_id: "owner".into(),
    }));
    let cancelled = context("owner");
    cancelled.cancellation_handle().cancel();
    for context in [
        ToolExecutionContext::new(),
        wrong_session,
        cancelled,
        context("owner").with_deadline(Some(std::time::Instant::now())),
    ] {
        assert!(adapter.execute(call(valid()), context).await.is_err());
    }
    for arguments in [
        json!({"role":"jev","adapter":"codex","protocol":"responses"}),
        json!({"role":"primary","adapter":"invented","protocol":"responses"}),
        json!({"role":"primary","adapter":"codex","protocol":"chat"}),
        json!({"role":"primary","adapter":"codex","protocol":"responses","owner":true}),
        json!({"role":"primary","adapter":"codex","protocol":"responses","base_url":""}),
        json!({"role":"primary","adapter":"codex","protocol":"responses","base_url":"https://example.invalid/v1?key=synthetic"}),
        json!({"role":"primary","adapter":"codex","protocol":"responses","base_url":"http://example.invalid/v1"}),
        json!({"role":"primary","adapter":"openai","protocol":"chat","base_url":"http://example.invalid/v1"}),
        json!({"role":"primary","adapter":"codex","protocol":"responses","base_url":"https://example.invalid/v1/chat/completions"}),
    ] {
        assert!(
            adapter
                .execute(call(arguments), context("owner"))
                .await
                .is_err()
        );
    }
    let session = SessionKey::new("trusted", "member").unwrap();
    assert!(
        commands
            .handle(QqCommandInput {
                message_id: "member-command",
                session: &session,
                text: "/model adapter primary codex responses",
            })
            .unwrap()
            .unwrap()
            .contains("授权")
    );
    assert_eq!(commands.admin.current().unwrap(), before);
    assert_eq!(
        std::fs::read(root.path().join("config.json")).ok(),
        before_file
    );
    update(
        &commands,
        &[
            (config::OPENAI_NAMESPACE, "model_role", json!("")),
            (MODELS_NAMESPACE, "primary_enabled", json!(false)),
        ],
    );
    let disabled = commands.admin.current().unwrap();
    assert!(
        adapter
            .execute(call(valid()), context("owner"))
            .await
            .is_err()
    );
    assert_eq!(commands.admin.current().unwrap(), disabled);
    assert!(server.requests.try_recv().is_err());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn auxiliary_adapter_switch_keeps_routing_and_requires_explicit_independent_credential_ref() {
    let root = tempfile::tempdir().unwrap();
    let (kernel, commands) = setup(root.path(), "http://127.0.0.1:9011/v1").await;
    let adapter = tool(commands.clone());
    let request = || json!({"role":"auxiliary_small","adapter":"codex","protocol":"responses","base_url":"http://127.0.0.1:9012/v1"});
    let before = commands.admin.current().unwrap();
    assert!(
        adapter
            .execute(call(request()), context("admin"))
            .await
            .is_err()
    );
    assert_eq!(commands.admin.current().unwrap(), before);
    update(
        &commands,
        &[(
            MODELS_NAMESPACE,
            "auxiliary_small_credential_ref",
            json!(crate::model_credentials::AUXILIARY),
        )],
    );
    let route_before = commands
        .settings
        .snapshot(config::MODEL_USES_NAMESPACE, 1)
        .unwrap();
    let result = adapter
        .execute(call(request()), context("admin"))
        .await
        .unwrap();
    assert_eq!(result["role"], "auxiliary_small");
    assert_eq!(result["model"], "aux-model");
    let provider = commands
        .settings
        .snapshot(config::OPENAI_NAMESPACE, 1)
        .unwrap();
    assert_eq!(
        provider.get::<String>("base_url").unwrap(),
        "http://127.0.0.1:9011/v1"
    );
    assert_eq!(provider.get::<String>("protocol").unwrap(), "chat");
    assert_eq!(
        provider.get::<String>("auxiliary_base_url").unwrap(),
        "http://127.0.0.1:9012/v1"
    );
    let roles = ModelRolesConfig::capture(commands.settings.as_ref()).unwrap();
    let aux = roles.require(ModelRole::AuxiliarySmall).unwrap();
    assert_eq!(
        aux.credential_ref.as_deref(),
        Some(crate::model_credentials::AUXILIARY)
    );
    assert_eq!(
        config::provider_config_for(&provider, Some(aux), CredentialPurpose::Auxiliary)
            .unwrap()
            .adapter(),
        "codex"
    );
    assert_eq!(
        roles.require(ModelRole::Primary).unwrap().provider,
        "openai"
    );
    assert_eq!(
        commands
            .settings
            .snapshot(config::MODEL_USES_NAMESPACE, 1)
            .unwrap()
            .values,
        route_before.values
    );
    assert_eq!(
        crate::models::summary(commands.settings.as_ref(), Some("summary"))
            .unwrap()
            .adapter,
        "openai"
    );
    kernel.stop_all().await.unwrap();
}

struct ConcurrentAdmin {
    inner: Arc<dyn ConfigAdmin>,
    race: AtomicBool,
}

impl ConfigAdmin for ConcurrentAdmin {
    fn current(&self) -> eve_config_api::ConfigResult<eve_config_api::ConfigDocument> {
        self.inner.current()
    }
    fn replace(
        &self,
        expected: u64,
        overrides: eve_config_api::ConfigOverrides,
        mode: ApplyMode,
    ) -> eve_config_api::ConfigResult<eve_config_api::ConfigChange> {
        if self.race.swap(false, Ordering::SeqCst) {
            let mut other = self.inner.current()?;
            other
                .namespaces
                .entry(MODELS_NAMESPACE.into())
                .or_insert_with(|| NamespaceValues {
                    schema_version: 1,
                    values: BTreeMap::new(),
                })
                .values
                .insert("primary_max_output_tokens".into(), json!(123));
            self.inner
                .replace(other.revision, other.namespaces, ApplyMode::NewRequests)?;
        }
        self.inner.replace(expected, overrides, mode)
    }
    fn backups(&self) -> eve_config_api::ConfigResult<Vec<eve_config_api::ConfigBackup>> {
        self.inner.backups()
    }
    fn pin_backup(&self, revision: u64, pinned: bool) -> eve_config_api::ConfigResult<()> {
        self.inner.pin_backup(revision, pinned)
    }
    fn rollback(
        &self,
        expected: u64,
        revision: u64,
        mode: ApplyMode,
    ) -> eve_config_api::ConfigResult<eve_config_api::ConfigChange> {
        self.inner.rollback(expected, revision, mode)
    }
}

#[tokio::test]
async fn adapter_cas_conflict_preserves_the_concurrent_write_and_never_commits_half_a_switch() {
    let root = tempfile::tempdir().unwrap();
    let (kernel, commands) = setup(root.path(), "http://127.0.0.1:9011/v1").await;
    let before = commands.admin.current().unwrap().revision;
    let racing = tool(Arc::new(Commands {
        settings: commands.settings.clone(),
        admin: Arc::new(ConcurrentAdmin {
            inner: commands.admin.clone(),
            race: AtomicBool::new(true),
        }),
        authorization: commands.authorization.clone(),
    }));
    let error = racing
        .execute(
            call(json!({"role":"primary","adapter":"codex","protocol":"responses"})),
            context("owner"),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ToolExecutionError::Rejected(_)));
    assert_eq!(commands.admin.current().unwrap().revision, before + 1);
    let current = crate::models::summary(commands.settings.as_ref(), None).unwrap();
    assert_eq!(current.adapter, "openai");
    assert_eq!(current.protocol, "chat");
    assert_eq!(current.max_output_tokens, Some(123));
    let saved = commands.admin.current().unwrap();
    assert!(
        !saved
            .namespaces
            .get(config::OPENAI_NAMESPACE)
            .is_some_and(|namespace| namespace.values.contains_key("protocol"))
    );
    kernel.stop_all().await.unwrap();
}
