//! Chat Completions 私有线协议；完整转换后才交给通用模型/工具契约。
use eve_llm_api::{ChatRole, LlmError, ModelRequest, ModelResponse, ToolCall, ToolOutput};
use serde_json::{Value, json};

pub(crate) fn encode_request(model: &str, request: ModelRequest) -> Result<Value, LlmError> {
    request.validate()?;
    if request.messages.is_empty() {
        return Err(protocol("请求消息不能为空"));
    }
    let mut messages = Vec::new();
    for message in request.messages {
        if let Some(text) = message.text {
            let role = match message.role {
                ChatRole::System => "system",
                ChatRole::User => "user",
                ChatRole::Assistant => "assistant",
                ChatRole::Tool => unreachable!("validated tool message has no text"),
            };
            messages.push(json!({"role":role,"content":text}));
        } else if message.role == ChatRole::Assistant {
            let mut calls = Vec::new();
            for call in message.tool_calls {
                crate::wire::validate_function_name(&call.name)?;
                calls.push(json!({"id":call.id,"type":"function",
                    "function":{"name":call.name,"arguments":call.arguments.to_string()}}));
            }
            messages.push(json!({"role":"assistant","content":null,"tool_calls":calls}));
        } else {
            for result in message.tool_results {
                let output = match result.output {
                    ToolOutput::Success(value) => value,
                    ToolOutput::Failure { code, message } => {
                        json!({"error":{"code":code,"message":message}})
                    }
                };
                messages.push(json!({"role":"tool","tool_call_id":result.call_id,
                    "content":output.to_string()}));
            }
        }
    }
    let mut body = json!({"model":model,"messages":messages,"stream":false});
    let mut definitions = request.tools;
    definitions.sort_by(|a, b| a.name.cmp(&b.name));
    if !definitions.is_empty() {
        let mut tools = Vec::new();
        for tool in definitions {
            crate::wire::validate_function_name(&tool.name)?;
            tools.push(json!({"type":"function","function":{"name":tool.name,
                "description":tool.description,"parameters":tool.argument_schema}}));
        }
        body["tools"] = json!(tools);
    }
    Ok(body)
}

