//! LLM 宿主的最小组合实现。
//!
//! 本模块负责将已经注册的 Provider、Context 和 Tool 服务组合为一次请求。
//! 服务仍通过 `plugin-api` 的公开注册表访问，业务插件不依赖本模块。
use eve_kernel::{Kernel, RuntimeAdmissionGuard};
use eve_llm_api::{
    ChatMessage, ChatRole, ContextAssembler, ContextService, ContextSnapshot, LlmError, LlmFuture,
    LlmModelResolver, LlmProvider, ModelRequest, ModelResponse, ModelTextSink, ResponseMode,
    SystemPromptMetadata, SystemPromptSnapshot, SystemPromptSource, Tool, ToolBinding, ToolCall,
    ToolCancellation, ToolConcurrency, ToolDefinition, ToolExecutionContext, ToolFailureCode,
    ToolResult, ToolService, TurnEvent, TurnEventKind, TurnEventSink, TurnInput,
};
use eve_plugin_api::{
    Permission, PermissionChecker, PluginError, PluginId, PluginManifest, PluginState,
    RuntimeInspector, ServiceId, ServiceRegistry,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::{Future, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::task::Poll;
use std::time::Duration;
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextBinding {
    pub service_id: ServiceId,
    pub expected_owner: PluginId,
}

#[derive(Clone, Eq, PartialEq)]
pub struct LlmHostConfig {
    pub version: String,
    pub system_prompt: String,
    /// 与 system_prompt 一致的不可变来源快照；直接文本兼容路径为 None。
    pub system_prompt_snapshot: Option<SystemPromptSnapshot>,
    pub output_format: String,
    pub provider_timeout: Duration,
    pub tool_timeout: Duration,
    pub tool_cancellation_grace: Duration,
    pub max_parallel_tool_calls: usize,
    pub max_tool_rounds: usize,
    pub response_mode: ResponseMode,
    pub event_timeout: Duration,
    pub max_stream_text_bytes: usize,
}

impl Default for LlmHostConfig {
    fn default() -> Self {
        Self {
            version: "1".into(),
            system_prompt: "You are Eve's assistant".into(),
            system_prompt_snapshot: None,
            output_format: "plain text".into(),
            provider_timeout: Duration::from_secs(30),
            tool_timeout: Duration::from_secs(10),
            tool_cancellation_grace: Duration::from_millis(100),
            max_parallel_tool_calls: 10,
            max_tool_rounds: 1,
            response_mode: ResponseMode::Complete,
            event_timeout: Duration::from_secs(5),
            max_stream_text_bytes: 8 * 1024 * 1024,
        }
    }
}

impl std::fmt::Debug for LlmHostConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmHostConfig")
            .field("version", &self.version)
            .field("system_prompt_bytes", &self.system_prompt.len())
            .field("system_prompt_snapshot", &self.system_prompt_snapshot)
            .field("output_format", &self.output_format)
            .field("provider_timeout", &self.provider_timeout)
            .field("tool_timeout", &self.tool_timeout)
            .field("tool_cancellation_grace", &self.tool_cancellation_grace)
            .field("max_parallel_tool_calls", &self.max_parallel_tool_calls)
            .field("max_tool_rounds", &self.max_tool_rounds)
            .field("response_mode", &self.response_mode)
            .field("event_timeout", &self.event_timeout)
            .field("max_stream_text_bytes", &self.max_stream_text_bytes)
            .finish()
    }
}

impl LlmHostConfig {
    /// 装配时恰好加载一次；显式选择替换旧来源，不拼接身份文本。
    pub fn with_prompt_source(self, source: &dyn SystemPromptSource) -> Result<Self, LlmError> {
        self.with_prompt_snapshot(source.load()?)
    }

    pub fn with_prompt_snapshot(
        mut self,
        snapshot: SystemPromptSnapshot,
    ) -> Result<Self, LlmError> {
        self.system_prompt = snapshot.text().into();
        self.system_prompt_snapshot = Some(snapshot);
        self.validate()?;
        Ok(self)
    }

