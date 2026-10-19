use eve_memory_api::*;
use eve_memory_plugin::ExtractiveConversationDistiller;

fn snapshot() -> MemorySnapshot {
    MemorySnapshot {
        scope: MemoryScope {
            channel: "qq".into(),
            session_id: "s".into(),
            user_id: "u".into(),
        },
        revision: 6,
        preferences: vec![],
        evidence: (1..=6)
            .map(|i| InteractionEvidence {
                id: format!("e-{i}"),
                revision: i,
                at_ms: i,
                source: EvidenceSource::CompletedInteraction {
                    message_id: format!("m-{i}"),
                    session_revision: i * 2,
                    turn_id: i,
                    user_text: format!("用户原话{i}：{}", "自然表达".repeat(60)),
                    assistant_text: format!("历史助手{i}：{}", "这是当时的回复".repeat(60)),
                },
            })
            .collect(),
        archived_evidence: 0,
    }
}

#[test]
fn excerpts_preserve_verbatim_sources_without_confirming_claims_or_mutating_history() {
    let source = snapshot();
    let before = source.clone();
    let view = ExtractiveConversationDistiller
        .distill(&source, DistillationOptions::default(), &["e-5".into()])
        .unwrap();
    assert_eq!(view.method, "extractive-v1");
    assert_eq!(view.episodes.len(), 4);
    assert_eq!(view.episodes[0].evidence_id, "e-3");
    assert_eq!(view.episodes[3].evidence_id, "e-6");
    assert_eq!(view.expression_examples.len(), 1);
    assert_eq!(view.expression_examples[0].evidence_id, "e-5");
    for episode in &view.episodes {
        let evidence = source
            .evidence
            .iter()
            .find(|e| e.id == episode.evidence_id)
            .unwrap();
        let EvidenceSource::CompletedInteraction {
            user_text,
            assistant_text,
            ..
        } = &evidence.source
        else {
            panic!()
        };
        assert!(user_text.starts_with(&episode.user.text));
        assert!(assistant_text.starts_with(&episode.assistant.text));
        assert!(episode.user.truncated && episode.assistant.truncated);
        assert_eq!(episode.user.source_characters, user_text.chars().count());
    }
    assert!(!view.expression_examples[0].user.text.contains("历史助手"));
    assert_eq!(source, before);
    assert!(serde_json::to_vec(&view).unwrap().len() <= DistillationOptions::default().max_bytes);
}

#[test]
fn byte_budget_and_invalid_sources_never_truncate_json_or_create_evidence() {
    let mut source = snapshot();
    let options = DistillationOptions {
        max_bytes: 512,
        ..DistillationOptions::default()
    };
    let view = ExtractiveConversationDistiller
        .distill(&source, options, &["e-5".into()])
        .unwrap();
    assert!(serde_json::to_vec(&view).unwrap().len() <= 512);
    assert!(
        ExtractiveConversationDistiller
            .distill(&source, options, &["other-scope-evidence".into()])
            .is_err()
    );
    assert!(
        ExtractiveConversationDistiller
            .distill(&source, options, &["e-5".into(), "e-5".into()])
            .is_err()
    );
    source.evidence[5].revision = 1;
    assert!(
        ExtractiveConversationDistiller
            .distill(&source, options, &[])
            .is_err()
    );
}
