//! 有界 SSE 解帧与 Responses 文本/函数状态机；不执行工具。
use eve_llm_api::{LlmError, ModelResponse};
use serde_json::Value;
use std::collections::BTreeMap;

fn invalid() -> LlmError {
    LlmError::Protocol("Responses 流事件缺失、重复或相互矛盾".into())
}
fn unsupported() -> LlmError {
    LlmError::Unsupported("Responses 流包含当前协议无法保留的内容".into())
}
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, LlmError> {
    value.get(key).and_then(Value::as_str).ok_or_else(invalid)
}
fn index(value: &Value, key: &str) -> Result<usize, LlmError> {
    usize::try_from(value.get(key).and_then(Value::as_u64).ok_or_else(invalid)?)
        .map_err(|_| invalid())
}

#[derive(Default)]
pub(crate) struct Sse {
    line: Vec<u8>,
    data: String,
    event: String,
    cr: bool,
    first: bool,
}
impl Sse {
    pub fn new() -> Self {
        Self {
            first: true,
            ..Self::default()
        }
    }
    // 每次仅返回一个帧，调用方立即 await 消费，不积累事件队列。
    pub fn byte(&mut self, byte: u8) -> Result<Option<(String, String)>, LlmError> {
        if self.cr {
            self.cr = false;
            if byte == b'\n' {
                return Ok(None);
            }
        }
        if byte != b'\r' && byte != b'\n' {
            self.line.push(byte);
            return Ok(None);
        }
        self.cr = byte == b'\r';
        let line = std::mem::take(&mut self.line);
        let line = std::str::from_utf8(&line).map_err(|_| invalid())?;
        let line = if self.first {
            self.first = false;
            line.trim_start_matches('\u{feff}')
        } else {
            line
        };
        if line.is_empty() {
            let event = std::mem::take(&mut self.event);
            if self.data.is_empty() {
                return Ok(None);
            }
            let mut data = std::mem::take(&mut self.data);
            data.pop(); // SSE 多行 data 以换行连接，删除最后一处换行。
            return Ok(Some((event, data)));
        }
        if line.starts_with(':') {
            return Ok(None);
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = value.to_owned(),
            "data" => {
                self.data.push_str(value);
                self.data.push('\n');
            }
            _ => {} // SSE 的 id/retry/未知字段不改变 Responses 内容。
        }
        Ok(None)
    }
}

