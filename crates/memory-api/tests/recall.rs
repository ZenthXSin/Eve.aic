use eve_memory_api::*;

fn scope() -> MemoryScope {
    MemoryScope {
        channel: "qq".into(),
        session_id: "private-session".into(),
        user_id: "private-user".into(),
    }
}

fn request() -> MemoryRecallRequest {
    MemoryRecallRequest {
        query: "检索 private-query".into(),
        limit: DEFAULT_RECALL_RESULTS,
    }
}

fn preference() -> MemoryRecallHit {
    MemoryRecallHit {
        score: 10,
        source: MemoryRecallSource::ConfirmedPreference {
            preference_id: "private-preference".into(),
            preference_revision: 2,
            evidence_id: "private-evidence".into(),
            evidence_revision: 4,
            at_ms: 40,
        },
        excerpt: "private-excerpt 中文".into(),
        excerpt_truncated: false,
    }
}

fn completed(field: RecallField) -> MemoryRecallHit {
    MemoryRecallHit {
        score: 1,
        source: MemoryRecallSource::CompletedInteraction {
            evidence_id: "private-completed".into(),
            evidence_revision: 3,
            message_id: "private-message".into(),
            session_revision: 24,
            turn_id: 12,
            at_ms: 30,
            field,
        },
        excerpt: "private-interaction".into(),
        excerpt_truncated: true,
    }
}

fn response(hits: Vec<MemoryRecallHit>) -> MemoryRecallResponse {
    MemoryRecallResponse {
        scope: scope(),
        revision: 4,
        hits,
    }
}

#[test]
fn request_enforces_utf8_byte_and_result_bounds_without_requiring_latin_tokens() {
    for query in ["", "   ", "a\0b", "a\nb", "a\tb", "a\u{7f}b"] {
        assert_eq!(
            MemoryRecallRequest {
                query: query.into(),
                ..request()
            }
            .validate(),
            Err(MemoryError::InvalidInput)
        );
    }
    for query in [
        "中文问题",
        "MIXED Case terms",
        &"a".repeat(MAX_RECALL_QUERY_BYTES),
    ] {
        assert!(
            MemoryRecallRequest {
                query: query.into(),
                ..request()
            }
            .validate()
            .is_ok()
        );
    }
    for query in [
        "a".repeat(MAX_RECALL_QUERY_BYTES + 1),
        "中".repeat(MAX_RECALL_QUERY_BYTES / 3 + 1),
    ] {
        assert_eq!(
            MemoryRecallRequest { query, ..request() }.validate(),
            Err(MemoryError::InvalidInput)
        );
    }
    for limit in [0, MAX_RECALL_RESULTS + 1, usize::MAX] {
        assert_eq!(
            MemoryRecallRequest { limit, ..request() }.validate(),
            Err(MemoryError::InvalidInput)
        );
    }
    assert!(
        MemoryRecallRequest {
            limit: MAX_RECALL_RESULTS,
            ..request()
        }
        .validate()
        .is_ok()
    );
}

#[test]
fn response_keeps_snapshot_and_session_revisions_distinct_and_allows_both_turn_fields() {
    let result = response(vec![
        preference(),
        completed(RecallField::User),
        completed(RecallField::Assistant),
    ]);
    assert!(result.validate_for(&scope(), &request()).is_ok());
    let empty = MemoryRecallResponse {
        revision: 0,
        hits: vec![],
        ..response(vec![])
    };
    assert!(empty.validate_for(&scope(), &request()).is_ok());
}

