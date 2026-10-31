use super::*;
use crate::{SegmentStore, SemanticPlugin};
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_memory_api::*;
use eve_semantic_api::{EmbedFuture, EmbeddingProfile};
use std::sync::Mutex;

struct Inputs {
    profile: EmbeddingProfile,
    calls: Mutex<Vec<Vec<String>>>,
}
impl EmbeddingProvider for Inputs {
    fn profile(&self) -> &EmbeddingProfile {
        &self.profile
    }
    fn embed<'a>(&'a self, inputs: &'a [String]) -> EmbedFuture<'a> {
        Box::pin(async move {
            eve_semantic_api::validate_inputs(inputs)?;
            self.calls.lock().unwrap().push(inputs.to_vec());
            Ok(vec![vec![1.0, 0.1]; inputs.len()])
        })
    }
}
struct Memory {
    cold: bool,
    late_error: bool,
}
fn scope(name: &str) -> MemoryScope {
    MemoryScope {
        channel: "test".into(),
        session_id: name.into(),
        user_id: "user".into(),
    }
}
struct Reader {
    scope: MemoryScope,
    cold: bool,
    late_error: bool,
}
fn evidence(revision: u64) -> InteractionEvidence {
    InteractionEvidence {
        id: format!("e-{revision}"),
        revision,
        at_ms: revision,
        source: EvidenceSource::CompletedInteraction {
            message_id: format!("m-{revision}"),
            session_revision: revision * 2,
            turn_id: revision,
            user_text: format!("user-{revision}"),
            assistant_text: format!("assistant-{revision}"),
        },
    }
}
impl MemoryService for Reader {
    fn snapshot(&self) -> MemoryResult<MemorySnapshot> {
        if self.late_error && self.scope.session_id == "second" {
            return Err(MemoryError::CorruptState);
        }
        Ok(MemorySnapshot {
            scope: self.scope.clone(),
            revision: 10000,
            evidence: if self.cold {
                vec![evidence(1000)]
            } else {
                (1..=10).map(evidence).collect()
            },
            preferences: vec![],
            archived_evidence: if self.cold { 192 } else { 0 },
        })
    }
    fn archived(&self, after: u64, limit: usize) -> MemoryResult<ArchivedEvidencePage> {
        assert!(self.cold);
        let end = (after + limit as u64).min(192);
        Ok(ArchivedEvidencePage {
            evidence: (after + 1..=end).map(evidence).collect(),
            next_after: (end < 192).then_some(end),
        })
    }
}
impl MemoryAdmin for Memory {
    fn scopes(&self) -> MemoryResult<Vec<MemoryScope>> {
        Ok(if self.cold {
            vec![scope("first")]
        } else {
            vec![scope("first"), scope("second")]
        })
    }
    fn reader(&self, scope: MemoryScope) -> MemoryResult<Arc<dyn MemoryService>> {
        Ok(Arc::new(Reader {
            scope,
            cold: self.cold,
            late_error: self.late_error,
        }))
    }
    fn import_completed(
        &self,
        _: &MemoryScope,
        _: u64,
        _: CompletedInteraction,
    ) -> MemoryResult<MemorySnapshot> {
        Err(MemoryError::Unavailable)
    }
    fn update_preference(
        &self,
        _: &MemoryScope,
        _: u64,
        _: PreferenceChange,
    ) -> MemoryResult<MemorySnapshot> {
        Err(MemoryError::Unavailable)
    }
}
async fn fixture(
    cold: bool,
    late_error: bool,
) -> (tempfile::TempDir, Kernel, Indexer, Arc<Inputs>) {
    let directory = tempfile::tempdir().unwrap();
    let kernel = Kernel::with_services(KernelServices {
        state: Arc::new(FileStateStore::open(directory.path().join("state")).unwrap()),
        ..Default::default()
    });
    let mut plugin = SemanticPlugin::new().unwrap();
    if cold {
        plugin = plugin.with_segments(Arc::new(
            SegmentStore::open(directory.path().join("segments")).unwrap(),
        ));
    }
    let index = plugin.index();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let embedder = Arc::new(Inputs {
        profile: EmbeddingProfile {
            model: "synthetic".into(),
            dimensions: 2,
        },
        calls: Mutex::new(vec![]),
    });
    let indexer = Indexer::new(
        Arc::new(Memory { cold, late_error }),
        index,
        embedder.clone(),
    );
    (directory, kernel, indexer, embedder)
}
#[tokio::test]
async fn hot_plan_keeps_one_batch_but_counts_all_scopes_and_preserves_next_batch_order() {
    let (_directory, kernel, indexer, embedder) = fixture(false, false).await;
    let plan = indexer.plan().unwrap();
    assert_eq!(plan.pending_count, 40);
    assert_eq!(plan.pending.len(), MAX_EMBED_BATCH);
    assert!(plan.block.is_none());
    assert_eq!(
        indexer.status().unwrap(),
        IndexStatus {
            indexed: 0,
            pending: 40
        }
    );
    assert_eq!(indexer.step(100).await.unwrap(), 16);
    let first: Vec<_> = (1..=8)
        .flat_map(|revision| [format!("user-{revision}"), format!("assistant-{revision}")])
        .collect();
    assert_eq!(embedder.calls.lock().unwrap()[0], first);
    let plan = indexer.plan().unwrap();
    assert_eq!(plan.pending_count, 24);
    assert_eq!(plan.pending.len(), 16);
    assert_eq!(plan.pending[0].input, "user-9");
    assert_eq!(plan.pending[4].input, "user-1");
    assert_eq!(plan.pending[4].scope, scope_key(&scope("second")));
    assert_eq!(
        indexer.status().unwrap(),
        IndexStatus {
            indexed: 16,
            pending: 24
        }
    );
    kernel.stop_all().await.unwrap();
}
#[tokio::test]
async fn cold_plan_keeps_one_segment_and_exact_progress_without_changing_embedding_batches() {
    let (_directory, kernel, indexer, embedder) = fixture(true, false).await;
    let plan = indexer.plan().unwrap();
    assert_eq!((plan.pending_count, plan.block_pending_count), (2, 384));
    let (segment, items) = plan.block.unwrap();
    assert_eq!(
        (segment.first_revision, segment.last_revision, items.len()),
        (1, 64, 128)
    );
    assert_eq!(indexer.step(100).await.unwrap(), 2);
    assert_eq!(
        indexer.status().unwrap(),
        IndexStatus {
            indexed: 2,
            pending: 384
        }
    );
    assert_eq!(indexer.step(101).await.unwrap(), 128);
    assert_eq!(
        indexer.status().unwrap(),
        IndexStatus {
            indexed: 130,
            pending: 256
        }
    );
    {
        let calls = embedder.calls.lock().unwrap();
        assert_eq!(calls.len(), 9);
        assert_eq!(calls[0], ["user-1000", "assistant-1000"]);
        assert!(calls[1..].iter().all(|batch| batch.len() == 16));
    }
    let plan = indexer.plan().unwrap();
    let (segment, items) = plan.block.unwrap();
    assert_eq!(
        (segment.first_revision, segment.last_revision, items.len()),
        (65, 128, 128)
    );
    kernel.stop_all().await.unwrap();
}
#[tokio::test]
async fn filling_first_batch_does_not_skip_a_later_scope_error_or_write_partial_index() {
    let (_directory, kernel, indexer, embedder) = fixture(false, true).await;
    assert!(matches!(
        indexer.step(100).await,
        Err(IndexStepError::Memory)
    ));
    assert!(embedder.calls.lock().unwrap().is_empty());
    assert!(indexer.index.entries().unwrap().is_empty());
    kernel.stop_all().await.unwrap();
}
