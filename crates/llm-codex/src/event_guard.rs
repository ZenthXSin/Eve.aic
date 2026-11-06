//! 原 SseTelemetry 的只读回调：不产生或更改事件，阻止原宽容解码丢项后提交部分批次。
use codex_api::SseTelemetry;
use codex_client::TransportError;
use codex_protocol::models::ResponseItem;
use eve_llm_api::{LlmError, ProviderFailureKind};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Mutex, time::Duration};

#[derive(Default)]
struct GuardState {
    failure: Option<LlmError>,
    done_items: BTreeMap<String, usize>,
    done_count: usize,
}

#[derive(Default)]
pub(crate) struct EventGuard(Mutex<GuardState>);

impl EventGuard {
    pub(crate) fn failure(&self) -> Option<LlmError> {
        match self.0.lock() {
            Ok(value) => value.failure.clone(),
            Err(_) => Some(LlmError::Backend("Codex 原事件校验状态不可用".into())),
        }
    }

    fn observe(state: &mut GuardState, data: &str) -> Result<(), LlmError> {
        let event = crate::strict_json::from_slice(data.as_bytes())
            .map_err(|_| protocol("原 SSE 含无法完整解码的 JSON 事件"))?;
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| protocol("原 SSE 事件没有有效类型"))?;
        match kind {
            "response.output_item.done" | "response.output_item.added" => {
                let item = event
                    .get("item")
                    .ok_or_else(|| protocol("原 SSE 输出项缺失"))?;
                let native = decode_item(item, kind.ends_with(".done"))?;
                if kind.ends_with(".done") {
                    *state.done_items.entry(signature(&native)?).or_default() += 1;
                    state.done_count += 1;
                }
            }
            "response.incomplete" => {
                let reason = event
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str);
                return Err(crate::error::failed(match reason {
                    Some("max_output_tokens") => ProviderFailureKind::Truncated,
                    Some("content_filter") => ProviderFailureKind::ContentFiltered,
                    _ => ProviderFailureKind::Incomplete,
                }));
            }
            "response.failed" | "error" => {
                return Err(crate::error::failed(ProviderFailureKind::Incomplete));
            }
            "response.completed" => {
                let response = event
                    .get("response")
                    .filter(|response| response.is_object())
                    .ok_or_else(|| protocol("原 SSE 完成事件没有 response 对象"))?;
                if response.get("error").is_some_and(|error| !error.is_null())
                    || response
                        .get("status")
                        .is_some_and(|status| status.as_str() != Some("completed"))
                {
                    return Err(crate::error::failed(ProviderFailureKind::Incomplete));
                }
                if let Some(output) = response.get("output") {
                    let output = output
                        .as_array()
                        .ok_or_else(|| protocol("原 SSE 完成输出必须是数组"))?;
                    if output.len() != state.done_count {
                        return Err(protocol("原 SSE 完成输出与已完成输出项数量不一致"));
                    }
                    // 原 Completed 事件不携带 output。只核对上游确实声明的集合，
                    // 防止缺失 done 的工具被静默略过；不补造或重新排序原事件。
                    let mut remaining = state.done_items.clone();
                    for item in output {
                        let native = decode_item(item, true)?;
                        let Some(count) = remaining.get_mut(&signature(&native)?) else {
                            return Err(protocol("原 SSE 完成输出与已完成输出项内容不一致"));
                        };
                        if *count == 0 {
                            return Err(protocol("原 SSE 完成输出重复声明同一输出项"));
                        }
                        *count -= 1;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

impl SseTelemetry for EventGuard {
    fn on_sse_poll(
        &self,
        result: &Result<
            Option<
                Result<
                    eventsource_stream::Event,
                    eventsource_stream::EventStreamError<TransportError>,
                >,
            >,
            tokio::time::error::Elapsed,
        >,
        _duration: Duration,
    ) {
        if let Ok(Some(Ok(event))) = result
            && let Ok(mut state) = self.0.lock()
            && state.failure.is_none()
            && let Err(error) = Self::observe(&mut state, &event.data)
        {
            state.failure = Some(error);
        }
    }
}

fn protocol(message: &str) -> LlmError {
    LlmError::Protocol(message.into())
}

fn decode_item(item: &Value, completed: bool) -> Result<ResponseItem, LlmError> {
    let native = serde_json::from_value::<ResponseItem>(item.clone())
        .map_err(|_| protocol("原 SSE 输出项不能由 Codex 原类型完整解码"))?;
    if completed {
        if item
            .get("status")
            .is_some_and(|status| status.as_str() != Some("completed"))
        {
            return Err(crate::error::failed(ProviderFailureKind::Incomplete));
        }
        if let Some(content) = item.get("content").and_then(Value::as_array) {
            for part in content {
                // 引用标注不改变普通文本或工具授权；当前 Eve 协议仅返回正文。
                if let Some(annotations) = part.get("annotations")
                    && !annotations.is_array()
                {
                    return Err(protocol("原 SSE 输出 annotations 必须是数组"));
                }
            }
        }
    }
    Ok(native)
}

fn signature(item: &ResponseItem) -> Result<String, LlmError> {
    serde_json::to_string(item).map_err(|_| protocol("原 SSE 输出项无法核对"))
}
