//! 从 OpenAI 兼容响应里读取输入用量与缓存命中；不同服务字段名不同，缺失的字段保持 None。
use eve_llm_api::ModelUsage;
use serde_json::Value;

fn number(value: &Value, path: &[&str]) -> Option<u64> {
    path.iter()
        .try_fold(value, |value, key| value.get(key))
        .and_then(Value::as_u64)
}

/// `usage` 对象：Chat 用 prompt/completion，Responses 用 input/output；
/// 缓存命中依次取 prompt_tokens_details.cached_tokens、input_tokens_details.cached_tokens
/// 与 DeepSeek 的 prompt_cache_hit_tokens。
pub(crate) fn from_usage(usage: &Value) -> Option<ModelUsage> {
    if !usage.is_object() {
        return None;
    }
    let usage = ModelUsage {
        input_tokens: number(usage, &["prompt_tokens"]).or(number(usage, &["input_tokens"])),
        cached_input_tokens: number(usage, &["prompt_tokens_details", "cached_tokens"])
            .or(number(usage, &["input_tokens_details", "cached_tokens"]))
            .or(number(usage, &["prompt_cache_hit_tokens"])),
        output_tokens: number(usage, &["completion_tokens"]).or(number(usage, &["output_tokens"])),
    };
    (!usage.is_empty()).then_some(usage)
}

/// 完整（非流式）响应体里的用量。
pub(crate) fn from_body(bytes: &[u8]) -> Option<ModelUsage> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    from_usage(value.get("usage")?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_chat_responses_and_deepseek_usage_fields() {
        let chat = from_usage(&json!({"prompt_tokens":1000,"completion_tokens":20,
            "prompt_tokens_details":{"cached_tokens":768}}))
        .unwrap();
        assert_eq!(
            (
                chat.input_tokens,
                chat.cached_input_tokens,
                chat.output_tokens
            ),
            (Some(1000), Some(768), Some(20))
        );
        let responses = from_usage(&json!({"input_tokens":500,"output_tokens":9,
            "input_tokens_details":{"cached_tokens":256}}))
        .unwrap();
        assert_eq!(responses.cached_input_tokens, Some(256));
        let deepseek =
            from_usage(&json!({"prompt_tokens":300,"prompt_cache_hit_tokens":128})).unwrap();
        assert_eq!(deepseek.cached_input_tokens, Some(128));
        let unreported = from_usage(&json!({"prompt_tokens":10})).unwrap();
        assert_eq!(unreported.cached_input_tokens, None);
        assert!(from_usage(&json!({})).is_none());
        assert!(from_body(b"{\"usage\":null}").is_none());
        assert_eq!(
            from_body(br#"{"choices":[],"usage":{"prompt_tokens":3}}"#)
                .unwrap()
                .input_tokens,
            Some(3)
        );
    }
}
