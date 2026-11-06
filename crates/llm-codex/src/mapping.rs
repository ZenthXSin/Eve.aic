//! Eve 公共契约与原 Codex 类型的组合转换；不改写原协议类型或 SSE parser。
use crate::CodexConfig;
use base64::Engine as _;
use codex_api::{Reasoning, ResponsesApiRequest, ResponsesApiTools};
use codex_protocol::models::{
    ContentItem, FunctionCallOutputPayload, ImageDetail, ImageReference, MessagePhase, ResponseItem,
};
use eve_llm_api::{ChatRole, LlmError, ModelRequest, ModelSession, ToolOutput};
use serde_json::{Value, json, value::RawValue};
use std::{collections::HashMap, sync::Arc};

pub(crate) fn encode(
    config: &CodexConfig,
    request: &ModelRequest,
    session: &ModelSession,
    json_mode: bool,
) -> Result<Value, LlmError> {
    request.validate()?;
    session.validate()?;
    if request.messages.is_empty() {
        return Err(LlmError::Protocol("请求消息不能为空".into()));
    }
    let mut input = Vec::new();
    for message in &request.messages {
        if let Some(text) = message.text_for_model() {
            let (role, mut content, phase) = match message.role {
                ChatRole::System => (
                    "system",
                    vec![ContentItem::InputText {
                        text: text.into_owned(),
                    }],
                    None,
                ),
                ChatRole::User => (
                    "user",
                    vec![ContentItem::InputText {
                        text: text.into_owned(),
                    }],
                    None,
                ),
                ChatRole::Assistant => (
                    "assistant",
                    vec![ContentItem::OutputText {
                        text: text.into_owned(),
                    }],
                    Some(MessagePhase::FinalAnswer),
                ),
                ChatRole::Tool => {
                    return Err(LlmError::Protocol("工具消息不能附带普通正文".into()));
                }
            };
            for image in &message.images {
                content.push(ContentItem::InputImage {
                    image: ImageReference::Inline {
                        image_url: format!(
                            "data:{};base64,{}",
                            image.media_type(),
                            base64::engine::general_purpose::STANDARD.encode(image.data())
                        ),
                    },
                    detail: Some(ImageDetail::Auto),
                });
            }
            input.push(ResponseItem::Message {
                id: None,
                role: role.into(),
                content,
                phase,
                internal_chat_message_metadata_passthrough: None,
            });
        } else if message.role == ChatRole::Assistant {
            for call in &message.tool_calls {
                function_name(&call.name)?;
                input.push(ResponseItem::FunctionCall {
                    id: None,
                    name: call.name.clone(),
                    namespace: None,
                    arguments: call.arguments.to_string(),
                    encrypted_function_args: None,
                    call_id: call.id.clone(),
                    internal_chat_message_metadata_passthrough: None,
                });
            }
        } else {
            for result in &message.tool_results {
                let output = match &result.output {
                    ToolOutput::Success(value) => value.clone(),
                    ToolOutput::Failure { code, message } => {
                        json!({"error":{"code":code,"message":message}})
                    }
                };
                input.push(ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: Some(result.call_id.clone()),
                    name: None,
                    namespace: None,
                    output: FunctionCallOutputPayload::from_text(output.to_string()),
                    internal_chat_message_metadata_passthrough: None,
                });
            }
        }
    }
    let mut definitions: Vec<_> = request.tools.iter().collect();
    definitions.sort_by(|left, right| left.name.cmp(&right.name));
    let mut tools = Vec::with_capacity(definitions.len());
    for definition in definitions {
        function_name(&definition.name)?;
        tools.push(json!({
            "type":"function", "name":definition.name,
            "description":definition.description, "parameters":definition.argument_schema,
            "strict":false,
        }));
    }
    let raw = RawValue::from_string(serde_json::to_string(&tools).map_err(|_| encode_error())?)
        .map_err(|_| encode_error())?;
    let reasoning = config
        .reasoning_effort
        .as_ref()
        .map(|effort| {
            serde_json::from_value(json!(effort))
                .map(|effort| Reasoning {
                    effort: Some(effort),
                    summary: None,
                    context: None,
                })
                .map_err(|_| {
                    LlmError::Configuration("reasoning effort 无法由原 Codex 类型表示".into())
                })
        })
        .transpose()?;
    let mut metadata = HashMap::from([
        ("session_id".into(), session.session_id().into()),
        ("thread_id".into(), session.thread_id().into()),
    ]);
    if let Some(turn_id) = session.turn_id() {
        metadata.insert("turn_id".into(), turn_id.into());
    }
    let native = ResponsesApiRequest {
        model: config.model.clone(),
        stream: true,
        service_tier: None,
        input,
        tools: Some(ResponsesApiTools::from(Arc::<RawValue>::from(raw))),
        tool_choice: if tools.is_empty() { "none" } else { "auto" }.into(),
        parallel_tool_calls: true,
        reasoning,
        store: false,
        stream_options: None,
        // 不请求不能由 Eve 通用历史持久化的加密 reasoning；原 parser 仍处理 reasoning 事件。
        include: Vec::new(),
        prompt_cache_key: Some(session.prompt_cache_key().into()),
        text: None,
        client_metadata: Some(metadata),
        access_programs: None,
    };
    let mut body = serde_json::to_value(native).map_err(|_| encode_error())?;
    let object = body.as_object_mut().ok_or_else(encode_error)?;
    // 原请求类型没有 max_output_tokens，reasoning=None 又会成为 null。
    // 这些 Eve 配置扩展仅发生在此组合层，随后交给原 ResponsesClient::stream。
    if config.reasoning_effort.is_none() {
        object.remove("reasoning");
    }
    if let Some(limit) = config.max_output_tokens {
        object.insert("max_output_tokens".into(), json!(limit));
    }
    if json_mode {
        object.insert("text".into(), json!({"format":{"type":"json_object"}}));
    }
    Ok(body)
}

pub(crate) fn function_name(name: &str) -> Result<(), LlmError> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(LlmError::Unsupported(
            "工具名不在 Eve 函数工具支持的范围内".into(),
        ));
    }
    Ok(())
}

fn encode_error() -> LlmError {
    LlmError::Protocol("Eve 请求无法转换为原 Codex Responses 请求".into())
}
