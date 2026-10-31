use eve_llm_api::{
    ContextScope, SharedToolSteps, ToolCall, ToolExecutionContext, ToolExecutionError, ToolFuture,
};
use eve_script_runtime::*;
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[cfg(target_os = "linux")]
use std::time::Instant;

struct Gate {
    allowed: AtomicBool,
    calls: AtomicUsize,
    unknown: AtomicBool,
    delay: AtomicBool,
    pause: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    revoke_after_first: AtomicBool,
    identities: Mutex<Vec<ContextScope>>,
    paths: Mutex<Vec<PathBuf>>,
}
impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            allowed: AtomicBool::new(true),
            calls: AtomicUsize::new(0),
            unknown: AtomicBool::new(false),
            delay: AtomicBool::new(false),
            pause: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            revoke_after_first: AtomicBool::new(false),
            identities: Mutex::new(vec![]),
            paths: Mutex::new(vec![]),
        })
    }
}
impl AuthorizedToolDispatch for Gate {
    fn preflight_tool(&self, name: &str, context: &ToolExecutionContext) -> Result<()> {
        if !self.allowed.load(Ordering::SeqCst)
            || name != "qq.send_file"
            || context.scope() != Some(&owner())
        {
            return Err(ToolExecutionError::Rejected("权限拒绝".into()));
        }
        Ok(())
    }
    fn preflight_call(&self, call: &ToolCall, context: &ToolExecutionContext) -> Result<()> {
        self.preflight_tool(&call.name, context)
    }
    fn dispatch(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            self.preflight_call(&call, &context)?;
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.identities
                .lock()
                .unwrap()
                .push(context.scope().unwrap().clone());
            if let Some(path) = call.arguments["path"].as_str() {
                assert!(Path::new(path).is_file());
                self.paths.lock().unwrap().push(path.into());
            }
            if self.pause.load(Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            if self.delay.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            if self.unknown.load(Ordering::SeqCst) {
                return Err(ToolExecutionError::Failed("外部响应丢失".into()));
            }
            if self.revoke_after_first.load(Ordering::SeqCst) {
                self.allowed.store(false, Ordering::SeqCst);
            }
            Ok(json!({"success":true,"simulated":true}))
        })
    }
}
fn owner() -> ContextScope {
    ContextScope {
        session_id: "session".into(),
        user_id: "owner".into(),
    }
}
fn context(limit: usize) -> ToolExecutionContext {
    ToolExecutionContext::new()
        .with_scope(Some(owner()))
        .with_shared_steps(SharedToolSteps::new(limit).unwrap())
}
#[cfg(target_os = "linux")]
fn trusted_runtime(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join(name))
        .find(|p| p.is_file())
        .unwrap_or_else(|| panic!("Linux 隔离验收缺少可信 PATH 运行时: {name}"))
        .canonicalize()
        .expect("运行时必须能解析为真实绝对路径")
}
#[cfg(target_os = "linux")]
fn sandbox() -> Bubblewrap {
    let mut sandbox = Bubblewrap::new(trusted_runtime("python3"), trusted_runtime("node"));
    sandbox.bwrap = trusted_runtime("bwrap");
    sandbox
}
fn spec(name: &str, source: &str) -> ScriptSpec {
    ScriptSpec {
        name: name.into(),
        language: Language::Python,
        source: source.into(),
        dependencies: BTreeMap::new(),
        tools: vec![],
        state_schema: StateSchema {
            version: 1,
            fields: BTreeMap::new(),
        },
        initial_state: json!({}),
    }
}
type Setup = (
    tempfile::TempDir,
    Arc<ScriptStore>,
    Arc<Gate>,
    ScriptRuntime,
);
fn setup() -> Setup {
    // 纯契约不依赖机器安装解释器/隔离器，也从不启动这个占位可执行文件。
    let executable = std::env::current_exe().unwrap();
    let mut sandbox = Bubblewrap::new(executable.clone(), executable.clone());
    sandbox.bwrap = executable;
    setup_with_sandbox(sandbox)
}
#[cfg(target_os = "linux")]
fn setup_isolated() -> Setup {
    setup_with_sandbox(sandbox())
}
fn setup_with_sandbox(sandbox: Bubblewrap) -> Setup {
    let root = tempfile::tempdir().unwrap();
    let store = ScriptStore::open(root.path().join("state")).unwrap();
    let gate = Gate::new();
    let runtime = ScriptRuntime::new(
        store.clone(),
        gate.clone(),
        sandbox,
        root.path().join("artifacts"),
    );
    (root, store, gate, runtime)
}
fn install(store: &ScriptStore, spec: ScriptSpec) -> VersionRef {
    let v = store.publish(&owner(), spec).unwrap();
    store.activate(&owner(), &v.reference, None).unwrap();
    v.reference
}
const RETURN_ONE: &str = "import json,sys\nx=json.loads(input())\nprint(json.dumps({'op':'return','value':1}),flush=True)\n";
#[cfg(target_os = "linux")]
const TOOL: &str = "import json,sys\nx=json.loads(input())\nprint(json.dumps({'op':'tool','name':'qq.send_file','arguments':{}}),flush=True)\ny=json.loads(input())\nprint(json.dumps({'op':'return','value':y}),flush=True)\n";

