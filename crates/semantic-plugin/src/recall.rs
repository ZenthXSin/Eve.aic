//! 混合召回：同一快照上的词项召回与按向量相似度排序的语义召回，用倒数排名融合（RRF）合并。
//! 只读绑定范围内的现行偏好与已导入交互；未建索引或正文已变的条目只参与词项召回。
use crate::{
    SemanticIndex,
    items::{content_hash, items, prefix, scope_key, source_key},
};
use eve_memory_api::*;
use eve_semantic_api::{EmbeddingProvider, MAX_EMBED_INPUT_BYTES, cosine};
use std::{collections::BTreeMap, sync::Arc};

/// 低于此余弦相似度的语义候选不纳入；分数不是事实置信度。
pub const MIN_SIMILARITY: f32 = 0.35;
/// RRF 常数：排名靠后的贡献平缓下降。
const RRF_K: f64 = 60.0;
const ELLIPSIS: &str = "…";

pub struct HybridRecall {
    memory: Arc<dyn MemoryAdmin>,
    lexical: Arc<dyn MemoryRecallFactory>,
    index: SemanticIndex,
    embedder: Arc<dyn EmbeddingProvider>,
}
impl HybridRecall {
    pub fn new(
        memory: Arc<dyn MemoryAdmin>,
        lexical: Arc<dyn MemoryRecallFactory>,
        index: SemanticIndex,
        embedder: Arc<dyn EmbeddingProvider>,
    ) -> Self {
        Self {
            memory,
            lexical,
            index,
            embedder,
        }
    }
}

impl AsyncMemoryRecallFactory for HybridRecall {
    fn reader(&self, scope: MemoryScope) -> MemoryResult<Arc<dyn AsyncMemoryRecallService>> {
        scope.validate()?;
        Ok(Arc::new(HybridReader {
            memory: self.memory.reader(scope.clone())?,
            lexical: self.lexical.reader(scope.clone())?,
            index: self.index.clone(),
            embedder: self.embedder.clone(),
            scope,
        }))
    }
}

struct HybridReader {
    scope: MemoryScope,
    memory: Arc<dyn MemoryService>,
    lexical: Arc<dyn MemoryRecallService>,
    index: SemanticIndex,
    embedder: Arc<dyn EmbeddingProvider>,
}

impl AsyncMemoryRecallService for HybridReader {
    fn recall<'a>(&'a self, request: &'a MemoryRecallRequest) -> RecallFuture<'a> {
        Box::pin(async move {
            request.validate()?;
            let query = prefix(&request.query, MAX_EMBED_INPUT_BYTES).to_string();
            let mut vectors = self
                .embedder
                .embed(std::slice::from_ref(&query))
                .await
                .map_err(|_| MemoryError::Unavailable)?;
            eve_semantic_api::validate_vectors(self.embedder.profile(), 1, &vectors)
                .map_err(|_| MemoryError::Unavailable)?;
            let query = vectors.pop().ok_or(MemoryError::Unavailable)?;
            // 词项结果与语义候选须来自同一记忆修订；并发写入时重读，仍不一致则报错。
            for _ in 0..3 {
                let snapshot = self.memory.snapshot()?;
                let lexical = self.lexical.recall(&MemoryRecallRequest {
                    query: request.query.clone(),
                    limit: MAX_RECALL_RESULTS,
                })?;
                if lexical.revision != snapshot.revision || snapshot.scope != self.scope {
                    continue;
                }
                return self.fuse(request, &query, snapshot, lexical);
            }
            Err(MemoryError::StaleRevision)
        })
    }
}

impl HybridReader {
    fn fuse(
        &self,
        request: &MemoryRecallRequest,
        query: &[f32],
        snapshot: MemorySnapshot,
        lexical: MemoryRecallResponse,
    ) -> MemoryResult<MemoryRecallResponse> {
        let profile = self.embedder.profile();
        let scope = scope_key(&self.scope);
        let entries: BTreeMap<String, _> = self
            .index
            .entries()
            .map_err(|_| MemoryError::Unavailable)?
            .into_iter()
            .filter(|entry| entry.belongs_to(profile) && entry.scope == scope)
            .map(|entry| (entry.item.clone(), entry))
            .collect();
        let mut semantic = Vec::new();
        for item in items(&snapshot)? {
            let input = prefix(&item.text, MAX_EMBED_INPUT_BYTES).trim();
            let Some(entry) = entries
                .get(&item.key)
                .filter(|entry| entry.content_sha256 == content_hash(input))
            else {
                continue;
            };
            let vector = entry.restore().ok_or(MemoryError::CorruptState)?;
            let similarity = cosine(query, &vector);
            if similarity >= MIN_SIMILARITY {
                semantic.push((similarity, item));
            }
        }
        semantic.sort_by(|left, right| {
            right
                .0
                .total_cmp(&left.0)
                .then_with(|| right.1.evidence_revision.cmp(&left.1.evidence_revision))
                .then_with(|| left.1.key.cmp(&right.1.key))
        });
        let mut fused: BTreeMap<String, (f64, MemoryRecallHit)> = BTreeMap::new();
        for (rank, hit) in lexical.hits.into_iter().enumerate() {
            let key = source_key(&hit.source);
            fused.insert(key, (1.0 / (RRF_K + rank as f64 + 1.0), hit));
        }
        for (rank, (_, item)) in semantic.into_iter().enumerate() {
            let contribution = 1.0 / (RRF_K + rank as f64 + 1.0);
            let entry = fused.entry(item.key.clone()).or_insert_with(|| {
                let (excerpt, excerpt_truncated) = excerpt(&item.text);
                (
                    0.0,
                    MemoryRecallHit {
                        score: 0,
                        source: item.source.clone(),
                        excerpt,
                        excerpt_truncated,
                    },
                )
            });
            entry.0 += contribution;
        }
        let mut ranked: Vec<(String, f64, MemoryRecallHit)> = fused
            .into_iter()
            .map(|(key, (score, hit))| (key, score, hit))
            .collect();
        ranked.sort_by(|left, right| {
            right
                .1
                .total_cmp(&left.1)
                .then_with(|| left.0.cmp(&right.0))
        });
        let mut response = MemoryRecallResponse {
            scope: snapshot.scope,
            revision: snapshot.revision,
            hits: Vec::new(),
        };
        for (_, score, mut hit) in ranked {
            if response.hits.len() == request.limit {
                break;
            }
            hit.score = ((score * 1_000_000.0).round() as u32).max(1);
            response.hits.push(hit);
            let encoded = serde_json::to_vec(&response).map_err(|_| MemoryError::CorruptState)?;
            if encoded.len() > MAX_RECALL_RESPONSE_BYTES {
                response.hits.pop();
            }
        }
        response.validate_for(&self.scope, request)?;
        Ok(response)
    }
}

/// 语义命中没有匹配位置，取正文开头；超长时以省略号结尾，合计不超过片段上限。
fn excerpt(text: &str) -> (String, bool) {
    if text.len() <= MAX_RECALL_EXCERPT_BYTES {
        return (text.to_string(), false);
    }
    let head = prefix(text, MAX_RECALL_EXCERPT_BYTES - ELLIPSIS.len());
    (format!("{head}{ELLIPSIS}"), true)
}
