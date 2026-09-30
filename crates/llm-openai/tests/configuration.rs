mod support;

use eve_llm_api::{ChatMessage, ChatRole, LlmError, LlmProvider, ModelRequest};
use eve_llm_openai::{OpenAiConfig, OpenAiProvider};
use serde_json::json;
use support::{Reply, Server, final_response};

#[test]
fn base_addresses_normalize_without_duplicating_responses() {
    for (base, expected) in [
        ("https://example.com", "https://example.com/v1/responses"),
        (
            "https://example.com/v1/",
            "https://example.com/v1/responses",
        ),
        (
            "https://example.com/proxy/v1",
            "https://example.com/proxy/v1/responses",
        ),
        (
            "https://example.com/v1/responses",
            "https://example.com/v1/responses",
        ),
    ] {
        let config = OpenAiConfig::new("model").with_base_url(base).unwrap();
        assert_eq!(config.responses_url, expected);
        assert!(OpenAiProvider::new(config, "fixture-key").is_ok());
    }
    for base in [
        "http://example.com",
        "https://user:secret@example.com",
        "https://example.com?key=secret",
    ] {
        let config = OpenAiConfig::new("model").with_base_url(base).unwrap();
        let error = OpenAiProvider::new(config, "fixture-key").err().unwrap();
        assert!(!error.to_string().contains("secret"));
    }
}

#[test]
fn invalid_optional_parameters_are_rejected_before_network() {
    for config in [
        OpenAiConfig {
            max_output_tokens: Some(0),
            ..OpenAiConfig::new("model")
        },
        OpenAiConfig {
            reasoning_effort: Some("invalid-secret".into()),
            ..OpenAiConfig::new("model")
        },
    ] {
        let error = OpenAiProvider::new(config, "fixture-key").err().unwrap();
        assert!(matches!(error, LlmError::Configuration(_)));
        assert!(!error.to_string().contains("secret"));
    }
}

#[tokio::test]
async fn explicit_model_options_are_sent_and_defaults_are_omitted() {
    let mut server = Server::start(vec![
        Reply::json(final_response("ok")),
        Reply::json(final_response("ok")),
    ])
    .await;
    let request = || ModelRequest {
        messages: vec![ChatMessage::text(ChatRole::User, "test")],
        tools: vec![],
    };
    let mut config = OpenAiConfig::new("fixture-model");
    config.responses_url = server.url.clone();
    let default = OpenAiProvider::new(config.clone(), "fixture-key").unwrap();
    default.complete(request()).await.unwrap();
    let captured = server.next().await;
    assert!(
        captured
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer fixture-key")
    );
    let body = captured.body;
    assert!(body.get("reasoning").is_none());
    assert!(body.get("max_output_tokens").is_none());
    config.reasoning_effort = Some("none".into());
    config.max_output_tokens = Some(512);
    let explicit = OpenAiProvider::new(config, "fixture-key").unwrap();
    explicit.complete(request()).await.unwrap();
    let body = server.next().await.body;
    assert_eq!(body["reasoning"], json!({"effort":"none"}));
    assert_eq!(body["max_output_tokens"], 512);
    assert_eq!(body["store"], false);
    assert!(!body.to_string().contains("fixture-key"));
}
