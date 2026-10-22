//! 识图的独立多模态请求体。不进入主对话的 ChatMessage，也不把附件 URL 交给模型。
use crate::OpenAiProtocol;
use eve_llm_api::LlmError;
use serde_json::{Value, json};

/// 用户没有具体问题时使用的客观描述提示。
pub const DEFAULT_VISION_PROMPT: &str = "客观描述图片里看得见的内容，不要编造";

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut index = 0;
    while index + 3 <= bytes.len() {
        let n = u32::from_be_bytes([0, bytes[index], bytes[index + 1], bytes[index + 2]]);
        for shift in [18, 12, 6, 0] {
            out.push(B64[((n >> shift) & 0x3f) as usize] as char);
        }
        index += 3;
    }
    let rest = bytes.len() - index;
    if rest == 1 {
        let n = u32::from(bytes[index]) << 16;
        out.push(B64[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64[((n >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rest == 2 {
        let n = (u32::from(bytes[index]) << 16) | (u32::from(bytes[index + 1]) << 8);
        out.push(B64[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64[((n >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }
    out
}

fn data_url(media_type: &str, image: &[u8]) -> Result<String, LlmError> {
    if !media_type.starts_with("image/")
        || media_type[6..].is_empty()
        || media_type
            .bytes()
            .any(|b| !b.is_ascii() || b.is_ascii_whitespace())
    {
        return Err(LlmError::Protocol("识图只接受 image/* 类型".into()));
    }
    if image.is_empty() {
        return Err(LlmError::Protocol("图片内容为空".into()));
    }
    Ok(format!("data:{media_type};base64,{}", base64(image)))
}

/// 按主模型协议构造一次识图请求。`model` 原样写入，不在这里选择模型。
pub fn encode_vision_request(
    model: &str,
    protocol: OpenAiProtocol,
    prompt: &str,
    media_type: &str,
    image: &[u8],
    reasoning_effort: Option<&str>,
    max_output_tokens: Option<u32>,
) -> Result<Value, LlmError> {
    let prompt = prompt.trim();
    if model.trim().is_empty() || prompt.is_empty() {
        return Err(LlmError::Protocol("识图模型和提示不能为空".into()));
    }
    let url = data_url(media_type, image)?;
    let mut body = match protocol {
        OpenAiProtocol::ChatCompletions => json!({
            "model": model,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": prompt},
                    {"type": "image_url", "image_url": {"url": url}}
                ]
            }],
            "stream": false
        }),
        OpenAiProtocol::Responses => json!({
            "model": model,
            "input": [{
                "role": "user",
                "content": [
                    {"type": "input_text", "text": prompt},
                    {"type": "input_image", "image_url": url}
                ]
            }],
            "tools": [],
            "parallel_tool_calls": true,
            "store": false,
            "stream": false
        }),
    };
    if let Some(effort) = reasoning_effort {
        match protocol {
            OpenAiProtocol::Responses => {
                body["reasoning"] = json!({"effort": effort});
            }
            OpenAiProtocol::ChatCompletions => {
                body["reasoning_effort"] = json!(effort);
            }
        }
    }
    if let Some(limit) = max_output_tokens {
        let field = match protocol {
            OpenAiProtocol::Responses => "max_output_tokens",
            OpenAiProtocol::ChatCompletions => "max_tokens",
        };
        body[field] = json!(limit);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_body_uses_the_supplied_model_and_embeds_bytes() {
        let body = encode_vision_request(
            "primary-model-name",
            OpenAiProtocol::ChatCompletions,
            "图里有什么",
            "image/png",
            b"png-bytes",
            Some("none"),
            Some(128),
        )
        .unwrap();
        assert_eq!(body["model"], "primary-model-name");
        assert_eq!(body["stream"], false);
        assert_eq!(body["reasoning_effort"], "none");
        assert_eq!(body["max_tokens"], 128);
        assert!(body.get("max_output_tokens").is_none());
        let content = &body["messages"][0]["content"];
        assert_eq!(content[0]["text"], "图里有什么");
        let url = content[1]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
        assert!(!url.contains("http"));
        assert_eq!(
            url,
            format!("data:image/png;base64,{}", base64(b"png-bytes"))
        );
    }

    #[test]
    fn responses_body_uses_input_image_and_the_same_model_field() {
        let body = encode_vision_request(
            "role-model",
            OpenAiProtocol::Responses,
            DEFAULT_VISION_PROMPT,
            "image/jpeg",
            &[0, 1, 2],
            Some("low"),
            Some(64),
        )
        .unwrap();
        assert_eq!(body["model"], "role-model");
        assert_eq!(body["store"], false);
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["max_output_tokens"], 64);
        assert!(body.get("messages").is_none());
        let content = &body["input"][0]["content"];
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[1]["type"], "input_image");
        let url = content[1]["image_url"].as_str().unwrap();
        assert!(url.starts_with("data:image/jpeg;base64,"));
        assert!(!url.contains("https://"));
    }

    #[test]
    fn rejects_non_image_media_and_empty_prompt() {
        assert!(
            encode_vision_request(
                "m",
                OpenAiProtocol::ChatCompletions,
                "问",
                "text/plain",
                b"abc",
                None,
                None
            )
            .is_err()
        );
        assert!(
            encode_vision_request(
                "m",
                OpenAiProtocol::Responses,
                "  ",
                "image/png",
                b"abc",
                None,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn base64_round_trips_lengths() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
    }
}
