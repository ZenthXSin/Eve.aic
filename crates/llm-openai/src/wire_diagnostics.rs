//! 最终请求的可选私密摘要。只读取已组装正文，不改变正文、请求或模型轮次。
//! 正文使用每进程随机 HMAC，日志不能用于离线猜测短句；关联 ID 单独 SHA256。
use crate::timing::RequestIdentity;
use reqwest::header::HeaderMap;
use ring::{
    digest::{SHA256, digest},
    hmac,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Serialize, Serializer, ser::SerializeMap};
use serde_json::{Value, json};
use std::{io, sync::OnceLock};

const DOMAIN: &str = "eve.wire_diagnostics.v1";
const MAX_ITEMS: usize = 512;
const MAX_TOOL_ITEMS: usize = 128;
const MAX_ID_BYTES: usize = 1024;

struct ProcessSecret {
    key: hmac::Key,
    nonce: String,
}
static PROCESS_SECRET: OnceLock<Option<ProcessSecret>> = OnceLock::new();

#[derive(Clone, Copy)]
pub(crate) enum WireProtocol {
    Chat,
    Responses,
}
impl WireProtocol {
    fn name(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Responses => "responses",
        }
    }
}

pub(crate) struct WireDiagnostics {
    secret: &'static ProcessSecret,
}
impl WireDiagnostics {
    /// 调用方只在显式启用后构造；随机源失败仅禁用诊断，不中断实际请求。
    pub(crate) fn new() -> Option<Self> {
        PROCESS_SECRET
            .get_or_init(|| {
                let mut random = [0; 48];
                SystemRandom::new().fill(&mut random).ok()?;
                Some(ProcessSecret {
                    key: hmac::Key::new(hmac::HMAC_SHA256, &random[..32]),
                    nonce: hex(&random[32..]),
                })
            })
            .as_ref()
            .map(|secret| Self { secret })
    }

    fn base(&self, identity: RequestIdentity, protocol: WireProtocol, event: &str) -> Value {
        let mut result = identity.fields();
        result["schema_version"] = json!(1);
        result["process_nonce"] = json!(self.secret.nonce);
        result["domain"] = json!(DOMAIN);
        result["protocol"] = json!(protocol.name());
        result["event"] = json!(event);
        result
    }

    pub(crate) fn capture(
        &self,
        body: &Value,
        headers: &HeaderMap,
        identity: RequestIdentity,
        protocol: WireProtocol,
    ) -> Value {
        let mut result = self.base(identity, protocol, "request");
        result["body"] = self.summary("body", body);
        result["config"] = self.summary("config", &Config(body));
        let input = match protocol {
            WireProtocol::Chat => body.get("messages"),
            WireProtocol::Responses => body.get("input"),
        };
        let (messages, count) = self.items(input, "message", MAX_ITEMS);
        result["messages"] = json!(messages);
        result["message_count"] = json!(count);
        result["messages_truncated"] = json!(count > MAX_ITEMS);
        // 数组总摘要始终覆盖全部条目；列表只限制可见元数据，绝不裁剪实际正文。
        result["input"] = self.optional_summary("input", input);
        let tools = body.get("tools");
        let (items, count) = self.items(tools, "tool", MAX_TOOL_ITEMS);
        result["tools"] = self.optional_summary("tools", tools);
        result["tools"]["items"] = json!(items);
        result["tools"]["count"] = json!(count);
        result["tools"]["truncated"] = json!(count > MAX_TOOL_ITEMS);
        result["identifiers"] = json!({
            "session_id":header_id(headers, "session-id"),
            "thread_id":header_id(headers, "thread-id"),
            "client_request_id":header_id(headers, "x-client-request-id"),
            "prompt_cache_key":match body.get("prompt_cache_key") {
                None | Some(Value::Null) => identifier(None),
                Some(Value::String(value)) => identifier(Some(value.as_bytes())),
                Some(_) => json!({"status":"invalid_type"}),
            },
        });
        result
    }

    pub(crate) fn response_headers(
        &self,
        headers: &HeaderMap,
        identity: RequestIdentity,
        protocol: WireProtocol,
    ) -> Value {
        let mut result = self.base(identity, protocol, "response_headers");
        result["upstream_request_id"] = header_id(headers, "x-request-id");
        result["oneapi_request_id"] = header_id(headers, "x-oneapi-request-id");
        result
    }

