use codex_api::{ResponseEvent, ResponseStream};
use codex_protocol::{
    ResponseUsageMetadata,
    models::{ContentItem, MessagePhase, ResponseItem},
    protocol::TokenUsage,
};
use eve_llm_api::{
    LlmError, ModelResponse, ModelTextSink, ModelUsage, ProviderFailureKind, ToolCall,
};
use futures::StreamExt;
use std::collections::HashSet;

pub(crate) struct Completion {
    pub response: ModelResponse,
    pub usage: ModelUsage,
}

pub(crate) async fn collect(
    mut stream: ResponseStream,
    sink: Option<&dyn ModelTextSink>,
    max_bytes: usize,
    wire: &crate::transport::WireState,
    guard: &crate::event_guard::EventGuard,
) -> Result<Completion, LlmError> {
    let mut output = Output::default();
    let mut deltas = String::new();
    let mut received_delta = false;
    let mut expected = HashSet::new();
    let mut finished = HashSet::new();
    let mut response_id = None;
    while let Some(event) = stream.next().await {
        if let Some(error) = guard.failure() {
            return Err(error);
        }
        let event =
            event.map_err(|error| wire.failure().unwrap_or_else(|| crate::error::api(error)))?;
        match event {
            ResponseEvent::Created {
                response_id: created,
            } => {
                if response_id.is_some() && response_id != created {
                    return Err(protocol("原响应流出现不一致的响应身份"));
                }
                response_id = created;
            }
            ResponseEvent::OutputItemAdded(item) => {
                supported_item(&item)?;
                if let Some(id) = item.id() {
                    let key =
                        serde_json::to_string(id).map_err(|_| protocol("输出项身份无法读取"))?;
                    if !expected.insert(key) {
                        return Err(protocol("原响应流重复声明输出项"));
                    }
                }
            }
            ResponseEvent::OutputItemDone(item) => {
                if let Some(id) = item.id() {
                    let key =
                        serde_json::to_string(id).map_err(|_| protocol("输出项身份无法读取"))?;
                    if !finished.insert(key) {
                        return Err(protocol("原响应流重复完成输出项"));
                    }
                }
                output.push(item)?;
            }
            ResponseEvent::OutputTextDelta(delta) => {
                if deltas.len().saturating_add(delta.len()) > max_bytes {
                    return Err(crate::error::failed(ProviderFailureKind::ResponseTooLarge));
                }
                received_delta = true;
                deltas.push_str(&delta);
            }
            ResponseEvent::Completed {
                response_id: completed_id,
                token_usage,
                usage_metadata,
                end_turn,
            } => {
                if completed_id.trim().is_empty()
                    || response_id
                        .as_ref()
                        .is_some_and(|created| created != &completed_id)
                    || !expected.is_subset(&finished)
                {
                    return Err(protocol("原响应流缺少匹配的完成输出"));
                }
                if received_delta && deltas != output.all_text {
                    return Err(protocol("原响应流增量与完成文本不一致"));
                }
                let usage = usage(token_usage, usage_metadata)?;
                let response = output.finish(end_turn)?;
                // 原 OutputTextDelta 没有 item_id/phase，无法区分 commentary 和最终正文。
                // Eve 的 sink 必须恰等 Final，工具批次必须为空；只交付完整验证后的最终正文。
                if let (Some(sink), ModelResponse::Final { text }) = (sink, &response) {
                    sink.text_delta(text.clone()).await?;
                }
                return Ok(Completion { response, usage });
            }
            // 原 parser 接收 reasoning 和服务端元数据；它们不能冒充用户回复或工具回执。
            ResponseEvent::ReasoningSummaryDelta { .. }
            | ResponseEvent::ReasoningSummaryDone { .. }
            | ResponseEvent::ReasoningContentDelta { .. }
            | ResponseEvent::ReasoningSummaryPartAdded { .. }
            | ResponseEvent::ToolCallInputDelta { .. }
            | ResponseEvent::RateLimits(_)
            | ResponseEvent::ServerModel(_)
            | ResponseEvent::ModelVerifications(_)
            | ResponseEvent::TurnModerationMetadata(_)
            | ResponseEvent::ServerReasoningIncluded(_)
            | ResponseEvent::ModelsEtag(_)
            | ResponseEvent::SafetyBuffering(_) => {}
        }
    }
    if let Some(error) = guard.failure() {
        return Err(error);
    }
    Err(wire
        .failure()
        .unwrap_or_else(|| crate::error::failed(ProviderFailureKind::Incomplete)))
}

