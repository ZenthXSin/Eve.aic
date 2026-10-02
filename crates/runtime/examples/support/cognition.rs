#![allow(dead_code)] // 进程示例与集成测试共享装配和故障注入能力。
use eve_cognition_api::*;
use eve_cognition_loop_api::*;
use eve_cognition_loop_plugin::{CognitionLoopPlugin, EchoReceiptVerifier, PriorityDrivePolicy};
use eve_cognition_plugin::{CognitionController, CognitionPlugin};
use eve_control_api::*;
use eve_control_plugin::ControlPlugin;
use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::*;
use eve_plugin_api::*;
use eve_runtime::{BudgetedSessionRunner, ContextBinding, ControlGoalExecutor, LlmHost, LlmHostConfig, SessionBinding, SessionLlmHost};
use eve_session_api::SESSION_PLUGIN_ID;
use eve_session_plugin::SessionPlugin;
use serde_json::{Value, json};
use std::{sync::{Arc, atomic::{AtomicUsize, Ordering}}, time::{Duration, SystemTime, UNIX_EPOCH}};
use tokio::sync::Notify;

pub const OWNER: &str = "cognition.acceptance.services";
pub const CONTEXT: &str = "cognition.acceptance.context";
pub const TOOL: &str = "cognition.acceptance.echo";
pub fn id(value: &str) -> PluginId { PluginId::new(value).unwrap() }
pub fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64 }
pub fn goal(name: &str) -> Goal {
    Goal {
        id: name.into(), revision: 0,
        source: Source { kind: SourceKind::Internal, channel: "local".into(), reference: "fixture".into() },
        visibility: Visibility::Internal,
        description: "调用 echo，text 参数为 cognition-proof，然后回复完成".into(),
        verification: "echo:cognition-proof".into(), priority: 50,
        budget: ExecutionBudget { max_model_requests: 2, max_tool_calls: 1, max_attempts: 1, timeout_ms: 10_000 },
        stop_condition: "single-attempt".into(), expires_at_ms: None, status: GoalStatus::Ready,
        wait_reason: None, block_reason: None, execution: None, feedback: None,
    }
}
pub fn options() -> LoopOptions {
    LoopOptions {
        scope: ExecutionScope {
            subject_id: "eve".into(), access: ReadAccess::Internal,
            sources: vec![AllowedSource { kind: SourceKind::Internal, channel: "local".into() }],
        },
        poll_interval_ms: 20, max_executions: 1,
    }
}
pub fn call_response(calls: usize) -> ModelResponse {
    ModelResponse::ToolCalls { calls: (0..calls).map(|index| ToolCall {
        id: format!("echo-{index}"), name: "echo".into(), arguments: json!({"text":"cognition-proof"}),
    }).collect() }
}
#[derive(Default)]
pub struct ToolProbe {
    pub started: AtomicUsize,
    pub dropped: AtomicUsize,
    pub block: std::sync::atomic::AtomicBool,
    pub entered: Notify,
}
struct ToolGuard(Arc<ToolProbe>);
impl Drop for ToolGuard {
    fn drop(&mut self) { self.0.dropped.fetch_add(1, Ordering::SeqCst); }
}
struct Echo(Arc<ToolProbe>);
impl Tool for Echo {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo".into(), description: "本地 echo 验收工具".into(),
            argument_schema: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}),
            required_permissions: vec![], concurrency: Some(ToolConcurrency::ParallelSafe),
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        if value.as_object().is_some_and(|object| object.len() == 1)
            && value.get("text").is_some_and(Value::is_string) { Ok(()) }
        else { Err(ToolValidationError { message: "echo 参数无效".into() }) }
    }
    fn execute(&self, call: ToolCall, _: ToolExecutionContext) -> ToolFuture<'_> {
        let probe = self.0.clone();
        Box::pin(async move {
            let _guard = ToolGuard(probe.clone());
            probe.started.fetch_add(1, Ordering::SeqCst);
            probe.entered.notify_one();
            if probe.block.load(Ordering::SeqCst) { std::future::pending::<()>().await; }
            Ok(json!({"echo":call.arguments["text"]}))
        })
    }
}
struct Context;
impl ContextAssembler for Context {
    fn assemble(&self, _: TurnInput) -> LlmFuture<'_, ContextSnapshot> {
        Box::pin(async { Ok(ContextSnapshot { revision: "cognition-fixture".into(),
            profile: String::new(), memories: vec![], history: vec![] }) })
    }
}
struct Services { manifest: PluginManifest, probe: Arc<ToolProbe> }
impl Plugin for Services {
    fn manifest(&self) -> &PluginManifest { &self.manifest }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        let probe = self.probe.clone();
        Box::pin(async move {
            context.provide_service(ServiceId::new(CONTEXT)?, ContextService(Arc::new(Context)))?;
            context.provide_service(ServiceId::new(TOOL)?, ToolService(Arc::new(Echo(probe))))?;
            Ok(None)
        })
    }
}
pub struct Rig {
    pub kernel: Kernel,
    pub admin: CognitionController,
    pub registry: Arc<dyn ServiceRegistry>,
    pub probe: Arc<ToolProbe>,
    pub runner: Arc<BudgetedSessionRunner>,
    pub control: Arc<dyn ControlService>,
}
impl Rig {
    pub async fn open(store: Arc<dyn StateStore>, provider: Arc<dyn LlmProvider>,
        resolver: Option<Arc<dyn LlmModelResolver>>) -> Self {
        let services = KernelServices { state: store, ..KernelServices::default() };
        let registry = services.registry.clone();
        let permissions = services.permissions.clone();
        let logger = services.logger.clone();
        let kernel = Kernel::with_services(services);
        let plugin = CognitionPlugin::new("eve").unwrap();
        let admin = plugin.controller();
        kernel.register(Box::new(plugin)).unwrap();
        kernel.register(Box::new(SessionPlugin::new().unwrap())).unwrap();
        let probe = Arc::new(ToolProbe::default());
        kernel.register(Box::new(Services { manifest: PluginManifest::new(OWNER, "0.1.0").unwrap(), probe: probe.clone() })).unwrap();
        for owner in [COGNITION_PLUGIN_ID, SESSION_PLUGIN_ID, OWNER] { kernel.start(&id(owner)).await.unwrap(); }
        let mut host = LlmHost::new(provider, registry.clone(), kernel.clone(), permissions,
            ContextBinding { service_id: ServiceId::new(CONTEXT).unwrap(), expected_owner: id(OWNER) },
            vec![ToolBinding { name: "echo".into(), service_id: ServiceId::new(TOOL).unwrap(), expected_owner: id(OWNER) }],
            LlmHostConfig { provider_timeout: Duration::from_secs(10), ..LlmHostConfig::default() }).unwrap();
        if let Some(resolver) = resolver { host = host.with_model_resolver(resolver); }
        let host = Arc::new(SessionLlmHost::new(host, SessionBinding::builtin()).with_logger(logger));
        let runner = Arc::new(BudgetedSessionRunner::new(host));
        kernel.register(Box::new(ControlPlugin::new(runner.clone(), vec![]).unwrap())).unwrap();
        kernel.start(&id(CONTROL_PLUGIN_ID)).await.unwrap();
        let control = registry.get(&ServiceId::new(CONTROL_SERVICE_ID).unwrap()).unwrap().unwrap()
            .value.downcast::<ControlServiceHandle>().unwrap().0.clone();
        Self { kernel, admin, registry, probe, runner, control }
    }
    pub fn seed(&self, goals: Vec<Goal>) {
        let snapshot = self.admin.snapshot().unwrap();
        let mut state = snapshot.state;
        for goal in goals { state.goals.insert(goal.id.clone(), goal); }
        self.admin.replace(snapshot.revision, state).unwrap();
    }
    pub async fn start_loop(&self, options: LoopOptions) -> eve_cognition_loop_plugin::LoopController {
        let executor = Arc::new(ControlGoalExecutor::new(self.control.clone(), self.runner.clone(), "internal").unwrap());
        let plugin = CognitionLoopPlugin::new(Arc::new(self.admin.clone()), Arc::new(PriorityDrivePolicy),
            Arc::new(EchoReceiptVerifier), executor, options,
            [COGNITION_PLUGIN_ID, CONTROL_PLUGIN_ID].into_iter().map(|name| PluginDependency {
                id: id(name), requirement: Some("^0.1".into())
            }).collect()).unwrap();
        let controller = plugin.controller();
        self.kernel.register(Box::new(plugin)).unwrap();
        self.kernel.start(&id(LOOP_PLUGIN_ID)).await.unwrap();
        controller
    }
    pub async fn wait_terminal(&self, name: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = self.admin.snapshot().unwrap();
                if !matches!(snapshot.state.goals[name].status, GoalStatus::Ready | GoalStatus::Executing) { return; }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.unwrap();
    }
    pub async fn close(&self) {
        // 调用方已经 shutdown 循环；停止后卸载组合执行器，打破其 Kernel 引用环。
        self.kernel.stop_all().await.unwrap();
        for owner in [LOOP_PLUGIN_ID, CONTROL_PLUGIN_ID] {
            if self.kernel.state(&id(owner)).is_some() { self.kernel.unregister(&id(owner)).unwrap(); }
        }
    }
}
