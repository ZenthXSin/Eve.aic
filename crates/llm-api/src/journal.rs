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
    /// 子代理准入计数，不包含在本执行器的消息调用列表里。
    #[serde(default, skip_serializing_if = "zero")]
    pub delegated_tool_calls: usize,
    #[serde(default, skip_serializing_if = "zero")]
    pub delegated_model_requests: usize,
    pub provider_requests: usize,
    pub tool_batches: usize,
    pub remaining: Option<String>,
    pub continuation_checks: usize,
    pub closing: bool,
    /// 先持久化模型最终结果再提交任务结局，重启可以直接了结，不再次请求模型。
    pub result: Option<String>,
}
fn zero(value: &usize) -> bool {
    *value == 0
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
        if !matches!(self.version, 1 | 2)
            || (self.version == 1
                && (self.delegated_tool_calls != 0 || self.delegated_model_requests != 0))
            || self.messages.is_empty()
            || self.scope.session_id.is_empty()
            || self.scope.user_id.is_empty()
            || self.continuation_checks > 3
            || self
                .used_tool_calls
                .checked_add(self.delegated_tool_calls)
                .is_none_or(|n| n > max_tool_calls)
            || self.delegated_model_requests > max_tool_calls.saturating_mul(2).saturating_add(16)
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
        ModelRequest::validate_parts(&self.messages, &[])?;
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
            || serde_json::to_writer(&mut ByteBudget(MAX_CHECKPOINT_BYTES), self).is_err()
        {
            return Err(invalid());
        }
        Ok(())
    }
}

/// 计数 JSON 实际转义后的 UTF-8 字节；超出既有预算即停止，不构造正文副本。
struct ByteBudget(usize);
impl std::io::Write for ByteBudget {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if buffer.len() > self.0 {
            return Err(std::io::Error::other("执行检查点超过字节预算"));
        }
        self.0 -= buffer.len();
        Ok(buffer.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 仅注入可信宿主；模型没有读写检查点的工具。保存失败必须停止，不能继续产生副作用。
pub trait TurnJournal: Send + Sync {
    fn load(&self) -> Result<Option<TurnCheckpoint>, LlmError>;
    fn save(&self, checkpoint: &TurnCheckpoint) -> Result<(), LlmError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChatRole, ToolResult};
    use serde_json::json;

    fn checkpoint() -> TurnCheckpoint {
        TurnCheckpoint {
            version: 1,
            scope: ContextScope {
                session_id: "s".into(),
                user_id: "u".into(),
            },
            messages: vec![ChatMessage::text(ChatRole::User, "x")],
            pending_calls: vec![],
            used_tool_calls: 0,
            delegated_tool_calls: 0,
            delegated_model_requests: 0,
            provider_requests: 1,
            tool_batches: 0,
            remaining: None,
            continuation_checks: 0,
            closing: false,
            result: None,
        }
    }
    #[test]
    fn checkpoint_accepts_exact_json_budget_and_rejects_the_next_byte() {
        let mut checkpoint = checkpoint();
        let overhead = serde_json::to_vec(&checkpoint).unwrap().len() - 1;
        checkpoint.messages[0].text = Some("x".repeat(MAX_CHECKPOINT_BYTES - overhead));
        assert_eq!(
            serde_json::to_vec(&checkpoint).unwrap().len(),
            MAX_CHECKPOINT_BYTES
        );
        checkpoint.validate(0).unwrap();
        checkpoint.messages[0].text.as_mut().unwrap().push('x');
        assert_eq!(
            serde_json::to_vec(&checkpoint).unwrap().len(),
            MAX_CHECKPOINT_BYTES + 1
        );
        assert!(checkpoint.validate(0).is_err());
    }
    #[test]
    fn checkpoint_counts_escaped_bytes_rather_than_raw_text_length() {
        let mut checkpoint = checkpoint();
        let text = "\"\\\n猫".repeat(240_000);
        assert!(text.len() < MAX_CHECKPOINT_BYTES);
        checkpoint.messages[0].text = Some(text);
        assert!(serde_json::to_vec(&checkpoint).unwrap().len() > MAX_CHECKPOINT_BYTES);
        assert!(checkpoint.validate(0).is_err());
    }
    #[test]
    fn borrowed_protocol_validation_keeps_pending_calls_and_rejects_mismatched_receipts() {
        let mut checkpoint = checkpoint();
        let call = ToolCall {
            id: "call".into(),
            name: "echo".into(),
            arguments: json!({"text":"原始参数"}),
        };
        checkpoint.pending_calls.push(call.clone());
        checkpoint.used_tool_calls = 1;
        checkpoint.tool_batches = 1;
        checkpoint.validate(1).unwrap();
        assert!(!checkpoint.resumable());
        assert!(checkpoint.validate(0).is_err());
        checkpoint.pending_calls.clear();
        checkpoint
            .messages
            .push(ChatMessage::assistant_tool_calls(vec![call]).unwrap());
        checkpoint.messages.push(
            ChatMessage::tool_results(vec![
                ToolResult::success("foreign", json!("不能错配")).unwrap(),
            ])
            .unwrap(),
        );
        assert!(matches!(checkpoint.validate(1), Err(LlmError::Protocol(_))));
        checkpoint.messages.last_mut().unwrap().tool_results[0].call_id = "call".into();
        checkpoint.validate(1).unwrap();
        assert!(checkpoint.resumable());
    }
}
