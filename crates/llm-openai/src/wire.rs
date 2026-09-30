use eve_llm_api::{ChatRole, LlmError, ModelRequest, ModelResponse, ToolCall, ToolOutput};
use serde_json::{Value, json};

pub(crate) fn encode_request(model: &str, request: ModelRequest) -> Result<Value, LlmError> {
    request.validate()?;
    if request.messages.is_empty() {
        return Err(protocol("请求消息不能为空"));
    }
    let mut input = Vec::new();
    for message in request.messages {
        if let Some(text) = message.text {
            let role = match message.role {
                ChatRole::System => "system",
                ChatRole::User => "user",
                ChatRole::Assistant => "assistant",
                ChatRole::Tool => unreachable!("validated tool message has no text"),
            };
            let mut item = json!({"role": role, "content": text});
            // Eve 的 assistant 文本历史只有已完成回复，等价于 final_answer。
            if role == "assistant" {
                item["phase"] = json!("final_answer");
            }
            input.push(item);
        } else if message.role == ChatRole::Assistant {
            for call in message.tool_calls {
                validate_function_name(&call.name)?;
                input.push(json!({
                    "type": "function_call", "call_id": call.id,
                    "name": call.name, "arguments": call.arguments.to_string()
                }));
            }
        } else {
            for result in message.tool_results {
                let output = match result.output {
                    ToolOutput::Success(value) => value,
                    ToolOutput::Failure { code, message } => {
                        json!({"error": {"code": code, "message": message}})
                    }
                };
                input.push(json!({
                    "type": "function_call_output", "call_id": result.call_id,
                    "output": output.to_string()
                }));
            }
        }
    }
    let mut definitions = request.tools;
    definitions.sort_by(|a, b| a.name.cmp(&b.name));
    let mut tools = Vec::with_capacity(definitions.len());
    for tool in definitions {
        validate_function_name(&tool.name)?;
        tools.push(json!({
            "type": "function", "name": tool.name, "description": tool.description,
            "parameters": tool.argument_schema, "strict": false
        }));
    }
    Ok(json!({
        "model": model, "input": input, "tools": tools,
        "parallel_tool_calls": true, "store": false, "stream": false
    }))
}

fn validate_function_name(name: &str) -> Result<(), LlmError> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(LlmError::Unsupported(
            "工具名不在当前 OpenAI 适配器的支持子集中".into(),
        ));
    }
    Ok(())
}

pub(crate) fn decode_response(bytes: &[u8]) -> Result<ModelResponse, LlmError> {
    let response: Value =
        crate::strict_json::from_slice(bytes).map_err(|_| protocol("OpenAI 响应不是有效 JSON"))?;
    let status = required_str(&response, "status")?;
    if status != "completed" || response.get("error").is_some_and(|v| !v.is_null()) {
        return Err(LlmError::Provider("OpenAI 响应未成功完成".into()));
    }
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| protocol("OpenAI output 必须是数组"))?;
    if output.is_empty() {
        return Err(protocol("OpenAI output 不能为空"));
    }
    let mut text = None;
    let mut calls = Vec::new();
    for item in output {
        match required_str(item, "type")? {
            "function_call" => {
                completed_item(item)?;
                if item.get("namespace").is_some_and(|v| !v.is_null()) {
                    return Err(unsupported("通用协议不能保留函数 namespace"));
                }
                let id = required_str(item, "call_id")?.to_owned();
                let name = required_str(item, "name")?.to_owned();
                let arguments =
                    crate::strict_json::from_slice(required_str(item, "arguments")?.as_bytes())
                        .map_err(|_| protocol("OpenAI 函数参数不是有效 JSON"))?;
                calls.push(ToolCall {
                    id,
                    name,
                    arguments,
                });
            }
            "message" => {
                completed_item(item)?;
                if item
                    .get("phase")
                    .is_some_and(|v| !v.is_null() && v.as_str() != Some("final_answer"))
                {
                    return Err(unsupported("通用协议不能保留中间或未知消息 phase"));
                }
                if required_str(item, "role")? != "assistant" || text.is_some() {
                    return Err(unsupported("仅支持单个 assistant 文本消息"));
                }
                let content = item
                    .get("content")
                    .and_then(Value::as_array)
                    .ok_or_else(|| protocol("OpenAI message content 必须是数组"))?;
                let mut combined = String::new();
                for part in content {
                    if required_str(part, "type")? != "output_text" {
                        return Err(unsupported("不支持非文本或 refusal 内容"));
                    }
                    if let Some(annotations) = part.get("annotations") {
                        let annotations = annotations
                            .as_array()
                            .ok_or_else(|| protocol("OpenAI annotations 必须是数组"))?;
                        if !annotations.is_empty() {
                            return Err(unsupported("通用文本协议不能保留 annotations"));
                        }
                    }
                    combined.push_str(required_str(part, "text")?);
                }
                text = Some(combined);
            }
            _ => return Err(unsupported("通用协议不能保留 reasoning 或其他输出项")),
        }
    }
    let result = match (text, calls.is_empty()) {
        (Some(_), false) => return Err(unsupported("不支持文本与函数调用混合输出")),
        (Some(text), true) => ModelResponse::Final { text },
        (None, _) => ModelResponse::ToolCalls { calls },
    };
    // 保留完整批次后校验非空文本、对象参数、调用 ID/名称及 ID 唯一性。
    result
        .validate()
        .map_err(|_| protocol("OpenAI 文本或函数调用批次不符合协议"))?;
    Ok(result)
}

