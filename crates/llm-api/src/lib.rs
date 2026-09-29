//! 与模型厂商无关的 LLM 组合契约。
//!
//! 该 crate 只定义宿主、Provider、上下文和工具之间传递的数据与 trait。
//! 它不依赖 Tokio，也不包含网络、生命周期或具体模型实现。

use eve_plugin_api::{Permission, PluginId, ServiceId};
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
pub struct ChatMessage {
    pub role: ChatRole,
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<ToolResult>,
}

impl ChatMessage {
    pub fn text(role: ChatRole, text: impl Into<String>) -> Self {
        Self {
            role,
            text: Some(text.into()),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
        }
    }

    pub fn assistant_tool_calls(calls: Vec<ToolCall>) -> Result<Self, LlmError> {
        validate_calls(&calls)?;
        Ok(Self {
            role: ChatRole::Assistant,
            text: None,
            tool_calls: calls,
            tool_results: Vec::new(),
        })
    }

    pub fn tool_results(results: Vec<ToolResult>) -> Result<Self, LlmError> {
        if results.is_empty() {
            return Err(LlmError::Protocol(
                "tool result batch cannot be empty".into(),
            ));
        }
        Ok(Self {
            role: ChatRole::Tool,
            text: None,
            tool_calls: Vec::new(),
            tool_results: results,
        })
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LlmError {
    Configuration(String),
    Context(String),
    Provider(String),
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
            Self::ProviderTimeout => f.write_str("provider timeout"),
            Self::RoundLimit => f.write_str("tool round limit exhausted"),
            Self::Cancelled => f.write_str("turn cancelled"),
            Self::Configuration(s) => write!(f, "configuration error: {s}"),
            Self::Context(s) => write!(f, "context error: {s}"),
            Self::Provider(s) => write!(f, "provider error: {s}"),
            Self::Protocol(s) => write!(f, "protocol error: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported: {s}"),
            Self::Backend(s) => write!(f, "backend error: {s}"),
        }
    }
}
impl std::error::Error for LlmError {}

pub trait LlmProvider: Send + Sync {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse>;
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

pub trait ContextAssembler: Send + Sync {
    fn assemble(&self, input: TurnInput) -> LlmFuture<'_, ContextSnapshot>;
}

pub struct ToolExecutionContext {
    cancelled: Arc<AtomicBool>,
}
impl ToolExecutionContext {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
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
}
