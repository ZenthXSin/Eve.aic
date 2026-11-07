//! 与模型厂商无关的 LLM 组合契约。
//!
//! 该 crate 只定义宿主、Provider、上下文和工具之间传递的数据与 trait。
//! 它不依赖 Tokio，也不包含网络、生命周期或具体模型实现。

mod prompt;
pub use prompt::{
    SystemPromptMetadata, SystemPromptOrigin, SystemPromptSnapshot, SystemPromptSource,
    SystemPromptSourceHandle,
};

use eve_plugin_api::{Permission, PluginId, ServiceId};
mod budget;
mod credentials;
mod deferred_reply;
pub use credentials::{CredentialError, CredentialLease, CredentialPurpose, ModelCredentials};
pub use deferred_reply::DeferredReplyPolicy;
mod failure;
mod history;
mod image;
mod journal;
mod protocol;
mod request_fingerprint;
mod session;
mod steering;
mod usage;
pub use budget::{BudgetUsage, ExecutionLimits, SharedStepUsage, SharedToolSteps, TurnBudget};
pub use failure::{ModelHealthObserver, ModelOutcome, ProviderFailure, ProviderFailureKind};
pub use history::{
    CONTEXT_COMPACTION_TIMEOUT, ContextCompactionPolicy, HistoryContextPlan, HistoryContextPlanner,
    HistoryDigest, HistoryDigests, HistoryWindow, HistoryWindowResolver, MAX_HISTORY_DIGEST_BYTES,
    MAX_HISTORY_WORKING_SET_BYTES, MAX_REVIEW_USER_MESSAGES_BYTES, history_turns,
};
pub use image::{
    ImageInput, MAX_IMAGE_BYTES, MAX_TURN_IMAGES, TurnImageSource, TurnImages, sniff_image,
};
pub use journal::{MAX_CHECKPOINT_BYTES, TurnCheckpoint, TurnJournal};
pub use protocol::ProtocolFailureKind;
pub use request_fingerprint::ModelRequestFingerprint;
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub use session::ModelSession;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
pub use steering::{
    InputReplyDelivery, InputReplyHandoff, SteeringBatch, SteeringUpdate, TurnSteering,
    TurnSteeringSource,
};
pub use usage::{ModelUsage, ModelUsageObserver, PrefixReuse};

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
    /// 宿主确认未对外发送的普通助手文本；原文保存，模型回放时带明确标识。
    /// 缺省保留旧会话语义，不能推断旧版 final 未发送。
    #[serde(default, skip_serializing_if = "is_false")]
    pub internal_only: bool,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<ToolResult>,
    /// 本轮用户消息附带的图像；从不序列化，持久历史中始终为空。
    #[serde(skip)]
    pub images: Vec<ImageInput>,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl ChatMessage {
    pub fn text(role: ChatRole, text: impl Into<String>) -> Self {
        Self {
            role,
            text: Some(text.into()),
            internal_only: false,
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            images: Vec::new(),
        }
    }

    pub fn internal_text(text: impl Into<String>) -> Self {
        Self {
            internal_only: true,
            ..Self::text(ChatRole::Assistant, text)
        }
    }

    /// 仅用于模型请求。存储、原始回执与对外发送继续使用未改写的 text。
    pub fn text_for_model(&self) -> Option<std::borrow::Cow<'_, str>> {
        self.text.as_deref().map(|text| {
            if self.internal_only {
                std::borrow::Cow::Owned(format!(
                    "【宿主内部记录，未发送】以下原文只是内部结束记录，不是对方收到的话，不能据此判断已经回复或交付：\n{}",
                    serde_json::json!({"kind":"eve.internal_record","text":text})
                ))
            } else {
                std::borrow::Cow::Borrowed(text)
            }
        })
    }

    pub fn assistant_tool_calls(calls: Vec<ToolCall>) -> Result<Self, LlmError> {
        validate_calls(&calls)?;
        Ok(Self {
            role: ChatRole::Assistant,
            text: None,
            internal_only: false,
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
            internal_only: false,
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
        if self.internal_only
            && (self.role != ChatRole::Assistant
                || !self.tool_calls.is_empty()
                || !self.tool_results.is_empty())
        {
            return Err(LlmError::Protocol(
                "internal record must be plain assistant text".into(),
            ));
        }
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
    /// 宿主显式绑定，不能从提示词推断。None 仅用于本地协议转换/估算或旧的非会话调用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<ModelSession>,
}

impl ModelRequest {
    pub fn validate(&self) -> Result<(), LlmError> {
        if let Some(session) = &self.session {
            session.validate()?;
        }
        Self::validate_parts(&self.messages, &self.tools)
    }
    /// 按同一协议直接核验借用的消息和工具定义，不复制正文或工具回执。
    pub fn validate_parts(
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
    ) -> Result<(), LlmError> {
        let mut names = std::collections::HashSet::new();
        for tool in tools {
            tool.validate()?;
            if !names.insert(tool.name.clone()) {
                return Err(LlmError::Protocol(format!(
                    "duplicate tool name: {}",
                    tool.name
                )));
            }
        }

        for (index, message) in messages.iter().enumerate() {
            message.validate()?;

            match &message.role {
                ChatRole::Assistant if !message.tool_calls.is_empty() => {
                    let Some(next) = messages.get(index + 1) else {
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
                        .and_then(|previous_index| messages.get(previous_index))
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

/// 对话与后台表达共用的行为约定；正文发送只能来自显式模型决定。
pub const DIALOGUE_EXPRESSION_PROMPT: &str = include_str!("dialogue_prompt.md");

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
    /// 压缩阶段的模型失败；只保留固定类别与 HTTP 状态，不能误归因为主对话请求失败。
    ContextCompactionFailed(ProviderFailure),
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
            Self::ContextCompactionFailed(failure) => match failure.http_status {
                Some(status) => write!(
                    f,
                    "context compaction provider failure: {:?} (HTTP {status})",
                    failure.kind
                ),
                None => write!(f, "context compaction provider failure: {:?}", failure.kind),
            },
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
            Self::ProviderFailed(failure) | Self::ContextCompactionFailed(failure) => {
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
            Self::ProviderFailed(failure) | Self::ContextCompactionFailed(failure) => {
                Some(*failure)
            }
            _ => ProviderFailure::http(self.provider_http_status()?),
        }
    }
}

impl std::error::Error for LlmError {}

pub trait LlmProvider: Send + Sync {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse>;

    /// Runtime 在发送 complete/stream 前只读计算；不能访问网络、执行模型或修改请求状态。
    /// 必须覆盖实际正文、目的地与固定配置，不含 x-client-request-id 或凭据。
    /// 只有两次 Some 指纹相同才可复用待恢复请求 ID；None 表示无法证明，宿主分配新序号。
    /// 修改正文的包装器必须按同一转换计算或保留 None，不能直接透传旧指纹。
    fn request_fingerprint(
        &self,
        _request: &ModelRequest,
        _mode: ResponseMode,
    ) -> Result<Option<ModelRequestFingerprint>, LlmError> {
        Ok(None)
    }

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
    /// 模型通过发送工具生成的中途消息，不是最终交付。
    AssistantMessage {
        text: String,
    },
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

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextSnapshot {
    pub revision: String,
    pub profile: String,
    pub memories: Vec<String>,
    pub history: Vec<ChatMessage>,
    /// 在一次进程内几乎不变的宿主资料（宿主事实、授权说明等）。放在历史之前、紧跟工具清单，
    /// 与历史一起成为可缓存的前缀；它变化时，该会话的前缀缓存会整体失效一次。
    /// 每轮变化的资料必须放在 `memories`。
    pub pinned: Vec<String>,
}

/// 由会话宿主传入的可信作用域；不从用户文字或模型输出解析。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextScope {
    pub session_id: String,
    pub user_id: String,
}

/// 可信原文窗口与授权证据可用性分开记录，不能把缺口重建成加工正文或空的完整证据。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReviewUserMessageWindow {
    Complete(Vec<String>),
    /// 来源已核对持久库绑定、完整身份和指定消息；只有原文缺失或已登记的旧历史缺口。
    /// 不表示任何新权限。损坏、身份不符、未知锚点或来源不可用必须返回错误。
    MissingOriginals,
}

/// 通道验证和持久化后的独立用户原文来源。引用、附件提示、模型上下文不是授权证据。
/// None 表示此来源不处理该通道；Some(empty) 表示该范围没有可用的真实用户原话。
pub trait ReviewUserMessageSource: Send + Sync {
    fn read(&self, scope: &ContextScope) -> Result<Option<Vec<String>>, LlmError>;
    /// 可信宿主冻结的消息锚点；实现必须精确读取到该消息，不能换成最新收到的正文。
    fn read_at(
        &self,
        _scope: &ContextScope,
        _anchor: &str,
    ) -> Result<Option<Vec<String>>, LlmError> {
        Err(LlmError::Context(
            "原话来源不支持可信消息锚点；未回退到可变最新消息".into(),
        ))
    }
    /// 前台执行独立核对原文窗口；明确缺口不关闭普通工具，身份变更仍须完整原话。
    /// 默认沿用严格原文读取；只有已验证上述缺口类别的来源才能返回 MissingOriginals。
    fn read_window_at(
        &self,
        scope: &ContextScope,
        anchor: &str,
    ) -> Result<Option<ReviewUserMessageWindow>, LlmError> {
        self.read_at(scope, anchor)
            .map(|messages| messages.map(ReviewUserMessageWindow::Complete))
    }
    /// 只核对最新可信入站消息是否仍为已消费的锚点，不读取或提前消费新授权正文。
    fn is_current(&self, _scope: &ContextScope, _anchor: &str) -> Result<bool, LlmError> {
        Err(LlmError::Context("原话来源不支持审核新鲜度检查".into()))
    }
    /// 仅可信来源确认的旧格式原话缺口可继续无工具聊天；损坏、身份错误等不得放行。
    /// 宿主在整个当前轮次移除需要完整审核原话的工具；仅保留交流工具和
    /// ToolScopePolicy 明确允许的不依赖历史授权的只读查询，下一轮重新核对。
    fn allows_chat_without_tools_at(
        &self,
        _scope: &ContextScope,
        _anchor: &str,
    ) -> Result<bool, LlmError> {
        Ok(false)
    }
}

/// 宿主绑定的审核有效性检查；模型参数不能构造或替换此守卫。
/// 新消息使旧审核失效，但不会自动授权新动作，也不改变冻结的委派原话。
pub trait ReviewEvidenceGuard: Send + Sync {
    fn check(&self) -> Result<(), ToolExecutionError>;
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

    /// 宿主模型会话与输入正文独立传递。组合装配器必须继续向下传递，
    /// 内部相关性/召回模型由此派生独立用途；旧叶子实现可以不使用它。
    fn assemble_with_model_session(
        &self,
        input: TurnInput,
        scope: Option<ContextScope>,
        _session: Option<ModelSession>,
    ) -> LlmFuture<'_, ContextSnapshot> {
        self.assemble_scoped(input, scope)
    }
}

#[derive(Clone)]
pub struct ToolExecutionContext {
    cancelled: Arc<AtomicBool>,
    scope: Option<ContextScope>,
    turn_id: Option<u64>,
    call_id: Option<String>,
    background_task_id: Option<String>,
    model_session: Option<ModelSession>,
    continuation: TaskContinuation,
    conversation: Option<Arc<Vec<ChatMessage>>>,
    review_user_messages: Option<Arc<Vec<String>>>,
    review_evidence_guard: Option<Arc<dyn ReviewEvidenceGuard>>,
    shared_steps: Option<SharedToolSteps>,
    deadline: Option<std::time::Instant>,
    delegated: bool,
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
            call_id: None,
            background_task_id: None,
            model_session: None,
            continuation: TaskContinuation::default(),
            conversation: None,
            review_user_messages: None,
            review_evidence_guard: None,
            shared_steps: None,
            deadline: None,
            delegated: false,
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
    /// 执行器绑定的当前工具调用编号，用于关联审计和真实回执，不能由工具参数覆盖。
    pub fn with_call_id(mut self, call_id: String) -> Self {
        self.call_id = Some(call_id);
        self
    }
    pub fn call_id(&self) -> Option<&str> {
        self.call_id.as_deref()
    }
    /// 当前执行器已经认领的持久后台任务；普通对话/子代理不伪造此标识。
    pub fn with_background_task_id(mut self, id: String) -> Self {
        self.background_task_id = Some(id);
        self
    }
    pub fn background_task_id(&self) -> Option<&str> {
        self.background_task_id.as_deref()
    }
    /// 可信运行宿主绑定的模型会话；工具内模型用途应 fork 独立角色，不能修改父会话。
    pub fn with_model_session(mut self, session: ModelSession) -> Self {
        self.model_session = Some(session);
        self
    }
    pub fn model_session(&self) -> Option<&ModelSession> {
        self.model_session.as_ref()
    }
    /// 工具内的独立模型用途。长期会话按可信 scope/role 隔离，当前工具轮次
    /// 取自宿主绑定的父轮次与调用 ID，不读取工具参数或用户正文。
    pub fn model_session_for(&self, role: &str) -> Result<Option<ModelSession>, LlmError> {
        let session = match (&self.scope, &self.model_session) {
            (scope, Some(parent)) => {
                parent.validate_scope(scope.as_ref())?;
                Some(parent.fork(role, None)?)
            }
            (Some(scope), None) => Some(ModelSession::scoped(role, scope)?),
            (None, None) => None,
        };
        session
            .map(|session| {
                if let Some(turn) = self.model_session.as_ref().and_then(ModelSession::turn_id) {
                    session.with_turn(&format!("{turn}:{}", self.call_id.as_deref().unwrap_or("")))
                } else if let Some(turn) = self.turn_id {
                    session.with_turn(&format!("{turn}:{}", self.call_id.as_deref().unwrap_or("")))
                } else {
                    Ok(session)
                }
            })
            .transpose()
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
    /// 宿主从压缩前历史采集的最近六条真实用户正文；不能用摘要、工具结果或模型自报授权替代。
    pub fn with_review_user_messages(mut self, messages: Arc<Vec<String>>) -> Self {
        self.review_user_messages = Some(messages);
        self
    }
    pub fn review_user_messages(&self) -> Option<&[String]> {
        self.review_user_messages.as_deref().map(Vec::as_slice)
    }
    pub fn with_review_evidence_guard(mut self, guard: Arc<dyn ReviewEvidenceGuard>) -> Self {
        self.review_evidence_guard = Some(guard);
        self
    }
    /// 在送审及副作用提交前核对；没有通道锚点的本地或冻结委派上下文不增加检查。
    pub fn check_review_evidence(&self) -> Result<(), ToolExecutionError> {
        match &self.review_evidence_guard {
            Some(guard) => guard.check(),
            None => Ok(()),
        }
    }
    pub fn with_shared_steps(mut self, steps: SharedToolSteps) -> Self {
        self.shared_steps = Some(steps);
        self
    }
    /// 由宿主标记的委托执行（后台任务、子代理）：没有人在场，用户角色的消息是派发时由模型写的
    /// 任务说明，不是用户原话。模型参数不能设置它。
    pub fn with_delegated(mut self, delegated: bool) -> Self {
        self.delegated = delegated;
        self
    }
    pub fn is_delegated(&self) -> bool {
        self.delegated
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
    /// 已确认未产生业务副作用的拒绝；模型可以依据回执纠正输入，不能当作成功。
    Rejected(String),
    Failed(String),
    Cancelled(String),
}
impl fmt::Display for ToolExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(s) => write!(f, "{s}"),
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
    fn borrowed_validation_preserves_complete_pairing_and_definition_checks() {
        let mut messages = vec![
            ChatMessage::text(ChatRole::User, "完整原话".repeat(10_000)),
            ChatMessage::assistant_tool_calls(vec![call("first"), call("second")]).unwrap(),
            ChatMessage::tool_results(vec![
                ToolResult::success("first", json!(1)).unwrap(),
                ToolResult::success("second", json!(2)).unwrap(),
            ])
            .unwrap(),
        ];
        assert!(ModelRequest::validate_parts(&messages, &[]).is_ok());
        messages[2].tool_results.swap(0, 1);
        assert!(matches!(
            ModelRequest::validate_parts(&messages, &[]),
            Err(LlmError::Protocol(_))
        ));
        assert!(ModelRequest::validate_parts(&messages[..2], &[]).is_err());
        let tool = ToolDefinition {
            name: "demo".into(),
            description: String::new(),
            argument_schema: json!({"type":"object"}),
            required_permissions: vec![],
            concurrency: None,
        };
        assert!(ModelRequest::validate_parts(&[], &[tool.clone(), tool]).is_err());
        assert!(ModelRequest::validate_parts(&[], &[]).is_ok());
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
            session: None,
            messages,
            tools: vec![],
        }
    }

    #[test]
    fn internal_record_preserves_raw_text_and_legacy_serialization() {
        let legacy = ChatMessage::text(ChatRole::Assistant, "旧版真实回复");
        let encoded = serde_json::to_value(&legacy).unwrap();
        assert!(encoded.get("internal_only").is_none());
        assert_eq!(
            serde_json::from_value::<ChatMessage>(encoded).unwrap(),
            legacy
        );
        assert_eq!(legacy.text_for_model().as_deref(), Some("旧版真实回复"));
        let internal = ChatMessage::internal_text("我已经告诉你了\n但其实没有发送");
        assert!(request(vec![internal.clone()]).validate().is_ok());
        let stored = serde_json::to_value(&internal).unwrap();
        assert_eq!(stored["text"], "我已经告诉你了\n但其实没有发送");
        assert_eq!(stored["internal_only"], true);
        let restored: ChatMessage = serde_json::from_value(stored).unwrap();
        assert_eq!(restored, internal);
        assert!(
            restored
                .text_for_model()
                .unwrap()
                .starts_with("【宿主内部记录，未发送】")
        );
        let mut invalid = internal;
        invalid.role = ChatRole::User;
        assert!(request(vec![invalid]).validate().is_err());
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
            internal_only: false,
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
                internal_only: false,
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
            session: None,
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
    /// 个别工具可能只存在于某个真实通道；默认保留旧的整组策略。
    fn tool_allowed(&self, _scope: &ContextScope, _name: &str) -> bool {
        true
    }
    /// 仅在可信通道确认旧格式历史原话缺口时使用。不能放开需要完整授权证据、
    /// 任意执行或写入的工具；当前消息缺失、身份错误和存储异常不进入此路径。
    /// 该策略只收窄已装配目录，不能由模型参数授予；旧实现默认不开放额外工具。
    fn tool_allowed_without_review(&self, _scope: &ContextScope, _name: &str) -> bool {
        false
    }
}
