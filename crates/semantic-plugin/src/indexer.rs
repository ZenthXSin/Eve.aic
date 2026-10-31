//! 后台建索引：对比记忆中的可召回条目与索引，缺失或正文已变的条目每次至多嵌入一批，
//! 已不再可召回的条目（例如撤销的偏好）一并移出。先请求向量，成功后一次提交；请求失败不写入。
use crate::{
    SemanticIndex,
    items::{LayoutPart, content_hash, embed_input, scan_layout, scope_key},
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
    /// 本次最多写一段；未选中段只计数，不保留输入正文。
    block: Option<(SegmentKey, Vec<Pending>)>,
    pending_count: usize,
    block_pending_count: usize,
    stale: Vec<(String, String)>,
    indexed: usize,
}

#[derive(Default)]
struct ScopePlan {
    wanted: BTreeSet<(String, String)>,
    pending: Vec<Pending>,
    block: Option<(SegmentKey, Vec<Pending>)>,
    pending_count: usize,
    block_pending_count: usize,
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
        let mut block = None;
        let mut pending_count = 0;
        let mut block_pending_count = 0;
        let mut indexed = 0;
        for scope in self.memory.scopes().map_err(|_| IndexStepError::Memory)? {
            let reader = self
                .memory
                .reader(scope.clone())
                .map_err(|_| IndexStepError::Memory)?;
            let key = scope_key(&scope);
            let available = MAX_EMBED_BATCH.saturating_sub(pending.len());
            let choose_block = block.is_none();
            let scanned = scan_layout(
                reader.as_ref(),
                segments.is_some(),
                |actual, _| {
                    if *actual != scope {
                        return Err(eve_memory_api::MemoryError::CorruptState);
                    }
                    Ok(ScopePlan::default())
                },
                |plan, part| {
                    match part {
                        LayoutPart::Ledger(ledger) => {
                            for item in ledger {
                                let Some(input) = embed_input(&item) else {
                                    continue;
                                };
                                let hash = content_hash(&input);
                                let identity = (key.clone(), item.key.clone());
                                let previous = current.get(&identity);
                                // wanted 只用于核对现有索引的淘汰，不需记住尚不存在的来源。
                                if previous.is_some() {
                                    plan.wanted.insert(identity);
                                }
                                if previous == Some(&hash) {
                                    plan.indexed += 1;
                                } else {
                                    plan.pending_count += 1;
                                    if plan.pending.len() < available {
                                        plan.pending.push(Pending {
                                            scope: key.clone(),
                                            item: item.key,
                                            content_sha256: hash,
                                            input,
                                        });
                                    }
                                }
                            }
                        }
                        LayoutPart::Block(archived) => {
                            let segments =
                                segments.ok_or(eve_memory_api::MemoryError::CorruptState)?;
                            let items: Vec<Pending> = archived
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
                            let pairs: Vec<_> = items
                                .iter()
                                .map(|item| (item.item.clone(), item.content_sha256.clone()))
                                .collect();
                            let segment = SegmentKey {
                                profile: profile_key(profile),
                                scope: key.clone(),
                                first_revision: archived.first_revision,
                                last_revision: archived.last_revision,
                                content: content_key(&pairs),
                            };
                            if segments
                                .contains(&segment)
                                .map_err(|_| eve_memory_api::MemoryError::CorruptState)?
                            {
                                plan.indexed += items.len();
                            } else {
                                // 未建好的冷段原热索引仍保留，不能在扫描阶段提交淘汰。
                                for item in &items {
                                    let identity = (item.scope.clone(), item.item.clone());
                                    if current.contains_key(&identity) {
                                        plan.wanted.insert(identity);
                                    }
                                }
                                plan.block_pending_count += items.len();
                                if choose_block && plan.block.is_none() {
                                    plan.block = Some((segment, items));
                                }
                            }
                        }
                    }
                    Ok(())
                },
            )
            .map_err(|_| IndexStepError::Memory)?
            .value;
            wanted.extend(scanned.wanted);
            pending.extend(scanned.pending);
            if block.is_none() {
                block = scanned.block;
            }
            pending_count += scanned.pending_count;
            block_pending_count += scanned.block_pending_count;
            indexed += scanned.indexed;
        }
        let stale = current
            .into_keys()
            .filter(|identity| !wanted.contains(identity))
            .collect();
        Ok(Plan {
            pending,
            block,
            pending_count,
            block_pending_count,
            stale,
            indexed,
        })
    }

    pub fn status(&self) -> Result<IndexStatus, IndexStepError> {
        let plan = self.plan()?;
        Ok(IndexStatus {
            indexed: plan.indexed,
            pending: plan.pending_count + plan.block_pending_count,
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
        let Some((segment, items)) = plan.block else {
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

#[cfg(test)]
#[path = "indexer_tests.rs"]
mod tests;
