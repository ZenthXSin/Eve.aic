//! 与真实 smoke test 共用流程，HTTP 后端仅监听 loopback，使用虚构凭据。
use eve_llm_openai::{OpenAiConfig, OpenAiProvider};
use serde_json::json;
use std::sync::Arc;

#[path = "../../llm-openai/tests/support/mod.rs"]
mod http_support;
#[path = "support/openai_smoke.rs"]
mod openai_smoke;
use http_support::{Reply, Server, final_response};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut server = Server::start(vec![
        Reply::json(final_response("你好，Eve")),
        Reply::json(json!({"status":"completed", "error":null, "output":[
            {"type":"function_call", "call_id":"offline-call", "name":"echo", "arguments":"{\"text\":\"你好，Eve.aic\"}", "status":"completed"}
        ]})),
        Reply::json(final_response("你好，Eve.aic")),
    ]).await;
    let provider = Arc::new(OpenAiProvider::new(
        OpenAiConfig {
            responses_url: server.url.clone(),
            ..OpenAiConfig::new("fixture-model")
        },
        "fake-key",
    )?);
    openai_smoke::run(provider).await?;
    let text = server.next().await;
    let first = server.next().await;
    let second = server.next().await;
    assert!(text.headers.starts_with("POST /v1/responses HTTP/1.1"));
    assert!(
        first
            .headers
            .to_lowercase()
            .contains("authorization: bearer fake-key")
    );
    assert_eq!(first.body["tools"][0]["name"], "echo");
    assert_eq!(first.body["tools"], second.body["tools"]);
    assert_eq!(first.body["store"], false);
    let prefix = first.body["input"].as_array().unwrap();
    let messages = second.body["input"].as_array().unwrap();
    assert_eq!(prefix, &messages[..prefix.len()]);
    assert_eq!(messages[prefix.len()]["call_id"], "offline-call");
    assert_eq!(messages[prefix.len() + 1]["call_id"], "offline-call");
    let result: serde_json::Value =
        serde_json::from_str(messages[prefix.len() + 1]["output"].as_str().unwrap())?;
    assert_eq!(result, json!({"echo":"你好，Eve.aic"}));
    assert!(server.requests.try_recv().is_err());
    println!("离线 HTTP 验收：3 次请求；调用与结果 ID、顺序、上下文前缀一致。");
    Ok(())
}
