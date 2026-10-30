//! 固定合成原文的 RSS 对照；seed 与读取分进程，full 复现旧完整归档入口，scan 使用原生分页。
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::{ChatMessage, ChatRole};
use eve_memory_api::*;
use eve_memory_plugin::{ArchivePolicy, FileEvidenceArchive, MemoryPlugin, ScopedLexicalRecall};
use eve_session_api::{SessionKey, SessionSnapshot, SessionTurn, SessionTurnStatus};
use eve_session_plugin::SessionPlugin;
use std::{path::PathBuf, sync::Arc, time::Duration};

struct FullView(Arc<dyn MemoryService>);
impl MemoryService for FullView {
    fn snapshot(&self) -> MemoryResult<MemorySnapshot> {
        self.0.complete_snapshot()
    }
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().ok_or("mode: seed|full|scan")?;
    if !matches!(mode.as_str(), "seed" | "full" | "scan") {
        return Err("unknown mode".into());
    }
    let path = PathBuf::from(args.next().ok_or("temporary sample directory required")?);
    let count: usize = args.next().unwrap_or("160".into()).parse()?;
    if !(80..=512).contains(&count) {
        return Err("sample count must be 80..512".into());
    }
    let state = Arc::new(FileStateStore::open(path.join("state"))?);
    let backends = KernelServices {
        state,
        ..KernelServices::default()
    };
    let kernel = Kernel::with_services(backends);
    let plugin = MemoryPlugin::new()?.with_archive(
        Arc::new(FileEvidenceArchive::open(path.join("archive"))?),
        ArchivePolicy::default(),
    )?;
    let admin = plugin.controller();
    kernel.register(Box::new(plugin))?;
    kernel.register(Box::new(SessionPlugin::new()?))?;
    kernel.start_all().await?;
    let scope = MemoryScope {
        channel: "test".into(),
        session_id: "rss-sample".into(),
        user_id: "synthetic".into(),
    };
    if mode == "seed" {
        if admin.reader(scope.clone())?.snapshot()?.revision != 0 {
            return Err("seed requires empty sample".into());
        }
        let mut session = SessionSnapshot {
            key: SessionKey::new(&scope.session_id, &scope.user_id)?,
            revision: 0,
            turns: vec![],
        };
        for n in 1..=count as u64 {
            let user = format!("needle-{n} {}", "x".repeat(30_000));
            session.turns.push(SessionTurn {
                id: n,
                input: user.clone(),
                status: SessionTurnStatus::Completed {
                    messages: vec![
                        ChatMessage::text(ChatRole::User, user),
                        ChatMessage::text(ChatRole::Assistant, "y".repeat(30_000)),
                    ],
                },
            });
            session.revision += 2;
            admin.import_completed(
                &scope,
                n - 1,
                CompletedInteraction {
                    evidence_id: format!("e-{n}"),
                    message_id: format!("m-{n}"),
                    at_ms: n,
                    snapshot: session.clone(),
                    turn_id: n,
                },
            )?;
        }
        println!("seeded {count}");
    } else {
        let reader = admin.reader(scope.clone())?;
        let reader: Arc<dyn MemoryService> = if mode == "full" {
            Arc::new(FullView(reader))
        } else {
            reader
        };
        let recall = ScopedLexicalRecall::new(scope, reader)?;
        for _ in 0..3 {
            let result = recall.recall(&MemoryRecallRequest {
                query: "needle-1".into(),
                limit: 8,
            })?;
            assert!(
                result
                    .hits
                    .iter()
                    .any(|hit| hit.excerpt.contains("needle-1"))
            );
            println!("{}", serde_json::to_string(&result)?);
        }
        // 供外部采样器区分计算峰值与处理后的保留内存，不发出任何网络请求。
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    kernel.stop_all().await?;
    Ok(())
}