#[derive(Default)]
struct Output {
    calls: Vec<ToolCall>,
    final_text: String,
    all_text: String,
    has_final: bool,
}

impl Output {
    fn push(&mut self, item: ResponseItem) -> Result<(), LlmError> {
        supported_item(&item)?;
        match item {
            ResponseItem::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            } => {
                crate::mapping::function_name(&name)?;
                let arguments = crate::strict_json::from_slice(arguments.as_bytes())
                    .map_err(|_| protocol("函数工具参数不是无重复键的 JSON 对象"))?;
                self.calls.push(ToolCall {
                    id: call_id,
                    name,
                    arguments,
                });
            }
            ResponseItem::Message {
                role,
                content,
                phase,
                ..
            } => {
                if role != "assistant" {
                    return Err(protocol("模型输出不是 assistant 消息"));
                }
                let mut text = String::new();
                for part in content {
                    match part {
                        ContentItem::OutputText { text: part } => text.push_str(&part),
                        _ => return Err(unsupported("Eve 不能提交非文本消息内容")),
                    }
                }
                self.all_text.push_str(&text);
                if matches!(phase, None | Some(MessagePhase::FinalAnswer)) {
                    self.has_final = true;
                    self.final_text.push_str(&text);
                }
            }
            ResponseItem::Reasoning { .. } => {}
            _ => unreachable!("supported_item rejects other native capabilities"),
        }
        Ok(())
    }

    fn finish(self, end_turn: Option<bool>) -> Result<ModelResponse, LlmError> {
        let response = if self.calls.is_empty() {
            if !self.has_final || end_turn == Some(false) {
                return Err(crate::error::failed(ProviderFailureKind::Incomplete));
            }
            ModelResponse::Final {
                text: self.final_text,
            }
        } else {
            if self.has_final {
                return Err(unsupported(
                    "Eve 的单次回复契约不能同时保留最终文本和工具批次",
                ));
            }
            ModelResponse::ToolCalls { calls: self.calls }
        };
        response
            .validate()
            .map_err(|_| protocol("完成文本或工具批次不符合 Eve 协议"))?;
        Ok(response)
    }
}

fn supported_item(item: &ResponseItem) -> Result<(), LlmError> {
    match item {
        ResponseItem::Message {
            internal_chat_message_metadata_passthrough: None,
            ..
        }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::FunctionCall {
            namespace: None,
            encrypted_function_args: None,
            internal_chat_message_metadata_passthrough: None,
            ..
        } => Ok(()),
        _ => Err(unsupported(
            "原 Codex 返回了未装配到 Eve 公共契约的输出能力",
        )),
    }
}

fn usage(
    tokens: Option<TokenUsage>,
    metadata: Option<ResponseUsageMetadata>,
) -> Result<ModelUsage, LlmError> {
    if let Some(raw) = metadata.and_then(|metadata| metadata.metadata) {
        // 原 parser 的 TokenUsage 会把缺失 cached_tokens 补零。这里保留原始上游字段的有无。
        let read = |value: Option<&serde_json::Value>| match value {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(value) => value
                .as_u64()
                .map(Some)
                .ok_or_else(|| protocol("模型用量必须是非负整数")),
        };
        let input_tokens = read(raw.get("input_tokens"))?;
        let cached_input_tokens = read(
            raw.get("input_tokens_details")
                .and_then(|details| details.get("cached_tokens")),
        )?;
        let output_tokens = read(raw.get("output_tokens"))?;
        if let (Some(input), Some(cached)) = (input_tokens, cached_input_tokens)
            && cached > input
        {
            return Err(protocol("缓存命中 token 超过输入 token"));
        }
        return Ok(ModelUsage {
            input_tokens,
            cached_input_tokens,
            output_tokens,
        });
    }
    match tokens {
        None => Ok(ModelUsage::default()),
        Some(tokens) => Ok(ModelUsage {
            input_tokens: Some(
                u64::try_from(tokens.input_tokens)
                    .map_err(|_| protocol("模型用量必须是非负整数"))?,
            ),
            // 没有原始字段证据时，无法区分原 parser 的默认零与上游真实零。
            cached_input_tokens: None,
            output_tokens: Some(
                u64::try_from(tokens.output_tokens)
                    .map_err(|_| protocol("模型用量必须是非负整数"))?,
            ),
        }),
    }
}

fn protocol(message: &str) -> LlmError {
    LlmError::Protocol(message.into())
}
fn unsupported(message: &str) -> LlmError {
    LlmError::Unsupported(message.into())
}
