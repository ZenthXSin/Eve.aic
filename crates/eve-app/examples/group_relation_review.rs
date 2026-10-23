//! 用合成问答验收独立经验整理，不连接 QQ 或写入运行数据。
use eve_chime_api::*;
use eve_chime_plugin::ModelGroupRelationReviewer;
use eve_llm_api::{LlmError, LlmModelResolver, ModelSelection};
use eve_llm_openai::{OpenAiConfig, OpenAiProvider};
use std::{sync::Arc, time::Duration};
struct Resolver(ModelSelection);
impl LlmModelResolver for Resolver {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        Ok(self.0.clone())
    }
}
fn message(id: &str, text: &str, at: u64, me: bool) -> GroupMessage {
    GroupMessage {
        id: id.into(),
        conversation: "synthetic-group".into(),
        speaker: if me { "eve" } else { "member" }.into(),
        speaker_name: None,
        text: text.into(),
        mentions_me: false,
        replies_to_me: false,
        from_me: me,
        at_ms: at,
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let model = std::env::var("EVE_OPENAI_MODEL")?;
    let mut config = if std::env::var("EVE_OPENAI_PROTOCOL").as_deref() == Ok("responses") {
        OpenAiConfig::new(&model)
    } else {
        OpenAiConfig::chat(&model)
    }
    .with_base_url(&std::env::var("EVE_OPENAI_BASE_URL")?)?;
    config.max_output_tokens = Some(800);
    if std::env::var("EVE_OPENAI_PROTOCOL").as_deref() != Ok("responses") {
        config.reasoning_effort = Some("none".into());
    }
    config.request_timeout = Duration::from_secs(15);
    let mut provider = OpenAiProvider::new(config, std::env::var("EVE_OPENAI_API_KEY")?)?;
    if std::env::var("EVE_OPENAI_STREAM_COMPLETIONS").is_ok_and(|s| s == "true" || s == "1") {
        provider = provider.with_streamed_completions();
    }
    let reviewer = ModelGroupRelationReviewer::new(Arc::new(Resolver(ModelSelection {
        provider: Arc::new(provider),
        provider_timeout: Duration::from_secs(15),
    })));
    let input = GroupRelationReview {
        input: GroupRelationInput {
            message: message("new", "那绑定用哪条指令", 3, false),
            exchanges: vec![GroupExchange {
                source: message("old", "Mindustry 的逻辑处理器怎么控制单位挖矿", 1, false),
                reply: message("answer", "先绑定单位，再发送采矿指令", 2, true),
            }],
            examples: vec![],
        },
        subsequent: vec![message(
            "proof",
            "我刚才是在追问你上面说的单位绑定指令",
            4,
            false,
        )],
    };
    let result = reviewer.review(input.clone()).await?;
    let passed = result.decision.validate(&input.input)
        && result.decision.kind == GroupRelationKind::Related
        && result.decision.needs_reply
        && result.decision.confidence >= 85
        && result.decision.topic.as_deref() == Some("old")
        && result.evidence.iter().any(|id| id == "proof");
    println!(
        "{}",
        serde_json::json!({"success":passed,"decision":result.decision,"evidence":result.evidence})
    );
    if !passed {
        return Err("合成经验整理未通过".into());
    }
    Ok(())
}
