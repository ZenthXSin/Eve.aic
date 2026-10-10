use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::{
    ChatMessage, ChatRole, ContextAssembler, ContextScope, ContextSnapshot, LlmFuture, TurnInput,
};
use eve_memory_api::*;
use eve_memory_plugin::{LexicalMemoryRecall, MemoryController, MemoryPlugin, MemoryRecallContext};
use eve_plugin_api::{PluginId, PluginResult, StateStore};
use eve_semantic_api::*;
use eve_semantic_plugin::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

/// 确定性替身：按概念关键词给出 4 维向量，同一概念的不同说法方向相同。
struct Concepts {
    profile: EmbeddingProfile,
    calls: AtomicUsize,
    fail: AtomicBool,
}
impl Concepts {
    fn new(model: &str) -> Arc<Self> {
        Arc::new(Self {
            profile: EmbeddingProfile {
                model: model.into(),
                dimensions: 4,
            },
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        })
    }
    fn vector(text: &str) -> Vec<f32> {
        let groups: [&[&str]; 3] = [
            &["简短", "简洁", "精简", "长篇", "啰嗦"],
            &["辣", "川菜", "吃"],
            &["Mindustry", "模组", "游戏"],
        ];
        let mut vector: Vec<f32> = groups
            .iter()
            .map(|words| words.iter().filter(|word| text.contains(*word)).count() as f32)
            .collect();
        vector.push(0.05);
        vector
    }
}
impl EmbeddingProvider for Concepts {
    fn profile(&self) -> &EmbeddingProfile {
        &self.profile
    }
    fn embed<'a>(&'a self, inputs: &'a [String]) -> EmbedFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let fail = self.fail.load(Ordering::SeqCst);
        Box::pin(async move {
            validate_inputs(inputs)?;
            if fail {
                return Err(EmbeddingError::Provider);
            }
            Ok(inputs.iter().map(|text| Self::vector(text)).collect())
        })
    }
}

fn scope() -> MemoryScope {
    MemoryScope {
        channel: "qq".into(),
        session_id: "private-a".into(),
        user_id: "alice".into(),
    }
}

fn request(query: &str) -> MemoryRecallRequest {
    MemoryRecallRequest {
        query: query.into(),
        limit: 3,
    }
}

struct World {
    _directory: tempfile::TempDir,
    state: Arc<FileStateStore>,
    kernel: Kernel,
    memory: MemoryController,
    index: SemanticIndex,
}
impl World {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(FileStateStore::open(directory.path().join("state")).unwrap());
        let (kernel, memory, index) = open(state.clone()).await.unwrap();
        Self {
            _directory: directory,
            state,
            kernel,
            memory,
            index,
        }
    }
    fn change(&self, action: PreferenceAction) {
        let revision = self
            .memory
            .reader(scope())
            .unwrap()
            .snapshot()
            .unwrap()
            .revision;
        self.memory
            .update_preference(
                &scope(),
                revision,
                PreferenceChange {
                    operation_id: format!("operation-{revision}"),
                    at_ms: 200 + revision,
                    evidence: PreferenceEvidence::Statement(UserStatement {
                        evidence_id: format!("evidence-{revision}"),
                        message_id: format!("message-{revision}"),
                        text: "保存偏好".into(),
                        at_ms: 100 + revision,
                    }),
                    action,
                },
            )
            .unwrap();
    }
    fn confirm(&self, id: &str, text: &str) {
        self.change(PreferenceAction::Confirm {
            id: id.into(),
            text: text.into(),
        });
    }
    fn indexer(&self, embedder: Arc<Concepts>) -> Indexer {
        Indexer::new(Arc::new(self.memory.clone()), self.index.clone(), embedder)
    }
    fn hybrid(&self, embedder: Arc<Concepts>) -> HybridRecall {
        HybridRecall::new(
            Arc::new(self.memory.clone()),
            Arc::new(LexicalMemoryRecall::new(Arc::new(self.memory.clone()))),
            self.index.clone(),
            embedder,
        )
    }
    async fn recall(&self, embedder: Arc<Concepts>, query: &str) -> MemoryResult<Vec<String>> {
        let response = self
            .hybrid(embedder)
            .reader(scope())?
            .recall(&request(query))
            .await?;
        response.validate_for(&scope(), &request(query)).unwrap();
        Ok(response
            .hits
            .iter()
            .map(|hit| match &hit.source {
                MemoryRecallSource::ConfirmedPreference { preference_id, .. } => {
                    preference_id.clone()
                }
                MemoryRecallSource::CompletedInteraction { evidence_id, .. } => evidence_id.clone(),
            })
            .collect())
    }
    fn raw(&self) -> Option<Vec<u8>> {
        self.state
            .get(
                &PluginId::new(SEMANTIC_PLUGIN_ID).unwrap(),
                SEMANTIC_STATE_KEY,
            )
            .unwrap()
    }
}

