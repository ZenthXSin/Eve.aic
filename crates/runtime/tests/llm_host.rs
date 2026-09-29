use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{
    ChatRole, ContextAssembler, ContextService, ContextSnapshot, LlmError, LlmFuture, LlmProvider,
    ModelRequest, ModelResponse, Tool, ToolBinding, ToolCall, ToolConcurrency, ToolDefinition,
    ToolExecutionContext, ToolExecutionError, ToolFailureCode, ToolFuture, ToolService,
    ToolValidationError, TurnInput,
};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginId, PluginManifest,
    ServiceEntry, ServiceId, ServiceRegistry, ServiceValue,
};
use eve_runtime::{ContextBinding, LlmHost, LlmHostConfig, TurnStage};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

const OWNER: &str = "llm-test-owner";
const CONTEXT: &str = "llm-test.context";

fn owner_id() -> PluginId {
    PluginId::new(OWNER).expect("valid test owner")
}

fn service_id(name: &str) -> ServiceId {
    ServiceId::new(format!("llm-test.tool.{name}")).expect("valid test service")
}

#[derive(Default)]
struct CountingContext {
    snapshots: AtomicUsize,
}

impl ContextAssembler for CountingContext {
    fn assemble(&self, _input: TurnInput) -> LlmFuture<'_, ContextSnapshot> {
        self.snapshots.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(ContextSnapshot {
                revision: "revision-1".into(),
                profile: "测试用户".into(),
                memories: vec!["长期记忆".into()],
                history: vec![],
            })
        })
    }
}

#[derive(Clone)]
enum ToolMode {
    Echo,
    Error,
    PanicSync,
    PanicFuture,
    WaitForCancellation(Arc<AtomicBool>),
    WaitForRelease {
        started: Arc<Notify>,
        release: Arc<Notify>,
    },
    WaitForDrop(Arc<AtomicBool>),
}

struct NeverFuture {
    dropped: Arc<AtomicBool>,
}

impl Future for NeverFuture {
    type Output = Result<Value, ToolExecutionError>;

    fn poll(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::task::Poll::Pending
    }
}

impl Drop for NeverFuture {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

struct TestTool {
    name: String,
    concurrency: Option<ToolConcurrency>,
    mode: ToolMode,
    delay: Duration,
    starts: Arc<Mutex<Vec<String>>>,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl TestTool {
    fn new(name: &str, concurrency: Option<ToolConcurrency>, mode: ToolMode) -> Self {
        Self {
            name: name.into(),
            concurrency,
            mode,
            delay: Duration::from_millis(20),
            starts: Arc::new(Mutex::new(Vec::new())),
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Tool for TestTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: format!("测试工具 {}", self.name),
            argument_schema: json!({"type": "object"}),
            required_permissions: vec![],
            concurrency: self.concurrency.clone(),
        }
    }

    fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolValidationError> {
        if arguments.get("invalid").is_some() {
            Err(ToolValidationError {
                message: "参数不符合测试工具约束".into(),
            })
        } else {
            Ok(())
        }
    }

    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        let mode = self.mode.clone();
        let delay = self.delay;
        let starts = self.starts.clone();
        let active = self.active.clone();
        let peak = self.peak.clone();
        match mode {
            ToolMode::PanicSync => panic!("同步执行入口故意 panic"),
            ToolMode::PanicFuture => Box::pin(async move {
                panic!("Future 故意 panic");
            }),
            ToolMode::WaitForCancellation(cancelled) => Box::pin(async move {
                loop {
                    if context.is_cancel_requested() {
                        cancelled.store(true, Ordering::SeqCst);
                        return Err(ToolExecutionError::Cancelled("收到取消请求".into()));
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }),
            ToolMode::WaitForRelease { started, release } => Box::pin(async move {
                starts.lock().expect("starts lock").push(call.id.clone());
                started.notify_one();
                release.notified().await;
                Ok(json!({"id": call.id}))
            }),
            ToolMode::WaitForDrop(dropped) => {
                starts.lock().expect("starts lock").push(call.id);
                Box::pin(NeverFuture { dropped })
            }
            ToolMode::Echo | ToolMode::Error => Box::pin(async move {
                starts.lock().expect("starts lock").push(call.id.clone());
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(delay).await;
                active.fetch_sub(1, Ordering::SeqCst);
                if matches!(mode, ToolMode::Error) {
                    Err(ToolExecutionError::Failed("工具执行失败".into()))
                } else {
                    Ok(json!({"id": call.id, "arguments": call.arguments}))
                }
            }),
        }
    }
}

struct RecordingProvider {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    responses: Mutex<VecDeque<Result<ModelResponse, LlmError>>>,
    delay: Option<Duration>,
    block: Option<(Arc<Notify>, Arc<Notify>)>,
}

impl RecordingProvider {
    fn scripted(responses: Vec<Result<ModelResponse, LlmError>>) -> Arc<Self> {
        Arc::new(Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            responses: Mutex::new(responses.into()),
            delay: None,
            block: None,
        })
    }

    fn delayed(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            responses: Mutex::new(VecDeque::new()),
            delay: Some(delay),
            block: None,
        })
    }