    pub fn validate(&self) -> Result<(), LlmError> {
        if self.version.trim().is_empty() {
            return Err(LlmError::Configuration("配置版本不能为空".into()));
        }
        if self.system_prompt.trim().is_empty() {
            return Err(LlmError::Configuration("系统指令不能为空".into()));
        }
        if self
            .system_prompt_snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.text() != self.system_prompt)
        {
            return Err(LlmError::Configuration(
                "固定提示词与来源快照不一致，请显式重新装配来源".into(),
            ));
        }
        if self.output_format.trim().is_empty() {
            return Err(LlmError::Configuration("输出格式不能为空".into()));
        }
        if self.event_timeout.is_zero()
            || self.max_stream_text_bytes == 0
            || self.provider_timeout.is_zero()
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
    /// 本轮用户输入、配对工具调用/结果和最终回复，不含系统提示或旧历史。
    pub transcript: Vec<ChatMessage>,
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
    pending: StdMutex<VecDeque<Arc<()>>>,
    changed: Notify,
}

struct SerialTicket {
    queue: Arc<SerialQueue>,
    identity: Arc<()>,
}

impl Drop for SerialTicket {
    fn drop(&mut self) {
        self.queue
            .pending
            .lock()
            .expect("串行队列锁有效")
            .retain(|identity| !Arc::ptr_eq(identity, &self.identity));
        self.queue.changed.notify_waiters();
    }
}

impl SerialTicket {
    async fn wait_turn(&self) {
        loop {
            let changed = self.queue.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self
                .queue
                .pending
                .lock()
                .expect("串行队列锁有效")
                .front()
                .is_some_and(|identity| Arc::ptr_eq(identity, &self.identity))
            {
                return;
            }
            changed.await;
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
                    pending: StdMutex::new(VecDeque::new()),
                    changed: Notify::new(),
                })
            })
            .clone()
    };
    let identity = Arc::new(());
    queue
        .pending
        .lock()
        .expect("串行队列锁有效")
        .push_back(identity.clone());
    SerialTicket { queue, identity }
}

pub(crate) async fn contain_panic<T, F>(future: F) -> Result<T, LlmError>
where
    F: Future<Output = Result<T, LlmError>>,
{
    let mut future = Box::pin(future);
    poll_fn(
        |context| match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(context))) {
            Ok(result) => result,
            Err(_) => Poll::Ready(Err(LlmError::Backend("异步回调发生异常".into()))),
        },
    )
    .await
}

pub(crate) struct EventDelivery<'a> {
    pub sink: &'a dyn TurnEventSink,
    pub turn_id: Option<u64>,
    pub timeout: Duration,
}
impl EventDelivery<'_> {
    pub(crate) async fn emit(&self, kind: TurnEventKind) -> Result<(), LlmError> {
        let event = TurnEvent {
            turn_id: self.turn_id,
            kind,
        };
        tokio::select! {
            biased;
            _ = contain_panic(async { self.sink.closed().await }) => Err(LlmError::Cancelled),
            _ = tokio::time::sleep(self.timeout) => Err(LlmError::Cancelled),
            result = contain_panic(async { self.sink.emit(event).await }) => result,
        }
    }
}
async fn emit(events: Option<&EventDelivery<'_>>, kind: TurnEventKind) -> Result<(), LlmError> {
    if let Some(events) = events {
        events.emit(kind).await?;
    }
    Ok(())
}
async fn wait_closed(events: Option<&EventDelivery<'_>>) {
    if let Some(events) = events {
        let _ = contain_panic(async { events.sink.closed().await }).await;
    } else {
        std::future::pending::<()>().await;
    }
}
struct TextDelivery<'a> {
    events: Option<&'a EventDelivery<'a>>,
    request: usize,
    text: StdMutex<String>,
    limit: usize,
}
impl ModelTextSink for TextDelivery<'_> {
    fn text_delta(&self, text: String) -> LlmFuture<'_, ()> {
        Box::pin(async move {
            {
                let mut combined = self
                    .text
                    .lock()
                    .map_err(|_| LlmError::Backend("增量文本锁失效".into()))?;
                if text.len() > self.limit - combined.len() {
                    return Err(LlmError::Protocol("流式文本超过宿主字节上限".into()));
                }
                combined.push_str(&text);
            }
            emit(
                self.events,
                TurnEventKind::TextDelta {
                    request: self.request,
                    text,
                },
            )
            .await
        })
    }
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