async fn open(
    state: Arc<FileStateStore>,
) -> PluginResult<(Kernel, MemoryController, SemanticIndex)> {
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let memory = MemoryPlugin::new()?;
    let semantic = SemanticPlugin::new()?;
    let (controller, index) = (memory.controller(), semantic.index());
    kernel.register(Box::new(memory))?;
    kernel.register(Box::new(semantic))?;
    kernel.start_all().await?;
    Ok((kernel, controller, index))
}

#[tokio::test]
async fn a_paraphrase_with_no_shared_terms_is_recalled_only_after_indexing() {
    let world = World::new().await;
    world.confirm("brief", "以后回答尽量简短");
    world.confirm("food", "我喜欢辣的川菜");
    let embedder = Concepts::new("concepts-v1");
    // 与偏好没有任何共同字词：词项召回找不到。
    let lexical = LexicalMemoryRecall::new(Arc::new(world.memory.clone()))
        .reader(scope())
        .unwrap()
        .recall(&request("别写长篇大论"))
        .unwrap();
    assert!(lexical.hits.is_empty());
    assert!(
        world
            .recall(embedder.clone(), "别写长篇大论")
            .await
            .unwrap()
            .is_empty(),
        "尚未建索引"
    );

    let indexer = world.indexer(embedder.clone());
    assert_eq!(
        indexer.status().unwrap(),
        IndexStatus {
            indexed: 0,
            pending: 2
        }
    );
    assert_eq!(indexer.step(1_000).await.unwrap(), 2);
    assert_eq!(
        embedder.calls.load(Ordering::SeqCst),
        2,
        "一次查询加一次批量嵌入"
    );
    assert_eq!(
        indexer.status().unwrap(),
        IndexStatus {
            indexed: 2,
            pending: 0
        }
    );
    assert_eq!(indexer.step(1_001).await.unwrap(), 0);
    assert_eq!(
        embedder.calls.load(Ordering::SeqCst),
        2,
        "没有待处理条目时不请求"
    );
    // 索引只保存范围摘要、条目键、正文摘要与量化向量，不含正文或身份原文。
    let raw = String::from_utf8(world.raw().unwrap()).unwrap();
    for secret in ["简短", "川菜", "alice", "private-a"] {
        assert!(!raw.contains(secret), "{secret}");
    }
    assert_eq!(
        world
            .recall(embedder.clone(), "别写长篇大论")
            .await
            .unwrap(),
        ["brief"]
    );
    assert_eq!(
        world
            .recall(embedder.clone(), "晚饭想吃点什么")
            .await
            .unwrap(),
        ["food"]
    );
}

#[tokio::test]
async fn corrected_text_is_reindexed_revoked_items_leave_and_lexical_hits_fuse_first() {
    let world = World::new().await;
    world.confirm("brief", "以后回答尽量简短");
    world.confirm("game", "我在做 Mindustry 模组");
    let embedder = Concepts::new("concepts-v1");
    let indexer = world.indexer(embedder.clone());
    indexer.step(1_000).await.unwrap();
    // 同时被词项与语义命中的条目排在只被语义命中的条目之前。
    world.confirm("brief-2", "回答精简一点");
    indexer.step(1_100).await.unwrap();
    assert_eq!(
        world.recall(embedder.clone(), "回答要简洁").await.unwrap(),
        ["brief-2", "brief"]
    );
    // 更正后正文摘要改变：旧向量不再使用，重新嵌入。
    world.change(PreferenceAction::Correct {
        id: "game".into(),
        text: "我想吃辣的".into(),
    });
    assert_eq!(indexer.status().unwrap().pending, 1);
    indexer.step(1_200).await.unwrap();
    assert_eq!(
        world.recall(embedder.clone(), "游戏模组").await.unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        world.recall(embedder.clone(), "川菜").await.unwrap(),
        ["game"]
    );
    // 撤销后从索引移出。
    world.change(PreferenceAction::Revoke { id: "game".into() });
    let before = world.index.entries().unwrap().len();
    indexer.step(1_300).await.unwrap();
    assert_eq!(world.index.entries().unwrap().len(), before - 1);
}

