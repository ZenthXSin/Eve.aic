//! LLM 瀹夸富鐨勬渶灏忕粍鍚堝疄鐜般€?//!
//! 鏈ā鍧楀彧璐熻矗鎶婂凡缁忔敞鍐岀殑 Provider銆丆ontext 鍜?Tool 鏈嶅姟瑁呴厤鎴愪竴杞?//! 璇锋眰銆傛湇鍔′粛閫氳繃 `plugin-api` 鐨勫叕寮€娉ㄥ唽琛ㄨ闂紝涓氬姟鎻掍欢涓嶄緷璧栨湰妯″潡銆?
use eve_llm_api::{
    ChatMessage, ChatRole, ContextAssembler, ContextService, ContextSnapshot, LlmError,
    LlmProvider, ModelRequest, ModelResponse, Tool, ToolBinding, ToolCall, ToolCancellation,
    ToolConcurrency, ToolDefinition, ToolExecutionContext, ToolFailureCode, ToolResult,
    ToolService, TurnInput,
};
use eve_plugin_api::{
    Permission, PermissionChecker, PluginError, PluginId, PluginManifest, PluginState,
    RuntimeInspector, ServiceId, ServiceRegistry,
};
use std::collections::{HashMap, HashSet};
use std::future::{Future, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;
use std::time::Duration;
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextBinding {
    pub service_id: ServiceId,
    pub expected_owner: PluginId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LlmHostConfig {
    pub version: String,
    pub system_prompt: String,
    pub output_format: String,
    pub provider_timeout: Duration,
    pub tool_timeout: Duration,
    pub tool_cancellation_grace: Duration,
    pub max_parallel_tool_calls: usize,
    pub max_tool_rounds: usize,
}

impl Default for LlmHostConfig {
    fn default() -> Self {
        Self {
            version: "1".into(),
            system_prompt: "You are Eve's assistant".into(),
            output_format: "plain text".into(),
            provider_timeout: Duration::from_secs(30),
            tool_timeout: Duration::from_secs(10),
            tool_cancellation_grace: Duration::from_millis(100),
            max_parallel_tool_calls: 10,
            max_tool_rounds: 1,
        }
    }
}

impl LlmHostConfig {
    pub fn validate(&self) -> Result<(), LlmError> {
        if self.version.trim().is_empty() {
            return Err(LlmError::Configuration("閰嶇疆鐗堟湰涓嶈兘涓虹┖".into()));
        }
        if self.system_prompt.trim().is_empty() {
            return Err(LlmError::Configuration("绯荤粺鎸囦护涓嶈兘涓虹┖".into()));
        }
        if self.output_format.trim().is_empty() {
            return Err(LlmError::Configuration("杈撳嚭鏍煎紡涓嶈兘涓虹┖".into()));
        }
        if self.provider_timeout.is_zero()
            || self.tool_timeout.is_zero()
            || self.tool_cancellation_grace.is_zero()
            || self.max_parallel_tool_calls == 0
            || self.max_tool_rounds != 1
        {
            return Err(LlmError::Configuration(
                "timeouts and concurrency must be positive; tool rounds must be 1".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnStage {
    Context,
    Provider,
    Tools,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCallDiagnostic {
    pub id: String,
    pub name: String,
    pub ordinal: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnDiagnostics {
    pub stage: TurnStage,
    pub provider_requests: usize,
    pub tool_batches: usize,
    pub started_tools: usize,
    pub peak_parallelism: usize,
    pub calls: Vec<ToolCallDiagnostic>,
    pub tool_results: Vec<ToolResult>,
}

impl Default for TurnDiagnostics {
    fn default() -> Self {
        Self {
            stage: TurnStage::Context,
            provider_requests: 0,
            tool_batches: 0,
            started_tools: 0,
            peak_parallelism: 0,
            calls: Vec::new(),
            tool_results: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnOutput {
    pub text: String,
    pub diagnostics: TurnDiagnostics,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnFailure {
    pub error: LlmError,
    pub diagnostics: TurnDiagnostics,
}

struct PreparedTool {
    binding: ToolBinding,
    definition: ToolDefinition,
}

struct PreparedTurn {
    context: ContextSnapshot,
    tools: Vec<PreparedTool>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum ScopeKey {
    Default,
    Named(String),
}

struct SerialQueue {
    tail: Mutex<Option<Arc<Notify>>>,
}

struct SerialTicket {
    predecessor: Option<Arc<Notify>>,
    release: Arc<Notify>,
}

impl Drop for SerialTicket {
    fn drop(&mut self) {
        self.release.notify_one();
    }
}

impl SerialTicket {
    async fn wait_turn(&self) {
        if let Some(predecessor) = &self.predecessor {
            predecessor.notified().await;
        }
    }
}

struct PreparedCall {
    index: usize,
    call: ToolCall,
    service: Arc<dyn Tool>,
    scope: Option<ScopeKey>,
    ticket: Option<SerialTicket>,
}

struct ToolTask {
    index: usize,
    call_id: String,
    cancellation: ToolCancellation,
    handle: tokio::task::JoinHandle<ToolResult>,
}

struct ToolTaskGuard {
    tasks: Vec<ToolTask>,
}

impl Drop for ToolTaskGuard {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.cancellation.cancel();
            if !task.handle.is_finished() {
                task.handle.abort();
            }
        }
    }
}

fn tool_failure(call: &ToolCall, code: ToolFailureCode, message: impl Into<String>) -> ToolResult {
    ToolResult::failure(call.id.clone(), code, message).expect("validated tool call id")
}

async fn issue_serial_ticket(
    scopes: &Arc<Mutex<HashMap<ScopeKey, Arc<SerialQueue>>>>,
    scope: ScopeKey,
) -> SerialTicket {
    let queue = {
        let mut queues = scopes.lock().await;
        queues
            .entry(scope)
            .or_insert_with(|| {
                Arc::new(SerialQueue {
                    tail: Mutex::new(None),
                })
            })
            .clone()
    };
    let mut tail = queue.tail.lock().await;
    let predecessor = tail.take();
    let release = Arc::new(Notify::new());
    *tail = Some(release.clone());
    SerialTicket {
        predecessor,
        release,
    }
}

async fn contain_panic<T, F>(future: F) -> Result<T, LlmError>
where
    F: Future<Output = Result<T, LlmError>>,
{
    let mut future = Box::pin(future);
    poll_fn(
        |context| match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(context))) {
            Ok(result) => result,
            Err(_) => Poll::Ready(Err(LlmError::Backend("寮傛鍥炶皟鍙戠敓寮傚父".into()))),
        },
    )
    .await
}

enum CallLookupError {
    Unavailable,
    OwnerMismatch,
    Inactive,
    Backend(String),
}

impl CallLookupError {
    fn as_tool_failure(&self) -> (ToolFailureCode, &'static str) {
        match self {
            Self::Unavailable => (ToolFailureCode::Unavailable, "tool service unavailable"),
            Self::OwnerMismatch => (ToolFailureCode::OwnerMismatch, "tool owner mismatch"),
            Self::Inactive => (ToolFailureCode::Inactive, "plugin inactive"),
            Self::Backend(_) => (ToolFailureCode::Unavailable, "tool service unavailable"),
        }
    }
}

enum PermissionFailure {
    Denied,
    Backend(PluginError),
}

pub struct LlmHost {
    provider: Arc<dyn LlmProvider>,
    registry: Arc<dyn ServiceRegistry>,
    inspector: Arc<dyn RuntimeInspector>,
    permissions: Arc<dyn PermissionChecker>,
    context: ContextBinding,
    bindings: Vec<ToolBinding>,
    config: LlmHostConfig,
    gate: Arc<Mutex<()>>,
}

impl LlmHost {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        registry: Arc<dyn ServiceRegistry>,
        inspector: Arc<dyn RuntimeInspector>,
        permissions: Arc<dyn PermissionChecker>,
        context: ContextBinding,
        bindings: Vec<ToolBinding>,
        config: LlmHostConfig,
    ) -> Result<Self, LlmError> {
        config.validate()?;
        validate_context_binding(&context)?;
        let mut names = HashSet::new();
        for binding in &bindings {
            if binding.name.trim().is_empty()
                || binding.service_id.as_str().trim().is_empty()
                || binding.expected_owner.as_str().trim().is_empty()
            {
                return Err(LlmError::Configuration(
                    "宸ュ叿缁戝畾鏍囪瘑涓嶈兘涓虹┖".into(),
                ));
            }
            if !names.insert(binding.name.clone()) {
                return Err(LlmError::Configuration(format!(
                    "宸ュ叿缁戝畾鍚嶇О閲嶅: {}",
                    binding.name
                )));
            }
        }
        Ok(Self {
            provider,
            registry,
            inspector,
            permissions,
            context,
            bindings,
            config,
            gate: Arc::new(Mutex::new(())),
        })
    }

    pub async fn run_turn(&self, input: TurnInput) -> Result<TurnOutput, TurnFailure> {
        let _gate = self.gate.lock().await;
        let mut diagnostics = TurnDiagnostics::default();
        let prepared = match self.prepare(&input, &mut diagnostics).await {
            Ok(value) => value,
            Err(error) => return Err(fail(error, diagnostics)),
        };
        let mut messages = build_messages(&self.config, &prepared.context, &input);
        let definitions = prepared
            .tools
            .iter()
            .map(|tool| tool.definition.clone())
            .collect::<Vec<_>>();
        let request = ModelRequest {
            messages: messages.clone(),
            tools: definitions,
        };
        let response = match self.complete(request, &mut diagnostics).await {
            Ok(value) => value,
            Err(error) => return Err(fail(error, diagnostics)),
        };
        if let ModelResponse::Final { text } = response {
            diagnostics.stage = TurnStage::Completed;
            return Ok(TurnOutput { text, diagnostics });
        }
        let ModelResponse::ToolCalls { calls } = response else {
            unreachable!()
        };
        diagnostics.stage = TurnStage::Tools;
        diagnostics.tool_batches += 1;
        diagnostics.calls = calls
            .iter()
            .enumerate()
            .map(|(ordinal, call)| ToolCallDiagnostic {
                id: call.id.clone(),
                name: call.name.clone(),
                ordinal,
            })
            .collect();
        let results = match self
            .execute_calls_ordered(&prepared.tools, &calls, &mut diagnostics)
            .await
        {
            Ok(results) => results,
            Err(error) => return Err(fail(error, diagnostics)),
        };
        messages.push(
            ChatMessage::assistant_tool_calls(calls.clone())
                .map_err(|error| fail(error, diagnostics.clone()))?,
        );
        messages.push(
            ChatMessage::tool_results(results.clone())
                .map_err(|error| fail(error, diagnostics.clone()))?,
        );
        diagnostics.tool_results = results;
        let response = match self
            .complete(
                ModelRequest {
                    messages,
                    tools: prepared
                        .tools
                        .iter()
                        .map(|tool| tool.definition.clone())
                        .collect(),
                },
                &mut diagnostics,
            )
            .await
        {
            Ok(value) => value,
            Err(error) => return Err(fail(error, diagnostics)),
        };
        match response {
            ModelResponse::Final { text } => {
                diagnostics.stage = TurnStage::Completed;
                Ok(TurnOutput { text, diagnostics })
            }
            ModelResponse::ToolCalls { .. } => Err(fail(LlmError::RoundLimit, diagnostics)),
        }
    }

    async fn prepare(
        &self,
        input: &TurnInput,
        diagnostics: &mut TurnDiagnostics,
    ) -> Result<PreparedTurn, LlmError> {
        if input.text.trim().is_empty() {
            return Err(LlmError::Context("鏈疆杈撳叆涓嶈兘涓虹┖".into()));
        }
        let assembler = self.context_service()?;
        let context = contain_panic(async { assembler.assemble(input.clone()).await }).await?;
        validate_context(&context)?;
        let mut tools = Vec::with_capacity(self.bindings.len());
        for binding in &self.bindings {
            let service = self.tool_service(binding)?;
            let definition = catch_unwind(AssertUnwindSafe(|| service.definition()))
                .map_err(|_| LlmError::Configuration("宸ュ叿瀹氫箟瑁呴厤鍙戠敓寮傚父".into()))?;
            definition.validate()?;
            if definition.name != binding.name {
                return Err(LlmError::Configuration(format!(
                    "宸ュ叿瀹氫箟鍚嶇О婕傜Щ: {}",
                    binding.name
                )));
            }
            check_permissions(
                &self.permissions,
                &self
                    .inspector
                    .plugin_manifest(&binding.expected_owner)
                    .map_err(backend)?,
                &definition.required_permissions,
            )
            .map_err(|error| match error {
                PermissionFailure::Denied => {
                    LlmError::Configuration("tool permission preflight denied".into())
                }
                PermissionFailure::Backend(error) => backend(error),
            })?;
            tools.push(PreparedTool {
                binding: binding.clone(),
                definition,
            });
        }
        tools.sort_by(|left, right| left.definition.name.cmp(&right.definition.name));
        diagnostics.stage = TurnStage::Provider;
        Ok(PreparedTurn { context, tools })
    }

    async fn complete(
        &self,
        request: ModelRequest,
        diagnostics: &mut TurnDiagnostics,
    ) -> Result<ModelResponse, LlmError> {
        request.validate()?;
        diagnostics.provider_requests += 1;
        let provider = self.provider.clone();
        let future = catch_unwind(AssertUnwindSafe(|| provider.complete(request)))
            .map_err(|_| LlmError::Provider("Provider 璇锋眰鍙戠敓寮傚父".into()))?;
        let response = tokio::select! {
            biased;
            _ = tokio::time::sleep(self.config.provider_timeout) => {
                return Err(LlmError::ProviderTimeout);
            }
            response = contain_panic(future) => {
                response?
            }
        };
        response.validate()?;
        Ok(response)
    }

    fn context_service(&self) -> Result<Arc<dyn ContextAssembler>, LlmError> {
        let entry = self
            .registry
            .get(&self.context.service_id)
            .map_err(backend)?
            .ok_or_else(|| LlmError::Configuration("涓婁笅鏂囨湇鍔′笉瀛樺湪".into()))?;
        ensure_owner_and_active(&*self.inspector, &entry.owner, &self.context.expected_owner)?;
        Arc::downcast::<ContextService>(entry.value)
            .map(|service| service.0.clone())
            .map_err(|_| LlmError::Configuration("涓婁笅鏂囨湇鍔＄被鍨嬩笉鍖归厤".into()))
    }

    fn tool_service(&self, binding: &ToolBinding) -> Result<Arc<dyn Tool>, LlmError> {
        let entry = self
            .registry
            .get(&binding.service_id)
            .map_err(backend)?
            .ok_or_else(|| {
                LlmError::Configuration(format!("宸ュ叿鏈嶅姟涓嶅瓨鍦? {}", binding.name))
            })?;
        ensure_owner_and_active(&*self.inspector, &entry.owner, &binding.expected_owner)?;
        Arc::downcast::<ToolService>(entry.value)
            .map(|service| service.0.clone())
            .map_err(|_| {
                LlmError::Configuration(format!("宸ュ叿鏈嶅姟绫诲瀷涓嶅尮閰? {}", binding.name))
            })
    }

    async fn execute_calls_ordered(
        &self,
        prepared: &[PreparedTool],
        calls: &[ToolCall],
        diagnostics: &mut TurnDiagnostics,
    ) -> Result<Vec<ToolResult>, LlmError> {
        let by_name = prepared
            .iter()
            .map(|tool| (tool.binding.name.clone(), tool))
            .collect::<HashMap<_, _>>();
        let semaphore = Arc::new(Semaphore::new(self.config.max_parallel_tool_calls));
        let scopes = Arc::new(Mutex::new(HashMap::<ScopeKey, Arc<SerialQueue>>::new()));
        let mut results = vec![None; calls.len()];
        let mut ready = Vec::with_capacity(calls.len());
        let started = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        // Complete validation runs before any tool task starts.
        for (index, call) in calls.iter().cloned().enumerate() {
            let Some(prepared_tool) = by_name.get(&call.name) else {
                results[index] = Some(tool_failure(
                    &call,
                    ToolFailureCode::UnknownTool,
                    "鏈煡宸ュ叿",
                ));
                continue;
            };
            let binding = prepared_tool.binding.clone();
            let definition = prepared_tool.definition.clone();
            let service = match self.lookup_tool_service(&binding) {
                Ok(value) => value,
                Err(CallLookupError::Backend(message)) => return Err(LlmError::Backend(message)),
                Err(error) => {
                    let (code, message) = error.as_tool_failure();
                    results[index] = Some(tool_failure(&call, code, message));
                    continue;
                }
            };
            let current_definition =
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    service.definition()
                })) {
                    Ok(definition) => definition,
                    Err(_) => {
                        results[index] = Some(tool_failure(
                            &call,
                            ToolFailureCode::Unavailable,
                            "tool definition unavailable",
                        ));
                        continue;
                    }
                };
            if current_definition != definition {
                results[index] = Some(tool_failure(
                    &call,
                    ToolFailureCode::Unavailable,
                    "tool definition changed",
                ));
                continue;
            }
            let validation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                service.validate_arguments(&call.arguments)
            }));
            let validation = match validation {
                Ok(validation) => validation,
                Err(_) => {
                    results[index] = Some(tool_failure(
                        &call,
                        ToolFailureCode::ExecutionFailed,
                        "宸ュ叿鍙傛暟鏍￠獙澶辫触",
                    ));
                    continue;
                }
            };
            if let Err(error) = validation {
                results[index] = Some(tool_failure(
                    &call,
                    ToolFailureCode::InvalidArguments,
                    error.message,
                ));
                continue;
            }
            let manifest = match self.inspector.plugin_manifest(&binding.expected_owner) {
                Ok(manifest) => manifest,
                Err(PluginError::PluginNotFound(_)) => {
                    results[index] = Some(tool_failure(
                        &call,
                        ToolFailureCode::Inactive,
                        "plugin inactive",
                    ));
                    continue;
                }
                Err(error) => return Err(backend(error)),
            };
            if let Err(error) = check_permissions(
                &self.permissions,
                &manifest,
                &definition.required_permissions,
            ) {
                match error {
                    PermissionFailure::Denied => {
                        results[index] = Some(tool_failure(
                            &call,
                            ToolFailureCode::PermissionDenied,
                            "tool permission denied",
                        ));
                        continue;
                    }
                    PermissionFailure::Backend(error) => return Err(backend(error)),
                }
            }
            let scope = match definition.concurrency {
                Some(ToolConcurrency::ParallelSafe) => None,
                Some(ToolConcurrency::Serial { scope }) => Some(ScopeKey::Named(scope)),
                None => Some(ScopeKey::Default),
            };
            ready.push(PreparedCall {
                index,
                call,
                service,
                scope,
                ticket: None,
            });
        }
        // Only after validation, reserve serial tickets and start tasks.
        for prepared_call in &mut ready {
            if let Some(scope) = prepared_call.scope.take() {
                prepared_call.ticket = Some(issue_serial_ticket(&scopes, scope).await);
            }
        }
        let mut tasks = ToolTaskGuard {
            tasks: Vec::with_capacity(ready.len()),
        };
        for prepared_call in ready {
            let context = ToolExecutionContext::new();
            let cancellation = context.cancellation_handle();
            let call_id = prepared_call.call.id.clone();
            let run_config = ToolRunConfig {
                semaphore: semaphore.clone(),
                ticket: prepared_call.ticket,
                started: started.clone(),
                active: active.clone(),
                peak: peak.clone(),
                timeout: self.config.tool_timeout,
                grace: self.config.tool_cancellation_grace,
            };
            tasks.tasks.push(ToolTask {
                index: prepared_call.index,
                call_id,
                cancellation,
                handle: tokio::spawn(async move {
                    run_tool(
                        prepared_call.service,
                        prepared_call.call,
                        context,
                        run_config,
                    )
                    .await
                }),
            });
        }
        for task in &mut tasks.tasks {
            match (&mut task.handle).await {
                Ok(result) => results[task.index] = Some(result),
                Err(error) => {
                    results[task.index] = Some(
                        ToolResult::failure(
                            task.call_id.clone(),
                            ToolFailureCode::ExecutionFailed,
                            if error.is_panic() {
                                "宸ュ叿鎵ц寮傚父"
                            } else {
                                "tool execution cancelled"
                            },
                        )
                        .expect("call id validated"),
                    );
                }
            }
        }
        diagnostics.started_tools += started.load(Ordering::SeqCst);
        diagnostics.peak_parallelism = diagnostics
            .peak_parallelism
            .max(peak.load(Ordering::SeqCst));
        Ok(results
            .into_iter()
            .map(|result| result.expect("every tool call has a result"))
            .collect())
    }

    fn lookup_tool_service(&self, binding: &ToolBinding) -> Result<Arc<dyn Tool>, CallLookupError> {
        let entry = self
            .registry
            .get(&binding.service_id)
            .map_err(|error| CallLookupError::Backend(error.to_string()))?
            .ok_or(CallLookupError::Unavailable)?;
        if entry.owner != binding.expected_owner {
            return Err(CallLookupError::OwnerMismatch);
        }
        match self.inspector.plugins() {
            Ok(statuses) => {
                let Some(status) = statuses
                    .iter()
                    .find(|status| status.info.id == binding.expected_owner)
                else {
                    return Err(CallLookupError::Inactive);
                };
                if status.state != PluginState::Active {
                    return Err(CallLookupError::Inactive);
                }
            }
            Err(error) => return Err(CallLookupError::Backend(error.to_string())),
        }
        Arc::downcast::<ToolService>(entry.value)
            .map(|service| service.0.clone())
            .map_err(|_| CallLookupError::Unavailable)
    }
}

struct ActiveExecutionGuard {
    active: Arc<AtomicUsize>,
}

impl Drop for ActiveExecutionGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn run_tool(
    tool: Arc<dyn Tool>,
    call: ToolCall,
    context: ToolExecutionContext,
    config: ToolRunConfig,
) -> ToolResult {
    let ToolRunConfig {
        semaphore,
        ticket,
        started,
        active,
        peak,
        timeout,
        grace,
    } = config;
    let _ticket = ticket;
    if let Some(ticket) = &_ticket {
        ticket.wait_turn().await;
    }
    let permit: OwnedSemaphorePermit = semaphore.acquire_owned().await.expect("semaphore lives");
    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
    started.fetch_add(1, Ordering::SeqCst);
    peak.fetch_max(current, Ordering::SeqCst);
    let _active_guard = ActiveExecutionGuard { active };
    let cancellation: ToolCancellation = context.cancellation_handle();
    let future = tool.execute(call.clone(), context);
    tokio::pin!(future);
    let value = tokio::select! {
        biased;
        _ = tokio::time::sleep(timeout) => {
            cancellation.cancel();
            let _ = tokio::time::timeout(grace, &mut future).await;
            ToolResult::failure(call.id, ToolFailureCode::TimedOut, "tool execution timed out")
        }
        result = &mut future => match result {
            Ok(value) => ToolResult::success(call.id, value),
            Err(error) => {
                let code = if matches!(error, eve_llm_api::ToolExecutionError::Cancelled(_)) {
                    ToolFailureCode::Cancelled
                } else {
                    ToolFailureCode::ExecutionFailed
                };
                ToolResult::failure(call.id, code, error.to_string())
            }
        },
    };
    drop(permit);
    value.expect("call id validated")
}

struct ToolRunConfig {
    semaphore: Arc<Semaphore>,
    ticket: Option<SerialTicket>,
    started: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    timeout: Duration,
    grace: Duration,
}

fn build_messages(
    config: &LlmHostConfig,
    context: &ContextSnapshot,
    input: &TurnInput,
) -> Vec<ChatMessage> {
    let mut messages = vec![ChatMessage::text(
        ChatRole::System,
        format!(
            "{}\noutput format: {}",
            config.system_prompt, config.output_format
        ),
    )];
    messages.push(ChatMessage::text(
        ChatRole::System,
        format!("context revision: {}", context.revision),
    ));
    if !context.profile.is_empty() {
        messages.push(ChatMessage::text(
            ChatRole::System,
            format!("profile: {}", context.profile),
        ));
    }
    for memory in &context.memories {
        messages.push(ChatMessage::text(
            ChatRole::System,
            format!("memory: {}", memory),
        ));
    }
    messages.extend(context.history.clone());
    messages.push(ChatMessage::text(ChatRole::User, input.text.clone()));
    messages
}

fn validate_context_binding(binding: &ContextBinding) -> Result<(), LlmError> {
    if binding.service_id.as_str().trim().is_empty()
        || binding.expected_owner.as_str().trim().is_empty()
    {
        Err(LlmError::Configuration(
            "context binding identifiers cannot be empty".into(),
        ))
    } else {
        Ok(())
    }
}

fn validate_context(context: &ContextSnapshot) -> Result<(), LlmError> {
    if context.revision.trim().is_empty() {
        return Err(LlmError::Context("context revision cannot be empty".into()));
    }
    if context.history.iter().any(|message| {
        !matches!(message.role, ChatRole::User | ChatRole::Assistant)
            || message
                .text
                .as_ref()
                .is_none_or(|text| text.trim().is_empty())
            || !message.tool_calls.is_empty()
            || !message.tool_results.is_empty()
    }) {
        return Err(LlmError::Context(
            "history must contain complete user or assistant text messages".into(),
        ));
    }
    Ok(())
}

fn ensure_owner_and_active(
    inspector: &dyn RuntimeInspector,
    actual: &PluginId,
    expected: &PluginId,
) -> Result<(), LlmError> {
    if actual != expected {
        return Err(LlmError::Backend(format!(
            "鏈嶅姟鎵€鏈夎€呬笉鍖归厤: {actual}"
        )));
    }
    let statuses = inspector.plugins().map_err(backend)?;
    let status = statuses
        .iter()
        .find(|status| status.info.id == *expected)
        .ok_or_else(|| LlmError::Backend(format!("鎻掍欢涓嶅瓨鍦? {expected}")))?;
    if status.state != PluginState::Active {
        return Err(LlmError::Backend(format!("鎻掍欢鏈縺娲? {expected}")));
    }
    Ok(())
}

fn check_permissions(
    checker: &Arc<dyn PermissionChecker>,
    manifest: &PluginManifest,
    permissions: &[Permission],
) -> Result<(), PermissionFailure> {
    for permission in permissions {
        if let Err(error) = checker.check(manifest, permission) {
            if matches!(error, PluginError::PermissionDenied { .. }) {
                return Err(PermissionFailure::Denied);
            }
            return Err(PermissionFailure::Backend(error));
        }
    }
    Ok(())
}

fn backend(error: PluginError) -> LlmError {
    LlmError::Backend(error.to_string())
}

fn fail(error: LlmError, mut diagnostics: TurnDiagnostics) -> TurnFailure {
    diagnostics.stage = TurnStage::Failed;
    TurnFailure { error, diagnostics }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_kernel::{Kernel, KernelServices};
    use eve_llm_api::{LlmFuture, ToolOutput, ToolValidationError};
    use eve_plugin_api::{Cleanup, Plugin, PluginContext, PluginFuture, PluginManifest};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    const CONTEXT: &str = "test.context";
    const TOOL: &str = "test.tool";
    const OWNER: &str = "test.owner";

    struct ContextImpl;
    impl ContextAssembler for ContextImpl {
        fn assemble(&self, _input: TurnInput) -> LlmFuture<'_, ContextSnapshot> {
            Box::pin(async move {
                Ok(ContextSnapshot {
                    revision: "r1".into(),
                    profile: "娴嬭瘯鐢ㄦ埛".into(),
                    memories: vec!["鍥哄畾璁板繂".into()],
                    history: vec![],
                })
            })
        }
    }

    struct EchoTool {
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }
    impl Tool for EchoTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "echo".into(),
                description: "鍥炴樉".into(),
                argument_schema: json!({"type":"object"}),
                required_permissions: vec![],
                concurrency: Some(ToolConcurrency::ParallelSafe),
            }
        }
        fn validate_arguments(
            &self,
            arguments: &serde_json::Value,
        ) -> Result<(), ToolValidationError> {
            if arguments.get("value").is_some() {
                Ok(())
            } else {
                Err(ToolValidationError {
                    message: "缂哄皯 value".into(),
                })
            }
        }
        fn execute(
            &self,
            call: ToolCall,
            _context: ToolExecutionContext,
        ) -> eve_llm_api::ToolFuture<'_> {
            let active = self.active.clone();
            let peak = self.peak.clone();
            Box::pin(async move {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(json!({"echo": call.arguments["value"].clone()}))
            })
        }
    }

    struct ScriptedProvider {
        calls: Mutex<Vec<ModelResponse>>,
    }
    impl LlmProvider for ScriptedProvider {
        fn complete(&self, _request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .await
                    .pop()
                    .ok_or_else(|| LlmError::Provider("鑴氭湰鑰楀敖".into()))
            })
        }
    }

    struct OwnerPlugin {
        manifest: PluginManifest,
    }
    impl Plugin for OwnerPlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }
        fn start(&mut self, _ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
            Box::pin(async { Ok(None) })
        }
    }

    async fn setup(provider: Arc<dyn LlmProvider>, tool: Arc<dyn Tool>) -> LlmHost {
        let services = KernelServices::default();
        let registry = services.registry.clone();
        let permissions = services.permissions.clone();
        let kernel = Kernel::with_services(services);
        let manifest = PluginManifest::new(OWNER, "0.1.0").unwrap();
        kernel.register(Box::new(OwnerPlugin { manifest })).unwrap();
        kernel.start(&PluginId::new(OWNER).unwrap()).await.unwrap();
        let _context_cleanup = registry
            .clone()
            .provide(
                PluginId::new(OWNER).unwrap(),
                ServiceId::new(CONTEXT).unwrap(),
                Arc::new(ContextService(Arc::new(ContextImpl))),
            )
            .unwrap();
        let _tool_cleanup = registry
            .clone()
            .provide(
                PluginId::new(OWNER).unwrap(),
                ServiceId::new(TOOL).unwrap(),
                Arc::new(ToolService(tool)),
            )
            .unwrap();
        LlmHost::new(
            provider,
            registry,
            Arc::new(kernel),
            permissions,
            ContextBinding {
                service_id: ServiceId::new(CONTEXT).unwrap(),
                expected_owner: PluginId::new(OWNER).unwrap(),
            },
            vec![ToolBinding {
                name: "echo".into(),
                service_id: ServiceId::new(TOOL).unwrap(),
                expected_owner: PluginId::new(OWNER).unwrap(),
            }],
            LlmHostConfig::default(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn mock_provider_tool_round_trip_preserves_results() {
        let provider = Arc::new(ScriptedProvider {
            calls: Mutex::new(vec![
                ModelResponse::Final {
                    text: "瀹屾垚".into(),
                },
                ModelResponse::ToolCalls {
                    calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "echo".into(),
                        arguments: json!({"value":"x"}),
                    }],
                },
            ]),
        });
        let host = setup(
            provider,
            Arc::new(EchoTool {
                active: Arc::new(AtomicUsize::new(0)),
                peak: Arc::new(AtomicUsize::new(0)),
            }),
        )
        .await;
        let output = host
            .run_turn(TurnInput {
                text: "start".into(),
            })
            .await
            .unwrap();
        assert_eq!(output.text, "瀹屾垚");
        assert_eq!(output.diagnostics.provider_requests, 2);
        assert_eq!(output.diagnostics.tool_results[0].call_id, "c1");
    }

    #[tokio::test]
    async fn unknown_tool_is_returned_to_provider_as_failure() {
        let provider = Arc::new(ScriptedProvider {
            calls: Mutex::new(vec![
                ModelResponse::Final {
                    text: "processed".into(),
                },
                ModelResponse::ToolCalls {
                    calls: vec![ToolCall {
                        id: "u1".into(),
                        name: "missing".into(),
                        arguments: json!({}),
                    }],
                },
            ]),
        });
        let host = setup(
            provider,
            Arc::new(EchoTool {
                active: Arc::new(AtomicUsize::new(0)),
                peak: Arc::new(AtomicUsize::new(0)),
            }),
        )
        .await;
        let output = host
            .run_turn(TurnInput {
                text: "start".into(),
            })
            .await
            .unwrap();
        assert!(matches!(
            output.diagnostics.tool_results[0].output,
            ToolOutput::Failure {
                code: ToolFailureCode::UnknownTool,
                ..
            }
        ));
    }
}