#[derive(Clone)]
pub struct LlmHost {
    provider: Arc<dyn LlmProvider>,
    model_resolver: Option<Arc<dyn LlmModelResolver>>,
    execution_budget: Option<Arc<eve_llm_api::TurnBudget>>,
    pub(crate) registry: Arc<dyn ServiceRegistry>,
    pub(crate) kernel: Kernel,
    permissions: Arc<dyn PermissionChecker>,
    context: ContextBinding,
    bindings: Vec<ToolBinding>,
    config: LlmHostConfig,
    tool_slots: Arc<Semaphore>,
    serial_scopes: Arc<Mutex<HashMap<ScopeKey, Arc<SerialQueue>>>>,
}

impl LlmHost {
    /// 来源元数据不包含正文；旧的直接 system_prompt 路径返回 None。
    pub fn system_prompt_metadata(&self) -> Option<&SystemPromptMetadata> {
        self.config
            .system_prompt_snapshot
            .as_ref()
            .map(SystemPromptSnapshot::metadata)
    }

    pub fn new(
        provider: Arc<dyn LlmProvider>,
        registry: Arc<dyn ServiceRegistry>,
        kernel: Kernel,
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
                return Err(LlmError::Configuration("工具绑定标识不能为空".into()));
            }
            if !names.insert(binding.name.clone()) {
                return Err(LlmError::Configuration(format!(
                    "工具绑定名称重复: {}",
                    binding.name
                )));
            }
        }
        Ok(Self {
            provider,
            model_resolver: None,
            execution_budget: None,
            registry,
            kernel,
            permissions,
            context,
            bindings,
            tool_slots: Arc::new(Semaphore::new(config.max_parallel_tool_calls)),
            serial_scopes: Arc::new(Mutex::new(HashMap::new())),
            config,
        })
    }

    /// 显式启用每轮模型解析；原 new 构造路径继续固定 Provider。
    pub fn with_model_resolver(mut self, resolver: Arc<dyn LlmModelResolver>) -> Self {
        self.model_resolver = Some(resolver);
        self
    }

    /// 每次认知尝试独立预算；克隆继续共享工具槽位和串行队列。
    pub fn with_execution_budget(mut self, budget: Arc<eve_llm_api::TurnBudget>) -> Self {
        self.execution_budget = Some(budget);
        self
    }

    pub(crate) fn for_turn(&self) -> Result<Self, LlmError> {
        let mut host = self.clone();
        if let Some(resolver) = &self.model_resolver {
            let selected = catch_unwind(AssertUnwindSafe(|| resolver.resolve()))
                .map_err(|_| LlmError::Backend("模型解析回调发生异常".into()))??;
            host.provider = selected.provider;
            host.config.provider_timeout = selected.provider_timeout;
            host.config.validate()?;
        }
        host.model_resolver = None;
        Ok(host)
    }

    pub async fn run_turn(&self, input: TurnInput) -> Result<TurnOutput, TurnFailure> {
        let admission = Arc::new(self.kernel.acquire_runtime_admission().await);
        self.for_turn()
            .map_err(|error| fail(error, TurnDiagnostics::default()))?
            .run_turn_inner(input, None, None, admission, None)
            .await
    }

    pub async fn run_turn_with_events(
        &self,
        input: TurnInput,
        sink: &dyn TurnEventSink,
    ) -> Result<TurnOutput, TurnFailure> {
        let admission = Arc::new(self.kernel.acquire_runtime_admission().await);
        let events = self.event_delivery(sink, None);
        let result = match self.for_turn() {
            Ok(host) => {
                host.run_turn_inner(input, None, None, admission.clone(), Some(&events))
                    .await
            }
            Err(error) => Err(fail(error, TurnDiagnostics::default())),
        };
        if let Err(failure) = &result {
            let _ = events
                .emit(TurnEventKind::Failed {
                    error: failure.error.clone(),
                })
                .await;
        }
        result
    }

    pub(crate) fn event_delivery<'a>(
        &self,
        sink: &'a dyn TurnEventSink,
        turn_id: Option<u64>,
    ) -> EventDelivery<'a> {
        EventDelivery {
            sink,
            turn_id,
            timeout: self.config.event_timeout,
        }
    }

    // 会话宿主已持有相同 Kernel 的准入锁，覆盖 Pending 到最终状态提交。
    pub(crate) async fn run_turn_inner(
        &self,
        input: TurnInput,
        history: Option<Vec<ChatMessage>>,
        scope: Option<eve_llm_api::ContextScope>,
        admission: Arc<RuntimeAdmissionGuard>,
        events: Option<&EventDelivery<'_>>,
    ) -> Result<TurnOutput, TurnFailure> {
        let mut diagnostics = TurnDiagnostics::default();
        let prepared = tokio::select! {
            biased;
            _ = wait_closed(events) => Err(LlmError::Cancelled),
            prepared = self.prepare(&input, scope.clone(), &mut diagnostics) => prepared,
        };
        let mut prepared = match prepared {
            Ok(value) => value,
            Err(error) => return Err(fail(error, diagnostics)),
        };
        if let Some(history) = history {
            if !prepared.context.history.is_empty() {
                return Err(fail(
                    LlmError::Context("会话模式只允许会话服务提供历史".into()),
                    diagnostics,
                ));
            }
            prepared.context.history = history;
            if let Err(error) = validate_context(&prepared.context) {
                return Err(fail(error, diagnostics));
            }
        }
        let mut transcript = vec![ChatMessage::text(ChatRole::User, input.text.clone())];
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
        let response = match self.complete(request, &mut diagnostics, events).await {
            Ok(value) => value,
            Err(error) => return Err(fail(error, diagnostics)),
        };
        if let ModelResponse::Final { text } = response {
            diagnostics.stage = TurnStage::Completed;
            transcript.push(ChatMessage::text(ChatRole::Assistant, text.clone()));
            emit(events, TurnEventKind::TurnCompleted { text: text.clone() })
                .await
                .map_err(|error| fail(error, diagnostics.clone()))?;
            return Ok(TurnOutput {
                text,
                diagnostics,
                transcript,
            });
        }
        let ModelResponse::ToolCalls { calls } = response else {
            unreachable!()
        };
        if let Some(budget) = &self.execution_budget {
            budget
                .reserve_tool_batch(calls.len())
                .map_err(|error| fail(error, diagnostics.clone()))?;
        }
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
        emit(
            events,
            TurnEventKind::ToolBatchStarted {
                calls: calls.clone(),
            },
        )
        .await
        .map_err(|error| fail(error, diagnostics.clone()))?;
        let results = match self
            .execute_calls_ordered(
                &prepared.tools,
                &calls,
                &mut diagnostics,
                admission,
                events,
                scope.clone(),
            )
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
        transcript.extend_from_slice(&messages[messages.len() - 2..]);
        diagnostics.tool_results = results;
        for (ordinal, result) in diagnostics.tool_results.iter().cloned().enumerate() {
            emit(events, TurnEventKind::ToolResult { ordinal, result })
                .await
                .map_err(|error| fail(error, diagnostics.clone()))?;
        }
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
                events,
            )
            .await
        {
            Ok(value) => value,
            Err(error) => return Err(fail(error, diagnostics)),
        };
        match response {
            ModelResponse::Final { text } => {
                diagnostics.stage = TurnStage::Completed;
                transcript.push(ChatMessage::text(ChatRole::Assistant, text.clone()));
                emit(events, TurnEventKind::TurnCompleted { text: text.clone() })
                    .await
                    .map_err(|error| fail(error, diagnostics.clone()))?;
                Ok(TurnOutput {
                    text,
                    diagnostics,
                    transcript,
                })
            }
            ModelResponse::ToolCalls { .. } => Err(fail(LlmError::RoundLimit, diagnostics)),
        }
    }

    async fn prepare(
        &self,
        input: &TurnInput,
        scope: Option<eve_llm_api::ContextScope>,
        diagnostics: &mut TurnDiagnostics,
    ) -> Result<PreparedTurn, LlmError> {
        if input.text.trim().is_empty() {
            return Err(LlmError::Context("本轮输入不能为空".into()));
        }
        let assembler = self.context_service()?;
        let context =
            contain_panic(async { assembler.assemble_scoped(input.clone(), scope).await }).await?;
        validate_context(&context)?;
        let mut tools = Vec::with_capacity(self.bindings.len());
        for binding in &self.bindings {
            let service = self.tool_service(binding)?;
            let definition = catch_unwind(AssertUnwindSafe(|| service.definition()))
                .map_err(|_| LlmError::Configuration("工具定义装配发生异常".into()))?;
            definition.validate()?;
            if definition.name != binding.name {
                return Err(LlmError::Configuration(format!(
                    "工具定义名称不匹配: {}",
                    binding.name
                )));
            }
            check_permissions(
                &self.permissions,
                &self
                    .kernel
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
        events: Option<&EventDelivery<'_>>,
    ) -> Result<ModelResponse, LlmError> {
        request.validate()?;
        if let Some(budget) = &self.execution_budget {
            budget.reserve_model_request()?;
        }
        let request_number = diagnostics.provider_requests + 1;
        emit(
            events,
            TurnEventKind::ProviderStarted {
                request: request_number,
            },
        )
        .await?;
        diagnostics.provider_requests += 1;
        let text = TextDelivery {
            events,
            request: request_number,
            text: StdMutex::new(String::new()),
            limit: self.config.max_stream_text_bytes,
        };
        let future = contain_panic(async {
            match self.config.response_mode {
                ResponseMode::Complete => self.provider.complete(request).await,
                ResponseMode::Stream => self.provider.stream(request, &text).await,
            }
        });
        let response = tokio::select! {
            biased;
            _ = tokio::time::sleep(self.config.provider_timeout) => return Err(LlmError::ProviderTimeout),
            _ = wait_closed(events) => return Err(LlmError::Cancelled),
            response = future => response?,
        };
        response.validate()?;
        if self.config.response_mode == ResponseMode::Stream {
            let combined = text
                .text
                .lock()
                .map_err(|_| LlmError::Backend("增量文本锁失效".into()))?;
            match &response {
                ModelResponse::Final { text } if text == &*combined => {}
                ModelResponse::ToolCalls { .. } if combined.is_empty() => {}
                _ => return Err(LlmError::Protocol("流式增量与完整输出不一致".into())),
            }
        }
        emit(
            events,
            TurnEventKind::ResponseCompleted {
                request: request_number,
                response: response.clone(),
            },
        )
        .await?;
        Ok(response)
    }

    fn context_service(&self) -> Result<Arc<dyn ContextAssembler>, LlmError> {
        let entry = self
            .registry
            .get(&self.context.service_id)
            .map_err(backend)?
            .ok_or_else(|| LlmError::Configuration("上下文服务不存在".into()))?;
        ensure_owner_and_active(&self.kernel, &entry.owner, &self.context.expected_owner)?;
        Arc::downcast::<ContextService>(entry.value)
            .map(|service| service.0.clone())
            .map_err(|_| LlmError::Configuration("上下文服务类型不匹配".into()))
    }

    fn tool_service(&self, binding: &ToolBinding) -> Result<Arc<dyn Tool>, LlmError> {
        let entry = self
            .registry
            .get(&binding.service_id)
            .map_err(backend)?
            .ok_or_else(|| LlmError::Configuration(format!("工具服务不存在: {}", binding.name)))?;
        ensure_owner_and_active(&self.kernel, &entry.owner, &binding.expected_owner)?;
        Arc::downcast::<ToolService>(entry.value)
            .map(|service| service.0.clone())
            .map_err(|_| LlmError::Configuration(format!("工具服务类型不匹配: {}", binding.name)))
    }

    async fn execute_calls_ordered(
        &self,
        prepared: &[PreparedTool],
        calls: &[ToolCall],
        diagnostics: &mut TurnDiagnostics,
        admission: Arc<RuntimeAdmissionGuard>,
        events: Option<&EventDelivery<'_>>,
        caller: Option<eve_llm_api::ContextScope>,
    ) -> Result<Vec<ToolResult>, LlmError> {
        let by_name = prepared
            .iter()
            .map(|tool| (tool.binding.name.clone(), tool))
            .collect::<HashMap<_, _>>();
        let semaphore = self.tool_slots.clone();
        let scopes = &self.serial_scopes;
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
                    "未知工具",
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
                        "工具参数校验失败",
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
            let manifest = match self.kernel.plugin_manifest(&binding.expected_owner) {
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
                Some(ToolConcurrency::Serial { scope }) if scope == "eve.default" => {
                    Some(ScopeKey::Default)
                }
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
                prepared_call.ticket = Some(issue_serial_ticket(scopes, scope).await);
            }
        }
        let mut tasks = ToolTaskGuard {
            tasks: Vec::with_capacity(ready.len()),
        };
        for prepared_call in ready {
            // 取消外层 Future 后，停止操作仍需等到已创建的工具任务真正析构。
            let admission = admission.clone();
            let context = ToolExecutionContext::new().with_scope(caller.clone());
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
                    let _admission = admission;
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
        for position in 0..tasks.tasks.len() {
            let result = tokio::select! {
                biased;
                _ = wait_closed(events) => None,
                result = &mut tasks.tasks[position].handle => Some(result),
            };
            if result.is_none() {
                // 取消后等待剩余 Future 析构，使开始计数和副作用诊断稳定。
                for task in &tasks.tasks[position..] {
                    task.cancellation.cancel();
                    task.handle.abort();
                }
                for task in &mut tasks.tasks[position..] {
                    let result = match (&mut task.handle).await {
                        Ok(result) => result,
                        Err(error) => ToolResult::failure(
                            task.call_id.clone(),
                            if error.is_panic() {
                                ToolFailureCode::ExecutionFailed
                            } else {
                                ToolFailureCode::Cancelled
                            },
                            if error.is_panic() {
                                "工具执行异常"
                            } else {
                                "事件消费者已离开，工具等待已取消"
                            },
                        )
                        .expect("validated call id"),
                    };
                    results[task.index] = Some(result);
                }
                diagnostics.started_tools += started.load(Ordering::SeqCst);
                diagnostics.peak_parallelism = diagnostics
                    .peak_parallelism
                    .max(peak.load(Ordering::SeqCst));
                diagnostics.tool_results = results.into_iter().flatten().collect();
                return Err(LlmError::Cancelled);
            }
            let task = &tasks.tasks[position];
            results[task.index] = Some(match result.expect("completed task") {
                Ok(result) => result,
                Err(error) => ToolResult::failure(
                    task.call_id.clone(),
                    ToolFailureCode::ExecutionFailed,
                    if error.is_panic() {
                        "工具执行异常"
                    } else {
                        "tool execution cancelled"
                    },
                )
                .expect("call id validated"),
            });
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
        match self.kernel.plugins() {
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
    if context
        .history
        .iter()
        .any(|message| message.role == ChatRole::System)
        || (ModelRequest {
            messages: context.history.clone(),
            tools: vec![],
        })
        .validate()
        .is_err()
    {
        return Err(LlmError::Context(
            "历史必须是有效用户/助手消息或完整配对的工具调用与结果".into(),
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
        return Err(LlmError::Backend(format!("服务所有者不匹配: {actual}")));
    }
    let statuses = inspector.plugins().map_err(backend)?;
    let status = statuses
        .iter()
        .find(|status| status.info.id == *expected)
        .ok_or_else(|| LlmError::Backend(format!("插件不存在: {expected}")))?;
    if status.state != PluginState::Active {
        return Err(LlmError::Backend(format!("插件未激活: {expected}")));
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
                    profile: "测试用户".into(),
                    memories: vec!["固定记忆".into()],
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
                description: "回显".into(),
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
                    .ok_or_else(|| LlmError::Provider("脚本已耗尽".into()))
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
            kernel,
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
                    text: "完成".into(),
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
        assert_eq!(output.text, "完成");
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