    fn blocked() -> (Arc<Self>, Arc<Notify>, Arc<Notify>) {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let provider = Arc::new(Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            responses: Mutex::new(VecDeque::from([final_response("Provider 已释放")])),
            delay: None,
            block: Some((started.clone(), release.clone())),
        });
        (provider, started, release)
    }
}

impl LlmProvider for RecordingProvider {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        self.requests.lock().expect("request lock").push(request);
        let delay = self.delay;
        let block = self.block.clone();
        let response = self.responses.lock().expect("response lock").pop_front();
        Box::pin(async move {
            if let Some((started, release)) = block {
                started.notify_one();
                release.notified().await;
            }
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            response.unwrap_or_else(|| Err(LlmError::Provider("没有预设响应".into())))
        })
    }
}

struct ServicePlugin {
    manifest: PluginManifest,
    context: Arc<CountingContext>,
    tools: Vec<(ServiceId, Arc<dyn Tool>)>,
}

struct FailingRegistry {
    inner: Arc<dyn ServiceRegistry>,
    gets: AtomicUsize,
    fail_on_get: usize,
}

impl ServiceRegistry for FailingRegistry {
    fn provide(
        self: Arc<Self>,
        owner: PluginId,
        id: ServiceId,
        value: ServiceValue,
    ) -> eve_plugin_api::PluginResult<Cleanup> {
        self.inner.clone().provide(owner, id, value)
    }

    fn get(&self, id: &ServiceId) -> eve_plugin_api::PluginResult<Option<ServiceEntry>> {
        let ordinal = self.gets.fetch_add(1, Ordering::SeqCst) + 1;
        if ordinal == self.fail_on_get {
            return Err(PluginError::State("simulated registry failure".into()));
        }
        self.inner.get(id)
    }
}

impl Plugin for ServicePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        let assembler = self.context.clone();
        let tools = self.tools.clone();
        Box::pin(async move {
            context.provide_service(
                ServiceId::new(CONTEXT).expect("valid context service"),
                ContextService(assembler),
            )?;
            for (id, tool) in tools {
                context.provide_service(id, ToolService(tool))?;
            }
            Ok(None)
        })
    }
}

struct EmptyPlugin {
    manifest: PluginManifest,
}

impl Plugin for EmptyPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, _context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async { Ok(None) })
    }
}

async fn make_host(
    provider: Arc<RecordingProvider>,
    tools: Vec<Arc<TestTool>>,
    config: LlmHostConfig,
) -> (LlmHost, Arc<CountingContext>, Vec<Arc<TestTool>>) {
    let (host, context, tools, _) = make_host_with_registry(provider, tools, config, None).await;
    (host, context, tools)
}