    pub(crate) fn usage(
        &self,
        usage: Option<eve_llm_api::ModelUsage>,
        identity: RequestIdentity,
        protocol: WireProtocol,
    ) -> Value {
        let mut result = self.base(identity, protocol, "usage");
        result["usage_reported"] = json!(usage.is_some());
        result["input_tokens"] = json!(usage.and_then(|value| value.input_tokens));
        result["cached_input_tokens"] = json!(usage.and_then(|value| value.cached_input_tokens));
        result["output_tokens"] = json!(usage.and_then(|value| value.output_tokens));
        result
    }

    /// 客户端配置的目标摘要，不证明实际后端节点或凭据路由。
    pub(crate) fn endpoint(&self, endpoint: &str) -> Value {
        self.summary("endpoint", endpoint)
    }

    fn items(&self, value: Option<&Value>, domain: &str, limit: usize) -> (Vec<Value>, usize) {
        match value {
            None | Some(Value::Null) => (Vec::new(), 0),
            Some(Value::Array(items)) => (
                items
                    .iter()
                    .take(limit)
                    .enumerate()
                    .map(|(index, item)| self.item(domain, index, item))
                    .collect(),
                items.len(),
            ),
            Some(value) => (vec![self.item(domain, 0, value)], 1),
        }
    }

    fn optional_summary(&self, domain: &str, value: Option<&Value>) -> Value {
        match value {
            None => json!({"status":"missing","bytes":0}),
            Some(value) => {
                let mut summary = self.summary(domain, value);
                summary["status"] = json!("present");
                summary
            }
        }
    }

    fn item(&self, domain: &str, index: usize, value: &Value) -> Value {
        let mut item = self.summary(domain, value);
        item["index"] = json!(index);
        item["role"] = json!(match value.get("role").and_then(Value::as_str) {
            Some(role @ ("system" | "developer" | "user" | "assistant" | "tool" | "function")) =>
                role,
            None => "absent",
            Some(_) => "other",
        });
        item["type"] = json!(match value.get("type").and_then(Value::as_str) {
            Some(
                kind @ ("message"
                | "function"
                | "function_call"
                | "function_call_output"
                | "reasoning"
                | "text"
                | "input_text"
                | "input_image"
                | "output_text"
                | "image_url"
                | "custom"
                | "custom_tool_call"
                | "custom_tool_call_output"),
            ) => kind,
            None => "absent",
            Some(_) => "other",
        });
        item
    }

    fn summary<T: Serialize + ?Sized>(&self, domain: &str, value: &T) -> Value {
        let mut context = hmac::Context::with_key(&self.secret.key);
        context.update(DOMAIN.as_bytes());
        context.update(&[0]);
        context.update(domain.as_bytes());
        context.update(&[0]);
        let mut writer = DigestWriter { context, bytes: 0 };
        // writer 不分配正文副本；错误也只输出固定类别，不格式化潜在私密值。
        if serde_json::to_writer(&mut writer, value).is_err() {
            return json!({"status":"serialization_failed"});
        }
        json!({"hmac_sha256":hex(writer.context.sign().as_ref()),"bytes":writer.bytes})
    }
}

struct DigestWriter {
    context: hmac::Context,
    bytes: usize,
}
impl io::Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.context.update(bytes);
        self.bytes = self.bytes.saturating_add(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 借用原对象按 serde_json 的实际键顺序编码；不克隆大字段或输出配置键名。
struct Config<'a>(&'a Value);
impl Serialize for Config<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        if let Some(object) = self.0.as_object() {
            for (key, value) in object {
                if !matches!(
                    key.as_str(),
                    "messages" | "input" | "tools" | "prompt_cache_key"
                ) {
                    map.serialize_entry(key, value)?;
                }
            }
        }
        map.end()
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 15) as usize] as char);
    }
    output
}

fn identifier(value: Option<&[u8]>) -> Value {
    match value {
        None => json!({"status":"missing","bytes":0}),
        Some(value) if value.len() > MAX_ID_BYTES => {
            json!({"status":"oversized","bytes":value.len()})
        }
        Some(value) => {
            json!({"status":"present","bytes":value.len(),"sha256":hex(digest(&SHA256, value).as_ref())})
        }
    }
}

