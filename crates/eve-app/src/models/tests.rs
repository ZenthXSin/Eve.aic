#[path = "../../../llm-openai/tests/support/mod.rs"]
mod http_support;

use super::*;
use crate::{AppError, CoreBootstrap, config, finish_core, install_core_with_config};
use eve_config_api::{
    ApplyMode, CONFIG_SERVICE_ID, ConfigAdmin, ConfigServiceHandle, NamespaceValues,
    model_roles_schema, runtime_llm_schema,
};
use eve_config_plugin::{ConfigBootstrap, ConfigController, ConfigPlugin};
use eve_control_api::{
    CommitState, ControlEvent, ControlEventSink, ControlInput, ControlReport, ControlService,
    DiscardControlEvents, GenerationKey, RunFailure,
};
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::{LlmFuture, TurnEventKind};
use eve_plugin_api::ServiceId;
use eve_runtime::LlmHostConfig;
use eve_session_api::{SessionInput, SessionKey};
use http_support::{Reply, Server, final_response};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, time::Duration};
use tokio::sync::Notify;

struct Harness {
    kernel: Kernel,
    admin: ConfigController,
    settings: Arc<dyn ConfigService>,
    control: Arc<dyn ControlService>,
}
impl Harness {
    async fn start(root: &Path, url: &str, protocol: &str) -> Self {
        let backends = KernelServices {
            state: Arc::new(FileStateStore::open(root.join("state")).unwrap()),
            ..KernelServices::default()
        };
        let registry = backends.registry.clone();
        let permissions = backends.permissions.clone();
        let logger = backends.logger.clone();
        let kernel = Kernel::with_services(backends);
        let plugin = ConfigPlugin::new(
            ConfigBootstrap::new(
                root.join("state/configuration"),
                vec![
                    runtime_llm_schema(),
                    config::openai_schema(),
                    model_roles_schema(),
                ],
            )
            .with_environment(BTreeMap::from([
                (
                    "EVE_OPENAI_BASE_URL".into(),
                    url.trim_end_matches("/responses").into(),
                ),
                ("EVE_OPENAI_PROTOCOL".into(), protocol.into()),
                ("EVE_OPENAI_MODEL_ROLE".into(), "primary".into()),
                ("EVE_MODELS_PRIMARY_ENABLED".into(), "true".into()),
                ("EVE_MODELS_PRIMARY_PROVIDER".into(), "openai".into()),
                ("EVE_MODELS_PRIMARY_MODEL".into(), "model-a".into()),
                ("EVE_MODELS_PRIMARY_TIMEOUT_MS".into(), "2500".into()),
                ("EVE_MODELS_PRIMARY_MAX_OUTPUT_TOKENS".into(), "32".into()),
            ])),
        )
        .unwrap();
        let admin = plugin.controller();
        let control = install_core_with_config(
            &kernel,
            registry.clone(),
            permissions,
            logger,
            CoreBootstrap {
                host_config: LlmHostConfig::default(),
                api_key: "resolver-test-key".into(),
            },
            plugin,
        )
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
            admin,
            settings,
            control,
        }
    }

    fn update(&self, changes: &[(&str, &str, Value)], mode: ApplyMode) {
        let document = self.admin.current().unwrap();
        let mut overrides = document.namespaces;
        for (namespace, field, value) in changes {
            overrides
                .entry((*namespace).into())
                .or_insert_with(|| NamespaceValues {
                    schema_version: 1,
                    values: BTreeMap::new(),
                })
                .values
                .insert((*field).into(), value.clone());
        }
        self.admin
            .replace(document.revision, overrides, mode)
            .unwrap();
    }

    fn submit(&self, text: &str, sink: Arc<dyn ControlEventSink>) -> GenerationKey {
        self.control
            .submit(
                ControlInput {
                    session: SessionInput {
                        key: SessionKey::new("default", "owner").unwrap(),
                        text: text.into(),
                    },
                    task_id: text.into(),
                },
                sink,
            )
            .unwrap()
    }

    async fn done(&self, key: &GenerationKey) -> ControlReport {
        tokio::time::timeout(Duration::from_secs(5), self.control.wait(key))
            .await
            .unwrap()
            .unwrap()
    }

    async fn stop(self) {
        finish_core(&self.kernel, Ok::<(), AppError>(()))
            .await
            .unwrap();
    }
}