async fn make_host_with_registry(
    provider: Arc<RecordingProvider>,
    tools: Vec<Arc<TestTool>>,
    mut config: LlmHostConfig,
    registry_override: Option<Arc<dyn ServiceRegistry>>,
) -> (LlmHost, Arc<CountingContext>, Vec<Arc<TestTool>>, Kernel) {
    let context = Arc::new(CountingContext::default());
    let service_tools = tools
        .iter()
        .map(|tool| (service_id(&tool.name), Arc::clone(tool) as Arc<dyn Tool>))
        .collect::<Vec<_>>();
    let services = KernelServices::default();
    let base_registry = services.registry.clone();
    let registry = registry_override.unwrap_or_else(|| base_registry.clone());
    let permissions = services.permissions.clone();
    let kernel = Kernel::with_services(services);
    let manifest = PluginManifest::new(OWNER, "0.1.0").expect("valid manifest");
    kernel
        .register(Box::new(ServicePlugin {
            manifest,
            context: context.clone(),
            tools: service_tools,
        }))
        .expect("register service plugin");

    config.system_prompt = "你是测试助手".into();
    config.output_format = "纯文本".into();
    let bindings = tools
        .iter()
        .map(|tool| ToolBinding {
            name: tool.name.clone(),
            service_id: service_id(&tool.name),
            expected_owner: owner_id(),
        })
        .collect();
    kernel
        .start(&owner_id())
        .await
        .expect("start service plugin");
    let host = LlmHost::new(
        provider,
        registry,
        kernel.clone(),
        permissions,
        ContextBinding {
            service_id: ServiceId::new(CONTEXT).expect("valid context binding"),
            expected_owner: owner_id(),
        },
        bindings,
        config,
    )
    .expect("construct LLM host");
    (host, context, tools, kernel)
}

fn turn() -> TurnInput {
    TurnInput {
        text: "请执行测试".into(),
    }
}

fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}

fn final_response(text: &str) -> Result<ModelResponse, LlmError> {
    Ok(ModelResponse::Final { text: text.into() })
}

fn tool_response(calls: Vec<ToolCall>) -> Result<ModelResponse, LlmError> {
    Ok(ModelResponse::ToolCalls { calls })
}

#[tokio::test]
async fn records_request_layout_sorted_tools_and_single_context_snapshot() {
    let provider = RecordingProvider::scripted(vec![
        tool_response(vec![call("a1", "alpha", json!({}))]),
        final_response("已完成"),
    ]);
    let alpha = Arc::new(TestTool::new(
        "alpha",
        Some(ToolConcurrency::ParallelSafe),
        ToolMode::Echo,
    ));
    let beta = Arc::new(TestTool::new(
        "beta",
        Some(ToolConcurrency::ParallelSafe),
        ToolMode::Echo,
    ));
    let (host, context, _) = make_host(
        provider.clone(),
        vec![beta, alpha],
        LlmHostConfig::default(),
    )
    .await;

    let output = host.run_turn(turn()).await.expect("turn succeeds");
    assert_eq!(output.text, "已完成");
    assert_eq!(context.snapshots.load(Ordering::SeqCst), 1);

    let requests = provider.requests.lock().expect("request lock");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].messages[0].role, ChatRole::System);
    assert_eq!(requests[0].messages[1].role, ChatRole::System);
    assert_eq!(requests[0].messages[2].role, ChatRole::System);
    assert_eq!(requests[0].messages[3].role, ChatRole::System);
    assert_eq!(requests[0].messages[4].role, ChatRole::User);
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "beta"]
    );
    assert!(matches!(
        requests[1].messages.last().map(|message| &message.role),
        Some(ChatRole::Tool)
    ));
}