#[test]
fn response_rejects_foreign_scope_bad_sources_and_duplicate_results() {
    let original = response(vec![preference()]);
    let mut variants = Vec::new();
    let mut foreign = original.clone();
    foreign.scope.channel = "terminal".into();
    variants.push(foreign);
    let mut foreign = original.clone();
    foreign.scope.session_id = "another-session".into();
    variants.push(foreign);
    let mut foreign = original.clone();
    foreign.scope.user_id = "another-user".into();
    variants.push(foreign);
    let mut zero_revision = original.clone();
    zero_revision.revision = 0;
    variants.push(zero_revision);
    let mut zero_score = original.clone();
    zero_score.hits[0].score = 0;
    variants.push(zero_score);
    for text in [
        String::new(),
        " ".into(),
        "bad\0text".into(),
        "a".repeat(MAX_RECALL_EXCERPT_BYTES + 1),
    ] {
        let mut invalid = original.clone();
        invalid.hits[0].excerpt = text;
        variants.push(invalid);
    }
    let mut duplicate = original.clone();
    duplicate.hits.push(preference());
    variants.push(duplicate);
    variants.push(response(vec![
        completed(RecallField::User),
        completed(RecallField::User),
    ]));
    for (preference_revision, evidence_revision) in [(0, 4), (5, 4), (2, 0), (2, 5)] {
        let mut invalid = original.clone();
        if let MemoryRecallSource::ConfirmedPreference {
            preference_revision: p,
            evidence_revision: e,
            ..
        } = &mut invalid.hits[0].source
        {
            *p = preference_revision;
            *e = evidence_revision;
        }
        variants.push(invalid);
    }
    for invalid in variants {
        assert_eq!(
            invalid.validate_for(&scope(), &request()),
            Err(MemoryError::InvalidInput)
        );
    }
    assert_eq!(
        original.validate_for(
            &scope(),
            &MemoryRecallRequest {
                limit: 0,
                ..request()
            }
        ),
        Err(MemoryError::InvalidInput)
    );
    let two = response(vec![preference(), completed(RecallField::User)]);
    assert_eq!(
        two.validate_for(
            &scope(),
            &MemoryRecallRequest {
                limit: 1,
                ..request()
            }
        ),
        Err(MemoryError::InvalidInput)
    );
}

#[test]
fn completed_source_requires_a_possible_saved_turn_and_nonempty_identifiers() {
    for (session_revision, turn_id) in [(0, 1), (1, 1), (24, 0), (23, 12), (u64::MAX, u64::MAX)] {
        let mut hit = completed(RecallField::User);
        if let MemoryRecallSource::CompletedInteraction {
            session_revision: s,
            turn_id: t,
            ..
        } = &mut hit.source
        {
            *s = session_revision;
            *t = turn_id;
        }
        assert_eq!(
            response(vec![hit]).validate_for(&scope(), &request()),
            Err(MemoryError::InvalidInput)
        );
    }
    for field in 0..3 {
        let mut hit = completed(RecallField::User);
        if let MemoryRecallSource::CompletedInteraction {
            evidence_id,
            message_id,
            evidence_revision,
            ..
        } = &mut hit.source
        {
            match field {
                0 => evidence_id.clear(),
                1 => *message_id = " invalid ".into(),
                _ => *evidence_revision = 0,
            }
        }
        assert_eq!(
            response(vec![hit]).validate_for(&scope(), &request()),
            Err(MemoryError::InvalidInput)
        );
    }
}

#[test]
fn debug_does_not_disclose_query_scope_excerpt_or_provenance_ids() {
    let text = format!(
        "{:?} {:?} {:?} {:?}",
        request(),
        response(vec![preference()]),
        preference(),
        completed(RecallField::User).source
    );
    for secret in [
        "private-query",
        "private-session",
        "private-user",
        "private-preference",
        "private-evidence",
        "private-excerpt",
        "private-completed",
        "private-message",
    ] {
        assert!(!text.contains(secret), "unexpected disclosure: {secret}");
    }
}

#[test]
fn json_escaping_is_counted_against_the_response_byte_budget() {
    let hits = (0..MAX_RECALL_RESULTS)
        .map(|index| {
            let mut hit = preference();
            if let MemoryRecallSource::ConfirmedPreference { preference_id, .. } = &mut hit.source {
                *preference_id = format!("p-{index}");
            }
            hit.excerpt = format!("x{}", "\u{1f}".repeat(MAX_RECALL_EXCERPT_BYTES - 1));
            hit
        })
        .collect();
    let result = response(hits);
    let request = MemoryRecallRequest {
        limit: MAX_RECALL_RESULTS,
        ..request()
    };
    assert!(serde_json::to_vec(&result).unwrap().len() > MAX_RECALL_RESPONSE_BYTES);
    assert_eq!(
        result.validate_for(&scope(), &request),
        Err(MemoryError::InvalidInput)
    );
}

#[test]
fn paired_fields_cannot_disagree_about_the_saved_interaction_origin() {
    for field in 0..5 {
        let mut assistant = completed(RecallField::Assistant);
        if let MemoryRecallSource::CompletedInteraction {
            evidence_revision,
            message_id,
            session_revision,
            turn_id,
            at_ms,
            ..
        } = &mut assistant.source
        {
            match field {
                0 => *evidence_revision = 4,
                1 => *message_id = "different-message".into(),
                2 => *session_revision = 26,
                3 => *turn_id = 11,
                _ => *at_ms += 1,
            }
        }
        assert_eq!(
            response(vec![completed(RecallField::User), assistant])
                .validate_for(&scope(), &request()),
            Err(MemoryError::InvalidInput)
        );
    }
}
