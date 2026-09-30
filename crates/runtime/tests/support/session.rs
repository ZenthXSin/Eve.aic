#![allow(dead_code)] // 多个验收文件共享夹具，各自使用不同故障注入能力。
use eve_kernel::{
    Kernel, KernelServices,
    backends::{MemoryLogger, MemoryStateStore},
};
use eve_llm_api::*;
use eve_plugin_api::*;
use eve_runtime::{ContextBinding, LlmHost, LlmHostConfig, SessionBinding, SessionLlmHost};
use eve_session_api::*;
use eve_session_plugin::{SESSION_STATE_KEY, SessionPlugin};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

pub const OWNER: &str = "session-test.services";
pub const CONTEXT: &str = "session-test.context";
pub const TOOL: &str = "session-test.tool";

pub fn id(value: &str) -> PluginId {
    PluginId::new(value).unwrap()
}
pub fn key(session: &str) -> SessionKey {
    SessionKey::new(session, "用户一").unwrap()
}
pub fn input(session: &str, text: &str) -> SessionInput {
    SessionInput {
        key: key(session),
        text: text.into(),
    }
}
pub fn final_response(text: &str) -> Result<ModelResponse, LlmError> {
    Ok(ModelResponse::Final { text: text.into() })
}
pub fn calls() -> Result<ModelResponse, LlmError> {
    Ok(ModelResponse::ToolCalls {
        calls: vec![
            ToolCall {
                id: "call-b".into(),
                name: "receipt".into(),
                arguments: json!({"text":"中文回执"}),
            },
            ToolCall {
                id: "call-a".into(),
                name: "receipt".into(),
                arguments: json!({}),
            },
        ],
    })
}

#[derive(Default)]
pub struct FaultStore {
    pub memory: MemoryStateStore,
    pub fail: AtomicBool,
    pub pause_next: AtomicBool,
    pub entered: Notify,
    released: Mutex<bool>,
    release: Condvar,
}
impl FaultStore {
    pub fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
    pub fn bytes(&self) -> Option<Vec<u8>> {
        self.get(&id(SESSION_PLUGIN_ID), SESSION_STATE_KEY).unwrap()
    }
}
impl StateStore for FaultStore {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.memory.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, bytes: Vec<u8>) -> PluginResult<()> {
        if self.pause_next.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            let mut released = self.released.lock().unwrap();
            while !*released {
                let (next, timeout) = self
                    .release
                    .wait_timeout(released, Duration::from_secs(5))
                    .unwrap();
                released = next;
                if timeout.timed_out() && !*released {
                    return Err(PluginError::State("验收等待提交释放超时".into()));
                }
            }
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err(PluginError::State("backend-secret".into()));
        }
        self.memory.set(namespace, key, bytes)
    }
}

pub struct Step {
    pub response: Result<ModelResponse, LlmError>,
    pub gate: Option<Arc<Notify>>,
    pub fail_commit: Option<Arc<FaultStore>>,
    pub pause_commit: Option<Arc<FaultStore>>,
}
impl Step {
    pub fn new(response: Result<ModelResponse, LlmError>) -> Self {
        Self {
            response,
            gate: None,
            fail_commit: None,
            pause_commit: None,
        }
    }
    pub fn blocked(gate: Arc<Notify>) -> Self {
        Self {
            gate: Some(gate),
            ..Self::new(final_response("完成"))
        }
    }
}
pub struct Provider {
    pub requests: Mutex<Vec<ModelRequest>>,
    steps: Mutex<VecDeque<Step>>,
    pub entered: Notify,
}
impl Provider {
    pub fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(vec![]),
            steps: Mutex::new(steps.into()),
            entered: Notify::new(),
        })
    }
    pub async fn wait_requests(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let entered = self.entered.notified();
                if self.requests.lock().unwrap().len() >= count {
                    return;
                }
                entered.await;
            }
        })
        .await
        .unwrap();
    }
}
impl LlmProvider for Provider {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        self.requests.lock().unwrap().push(request);
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("不得产生额外 Provider 请求");
        Box::pin(async move {
            self.entered.notify_one();
            if let Some(gate) = step.gate {
                gate.notified().await;
            }
            if let Some(store) = step.fail_commit {
                store.fail.store(true, Ordering::SeqCst);
            }
            if let Some(store) = step.pause_commit {
                store.pause_next.store(true, Ordering::SeqCst);
            }
            step.response
        })
    }
}

