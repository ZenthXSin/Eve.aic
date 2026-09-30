//! 手动真实 smoke test。只有宿主示例读取环境变量；Provider 不读取配置文件或环境。
use eve_llm_api::LlmError;
use eve_llm_openai::{OpenAiConfig, OpenAiProvider};
use std::sync::Arc;

#[path = "support/openai_smoke.rs"]
mod openai_smoke;

fn required_env(name: &str) -> Result<String, LlmError> {
    std::env::var(name).map_err(|_| LlmError::Configuration(format!("请在宿主环境中提供 {name}")))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let model = required_env("OPENAI_MODEL")?;
    let key = required_env("OPENAI_API_KEY")?;
    let mut config = OpenAiConfig::new(model);
    if let Ok(url) = std::env::var("EVE_OPENAI_RESPONSES_URL") {
        config.responses_url = url;
    }
    let provider = Arc::new(OpenAiProvider::new(config, key)?);
    openai_smoke::run(provider).await
}
