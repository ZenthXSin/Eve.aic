//! 已归档的旧原文同样建立向量索引：凑满一段的归档证据写成独立的压缩分段，不进入 index.v1。
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::{ChatMessage, ChatRole};
use eve_memory_api::*;
use eve_memory_plugin::{
    ArchivePolicy, FileEvidenceArchive, LexicalMemoryRecall, MemoryController, MemoryPlugin,
};
use eve_semantic_api::*;
use eve_semantic_plugin::*;
use eve_session_api::{SessionKey, SessionSnapshot, SessionTurn, SessionTurnStatus};
use std::{path::Path, sync::Arc};

/// 确定性替身：按概念关键词给出 4 维向量，同一概念的不同说法方向相同。
struct Concepts(EmbeddingProfile);
impl EmbeddingProvider for Concepts {
    fn profile(&self) -> &EmbeddingProfile {
        &self.0
    }
    fn embed<'a>(&'a self, inputs: &'a [String]) -> EmbedFuture<'a> {
        Box::pin(async move {
            validate_inputs(inputs)?;
            Ok(inputs
                .iter()
                .map(|text| {
                    let groups: [&[&str]; 2] = [&["辣", "川菜", "吃"], &["闲聊"]];
                    let mut vector: Vec<f32> = groups
                        .iter()
                        .map(|words| words.iter().filter(|w| text.contains(*w)).count() as f32)
                        .collect();
                    vector.extend([0.05, 0.05]);
                    vector
                })
                .collect())
        })
    }
}

fn embedder() -> Arc<Concepts> {
    Arc::new(Concepts(EmbeddingProfile {
        model: "concepts".into(),
        dimensions: 4,
    }))
}

fn scope() -> MemoryScope {
    MemoryScope {
        channel: "qq".into(),
        session_id: "private-a".into(),
        user_id: "alice".into(),
    }
}

struct World {
    kernel: Kernel,
    memory: MemoryController,
    index: SemanticIndex,
}

async fn open(root: &Path) -> World {
    let kernel = Kernel::with_services(KernelServices {
        state: Arc::new(FileStateStore::open(root.join("state")).unwrap()),
        ..KernelServices::default()
    });
    // 凑满 64 条才成块，热层只留最近 2 条，便于得到恰好一段归档。
    let policy = ArchivePolicy {
        trigger_evidence: 6,
        keep_recent: 2,
        pressure_keep_recent: 1,
        min_age_ms: 0,
        min_chunk: 64,
        max_chunk_evidence: 64,
        max_chunk_bytes: 512 * 1024,
        ..ArchivePolicy::default()
    };
    let memory = MemoryPlugin::new()
        .unwrap()
        .with_archive(
            Arc::new(FileEvidenceArchive::open(root.join("memory-archive")).unwrap()),
            policy,
        )
        .unwrap();
    let controller = memory.controller();
    let semantic = SemanticPlugin::new().unwrap().with_segments(Arc::new(
        SegmentStore::open(root.join("semantic-archive")).unwrap(),
    ));
    let index = semantic.index();
    kernel.register(Box::new(memory)).unwrap();
    kernel.register(Box::new(semantic)).unwrap();
    kernel.start_all().await.unwrap();
    World {
        kernel,
        memory: controller,
        index,
    }
}

fn text(number: usize) -> String {
    if number == 7 {
        "我最近迷上了川菜".into()
    } else {
        format!("第 {number} 次闲聊")
    }
}

fn import(world: &World, session: &mut SessionSnapshot, number: usize) {
    import_text(world, session, number, text(number));
}
fn import_text(world: &World, session: &mut SessionSnapshot, number: usize, user: String) {
    session.turns.push(SessionTurn {
        id: number as u64,
        input: user.clone(),
        status: SessionTurnStatus::Completed {
            messages: vec![
                ChatMessage::text(ChatRole::User, &user),
                ChatMessage::text(ChatRole::Assistant, "嗯嗯"),
            ],
        },
    });
    session.revision += 2;
    let expected = world
        .memory
        .reader(scope())
        .unwrap()
        .snapshot()
        .unwrap()
        .revision;
    world
        .memory
        .import_completed(
            &scope(),
            expected,
            CompletedInteraction {
                evidence_id: format!("e-{number}"),
                message_id: format!("m-{number}"),
                at_ms: 1000 + number as u64,
                snapshot: session.clone(),
                turn_id: number as u64,
            },
        )
        .unwrap();
}

async fn recall(world: &World, query: &str) -> MemoryResult<Vec<String>> {
    let hybrid = HybridRecall::new(
        Arc::new(world.memory.clone()),
        Arc::new(LexicalMemoryRecall::new(Arc::new(world.memory.clone()))),
        world.index.clone(),
        embedder(),
    );
    let response = hybrid
        .reader(scope())?
        .recall(&MemoryRecallRequest {
            query: query.into(),
            limit: 3,
        })
        .await?;
    Ok(response
        .hits
        .iter()
        .filter_map(|hit| match &hit.source {
            MemoryRecallSource::CompletedInteraction { evidence_id, .. } => {
                Some(evidence_id.clone())
            }
            _ => None,
        })
        .collect())
}

