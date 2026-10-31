//! 原生确认策略只保留本批和偏好历史来源；全页核验及旧策略兼容分别验收。
use eve_learning_api::*;
use eve_learning_plugin::{
    EvidenceConfirmationPolicy, constrain_decision, constrain_decision_reader,
};
use eve_memory_api::*;

#[derive(Clone, Copy)]
enum History {
    Empty,
    Automatic,
    Manual,
    Revoked,
}
#[derive(Clone)]
struct Reader {
    history: History,
    fail_at: Option<usize>,
    scope: MemoryScope,
}
impl Reader {
    fn new(history: History) -> Self {
        Self {
            history,
            fail_at: None,
            scope: MemoryScope {
                channel: "qq".into(),
                session_id: "paging".into(),
                user_id: "alice".into(),
            },
        }
    }
    fn evidence(&self, n: usize) -> InteractionEvidence {
        let manual = (n == 1 && matches!(self.history, History::Manual | History::Revoked))
            || (n == 2 && matches!(self.history, History::Revoked));
        let revision = n as u64 + u64::from(n > 1 && matches!(self.history, History::Automatic));
        InteractionEvidence {
            id: format!("e{n}"),
            revision,
            at_ms: n as u64,
            source: if manual {
                EvidenceSource::UserStatement {
                    message_id: format!("m{n}"),
                    text: if n == 1 {
                        "/remember 回复最多2段"
                    } else {
                        "/forget original"
                    }
                    .into(),
                }
            } else {
                EvidenceSource::CompletedInteraction {
                    message_id: format!("m{n}"),
                    session_revision: n as u64 * 2,
                    turn_id: n as u64,
                    user_text: "本轮明确偏好".into(),
                    assistant_text: "实际回复".into(),
                }
            },
        }
    }
    fn summary(&self) -> MemoryScanSummary {
        let mut preferences = vec![];
        if !matches!(self.history, History::Empty) {
            let text = "回复最多2段".to_owned();
            let mut history = vec![PreferenceVersion {
                revision: 1,
                evidence_id: "e1".into(),
                at_ms: 2,
                text: text.clone(),
                status: PreferenceStatus::Confirmed,
            }];
            if matches!(self.history, History::Revoked) {
                history.push(PreferenceVersion {
                    revision: 2,
                    evidence_id: "e2".into(),
                    at_ms: 3,
                    text: text.clone(),
                    status: PreferenceStatus::Revoked,
                });
            }
            preferences.push(Preference {
                id: "original".into(),
                text,
                revision: history.len() as u64,
                status: history.last().unwrap().status.clone(),
                history,
            });
        }
        MemoryScanSummary {
            scope: self.scope.clone(),
            revision: 6000 + u64::from(matches!(self.history, History::Automatic)),
            preferences,
            evidence_count: 6000,
        }
    }
    fn batch(&self) -> (LearningBatch, PreferenceCandidate) {
        let batch = LearningBatch {
            attempt: 0,
            id: "batch".into(),
            scope: self.scope.clone(),
            extractor_version: "test-v1".into(),
            started_at_ms: 7000,
            evidence: vec![self.evidence(100), self.evidence(101)],
        };
        let candidate = PreferenceCandidate {
            id: "candidate".into(),
            batch_id: batch.id.clone(),
            draft: CandidateDraft {
                text: "回复最多3段".into(),
                confidence: 90,
                evidence_ids: batch.evidence.iter().map(|e| e.id.clone()).collect(),
            },
            created_at_ms: 7001,
            expires_at_ms: 9000,
        };
        (batch, candidate)
    }
    fn whole(&self) -> MemorySnapshot {
        let summary = self.summary();
        MemorySnapshot {
            scope: summary.scope,
            revision: summary.revision,
            preferences: summary.preferences,
            evidence: (1..=6000).map(|n| self.evidence(n)).collect(),
            archived_evidence: 0,
        }
    }
}
struct Cursor {
    reader: Reader,
    summary: MemoryScanSummary,
    position: usize,
}
impl MemoryEvidenceCursor for Cursor {
    fn summary(&self) -> &MemoryScanSummary {
        &self.summary
    }
    fn next_page(&mut self) -> MemoryResult<Vec<InteractionEvidence>> {
        if self.reader.fail_at.is_some_and(|n| self.position >= n) {
            return Err(MemoryError::Storage);
        }
        let end = (self.position + MAX_ARCHIVE_PAGE).min(6000);
        let values = (self.position + 1..=end)
            .map(|n| self.reader.evidence(n))
            .collect();
        self.position = end;
        Ok(values)
    }
}
impl MemoryService for Reader {
    fn snapshot(&self) -> MemoryResult<MemorySnapshot> {
        panic!("原生策略不得读取全部正文")
    }
    fn complete_snapshot(&self) -> MemoryResult<MemorySnapshot> {
        panic!("原生策略不得读取全部正文")
    }
    fn scan(&self) -> MemoryResult<Box<dyn MemoryEvidenceCursor>> {
        Ok(Box::new(Cursor {
            reader: self.clone(),
            summary: self.summary(),
            position: 0,
        }))
    }
}