#[derive(Default)]
pub struct Context {
    pub history: Mutex<Vec<ChatMessage>>,
    pub gate: Mutex<Option<Arc<Notify>>>,
    pub entered: Notify,
}
impl ContextAssembler for Context {
    fn assemble(&self, _: TurnInput) -> LlmFuture<'_, ContextSnapshot> {
        let history = self.history.lock().unwrap().clone();
        let gate = self.gate.lock().unwrap().clone();
        Box::pin(async move {
            self.entered.notify_one();
            if let Some(gate) = gate {
                gate.notified().await;
            }
            Ok(ContextSnapshot {
                revision: "fixed-1".into(),
                profile: "用户档案".into(),
                memories: vec!["使用中文".into()],
                history,
            })
        })
    }
}
pub struct ReceiptTool {
    pub starts: Arc<AtomicUsize>,
}
impl Tool for ReceiptTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "receipt".into(),
            description: "生成本地回执".into(),
            argument_schema: json!({"type":"object","properties":{"text":{"type":"string"}}}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::ParallelSafe),
        }
    }
    fn validate_arguments(&self, args: &Value) -> Result<(), ToolValidationError> {
        if args.get("text").is_some_and(Value::is_string) {
            Ok(())
        } else {
            Err(ToolValidationError {
                message: "text 必须是字符串".into(),
            })
        }
    }
    fn execute(&self, call: ToolCall, _: ToolExecutionContext) -> ToolFuture<'_> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if call.arguments["text"] == "执行失败" {
                return Err(ToolExecutionError::Failed("可诊断失败".into()));
            }
            if call.arguments["text"] == "超时" {
                std::future::pending::<()>().await;
            }
            if call.arguments["text"] == "取消" {
                return Err(ToolExecutionError::Cancelled("本地取消".into()));
            }
            Ok(json!({"receipt":call.arguments["text"]}))
        })
    }
}
pub struct ServicesPlugin {
    pub manifest: PluginManifest,
    pub context: Arc<Context>,
    pub tool: Arc<dyn Tool>,
}
impl Plugin for ServicesPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        let assembler = self.context.clone();
        let tool = self.tool.clone();
        Box::pin(async move {
            context.provide_service(ServiceId::new(CONTEXT)?, ContextService(assembler))?;
            context.provide_service(ServiceId::new(TOOL)?, ToolService(tool))?;
            Ok(None)
        })
    }
}
pub struct Rig {
    pub kernel: Kernel,
    pub registry: Arc<dyn ServiceRegistry>,
    pub permissions: Arc<dyn PermissionChecker>,
    pub logger: Arc<MemoryLogger>,
    pub context: Arc<Context>,
    pub starts: Arc<AtomicUsize>,
    pub host: Arc<SessionLlmHost>,
}
impl Rig {
    pub async fn new(
        provider: Arc<dyn LlmProvider>,
        state: Arc<dyn StateStore>,
        config: LlmHostConfig,
    ) -> Self {
        let logger = Arc::new(MemoryLogger::default());
        let backends = KernelServices {
            state,
            logger: logger.clone(),
            ..KernelServices::default()
        };
        let registry = backends.registry.clone();
        let permissions = backends.permissions.clone();
        let kernel = Kernel::with_services(backends);
        let context = Arc::new(Context::default());
        let starts = Arc::new(AtomicUsize::new(0));
        kernel
            .register(Box::new(ServicesPlugin {
                manifest: PluginManifest::new(OWNER, "0.1.0").unwrap(),
                context: context.clone(),
                tool: Arc::new(ReceiptTool {
                    starts: starts.clone(),
                }),
            }))
            .unwrap();
        kernel
            .register(Box::new(SessionPlugin::new().unwrap()))
            .unwrap();
        kernel.start(&id(OWNER)).await.unwrap();
        kernel.start(&id(SESSION_PLUGIN_ID)).await.unwrap();
        let host = Arc::new(Self::make_host(
            &kernel,
            &registry,
            &permissions,
            &logger,
            provider,
            config,
            SessionBinding::builtin(),
        ));
        Self {
            kernel,
            registry,
            permissions,
            logger,
            context,
            starts,
            host,
        }
    }
    pub fn make_host(
        kernel: &Kernel,
        registry: &Arc<dyn ServiceRegistry>,
        permissions: &Arc<dyn PermissionChecker>,
        logger: &Arc<MemoryLogger>,
        provider: Arc<dyn LlmProvider>,
        config: LlmHostConfig,
        binding: SessionBinding,
    ) -> SessionLlmHost {
        let host = LlmHost::new(
            provider,
            registry.clone(),
            kernel.clone(),
            permissions.clone(),
            ContextBinding {
                service_id: ServiceId::new(CONTEXT).unwrap(),
                expected_owner: id(OWNER),
            },
            vec![ToolBinding {
                name: "receipt".into(),
                service_id: ServiceId::new(TOOL).unwrap(),
                expected_owner: id(OWNER),
            }],
            config,
        )
        .unwrap();
        SessionLlmHost::new(host, binding).with_logger(logger.clone())
    }
    pub fn service(&self) -> Arc<dyn SessionService> {
        self.registry
            .get(&ServiceId::new(SESSION_SERVICE_ID).unwrap())
            .unwrap()
            .unwrap()
            .value
            .downcast::<SessionServiceHandle>()
            .unwrap()
            .0
            .clone()
    }
    pub fn snapshot(&self, session: &str) -> SessionSnapshot {
        self.service().snapshot(&key(session)).unwrap().unwrap()
    }
    pub async fn stop(&self) {
        tokio::time::timeout(Duration::from_secs(3), self.kernel.stop_all())
            .await
            .unwrap()
            .unwrap();
    }
}
