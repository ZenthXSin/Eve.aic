//! 合成情景的摘要持久化/恢复示例；--validate-response 仅核对公开模型探测结果的契约。
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::{
    LlmFuture, LlmModelResolver, LlmProvider, ModelRequest, ModelResponse, ModelSelection,
};
use eve_memory_api::*;
use eve_memory_plugin::{ConversationSummaryPlugin, ModelConversationSummarizer};
use eve_plugin_api::PluginId;
use serde::Deserialize;
use std::{error::Error, path::Path, sync::Arc, time::Duration};

struct Reply(String);
impl LlmProvider for Reply {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async move {
            assert!(request.tools.is_empty());
            Ok(ModelResponse::Final {
                text: self.0.clone(),
            })
        })
    }
}
struct Resolver(String);
impl LlmModelResolver for Resolver {
    fn resolve(&self) -> Result<ModelSelection, eve_llm_api::LlmError> {
        Ok(ModelSelection {
            provider: Arc::new(Reply(self.0.clone())),
            provider_timeout: Duration::from_secs(1),
        })
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Probe {
    batch: SummaryBatch,
    response: String,
}
fn extractor(reply: String) -> ModelConversationSummarizer {
    ModelConversationSummarizer::new(Arc::new(Resolver(reply)))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|s| s == "--validate-response") && args.len() == 2 {
        let input = std::fs::read(&args[1])?;
        if input.len() > 256 * 1024 {
            return Err("probe too large".into());
        }
        let probe: Probe = serde_json::from_slice(&input)?;
        let points = extractor(probe.response)
            .summarize(probe.batch)
            .await
            .map_err(|_| "summary output rejected")?;
        println!(
            "{}",
            serde_json::json!({"contract_valid":true,"points":points.len(),"semantic_acceptance":"requires_review"})
        );
        return Ok(());
    }
    if args.len() != 1 {
        return Err("用法：summary 状态目录，或 summary --validate-response 合成探测JSON".into());
    }
    let state = Arc::new(FileStateStore::open(Path::new(&args[0]))?);
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let plugin = ConversationSummaryPlugin::new()?;
    let admin = plugin.controller();
    kernel.register(Box::new(plugin))?;
    kernel.start(&PluginId::new(SUMMARY_PLUGIN_ID)?).await?;
    let scope = MemoryScope {
        channel: "example".into(),
        session_id: "public".into(),
        user_id: "synthetic".into(),
    };
    let snapshot = MemorySnapshot {
        scope: scope.clone(),
        revision: 1,
        preferences: vec![],
        evidence: vec![InteractionEvidence {
            id: "public-e1".into(),
            revision: 1,
            at_ms: 1,
            source: EvidenceSource::CompletedInteraction {
                message_id: "public-message".into(),
                session_revision: 2,
                turn_id: 1,
                user_text: "我喜欢 Mindustry 的模组，但不知道怎么创作".into(),
                assistant_text: "旧助手未核实的自述，不进入摘要输入".into(),
            },
        }],
        archived_evidence: 0,
    };
    let model = extractor(r#"{"points":[{"text":"用户对 Mindustry 模组创作感兴趣，希望了解入门方法","quotes":[{"evidence_id":"public-e1","text":"我喜欢 Mindustry 的模组，但不知道怎么创作"}]}]}"#.into());
    let mut calls = 0;
    if let Some(batch) = admin.reserve(
        &snapshot,
        100,
        model.version(),
        SummaryOptions {
            min_new_evidence: 1,
            ..SummaryOptions::default()
        },
    )? {
        calls += 1;
        let result = model.summarize(batch.clone()).await;
        admin.finish(&batch, 101, result)?;
    }
    let jobs = admin.jobs(&scope)?;
    kernel.stop_all().await?;
    println!(
        "{}",
        serde_json::json!({"synthetic_provider_calls":calls,"external_model_requests":0,"jobs":jobs.len(),
        "completed":jobs.iter().filter(|j| j.status==SummaryStatus::Completed).count(),"old_handles_closed":admin.jobs(&scope)==Err(MemoryError::Unavailable)})
    );
    Ok(())
}