#[test]
fn paged_confirmation_matches_full_rules_for_confirmation_update_manual_and_revoked_history() {
    let policy = EvidenceConfirmationPolicy;
    for history in [
        History::Empty,
        History::Automatic,
        History::Manual,
        History::Revoked,
    ] {
        let reader = Reader::new(history);
        let (batch, candidate) = reader.batch();
        let whole = reader.whole();
        let expected = policy.decide(&candidate, &batch, &whole, 8000).unwrap();
        let actual = policy
            .decide_reader(&candidate, &batch, &reader, 8000)
            .unwrap();
        assert_eq!(actual, expected);
        if matches!(history, History::Manual | History::Revoked) {
            let mut forced = actual.clone();
            forced.action = LearningDecisionAction::Confirm;
            forced.reason = DecisionReason::Eligible;
            let constrained = constrain_decision_reader(
                &candidate,
                &batch,
                &reader,
                whole.revision,
                8000,
                forced,
            )
            .unwrap();
            assert!(!matches!(
                constrained.action,
                LearningDecisionAction::Confirm | LearningDecisionAction::Update { .. }
            ));
        }
        let expected = constrain_decision(&candidate, &batch, &whole, 8000, expected).unwrap();
        assert_eq!(
            constrain_decision_reader(&candidate, &batch, &reader, whole.revision, 8000, actual)
                .unwrap(),
            expected
        );
    }
}

#[test]
fn matched_or_expired_candidate_cannot_skip_the_final_page_error() {
    let policy = EvidenceConfirmationPolicy;
    let mut reader = Reader::new(History::Empty);
    let (batch, mut candidate) = reader.batch();
    let proposal = policy
        .decide_reader(&candidate, &batch, &reader, 8000)
        .unwrap();
    reader.fail_at = Some(6000);
    assert_eq!(
        policy.decide_reader(&candidate, &batch, &reader, 8000),
        Err(LearningError::Memory(MemoryError::Storage))
    );
    assert_eq!(
        constrain_decision_reader(&candidate, &batch, &reader, 6000, 8000, proposal),
        Err(LearningError::Memory(MemoryError::Storage))
    );
    candidate.expires_at_ms = 7500;
    assert_eq!(
        policy.decide_reader(&candidate, &batch, &reader, 8000),
        Err(LearningError::Memory(MemoryError::Storage))
    );
}

#[test]
fn exact_batch_sources_scope_and_revision_are_checked_independently_of_proposal() {
    let policy = EvidenceConfirmationPolicy;
    let reader = Reader::new(History::Empty);
    let (mut batch, candidate) = reader.batch();
    let proposal = policy
        .decide_reader(&candidate, &batch, &reader, 8000)
        .unwrap();
    assert_eq!(
        constrain_decision_reader(&candidate, &batch, &reader, 5999, 8000, proposal.clone()),
        Err(LearningError::Memory(MemoryError::StaleRevision))
    );
    let mut forged = proposal.clone();
    forged.memory_revision -= 1;
    assert_eq!(
        constrain_decision_reader(&candidate, &batch, &reader, 6000, 8000, forged),
        Err(LearningError::InvalidInput)
    );
    batch.evidence[0].at_ms += 1;
    assert_eq!(
        policy.decide_reader(&candidate, &batch, &reader, 8000),
        Err(LearningError::Conflict)
    );
    let (mut batch, _) = reader.batch();
    batch.scope.user_id = "bob".into();
    assert!(
        policy
            .decide_reader(&candidate, &batch, &reader, 8000)
            .is_err()
    );
}

struct Legacy;
impl AutoConfirmationPolicy for Legacy {
    fn allows(
        &self,
        _: &PreferenceCandidate,
        _: &LearningBatch,
        memory: &MemorySnapshot,
        _: u64,
    ) -> LearningResult<bool> {
        assert_eq!(memory.evidence.len(), 6000, "旧策略仍收到全部正文");
        Ok(true)
    }
}
struct Whole(Reader);
impl MemoryService for Whole {
    fn snapshot(&self) -> MemoryResult<MemorySnapshot> {
        Ok(self.0.whole())
    }
}
#[test]
fn legacy_policy_keeps_complete_snapshot_semantics() {
    let reader = Reader::new(History::Empty);
    let (batch, candidate) = reader.batch();
    let decision = Legacy
        .decide_reader(&candidate, &batch, &Whole(reader), 8000)
        .unwrap();
    assert_eq!(decision.action, LearningDecisionAction::Confirm);
    assert_eq!(decision.policy_version, "legacy-auto-confirm-v1");
}