pub(crate) fn decode_response(bytes: &[u8]) -> Result<ModelResponse, LlmError> {
    let response: Value =
        crate::strict_json::from_slice(bytes).map_err(|_| protocol("Chat 响应不是有效 JSON"))?;
    if response.get("error").is_some_and(|v| !v.is_null()) {
        return Err(LlmError::Provider("Chat 响应返回错误".into()));
    }
    let choices = response
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| protocol("Chat choices 必须是数组"))?;
    if choices.is_empty() {
        return Err(protocol("Chat choices 不能为空"));
    }
    if choices.len() != 1 {
        return Err(unsupported("只支持单个 Chat choice"));
    }
    let choice = &choices[0];
    if choice.get("index").and_then(Value::as_u64) != Some(0) {
        return Err(protocol("Chat choice index 必须为零"));
    }
    let finish = required_str(choice, "finish_reason")?;
    match finish {
        "stop" | "tool_calls" => {}
        "length" | "content_filter" => {
            return Err(LlmError::Provider("Chat 输出未完整完成".into()));
        }
        _ => return Err(unsupported("不支持当前 Chat finish_reason")),
    }
    let message = choice
        .get("message")
        .filter(|v| v.is_object())
        .ok_or_else(|| protocol("Chat message 必须是对象"))?;
    if required_str(message, "role")? != "assistant" {
        return Err(protocol("Chat 回复必须属于 assistant"));
    }
    for key in ["reasoning_content", "reasoning"] {
        if message
            .get(key)
            .is_some_and(|v| !v.is_null() && v.as_str() != Some(""))
        {
            eprintln!("WARN eve.llm.chat: ignored auxiliary reasoning field");
        }
    }
    for key in ["refusal", "audio", "function_call"] {
        if message
            .get(key)
            .is_some_and(|v| !v.is_null() && v.as_str() != Some(""))
        {
            return Err(unsupported("不支持 Chat 拒绝、音频或旧式函数调用"));
        }
    }
    let text = match message.get("content") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.as_str()),
        _ => return Err(unsupported("只支持 Chat 纯文本 content")),
    };
    let mut calls = Vec::new();
    if let Some(value) = message.get("tool_calls").filter(|v| !v.is_null()) {
        for item in value
            .as_array()
            .ok_or_else(|| protocol("Chat tool_calls 必须是数组"))?
        {
            if required_str(item, "type")? != "function" {
                return Err(unsupported("只支持 Chat function 工具"));
            }
            let function = item
                .get("function")
                .filter(|v| v.is_object())
                .ok_or_else(|| protocol("Chat function 必须是对象"))?;
            let name = required_str(function, "name")?.to_owned();
            crate::wire::validate_function_name(&name)?;
            let arguments =
                crate::strict_json::from_slice(required_str(function, "arguments")?.as_bytes())
                    .map_err(|_| protocol("Chat 函数参数不是有效 JSON"))?;
            calls.push(ToolCall {
                id: required_str(item, "id")?.into(),
                name,
                arguments,
            });
        }
    }
    let result = match (finish, calls.is_empty()) {
        ("stop", true) => ModelResponse::Final {
            text: text
                .ok_or_else(|| protocol("Chat 完整回复缺少文本"))?
                .into(),
        },
        ("tool_calls", false) => {
            if text.is_some_and(|v| !v.trim().is_empty()) {
                eprintln!("WARN eve.llm.chat: ignored tool-call explanatory text");
            }
            ModelResponse::ToolCalls { calls }
        }
        _ => return Err(protocol("Chat 终态与工具批次不匹配")),
    };
    result
        .validate()
        .map_err(|_| protocol("Chat 文本或工具调用批次不符合协议"))?;
    Ok(result)
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, LlmError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| protocol("Chat 响应缺少必需字符串字段"))
}
fn protocol(message: &str) -> LlmError {
    LlmError::Protocol(message.into())
}
fn unsupported(message: &str) -> LlmError {
    LlmError::Unsupported(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_llm_api::{ChatMessage, ToolDefinition, ToolFailureCode, ToolResult};

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "echo".into(),
            arguments: json!({"text":"中文"}),
        }
    }
    fn function(id: &str) -> Value {
        json!({"id":id,"type":"function",
            "function":{"name":"echo","arguments":"{\"text\":\"中文\"}"}})
    }
    fn response(message: Value, finish: &str) -> Value {
        json!({"choices":[{"index":0,"finish_reason":finish,"message":message}]})
    }
    fn decode(value: Value) -> Result<ModelResponse, LlmError> {
        decode_response(&serde_json::to_vec(&value).unwrap())
    }
    #[test]
    fn preserves_ordered_history_tool_ids_and_structured_results() {
        let definition = |name: &str| ToolDefinition {
            name: name.into(),
            description: "说明".into(),
            argument_schema: json!({"type":"object"}),
            required_permissions: vec![],
            concurrency: None,
        };
        let body = encode_request(
            "model",
            ModelRequest {
                messages: vec![
                    ChatMessage::text(ChatRole::System, "规则"),
                    ChatMessage::text(ChatRole::User, "问题"),
                    ChatMessage::text(ChatRole::Assistant, "历史回复"),
                    ChatMessage::assistant_tool_calls(vec![call("b"), call("a")]).unwrap(),
                    ChatMessage::tool_results(vec![
                        ToolResult::success("b", json!({"echo":"中文"})).unwrap(),
                        ToolResult::failure("a", ToolFailureCode::TimedOut, "超时").unwrap(),
                    ])
                    .unwrap(),
                ],
                tools: vec![definition("z"), definition("echo")],
            },
        )
        .unwrap();
        assert_eq!(
            body["messages"][0],
            json!({"role":"system","content":"规则"})
        );
        assert_eq!(body["messages"][2]["content"], "历史回复");
        assert_eq!(body["messages"][3]["tool_calls"][0]["id"], "b");
        assert_eq!(body["messages"][3]["tool_calls"][1]["id"], "a");
        assert_eq!(body["messages"][4]["tool_call_id"], "b");
        assert_eq!(body["messages"][5]["tool_call_id"], "a");
        assert_eq!(
            serde_json::from_str::<Value>(body["messages"][4]["content"].as_str().unwrap())
                .unwrap(),
            json!({"echo":"中文"})
        );
        assert_eq!(
            serde_json::from_str::<Value>(body["messages"][5]["content"].as_str().unwrap())
                .unwrap(),
            json!({"error":{"code":"TimedOut","message":"超时"}})
        );
        assert_eq!(body["tools"][0]["function"]["name"], "echo");
        assert!(
            body["tools"][0]["function"]
                .get("required_permissions")
                .is_none()
        );
        assert!(body.get("input").is_none());
        assert!(body.get("store").is_none());
    }
    #[test]
    fn maps_text_and_complete_call_batches_without_losing_ids() {
        assert_eq!(
            decode(response(
                json!({"role":"assistant","content":"完整中文"}),
                "stop"
            ))
            .unwrap(),
            ModelResponse::Final {
                text: "完整中文".into()
            }
        );
        assert_eq!(
            decode(response(
                json!({"role":"assistant","content":null,"tool_calls":[function("b"),function("a")]}),
                "tool_calls",
            ))
            .unwrap(),
            ModelResponse::ToolCalls {
                calls: vec![call("b"), call("a")]
            }
        );
    }
    #[test]
    fn rejects_incomplete_unsupported_and_multiple_choices() {
        for finish in ["length", "content_filter"] {
            assert!(matches!(
                decode(response(
                    json!({"role":"assistant","content":"部分回复"}),
                    finish
                )),
                Err(LlmError::Provider(_))
            ));
        }
        for message in [
            json!({"role":"assistant","content":null,"refusal":"拒绝",
                "tool_calls":[function("a")]}),
            json!({"role":"assistant","content":[],"tool_calls":[function("a")]}),
        ] {
            assert!(matches!(
                decode(response(message, "tool_calls")),
                Err(LlmError::Unsupported(_))
            ));
        }
        let one =
            response(json!({"role":"assistant","content":"回复"}), "stop")["choices"][0].clone();
        assert!(matches!(
            decode(json!({"choices":[one.clone(),one]})),
            Err(LlmError::Unsupported(_))
        ));
    }
    #[test]
    fn accepts_reasoning_metadata_and_explanation_without_changing_calls() {
        assert_eq!(
            decode(response(
                json!({"role":"assistant","content":"解释","reasoning_content":"内部内容",
                    "tool_calls":[function("a")]}),
                "tool_calls",
            ))
            .unwrap(),
            ModelResponse::ToolCalls {
                calls: vec![call("a")]
            }
        );
        assert_eq!(
            decode(response(
                json!({"role":"assistant","content":"最终回复","reasoning":"附加内容"}),
                "stop",
            ))
            .unwrap(),
            ModelResponse::Final {
                text: "最终回复".into()
            }
        );
    }
    #[test]
    fn rejects_invalid_batches_and_duplicate_json_without_body_leakage() {
        for calls in [
            json!([function("same"), function("same")]),
            json!([{"id":"a","type":"function","function":{"name":"echo","arguments":"[]"}}]),
            json!([{"id":"a","type":"function","function":{"name":"echo",
                "arguments":"{\"text\":\"first\",\"text\":\"secret\"}"}}]),
            json!([{"id":"a","type":"function","function":{"name":"echo","arguments":"secret"}}]),
            json!([function(" ")]),
        ] {
            let error = decode(response(
                json!({"role":"assistant","content":null,"tool_calls":calls}),
                "tool_calls",
            ))
            .unwrap_err();
            assert!(matches!(error, LlmError::Protocol(_)));
            assert!(!error.to_string().contains("secret"));
        }
        for bytes in [
            b"secret invalid JSON".as_slice(),
            b"{\"choices\":[],\"choices\":[]}",
            b"{\"choices\":[{\"index\":0,\"finish_reason\":null}]}",
        ] {
            let error = decode_response(bytes).unwrap_err();
            assert!(matches!(error, LlmError::Protocol(_)));
            assert!(!error.to_string().contains("secret"));
        }
        for (finish, message) in [
            ("stop", json!({"role":"assistant","content":" " })),
            (
                "stop",
                json!({"role":"assistant","content":"reply","tool_calls":[function("a")]}),
            ),
            (
                "tool_calls",
                json!({"role":"assistant","content":null,"tool_calls":[]}),
            ),
        ] {
            assert!(matches!(
                decode(response(message, finish)),
                Err(LlmError::Protocol(_))
            ));
        }
    }
}