fn completed_item(item: &Value) -> Result<(), LlmError> {
    if let Some(status) = item.get("status") {
        let status = status
            .as_str()
            .ok_or_else(|| protocol("OpenAI item status 必须是字符串"))?;
        if status != "completed" {
            return Err(LlmError::Provider("OpenAI 输出项未完成".into()));
        }
    }
    Ok(())
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, LlmError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| protocol("OpenAI 响应缺少必需字符串字段"))
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
    use eve_llm_api::{ChatMessage, ToolConcurrency, ToolDefinition, ToolFailureCode, ToolResult};

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "echo".into(),
            arguments: json!({"text": "中文"}),
        }
    }
    fn function(id: &str) -> Value {
        json!({"type":"function_call", "id":"item-id-is-not-call-id", "call_id":id,
            "name":"echo", "arguments":"{\"text\":\"中文\"}", "status":"completed"})
    }
    fn message(text: &str) -> Value {
        json!({"type":"message", "role":"assistant", "status":"completed",
            "content":[{"type":"output_text", "text":text, "annotations":[]}]})
    }
    fn decode(output: Value) -> Result<ModelResponse, LlmError> {
        decode_response(
            &serde_json::to_vec(&json!({"status":"completed", "error":null, "output":output}))
                .unwrap(),
        )
    }

    #[test]
    fn maps_full_history_stable_tools_and_structured_failures() {
        let definition = |name: &str| ToolDefinition {
            name: name.into(),
            description: "说明".into(),
            argument_schema: json!({"type":"object", "properties":{"text":{"type":"string"}}}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::ParallelSafe),
        };
        let request = ModelRequest {
            messages: vec![
                ChatMessage::text(ChatRole::System, "规则"),
                ChatMessage::text(ChatRole::User, "问题"),
                ChatMessage::text(ChatRole::Assistant, "历史回复"),
                ChatMessage::assistant_tool_calls(vec![call("b"), call("a")]).unwrap(),
                ChatMessage::tool_results(vec![
                    ToolResult::success("b", json!({"echo":"中文"})).unwrap(),
                    ToolResult::failure("a", ToolFailureCode::TimedOut, "已超时").unwrap(),
                ])
                .unwrap(),
            ],
            tools: vec![definition("z"), definition("echo")],
        };
        let encoded = encode_request("model", request).unwrap();
        assert_eq!(encoded["store"], false);
        assert_eq!(encoded["stream"], false);
        assert_eq!(encoded["parallel_tool_calls"], true);
        assert!(encoded.get("previous_response_id").is_none());
        assert_eq!(encoded["tools"][0]["name"], "echo");
        assert_eq!(encoded["tools"][0]["strict"], false);
        assert!(encoded["tools"][0]["parameters"].get("required").is_none());
        assert!(encoded["tools"][0].get("required_permissions").is_none());
        assert_eq!(
            encoded["input"][0],
            json!({"role":"system", "content":"规则"})
        );
        assert_eq!(
            encoded["input"][2],
            json!({"role":"assistant", "content":"历史回复", "phase":"final_answer"})
        );
        assert!(encoded["input"][1].get("phase").is_none());
        assert_eq!(encoded["input"][3]["call_id"], "b");
        assert_eq!(encoded["input"][4]["call_id"], "a");
        assert_eq!(encoded["input"][5]["call_id"], "b");
        assert_eq!(encoded["input"][6]["call_id"], "a");
        let args: Value =
            serde_json::from_str(encoded["input"][3]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args, json!({"text":"中文"}));
        let failure: Value =
            serde_json::from_str(encoded["input"][6]["output"].as_str().unwrap()).unwrap();
        assert_eq!(
            failure,
            json!({"error":{"code":"TimedOut", "message":"已超时"}})
        );
    }

    #[test]
    fn preserves_completed_phase_and_rejects_intermediate_or_unknown_phase() {
        for phase in [Value::Null, json!("final_answer")] {
            let mut item = message("完整回复");
            item["phase"] = phase;
            assert_eq!(
                decode(json!([item])).unwrap(),
                ModelResponse::Final {
                    text: "完整回复".into()
                }
            );
        }
        for phase in [json!("commentary"), json!("unknown"), json!(42)] {
            let mut item = message("中间信息");
            item["phase"] = phase;
            assert!(matches!(
                decode(json!([item])),
                Err(LlmError::Unsupported(_))
            ));
        }
    }

    #[test]
    fn preserves_unicode_text_and_function_call_ids_in_output_order() {
        let mut text = message("第一段");
        text["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"output_text", "text":"\n第二段"}));
        assert_eq!(
            decode(json!([text])).unwrap(),
            ModelResponse::Final {
                text: "第一段\n第二段".into()
            }
        );
        assert_eq!(
            decode(json!([function("b"), function("a")])).unwrap(),
            ModelResponse::ToolCalls {
                calls: vec![call("b"), call("a")]
            }
        );
    }

    #[test]
    fn rejects_lossy_output_instead_of_returning_partial_calls() {
        let mut annotations = message("引用");
        annotations["content"][0]["annotations"] = json!([{"type":"url_citation"}]);
        for output in [
            json!([function("a"), message("同时解释")]),
            json!([message("同时解释"), function("a")]),
            json!([function("a"), {"type":"reasoning", "summary":[]}]),
            json!([{"type":"web_search_call"}]),
            json!([{"type":"message", "role":"assistant", "content":[{"type":"refusal", "refusal":"拒绝"}]}]),
            json!([annotations]),
            json!([{"type":"message", "role":"assistant", "phase":"commentary", "content":[]}]),
            json!([{"type":"function_call", "namespace":"other", "name":"echo", "call_id":"a", "arguments":"{}"}]),
            json!([message("一"), message("二")]),
        ] {
            assert!(matches!(decode(output), Err(LlmError::Unsupported(_))));
        }
    }

    #[test]
    fn rejects_corrupt_or_invalid_batches_without_echoing_backend_content() {
        for output in [
            json!([]),
            json!([message("  ")]),
            json!([function("same"), function("same")]),
        ] {
            assert!(matches!(decode(output), Err(LlmError::Protocol(_))));
        }
        for (field, value) in [
            ("arguments", json!("[]")),
            (
                "arguments",
                json!("{\"text\":\"first\",\"text\":\"second\"}"),
            ),
            ("arguments", json!("invalid secret")),
            ("arguments", json!({})),
            ("call_id", json!(" ")),
            ("name", json!("")),
            ("call_id", Value::Null),
        ] {
            let mut item = function("a");
            item[field] = value;
            let error = decode(json!([item])).unwrap_err();
            assert!(matches!(error, LlmError::Protocol(_)));
            assert!(!error.to_string().contains("secret"));
        }
        for bytes in [
            b"not JSON secret".as_slice(),
            b"{}",
            b"null",
            b"{\"status\":42}",
            b"{\"status\":\"failed\",\"status\":\"completed\",\"output\":[]}",
        ] {
            assert!(matches!(decode_response(bytes), Err(LlmError::Protocol(_))));
        }
    }

    #[test]
    fn maps_failed_incomplete_and_unfinished_items_to_provider_errors() {
        for status in ["failed", "incomplete", "in_progress", "queued", "cancelled"] {
            let response =
                json!({"status":status, "output":[message("text")], "error":{"message":"secret"}});
            let error = decode_response(&serde_json::to_vec(&response).unwrap()).unwrap_err();
            assert!(matches!(error, LlmError::Provider(_)));
            assert!(!error.to_string().contains("secret"));
        }
        let mut item = function("a");
        item["status"] = json!("in_progress");
        assert!(matches!(decode(json!([item])), Err(LlmError::Provider(_))));
    }

    #[test]
    fn rejects_invalid_requests_before_mapping() {
        for request in [
            ModelRequest {
                messages: vec![],
                tools: vec![],
            },
            ModelRequest {
                messages: vec![ChatMessage::text(ChatRole::User, " ")],
                tools: vec![],
            },
            ModelRequest {
                messages: vec![ChatMessage::assistant_tool_calls(vec![call("a")]).unwrap()],
                tools: vec![],
            },
        ] {
            assert!(matches!(
                encode_request("model", request),
                Err(LlmError::Protocol(_))
            ));
        }
        for name in ["tool.with.dots", "工具", &"a".repeat(65)] {
            let request = ModelRequest {
                messages: vec![ChatMessage::text(ChatRole::User, "run")],
                tools: vec![ToolDefinition {
                    name: name.into(),
                    description: String::new(),
                    argument_schema: json!({}),
                    required_permissions: vec![],
                    concurrency: None,
                }],
            };
            assert!(matches!(
                encode_request("model", request),
                Err(LlmError::Unsupported(_))
            ));
        }
    }
}