#[derive(Default)]
struct ToolGate {
    entered: Notify,
    released: Notify,
}
impl ControlEventSink for ToolGate {
    fn emit(&self, event: ControlEvent) -> LlmFuture<'_, ()> {
        Box::pin(async move {
            if matches!(event.event.kind, TurnEventKind::ToolBatchStarted { .. }) {
                self.entered.notify_one();
                self.released.notified().await;
            }
            Ok(())
        })
    }
}
fn final_reply(protocol: &str) -> Reply {
    Reply::json(if protocol == "chat" {
        json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"done"}}]})
    } else {
        final_response("done")
    })
}
fn tool_reply(protocol: &str) -> Reply {
    Reply::json(if protocol == "chat" {
        json!({"choices":[{"index":0,"finish_reason":"tool_calls","message":{"role":"assistant","content":null,
            "tool_calls":[{"id":"role-echo","type":"function","function":{"name":"echo","arguments":"{\"text\":\"marker\"}"}}]}}]})
    } else {
        json!({"status":"completed","error":null,"output":[
            {"type":"function_call","call_id":"role-echo","name":"echo","arguments":"{\"text\":\"marker\"}"}
        ]})
    })
}
fn user_texts<'a>(body: &'a Value, protocol: &str) -> Vec<&'a str> {
    let messages = body[if protocol == "chat" {
        "messages"
    } else {
        "input"
    }]
    .as_array()
    .unwrap();
    messages
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn primary_selection_is_fixed_during_tools_and_changes_at_next_round() {
    for (protocol, mode) in [
        ("chat", ApplyMode::Immediate),
        ("responses", ApplyMode::NewRequests),
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut server = Server::start(vec![
            tool_reply(protocol),
            final_reply(protocol),
            final_reply(protocol),
            final_reply(protocol),
            final_reply(protocol),
        ])
        .await;
        let h = Harness::start(root.path(), &server.url, protocol).await;
        let gate = Arc::new(ToolGate::default());
        let first = h.submit("first", gate.clone());
        let request = server.next().await;
        let expected_path = if protocol == "chat" {
            "POST /v1/chat/completions HTTP/1.1"
        } else {
            "POST /v1/responses HTTP/1.1"
        };
        assert!(request.headers.starts_with(expected_path));
        assert_eq!(request.body["model"], "model-a");
        assert_eq!(
            request.body[if protocol == "chat" {
                "max_tokens"
            } else {
                "max_output_tokens"
            }],
            32
        );
        tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
            .await
            .unwrap();
        h.update(
            &[
                ("runtime.models", "primary_model", json!("model-b")),
                ("runtime.models", "primary_timeout_ms", json!(4000)),
                ("runtime.models", "primary_max_output_tokens", json!(64)),
            ],
            mode,
        );
        gate.released.notify_one();
        let completed = h.done(&first).await;
        assert_eq!(completed.run.commit, CommitState::Completed);
        assert_eq!(completed.run.started_tools, Some(1));
        let follow = server.next().await;
        assert_eq!(follow.body["model"], "model-a");
        assert_eq!(
            follow.body[if protocol == "chat" {
                "max_tokens"
            } else {
                "max_output_tokens"
            }],
            32
        );
        let second = h.submit("second", Arc::new(DiscardControlEvents));
        assert_eq!(h.done(&second).await.run.commit, CommitState::Completed);
        let next = server.next().await;
        assert_eq!(next.body["model"], "model-b");
        assert_eq!(
            next.body[if protocol == "chat" {
                "max_tokens"
            } else {
                "max_output_tokens"
            }],
            64
        );
        assert_eq!(user_texts(&next.body, protocol), ["first", "second"]);
        let before = std::fs::read(root.path().join("state/state.json")).unwrap();
        h.update(
            &[("runtime.models", "primary_provider", json!("unsupported"))],
            mode,
        );
        let invalid = h.submit("must-not-run", Arc::new(DiscardControlEvents));
        let rejected = h.done(&invalid).await;
        assert_eq!(rejected.run.commit, CommitState::NotStarted);
        assert_eq!(rejected.run.started_tools, Some(0));
        assert!(matches!(
            rejected.run.failure,
            Some(RunFailure::Execution(LlmError::Configuration(_)))
        ));
        assert_eq!(
            std::fs::read(root.path().join("state/state.json")).unwrap(),
            before
        );
        assert!(server.requests.try_recv().is_err());
        h.update(
            &[
                ("runtime.models", "primary_provider", json!("openai")),
                ("runtime.models", "primary_model", json!("model-c")),
            ],
            mode,
        );
        let third = h.submit("third", Arc::new(DiscardControlEvents));
        assert_eq!(h.done(&third).await.run.commit, CommitState::Completed);
        assert_eq!(server.next().await.body["model"], "model-c");
        h.stop().await;
        let reopened = Harness::start(root.path(), &server.url, protocol).await;
        let fourth = reopened.submit("fourth", Arc::new(DiscardControlEvents));
        let result = reopened.done(&fourth).await;
        assert_eq!(result.run.commit, CommitState::Completed);
        assert_eq!(result.run.started_tools, Some(0));
        let restored = server.next().await.body;
        assert_eq!(restored["model"], "model-c");
        assert_eq!(
            user_texts(&restored, protocol),
            ["first", "second", "third", "fourth"]
        );
        let history = restored[if protocol == "chat" {
            "messages"
        } else {
            "input"
        }]
        .as_array()
        .unwrap();
        assert_eq!(
            history
                .iter()
                .filter(|m| m["role"] == "tool" || m["type"] == "function_call_output")
                .count(),
            1
        );
        assert!(server.requests.try_recv().is_err());
        reopened.stop().await;
    }
}