#[cfg(target_os = "linux")]
struct OperationGate {
    inner: Arc<Gate>,
    checks: AtomicUsize,
}
#[cfg(target_os = "linux")]
impl AuthorizedToolDispatch for OperationGate {
    fn preflight_tool(&self, name: &str, ctx: &ToolExecutionContext) -> Result<()> {
        self.inner.preflight_tool(name, ctx)
    }
    fn preflight_call(&self, call: &ToolCall, ctx: &ToolExecutionContext) -> Result<()> {
        self.preflight_tool(&call.name, ctx)?;
        self.checks.fetch_add(1, Ordering::SeqCst);
        match call.arguments["case"].as_str() {
            Some("denied") => Err(ToolExecutionError::Rejected("private policy detail".into())),
            Some("failure") => Err(ToolExecutionError::Failed("private backend detail".into())),
            Some("cancel") => {
                ctx.cancellation_handle().cancel();
                Ok(())
            }
            Some("revoke") => {
                self.inner.allowed.store(false, Ordering::SeqCst);
                Ok(())
            }
            _ => Ok(()),
        }
    }
    fn dispatch(&self, call: ToolCall, ctx: ToolExecutionContext) -> ToolFuture<'_> {
        self.inner.dispatch(call, ctx)
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn concrete_preflight_is_read_only_scoped_bounded_and_never_reserves_authorization() {
    let (root, store, gate, _) = setup_isolated();
    let checked = Arc::new(OperationGate {
        inner: gate.clone(),
        checks: AtomicUsize::new(0),
    });
    let runtime = ScriptRuntime::new(
        store.clone(),
        checked.clone(),
        sandbox(),
        root.path().join("artifacts"),
    );
    let source = r#"import json
x=json.loads(input())
for i in range(x['arguments'].get('repeat',1)):
 print(json.dumps({'op':'check_tool','name':x['arguments'].get('name','qq.send_file'),'arguments':{'case':x['arguments']['case']}}),flush=True)
 result=json.loads(input())['result']
if x['arguments'].get('execute',False):
 print(json.dumps({'op':'tool','name':'qq.send_file','arguments':{}}),flush=True)
 result=json.loads(input())['result']
print(json.dumps({'op':'return','value':result}),flush=True)
"#;
    let mut candidate = spec("permission-check", source);
    candidate.tools = vec!["qq.send_file".into()];
    let reference = install(&store, candidate);
    let bound = runtime.bind(&owner(), &reference).unwrap();
    for (id, args, expected, check_count, leaf_count) in [
        (
            "allowed",
            json!({"case":"allowed"}),
            ReceiptStatus::Succeeded,
            1,
            0,
        ),
        (
            "denied",
            json!({"case":"denied"}),
            ReceiptStatus::Succeeded,
            1,
            0,
        ),
        (
            "failure",
            json!({"case":"failure"}),
            ReceiptStatus::Failed,
            1,
            0,
        ),
        (
            "cancel",
            json!({"case":"cancel"}),
            ReceiptStatus::Cancelled,
            1,
            0,
        ),
        (
            "undeclared",
            json!({"case":"allowed","name":"undeclared"}),
            ReceiptStatus::Rejected,
            0,
            0,
        ),
        (
            "rpc-limit",
            json!({"case":"allowed","repeat":129}),
            ReceiptStatus::Rejected,
            128,
            0,
        ),
        (
            "execute",
            json!({"case":"allowed","execute":true}),
            ReceiptStatus::Succeeded,
            2,
            1,
        ),
        (
            "revoked",
            json!({"case":"revoke","execute":true}),
            ReceiptStatus::Rejected,
            1,
            0,
        ),
    ] {
        let before_checks = checked.checks.load(Ordering::SeqCst);
        let before_calls = gate.calls.load(Ordering::SeqCst);
        let ctx = context(2);
        let receipt = runtime.run(&bound, id, args, ctx.clone()).await.unwrap();
        assert_eq!(receipt.status, expected, "{id}: {receipt:?}");
        assert_eq!(
            checked.checks.load(Ordering::SeqCst) - before_checks,
            check_count,
            "{id}"
        );
        assert_eq!(
            gate.calls.load(Ordering::SeqCst) - before_calls,
            leaf_count,
            "{id}"
        );
        assert_eq!(
            ctx.shared_steps().unwrap().remaining().unwrap(),
            1 - leaf_count,
            "{id}"
        );
        assert_eq!(
            receipt.evidence.len(),
            leaf_count,
            "预检不能伪造工具执行证据: {id}"
        );
        assert!(!serde_json::to_string(&receipt).unwrap().contains("private"));
        if matches!(id, "allowed" | "denied") {
            assert_eq!(
                receipt.result.unwrap(),
                json!({"allowed":id=="allowed","executed":false,
                "authorization_reserved":false,"reason":if id=="allowed" {"preflight_passed"} else {"rejected"}})
            );
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn python_calls_javascript_and_host_delivery_with_shared_identity_budget_and_temp_lifecycle()
{
    let (root, store, gate, runtime) = setup_isolated();
    let mut child = spec("report", include_str!("../examples/plugins/report.js"));
    child.language = Language::Javascript;
    child.tools = vec!["qq.send_file".into()];
    child
        .state_schema
        .fields
        .insert("count".into(), JsonType::Integer);
    child.initial_state = json!({"count":0});
    let child = install(&store, child);
    let mut parent = spec("parent", include_str!("../examples/plugins/main.py"));
    parent.dependencies.insert("report".into(), child.clone());
    let parent = install(&store, parent);
    let bound = runtime.bind(&owner(), &parent).unwrap();
    let ctx = context(3);
    let before = std::fs::read(root.path().join("state/ledger.json")).unwrap();
    let preflight = runtime.preflight(&bound, &ctx).unwrap();
    assert!(preflight.dry_run);
    assert_eq!(preflight.versions.len(), 2);
    assert_eq!(ctx.shared_steps().unwrap().remaining().unwrap(), 3);
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        before,
        std::fs::read(root.path().join("state/ledger.json")).unwrap()
    );
    let receipt = runtime
        .run(
            &bound,
            "cross-process",
            json!({"title":"测试"}),
            ctx.clone(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.status, ReceiptStatus::Succeeded, "{receipt:?}");
    assert_eq!(ctx.shared_steps().unwrap().remaining().unwrap(), 0);
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    assert_eq!(*gate.identities.lock().unwrap(), vec![owner()]);
    assert_eq!(
        store.state(&owner(), &child).unwrap().value,
        json!({"count":1})
    );
    let child_receipt = store
        .receipt(&owner(), &receipt.evidence[0].target)
        .unwrap();
    assert_eq!(child_receipt.parent_call_id, Some("cross-process".into()));
    assert_eq!(child_receipt.root_call_id, "cross-process");
    assert!(
        root.path()
            .join("artifacts/cross-process/report.txt")
            .is_file()
    );
    for path in gate.paths.lock().unwrap().iter() {
        assert!(!path.exists(), "调用链结束必须删除临时文件");
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn immutable_handles_survive_hot_switch_and_rollback_keeps_independent_state() {
    let (_root, store, _gate, runtime) = setup_isolated();
    let first = install(&store, spec("versioned", RETURN_ONE));
    let old = runtime.bind(&owner(), &first).unwrap();
    let second = install(
        &store,
        spec("versioned", &RETURN_ONE.replace("'value':1", "'value':2")),
    );
    let new = runtime.bind(&owner(), &second).unwrap();
    assert_ne!(first, second);
    assert_eq!(
        runtime
            .run(&old, "old", json!({}), context(1))
            .await
            .unwrap()
            .result,
        Some(json!(1))
    );
    assert_eq!(
        runtime
            .run(&new, "new", json!({}), context(1))
            .await
            .unwrap()
            .result,
        Some(json!(2))
    );
    store.activate(&owner(), &first, None).unwrap();
    assert_eq!(store.selected(&owner(), "versioned").unwrap(), first);
    assert_eq!(
        runtime
            .run(&new, "still-new", json!({}), context(1))
            .await
            .unwrap()
            .result,
        Some(json!(2))
    );
}

#[test]
fn schema_changes_require_explicit_revision_checked_migration_and_rollback_preserves_snapshots() {
    let (_root, store, _gate, _runtime) = setup();
    let first = install(&store, spec("state", RETURN_ONE));
    let mut next = spec("state", RETURN_ONE);
    next.state_schema.version = 2;
    next.state_schema
        .fields
        .insert("label".into(), JsonType::String);
    next.initial_state = json!({"label":"initial"});
    let next = store.publish(&owner(), next).unwrap().reference;
    assert!(store.activate(&owner(), &next, None).is_err());
    assert!(
        store
            .activate(
                &owner(),
                &next,
                Some(StateMigration {
                    from: first.clone(),
                    expected_revision: 1,
                    value: json!({"label":"migrated"})
                })
            )
            .is_err()
    );
    store
        .activate(
            &owner(),
            &next,
            Some(StateMigration {
                from: first.clone(),
                expected_revision: 0,
                value: json!({"label":"migrated"}),
            }),
        )
        .unwrap();
    assert_eq!(
        store.state(&owner(), &next).unwrap().value,
        json!({"label":"migrated"})
    );
    store.activate(&owner(), &first, None).unwrap();
    assert_eq!(store.state(&owner(), &first).unwrap().value, json!({}));
    assert!(
        store
            .activate(
                &owner(),
                &next,
                Some(StateMigration {
                    from: first,
                    expected_revision: 0,
                    value: json!({"label":"overwrite"})
                })
            )
            .is_err()
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn permissions_and_budget_reject_before_leaf_side_effects_and_untrusted_identity_cannot_bind()
{
    let (_root, store, gate, runtime) = setup_isolated();
    let mut source = spec("delivery", TOOL);
    source.tools = vec!["qq.send_file".into()];
    let version = install(&store, source);
    let bound = runtime.bind(&owner(), &version).unwrap();
    gate.allowed.store(false, Ordering::SeqCst);
    assert!(runtime.preflight(&bound, &context(2)).is_err());
    assert!(
        runtime
            .run(&bound, "denied", json!({}), context(2))
            .await
            .is_err()
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
    gate.allowed.store(true, Ordering::SeqCst);
    let receipt = runtime
        .run(&bound, "budget", json!({}), context(1))
        .await
        .unwrap();
    assert_eq!(receipt.status, ReceiptStatus::Rejected);
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
    let stranger = ContextScope {
        session_id: "other".into(),
        ..owner()
    };
    assert!(runtime.bind(&stranger, &version).is_err());
    let ctx = context(2).with_scope(Some(stranger));
    assert!(
        runtime
            .run(&bound, "wrong-owner", json!({}), ctx)
            .await
            .is_err()
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn undeclared_host_tool_and_undeclared_dependency_are_closed() {
    let (_root, store, gate, runtime) = setup_isolated();
    let version = install(&store, spec("undeclared", TOOL));
    let bound = runtime.bind(&owner(), &version).unwrap();
    assert_eq!(
        runtime
            .run(&bound, "undeclared-call", json!({}), context(2))
            .await
            .unwrap()
            .status,
        ReceiptStatus::Rejected
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
    let source = "import json\ninput()\nprint(json.dumps({'op':'script','dependency':'unknown','arguments':{}}),flush=True)\ninput()";
    let version = install(&store, spec("undeclared-dependency", source));
    let bound = runtime.bind(&owner(), &version).unwrap();
    assert_eq!(
        runtime
            .run(&bound, "undeclared-script", json!({}), context(2))
            .await
            .unwrap()
            .status,
        ReceiptStatus::Rejected
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn isolation_hides_host_files_credentials_and_network_and_work_is_readonly() {
    let (root, store, _gate, runtime) = setup_isolated();
    let canary = root.path().join("credential-canary");
    std::fs::write(&canary, "secret").unwrap();
    let source = format!(
        "import os,json,socket\nx=json.loads(input())\nassert not os.path.exists({})\nassert 'HOME' not in os.environ or os.environ['HOME']=='/tmp'\nassert not os.path.exists('/workspace')\ntry:\n open('/work/native-write','w').write('bad')\n raise AssertionError('work writable')\nexcept OSError: pass\ns=socket.socket()\ns.settimeout(.1)\ntry:\n s.connect(('1.1.1.1',443))\n raise AssertionError('network available')\nexcept OSError: pass\nprint(json.dumps({{'op':'return','value':{{'isolated':True}}}}),flush=True)\n",
        serde_json::to_string(&canary.to_str().unwrap()).unwrap()
    );
    let version = install(&store, spec("isolation", &source));
    let bound = runtime.bind(&owner(), &version).unwrap();
    let receipt = runtime
        .run(&bound, "isolation-call", json!({}), context(1))
        .await
        .unwrap();
    assert_eq!(receipt.status, ReceiptStatus::Succeeded, "{receipt:?}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn seccomp_denies_native_process_creation_and_resource_limits_bound_memory() {
    let (_root, store, _gate, runtime) = setup_isolated();
    let source = "import json,os,resource\ninput()\nassert resource.getrlimit(resource.RLIMIT_AS)[0] <= 1024**3\nassert resource.getrlimit(resource.RLIMIT_NOFILE)[0] <= 64\ntry:\n pid=os.fork()\n if pid==0: os._exit(1)\n os.waitpid(pid,0)\n raise AssertionError('fork permitted')\nexcept OSError: pass\ntry:\n bytearray(2*1024**3)\n raise AssertionError('memory limit missing')\nexcept MemoryError: pass\nprint(json.dumps({'op':'return','value':{'bounded':True}}),flush=True)\n";
    let version = install(&store, spec("limits", source));
    let bound = runtime.bind(&owner(), &version).unwrap();
    let receipt = runtime
        .run(&bound, "limits-call", json!({}), context(1))
        .await
        .unwrap();
    assert_eq!(receipt.status, ReceiptStatus::Succeeded, "{receipt:?}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn unknown_effects_survive_restart_and_cannot_be_replayed_or_migrated() {
    let (root, store, gate, runtime) = setup_isolated();
    gate.unknown.store(true, Ordering::SeqCst);
    let mut source = spec("unknown", TOOL);
    source.tools = vec!["qq.send_file".into()];
    let version = install(&store, source);
    let bound = runtime.bind(&owner(), &version).unwrap();
    let receipt = runtime
        .run(&bound, "uncertain", json!({}), context(2))
        .await
        .unwrap();
    assert_eq!(receipt.status, ReceiptStatus::Unknown);
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    assert!(
        runtime
            .run(&bound, "uncertain", json!({}), context(2))
            .await
            .is_err()
    );
    assert!(
        runtime
            .run(&bound, "new-id", json!({}), context(2))
            .await
            .is_err()
    );
    let mut next = spec("unknown", RETURN_ONE);
    next.state_schema.version = 2;
    let next = store.publish(&owner(), next).unwrap().reference;
    assert!(
        store
            .activate(
                &owner(),
                &next,
                Some(StateMigration {
                    from: version.clone(),
                    expected_revision: 0,
                    value: json!({})
                })
            )
            .is_err()
    );
    drop(runtime);
    drop(store);
    let reopened = ScriptStore::open(root.path().join("state")).unwrap();
    assert_eq!(
        reopened.receipt(&owner(), "uncertain").unwrap().status,
        ReceiptStatus::Unknown
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    reopened
        .acknowledge_unknown(&owner(), "uncertain", "人工核对实际交付结果，保留失败记录")
        .unwrap();
    assert_eq!(
        reopened.receipt(&owner(), "uncertain").unwrap().status,
        ReceiptStatus::Failed
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancellation_during_external_dispatch_marks_unknown_and_stops_entire_chain() {
    let (_root, store, gate, runtime) = setup_isolated();
    gate.delay.store(true, Ordering::SeqCst);
    let mut source = spec("cancel", TOOL);
    source.tools = vec!["qq.send_file".into()];
    let version = install(&store, source);
    let bound = runtime.bind(&owner(), &version).unwrap();
    let ctx = context(2);
    let cancel = ctx.cancellation_handle();
    let calls = gate.clone();
    let trigger = tokio::spawn(async move {
        while calls.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        cancel.cancel();
    });
    let started = Instant::now();
    let receipt = runtime
        .run(&bound, "cancel-call", json!({}), ctx)
        .await
        .unwrap();
    trigger.await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(receipt.status, ReceiptStatus::Unknown);
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn deadline_kills_hung_process_without_waiting_for_native_sleep() {
    let (_root, store, _gate, runtime) = setup_isolated();
    let source = "import time\ninput()\ntime.sleep(600)\n";
    let version = install(&store, spec("hung", source));
    let bound = runtime.bind(&owner(), &version).unwrap();
    let started = Instant::now();
    let receipt = runtime
        .run(
            &bound,
            "hung-call",
            json!({}),
            context(1).with_deadline(Some(Instant::now() + Duration::from_millis(100))),
        )
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(receipt.status, ReceiptStatus::Cancelled);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn missing_or_failing_isolator_never_falls_back_to_native_exec() {
    let (root, store, gate, _runtime) = setup_isolated();
    let mut source = spec("no-isolation", TOOL);
    source.tools = vec!["qq.send_file".into()];
    let version = install(&store, source);
    let mut unavailable = sandbox();
    unavailable.bwrap = "/no/bwrap".into();
    let runtime = ScriptRuntime::new(
        store.clone(),
        gate.clone(),
        unavailable,
        root.path().join("artifacts"),
    );
    let bound = runtime.bind(&owner(), &version).unwrap();
    assert!(
        runtime
            .run(&bound, "no-backend", json!({}), context(2))
            .await
            .is_err()
    );
    let mut failing = sandbox();
    failing.bwrap = trusted_runtime("false");
    let runtime = ScriptRuntime::new(store, gate.clone(), failing, root.path().join("artifacts"));
    let bound = runtime.bind(&owner(), &version).unwrap();
    assert_ne!(
        runtime
            .run(&bound, "backend-denied", json!({}), context(2))
            .await
            .unwrap()
            .status,
        ReceiptStatus::Succeeded
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn corrupt_duplicate_unknown_format_and_missing_ledger_are_preserved_and_refused() {
    for bytes in [
        b"not-json".as_slice(),
        b"{\"format\":1,\"format\":1}".as_slice(),
        b"{\"format\":99,\"checksum\":\"x\",\"ledger\":{}}".as_slice(),
    ] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("ledger.json"), bytes).unwrap();
        assert!(ScriptStore::open(root.path()).is_err());
        assert_eq!(
            std::fs::read(root.path().join("ledger.json")).unwrap(),
            bytes
        );
    }
    let root = tempfile::tempdir().unwrap();
    let store = ScriptStore::open(root.path()).unwrap();
    assert!(ScriptStore::open(root.path()).is_err());
    drop(store);
    std::fs::remove_file(root.path().join("ledger.json")).unwrap();
    assert!(ScriptStore::open(root.path()).is_err());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn malformed_protocol_and_traversal_do_not_promote_or_write_outside_chain() {
    let (root, store, _gate, runtime) = setup_isolated();
    for (name, source) in [
        (
            "duplicate",
            "input()\nprint('{\"op\":\"return\",\"op\":\"return\",\"value\":1}',flush=True)\n",
        ),
        (
            "traversal",
            "input()\nprint('{\"op\":\"write\",\"path\":\"../escaped\",\"content\":\"bad\"}',flush=True)\ninput()\n",
        ),
    ] {
        let version = install(&store, spec(name, source));
        let bound = runtime.bind(&owner(), &version).unwrap();
        let receipt = runtime
            .run(&bound, name, json!({}), context(1))
            .await
            .unwrap();
        assert_ne!(receipt.status, ReceiptStatus::Succeeded);
    }
    assert!(!root.path().join("artifacts").exists());
    assert!(!root.path().join("escaped").exists());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn dropping_a_running_future_keeps_unconfirmed_receipt_and_restart_never_replays() {
    let (root, store, gate, runtime) = setup_isolated();
    let version = install(
        &store,
        spec("drop-future", "import time\ninput()\ntime.sleep(600)\n"),
    );
    let bound = runtime.bind(&owner(), &version).unwrap();
    {
        let future = runtime.run(&bound, "dropped", json!({}), context(1));
        tokio::pin!(future);
        tokio::select! { _ = &mut future => panic!("unexpected completion"), _ = tokio::time::sleep(Duration::from_millis(100)) => {} }
    }
    assert_eq!(
        store.receipt(&owner(), "dropped").unwrap().status,
        ReceiptStatus::Unknown
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
    drop(runtime);
    drop(store);
    let reopened = ScriptStore::open(root.path().join("state")).unwrap();
    assert_eq!(
        reopened.receipt(&owner(), "dropped").unwrap().status,
        ReceiptStatus::Unknown
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn concurrent_roots_share_one_atomic_budget_without_overspending() {
    let (_root, store, gate, runtime) = setup_isolated();
    let mut first = spec("parallel-one", TOOL);
    first.tools = vec!["qq.send_file".into()];
    let first = install(&store, first);
    let mut second = spec("parallel-two", TOOL);
    second.tools = vec!["qq.send_file".into()];
    let second = install(&store, second);
    let first = runtime.bind(&owner(), &first).unwrap();
    let second = runtime.bind(&owner(), &second).unwrap();
    let ctx = context(3);
    let (one, two) = tokio::join!(
        runtime.run(&first, "parallel-call-one", json!({}), ctx.clone()),
        runtime.run(&second, "parallel-call-two", json!({}), ctx.clone())
    );
    let statuses = [one.unwrap().status, two.unwrap().status];
    assert!(statuses.contains(&ReceiptStatus::Succeeded));
    assert!(statuses.contains(&ReceiptStatus::Rejected));
    assert_eq!(ctx.shared_steps().unwrap().usage().unwrap().tool_calls, 3);
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn permission_revocation_after_first_leaf_stops_following_leaf_with_partial_effect_evidence()
{
    let (_root, store, gate, runtime) = setup_isolated();
    gate.revoke_after_first.store(true, Ordering::SeqCst);
    let source = "import json\ninput()\nfor i in range(2):\n print(json.dumps({'op':'tool','name':'qq.send_file','arguments':{}}),flush=True)\n input()\nprint(json.dumps({'op':'return','value':True}),flush=True)\n";
    let mut source = spec("revoke", source);
    source.tools = vec!["qq.send_file".into()];
    let version = install(&store, source);
    let bound = runtime.bind(&owner(), &version).unwrap();
    let receipt = runtime
        .run(&bound, "revoke-call", json!({}), context(3))
        .await
        .unwrap();
    assert_eq!(receipt.status, ReceiptStatus::Failed);
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    assert_eq!(receipt.evidence[0].status, ReceiptStatus::Succeeded);
}

#[test]
fn process_lock_probe() {
    if let Some(path) = std::env::var_os("EVE_SCRIPT_LOCK_PROBE") {
        assert!(
            ScriptStore::open(path).is_err(),
            "另一个宿主进程不能绕过独占锁"
        );
    }
}

#[test]
fn separate_host_process_cannot_open_the_same_ledger() {
    let root = tempfile::tempdir().unwrap();
    let _store = ScriptStore::open(root.path()).unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "process_lock_probe"])
        .env("EVE_SCRIPT_LOCK_PROBE", root.path())
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn portable_version_identity_and_atomic_budget_contracts_remain_available_without_execution() {
    let (_root, store, _gate, runtime) = setup();
    let first = install(&store, spec("portable", RETURN_ONE));
    let bound = runtime.bind(&owner(), &first).unwrap();
    let second = install(
        &store,
        spec("portable", &RETURN_ONE.replace("'value':1", "'value':2")),
    );
    assert_ne!(first, second);
    assert_eq!(bound.reference(), &first);
    assert_eq!(
        store.version(&owner(), &first).unwrap().spec.source,
        RETURN_ONE
    );
    let stranger = ContextScope {
        user_id: "stranger".into(),
        ..owner()
    };
    assert!(store.version(&stranger, &first).is_err());
    assert!(runtime.bind(&stranger, &first).is_err());
    let budget = SharedToolSteps::new(4).unwrap();
    let threads = (0..8)
        .map(|_| {
            let budget = budget.clone();
            std::thread::spawn(move || budget.reserve(1).unwrap())
        })
        .collect::<Vec<_>>();
    let admitted = threads
        .into_iter()
        .filter_map(|t| t.join().unwrap().then_some(()))
        .count();
    assert_eq!(admitted, 4);
    assert_eq!(budget.remaining().unwrap(), 0);
}

#[tokio::test]
async fn portable_dispatch_rechecks_revoked_permission_without_starting_a_process() {
    let gate = Gate::new();
    let ctx = context(2);
    let call = ToolCall {
        id: "portable-gate".into(),
        name: "qq.send_file".into(),
        arguments: json!({}),
    };
    gate.preflight_call(&call, &ctx).unwrap();
    gate.allowed.store(false, Ordering::SeqCst);
    assert!(gate.dispatch(call, ctx).await.is_err());
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
}

#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn non_linux_execution_fails_closed_without_ledger_budget_or_native_side_effects() {
    let (root, store, gate, runtime) = setup();
    let version = install(&store, spec("unsupported", RETURN_ONE));
    let bound = runtime.bind(&owner(), &version).unwrap();
    let ctx = context(2);
    let before = std::fs::read(root.path().join("state/ledger.json")).unwrap();
    let preview = runtime.preflight(&bound, &ctx).unwrap_err();
    assert!(matches!(preview, ToolExecutionError::Rejected(_)));
    assert!(preview.to_string().contains("Linux"));
    let result = runtime
        .run(&bound, "unsupported-native-call", json!({}), ctx.clone())
        .await;
    assert!(matches!(result, Err(ToolExecutionError::Rejected(_))));
    assert_eq!(ctx.shared_steps().unwrap().remaining().unwrap(), 2);
    assert_eq!(
        before,
        std::fs::read(root.path().join("state/ledger.json")).unwrap()
    );
    assert!(store.receipt(&owner(), "unsupported-native-call").is_err());
    assert!(!root.path().join("artifacts").exists());
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn production_protocol_example_writes_member_export_and_keeps_pending_delivery_unconfirmed() {
    struct MemberLeaves {
        root: PathBuf,
        names: Mutex<Vec<String>>,
    }
    impl AuthorizedToolDispatch for MemberLeaves {
        fn preflight_tool(&self, name: &str, ctx: &ToolExecutionContext) -> Result<()> {
            if ctx.scope() != Some(&owner())
                || !["write_file", "send_file", "query_file_delivery"].contains(&name)
            {
                return Err(ToolExecutionError::Rejected("权限拒绝".into()));
            }
            Ok(())
        }
        fn preflight_call(&self, call: &ToolCall, ctx: &ToolExecutionContext) -> Result<()> {
            self.preflight_tool(&call.name, ctx)
        }
        fn dispatch(&self, call: ToolCall, ctx: ToolExecutionContext) -> ToolFuture<'_> {
            Box::pin(async move {
                self.preflight_call(&call, &ctx)?;
                self.names.lock().unwrap().push(call.name.clone());
                match call.name.as_str() {
                    "write_file" => {
                        let relative = call.arguments["path"].as_str().unwrap();
                        assert!(relative.starts_with("exports/report-"));
                        assert_eq!(call.arguments["expected_sha256"], "");
                        std::fs::write(
                            self.root.join(relative),
                            call.arguments["content"].as_str().unwrap(),
                        )
                        .unwrap();
                        Ok(json!({"success":true}))
                    }
                    "send_file" => {
                        assert!(
                            self.root
                                .join(call.arguments["path"].as_str().unwrap())
                                .is_file()
                        );
                        Ok(json!({"id":"local-delivery","queued":true}))
                    }
                    _ => {
                        assert_eq!(call.arguments["id"], "local-delivery");
                        Ok(
                            json!({"sent":false,"status":"Pending","message_id":null,"session_id":null}),
                        )
                    }
                }
            })
        }
    }
    let (root, store, _gate, _runtime) = setup_isolated();
    std::fs::create_dir_all(root.path().join("project/exports")).unwrap();
    let leaves = Arc::new(MemberLeaves {
        root: root.path().join("project"),
        names: Mutex::new(vec![]),
    });
    let runtime = ScriptRuntime::new(
        store.clone(),
        leaves.clone(),
        sandbox(),
        root.path().join("artifacts"),
    );
    let mut source = spec(
        "eve-report",
        include_str!("../examples/plugins/eve_report.js"),
    );
    source.language = Language::Javascript;
    source.tools = vec![
        "write_file".into(),
        "send_file".into(),
        "query_file_delivery".into(),
    ];
    let version = install(&store, source);
    let bound = runtime.bind(&owner(), &version).unwrap();
    let receipt = runtime
        .run(
            &bound,
            "eve-report-call",
            json!({"title":"成员报告"}),
            context(4),
        )
        .await
        .unwrap();
    assert_eq!(receipt.status, ReceiptStatus::Succeeded, "{receipt:?}");
    let result = receipt.result.unwrap();
    assert_eq!(result["delivered"], false);
    assert_eq!(result["delivery"]["status"], "Pending");
    assert_eq!(
        *leaves.names.lock().unwrap(),
        vec!["write_file", "send_file", "query_file_delivery"]
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn migration_script_computes_state_and_commits_audit_surviving_restart() {
    let (root, store, gate, runtime) = setup_isolated();
    let mut old = spec("migrate-function", RETURN_ONE);
    old.state_schema
        .fields
        .insert("count".into(), JsonType::Integer);
    old.initial_state = json!({"count":7});
    let from = install(&store, old.clone());
    old.state_schema.version = 2;
    old.state_schema
        .fields
        .insert("label".into(), JsonType::String);
    old.initial_state = json!({"count":0,"label":""});
    let to = store.publish(&owner(), old).unwrap().reference;
    let receipt = runtime.activate_with_script(&to, "migrate-success", MigrationScript {
        from: from.clone(), expected_revision: 0, language: Language::Python,
        source: "import json\nx=json.loads(input())\nv=x['state']['value']\nv['count']+=1\nv['label']='converted'\nprint(json.dumps({'op':'return','value':v}))".into(),
    }, context(1)).await.unwrap();
    assert_eq!(receipt.status, ReceiptStatus::Succeeded);
    assert_eq!(
        store.state(&owner(), &to).unwrap().value,
        json!({"count":8,"label":"converted"})
    );
    assert_eq!(
        store.state(&owner(), &from).unwrap().value,
        json!({"count":7})
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
    drop(runtime);
    drop(store);
    let reopened = ScriptStore::open(root.path().join("state")).unwrap();
    assert_eq!(reopened.selected(&owner(), &to.script).unwrap(), to);
    assert_eq!(
        reopened.receipt(&owner(), "migrate-success").unwrap(),
        receipt
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn migration_script_rejects_side_effects_bad_state_exit_and_deadline() {
    let (root, store, gate, runtime) = setup_isolated();
    let from = install(&store, spec("migrate-rejected", RETURN_ONE));
    let mut next = spec("migrate-rejected", RETURN_ONE);
    next.state_schema.version = 2;
    let to = store.publish(&owner(), next).unwrap().reference;
    let cases = [
        "print('{\"op\":\"tool\",\"name\":\"qq.send_file\",\"arguments\":{}}')",
        "print('{\"op\":\"write\",\"path\":\"x\",\"content\":\"x\"}')",
        "print('{\"op\":\"script\",\"dependency\":\"x\",\"arguments\":{}}')",
        "print('{\"op\":\"return\",\"value\":{\"extra\":1}}')",
        "print('{\"op\":\"return\",\"value\":{}}'); raise RuntimeError()",
        "print('{\"op\":\"return\",\"value\":{}}'); print('extra')",
        "import time;time.sleep(5)",
    ];
    for (index, source) in cases.into_iter().enumerate() {
        let result = runtime
            .activate_with_script(
                &to,
                &format!("bad-migration-{index}"),
                MigrationScript {
                    from: from.clone(),
                    expected_revision: 0,
                    language: Language::Python,
                    source: source.into(),
                },
                context(1).with_deadline(Some(Instant::now() + Duration::from_millis(250))),
            )
            .await;
        assert!(result.is_err(), "case {index}");
        assert_eq!(store.selected(&owner(), &from.script).unwrap(), from);
        assert!(store.state(&owner(), &to).is_err());
    }
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime
            .run(
                &runtime.bind(&owner(), &from).unwrap(),
                "still-usable",
                json!({}),
                context(1)
            )
            .await
            .unwrap()
            .status,
        ReceiptStatus::Succeeded
    );
    drop(runtime);
    drop(store);
    let reopened = ScriptStore::open(root.path().join("state")).unwrap();
    assert_eq!(reopened.selected(&owner(), &from.script).unwrap(), from);
    assert!(reopened.state(&owner(), &to).is_err());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn migration_script_cas_rejects_concurrent_source_update() {
    let (_root, store, _, runtime) = setup_isolated();
    let from = install(
        &store,
        spec(
            "migrate-cas",
            "import json\nx=json.loads(input())\nprint(json.dumps({'op':'state','value':{}}),flush=True)\ninput()\nprint(json.dumps({'op':'return','value':1}))",
        ),
    );
    let mut next = spec("migrate-cas", RETURN_ONE);
    next.state_schema.version = 2;
    let to = store.publish(&owner(), next).unwrap().reference;
    let migration = runtime.activate_with_script(&to, "cas-migration", MigrationScript {
        from: from.clone(), expected_revision: 0, language: Language::Python,
        source: "import json,time\nx=json.loads(input())\ntime.sleep(0.5)\nprint(json.dumps({'op':'return','value':{}}))".into(),
    }, context(1));
    let bound = runtime.bind(&owner(), &from).unwrap();
    let update = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        runtime
            .run(&bound, "concurrent-update", json!({}), context(1))
            .await
            .unwrap()
    };
    let (result, updated) = tokio::join!(migration, update);
    assert_eq!(updated.status, ReceiptStatus::Succeeded);
    assert!(result.is_err());
    assert!(store.state(&owner(), &to).is_err());
    assert_eq!(store.selected(&owner(), &from.script).unwrap(), from);
    assert_eq!(store.state(&owner(), &from).unwrap().revision, 1);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn migration_javascript_and_budget_identity_checks() {
    let (_root, store, _, runtime) = setup_isolated();
    let from = install(&store, spec("migrate-js", RETURN_ONE));
    let mut next = spec("migrate-js", RETURN_ONE);
    next.state_schema.version = 2;
    let to = store.publish(&owner(), next).unwrap().reference;
    let migration = MigrationScript { from: from.clone(), expected_revision: 0,
        language: Language::Javascript, source: "const fs=require('fs'); const x=JSON.parse(fs.readFileSync(0,'utf8')); console.log(JSON.stringify({op:'return',value:x.state.value}));".into() };
    let exhausted = context(1);
    assert!(exhausted.shared_steps().unwrap().reserve(1).unwrap());
    assert!(
        runtime
            .activate_with_script(&to, "budget-migration", migration.clone(), exhausted)
            .await
            .is_err()
    );
    let wrong = context(1).with_scope(Some(ContextScope {
        session_id: "other".into(),
        user_id: "other".into(),
    }));
    assert!(
        runtime
            .activate_with_script(&to, "wrong-migration", migration.clone(), wrong)
            .await
            .is_err()
    );
    let stale = MigrationScript {
        expected_revision: 1,
        ..migration.clone()
    };
    assert!(
        runtime
            .activate_with_script(&to, "stale-migration", stale, context(1))
            .await
            .is_err()
    );
    assert!(store.state(&owner(), &to).is_err());
    runtime
        .activate_with_script(&to, "js-migration", migration.clone(), context(1))
        .await
        .unwrap();
    assert!(
        runtime
            .activate_with_script(&to, "duplicate-migration", migration, context(1))
            .await
            .is_err()
    );
    assert_eq!(store.state(&owner(), &to).unwrap().value, json!({}));
}

#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn migration_execution_fails_closed_without_consuming_budget() {
    let (_root, store, gate, runtime) = setup();
    let from = install(&store, spec("migration-platform", RETURN_ONE));
    let mut next = spec("migration-platform", RETURN_ONE);
    next.state_schema.version = 2;
    let to = store.publish(&owner(), next).unwrap().reference;
    let ctx = context(1);
    let budget = ctx.shared_steps().unwrap().clone();
    assert!(
        runtime
            .activate_with_script(
                &to,
                "unsupported-migration",
                MigrationScript {
                    from: from.clone(),
                    expected_revision: 0,
                    language: Language::Python,
                    source: "print('{\"op\":\"return\",\"value\":{}}')".into(),
                },
                ctx
            )
            .await
            .is_err()
    );
    assert!(budget.reserve(1).unwrap());
    assert_eq!(store.selected(&owner(), &from.script).unwrap(), from);
    assert!(store.state(&owner(), &to).is_err());
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
}

/// 必须由安装了Linux容器daemon的真实宿主显式执行；不以mock或跳过证明跨平台。
#[tokio::test]
#[ignore = "requires trusted EVE_TEST_CONTAINER_CLI and EVE_TEST_CONTAINER_IMAGE"]
async fn container_backend_real_python_javascript_isolation_and_migration() {
    let executable = std::env::current_exe().unwrap();
    let container = ContainerSandbox {
        cli: std::env::var_os("EVE_TEST_CONTAINER_CLI")
            .expect("trusted container CLI required")
            .into(),
        image: std::env::var("EVE_TEST_CONTAINER_IMAGE").expect("immutable local image required"),
    };
    let (_root, store, gate, runtime) = setup_with_sandbox(
        Bubblewrap::new(executable.clone(), executable).with_container(container),
    );
    let source = "import json,os,socket\nx=json.loads(input())\nassert os.getuid()!=0\nassert not os.path.exists('/var/run/docker.sock')\nassert not os.path.exists('/workspace')\nassert 'HTTP_PROXY' not in os.environ\ntry:\n open('/work/forbidden','w').write('x')\n raise Exception('writable work')\nexcept OSError: pass\ns=socket.socket();s.settimeout(.1)\ntry:\n s.connect(('1.1.1.1',443));raise Exception('external network')\nexcept OSError: pass\nprint(json.dumps({'op':'return','value':{'isolated':True}}))";
    let from = install(&store, spec("container-real", source));
    let receipt = runtime
        .run(
            &runtime.bind(&owner(), &from).unwrap(),
            "container-run",
            json!({}),
            context(1),
        )
        .await
        .unwrap();
    assert_eq!(receipt.status, ReceiptStatus::Succeeded, "{receipt:?}");
    let mut next = spec("container-real", source);
    next.state_schema.version = 2;
    let to = store.publish(&owner(), next).unwrap().reference;
    runtime.activate_with_script(&to, "container-migration", MigrationScript { from,
        expected_revision: 0, language: Language::Javascript,
        source: "const fs=require('fs');let x=JSON.parse(fs.readFileSync(0,'utf8'));console.log(JSON.stringify({op:'return',value:x.state.value}));".into(),
    }, context(1)).await.unwrap();
    assert_eq!(store.selected(&owner(), &to.script).unwrap(), to);
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
}

#[cfg(target_os = "linux")]
fn rust_sandbox() -> Bubblewrap {
    let output = std::process::Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let sysroot = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    sandbox().with_rustc(sysroot.join("bin/rustc"))
}
#[cfg(target_os = "linux")]
fn rust_spec(name: &str, value: i32, delay: u64) -> ScriptSpec {
    let mut spec = spec(
        name,
        &format!(
            r####"use std::io;fn main(){{let mut input=String::new();io::stdin().read_line(&mut input).unwrap();std::thread::sleep(std::time::Duration::from_millis({delay}));println!("{{}}",r###"{{"op":"return","value":{value}}}"###);}}"####
        ),
    );
    spec.language = Language::Rust;
    spec
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn rust_candidate_compiles_separately_and_hot_switch_keeps_inflight_version() {
    let (_root, store, gate, runtime) = setup_with_sandbox(rust_sandbox());
    let mut old_spec = rust_spec("rust-hot", 1, 0);
    old_spec.tools = vec!["qq.send_file".into()];
    old_spec.source = r####"use std::io::{self,Write};fn main(){let mut input=String::new();io::stdin().read_line(&mut input).unwrap();println!("{}",r#"{"op":"tool","name":"qq.send_file","arguments":{}}"#);io::stdout().flush().unwrap();input.clear();io::stdin().read_line(&mut input).unwrap();println!("{}",r#"{"op":"return","value":1}"#);}"####.into();
    let old = runtime
        .publish_candidate(old_spec, context(1))
        .await
        .unwrap();
    store.activate(&owner(), &old.reference, None).unwrap();
    let bound_old = runtime.bind(&owner(), &old.reference).unwrap();
    gate.pause.store(true, Ordering::SeqCst);
    let old_run = runtime.run(&bound_old, "rust-inflight", json!({}), context(2));
    let switch = async {
        gate.entered.notified().await;
        let new = runtime
            .publish_candidate(rust_spec("rust-hot", 2, 0), context(1))
            .await
            .unwrap();
        store.activate(&owner(), &new.reference, None).unwrap();
        let result = runtime
            .run(
                &runtime.bind(&owner(), &new.reference).unwrap(),
                "rust-new",
                json!({}),
                context(1),
            )
            .await
            .unwrap();
        assert_eq!(
            store.receipt(&owner(), "rust-inflight").unwrap().status,
            ReceiptStatus::Running
        );
        gate.pause.store(false, Ordering::SeqCst);
        gate.release.notify_one();
        (new.reference, result)
    };
    let (old_result, new_result) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(old_run, switch)
    })
    .await
    .expect("真实旧进程门闸/新版本调用必须在期限内完成");
    let (new_reference, new_result) = new_result;
    assert_eq!(old_result.unwrap().result, Some(json!(1)));
    assert_eq!(new_result.result, Some(json!(2)));
    assert_eq!(store.selected(&owner(), "rust-hot").unwrap(), new_reference);
    assert_eq!(
        store.state(&owner(), &new_reference).unwrap().value,
        json!({})
    );
    store.activate(&owner(), &old.reference, None).unwrap();
    assert_eq!(
        runtime
            .run(&bound_old, "rust-rollback", json!({}), context(2))
            .await
            .unwrap()
            .result,
        Some(json!(1))
    );
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn rust_bad_compile_and_host_include_cannot_replace_active_version() {
    let (_root, store, _, runtime) = setup_with_sandbox(rust_sandbox());
    let old = runtime
        .publish_candidate(rust_spec("rust-failure", 7, 0), context(1))
        .await
        .unwrap();
    store.activate(&owner(), &old.reference, None).unwrap();
    let marker = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(marker.path(), "private host marker").unwrap();
    for source in [
        "fn main(){ INVALID RUST }".to_owned(),
        format!(
            "fn main(){{println!(\"{{}}\",include_str!({:?}));}}",
            marker.path()
        ),
    ] {
        let mut candidate = rust_spec("rust-failure", 8, 0);
        candidate.source = source;
        assert!(
            runtime
                .publish_candidate(candidate, context(1))
                .await
                .is_err()
        );
        assert_eq!(
            store.selected(&owner(), "rust-failure").unwrap(),
            old.reference
        );
        assert_eq!(store.list(&owner()).unwrap().len(), 1);
    }
    assert_eq!(
        runtime
            .run(
                &runtime.bind(&owner(), &old.reference).unwrap(),
                "rust-survivor",
                json!({}),
                context(1)
            )
            .await
            .unwrap()
            .result,
        Some(json!(7))
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn rust_migration_and_compile_cancellation_keep_old_confirmed_state() {
    let (_root, store, _, runtime) = setup_with_sandbox(rust_sandbox());
    let mut old = rust_spec("rust-state", 1, 0);
    old.state_schema
        .fields
        .insert("count".into(), JsonType::Integer);
    old.initial_state = json!({"count":7});
    let from = runtime
        .publish_candidate(old.clone(), context(1))
        .await
        .unwrap()
        .reference;
    store.activate(&owner(), &from, None).unwrap();
    let mut hanging = old.clone();
    hanging.source =
        "#![allow(long_running_const_eval)] const N:usize=loop{}; fn main(){println!(\"{}\",N);}"
            .into();
    assert!(
        runtime
            .publish_candidate(
                hanging,
                context(1).with_deadline(Some(Instant::now() + Duration::from_millis(100)))
            )
            .await
            .is_err()
    );
    assert_eq!(store.selected(&owner(), "rust-state").unwrap(), from);
    old.state_schema.version = 2;
    old.state_schema
        .fields
        .insert("label".into(), JsonType::String);
    old.initial_state = json!({"count":0,"label":""});
    let to = runtime
        .publish_candidate(old, context(1))
        .await
        .unwrap()
        .reference;
    let migration_source = r####"use std::io;fn main(){let mut input=String::new();io::stdin().read_line(&mut input).unwrap();assert!(input.contains(r#""count":7"#));println!("{}",r#"{"op":"return","value":{"count":8,"label":"rust"}}"#);}"####;
    runtime
        .activate_with_script(
            &to,
            "rust-state-migrate",
            MigrationScript {
                from: from.clone(),
                expected_revision: 0,
                language: Language::Rust,
                source: migration_source.into(),
            },
            context(2),
        )
        .await
        .unwrap();
    assert_eq!(
        store.state(&owner(), &to).unwrap().value,
        json!({"count":8,"label":"rust"})
    );
    assert_eq!(
        store.state(&owner(), &from).unwrap().value,
        json!({"count":7})
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn rust_fifty_version_switches_and_old_handle_recompile_keep_exact_results() {
    let (_root, store, gate, runtime) = setup_with_sandbox(rust_sandbox());
    let mut first = None;
    // 每次调用仍各自使用原预算；测试包含真实隔离编译、执行、激活和回退。
    tokio::time::timeout(Duration::from_secs(120), async {
        for value in 0..50 {
            let candidate = runtime
                .publish_candidate(rust_spec("rust-cycles", value, 0), context(1))
                .await
                .unwrap();
            store
                .activate(&owner(), &candidate.reference, None)
                .unwrap();
            let bound = runtime.bind(&owner(), &candidate.reference).unwrap();
            if first.is_none() {
                first = Some((candidate.reference.clone(), bound.clone()));
            }
            let receipt = runtime
                .run(
                    &bound,
                    &format!("rust-cycle-{value}"),
                    json!({}),
                    context(1),
                )
                .await
                .unwrap();
            assert_eq!(receipt.status, ReceiptStatus::Succeeded);
            assert_eq!(receipt.result, Some(json!(value)));
            assert_eq!(
                store.selected(&owner(), "rust-cycles").unwrap(),
                candidate.reference
            );
        }
        let (reference, bound) = first.as_ref().unwrap();
        store.activate(&owner(), reference, None).unwrap();
        let receipt = runtime
            .run(bound, "rust-cycle-rollback", json!({}), context(2))
            .await
            .unwrap();
        assert_eq!(receipt.result, Some(json!(0)));
        assert_eq!(receipt.status, ReceiptStatus::Succeeded);
        assert_eq!(store.selected(&owner(), "rust-cycles").unwrap(), *reference);
    })
    .await
    .expect("50 次真实 Rust 插件切换应在总测试期限内收尾");
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn rust_compilation_diagnostics_are_bounded_and_failed_candidates_preserve_state() {
    let (_root, store, gate, runtime) = setup_with_sandbox(rust_sandbox());
    let old = runtime
        .publish_candidate(rust_spec("rust-diagnostics", 7, 0), context(1))
        .await
        .unwrap();
    store.activate(&owner(), &old.reference, None).unwrap();
    let previous = store.state(&owner(), &old.reference).unwrap();
    for (source, truncated) in [
        ("fn main(){ missing_identifier(); }".to_owned(), false),
        (
            format!("compile_error!({:?}); fn main() {{}}", "诊断".repeat(9000)),
            true,
        ),
    ] {
        let mut candidate = rust_spec("rust-diagnostics", 8, 0);
        candidate.source = source;
        let ctx = context(1);
        let steps = ctx.shared_steps().unwrap().clone();
        let report = tokio::time::timeout(
            Duration::from_secs(30),
            runtime.prepare_candidate(candidate, ctx),
        )
        .await
        .unwrap()
        .unwrap();
        let CandidatePreparation::CompilationFailed(report) = report else {
            panic!("失败候选不能保存版本")
        };
        assert_eq!(report.language, Language::Rust);
        assert!(report.exit_code.is_some_and(|code| code != 0));
        assert_eq!(report.signal, None);
        assert!(report.diagnostics.contains("error"), "{report:?}");
        assert!(report.diagnostics.contains("/source.rs:"), "{report:?}");
        assert!(report.diagnostics.len() <= 8192);
        assert_eq!(report.truncated, truncated);
        assert_eq!(steps.remaining().unwrap(), 0);
        assert_eq!(store.list(&owner()).unwrap().len(), 1);
        assert_eq!(
            store.selected(&owner(), "rust-diagnostics").unwrap(),
            old.reference
        );
        assert_eq!(store.state(&owner(), &old.reference).unwrap(), previous);
    }
    let CandidatePreparation::Ready(fixed) = runtime
        .prepare_candidate(rust_spec("rust-diagnostics", 8, 0), context(1))
        .await
        .unwrap()
    else {
        panic!("修复后的候选应编译通过")
    };
    assert_eq!(
        store.selected(&owner(), "rust-diagnostics").unwrap(),
        old.reference
    );
    store.activate(&owner(), &fixed.reference, None).unwrap();
    let result = runtime
        .run(
            &runtime.bind(&owner(), &fixed.reference).unwrap(),
            "rust-diagnostic-fixed",
            json!({}),
            context(1),
        )
        .await
        .unwrap();
    assert_eq!(result.status, ReceiptStatus::Succeeded);
    assert_eq!(result.result, Some(json!(8)));
    assert_eq!(gate.calls.load(Ordering::SeqCst), 0);
}
