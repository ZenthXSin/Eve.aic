#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use eve_llm_api::{ContextScope, SharedToolSteps, ToolExecutionContext};
use eve_script_runtime::CargoProject;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use eve_script_runtime::{CargoCompilation, CargoCompiler, CargoCompilerConfig};
use std::collections::BTreeMap;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::path::PathBuf;

fn project() -> CargoProject {
    CargoProject {
        source: "fn main(){}".into(),
        dependencies: BTreeMap::new(),
        bundle_sha256: "a".repeat(64),
        build_source: None,
    }
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn context() -> ToolExecutionContext {
    ToolExecutionContext::new()
        .with_scope(Some(ContextScope {
            session_id: "cargo-api".into(),
            user_id: "owner".into(),
        }))
        .with_shared_steps(SharedToolSteps::new(2).unwrap())
}
#[test]
fn project_parameters_cannot_select_host_paths_commands_ranges_or_anonymous_resources() {
    let mut value = project();
    value.validate().unwrap();
    for (name, version) in [
        ("../../private", "1.0.0"),
        ("crate\ncommand", "1.0.0"),
        ("serde", "*"),
        ("serde", "git:private"),
        ("serde", "01.0.0"),
    ] {
        value.dependencies = BTreeMap::from([(name.into(), version.into())]);
        assert!(value.validate().is_err());
    }
    value.dependencies.clear();
    value.bundle_sha256 = "not-a-frozen-bundle".into();
    assert!(value.validate().is_err());
    assert!(serde_json::from_value::<CargoProject>(serde_json::json!({"source":"fn main(){}","dependencies":{},"bundle_sha256":"a".repeat(64),"user_id":"owner"})).is_err());
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[tokio::test]
async fn missing_identity_budget_or_cancelled_context_stops_before_daemon_access() {
    let directory = tempfile::tempdir().unwrap();
    let compiler = CargoCompiler::new(CargoCompilerConfig {
        cli: std::env::current_exe().unwrap(),
        socket: PathBuf::from("/not-a-cargo-socket"),
        image: format!("sha256:{}", "a".repeat(64)),
        toolchain: directory.path().into(),
        vendor: directory.path().into(),
        bundle_sha256: "a".repeat(64),
    })
    .unwrap();
    assert!(
        compiler
            .compile(&project(), ToolExecutionContext::new())
            .await
            .is_err()
    );
    let no_budget = ToolExecutionContext::new().with_scope(context().scope().cloned());
    assert!(compiler.compile(&project(), no_budget).await.is_err());
    let cancelled = context();
    cancelled.cancellation_handle().cancel();
    assert!(matches!(
        compiler.compile(&project(), cancelled.clone()).await,
        Err(eve_llm_api::ToolExecutionError::Cancelled(_))
    ));
    assert_eq!(cancelled.shared_steps().unwrap().remaining().unwrap(), 2);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[tokio::test]
#[ignore = "显式配置可信本地Docker、冻结公共vendor和toolchain后运行真实Cargo验收"]
async fn actual_cargo_compilation_shares_budget_and_rejects_artifact_changes() {
    let required =
        |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("缺少真实验收配置 {name}"));
    let compiler = CargoCompiler::new(CargoCompilerConfig {
        cli: PathBuf::from(required("EVE_TEST_CARGO_CLI")),
        socket: PathBuf::from("/var/run/docker.sock"),
        image: required("EVE_TEST_CARGO_IMAGE"),
        toolchain: PathBuf::from(required("EVE_TEST_CARGO_TOOLCHAIN")),
        vendor: PathBuf::from(required("EVE_TEST_CARGO_VENDOR")),
        bundle_sha256: required("EVE_TEST_CARGO_BUNDLE_SHA256"),
    })
    .unwrap();
    let mut source = project();
    source.source = "fn main(){println!(\"{}\",serde_json::json!({\"verified\":42}));}".into();
    source
        .dependencies
        .insert("serde_json".into(), required("EVE_TEST_CARGO_SERDE_JSON"));
    source.bundle_sha256 = required("EVE_TEST_CARGO_BUNDLE_SHA256");
    let ctx = context();
    let artifact = match compiler.compile(&source, ctx.clone()).await.unwrap() {
        CargoCompilation::Ready(artifact) => artifact,
        CargoCompilation::Failed(failure) => {
            panic!("真实编译失败：{}", serde_json::to_string(&failure).unwrap())
        }
    };
    assert_eq!(ctx.shared_steps().unwrap().remaining().unwrap(), 1);
    assert_eq!(artifact.owner(), ctx.scope().unwrap());
    assert_eq!(artifact.bundle_sha256(), source.bundle_sha256);
    assert_eq!(artifact.sha256().len(), 64);
    let path = artifact.verified_binary().unwrap().to_path_buf();
    assert_eq!(
        std::fs::read(&path).unwrap().get(..4),
        Some(&b"\x7fELF"[..])
    );
    std::fs::write(path, b"tampered-private-artifact").unwrap();
    assert!(artifact.verified_binary().is_err());
    assert_eq!(
        ctx.shared_steps().unwrap().remaining().unwrap(),
        1,
        "核对不重编译、不重领预算"
    );
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn actual_compiler() -> std::sync::Arc<CargoCompiler> {
    let required =
        |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("缺少真实验收配置 {name}"));
    std::sync::Arc::new(
        CargoCompiler::new(CargoCompilerConfig {
            cli: required("EVE_TEST_CARGO_CLI").into(),
            socket: "/var/run/docker.sock".into(),
            image: required("EVE_TEST_CARGO_IMAGE"),
            toolchain: required("EVE_TEST_CARGO_TOOLCHAIN").into(),
            vendor: required("EVE_TEST_CARGO_VENDOR").into(),
            bundle_sha256: required("EVE_TEST_CARGO_BUNDLE_SHA256"),
        })
        .unwrap(),
    )
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
struct NoTools;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl eve_script_runtime::AuthorizedToolDispatch for NoTools {
    fn preflight_call(
        &self,
        _: &eve_llm_api::ToolCall,
        _: &ToolExecutionContext,
    ) -> eve_script_runtime::Result<()> {
        Err(eve_llm_api::ToolExecutionError::Rejected(
            "no leaf tools".into(),
        ))
    }
    fn preflight_tool(&self, _: &str, _: &ToolExecutionContext) -> eve_script_runtime::Result<()> {
        Err(eve_llm_api::ToolExecutionError::Rejected(
            "no leaf tools".into(),
        ))
    }
    fn dispatch(
        &self,
        _: eve_llm_api::ToolCall,
        _: ToolExecutionContext,
    ) -> eve_llm_api::ToolFuture<'_> {
        Box::pin(async {
            Err(eve_llm_api::ToolExecutionError::Rejected(
                "no leaf tools".into(),
            ))
        })
    }
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[tokio::test]
#[ignore = "显式配置可信Docker与Linux bubblewrap后运行真实Cargo版本恢复验收"]
async fn actual_cargo_runtime_hot_reload_restart_and_digest_refusal() {
    use eve_script_runtime::*;
    use serde_json::json;
    use std::sync::Arc;
    let root = tempfile::tempdir().unwrap();
    let compiler = actual_compiler();
    let sandbox = Bubblewrap::new("/usr/bin/python3", "/usr/bin/node").with_cargo(compiler);
    let open = || {
        let store = ScriptStore::open(root.path().join("state")).unwrap();
        let runtime = ScriptRuntime::new(
            store.clone(),
            Arc::new(NoTools),
            sandbox.clone(),
            root.path().join("artifacts"),
        );
        (store, runtime)
    };
    let (store, runtime) = open();
    let owner = context().scope().unwrap().clone();
    let ctx = || context().with_shared_steps(SharedToolSteps::new(20).unwrap());
    let source = |value: usize| {
        format!(
            r#"use std::io::{{self,BufRead}};
fn main(){{let mut line=String::new();io::stdin().lock().read_line(&mut line).unwrap();let input:serde_json::Value=serde_json::from_str(&line).unwrap();println!("{{}}",serde_json::json!({{"op":"return","value":{{"generation":{value},"argument":input["arguments"]["probe"]}}}}));}}"#
        )
    };
    let mut spec = ScriptSpec {
        name: "cargo_reload".into(),
        language: Language::RustCargo(CargoSettings {
            dependencies: BTreeMap::from([(
                "serde_json".into(),
                std::env::var("EVE_TEST_CARGO_SERDE_JSON").unwrap(),
            )]),
            bundle_sha256: std::env::var("EVE_TEST_CARGO_BUNDLE_SHA256").unwrap(),
            build_source: None,
            artifact_sha256: Some("f".repeat(64)),
        }),
        source: source(1),
        dependencies: BTreeMap::new(),
        tools: vec![],
        state_schema: StateSchema {
            version: 1,
            fields: BTreeMap::new(),
        },
        initial_state: json!({}),
    };
    assert!(
        store.publish(&owner, spec.clone()).is_err(),
        "不能绕过准备保存原始Cargo标签"
    );
    let before = ctx();
    let first = runtime
        .publish_candidate(spec.clone(), before.clone())
        .await
        .unwrap();
    assert_eq!(before.shared_steps().unwrap().remaining().unwrap(), 19);
    assert_eq!(first.spec.language, Language::Rust, "兼容旧账本的Rust标签");
    assert!(first.spec.source.starts_with("// eve-cargo-v1 "));
    assert!(
        first
            .spec
            .source
            .contains("compile_error!(\"EVE_CARGO_V1_REQUIRES_VERIFIED_COMPILER\")")
    );
    assert!(
        !first.spec.source.contains(&"f".repeat(64)),
        "忽略伪造的输入产物摘要，使用实际编译结果"
    );
    store.activate(&owner, &first.reference, None).unwrap();
    let handle = runtime.bind(&owner, &first.reference).unwrap();
    let budget = ctx();
    let receipt = runtime
        .run(&handle, "cargo_first", json!({"probe":7}), budget.clone())
        .await
        .unwrap();
    assert_eq!(receipt.status, ReceiptStatus::Succeeded, "{receipt:?}");
    assert_eq!(receipt.result, Some(json!({"generation":1,"argument":7})));
    assert_eq!(
        budget.shared_steps().unwrap().remaining().unwrap(),
        18,
        "脚本帧和实际编译共享原额度"
    );
    spec.source = source(2);
    let second = runtime
        .publish_candidate(spec.clone(), ctx())
        .await
        .unwrap();
    store.activate(&owner, &second.reference, None).unwrap();
    let old = runtime
        .run(&handle, "cargo_old_handle", json!({"probe":8}), ctx())
        .await
        .unwrap();
    assert_eq!(
        old.result,
        Some(json!({"generation":1,"argument":8})),
        "激活新版不改已有句柄"
    );
    spec.source = "fn main(){not rust}".into();
    let failed = runtime.prepare_candidate(spec, ctx()).await.unwrap();
    assert!(matches!(failed, CandidatePreparation::CompilationFailed(_)));
    assert_eq!(store.summaries(&owner).unwrap().len(), 2);
    assert_eq!(
        store.selected(&owner, "cargo_reload").unwrap(),
        second.reference
    );
    drop(handle);
    drop(runtime);
    drop(store);
    let (store, runtime) = open();
    let selected = store.selected(&owner, "cargo_reload").unwrap();
    let new = runtime
        .run(
            &runtime.bind(&owner, &selected).unwrap(),
            "cargo_after_restart",
            json!({"probe":9}),
            ctx(),
        )
        .await
        .unwrap();
    assert_eq!(
        new.result,
        Some(json!({"generation":2,"argument":9})),
        "新运行器重建同一冻结摘要"
    );
    let mut damaged = store.version(&owner, &selected).unwrap().spec;
    let (header, rest) = damaged.source.split_once('\n').unwrap();
    let mut settings: serde_json::Value =
        serde_json::from_str(header.strip_prefix("// eve-cargo-v1 ").unwrap()).unwrap();
    settings["artifact_sha256"] = json!("0".repeat(64));
    damaged.source = format!("// eve-cargo-v1 {settings}\n{rest}");
    let bad = store.publish(&owner, damaged).unwrap();
    store.activate(&owner, &bad.reference, None).unwrap();
    let refused = runtime
        .run(
            &runtime.bind(&owner, &bad.reference).unwrap(),
            "cargo_digest_refused",
            json!({}),
            ctx(),
        )
        .await
        .unwrap();
    assert_ne!(refused.status, ReceiptStatus::Succeeded);
    assert!(refused.result.is_none());
    assert_eq!(
        store.selected(&owner, "cargo_reload").unwrap(),
        bad.reference,
        "不偷偷替换请求版本"
    );
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[tokio::test]
#[ignore = "显式配置可信Docker和bubblewrap后运行真实Cargo纯状态迁移"]
async fn actual_cargo_migration_preserves_failed_candidates_and_commits_only_verified_state() {
    use eve_script_runtime::*;
    use serde_json::json;
    use std::sync::Arc;
    let root = tempfile::tempdir().unwrap();
    let store = ScriptStore::open(root.path().join("state")).unwrap();
    let runtime = ScriptRuntime::new(
        store.clone(),
        Arc::new(NoTools),
        Bubblewrap::new("/usr/bin/python3", "/usr/bin/node").with_cargo(actual_compiler()),
        root.path().join("artifacts"),
    );
    let owner = context().scope().unwrap().clone();
    let ctx = || context().with_shared_steps(SharedToolSteps::new(20).unwrap());
    let first = store
        .publish(
            &owner,
            ScriptSpec {
                name: "cargo_state".into(),
                language: Language::Python,
                source: "import json\nx=json.loads(input())\nprint(json.dumps({'op':'return','value':x['state']['value']}))".into(),
                dependencies: BTreeMap::new(),
                tools: vec![],
                state_schema: StateSchema {
                    version: 1,
                    fields: BTreeMap::from([("count".into(), JsonType::Integer)]),
                },
                initial_state: json!({"count":7}),
            },
        )
        .unwrap();
    store.activate(&owner, &first.reference, None).unwrap();
    let mut next = first.spec.clone();
    next.state_schema = StateSchema {
        version: 2,
        fields: BTreeMap::from([
            ("count".into(), JsonType::Integer),
            ("label".into(), JsonType::String),
        ]),
    };
    next.initial_state = json!({"count":0,"label":""});
    let target = store.publish(&owner, next).unwrap();
    let settings = CargoSettings {
        dependencies: BTreeMap::from([(
            "serde_json".into(),
            std::env::var("EVE_TEST_CARGO_SERDE_JSON").unwrap(),
        )]),
        bundle_sha256: std::env::var("EVE_TEST_CARGO_BUNDLE_SHA256").unwrap(),
        build_source: None,
        artifact_sha256: None,
    };
    let source = r#"use std::io::{self,BufRead};fn main(){let mut line=String::new();io::stdin().lock().read_line(&mut line).unwrap();let input:serde_json::Value=serde_json::from_str(&line).unwrap();assert_eq!(input["mode"],"migration");println!("{}",serde_json::json!({"op":"return","value":{"count":input["state"]["value"]["count"].as_i64().unwrap()*2,"label":"cargo"}}));}"#;
    let migration = MigrationScript {
        from: first.reference.clone(),
        expected_revision: 0,
        language: Language::RustCargo(settings.clone()),
        source: source.into(),
    };
    let before = std::fs::read(root.path().join("state/ledger.json")).unwrap();
    let failed = MigrationScript {
        source: "fn main(){ invalid_rust }".into(),
        ..migration.clone()
    };
    assert!(
        runtime
            .activate_with_script(&target.reference, "cargo_bad_compile", failed, ctx())
            .await
            .is_err()
    );
    let rpc=MigrationScript{source:r#"fn main(){println!("{}",serde_json::json!({"op":"tool","name":"write_file","arguments":{}}));}"#.into(),..migration.clone()};
    assert!(
        runtime
            .activate_with_script(&target.reference, "cargo_forbidden_rpc", rpc, ctx())
            .await
            .is_err()
    );
    let pinned = MigrationScript {
        language: Language::RustCargo(CargoSettings {
            artifact_sha256: Some("0".repeat(64)),
            ..settings
        }),
        ..migration.clone()
    };
    assert!(
        runtime
            .activate_with_script(&target.reference, "cargo_bad_pin", pinned, ctx())
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(root.path().join("state/ledger.json")).unwrap(),
        before,
        "失败编译、非return和摘要不符都不提交状态或活动版"
    );
    assert_eq!(
        store.selected(&owner, "cargo_state").unwrap(),
        first.reference
    );
    let budget = ctx();
    let receipt = runtime
        .activate_with_script(
            &target.reference,
            "cargo_state_ok",
            migration.clone(),
            budget.clone(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.status, ReceiptStatus::Succeeded);
    assert_eq!(budget.shared_steps().unwrap().remaining().unwrap(), 18);
    assert_eq!(receipt.evidence.len(), 2);
    assert_eq!(receipt.evidence[1].operation, "migration_artifact");
    assert_eq!(receipt.evidence[1].target.len(), 64);
    assert_eq!(
        store.state(&owner, &target.reference).unwrap().value,
        json!({"count":14,"label":"cargo"})
    );
    assert_eq!(
        store.state(&owner, &first.reference).unwrap().value,
        json!({"count":7})
    );
    let budget = ctx();
    assert!(
        runtime
            .activate_with_script(
                &target.reference,
                "cargo_stale_again",
                migration,
                budget.clone()
            )
            .await
            .is_err()
    );
    assert_eq!(
        budget.shared_steps().unwrap().remaining().unwrap(),
        20,
        "旧状态迁移拒绝发生在编译前"
    );
    let bound = runtime.bind(&owner, &target.reference).unwrap();
    let result = runtime
        .run(&bound, "cargo_migrated_state_visible", json!({}), ctx())
        .await
        .unwrap();
    assert_eq!(result.status, ReceiptStatus::Succeeded);
    assert_eq!(result.result, Some(json!({"count":14,"label":"cargo"})));
    drop(bound);
    drop(runtime);
    drop(store);
    let store = ScriptStore::open(root.path().join("state")).unwrap();
    assert_eq!(store.receipt(&owner, "cargo_state_ok").unwrap(), receipt);
    assert_eq!(
        store.selected(&owner, "cargo_state").unwrap(),
        target.reference
    );
    store.activate(&owner, &first.reference, None).unwrap();
    assert_eq!(
        store.state(&owner, &first.reference).unwrap().value,
        json!({"count":7})
    );
    store.activate(&owner, &target.reference, None).unwrap();
    assert_eq!(
        store.state(&owner, &target.reference).unwrap().value,
        json!({"count":14,"label":"cargo"})
    );
}