#[tokio::test]
async fn cancellation_and_closed_configuration_do_not_reuse_stale_selection() {
    let root = tempfile::tempdir().unwrap();
    let mut waiting = final_reply("chat");
    waiting.body_delay = Duration::from_secs(10);
    let mut server = Server::start(vec![waiting, final_reply("chat")]).await;
    let h = Harness::start(root.path(), &server.url, "chat").await;
    let first = h.submit("cancel-me", Arc::new(DiscardControlEvents));
    assert_eq!(server.next().await.body["model"], "model-a");
    h.update(
        &[("runtime.models", "primary_model", json!("model-b"))],
        ApplyMode::Immediate,
    );
    h.control.cancel(&first).unwrap();
    let cancelled = h.done(&first).await;
    assert!(cancelled.cancel_requested);
    assert_eq!(cancelled.run.commit, CommitState::Failed);
    let next = h.submit("after-cancel", Arc::new(DiscardControlEvents));
    assert_eq!(h.done(&next).await.run.commit, CommitState::Completed);
    let request = server.next().await.body;
    assert_eq!(request["model"], "model-b");
    assert_eq!(user_texts(&request, "chat"), ["after-cancel"]);
    let resolver = CoreModelResolver::new(h.settings.clone(), "resolver-test-key".into());
    let first_cached = resolver.resolve().unwrap();
    let second_cached = resolver.resolve().unwrap();
    assert!(Arc::ptr_eq(&first_cached.provider, &second_cached.provider));
    h.stop().await;
    assert!(resolver.resolve().is_err());
    assert!(server.requests.try_recv().is_err());
}

struct RacingSettings(std::sync::atomic::AtomicUsize);
impl ConfigService for RacingSettings {
    fn snapshot(&self, namespace: &str, _: u32) -> eve_config_api::ConfigResult<ConfigSnapshot> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let schema = if namespace == config::OPENAI_NAMESPACE {
            config::openai_schema()
        } else {
            model_roles_schema()
        };
        let mut snapshot = ConfigSnapshot {
            namespace: schema.namespace,
            schema_version: 1,
            revision: if namespace == config::OPENAI_NAMESPACE {
                0
            } else {
                1
            },
            values: schema
                .fields
                .into_iter()
                .map(|(name, field)| (name, field.default.unwrap()))
                .collect(),
        };
        if namespace == config::OPENAI_NAMESPACE {
            snapshot
                .values
                .insert("model_role".into(), json!("primary"));
        } else {
            snapshot
                .values
                .insert("primary_enabled".into(), json!(true));
            snapshot
                .values
                .insert("primary_provider".into(), json!("openai"));
            snapshot
                .values
                .insert("primary_model".into(), json!("model-a"));
        }
        Ok(snapshot)
    }
    fn begin_request(
        &self,
        _: &str,
        _: u32,
    ) -> eve_config_api::ConfigResult<eve_config_api::ConfigRequest> {
        Err(eve_config_api::ConfigError::Unavailable)
    }
    fn read_request(
        &self,
        _: &eve_config_api::ConfigRequest,
    ) -> eve_config_api::ConfigResult<ConfigSnapshot> {
        Err(eve_config_api::ConfigError::Unavailable)
    }
}
#[test]
fn mismatched_configuration_revisions_fail_after_bounded_reads() {
    let settings = Arc::new(RacingSettings(std::sync::atomic::AtomicUsize::new(0)));
    let resolver = CoreModelResolver::new(settings.clone(), "resolver-test-key".into());
    assert!(matches!(
        resolver.resolve(),
        Err(LlmError::Configuration(_))
    ));
    assert_eq!(settings.0.load(std::sync::atomic::Ordering::SeqCst), 8);
}