#[derive(Default)]
struct Part {
    text: String,
    text_done: bool,
    done: bool,
}
struct Item {
    added: Value,
    arguments: String,
    args_done: bool,
    parts: BTreeMap<usize, Part>,
    done: Option<Value>,
}
#[derive(Default)]
pub(crate) struct ResponseStream {
    response_id: Option<String>,
    sequence: Option<u64>,
    items: Vec<Item>,
}
pub(crate) enum Update {
    None,
    Text(String),
    Completed(ModelResponse),
}
impl ResponseStream {
    pub fn event(&mut self, name: &str, data: &str) -> Result<Update, LlmError> {
        let event: Value =
            crate::strict_json::from_slice(data.as_bytes()).map_err(|_| invalid())?;
        let kind = string(&event, "type")?;
        if !name.is_empty() && name != kind {
            return Err(invalid());
        }
        if let Some(sequence) = event.get("sequence_number") {
            let sequence = sequence.as_u64().ok_or_else(invalid)?;
            if self.sequence.is_some_and(|last| sequence <= last) {
                return Err(invalid());
            }
            self.sequence = Some(sequence);
        }
        if let Some(id) = event.get("response_id")
            && id.as_str() != self.response_id.as_deref()
        {
            return Err(invalid());
        }
        if matches!(kind, "error" | "response.failed" | "response.incomplete") {
            return Err(LlmError::Provider("Responses 流未成功完成".into()));
        }
        if kind == "response.created" {
            if self.response_id.is_some() || !self.items.is_empty() {
                return Err(invalid());
            }
            let response = &event["response"];
            let id = string(response, "id")?;
            if id.is_empty()
                || string(response, "status")? != "in_progress"
                || response["output"].as_array().is_none_or(|v| !v.is_empty())
            {
                return Err(invalid());
            }
            self.response_id = Some(id.to_owned());
            return Ok(Update::None);
        }
        if self.response_id.is_none() {
            return Err(invalid());
        }
        match kind {
            "response.in_progress" => {
                if string(&event["response"], "id")? != self.response_id.as_deref().unwrap()
                    || string(&event["response"], "status")? != "in_progress"
                {
                    return Err(invalid());
                }
            }
            "response.output_item.added" => {
                let i = index(&event, "output_index")?;
                let item = &event["item"];
                if i != self.items.len()
                    || string(item, "id")?.is_empty()
                    || self.items.iter().any(|v| v.added["id"] == item["id"])
                {
                    return Err(invalid());
                }
                match string(item, "type")? {
                    "function_call" => {
                        if item.get("namespace").is_some_and(|v| !v.is_null()) {
                            return Err(unsupported());
                        }
                        if !string(item, "arguments")?.is_empty()
                            || string(item, "call_id")?.trim().is_empty()
                            || string(item, "name")?.trim().is_empty()
                        {
                            return Err(invalid());
                        }
                    }
                    "message" => {
                        if string(item, "role")? != "assistant"
                            || item
                                .get("phase")
                                .is_some_and(|v| !v.is_null() && v.as_str() != Some("final_answer"))
                        {
                            return Err(unsupported());
                        }
                        if item["content"].as_array().is_none_or(|v| !v.is_empty()) {
                            return Err(invalid());
                        }
                    }
                    _ => return Err(unsupported()),
                }
                if self
                    .items
                    .first()
                    .is_some_and(|v| v.added["type"] != item["type"])
                    || item["type"] == "message" && !self.items.is_empty()
                {
                    return Err(unsupported());
                }
                self.items.push(Item {
                    added: item.clone(),
                    arguments: String::new(),
                    args_done: false,
                    parts: BTreeMap::new(),
                    done: None,
                });
            }
            "response.content_part.added" => {
                let item = self.item(&event)?;
                if item.added["type"] != "message" {
                    return Err(invalid());
                }
                let i = index(&event, "content_index")?;
                if i != item.parts.len() {
                    return Err(invalid());
                }
                check_part(&event["part"])?;
                if !string(&event["part"], "text")?.is_empty() {
                    return Err(invalid());
                }
                item.parts.insert(i, Part::default());
            }
            "response.output_text.delta" => {
                let item = self.item(&event)?;
                let part = item
                    .parts
                    .get_mut(&index(&event, "content_index")?)
                    .ok_or_else(invalid)?;
                if part.text_done {
                    return Err(invalid());
                }
                let delta = string(&event, "delta")?;
                part.text.push_str(delta);
                return Ok(Update::Text(delta.to_owned()));
            }
            "response.output_text.done" => {
                let item = self.item(&event)?;
                let part = item
                    .parts
                    .get_mut(&index(&event, "content_index")?)
                    .ok_or_else(invalid)?;
                if part.text_done || part.text != string(&event, "text")? {
                    return Err(invalid());
                }
                part.text_done = true;
            }
            "response.content_part.done" => {
                let item = self.item(&event)?;
                let part = item
                    .parts
                    .get_mut(&index(&event, "content_index")?)
                    .ok_or_else(invalid)?;
                check_part(&event["part"])?;
                if !part.text_done || part.done || part.text != string(&event["part"], "text")? {
                    return Err(invalid());
                }
                part.done = true;
            }
            "response.function_call_arguments.delta" => {
                let item = self.item(&event)?;
                if item.added["type"] != "function_call" || item.args_done {
                    return Err(invalid());
                }
                item.arguments.push_str(string(&event, "delta")?);
            }
            "response.function_call_arguments.done" => {
                let item = self.item(&event)?;
                if item.added["type"] != "function_call"
                    || item.args_done
                    || item.arguments != string(&event, "arguments")?
                {
                    return Err(invalid());
                }
                item.args_done = true;
            }
            "response.output_item.done" => {
                let i = index(&event, "output_index")?;
                let item = self.items.get_mut(i).ok_or_else(invalid)?;
                let done = &event["item"];
                if item.done.is_some()
                    || done["id"] != item.added["id"]
                    || done["type"] != item.added["type"]
                {
                    return Err(invalid());
                }
                if done["type"] == "function_call" {
                    if !item.args_done
                        || done["call_id"] != item.added["call_id"]
                        || done["name"] != item.added["name"]
                        || string(done, "arguments")? != item.arguments
                    {
                        return Err(invalid());
                    }
                } else {
                    let content = done["content"].as_array().ok_or_else(invalid)?;
                    if content.len() != item.parts.len() {
                        return Err(invalid());
                    }
                    for (part, expected) in content.iter().zip(item.parts.values()) {
                        check_part(part)?;
                        if !expected.done || string(part, "text")? != expected.text {
                            return Err(invalid());
                        }
                    }
                }
                // 与非流式相同的严格语义验证：refusal/reasoning/phase/重复 JSON 等不能丢失。
                crate::wire::decode_response(
                    &serde_json::to_vec(&serde_json::json!({
                        "status":"completed", "output":[done]
                    }))
                    .map_err(|_| invalid())?,
                )?;
                item.done = Some(done.clone());
            }
            "response.completed" => {
                let response = &event["response"];
                if string(response, "id")? != self.response_id.as_deref().unwrap() {
                    return Err(invalid());
                }
                let output = response["output"].as_array().ok_or_else(invalid)?;
                if output.len() != self.items.len() || output.is_empty() {
                    return Err(invalid());
                }
                for (value, item) in output.iter().zip(&self.items) {
                    if item.done.as_ref() != Some(value) {
                        return Err(invalid());
                    }
                }
                let completed = crate::wire::decode_response(
                    &serde_json::to_vec(response).map_err(|_| invalid())?,
                )?;
                return Ok(Update::Completed(completed));
            }
            _ => return Err(unsupported()),
        }
        Ok(Update::None)
    }
    fn item(&mut self, event: &Value) -> Result<&mut Item, LlmError> {
        let item = self
            .items
            .get_mut(index(event, "output_index")?)
            .ok_or_else(invalid)?;
        if item.done.is_some() || string(event, "item_id")? != string(&item.added, "id")? {
            return Err(invalid());
        }
        Ok(item)
    }
}
fn check_part(part: &Value) -> Result<(), LlmError> {
    if string(part, "type")? != "output_text" {
        return Err(unsupported());
    }
    if let Some(value) = part.get("annotations")
        && !value.as_array().ok_or_else(invalid)?.is_empty()
    {
        return Err(unsupported());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decodes_every_byte_with_multiline_data_all_line_endings_and_utf8() {
        for ending in ["\n", "\r\n", "\r"] {
            let input = format!(
                "\u{feff}: ping{ending}id: 7{ending}retry: 100{ending}event: delta{ending}data: {{\"text\":{ending}data: \"中文🌍\"}}{ending}{ending}"
            );
            let mut parser = Sse::new();
            let frames: Vec<_> = input
                .bytes()
                .filter_map(|b| parser.byte(b).unwrap())
                .collect();
            assert_eq!(
                frames,
                vec![("delta".into(), "{\"text\":\n\"中文🌍\"}".into())]
            );
        }
    }
}
