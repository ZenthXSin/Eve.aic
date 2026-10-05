use eve_learning_api::*;
use eve_learning_plugin::EvidenceConfirmationPolicy;
use eve_memory_api::*;

fn input() -> (PreferenceCandidate, LearningBatch, MemorySnapshot) {
    let scope = MemoryScope {
        channel: "qq".into(),
        session_id: "private-alice".into(),
        user_id: "alice".into(),
    };
    let evidence: Vec<_> = (1..=2)
        .map(|i| InteractionEvidence {
            id: format!("source-{i}"),
            revision: i,
            at_ms: i,
            source: EvidenceSource::CompletedInteraction {
                message_id: format!("message-{i}"),
                session_revision: i * 2,
                turn_id: i,
                user_text: "每次回复请先说结论".into(),
                assistant_text: "收到".into(),
            },
        })
        .collect();
    let batch = LearningBatch {
        id: "batch".into(),
        scope: scope.clone(),
        extractor_version: "test-v1".into(),
        started_at_ms: 3,
        evidence: evidence.clone(),
    };
    let candidate = PreferenceCandidate {
        id: "candidate".into(),
        batch_id: batch.id.clone(),
        draft: CandidateDraft {
            text: "先说结论".into(),
            confidence: 80,
            evidence_ids: evidence.iter().map(|e| e.id.clone()).collect(),
        },
        created_at_ms: 4,
        expires_at_ms: 10,
    };
    let memory = MemorySnapshot {
        scope,
        revision: 2,
        evidence,
        preferences: vec![],
    };
    (candidate, batch, memory)
}

#[test]
fn auto_confirmation_requires_current_sources_sufficient_support_and_unexpired_candidate() {
    let (candidate, batch, memory) = input();
    let policy = EvidenceConfirmationPolicy;
    assert_eq!(policy.allows(&candidate, &batch, &memory, 4), Ok(true));
    for (score, count, time) in [(79, 2, 4), (100, 1, 4), (100, 2, 10), (100, 2, 3)] {
        let mut rejected = candidate.clone();
        rejected.draft.confidence = score;
        rejected.draft.evidence_ids.truncate(count);
        assert_eq!(policy.allows(&rejected, &batch, &memory, time), Ok(false));
    }
    let mut foreign = memory.clone();
    foreign.scope.user_id = "bob".into();
    assert!(policy.allows(&candidate, &batch, &foreign, 4).is_err());
    let mut changed = memory.clone();
    changed.evidence[0].at_ms += 1;
    assert!(policy.allows(&candidate, &batch, &changed, 4).is_err());
    let mut missing = memory;
    missing.evidence.pop();
    assert!(policy.allows(&candidate, &batch, &missing, 4).is_err());
    let mut unrelated = candidate;
    unrelated.batch_id = "another-batch".into();
    assert!(policy.allows(&unrelated, &batch, &missing, 4).is_err());
}
