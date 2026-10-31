//! 混合召回：同一快照上的词项召回与按向量相似度排序的语义召回，用倒数排名融合（RRF）合并。
//! 只读绑定范围内的现行偏好与已导入交互；未建索引或正文已变的条目只参与词项召回。
use crate::{
    SemanticIndex,
    items::{
        LayoutPart, LayoutScan, content_hash, embed_input, prefix, scan_layout, scope_key,
        source_key,
    },
    segments::{SegmentKey, content_key, profile_key},
};
use eve_memory_api::*;
use eve_semantic_api::{EmbeddingProvider, MAX_EMBED_INPUT_BYTES, cosine};
use std::{collections::BTreeMap, sync::Arc};

/// 低于此余弦相似度的语义候选不纳入；分数不是事实置信度。
pub const MIN_SIMILARITY: f32 = 0.35;
/// RRF 常数：排名靠后的贡献平缓下降。
const RRF_K: f64 = 60.0;
const ELLIPSIS: &str = "…";

// 排序只需元数据及最终片段，不应为全部候选继续保留嵌入输入前缀。
struct SemanticCandidate {
    similarity: f32,
    evidence_revision: u64,
    key: String,
    hit: MemoryRecallHit,
}
impl SemanticCandidate {
    fn new(similarity: f32, item: crate::items::Item) -> Self {
        let (text, truncated) = excerpt(&item.text);
        Self {
            similarity,
            evidence_revision: item.evidence_revision,
            key: item.key,
            hit: MemoryRecallHit {
                score: 0,
                source: item.source,
                excerpt: text,
                excerpt_truncated: truncated,
            },
        }
    }
}

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
                let layout = self.scan(&query)?;
                let lexical_request = MemoryRecallRequest {
                    query: request.query.clone(),
                    limit: MAX_RECALL_RESULTS,
                };
                let lexical = self.lexical.recall(&lexical_request)?;
                lexical.validate_for(&self.scope, &lexical_request)?;
                if lexical.revision != layout.revision || layout.scope != self.scope {
                    continue;
                }
                return Self::fuse(
                    request,
                    layout.scope,
                    layout.revision,
                    layout.value,
                    lexical,
                );
            }
            Err(MemoryError::StaleRevision)
        })
    }
}

struct RecallScan {
    entries: BTreeMap<String, crate::store::IndexEntry>,
    semantic: Vec<SemanticCandidate>,
}
fn score_item(
    query: &[f32],
    semantic: &mut Vec<SemanticCandidate>,
    item: crate::items::Item,
    entry: Option<&crate::store::IndexEntry>,
) -> MemoryResult<()> {
    let input = prefix(&item.text, MAX_EMBED_INPUT_BYTES).trim();
    let Some(entry) = entry.filter(|entry| entry.content_sha256 == content_hash(input)) else {
        return Ok(());
    };
    let vector = entry.restore().ok_or(MemoryError::CorruptState)?;
    let similarity = cosine(query, &vector);
    if similarity >= MIN_SIMILARITY {
        semantic.push(SemanticCandidate::new(similarity, item));
    }
    Ok(())
}

