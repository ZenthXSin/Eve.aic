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
    use serde::{
        Deserializer,
        de::{IgnoredAny, MapAccess, Visitor},
    };
    // 只分配usage；其余字段验证JSON后丢弃。重复usage沿旧Value解析器取最后一项。
    struct UsageVisitor;
    impl<'de> Visitor<'de> for UsageVisitor {
        type Value = Value;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("response object")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Value, M::Error> {
            let mut usage = Value::Null;
            while let Some(key) = map.next_key::<String>()? {
                if key == "usage" {
                    usage = map.next_value()?;
                } else {
                    map.next_value::<IgnoredAny>()?;
                }
            }
            Ok(usage)
        }
    }
    let mut parser = serde_json::Deserializer::from_slice(bytes);
    let usage = parser.deserialize_map(UsageVisitor).ok()?;
    parser.end().ok()?;
    from_usage(&usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 独立进程运行，测量大正文只提取usage时的额外RSS；不连接网络。
    #[test]
    #[ignore = "standalone process RSS profile"]
    fn large_response_usage_memory_profile() {
        let mut body = Vec::with_capacity(6 * 1024 * 1024 + 128);
        body.extend_from_slice(br#"{"choices":[{"message":{"content":""#);
        body.resize(body.len() + 6 * 1024 * 1024, b'x');
        body.extend_from_slice(br#""}}],"usage":{"prompt_tokens":4096,"completion_tokens":128}}"#);
        let snapshot = || {
            std::fs::read_to_string("/proc/self/status").ok().map(|s| {
                s.lines()
                    .filter(|l| l.starts_with("VmRSS:") || l.starts_with("VmHWM:"))
                    .collect::<Vec<_>>()
                    .join("; ")
            })
        };
        println!("before={:?}", snapshot());
        let usage = from_body(&body).unwrap();
        println!("after={:?}; body_bytes={}", snapshot(), body.len());
        assert_eq!(usage.input_tokens, Some(4096));
        assert_eq!(usage.output_tokens, Some(128));
    }

    #[test]
    fn usage_extraction_ignores_non_usage_fields_but_rejects_invalid_json() {
        for body in [
            br#"{"choices":[{"nested":[1,true,null,{"text":"original"}]}],"usage":{"prompt_tokens":3,"extra":"provider field"}}"#.as_slice(),
            br#"{"output":{"details":[false]},"usage":{"input_tokens":3}}"#.as_slice(),
        ] {
            assert_eq!(from_body(body).unwrap().input_tokens, Some(3));
        }
        assert!(from_body(br#"{"usage":{"prompt_tokens":3},"output":[invalid]}"#).is_none());
        assert!(from_body(br#"{"choices":[],"usage":null}"#).is_none());
    }

    #[test]
    fn usage_body_matches_original_value_parser_including_duplicate_keys() {
        let mut cases = vec![
            r#"{"usage":{"prompt_tokens":1},"usage":{"prompt_tokens":9}}"#.to_owned(),
            r#"{"usage":false,"usage":{"input_tokens":7}}"#.to_owned(),
            r#"{"usage":{"prompt_tokens":1},"usage":null}"#.to_owned(),
            r#"{"usage":{"prompt_tokens":1},"usage":false}"#.to_owned(),
            r#"{"usage":false}"#.to_owned(),
            r#"{"usage":null}"#.to_owned(),
            r#"{"usage":{"input_tokens":1,"input_tokens":5}}"#.to_owned(),
            r#"{"other":1,"other":2,"usage":{"output_tokens":8}}"#.to_owned(),
            "null".to_owned(),
            "[]".to_owned(),
            "123".to_owned(),
        ];
        cases.push(format!(
            r#"{{"unknown":"{}","usage":{{"prompt_tokens":3}}}}"#,
            "x".repeat(1024 * 1024)
        ));
        cases.push(format!(
            r#"{{"usage":{{"prompt_tokens":3}},"unknown":["{}", invalid]}}"#,
            "x".repeat(1024 * 1024)
        ));
        for body in cases {
            let original = serde_json::from_slice::<Value>(body.as_bytes())
                .ok()
                .and_then(|v| v.get("usage").and_then(from_usage));
            assert_eq!(
                from_body(body.as_bytes()),
                original,
                "body prefix: {}",
                &body[..body.len().min(100)]
            );
        }
    }

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
