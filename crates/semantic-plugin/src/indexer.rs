//! 后台建索引：对比记忆中的可召回条目与索引，缺失或正文已变的条目每次至多嵌入一批，
//! 已不再可召回的条目（例如撤销的偏好）一并移出。先请求向量，成功后一次提交；请求失败不写入。
use crate::{
    SemanticIndex,
    items::{content_hash, items, prefix, scope_key},
    store::{IndexEntry, IndexError},
};
use eve_memory_api::MemoryAdmin;
use eve_semantic_api::{
    EmbeddingError, EmbeddingProvider, MAX_EMBED_BATCH, MAX_EMBED_INPUT_BYTES, QuantizedVector,
};
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
        let mut wanted = BTreeSet::new();
        let mut pending = Vec::new();
        let mut indexed = 0;
        for scope in self.memory.scopes().map_err(|_| IndexStepError::Memory)? {
            let snapshot = self
                .memory
                .reader(scope.clone())
                .and_then(|reader| reader.snapshot())
                .map_err(|_| IndexStepError::Memory)?;
            let key = scope_key(&scope);
            for item in items(&snapshot).map_err(|_| IndexStepError::Memory)? {
                let input = prefix(&item.text, MAX_EMBED_INPUT_BYTES).trim().to_string();
                if input.is_empty() || input.contains('\0') {
                    continue;
                }
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
        }
        let stale = current
            .into_keys()
            .filter(|identity| !wanted.contains(identity))
            .collect();
        Ok(Plan {
            pending,
            stale,
            indexed,
        })
    }

    pub fn status(&self) -> Result<IndexStatus, IndexStepError> {
        let plan = self.plan()?;
        Ok(IndexStatus {
            indexed: plan.indexed,
            pending: plan.pending.len(),
        })
    }

    /// 至多一次向量请求。返回本次新写入的条数；没有待处理条目且没有过期条目时为 0 且不写入。
    pub async fn step(&self, now_ms: u64) -> Result<usize, IndexStepError> {
        let plan = self.plan()?;
        let batch: Vec<Pending> = plan.pending.into_iter().take(MAX_EMBED_BATCH).collect();
        if batch.is_empty() && plan.stale.is_empty() {
            return Ok(0);
        }
        let mut upserts = Vec::new();
        if !batch.is_empty() {
            let inputs: Vec<String> = batch.iter().map(|item| item.input.clone()).collect();
            let vectors = self
                .embedder
                .embed(&inputs)
                .await
                .map_err(IndexStepError::Embedding)?;
            eve_semantic_api::validate_vectors(self.embedder.profile(), inputs.len(), &vectors)
                .map_err(IndexStepError::Embedding)?;
            let profile = self.embedder.profile();
            for (item, vector) in batch.into_iter().zip(vectors) {
                let quantized = QuantizedVector::quantize(&vector)
                    .ok_or(IndexStepError::Embedding(EmbeddingError::InvalidOutput))?;
                upserts.push(IndexEntry {
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
        }
        let written = upserts.len();
        self.index
            .apply(self.embedder.profile(), upserts, plan.stale)
            .map_err(IndexStepError::Index)?;
        Ok(written)
    }
}