impl HybridReader {
    fn scan(&self, query: &[f32]) -> MemoryResult<LayoutScan<Vec<SemanticCandidate>>> {
        let profile = self.embedder.profile();
        let scope = scope_key(&self.scope);
        let scanned = scan_layout(
            self.memory.as_ref(),
            self.index.segments().is_some(),
            |actual, _| {
                if *actual != self.scope {
                    return Err(MemoryError::CorruptState);
                }
                let entries = self
                    .index
                    .entries_for(profile, &scope)
                    .map_err(|_| MemoryError::Unavailable)?
                    .into_iter()
                    .map(|entry| (entry.item.clone(), entry))
                    .collect();
                Ok(RecallScan {
                    entries,
                    semantic: Vec::new(),
                })
            },
            |state, part| {
                match part {
                    LayoutPart::Ledger(items) => {
                        for item in items {
                            let entry = state.entries.get(&item.key);
                            score_item(query, &mut state.semantic, item, entry)?;
                        }
                    }
                    LayoutPart::Block(block) => {
                        let segments = self.index.segments().ok_or(MemoryError::CorruptState)?;
                        let pairs: Vec<_> = block
                            .items
                            .iter()
                            .filter_map(|item| {
                                Some((item.key.clone(), content_hash(&embed_input(item)?)))
                            })
                            .collect();
                        let key = SegmentKey {
                            profile: profile_key(profile),
                            scope: scope.clone(),
                            first_revision: block.first_revision,
                            last_revision: block.last_revision,
                            content: content_key(&pairs),
                        };
                        let loaded = segments
                            .load(profile, &scope, &[key])
                            .map_err(|_| MemoryError::CorruptState)?;
                        let segment_entries: BTreeMap<&str, _> = loaded
                            .iter()
                            .flat_map(|segment| segment.iter())
                            .map(|entry| (entry.item.as_str(), entry))
                            .collect();
                        for item in block.items {
                            let entry = segment_entries
                                .get(item.key.as_str())
                                .copied()
                                .or_else(|| state.entries.get(&item.key));
                            score_item(query, &mut state.semantic, item, entry)?;
                        }
                    }
                };
                Ok(())
            },
        )?;
        Ok(LayoutScan {
            scope: scanned.scope,
            revision: scanned.revision,
            value: scanned.value.semantic,
        })
    }
    fn fuse(
        request: &MemoryRecallRequest,
        scope: MemoryScope,
        revision: u64,
        mut semantic: Vec<SemanticCandidate>,
        lexical: MemoryRecallResponse,
    ) -> MemoryResult<MemoryRecallResponse> {
        semantic.sort_by(|left, right| {
            right
                .similarity
                .total_cmp(&left.similarity)
                .then_with(|| right.evidence_revision.cmp(&left.evidence_revision))
                .then_with(|| left.key.cmp(&right.key))
        });
        let mut fused: BTreeMap<String, (f64, MemoryRecallHit)> = BTreeMap::new();
        for (rank, hit) in lexical.hits.into_iter().enumerate() {
            let key = source_key(&hit.source);
            fused.insert(key, (1.0 / (RRF_K + rank as f64 + 1.0), hit));
        }
        for (rank, candidate) in semantic.into_iter().enumerate() {
            let contribution = 1.0 / (RRF_K + rank as f64 + 1.0);
            // 词项命中已经选择的原片段优先；同来源只累加贡献，不替换来源或片段。
            let entry = fused.entry(candidate.key).or_insert((0.0, candidate.hit));
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
            scope,
            revision,
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
        response.validate_for(&response.scope, request)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    fn scope() -> MemoryScope {
        MemoryScope {
            channel: "test".into(),
            session_id: "session".into(),
            user_id: "user".into(),
        }
    }
    fn item(id: &str, text: &str, revision: u64) -> crate::items::Item {
        crate::items::Item {
            key: format!("pref:{id}"),
            text: text.into(),
            evidence_revision: revision,
            source: MemoryRecallSource::ConfirmedPreference {
                preference_id: id.into(),
                preference_revision: 1,
                evidence_id: format!("e-{id}"),
                evidence_revision: revision,
                at_ms: 1,
            },
        }
    }
    fn id(hit: &MemoryRecallHit) -> &str {
        match &hit.source {
            MemoryRecallSource::ConfirmedPreference { preference_id, .. } => preference_id,
            _ => panic!("preference expected"),
        }
    }
    #[test]
    fn streamed_candidate_order_keeps_rrf_and_lexical_excerpt_priority() {
        let a = item("a", "语义的开头", 1);
        let lexical = MemoryRecallResponse {
            scope: scope(),
            revision: 100,
            hits: vec![MemoryRecallHit {
                score: 7,
                source: a.source.clone(),
                excerpt: "关键词附近的原片段".into(),
                excerpt_truncated: false,
            }],
        };
        let candidates = vec![
            SemanticCandidate::new(0.9, a),
            SemanticCandidate::new(0.9, item("b", &"汉字🦀".repeat(700), 2)),
            SemanticCandidate::new(0.8, item("c", "第三条", 3)),
        ];
        let response = HybridReader::fuse(
            &MemoryRecallRequest {
                query: "查询".into(),
                limit: 4,
            },
            scope(),
            100,
            candidates,
            lexical,
        )
        .unwrap();
        assert_eq!(
            response.hits.iter().map(id).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(response.hits[0].excerpt, "关键词附近的原片段");
        assert_eq!(
            response.hits[0].score,
            ((1.0f64 / 61.0 + 1.0 / 62.0) * 1_000_000.0).round() as u32
        );
        assert_eq!(
            response.hits[1].score,
            (1_000_000.0f64 / 61.0).round() as u32
        );
        assert!(response.hits[1].excerpt_truncated);
        assert!(response.hits[1].excerpt.ends_with('…'));
        assert!(response.hits[1].excerpt.len() <= MAX_RECALL_EXCERPT_BYTES);
        assert_eq!(
            response.hits[2].score,
            (1_000_000.0f64 / 63.0).round() as u32
        );
    }
    #[test]
    fn a_later_small_hit_survives_when_earlier_escaped_fragments_exhaust_byte_budget() {
        let mut candidates = Vec::new();
        for index in 0..8 {
            let text = format!("x{}", "\u{0001}".repeat(MAX_RECALL_EXCERPT_BYTES - 1));
            candidates.push(SemanticCandidate::new(
                1.0 - index as f32 / 100.0,
                item(&format!("big-{index}"), &text, index + 1),
            ));
        }
        candidates.push(SemanticCandidate::new(0.5, item("tiny", "短片段", 9)));
        let request = MemoryRecallRequest {
            query: "查询".into(),
            limit: MAX_RECALL_RESULTS,
        };
        let response = HybridReader::fuse(
            &request,
            scope(),
            100,
            candidates,
            MemoryRecallResponse {
                scope: scope(),
                revision: 100,
                hits: Vec::new(),
            },
        )
        .unwrap();
        assert!(response.hits.len() < MAX_RECALL_RESULTS);
        assert_eq!(id(response.hits.first().unwrap()), "big-0");
        assert_eq!(
            id(response.hits.last().unwrap()),
            "tiny",
            "cannot pretruncate candidates at request.limit"
        );
        assert!(serde_json::to_vec(&response).unwrap().len() <= MAX_RECALL_RESPONSE_BYTES);
    }
}
