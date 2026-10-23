//! 合成历史方法示例 + 真实模型独立目标 + 实际 Mindustry 验证，不连接 QQ。
use eve_llm_api::{
    LlmError, LlmFuture, LlmModelResolver, LlmProvider, ModelRequest, ModelResponse, ModelSelection,
};
use eve_llm_openai::{OpenAiConfig, OpenAiProvider};
use eve_practice_api::*;
use eve_practice_mindustry::{MindustryServerRunner, RuntimeCommand};
use eve_practice_plugin::{DRAFTER_VERSION, ModelPracticeDrafter};
use std::{sync::Arc, time::Duration};

struct Resolver(ModelSelection);
impl LlmModelResolver for Resolver {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        Ok(self.0.clone())
    }
}

struct ProbeProvider(OpenAiProvider);
impl LlmProvider for ProbeProvider {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        self.complete_json(request)
    }
    fn complete_json(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async move {
            let response = self.0.complete_json(request).await?;
            if let ModelResponse::Final { text } = &response
                && let Ok(path) = std::env::var("EVE_STRATEGY_PROBE_OUTPUT")
            {
                // 仅合成输入的本地诊断；不接收真实聊天内容。
                let _ = std::fs::write(path, text);
            }
            Ok(response)
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let model = std::env::var("EVE_OPENAI_MODEL")?;
    let responses = std::env::var("EVE_OPENAI_PROTOCOL").as_deref() == Ok("responses");
    let mut config = if responses {
        OpenAiConfig::new(&model)
    } else {
        OpenAiConfig::chat(&model)
    }
    .with_base_url(&std::env::var("EVE_OPENAI_BASE_URL")?)?;
    config.max_output_tokens = Some(1800);
    config.request_timeout = Duration::from_secs(45);
    if !responses {
        config.reasoning_effort = Some("none".into());
    }
    let mut provider = OpenAiProvider::new(config, std::env::var("EVE_OPENAI_API_KEY")?)?;
    if std::env::var("EVE_OPENAI_STREAM_COMPLETIONS").is_ok_and(|s| s == "true" || s == "1") {
        provider = provider.with_streamed_completions();
    }
    let drafter = ModelPracticeDrafter::new(Arc::new(Resolver(ModelSelection {
        provider: Arc::new(ProbeProvider(provider)),
        provider_timeout: Duration::from_secs(45),
    })));
    let runner = MindustryServerRunner::new(
        RuntimeCommand {
            program: "java".into(),
            prefix_args: vec![],
        },
        std::env::var("EVE_MINDUSTRY_SERVER_JAR")?.into(),
    )?;
    // 公开夹具，不伪称为本次真实模型先前做出的作品或真实用户经历。
    let example=PracticeDraft { applicable:true,files:vec![
        ArtifactFile {path:"mod.hjson".into(),content:"name: eve-strategy-history\nversion: 1\nminGameVersion: 146\n".into()},
        ArtifactFile {path:"content/blocks/history-wall.hjson".into(),content:"type: Wall\nhealth: 520\nsize: 1\nrequirements: [ copper/6 ]\ncategory: defense\n".into()}
    ],probes:vec![Probe {subject:"eve-strategy-history-history-wall".into(),property:"health".into(),expected:"520".into()}],rationale:"合成历史修正实例".into(),notes_used:vec![]};
    for health in [700, 900] {
        let task = PracticeTask {
            goal_id: format!("holdout-{health}"),
            goal_revision: 1,
            owner: "synthetic-owner".into(),
            brief: format!(
                "为 Mindustry 制作一个独特名称的最小纯数据方块模组，只含一个 Wall 方块，生命值必须是 {health}。探测必须包含 health={health}，并检查 class=Wall；有 requirements 和 category。不要照搬旧实例的模组名、方块名或生命值。"
            ),
            brief_truncated: false,
            notes: vec![],
        };
        let draft = drafter
            .draft(DraftRequest {
                run_id: run_id(&task.goal_id, 1),
                drafter_version: DRAFTER_VERSION.into(),
                attempt: 1,
                task,
                runner: runner.profile().clone(),
                previous: None,
                guidance: Some(PracticeGuidance {
                    version: 1,
                    source_run: run_id("synthetic-history", 1),
                    source_sha256: strategy_digest(&example),
                    previous_issues: vec!["No type 'Walll' found".into()],
                    corrected_example: example.clone(),
                }),
            })
            .await?;
        if !runner.check(&draft).is_empty() {
            return Err("真实模型产物结构未通过".into());
        }
        let work = tempfile::tempdir()?;
        let evidence = runner.run(&draft, work.path()).await;
        let adapted = draft
            .probes
            .iter()
            .any(|p| p.property == "health" && p.expected == health.to_string());
        let passed = adapted && evidence.verified(&draft);
        println!(
            "{}",
            serde_json::json!({"health":health,"success":passed,"runtime":evidence.runtime_version,"verified":evidence.verified(&draft),"adapted":adapted,"warnings":evidence.warnings})
        );
        if !passed {
            return Err("合成方法在真实模型和真实运行器中未通过独立目标验证".into());
        }
    }
    Ok(())
}
