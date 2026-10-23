//! 与模型厂商无关的 LLM 组合契约。
//!
//! 该 crate 只定义宿主、Provider、上下文和工具之间传递的数据与 trait。
//! 它不依赖 Tokio，也不包含网络、生命周期或具体模型实现。

mod prompt;
pub use prompt::{
    SystemPromptMetadata, SystemPromptOrigin, SystemPromptSnapshot, SystemPromptSource,
};

use eve_plugin_api::{Permission, PluginId, ServiceId};
mod budget;
mod failure;
mod history;
mod image;
mod journal;
pub use budget::{BudgetUsage, ExecutionLimits, SharedStepUsage, SharedToolSteps, TurnBudget};
pub use failure::{ModelHealthObserver, ModelOutcome, ProviderFailure, ProviderFailureKind};
pub use history::{
    CONTEXT_COMPACTION_TIMEOUT, ContextCompactionPolicy, HistoryDigest, HistoryDigests,
    HistoryWindow, HistoryWindowResolver, MAX_HISTORY_DIGEST_BYTES, history_turns,
};
pub use image::{
    ImageInput, MAX_IMAGE_BYTES, MAX_TURN_IMAGES, TurnImageSource, TurnImages, sniff_image,
};
pub use journal::{MAX_CHECKPOINT_BYTES, TurnCheckpoint, TurnJournal};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub type LlmFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, LlmError>> + Send + 'a>>;
pub type ToolFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Value, ToolExecutionError>> + Send + 'a>>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<ToolResult>,
    /// 本轮用户消息附带的图像；从不序列化，持久历史中始终为空。
    #[serde(skip)]
    pub images: Vec<ImageInput>,
}

impl ChatMessage {
    pub fn text(role: ChatRole, text: impl Into<String>) -> Self {
        Self {
            role,
            text: Some(text.into()),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            images: Vec::new(),
        }
    }

    pub fn assistant_tool_calls(calls: Vec<ToolCall>) -> Result<Self, LlmError> {
        validate_calls(&calls)?;
        Ok(Self {
            role: ChatRole::Assistant,
            text: None,
            tool_calls: calls,
            tool_results: Vec::new(),
            images: Vec::new(),
        })
    }

    pub fn tool_results(results: Vec<ToolResult>) -> Result<Self, LlmError> {
        if results.is_empty() {
            return Err(LlmError::Protocol(
                "tool result batch cannot be empty".into(),
            ));
        }
        validate_result_ids(&results)?;
        Ok(Self {
            role: ChatRole::Tool,
            text: None,
            tool_calls: Vec::new(),
            tool_results: results,
            images: Vec::new(),
        })
    }

    /// 给用户消息附上本轮图像；图像只在内存中随请求传递。
    pub fn with_images(mut self, images: Vec<ImageInput>) -> Self {
        self.images = images;
        self
    }