#[derive(Default)]
struct Base;
impl ContextAssembler for Base {
    fn assemble(&self, input: TurnInput) -> LlmFuture<'_, ContextSnapshot> {
        self.assemble_scoped(input, None)
    }
    fn assemble_scoped(
        &self,
        _input: TurnInput,
        _scope: Option<ContextScope>,
    ) -> LlmFuture<'_, ContextSnapshot> {
        Box::pin(async {
            Ok(ContextSnapshot {
                revision: "base".into(),
                profile: "profile".into(),
                memories: vec![],
                history: vec![ChatMessage::text(ChatRole::User, "earlier")],
            })
        })
    }
}

#[tokio::test]
async fn embedding_failure_writes_nothing_and_context_marks_the_lexical_fallback() {
    let world = World::new().await;
    world.confirm("brief", "以后回答尽量简短");
    let embedder = Concepts::new("concepts-v1");
    embedder.fail.store(true, Ordering::SeqCst);
    let indexer = world.indexer(embedder.clone());
    assert_eq!(
        indexer.step(1_000).await,
        Err(IndexStepError::Embedding(EmbeddingError::Provider))
    );
    assert!(world.raw().is_none(), "请求失败不写入");
    let context = MemoryRecallContext::with_async_recall(
        "qq",
        Arc::new(world.hybrid(embedder.clone())),
        Arc::new(LexicalMemoryRecall::new(Arc::new(world.memory.clone()))),
        Arc::new(Base),
    )
    .unwrap();
    let owner = Some(ContextScope {
        session_id: "private-a".into(),
        user_id: "alice".into(),
    });
    let fallback = context
        .assemble_scoped(
            TurnInput {
                text: "回答简短".into(),
            },
            owner.clone(),
        )
        .await
        .unwrap();
    assert!(fallback.memories[0].contains("\"retrieval\":\"lexical_fallback\""));
    assert!(fallback.revision.contains("eve-memory-recall-fallback-1"));
    embedder.fail.store(false, Ordering::SeqCst);
    indexer.step(1_100).await.unwrap();
    let hybrid = context
        .assemble_scoped(
            TurnInput {
                text: "别写长篇大论".into(),
            },
            owner,
        )
        .await
        .unwrap();
    assert!(hybrid.memories[0].contains("\"retrieval\":\"hybrid\""));
    assert!(hybrid.memories[0].contains("以后回答尽量简短"));
}

#[tokio::test]
async fn another_model_has_its_own_entries_and_corrupt_index_is_refused_and_preserved() {
    let world = World::new().await;
    world.confirm("brief", "以后回答尽量简短");
    let first = Concepts::new("concepts-v1");
    world.indexer(first.clone()).step(1_000).await.unwrap();
    // 换模型：旧条目不可比较，不参与；新模型自己建索引，旧条目保留。
    let second = Concepts::new("concepts-v2");
    let indexer = world.indexer(second.clone());
    assert_eq!(
        indexer.status().unwrap(),
        IndexStatus {
            indexed: 0,
            pending: 1
        }
    );
    assert!(
        world
            .recall(second.clone(), "别写长篇大论")
            .await
            .unwrap()
            .is_empty()
    );
    indexer.step(1_100).await.unwrap();
    assert_eq!(world.index.entries().unwrap().len(), 2);
    assert_eq!(
        world.recall(second, "别写长篇大论").await.unwrap(),
        ["brief"]
    );
    world.kernel.stop_all().await.unwrap();

    let valid: serde_json::Value = serde_json::from_slice(&world.raw().unwrap()).unwrap();
    let mutate = |change: &dyn Fn(&mut serde_json::Value)| {
        let mut value = valid.clone();
        change(&mut value);
        serde_json::to_vec(&value).unwrap()
    };
    let cases = vec![
        b"{\"format_version\":1,\"format_version\":1,\"entries\":[]}".to_vec(),
        serde_json::to_vec(&serde_json::json!({"format_version": 2, "entries": []})).unwrap(),
        mutate(&|value| value["entries"][0]["vector"] = "zz".into()),
        mutate(&|value| value["entries"][0]["dimensions"] = 5.into()),
        mutate(&|value| value["entries"][1] = value["entries"][0].clone()),
        mutate(&|value| value["entries"][0]["scope"] = "not-a-digest".into()),
        mutate(&|value| value["extra"] = 1.into()),
    ];
    for case in cases {
        world
            .state
            .set(
                &PluginId::new(SEMANTIC_PLUGIN_ID).unwrap(),
                SEMANTIC_STATE_KEY.into(),
                case.clone(),
            )
            .unwrap();
        assert!(
            open(world.state.clone()).await.is_err(),
            "{}",
            String::from_utf8_lossy(&case)
        );
        assert_eq!(world.raw().unwrap(), case, "拒绝打开时不改写原字节");
    }
}
