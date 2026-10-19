//! 后台建索引：对比记忆中的可召回条目与索引，缺失或正文已变的条目每次至多嵌入一批，
//! 已不再可召回的条目（例如撤销的偏好）一并移出。先请求向量，成功后一次提交；请求失败不写入。
use crate::{
    SemanticIndex,
    items::{content_hash, embed_input, layout, scope_key},
    segments::{SegmentError, SegmentKey, content_key, profile_key},
    store::{IndexEntry, IndexError},
};
use eve_memory_api::MemoryAdmin;
use eve_semantic_api::{EmbeddingError, EmbeddingProvider, MAX_EMBED_BATCH, QuantizedVector};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexStepError {
    Memory,
    Index(IndexError),
    Embedding(EmbeddingError),
}

/// 当前模型的索引进度。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IndexStatus {
    pub indexed: usize,
    pub pending: usize,
}

pub struct Indexer {
    memory: Arc<dyn MemoryAdmin>,
    index: SemanticIndex,
    embedder: Arc<dyn EmbeddingProvider>,
}

struct Pending {
    scope: String,
    item: String,
    content_sha256: String,
    input: String,
}

struct Plan {
    pending: Vec<Pending>,
    /// 尚未写出向量分段的归档段及其条目。
    blocks: Vec<(SegmentKey, Vec<Pending>)>,
    stale: Vec<(String, String)>,
    indexed: usize,
}

impl Indexer {
    pub fn new(
        memory: Arc<dyn MemoryAdmin>,
        index: SemanticIndex,
        embedder: Arc<dyn EmbeddingProvider>,
    ) -> Self {
        Self {
            memory,
            index,
            embedder,
        }
    }

    fn plan(&self) -> Result<Plan, IndexStepError> {
        let profile = self.embedder.profile();
        let entries = self.index.entries().map_err(IndexStepError::Index)?;
        let current: BTreeMap<(String, String), String> = entries
            .into_iter()
            .filter(|entry| entry.belongs_to(profile))
            .map(|entry| ((entry.scope, entry.item), entry.content_sha256))
            .collect();
        let segments = self.index.segments();
        let mut wanted = BTreeSet::new();
        let mut pending = Vec::new();
        let mut blocks = Vec::new();
        let mut indexed = 0;
        for scope in self.memory.scopes().map_err(|_| IndexStepError::Memory)? {
            let reader = self
                .memory
                .reader(scope.clone())
                .map_err(|_| IndexStepError::Memory)?;
            let layout =
                layout(reader.as_ref(), segments.is_some()).map_err(|_| IndexStepError::Memory)?;
            let key = scope_key(&scope);
            for item in layout.ledger {
                let Some(input) = embed_input(&item) else {
                    continue;
                };
                let hash = content_hash(&input);
                wanted.insert((key.clone(), item.key.clone()));
                if current.get(&(key.clone(), item.key.clone())) == Some(&hash) {
                    indexed += 1;
                } else {
                    pending.push(Pending {
                        scope: key.clone(),
                        item: item.key,
                        content_sha256: hash,
                        input,
                    });
                }
            }
            let Some(segments) = segments else {
                continue;
            };
            for block in layout.blocks {
                let items: Vec<Pending> = block
                    .items
                    .into_iter()
                    .filter_map(|item| {
                        let input = embed_input(&item)?;
                        Some(Pending {
                            scope: key.clone(),
                            content_sha256: content_hash(&input),
                            item: item.key,
                            input,
                        })
                    })
                    .collect();
                let pairs: Vec<(String, String)> = items
                    .iter()
                    .map(|item| (item.item.clone(), item.content_sha256.clone()))
                    .collect();
                let segment = SegmentKey {
                    profile: profile_key(profile),
                    scope: key.clone(),
                    first_revision: block.first_revision,
                    last_revision: block.last_revision,
                    content: content_key(&pairs),
                };
                if segments
                    .contains(&segment)
                    .map_err(|_| IndexStepError::Memory)?
                {
                    indexed += items.len();
                } else {
                    // 分段写出之前，这些条目若已在 index.v1 中就继续保留，召回不出现空档。
                    for item in &items {
                        wanted.insert((item.scope.clone(), item.item.clone()));
                    }
                    blocks.push((segment, items));
                }
            }
        }
        let stale = current
            .into_keys()
            .filter(|identity| !wanted.contains(identity))
            .collect();
        Ok(Plan {
            pending,
            blocks,
            stale,
            indexed,
        })
    }