    fn validate(&self) -> Result<(), LlmError> {
        if self.images.len() > MAX_TURN_IMAGES
            || (!self.images.is_empty() && self.role != ChatRole::User)
        {
            return Err(LlmError::Protocol(
                "only a user message can carry images, at most MAX_TURN_IMAGES".into(),
            ));
        }
        match &self.role {
            ChatRole::System | ChatRole::User => {
                if !has_non_empty_text(self.text.as_deref()) {
                    return Err(LlmError::Protocol(format!(
                        "{} message text cannot be empty",
                        role_name(&self.role)
                    )));
                }
                if !self.tool_calls.is_empty() || !self.tool_results.is_empty() {
                    return Err(LlmError::Protocol(format!(
                        "{} message cannot contain tool calls or results",
                        role_name(&self.role)
                    )));
                }
            }
            ChatRole::Assistant => {
                if !self.tool_results.is_empty() {
                    return Err(LlmError::Protocol(
                        "assistant message cannot contain tool results".into(),
                    ));
                }
                if self.tool_calls.is_empty() {
                    if !has_non_empty_text(self.text.as_deref()) {
                        return Err(LlmError::Protocol(
                            "assistant message text cannot be empty".into(),
                        ));
                    }
                } else {
                    if self.text.is_some() {
                        return Err(LlmError::Protocol(
                            "assistant tool call message cannot contain text".into(),
                        ));
                    }
                    validate_calls(&self.tool_calls)?;
                }
            }
            ChatRole::Tool => {
                if self.text.is_some() || !self.tool_calls.is_empty() {
                    return Err(LlmError::Protocol(
                        "tool message cannot contain text or tool calls".into(),
                    ));
                }
                if self.tool_results.is_empty() {
                    return Err(LlmError::Protocol(
                        "tool result batch cannot be empty".into(),
                    ));
                }
                validate_result_ids(&self.tool_results)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDefinition>,
}

impl ModelRequest {
    pub fn validate(&self) -> Result<(), LlmError> {
        let mut names = std::collections::HashSet::new();
        for tool in &self.tools {
            tool.validate()?;
            if !names.insert(tool.name.clone()) {
                return Err(LlmError::Protocol(format!(
                    "duplicate tool name: {}",
                    tool.name
                )));
            }
        }

        for (index, message) in self.messages.iter().enumerate() {
            message.validate()?;

            match &message.role {
                ChatRole::Assistant if !message.tool_calls.is_empty() => {
                    let Some(next) = self.messages.get(index + 1) else {
                        return Err(LlmError::Protocol(
                            "assistant tool call message must be followed by tool results".into(),
                        ));
                    };
                    if next.role != ChatRole::Tool {
                        return Err(LlmError::Protocol(
                            "assistant tool call message must be followed by tool results".into(),
                        ));
                    }
                }
                ChatRole::Tool => {
                    let Some(previous) = index
                        .checked_sub(1)
                        .and_then(|previous_index| self.messages.get(previous_index))
                    else {
                        return Err(LlmError::Protocol(
                            "tool results must follow assistant tool calls".into(),
                        ));
                    };
                    if previous.role != ChatRole::Assistant || previous.tool_calls.is_empty() {
                        return Err(LlmError::Protocol(
                            "tool results must follow assistant tool calls".into(),
                        ));
                    }
                    validate_result_batch(&previous.tool_calls, &message.tool_results)?;
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub argument_schema: Value,
    #[serde(skip)]
    pub required_permissions: Vec<Permission>,
    pub concurrency: Option<ToolConcurrency>,
}

impl ToolDefinition {
    pub fn validate(&self) -> Result<(), LlmError> {
        if self.name.trim().is_empty() {
            return Err(LlmError::Protocol("tool name cannot be empty".into()));
        }
        if !self.argument_schema.is_object() {
            return Err(LlmError::Protocol(format!(
                "tool {} argument schema must be an object",
                self.name
            )));
        }
        if let Some(ToolConcurrency::Serial { scope }) = &self.concurrency
            && scope.trim().is_empty()
        {
            return Err(LlmError::Protocol(format!(
                "tool {} serial scope cannot be empty",
                self.name
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ToolConcurrency {
    ParallelSafe,
    Serial { scope: String },
}

impl ToolConcurrency {
    pub fn serial_scope(&self) -> Option<&str> {
        match self {
            Self::ParallelSafe => None,
            Self::Serial { scope } => Some(scope),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

impl ToolCall {
    pub fn validate(&self) -> Result<(), LlmError> {
        if self.id.trim().is_empty() || self.name.trim().is_empty() {
            return Err(LlmError::Protocol(
                "tool call id and name cannot be empty".into(),
            ));
        }
        if !self.arguments.is_object() {
            return Err(LlmError::Protocol(format!(
                "tool call {} arguments must be an object",
                self.id
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    pub call_id: String,
    pub output: ToolOutput,
}

impl ToolResult {
    pub fn success(call_id: impl Into<String>, value: Value) -> Result<Self, LlmError> {
        let call_id = call_id.into();
        if call_id.trim().is_empty() {
            return Err(LlmError::Protocol(
                "tool result call id cannot be empty".into(),
            ));
        }
        Ok(Self {
            call_id,
            output: ToolOutput::Success(value),
        })
    }
    pub fn failure(
        call_id: impl Into<String>,
        code: ToolFailureCode,
        message: impl Into<String>,
    ) -> Result<Self, LlmError> {
        let call_id = call_id.into();
        if call_id.trim().is_empty() {
            return Err(LlmError::Protocol(
                "tool result call id cannot be empty".into(),
            ));
        }
        Ok(Self {
            call_id,
            output: ToolOutput::Failure {
                code,
                message: message.into(),
            },
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ToolOutput {
    Success(Value),
    Failure {
        code: ToolFailureCode,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ToolFailureCode {
    UnknownTool,
    Unavailable,
    OwnerMismatch,
    Inactive,
    PermissionDenied,
    InvalidArguments,
    ExecutionFailed,
    TimedOut,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ModelResponse {
    Final { text: String },
    ToolCalls { calls: Vec<ToolCall> },
}

impl ModelResponse {
    pub fn validate(&self) -> Result<(), LlmError> {
        match self {
            Self::Final { text } if text.trim().is_empty() => {
                Err(LlmError::Protocol("final text cannot be empty".into()))
            }
            Self::Final { .. } => Ok(()),
            Self::ToolCalls { calls } => validate_calls(calls),
        }
    }
}

fn validate_calls(calls: &[ToolCall]) -> Result<(), LlmError> {
    if calls.is_empty() {
        return Err(LlmError::Protocol("tool call batch cannot be empty".into()));
    }
    let mut ids = std::collections::HashSet::new();
    for call in calls {
        call.validate()?;
        if !ids.insert(call.id.clone()) {
            return Err(LlmError::Protocol(format!(
                "duplicate tool call id: {}",
                call.id
            )));
        }
    }
    Ok(())
}

fn validate_result_ids(results: &[ToolResult]) -> Result<(), LlmError> {
    for result in results {
        if result.call_id.trim().is_empty() {
            return Err(LlmError::Protocol(
                "tool result call id cannot be empty".into(),
            ));
        }
    }
    Ok(())
}

fn validate_result_batch(calls: &[ToolCall], results: &[ToolResult]) -> Result<(), LlmError> {
    if calls.len() != results.len() {
        return Err(LlmError::Protocol(format!(
            "tool result count {} does not match tool call count {}",
            results.len(),
            calls.len()
        )));
    }
    for (call, result) in calls.iter().zip(results) {
        if call.id != result.call_id {
            return Err(LlmError::Protocol(format!(
                "tool result call id {} does not match tool call {}",
                result.call_id, call.id
            )));
        }
    }
    Ok(())
}

fn has_non_empty_text(text: Option<&str>) -> bool {
    text.is_some_and(|text| !text.trim().is_empty())
}

fn role_name(role: &ChatRole) -> &'static str {
    match role {
        ChatRole::System => "system",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
        ChatRole::Tool => "tool",
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LlmError {
    /// 最近一个完整历史轮次无法放入显式窗口；尚未请求模型或执行工具。
    HistoryWindowExceeded,
    Configuration(String),
    Context(String),
    Provider(String),
    /// Provider 给出的有限失败类别与状态码；不含供应商错误正文。
    ProviderFailed(ProviderFailure),
    ProviderTimeout,
    Protocol(String),
    Unsupported(String),
    RoundLimit,
    Backend(String),
    Cancelled,
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HistoryWindowExceeded => f.write_str(
                "最近一轮历史超过 runtime.history.max_bytes；请在面板调整历史窗口，原记录仍保留",
            ),
            Self::ProviderTimeout => f.write_str("provider timeout"),
            Self::RoundLimit => f.write_str("tool round limit exhausted"),
            Self::Cancelled => f.write_str("turn cancelled"),
            Self::Configuration(s) => write!(f, "configuration error: {s}"),
            Self::Context(s) => write!(f, "context error: {s}"),
            Self::Provider(s) => write!(f, "provider error: {s}"),
            Self::ProviderFailed(failure) => match failure.http_status {
                Some(status) => write!(f, "provider failure: {:?} (HTTP {status})", failure.kind),
                None => write!(f, "provider failure: {:?}", failure.kind),
            },
            Self::Protocol(s) => write!(f, "protocol error: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported: {s}"),
            Self::Backend(s) => write!(f, "backend error: {s}"),
        }
    }
}
impl LlmError {
    /// 只提取本地 HTTP 适配器的固定状态码，不把供应商错误正文带入记录或回复。
    pub fn provider_http_status(&self) -> Option<u16> {
        let message = match self {
            Self::ProviderFailed(failure) => {
                return failure
                    .http_status
                    .filter(|status| (100..=599).contains(status));
            }
            Self::Provider(message) => message,
            _ => return None,
        };
        let digits = message.strip_prefix("OpenAI HTTP ")?;
        if digits.len() != 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits
            .parse::<u16>()
            .ok()
            .filter(|status| (100..=599).contains(status))
    }
}

impl LlmError {
    /// Provider 给出的失败类别；旧格式的固定 HTTP 文本按状态码分类，其余返回 None。
    pub fn provider_failure(&self) -> Option<ProviderFailure> {
        match self {
            Self::ProviderFailed(failure) => Some(*failure),
            _ => ProviderFailure::http(self.provider_http_status()?),
        }
    }
}

impl std::error::Error for LlmError {}

pub trait LlmProvider: Send + Sync {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse>;

    /// 请求 JSON 对象文本；实现可启用模型的 JSON 模式。兼容旧 Provider 时仍沿用
    /// complete；调用方必须自行严格检查 JSON、字段及业务约束，不能信任格式承诺。
    fn complete_json(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        self.complete(request)
    }

    /// 增量仅供暂时显示；成功返回值才是经验证的完整请求终态。
    /// 丢弃 Future 停止本地读取；不支持时明确失败，不自动降级。
    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        _sink: &'a dyn ModelTextSink,
    ) -> LlmFuture<'a, ModelResponse> {
        Box::pin(async { Err(LlmError::Unsupported("Provider 不支持流式输出".into())) })
    }
}

/// 宿主为一轮捕获的模型能力；Provider 应持有固定设置，不在工具往返间改变选择。
#[derive(Clone)]
pub struct ModelSelection {
    pub provider: Arc<dyn LlmProvider>,
    pub provider_timeout: std::time::Duration,
}

/// 组合层读取已启动的配置服务并装配 Provider；不在此同步方法中访问网络。
/// Runtime 每轮恰好解析一次，在途轮次只使用返回的固定选择。
pub trait LlmModelResolver: Send + Sync {
    fn resolve(&self) -> Result<ModelSelection, LlmError>;
}

pub trait ModelTextSink: Send + Sync {
    fn text_delta(&self, text: String) -> LlmFuture<'_, ()>;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ResponseMode {
    #[default]
    Complete,
    Stream,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnEvent {
    /// 普通宿主由调用方把接收器绑定一次调用；会话宿主附加持久化轮次 ID。
    pub turn_id: Option<u64>,
    pub kind: TurnEventKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TurnEventKind {
    /// 每轮至多一次；只表示开始压缩，不表示摘要或任务已完成。
    ContextCompactionStarted,
    ProviderStarted {
        request: usize,
    },
    TextDelta {
        request: usize,
        text: String,
    },
    ResponseCompleted {
        request: usize,
        response: ModelResponse,
    },
    ToolBatchStarted {
        calls: Vec<ToolCall>,
    },
    /// 按原调用顺序报告结果；ordinal 从 0 开始，不代表真实完成时间。
    ToolResult {
        ordinal: usize,
        result: ToolResult,
    },
    /// 完整工具批次返回后的有界进展；不代表整轮成功或业务目标完成。
    ToolProgress {
        completed_calls: usize,
        failed_calls: usize,
        tool_names: Vec<String>,
    },
    TurnCompleted {
        text: String,
    },
    SessionSaved,
    Failed {
        error: LlmError,
    },
}

/// 串行 await，无内置队列；桥接通道需使用有界队列，断开返回 Cancelled。
/// 回调不能阻塞线程，也不能重入当前 Kernel 的生命周期操作。
pub trait TurnEventSink: Send + Sync {
    fn emit(&self, event: TurnEvent) -> LlmFuture<'_, ()>;
    /// 通道断开信号；任意完成均视为 Cancelled。桥接通道应实现此方法，
    /// 使宿主在等待网络或工具时也能发现接收端离开。默认永久等待。
    fn closed(&self) -> LlmFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

pub struct DiscardTurnEvents;
impl TurnEventSink for DiscardTurnEvents {
    fn emit(&self, _: TurnEvent) -> LlmFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnInput {
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContextSnapshot {
    pub revision: String,
    pub profile: String,
    pub memories: Vec<String>,
    pub history: Vec<ChatMessage>,
}

/// 由会话宿主传入的可信作用域；不从用户文字或模型输出解析。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextScope {
    pub session_id: String,
    pub user_id: String,
}

pub trait ContextAssembler: Send + Sync {
    fn assemble(&self, input: TurnInput) -> LlmFuture<'_, ContextSnapshot>;

    /// 原有装配器继续使用 assemble；需要隔离用户偏好的实现可覆盖此方法。
    fn assemble_scoped(
        &self,
        input: TurnInput,
        _scope: Option<ContextScope>,
    ) -> LlmFuture<'_, ContextSnapshot> {
        self.assemble(input)
    }
}

pub struct ToolExecutionContext {
    cancelled: Arc<AtomicBool>,
    scope: Option<ContextScope>,
    turn_id: Option<u64>,
    continuation: TaskContinuation,
    conversation: Option<Arc<Vec<ChatMessage>>>,
    shared_steps: Option<SharedToolSteps>,
    deadline: Option<std::time::Instant>,
}
/// 单轮内共享的任务状态。只影响本轮收尾，不授予权限、不持久化或重放工具。
#[derive(Clone, Default)]
pub struct TaskContinuation(Arc<std::sync::Mutex<Option<String>>>);
impl TaskContinuation {
    pub fn set_remaining(&self, remaining: Option<String>) -> Result<(), ToolExecutionError> {
        if remaining
            .as_ref()
            .is_some_and(|s| s.trim().is_empty() || s.len() > 2048)
        {
            return Err(ToolExecutionError::Failed(
                "剩余任务需要 1 至 2048 字节".into(),
            ));
        }
        *self
            .0
            .lock()
            .map_err(|_| ToolExecutionError::Failed("任务状态不可用".into()))? = remaining;
        Ok(())
    }
    pub fn remaining(&self) -> Option<String> {
        self.0.lock().ok().and_then(|s| s.clone())
    }
}
impl ToolExecutionContext {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            scope: None,
            turn_id: None,
            continuation: TaskContinuation::default(),
            conversation: None,
            shared_steps: None,
            deadline: None,
        }
    }
    /// 绑定本轮的可信会话范围；由宿主依据通道身份设置，模型参数不能改变它。
    pub fn with_scope(mut self, scope: Option<ContextScope>) -> Self {
        self.scope = scope;
        self
    }
    /// 本轮的可信会话范围；没有范围的轮次（例如本地终端）为 None。
    pub fn scope(&self) -> Option<&ContextScope> {
        self.scope.as_ref()
    }
    /// 由会话宿主绑定的持久轮次；模型参数不能指定。
    pub fn with_turn_id(mut self, turn_id: Option<u64>) -> Self {
        self.turn_id = turn_id;
        self
    }
    pub fn turn_id(&self) -> Option<u64> {
        self.turn_id
    }
    pub fn with_continuation(mut self, continuation: TaskContinuation) -> Self {
        self.continuation = continuation;
        self
    }
    pub fn continuation(&self) -> &TaskContinuation {
        &self.continuation
    }
    /// 宿主冻结的当前模型上下文（已应用历史窗口/压缩，不包含正在执行的工具批次）。
    /// 仅供显式委派时传递数据；不改变接收者的权限、工具或系统指令。
    pub fn with_conversation(mut self, messages: Arc<Vec<ChatMessage>>) -> Self {
        self.conversation = Some(messages);
        self
    }
    pub fn conversation(&self) -> Option<&[ChatMessage]> {
        self.conversation.as_deref().map(Vec::as_slice)
    }
    pub fn with_shared_steps(mut self, steps: SharedToolSteps) -> Self {
        self.shared_steps = Some(steps);
        self
    }
    pub fn shared_steps(&self) -> Option<&SharedToolSteps> {
        self.shared_steps.as_ref()
    }
    pub fn with_deadline(mut self, deadline: Option<std::time::Instant>) -> Self {
        self.deadline = deadline;
        self
    }
    pub fn deadline(&self) -> Option<std::time::Instant> {
        self.deadline
    }
    pub fn cancellation_handle(&self) -> ToolCancellation {
        ToolCancellation(self.cancelled.clone())
    }
    pub fn is_cancel_requested(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}
impl Default for ToolExecutionContext {
    fn default() -> Self {
        Self::new()
    }
}
#[derive(Clone)]
pub struct ToolCancellation(Arc<AtomicBool>);
impl ToolCancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolValidationError {
    pub message: String,
}
impl fmt::Display for ToolValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ToolValidationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolExecutionError {
    Failed(String),
    Cancelled(String),
}
impl fmt::Display for ToolExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed(s) => write!(f, "{s}"),
            Self::Cancelled(s) => write!(f, "{s}"),
        }
    }
}
impl std::error::Error for ToolExecutionError {}

pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolValidationError>;
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_>;
}

#[derive(Clone)]
pub struct ContextService(pub Arc<dyn ContextAssembler>);
#[derive(Clone)]
pub struct ToolService(pub Arc<dyn Tool>);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolBinding {
    pub name: String,
    pub service_id: ServiceId,
    pub expected_owner: PluginId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "demo".into(),
            arguments: json!({}),
        }
    }

    #[test]
    fn rejects_empty_or_duplicate_call_ids() {
        assert!(
            ModelResponse::ToolCalls { calls: vec![] }
                .validate()
                .is_err()
        );
        assert!(
            ModelResponse::ToolCalls {
                calls: vec![call("x"), call("x")]
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn validates_object_arguments_and_schemas() {
        let mut c = call("x");
        c.arguments = json!("bad");
        assert!(c.validate().is_err());
        let definition = ToolDefinition {
            name: "demo".into(),
            description: String::new(),
            argument_schema: json!([]),
            required_permissions: vec![],
            concurrency: None,
        };
        assert!(definition.validate().is_err());
    }

    #[test]
    fn cancellation_is_shared_with_tool() {
        let context = ToolExecutionContext::new();
        let handle = context.cancellation_handle();
        assert!(!context.is_cancel_requested());
        handle.cancel();
        assert!(context.is_cancel_requested());
    }

    fn request(messages: Vec<ChatMessage>) -> ModelRequest {
        ModelRequest {
            messages,
            tools: vec![],
        }
    }

    #[test]
    fn accepts_matching_assistant_calls_and_tool_results() {
        let calls = vec![call("first"), call("second")];
        let results = vec![
            ToolResult::success("first", json!(1)).unwrap(),
            ToolResult::failure("second", ToolFailureCode::ExecutionFailed, "failed").unwrap(),
        ];
        let assistant = ChatMessage::assistant_tool_calls(calls).unwrap();
        let tool = ChatMessage::tool_results(results).unwrap();

        assert!(
            request(vec![
                ChatMessage::text(ChatRole::User, "run"),
                assistant,
                tool
            ])
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn rejects_mismatched_tool_result_ids() {
        let assistant = ChatMessage::assistant_tool_calls(vec![call("expected")]).unwrap();
        let tool =
            ChatMessage::tool_results(vec![ToolResult::success("actual", json!(null)).unwrap()])
                .unwrap();

        assert!(request(vec![assistant, tool]).validate().is_err());
    }

    #[test]
    fn rejects_tool_result_count_mismatch() {
        let assistant = ChatMessage::assistant_tool_calls(vec![call("one"), call("two")]).unwrap();
        let tool =
            ChatMessage::tool_results(vec![ToolResult::success("one", json!(null)).unwrap()])
                .unwrap();

        assert!(request(vec![assistant, tool]).validate().is_err());
    }

    #[test]
    fn rejects_tool_message_without_preceding_calls() {
        let tool =
            ChatMessage::tool_results(vec![ToolResult::success("orphan", json!(null)).unwrap()])
                .unwrap();

        assert!(request(vec![tool]).validate().is_err());
    }

    #[test]
    fn rejects_assistant_mixing_text_and_tool_calls() {
        let assistant = ChatMessage {
            role: ChatRole::Assistant,
            text: Some("also explain this".into()),
            tool_calls: vec![call("demo")],
            tool_results: vec![],
            images: vec![],
        };

        assert!(request(vec![assistant]).validate().is_err());
    }

    #[test]
    fn rejects_empty_message_text() {
        assert!(
            request(vec![ChatMessage::text(ChatRole::User, "  ")])
                .validate()
                .is_err()
        );
        assert!(
            request(vec![ChatMessage {
                role: ChatRole::Assistant,
                text: None,
                tool_calls: vec![],
                tool_results: vec![],
                images: vec![],
            }])
            .validate()
            .is_err()
        );
    }

    #[test]
    fn rejects_duplicate_tool_names() {
        let definition = |name: &str| ToolDefinition {
            name: name.into(),
            description: String::new(),
            argument_schema: json!({}),
            required_permissions: vec![],
            concurrency: None,
        };

        let request = ModelRequest {
            messages: vec![ChatMessage::text(ChatRole::User, "run")],
            tools: vec![definition("demo"), definition("demo")],
        };
        assert!(request.validate().is_err());
    }
}

#[cfg(test)]
mod http_failure_status_tests {
    use super::LlmError;
    #[test]
    fn only_fixed_http_adapter_codes_are_safe_diagnostics() {
        assert_eq!(
            LlmError::Provider("OpenAI HTTP 402".into()).provider_http_status(),
            Some(402)
        );
        for message in [
            "OpenAI HTTP 402 secret",
            "OpenAI HTTP 099",
            "OpenAI HTTP 600",
            "HTTP 402",
            "secret",
        ] {
            assert_eq!(
                LlmError::Provider(message.into()).provider_http_status(),
                None
            );
        }
    }

    #[test]
    fn typed_failures_carry_bounded_kinds_and_statuses() {
        use super::{ProviderFailure, ProviderFailureKind as Kind};
        for (status, kind) in [
            (402, Kind::PaymentRequired),
            (429, Kind::RateLimited),
            (401, Kind::Unauthorized),
            (403, Kind::Unauthorized),
            (400, Kind::Rejected),
            (413, Kind::Rejected),
            (500, Kind::ServerError),
            (503, Kind::ServerError),
            (307, Kind::UnexpectedStatus),
        ] {
            let error = LlmError::ProviderFailed(ProviderFailure::http(status).unwrap());
            assert_eq!(error.provider_http_status(), Some(status));
            assert_eq!(error.provider_failure().unwrap().kind, kind);
        }
        assert_eq!(ProviderFailure::http(200), None);
        assert_eq!(ProviderFailure::http(600), None);
        // 旧格式的固定文本仍按状态码分类。
        assert_eq!(
            LlmError::Provider("OpenAI HTTP 402".into())
                .provider_failure()
                .unwrap()
                .kind,
            Kind::PaymentRequired
        );
        let truncated = LlmError::ProviderFailed(ProviderFailure::new(Kind::Truncated));
        assert_eq!(truncated.provider_http_status(), None);
        assert_eq!(truncated.to_string(), "provider failure: Truncated");
        assert_eq!(LlmError::Provider("secret".into()).provider_failure(), None);
    }
}

/// 宿主为可信调用范围决定是否装配工具；拒绝时隐藏并禁止执行所有工具。
pub trait ToolScopePolicy: Send + Sync {
    fn tools_allowed(&self, scope: &ContextScope) -> bool;
}
