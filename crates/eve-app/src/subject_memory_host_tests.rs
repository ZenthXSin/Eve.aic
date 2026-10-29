//! 确定性模型驱动真实 LlmHost 与持久经验插件；通过文件产物而非模型自述验收。
use super::*;
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::*;
use eve_plugin_api::*;
use eve_runtime::{ContextBinding, LlmHost, LlmHostConfig, SessionBinding, SessionLlmHost};
use std::sync::atomic::{AtomicUsize, Ordering};
struct Empty;
impl ContextAssembler for Empty {
    fn assemble(&self, _: TurnInput) -> LlmFuture<'_, ContextSnapshot> {
        Box::pin(async {
            Ok(ContextSnapshot {
                revision: "base".into(),
                ..ContextSnapshot::default()
            })
        })
    }
}
struct Model {
    learn: bool,
    step: AtomicUsize,
}
impl LlmProvider for Model {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        let step = self.step.fetch_add(1, Ordering::SeqCst);
        let learn = self.learn;
        Box::pin(async move {
            if step > 0 {
                return Ok(ModelResponse::Final {
                    text: "工具执行结束".into(),
                });
            }
            if learn {
                return Ok(ModelResponse::ToolCalls {
                    calls: vec![ToolCall {
                        id: "learn".into(),
                        name: "experience_memory".into(),
                        arguments: json!({"action":"save","id":"atlas","text":"atlas产物校验值nonce-837"}),
                    }],
                });
            }
            // 只有宿主实际注入的经验中存在精确校验值，模型才构造产物核验请求。
            let found = request
                .messages
                .iter()
                .filter(|m| m.role != ChatRole::User)
                .filter_map(|m| m.text.as_deref())
                .any(|s| s.contains("atlas产物校验值nonce-837"));
            Ok(if found {
                ModelResponse::ToolCalls {
                    calls: vec![ToolCall {
                        id: "verify".into(),
                        name: "verify_artifact".into(),
                        arguments: json!({"expected":"nonce-837"}),
                    }],
                }
            } else {
                ModelResponse::Final {
                    text: "没有授权证据".into(),
                }
            })
        })
    }
}
struct Verify {
    root: std::path::PathBuf,
}
impl Tool for Verify {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "verify_artifact".into(),
            description: "核验固定本地产物".into(),
            argument_schema: json!({"type":"object","properties":{"expected":{"type":"string"}},"required":["expected"]}),
            required_permissions: vec![],
            concurrency: None,
        }
    }
    fn validate_arguments(&self, v: &Value) -> Result<(), ToolValidationError> {
        if v["expected"].as_str().is_some() {
            Ok(())
        } else {
            Err(ToolValidationError {
                message: "missing expected".into(),
            })
        }
    }
    fn execute(&self, call: ToolCall, _: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            let actual = std::fs::read_to_string(self.root.join("artifact"))
                .map_err(|_| ToolExecutionError::Failed("read".into()))?;
            if Some(actual.as_str()) != call.arguments["expected"].as_str() {
                return Err(ToolExecutionError::Failed("mismatch".into()));
            }
            std::fs::write(self.root.join("verified"), actual)
                .map_err(|_| ToolExecutionError::Failed("write".into()))?;
            Ok(json!({"verified":true}))
        })
    }
}
struct Services {
    manifest: PluginManifest,
    context: Arc<dyn ContextAssembler>,
    memory: Arc<dyn Tool>,
    verify: Arc<dyn Tool>,
}
impl Plugin for Services {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        let (context, memory, verify) = (
            self.context.clone(),
            self.memory.clone(),
            self.verify.clone(),
        );
        Box::pin(async move {
            ctx.provide_service(
                ServiceId::new("memory-test.context")?,
                ContextService(context),
            )?;
            ctx.provide_service(ServiceId::new("memory-test.memory")?, ToolService(memory))?;
            ctx.provide_service(ServiceId::new("memory-test.verify")?, ToolService(verify))?;
            Ok(None)
        })
    }
}
fn scope(session: &str, user: &str) -> ContextScope {
    ContextScope {
        session_id: session.into(),
        user_id: user.into(),
    }
}
async fn world(
    root: &std::path::Path,
) -> (
    Kernel,
    Arc<eve_memory_plugin::ExperienceMemoryController>,
    Arc<dyn ServiceRegistry>,
    Arc<dyn PermissionChecker>,
) {
    let services = KernelServices {
        state: Arc::new(FileStateStore::open(root.join("state")).unwrap()),
        ..KernelServices::default()
    };
    let registry = services.registry.clone();
    let permissions = services.permissions.clone();
    let kernel = Kernel::with_services(services);
    let plugin = eve_memory_plugin::ExperienceMemoryPlugin::new().unwrap();
    let admin = Arc::new(plugin.controller());
    kernel.register(Box::new(plugin)).unwrap();
    kernel
        .register(Box::new(eve_session_plugin::SessionPlugin::new().unwrap()))
        .unwrap();
    kernel.start_all().await.unwrap();
    // 精确可信映射，群中同名/同一正文和未认证 user 均不能建立关联。
    let resolver: Arc<dyn ExperiencePrincipalResolver> = Arc::new(|s: &MemoryScope| {
        Ok(match (s.session_id.as_str(), s.user_id.as_str()) {
            ("private-a", "openid-a") | ("private-b", "openid-b") => Some("principal-1".into()),
            _ => None,
        })
    });
    let context = ExperienceContext::new("qq", admin.clone(), Arc::new(Empty))
        .unwrap()
        .with_principal_resolver(resolver);
    kernel
        .register(Box::new(Services {
            manifest: PluginManifest::new("memory-test", "1.0.0").unwrap(),
            context: Arc::new(context),
            memory: tools("qq", admin.clone()).unwrap().remove(0),
            verify: Arc::new(Verify { root: root.into() }),
        }))
        .unwrap();
    kernel
        .start(&PluginId::new("memory-test").unwrap())
        .await
        .unwrap();
    (kernel, admin, registry, permissions)
}
async fn run(
    kernel: &Kernel,
    registry: Arc<dyn ServiceRegistry>,
    permissions: Arc<dyn PermissionChecker>,
    scope: ContextScope,
    text: &str,
    learn: bool,
) {
    let owner = PluginId::new("memory-test").unwrap();
    let host = LlmHost::new(
        Arc::new(Model {
            learn,
            step: AtomicUsize::new(0),
        }),
        registry,
        kernel.clone(),
        permissions,
        ContextBinding {
            service_id: ServiceId::new("memory-test.context").unwrap(),
            expected_owner: owner.clone(),
        },
        vec![
            ToolBinding {
                name: "experience_memory".into(),
                service_id: ServiceId::new("memory-test.memory").unwrap(),
                expected_owner: owner.clone(),
            },
            ToolBinding {
                name: "verify_artifact".into(),
                service_id: ServiceId::new("memory-test.verify").unwrap(),
                expected_owner: owner,
            },
        ],
        LlmHostConfig::default(),
    )
    .unwrap();
    SessionLlmHost::new(host, SessionBinding::builtin())
        .run_turn_with_events(
            eve_session_api::SessionInput {
                key: eve_session_api::SessionKey::new(scope.session_id, scope.user_id).unwrap(),
                text: text.into(),
            },
            &DiscardTurnEvents,
        )
        .await
        .unwrap();
}
#[tokio::test]
async fn actual_host_learns_shares_restarts_and_verifies_artifact_without_scope_leak() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("artifact"), "nonce-837").unwrap();
    let (kernel, admin, registry, permissions) = world(root.path()).await;
    run(
        &kernel,
        registry.clone(),
        permissions.clone(),
        scope("private-a", "openid-a"),
        "记住atlas产物",
        true,
    )
    .await;
    assert_eq!(admin.snapshot().unwrap().records.len(), 1);
    // 同一principal也不能自动读取另一个窗口的scope草稿。
    run(
        &kernel,
        registry.clone(),
        permissions.clone(),
        scope("private-b", "openid-b"),
        "atlas产物",
        false,
    )
    .await;
    assert!(!root.path().join("verified").exists());
    let source = ExperienceReadContext {
        scope: MemoryScope {
            channel: "qq".into(),
            session_id: "private-a".into(),
            user_id: "openid-a".into(),
        },
        principal: Some("principal-1".into()),
    };
    let mut version = admin.snapshot().unwrap().records[0].history[0].clone();
    version.operation_id = "explicit-share".into();
    // 宿主收到明确分享意图后才调用受信接口；保留原scope证据引用。
    admin
        .save_personal(&source, 1, "shared-atlas", version)
        .unwrap();
    kernel.stop_all().await.unwrap();
    drop(admin);
    drop(kernel);
    drop(registry);
    drop(permissions);
    let (kernel, admin, registry, permissions) = world(root.path()).await;
    for denied in [
        scope("private-impostor", "impostor-same-name"),
        scope("other-group", "openid-b"),
        scope("other-private", "openid-b"),
    ] {
        run(
            &kernel,
            registry.clone(),
            permissions.clone(),
            denied,
            "atlas产物",
            false,
        )
        .await;
        assert!(!root.path().join("verified").exists());
    }
    run(
        &kernel,
        registry.clone(),
        permissions.clone(),
        scope("private-b", "openid-b"),
        "今天天气",
        false,
    )
    .await;
    assert!(!root.path().join("verified").exists());
    run(
        &kernel,
        registry.clone(),
        permissions.clone(),
        scope("private-b", "openid-b"),
        "atlas产物",
        false,
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(root.path().join("verified")).unwrap(),
        "nonce-837"
    );
    std::fs::remove_file(root.path().join("verified")).unwrap();
    let snapshot = admin.snapshot().unwrap();
    let mut version = snapshot
        .records
        .iter()
        .find(|r| r.id == "shared-atlas")
        .unwrap()
        .history[0]
        .clone();
    version.revision = 2;
    version.operation_id = "correct-share".into();
    version.text = "atlas产物旧校验值已作废，尚待新核验".into();
    admin
        .revise(&source, snapshot.revision, "shared-atlas", version.clone())
        .unwrap();
    run(
        &kernel,
        registry.clone(),
        permissions.clone(),
        scope("private-b", "openid-b"),
        "atlas产物",
        false,
    )
    .await;
    assert!(
        !root.path().join("verified").exists(),
        "旧版本不能从经验摘要缓存复活"
    );
    version.revision = 3;
    version.operation_id = "revoke-share".into();
    version.status = ExperienceStatus::Revoked;
    admin
        .revise(&source, snapshot.revision + 1, "shared-atlas", version)
        .unwrap();
    run(
        &kernel,
        registry.clone(),
        permissions.clone(),
        scope("private-b", "openid-b"),
        "atlas产物",
        false,
    )
    .await;
    assert!(!root.path().join("verified").exists());
    assert_eq!(
        admin
            .snapshot()
            .unwrap()
            .records
            .iter()
            .find(|r| r.id == "shared-atlas")
            .unwrap()
            .history
            .len(),
        3
    );
    let snapshot = admin.snapshot().unwrap();
    let mut expired = snapshot
        .records
        .iter()
        .find(|r| r.id == "shared-atlas")
        .unwrap()
        .history[0]
        .clone();
    expired.operation_id = "expired-share".into();
    expired.at_ms = 1;
    expired.expires_at_ms = Some(2);
    admin
        .save_personal(&source, snapshot.revision, "expired-atlas", expired)
        .unwrap();
    run(
        &kernel,
        registry,
        permissions,
        scope("private-b", "openid-b"),
        "atlas产物",
        false,
    )
    .await;
    assert!(
        !root.path().join("verified").exists(),
        "已过期但正文匹配的经验不得驱动产物工具"
    );
    kernel.stop_all().await.unwrap();
}