fn segment_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(root.join("semantic-archive"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    files.sort();
    files
}

#[tokio::test]
async fn archived_originals_are_indexed_in_segments_and_recalled_by_meaning() {
    let directory = tempfile::tempdir().unwrap();
    let world = open(directory.path()).await;
    let mut session = SessionSnapshot {
        key: SessionKey::new("private-a", "alice").unwrap(),
        revision: 0,
        turns: vec![],
    };
    for number in 1..=80 {
        import(&world, &mut session, number);
    }
    let hot = world.memory.reader(scope()).unwrap().snapshot().unwrap();
    assert_eq!(hot.archived_evidence, 64, "恰好一段归档");
    assert!(
        !hot.evidence.iter().any(|e| e.id == "e-7"),
        "川菜那条已归档"
    );

    let indexer = Indexer::new(
        Arc::new(world.memory.clone()),
        world.index.clone(),
        embedder(),
    );
    assert_eq!(
        indexer.status().unwrap().pending,
        160,
        "热层 16 条与归档 64 条，各两个字段"
    );
    for _ in 0..20 {
        if indexer.step(5_000).await.unwrap() == 0 {
            break;
        }
    }
    let status = indexer.status().unwrap();
    assert_eq!((status.indexed, status.pending), (160, 0));
    // 归档段写成一个独立分段文件；index.v1 只有热层的 32 个条目。
    assert_eq!(segment_files(directory.path()).len(), 1);
    assert_eq!(world.index.entries().unwrap().len(), 32);

    // 查询与那条旧原文没有共同词项，只能靠归档分段里的向量找到。
    assert_eq!(recall(&world, "想吃辣的").await.unwrap()[0], "e-7");
    world.kernel.stop_all().await.unwrap();
    drop(world);

    // 重启后分段从磁盘读回，召回照常。
    let world = open(directory.path()).await;
    assert_eq!(recall(&world, "想吃辣的").await.unwrap()[0], "e-7");
    let indexer = Indexer::new(
        Arc::new(world.memory.clone()),
        world.index.clone(),
        embedder(),
    );
    assert_eq!(indexer.status().unwrap().pending, 0, "已写出的分段不重建");
    world.kernel.stop_all().await.unwrap();
    drop((world, indexer));

    // 分段被篡改：读取时逐字节核对失败，报告损坏而不是用错的向量。
    let [file] = segment_files(directory.path()).try_into().unwrap();
    let mut bytes = std::fs::read(&file).unwrap();
    let last = bytes.len() - 2;
    bytes[last] ^= 1;
    std::fs::write(&file, bytes).unwrap();
    let world = open(directory.path()).await;
    assert_eq!(
        recall(&world, "想吃辣的").await.unwrap_err(),
        MemoryError::CorruptState
    );
    world.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn multiple_cold_segments_keep_exact_ranking_unbuilt_fallback_and_reject_a_late_corrupt_file()
{
    let directory = tempfile::tempdir().unwrap();
    let world = open(directory.path()).await;
    let mut session = SessionSnapshot {
        key: SessionKey::new("private-a", "alice").unwrap(),
        revision: 0,
        turns: vec![],
    };
    for number in 1..=64 {
        import_text(
            &world,
            &mut session,
            number,
            if number == 7 {
                "周末选川菜".into()
            } else {
                text(number)
            },
        );
    }
    let indexer = Indexer::new(
        Arc::new(world.memory.clone()),
        world.index.clone(),
        embedder(),
    );
    for _ in 0..20 {
        if indexer.status().unwrap().pending == 0 {
            break;
        }
        indexer.step(5000).await.unwrap();
    }
    assert_eq!(indexer.status().unwrap().pending, 0);
    assert_eq!(world.index.entries().unwrap().len(), 128);
    for number in 65..=132 {
        import_text(
            &world,
            &mut session,
            number,
            if [71, 130].contains(&number) {
                "周末选川菜".into()
            } else {
                text(number)
            },
        );
    }
    assert_eq!(
        world
            .memory
            .reader(scope())
            .unwrap()
            .snapshot()
            .unwrap()
            .archived_evidence,
        128
    );
    indexer.step(5001).await.unwrap();
    assert!(segment_files(directory.path()).is_empty());
    assert_eq!(
        recall(&world, "想吃辣的").await.unwrap(),
        vec!["e-130", "e-7"],
        "未建分段时沿用热账本里已有的老向量"
    );
    for _ in 0..20 {
        if indexer.status().unwrap().pending == 0 {
            break;
        }
        // 只清理过期热向量的轮次可返回 0，pending 才是是否仍需建段的依据。
        indexer.step(5002).await.unwrap();
    }
    assert_eq!(indexer.status().unwrap().pending, 0);
    assert_eq!(segment_files(directory.path()).len(), 2);
    assert_eq!(world.index.entries().unwrap().len(), 8);
    assert_eq!(
        recall(&world, "想吃辣的").await.unwrap(),
        vec!["e-130", "e-71", "e-7"],
        "同向量按证据修订新到旧排序，冷热共同融合"
    );
    world.kernel.stop_all().await.unwrap();
    drop((world, indexer));
    let world = open(directory.path()).await;
    assert_eq!(
        recall(&world, "想吃辣的").await.unwrap(),
        vec!["e-130", "e-71", "e-7"]
    );
    let files = segment_files(directory.path());
    let last = files.last().unwrap();
    std::fs::write(last, b"late-corruption").unwrap();
    assert_eq!(
        recall(&world, "想吃辣的").await.unwrap_err(),
        MemoryError::CorruptState,
        "不能在第一段成功后返回部分语义结果，缓存也不能掩盖坏文件"
    );
    assert_eq!(std::fs::read(last).unwrap(), b"late-corruption");
    world.kernel.stop_all().await.unwrap();
}
