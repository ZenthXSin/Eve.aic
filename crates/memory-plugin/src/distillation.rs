//! 从不可变已送达交互取原文片段；不调用模型、不写状态、不推断事实或偏好。
use eve_memory_api::*;
use std::collections::BTreeSet;

pub struct ExtractiveConversationDistiller;

fn excerpt(text: &str, limit: usize) -> VerbatimExcerpt {
    let count = text.chars().count();
    VerbatimExcerpt {
        text: text.chars().take(limit).collect(),
        truncated: count > limit,
        source_characters: count,
    }
}

impl ConversationDistiller for ExtractiveConversationDistiller {
    fn distill(
        &self,
        snapshot: &MemorySnapshot,
        options: DistillationOptions,
        expression_evidence_ids: &[String],
    ) -> MemoryResult<ConversationDigest> {
        options.validate()?;
        snapshot.scope.validate()?;
        if snapshot.evidence.len() > MAX_EVIDENCE
            || expression_evidence_ids.len() > options.max_expression_examples
        {
            return Err(MemoryError::InvalidInput);
        }
        let wanted: BTreeSet<_> = expression_evidence_ids.iter().collect();
        if wanted.len() != expression_evidence_ids.len() {
            return Err(MemoryError::InvalidInput);
        }
        let mut seen = BTreeSet::new();
        let mut completed = vec![];
        let mut previous = 0;
        for evidence in &snapshot.evidence {
            validate_id(&evidence.id)?;
            if evidence.revision <= previous
                || evidence.revision > snapshot.revision
                || !seen.insert(&evidence.id)
            {
                return Err(MemoryError::InvalidInput);
            }
            previous = evidence.revision;
            if let EvidenceSource::CompletedInteraction {
                message_id,
                turn_id,
                session_revision,
                user_text,
                assistant_text,
            } = &evidence.source
            {
                validate_id(message_id)?;
                validate_text(user_text, MAX_TEXT_BYTES)?;
                validate_text(assistant_text, MAX_TEXT_BYTES)?;
                if *turn_id == 0 || turn_id.checked_mul(2).is_none_or(|r| *session_revision < r) {
                    return Err(MemoryError::InvalidInput);
                }
                completed.push((evidence, *turn_id, user_text, assistant_text));
            }
        }
        if wanted
            .iter()
            .any(|id| !completed.iter().any(|(e, _, _, _)| &e.id == *id))
        {
            return Err(MemoryError::InvalidInput);
        }
        let mut digest = ConversationDigest {
            kind: "eve.conversation-digest-v1".into(),
            method: "extractive-v1".into(),
            memory_revision: snapshot.revision,
            completed_interactions: completed.len(),
            episodes: vec![],
            expression_examples: vec![],
        };
        for (evidence, turn_id, user, assistant) in
            completed.iter().rev().take(options.max_episodes)
        {
            digest.episodes.push(ConversationEpisode {
                evidence_id: evidence.id.clone(),
                evidence_revision: evidence.revision,
                at_ms: evidence.at_ms,
                turn_id: *turn_id,
                user: excerpt(user, 96),
                assistant: excerpt(assistant, 160),
            });
        }
        for (evidence, _, user, _) in completed.iter().rev() {
            if wanted.contains(&evidence.id) {
                digest.expression_examples.push(ExpressionExample {
                    evidence_id: evidence.id.clone(),
                    user: excerpt(user, 128),
                });
            }
        }
        // 保留最新的完整片段条目；不用截断 JSON 或改变原文字句来凑预算。
        while serde_json::to_vec(&digest)
            .map_err(|_| MemoryError::InvalidInput)?
            .len()
            > options.max_bytes
        {
            if digest.episodes.pop().is_none() && digest.expression_examples.pop().is_none() {
                return Err(MemoryError::InvalidInput);
            }
        }
        digest.episodes.reverse();
        digest.expression_examples.reverse();
        Ok(digest)
    }
}
