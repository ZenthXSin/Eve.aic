//! Chat SSE 只拼接私有状态，正常终态后复用完整响应校验；参数片段不离开适配器。
use eve_llm_api::{LlmError, ModelResponse, ProviderFailureKind};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// 仅描述本地校验分支；不能装入模型返回的字段值。
#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StreamInvalidDetail {
    EventName,
    DoneBeforeFinish,
    InvalidJson,
    ResponseIdInvalid,
    ResponseIdChanged,
    ChoicesShape,
    ChoiceCount,
    ChoiceIndex,
    DeltaShape,
    Role,
    ContentShape,
    ToolCallsShape,
    ToolIndex,
    ToolIdInvalid,
    ToolIdChanged,
    ToolType,
    ToolFunctionShape,
    ToolFragmentShape,
    FinishShape,
    ToolBatchIdentity,
    CompletionEncoding,
}

pub(crate) struct StreamFailure {
    pub(crate) error: LlmError,
    pub(crate) detail: Option<StreamInvalidDetail>,
}
impl From<LlmError> for StreamFailure {
    fn from(error: LlmError) -> Self {
        Self {
            error,
            detail: None,
        }
    }
}
fn invalid(detail: StreamInvalidDetail) -> StreamFailure {
    StreamFailure {
        error: protocol(),
        detail: Some(detail),
    }
}

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
    seen_event: bool,
    bootstrap_id: bool,
    text: Option<String>,
    calls: BTreeMap<u64, Call>,
    usage: Option<eve_llm_api::ModelUsage>,
}

impl ChatStream {
    /// 语义终态后只保留统计所需的身份和用量，丢弃文本及工具片段。
    pub(crate) fn into_usage_tail(self) -> UsageTail {
        UsageTail {
            id: self.id,
            usage: self.usage,
        }
    }

    pub(crate) fn event(
        &mut self,
        name: &str,
        data: &str,
    ) -> Result<Option<ModelResponse>, StreamFailure> {
        use StreamInvalidDetail::*;
        if !matches!(name, "" | "message") {
            return Err(invalid(EventName));
        }
        if data.trim() == "[DONE]" {
            return Err(invalid(DoneBeforeFinish));
        }
        let value: Value =
            crate::strict_json::from_slice(data.as_bytes()).map_err(|_| invalid(InvalidJson))?;
        if value.get("error").is_some_and(|v| !v.is_null()) {
            return Err(crate::failed(ProviderFailureKind::Other).into());
        }
        // 只允许首个纯角色引导帧到紧随的首个正文/工具帧使用不同身份。
        // 第二帧无论是否为空都会关闭窗口，正文、工具或统计之后绝不重绑定。
        let can_bind_content = std::mem::take(&mut self.bootstrap_id);
        let initial_bootstrap = !self.seen_event && role_bootstrap(&value);
        self.seen_event = true;
        if let Some(id) = value.get("id").filter(|v| !v.is_null()) {
            let id = id
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| invalid(ResponseIdInvalid))?;
            if self.id.as_deref().is_some_and(|previous| previous != id)
                && !(can_bind_content && begins_content(&value))
            {
                return Err(invalid(ResponseIdChanged));
            }
            self.id = Some(id.into());
            self.bootstrap_id = initial_bootstrap;
        }
        let choices = value
            .get("choices")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid(ChoicesShape))?;
        if let Some(usage) = value.get("usage").and_then(crate::usage::from_usage) {
            self.usage = Some(usage);
        }
        if choices.is_empty() && value.get("usage").is_some_and(Value::is_object) {
            return Ok(None);
        }
        if choices.len() != 1 {
            return Err(invalid(ChoiceCount));
        }
        if choices[0].get("index").and_then(Value::as_u64) != Some(0) {
            return Err(invalid(ChoiceIndex));
        }
        let choice = &choices[0];
        let delta = choice
            .get("delta")
            .filter(|v| v.is_object())
            .ok_or_else(|| invalid(DeltaShape))?;
        if delta
            .get("role")
            .filter(|v| !v.is_null())
            .is_some_and(|v| v.as_str() != Some("assistant"))
        {
            return Err(invalid(Role));
        }
        for key in ["refusal", "audio", "function_call"] {
            if delta
                .get(key)
                .is_some_and(|v| !v.is_null() && v.as_str() != Some(""))
            {
                return Err(
                    LlmError::Unsupported("不支持 Chat 拒绝、音频或旧式函数调用".into()).into(),
                );
            }
        }
        // reasoning / reasoning_content 是内部推理，既不拼入文本也不输出诊断正文。
        if let Some(content) = delta.get("content").filter(|v| !v.is_null()) {
            self.text
                .get_or_insert_default()
                .push_str(content.as_str().ok_or_else(|| invalid(ContentShape))?);
        }
        if let Some(calls) = delta.get("tool_calls").filter(|v| !v.is_null()) {
            for item in calls.as_array().ok_or_else(|| invalid(ToolCallsShape))? {
                let index = item
                    .get("index")
                    .and_then(Value::as_u64)
                    .filter(|index| *index < 256)
                    .ok_or_else(|| invalid(ToolIndex))?;
                let call = self.calls.entry(index).or_default();
                // 一些兼容服务把未重复发送的可选字段显式编码为 null。
                // null 与省略相同，不清除已收集的值；终态仍严格核对完整身份与参数。
                if let Some(id) = item.get("id").filter(|v| !v.is_null()) {
                    let id = id
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| invalid(ToolIdInvalid))?;
                    if call.id.as_deref().is_some_and(|previous| previous != id) {
                        return Err(invalid(ToolIdChanged));
                    }
                    call.id = Some(id.into());
                }
                if let Some(kind) = item.get("type").filter(|v| !v.is_null()) {
                    if kind.as_str() != Some("function") {
                        return Err(invalid(ToolType));
                    }
                    call.function_type = true;
                }
                if let Some(function) = item.get("function").filter(|v| !v.is_null()) {
                    if !function.is_object() {
                        return Err(invalid(ToolFunctionShape));
                    }
                    for (key, destination) in
                        [("name", &mut call.name), ("arguments", &mut call.arguments)]
                    {
                        if let Some(part) = function.get(key).filter(|v| !v.is_null()) {
                            destination
                                .push_str(part.as_str().ok_or_else(|| invalid(ToolFragmentShape))?);
                        }
                    }
                }
            }
        }
        let Some(finish) = choice.get("finish_reason").filter(|v| !v.is_null()) else {
            return Ok(None);
        };
        let finish = finish.as_str().ok_or_else(|| invalid(FinishShape))?;
        let mut calls = Vec::new();
        for (expected, (index, call)) in self.calls.iter().enumerate() {
            if *index != expected as u64 || !call.function_type || call.id.is_none() {
                return Err(invalid(ToolBatchIdentity));
            }
            calls.push(json!({"id":call.id,"type":"function",
                "function":{"name":call.name,"arguments":call.arguments}}));
        }
        let response = json!({"choices":[{"index":0,"finish_reason":finish,
            "message":{"role":"assistant","content":self.text,"tool_calls":calls}}]});
        crate::chat::decode_response(
            &serde_json::to_vec(&response).map_err(|_| invalid(CompletionEncoding))?,
        )
        .map(Some)
        .map_err(Into::into)
    }
}