fn header_id(headers: &HeaderMap, name: &str) -> Value {
    let mut values = headers.get_all(name).iter();
    let first = values.next();
    if values.next().is_some() {
        return json!({"status":"multiple"});
    }
    identifier(first.map(|value| value.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timing::CallDiagnostics;

    fn identity() -> RequestIdentity {
        CallDiagnostics::start(Some("dialogue")).next_attempt()
    }
    fn body() -> Value {
        json!({
            "model":"private-model-name", "private-config-key":"https://private.invalid/api",
            "messages":[{"role":"system","content":"private instructions"},{"role":"user","content":"private user sentence"}],
            "tools":[{"type":"function","function":{"name":"private_tool_name","parameters":{"secret_argument":"secret-value"}}}],
            "prompt_cache_key":"opaque-private-cache-id",
        })
    }
    #[test]
    fn metadata_contains_no_body_tool_config_urls_or_credentials() {
        let diagnostics = WireDiagnostics::new().unwrap();
        let body = body();
        let before = body.clone();
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            "Bearer sensitive-credential".parse().unwrap(),
        );
        headers.insert("session-id", "opaque-private-session-id".parse().unwrap());
        headers.insert(
            "unrelated-header",
            "private-header-content".parse().unwrap(),
        );
        let value = diagnostics.capture(&body, &headers, identity(), WireProtocol::Chat);
        let text = value.to_string();
        for private in [
            "private",
            "sensitive-credential",
            "secret_argument",
            "secret-value",
            "Bearer",
            "https://",
            "authorization",
        ] {
            assert!(!text.contains(private), "{private}");
        }
        assert_eq!(body, before);
        assert_eq!(
            value["body"]["bytes"],
            serde_json::to_vec(&body).unwrap().len()
        );
        assert_eq!(
            value["identifiers"]["session_id"]["sha256"],
            hex(digest(&SHA256, b"opaque-private-session-id").as_ref())
        );
        assert_eq!(value["body"]["hmac_sha256"].as_str().unwrap().len(), 64);
        assert_ne!(
            value["body"]["hmac_sha256"],
            hex(digest(&SHA256, &serde_json::to_vec(&body).unwrap()).as_ref())
        );
    }
    #[test]
    fn final_body_append_keeps_prefix_but_early_edit_changes_its_digest() {
        let a = WireDiagnostics::new().unwrap();
        let b = WireDiagnostics::new().unwrap();
        let headers = HeaderMap::new();
        let mut body = body();
        let before = a.capture(&body, &headers, identity(), WireProtocol::Chat);
        body["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"assistant","content":"later reply"}));
        let appended = b.capture(&body, &headers, identity(), WireProtocol::Chat);
        assert_eq!(before["process_nonce"], appended["process_nonce"]);
        assert_eq!(before["messages"][0], appended["messages"][0]);
        assert_eq!(before["messages"][1], appended["messages"][1]);
        assert_ne!(before["body"], appended["body"]);
        assert_eq!(before["config"], appended["config"]);
        body["messages"][0]["content"] = json!("changed early instructions");
        let changed = a.capture(&body, &headers, identity(), WireProtocol::Chat);
        assert_ne!(changed["messages"][0], appended["messages"][0]);
        assert_eq!(changed["messages"][1], appended["messages"][1]);
    }
    #[test]
    fn tool_and_config_changes_are_distinguished_without_field_projection() {
        let diagnostics = WireDiagnostics::new().unwrap();
        let headers = HeaderMap::new();
        let mut body = body();
        let before = diagnostics.capture(&body, &headers, identity(), WireProtocol::Chat);
        body["tools"][0]["function"]["parameters"] = json!({"new-private-field":"new-value"});
        let tools = diagnostics.capture(&body, &headers, identity(), WireProtocol::Chat);
        assert_ne!(before["tools"], tools["tools"]);
        assert_eq!(before["config"], tools["config"]);
        assert_eq!(before["messages"], tools["messages"]);
        body["reasoning_effort"] = json!("high");
        let config = diagnostics.capture(&body, &headers, identity(), WireProtocol::Chat);
        assert_ne!(tools["config"], config["config"]);
        assert_eq!(tools["tools"], config["tools"]);
    }
    #[test]
    fn response_and_request_identifiers_are_bounded_and_allowlisted() {
        let diagnostics = WireDiagnostics::new().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", "x".repeat(MAX_ID_BYTES).parse().unwrap());
        headers.insert("set-cookie", "private cookie".parse().unwrap());
        let exact = diagnostics.response_headers(&headers, identity(), WireProtocol::Responses);
        assert_eq!(exact["upstream_request_id"]["status"], "present");
        headers.insert(
            "x-request-id",
            "x".repeat(MAX_ID_BYTES + 1).parse().unwrap(),
        );
        let oversized = diagnostics.response_headers(&headers, identity(), WireProtocol::Responses);
        assert_eq!(oversized["upstream_request_id"]["status"], "oversized");
        assert!(oversized["upstream_request_id"].get("sha256").is_none());
        assert!(!oversized.to_string().contains("cookie"));
        headers.append("x-request-id", "second-value".parse().unwrap());
        assert_eq!(
            diagnostics.response_headers(&headers, identity(), WireProtocol::Responses)["upstream_request_id"]
                ["status"],
            "multiple"
        );
        assert_eq!(
            diagnostics.response_headers(&HeaderMap::new(), identity(), WireProtocol::Chat)["upstream_request_id"]
                ["status"],
            "missing"
        );
    }
    #[test]
    fn responses_scalar_input_and_unknown_types_never_echo_values() {
        let diagnostics = WireDiagnostics::new().unwrap();
        let value = diagnostics.capture(
            &json!({"input":"private scalar prompt"}),
            &HeaderMap::new(),
            identity(),
            WireProtocol::Responses,
        );
        assert_eq!(value["message_count"], 1);
        assert_eq!(value["messages"][0]["role"], "absent");
        let value = diagnostics.capture(
            &json!({"input":[{"role":"private-role","type":"private-type","content":"private"}]}),
            &HeaderMap::new(),
            identity(),
            WireProtocol::Responses,
        );
        assert_eq!(value["messages"][0]["role"], "other");
        assert_eq!(value["messages"][0]["type"], "other");
        assert!(!value.to_string().contains("private"));
    }

    #[test]
    fn reported_usage_keeps_missing_distinct_from_zero() {
        let diagnostics = WireDiagnostics::new().unwrap();
        let missing = diagnostics.usage(None, identity(), WireProtocol::Chat);
        assert_eq!(missing["usage_reported"], false);
        assert!(missing["cached_input_tokens"].is_null());
        let partial = diagnostics.usage(
            Some(eve_llm_api::ModelUsage {
                input_tokens: Some(19),
                cached_input_tokens: None,
                output_tokens: Some(0),
            }),
            identity(),
            WireProtocol::Responses,
        );
        assert_eq!(partial["usage_reported"], true);
        assert!(partial["cached_input_tokens"].is_null());
        assert_eq!(partial["output_tokens"], 0);
        let zero = diagnostics.usage(
            Some(eve_llm_api::ModelUsage {
                input_tokens: Some(19),
                cached_input_tokens: Some(0),
                output_tokens: None,
            }),
            identity(),
            WireProtocol::Responses,
        );
        assert_eq!(zero["cached_input_tokens"], 0);
        assert!(zero["output_tokens"].is_null());
        assert_eq!(zero["event"], "usage");
        assert_eq!(zero["process_nonce"], missing["process_nonce"]);
    }

    #[test]
    fn destination_digest_separates_endpoints_without_exposing_urls() {
        let diagnostics = WireDiagnostics::new().unwrap();
        let other_instance = WireDiagnostics::new().unwrap();
        let endpoint = "https://private-relay.invalid/v1/chat/completions";
        let first = diagnostics.endpoint(endpoint);
        assert_eq!(first, other_instance.endpoint(endpoint));
        let changed =
            diagnostics.endpoint("https://other-private-relay.invalid/v1/chat/completions");
        assert_ne!(first["hmac_sha256"], changed["hmac_sha256"]);
        assert_eq!(first["bytes"], serde_json::to_vec(endpoint).unwrap().len());
        let serialized = first.to_string();
        assert!(!serialized.contains("private-relay"));
        assert!(!serialized.contains("https://"));
        assert!(!serialized.contains("completions"));
    }
}
