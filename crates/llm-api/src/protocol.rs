//! 安全的协议故障分类：只识别宿主固定错误，不转述原始模型输出或供应商正文。
use crate::LlmError;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProtocolFailureKind {
    InvalidJson,
    EmptyResponse,
    ToolArguments,
    ToolBatch,
    ReusedToolId,
    StreamContentType,
    StreamIncomplete,
    StreamInvalid,
    StreamMismatch,
    StreamTooLarge,
    TaskIncomplete,
    Other,
}

impl LlmError {
    pub fn protocol_failure(&self) -> Option<ProtocolFailureKind> {
        use ProtocolFailureKind::*;
        let Self::Protocol(reason) = self else {
            return None;
        };
        Some(match reason.as_str() {
            "Chat 响应不是有效 JSON" | "OpenAI 响应不是有效 JSON" => InvalidJson,
            "Chat choices 不能为空"
            | "OpenAI output 不能为空"
            | "Chat 完整回复缺少文本"
            | "final text cannot be empty" => EmptyResponse,
            "Chat 函数参数不是有效 JSON" | "OpenAI 函数参数不是有效 JSON" => {
                ToolArguments
            }
            "Chat 文本或工具调用批次不符合协议"
            | "OpenAI 文本或函数调用批次不符合协议"
            | "Chat 终态与工具批次不匹配"
            | "tool call batch cannot be empty" => ToolBatch,
            "不同工具批次不能复用调用 ID" => ReusedToolId,
            "模型流必须使用 text/event-stream" => StreamContentType,
            "模型流在正常终态前结束" => StreamIncomplete,
            "Chat 流片段无效、终态缺失或标识不一致" | "Responses 流事件缺失、重复或相互矛盾" => {
                StreamInvalid
            }
            "流式增量与完整输出不一致" => StreamMismatch,
            "流式文本超过宿主字节上限" => StreamTooLarge,
            "任务仍标记为未完成；自动续行三次后仍未完成或说明阻塞，已停止本轮，不能宣称完成" => {
                TaskIncomplete
            }
            _ => Other,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn categories_are_fixed_and_unknown_details_never_leave_the_error() {
        for (reason, expected) in [
            (
                "Chat 函数参数不是有效 JSON",
                ProtocolFailureKind::ToolArguments,
            ),
            (
                "Chat 流片段无效、终态缺失或标识不一致",
                ProtocolFailureKind::StreamInvalid,
            ),
            (
                "模型流在正常终态前结束",
                ProtocolFailureKind::StreamIncomplete,
            ),
            ("private response body secret", ProtocolFailureKind::Other),
        ] {
            let kind = LlmError::Protocol(reason.into())
                .protocol_failure()
                .unwrap();
            assert_eq!(kind, expected);
            assert!(!serde_json::to_string(&kind).unwrap().contains("secret"));
        }
        assert_eq!(LlmError::Cancelled.protocol_failure(), None);
    }
}