fn role_bootstrap(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if !object.keys().all(|key| {
        matches!(
            key.as_str(),
            "id" | "object" | "created" | "model" | "choices"
        )
    }) {
        return false;
    }
    let Some(choices) = value.get("choices").and_then(Value::as_array) else {
        return false;
    };
    if choices.len() != 1 {
        return false;
    }
    let Some(choice) = choices[0].as_object() else {
        return false;
    };
    choice
        .keys()
        .all(|key| matches!(key.as_str(), "index" | "delta" | "finish_reason"))
        && choice.get("index").and_then(Value::as_u64) == Some(0)
        && choice.get("finish_reason").is_none_or(Value::is_null)
        && choice
            .get("delta")
            .and_then(Value::as_object)
            .is_some_and(|delta| {
                delta.len() == 1 && delta.get("role").and_then(Value::as_str) == Some("assistant")
            })
}

fn begins_content(value: &Value) -> bool {
    let Some(delta) = value.pointer("/choices/0/delta").and_then(Value::as_object) else {
        return false;
    };
    delta
        .keys()
        .all(|key| matches!(key.as_str(), "role" | "content" | "tool_calls"))
        && (delta
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty())
            || delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| !calls.is_empty()))
}

fn protocol() -> LlmError {
    LlmError::Protocol("Chat 流片段无效、终态缺失或标识不一致".into())
}

/// 终态之后不能再次进入文本/工具状态机，只接受同一请求的统计和 DONE。
pub(crate) struct UsageTail {
    id: Option<String>,
    pub(crate) usage: Option<eve_llm_api::ModelUsage>,
}
impl UsageTail {
    /// true 表示 DONE；调用方把错误视为统计未完整收取，不改变模型结果。
    pub(crate) fn event(&mut self, name: &str, data: &str) -> Result<bool, LlmError> {
        if !matches!(name, "" | "message") {
            return Err(protocol());
        }
        if data.trim() == "[DONE]" {
            return Ok(true);
        }
        let value: Value =
            crate::strict_json::from_slice(data.as_bytes()).map_err(|_| protocol())?;
        if value.get("error").is_some_and(|v| !v.is_null()) {
            return Err(protocol());
        }
        if let Some(id) = value.get("id").filter(|v| !v.is_null()) {
            let id = id
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or_else(protocol)?;
            if self.id.as_deref().is_some_and(|previous| previous != id) {
                return Err(protocol());
            }
            self.id = Some(id.into());
        }
        if !value
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
            || !value
                .get("usage")
                .is_some_and(|v| v.is_null() || v.is_object())
        {
            return Err(protocol());
        }
        if let Some(usage) = value.get("usage").and_then(crate::usage::from_usage) {
            self.usage = Some(usage);
        }
        Ok(false)
    }
}
