//! 宿主持有的执行检查点；恢复消息不会重放工具。持久化实现由组合层注入。
use crate::{ChatMessage, ContextScope, LlmError, ModelRequest, ToolCall};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const MAX_CHECKPOINT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnCheckpoint {
    pub version: u32,
    pub scope: ContextScope,
    /// 已配对的完整消息，只供恢复本任务，不写入主会话历史。
    pub messages: Vec<ChatMessage>,
    /// 先记下待执行批次，再开始工具；非空时只能人工核对，不能自动重放。
    pub pending_calls: Vec<ToolCall>,
    pub used_tool_calls: usize,
    pub provider_requests: usize,
    pub tool_batches: usize,
    pub remaining: Option<String>,
    pub continuation_checks: usize,
    pub closing: bool,
    /// 先持久化模型最终结果再提交任务结局，重启可以直接了结，不再次请求模型。
    pub result: Option<String>,
}
impl std::fmt::Debug for TurnCheckpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnCheckpoint")
            .field("used_tool_calls", &self.used_tool_calls)
            .field("pending_calls", &self.pending_calls.len())
            .field("provider_requests", &self.provider_requests)
            .finish_non_exhaustive()
    }
}
impl TurnCheckpoint {
    pub fn resumable(&self) -> bool {
        self.pending_calls.is_empty()
            && !self.messages.iter().flat_map(|m| &m.tool_results).any(|r| {
                matches!(
                    r.output,
                    crate::ToolOutput::Failure {
                        code: crate::ToolFailureCode::TimedOut
                            | crate::ToolFailureCode::Cancelled
                            | crate::ToolFailureCode::ExecutionFailed,
                        ..
                    }
                )
            })
    }
    pub fn validate(&self, max_tool_calls: usize) -> Result<(), LlmError> {
        let invalid = || LlmError::Context("执行检查点无效；保留原记录，不自动重放".into());
        if self.version != 1
            || self.messages.is_empty()
            || self.scope.session_id.is_empty()
            || self.scope.user_id.is_empty()
            || self.continuation_checks > 3
            || self.used_tool_calls > max_tool_calls
            || self.provider_requests > max_tool_calls.saturating_add(16)
            || self.tool_batches > self.used_tool_calls
            || self
                .remaining
                .as_ref()
                .is_some_and(|s| s.trim().is_empty() || s.len() > 2048)
            || self.result.as_ref().is_some_and(|s| s.trim().is_empty())
            || (self.result.is_some()
                && (!self.resumable() || self.remaining.is_some() && !self.closing))
        {
            return Err(invalid());
        }
        ModelRequest {
            messages: self.messages.clone(),
            tools: vec![],
        }
        .validate()?;
        let mut ids = HashSet::new();
        let mut count = 0;
        for call in self
            .messages
            .iter()
            .flat_map(|m| &m.tool_calls)
            .chain(&self.pending_calls)
        {
            call.validate()?;
            if !ids.insert(&call.id) {
                return Err(invalid());
            }
            count += 1;
        }
        if count != self.used_tool_calls
            || self.tool_batches
                != self
                    .messages
                    .iter()
                    .filter(|m| !m.tool_calls.is_empty())
                    .count()
                    + usize::from(!self.pending_calls.is_empty())
            || self.provider_requests < self.tool_batches
            || (self.result.is_some() && self.provider_requests == 0)
            || serde_json::to_vec(self).map_err(|_| invalid())?.len() > MAX_CHECKPOINT_BYTES
        {
            return Err(invalid());
        }
        Ok(())
    }
}

/// 仅注入可信宿主；模型没有读写检查点的工具。保存失败必须停止，不能继续产生副作用。
pub trait TurnJournal: Send + Sync {
    fn load(&self) -> Result<Option<TurnCheckpoint>, LlmError>;
    fn save(&self, checkpoint: &TurnCheckpoint) -> Result<(), LlmError>;
}