#[tokio::test]
async fn preserves_multi_call_order_and_parallel_safe_overlap() {
    let provider = RecordingProvider::scripted(vec![
        tool_response(vec![
            call("first", "echo", json!({})),
            call("second", "echo", json!({})),
        ]),
        final_response("完成"),
    ]);
    let tool = Arc::new(TestTool::new(
        "echo",
        Some(ToolConcurrency::ParallelSafe),
        ToolMode::Echo,
    ));
    let (host, _, tools) = make_host(provider, vec![tool], LlmHostConfig::default()).await;
    let output = host.run_turn(turn()).await.expect("turn succeeds");

    assert_eq!(
        output
            .diagnostics
            .tool_results
            .iter()
            .map(|result| result.call_id.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert!(
        tools[0].peak.load(Ordering::SeqCst) >= 2,
        "ParallelSafe 工具应产生重叠执行"
    );
}

#[tokio::test]
async fn serial_scope_and_default_tools_never_overlap() {
    for concurrency in [
        Some(ToolConcurrency::Serial {
            scope: "shared".into(),
        }),
        None,
    ] {
        let provider = RecordingProvider::scripted(vec![
            tool_response(vec![
                call("one", "serial", json!({})),
                call("two", "serial", json!({})),
            ]),
            final_response("完成"),
        ]);
        let tool = Arc::new(TestTool::new("serial", concurrency, ToolMode::Echo));
        let (host, _, tools) = make_host(provider, vec![tool], LlmHostConfig::default()).await;
        host.run_turn(turn()).await.expect("turn succeeds");

        assert_eq!(tools[0].peak.load(Ordering::SeqCst), 1);
        assert_eq!(
            tools[0].starts.lock().expect("starts lock").as_slice(),
            ["one", "two"]
        );
    }
}

#[tokio::test]
async fn maps_argument_unknown_execution_and_panic_failures_to_original_ids() {
    let provider = RecordingProvider::scripted(vec![
        tool_response(vec![
            call("invalid", "echo", json!({"invalid": true})),
            call("unknown", "missing", json!({})),
            call("failed", "failure", json!({})),
            call("sync-panic", "sync-panic", json!({})),
            call("future-panic", "future-panic", json!({})),
        ]),
        final_response("已收敛"),
    ]);
    let tools = vec![
        Arc::new(TestTool::new(
            "echo",
            Some(ToolConcurrency::ParallelSafe),
            ToolMode::Echo,
        )),
        Arc::new(TestTool::new(
            "failure",
            Some(ToolConcurrency::ParallelSafe),
            ToolMode::Error,
        )),
        Arc::new(TestTool::new(
            "sync-panic",
            Some(ToolConcurrency::ParallelSafe),
            ToolMode::PanicSync,
        )),
        Arc::new(TestTool::new(
            "future-panic",
            Some(ToolConcurrency::ParallelSafe),
            ToolMode::PanicFuture,
        )),
    ];
    let (host, _, _) = make_host(provider, tools, LlmHostConfig::default()).await;
    let output = host.run_turn(turn()).await.expect("turn succeeds");

    let by_id = output
        .diagnostics
        .tool_results
        .iter()
        .map(|result| (&result.call_id, &result.output))
        .collect::<std::collections::HashMap<_, _>>();
    for (id, code) in [
        ("invalid", ToolFailureCode::InvalidArguments),
        ("unknown", ToolFailureCode::UnknownTool),
        ("failed", ToolFailureCode::ExecutionFailed),
        ("sync-panic", ToolFailureCode::ExecutionFailed),
        ("future-panic", ToolFailureCode::ExecutionFailed),
    ] {
        assert!(
            matches!(by_id.get(&id.to_string()), Some(eve_llm_api::ToolOutput::Failure { code: actual, .. }) if *actual == code)
        );
    }
}

#[tokio::test]
async fn reports_provider_error_timeout_and_round_limit() {
    let provider_error =
        RecordingProvider::scripted(vec![Err(LlmError::Provider("上游失败".into()))]);
    let (host, _, _) = make_host(provider_error, vec![], LlmHostConfig::default()).await;
    let failure = host
        .run_turn(turn())
        .await
        .expect_err("provider error expected");
    assert!(matches!(failure.error, LlmError::Provider(message) if message == "上游失败"));

    let timeout_config = LlmHostConfig {
        provider_timeout: Duration::from_millis(5),
        ..LlmHostConfig::default()
    };
    let timeout_provider = RecordingProvider::delayed(Duration::from_millis(50));
    let (host, _, _) = make_host(timeout_provider, vec![], timeout_config).await;
    let failure = host
        .run_turn(turn())
        .await
        .expect_err("provider timeout expected");
    assert_eq!(failure.error, LlmError::ProviderTimeout);

    let round_provider = RecordingProvider::scripted(vec![
        tool_response(vec![call("again", "echo", json!({}))]),
        tool_response(vec![call("still-again", "echo", json!({}))]),
    ]);
    let tool = Arc::new(TestTool::new(
        "echo",
        Some(ToolConcurrency::ParallelSafe),
        ToolMode::Echo,
    ));
    let (host, _, _) = make_host(round_provider, vec![tool], LlmHostConfig::default()).await;
    let failure = host
        .run_turn(turn())
        .await
        .expect_err("round limit expected");
    assert_eq!(failure.error, LlmError::RoundLimit);
    assert_eq!(failure.diagnostics.stage, TurnStage::Failed);
}

#[tokio::test]
async fn tool_timeout_requests_cancellation_and_preserves_call_id() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let provider = RecordingProvider::scripted(vec![
        tool_response(vec![call("cancel-me", "slow", json!({}))]),
        final_response("超时已处理"),
    ]);
    let tool = Arc::new(TestTool::new(
        "slow",
        Some(ToolConcurrency::ParallelSafe),
        ToolMode::WaitForCancellation(cancelled.clone()),
    ));
    let config = LlmHostConfig {
        tool_timeout: Duration::from_millis(5),
        tool_cancellation_grace: Duration::from_millis(20),
        ..LlmHostConfig::default()
    };
    let (host, _, _) = make_host(provider, vec![tool], config).await;
    let output = host.run_turn(turn()).await.expect("turn succeeds");
    assert_eq!(output.diagnostics.tool_results[0].call_id, "cancel-me");
    assert!(matches!(
        output.diagnostics.tool_results[0].output,
        eve_llm_api::ToolOutput::Failure {
            code: ToolFailureCode::TimedOut,
            ..
        }
    ));
    assert!(cancelled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn lifecycle_mutations_wait_for_turn_and_shared_kernel_clones_cannot_bypass_gate() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let provider = RecordingProvider::scripted(vec![
        tool_response(vec![call("held", "held", json!({}))]),
        final_response("已释放"),
    ]);
    let tool = Arc::new(TestTool::new(
        "held",
        Some(ToolConcurrency::ParallelSafe),
        ToolMode::WaitForRelease {
            started: started.clone(),
            release: release.clone(),
        },
    ));
    let (host, _, _, kernel) =
        make_host_with_registry(provider, vec![tool], LlmHostConfig::default(), None).await;
    let turn_task = tokio::spawn(async move { host.run_turn(turn()).await });
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("tool execution started");

    let register_error = kernel
        .clone()
        .register(Box::new(EmptyPlugin {
            manifest: PluginManifest::new("llm-test-extra", "0.1.0").expect("valid manifest"),
        }))
        .expect_err("registration must be rejected during an active turn");
    assert!(register_error.to_string().contains("不能注册插件"));
    let unregister_error = kernel
        .clone()
        .unregister(&owner_id())
        .expect_err("unregistration must be rejected during an active turn");
    assert!(unregister_error.to_string().contains("不能卸载插件"));

    let start_waiting = Arc::new(Notify::new());
    let starting_kernel = kernel.clone();
    let start_started = start_waiting.clone();
    let start_task = tokio::spawn(async move {
        start_started.notify_one();
        starting_kernel.start(&owner_id()).await
    });
    tokio::time::timeout(Duration::from_secs(1), start_waiting.notified())
        .await
        .expect("start request should be submitted");
    assert!(
        !start_task.is_finished(),
        "start must wait for the in-flight tool call"
    );

    let stop_waiting = Arc::new(Notify::new());
    let stopping_kernel = kernel.clone();
    let stop_started = stop_waiting.clone();
    let stop_task = tokio::spawn(async move {
        stop_started.notify_one();
        stopping_kernel.stop(&owner_id()).await
    });
    assert!(
        tokio::time::timeout(Duration::from_secs(1), stop_waiting.notified())
            .await
            .is_ok(),
        "stop request should be submitted"
    );
    assert!(
        !stop_task.is_finished(),
        "stop must wait for the in-flight tool call"
    );

    release.notify_one();
    let result = turn_task
        .await
        .expect("turn task join")
        .expect("turn succeeds");
    assert_eq!(result.text, "已释放");
    start_task
        .await
        .expect("start task join")
        .expect("start completes after the turn releases admission");
    stop_task
        .await
        .expect("stop task join")
        .expect("stop succeeds after the turn releases admission");
    kernel
        .unregister(&owner_id())
        .expect("unregister after stop and turn cleanup");
}

#[tokio::test]
async fn stop_waits_while_provider_request_is_in_flight() {
    let (provider, started, release) = RecordingProvider::blocked();
    let (host, _, _, kernel) =
        make_host_with_registry(provider, vec![], LlmHostConfig::default(), None).await;
    let turn_task = tokio::spawn(async move { host.run_turn(turn()).await });
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("provider request started");

    let stop_waiting = Arc::new(Notify::new());
    let stopping_kernel = kernel.clone();
    let stop_started = stop_waiting.clone();
    let stop_task = tokio::spawn(async move {
        stop_started.notify_one();
        stopping_kernel.stop(&owner_id()).await
    });
    assert!(
        tokio::time::timeout(Duration::from_secs(1), stop_waiting.notified())
            .await
            .is_ok(),
        "stop request should be submitted"
    );
    assert!(
        !stop_task.is_finished(),
        "stop must wait for the in-flight provider request"
    );

    release.notify_one();
    let output = turn_task
        .await
        .expect("turn task join")
        .expect("turn succeeds");
    assert_eq!(output.text, "Provider 已释放");
    stop_task
        .await
        .expect("stop task join")
        .expect("stop succeeds after the provider request");
    kernel
        .unregister(&owner_id())
        .expect("unregister after stop and turn cleanup");
}

#[tokio::test]
async fn later_preflight_backend_failure_starts_no_prior_tools_or_second_request() {
    let provider = RecordingProvider::scripted(vec![tool_response(vec![
        call("first", "echo", json!({})),
        call("second", "echo", json!({})),
    ])]);
    let tool = Arc::new(TestTool::new(
        "echo",
        Some(ToolConcurrency::ParallelSafe),
        ToolMode::Echo,
    ));
    let base_registry = Arc::new(eve_kernel::backends::MemoryServiceRegistry::default());
    let failing_registry = Arc::new(FailingRegistry {
        inner: base_registry.clone(),
        gets: AtomicUsize::new(0),
        fail_on_get: 4,
    });
    let context = Arc::new(CountingContext::default());
    let services = KernelServices {
        registry: base_registry.clone(),
        ..KernelServices::default()
    };
    let kernel = Kernel::with_services(services);
    let manifest = PluginManifest::new(OWNER, "0.1.0").expect("valid manifest");
    kernel
        .register(Box::new(ServicePlugin {
            manifest,
            context,
            tools: vec![(service_id("echo"), tool.clone() as Arc<dyn Tool>)],
        }))
        .expect("register service plugin");
    kernel
        .start(&owner_id())
        .await
        .expect("start service plugin");
    let permissions = KernelServices::default().permissions;
    let host = LlmHost::new(
        provider.clone(),
        failing_registry,
        kernel,
        permissions,
        ContextBinding {
            service_id: ServiceId::new(CONTEXT).expect("valid context binding"),
            expected_owner: owner_id(),
        },
        vec![ToolBinding {
            name: "echo".into(),
            service_id: service_id("echo"),
            expected_owner: owner_id(),
        }],
        LlmHostConfig::default(),
    )
    .expect("construct host");
    let failure = host
        .run_turn(turn())
        .await
        .expect_err("backend failure expected");
    assert!(matches!(failure.error, LlmError::Backend(_)));
    assert_eq!(failure.diagnostics.provider_requests, 1);
    assert!(tool.starts.lock().expect("starts lock").is_empty());
}

#[tokio::test]
async fn dropping_run_turn_drops_in_flight_tool_future() {
    let dropped = Arc::new(AtomicBool::new(false));
    let provider = RecordingProvider::scripted(vec![tool_response(vec![call(
        "drop-me",
        "pending",
        json!({}),
    )])]);
    let tool = Arc::new(TestTool::new(
        "pending",
        Some(ToolConcurrency::ParallelSafe),
        ToolMode::WaitForDrop(dropped.clone()),
    ));
    let (host, _, tools, kernel) =
        make_host_with_registry(provider, vec![tool], LlmHostConfig::default(), None).await;
    let task = tokio::spawn(async move { host.run_turn(turn()).await });
    for _ in 0..100 {
        if !tools[0].starts.lock().expect("starts lock").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(!tools[0].starts.lock().expect("starts lock").is_empty());
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_millis(100), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("tool future should be dropped after turn cancellation");
    tokio::time::timeout(Duration::from_secs(1), kernel.stop(&owner_id()))
        .await
        .expect("stop should acquire admission after turn cancellation")
        .expect("stop succeeds after turn cancellation");
    kernel
        .unregister(&owner_id())
        .expect("unregister after canceled turn cleanup");
}
