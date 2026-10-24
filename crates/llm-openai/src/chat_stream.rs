//! Chat SSE 只拼接私有状态，正常终态后复用完整响应校验；参数片段不离开适配器。
use eve_llm_api::{LlmError, ModelResponse, ProviderFailureKind};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Default)]
struct Call {
    id: Option<String>,
    name: String,
    arguments: String,
    function_type: bool,
}

#[derive(Default)]
pub(crate) struct ChatStream {
    id: Option<String>,
    text: Option<String>,
    calls: BTreeMap<u64, Call>,
    usage: Option<eve_llm_api::ModelUsage>,
}

impl ChatStream {
    /// 终态之前任一分块携带的用量；服务不发送时为 None。
    pub(crate) fn usage(&self) -> Option<eve_llm_api::ModelUsage> {
        self.usage
    }

    pub(crate) fn event(
        &mut self,
        name: &str,
        data: &str,
    ) -> Result<Option<ModelResponse>, LlmError> {
        if !matches!(name, "" | "message") || data.trim() == "[DONE]" {
            return Err(protocol());
        }
        let value: Value =
            crate::strict_json::from_slice(data.as_bytes()).map_err(|_| protocol())?;
        if value.get("error").is_some_and(|v| !v.is_null()) {
            return Err(crate::failed(ProviderFailureKind::Other));
        }
        if let Some(id) = value.get("id") {
            let id = id
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or_else(protocol)?;
            if self.id.as_deref().is_some_and(|previous| previous != id) {
                return Err(protocol());
            }
            self.id = Some(id.into());
        }
        let choices = value
            .get("choices")
            .and_then(Value::as_array)
            .ok_or_else(protocol)?;
        if let Some(usage) = value.get("usage").and_then(crate::usage::from_usage) {
            self.usage = Some(usage);
        }
        if choices.is_empty() && value.get("usage").is_some_and(Value::is_object) {
            return Ok(None);
        }
        if choices.len() != 1 || choices[0].get("index").and_then(Value::as_u64) != Some(0) {
            return Err(protocol());
        }
        let choice = &choices[0];
        let delta = choice
            .get("delta")
            .filter(|v| v.is_object())
            .ok_or_else(protocol)?;
        if delta
            .get("role")
            .is_some_and(|v| v.as_str() != Some("assistant"))
        {
            return Err(protocol());
        }
        for key in ["refusal", "audio", "function_call"] {
            if delta
                .get(key)
                .is_some_and(|v| !v.is_null() && v.as_str() != Some(""))
            {
                return Err(LlmError::Unsupported(
                    "不支持 Chat 拒绝、音频或旧式函数调用".into(),
                ));
            }
        }
        // reasoning / reasoning_content 是内部推理，既不拼入文本也不输出诊断正文。
        if let Some(content) = delta.get("content").filter(|v| !v.is_null()) {
            self.text
                .get_or_insert_default()
                .push_str(content.as_str().ok_or_else(protocol)?);
        }
        if let Some(calls) = delta.get("tool_calls").filter(|v| !v.is_null()) {
            for item in calls.as_array().ok_or_else(protocol)? {
                let index = item
                    .get("index")
                    .and_then(Value::as_u64)
                    .filter(|index| *index < 256)
                    .ok_or_else(protocol)?;
                let call = self.calls.entry(index).or_default();
                if let Some(id) = item.get("id") {
                    let id = id
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .ok_or_else(protocol)?;
                    if call.id.as_deref().is_some_and(|previous| previous != id) {
                        return Err(protocol());
                    }
                    call.id = Some(id.into());
                }
                if let Some(kind) = item.get("type") {
                    if kind.as_str() != Some("function") {
                        return Err(protocol());
                    }
                    call.function_type = true;
                }
                if let Some(function) = item.get("function") {
                    if !function.is_object() {
                        return Err(protocol());
                    }
                    for (key, destination) in
                        [("name", &mut call.name), ("arguments", &mut call.arguments)]
                    {
                        if let Some(part) = function.get(key) {
                            destination.push_str(part.as_str().ok_or_else(protocol)?);
                        }
                    }
                }
            }
        }
        let Some(finish) = choice.get("finish_reason").filter(|v| !v.is_null()) else {
            return Ok(None);
        };
        let finish = finish.as_str().ok_or_else(protocol)?;
        let mut calls = Vec::new();
        for (expected, (index, call)) in self.calls.iter().enumerate() {
            if *index != expected as u64 || !call.function_type || call.id.is_none() {
                return Err(protocol());
            }
            calls.push(json!({"id":call.id,"type":"function",
                "function":{"name":call.name,"arguments":call.arguments}}));
        }
        let response = json!({"choices":[{"index":0,"finish_reason":finish,
            "message":{"role":"assistant","content":self.text,"tool_calls":calls}}]});
        crate::chat::decode_response(&serde_json::to_vec(&response).map_err(|_| protocol())?)
            .map(Some)
    }
}

fn protocol() -> LlmError {
    LlmError::Protocol("Chat 流片段无效、终态缺失或标识不一致".into())
}