    pub fn status(&self) -> Result<IndexStatus, IndexStepError> {
        let plan = self.plan()?;
        Ok(IndexStatus {
            indexed: plan.indexed,
            pending: plan.pending.len()
                + plan
                    .blocks
                    .iter()
                    .map(|(_, items)| items.len())
                    .sum::<usize>(),
        })
    }

    async fn embed(
        &self,
        batch: Vec<Pending>,
        now_ms: u64,
    ) -> Result<Vec<IndexEntry>, IndexStepError> {
        let inputs: Vec<String> = batch.iter().map(|item| item.input.clone()).collect();
        let vectors = self
            .embedder
            .embed(&inputs)
            .await
            .map_err(IndexStepError::Embedding)?;
        eve_semantic_api::validate_vectors(self.embedder.profile(), inputs.len(), &vectors)
            .map_err(IndexStepError::Embedding)?;
        let profile = self.embedder.profile();
        let mut entries = Vec::with_capacity(batch.len());
        for (item, vector) in batch.into_iter().zip(vectors) {
            let quantized = QuantizedVector::quantize(&vector)
                .ok_or(IndexStepError::Embedding(EmbeddingError::InvalidOutput))?;
            entries.push(IndexEntry {
                model: profile.model.clone(),
                dimensions: profile.dimensions,
                scope: item.scope,
                item: item.item,
                content_sha256: item.content_sha256,
                scale: quantized.scale,
                vector: quantized.to_hex(),
                indexed_at_ms: now_ms.max(1),
            });
        }
        Ok(entries)
    }

    /// 热层条目优先：至多一次向量请求。热层都已建好时，为一段归档证据建索引并写出分段：
    /// 分批请求，全部成功才写出，任何一批失败都不写入。返回本次新写入的条数。
    pub async fn step(&self, now_ms: u64) -> Result<usize, IndexStepError> {
        let plan = self.plan()?;
        let batch: Vec<Pending> = plan.pending.into_iter().take(MAX_EMBED_BATCH).collect();
        if !batch.is_empty() || !plan.stale.is_empty() {
            let upserts = if batch.is_empty() {
                vec![]
            } else {
                self.embed(batch, now_ms).await?
            };
            let written = upserts.len();
            self.index
                .apply(self.embedder.profile(), upserts, plan.stale)
                .map_err(IndexStepError::Index)?;
            return Ok(written);
        }
        let Some((segment, items)) = plan.blocks.into_iter().next() else {
            return Ok(0);
        };
        let segments = self
            .index
            .segments()
            .ok_or(IndexStepError::Index(IndexError::Unavailable))?;
        let mut entries = Vec::with_capacity(items.len());
        let mut items = items.into_iter().peekable();
        while items.peek().is_some() {
            let batch: Vec<Pending> = items.by_ref().take(MAX_EMBED_BATCH).collect();
            entries.extend(self.embed(batch, now_ms).await?);
        }
        let written = entries.len();
        segments
            .put(self.embedder.profile(), &segment, entries)
            .map_err(|error| {
                IndexStepError::Index(match error {
                    SegmentError::Storage => IndexError::Storage,
                    SegmentError::CorruptState => IndexError::CorruptState,
                    SegmentError::InvalidInput => IndexError::InvalidInput,
                })
            })?;
        Ok(written)
    }
}
