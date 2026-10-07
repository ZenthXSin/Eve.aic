//! 偏好提炼批次、候选与学习决策到面板只读契约的投影。
//! 与 QQ `/memory-decision` 共用候选、决策账本与实际保存的核对函数，结论保持一致。
use crate::qq_learning_commands::{candidates, linked_candidate, validate_records};
use crate::web_panel_memory::MemoryView;
use eve_learning_api::{
    DecisionReason, JobStatus, LearningAdmin, LearningDecisionAction, LearningFailure,
};
use eve_memory_api::{MemoryScope, PreferenceStatus};
use eve_web_panel_api::*;
use std::sync::Arc;

/// 每个候选最多列出的决策条数，与 `/memory-decision` 一致。
const DECISION_LIMIT: usize = 8;

/// 宿主持有的学习管理能力只经此类型的读取方法进入面板；记录决策、预留批次等写入不在其中。
pub(crate) struct LearningRead {
    admin: Arc<dyn LearningAdmin>,
    autonomous: bool,
}
impl LearningRead {
    pub(crate) fn new(admin: Arc<dyn LearningAdmin>, autonomous: bool) -> Self {
        Self { admin, autonomous }
    }
}

fn reason(value: &DecisionReason) -> &'static str {
    match value {
        DecisionReason::Eligible => "eligible",
        DecisionReason::EvidenceThreshold => "evidence_threshold",
        DecisionReason::Expired => "expired",
        DecisionReason::PolicyDenied => "policy_denied",
        DecisionReason::Duplicate => "duplicate",
        DecisionReason::RevokedConflict => "revoked_conflict",
        DecisionReason::ManualConflict => "manual_conflict",
        DecisionReason::AmbiguousConflict => "ambiguous_conflict",
        DecisionReason::StaleEvidence => "stale_evidence",
        DecisionReason::ExplicitRevisionUpdate => "explicit_revision_update",
        DecisionReason::AlreadyLinked => "already_linked",
    }
}

pub(crate) fn learning(
    read: &LearningRead,
    memory: &MemoryView,
    scope: &MemoryScope,
    now_ms: u64,
) -> PanelResult<LearningView> {
    // 与 `/memory-decision` 相同顺序：先固定记忆快照，再读只追加的决策与候选。
    let snapshot = memory.snapshot(scope)?;
    let records = read
        .admin
        .decisions(scope)
        .map_err(|_| PanelError::Unavailable)?;
    let learning = read
        .admin
        .snapshot(scope)
        .map_err(|_| PanelError::Unavailable)?;
    // 候选或账本校验失败说明状态不一致；明确报告不可用，不显示部分结果。
    let candidates = candidates(&learning, scope).map_err(|_| PanelError::Unavailable)?;
    validate_records(&records, &candidates).map_err(|_| PanelError::Unavailable)?;
    let mut jobs: Vec<_> = learning
        .jobs
        .iter()
        .map(|job| {
            let (status, failure) = match &job.status {
                JobStatus::Running => ("running", None),
                JobStatus::Completed => ("completed", None),
                JobStatus::Interrupted => ("interrupted", None),
                JobStatus::Failed(failure) => (
                    "failed",
                    Some(match failure {
                        LearningFailure::Provider => "provider",
                        LearningFailure::InvalidOutput => "invalid_output",
                        LearningFailure::Timeout => "timeout",
                        LearningFailure::Cancelled => "cancelled",
                    }),
                ),
            };
            LearningJobView {
                batch_id: job.batch.id.clone(),
                status,
                failure,
                started_at_ms: job.batch.started_at_ms,
                finished_at_ms: job.finished_at_ms,
                evidence: job.batch.evidence.len(),
                candidates: job.candidates.len(),
            }
        })
        .collect();
    jobs.sort_by(|a, b| {
        b.started_at_ms
            .cmp(&a.started_at_ms)
            .then_with(|| a.batch_id.cmp(&b.batch_id))
    });
    let candidates = candidates
        .iter()
        .map(|candidate| {
            let saved = linked_candidate(&snapshot, candidate, &records)
                .map_err(|_| PanelError::Unavailable)?
                .map(|preference| LinkedPreference {
                    preference_id: preference.id.clone(),
                    revision: preference.revision,
                    status: match preference.status {
                        PreferenceStatus::Confirmed => "confirmed",
                        PreferenceStatus::Revoked => "revoked",
                    },
                    effective: preference.status == PreferenceStatus::Confirmed,
                });
            let history: Vec<_> = records
                .iter()
                .filter(|record| record.decision.candidate_id == candidate.id)
                .collect();
            let decisions = history
                .iter()
                .rev()
                .take(DECISION_LIMIT)
                .map(|record| {
                    let decision = &record.decision;
                    let (action, update_preference, update_revision) = match &decision.action {
                        LearningDecisionAction::Confirm => ("confirm", None, None),
                        LearningDecisionAction::Update {
                            preference_id,
                            expected_revision,
                        } => (
                            "update",
                            Some(preference_id.clone()),
                            Some(*expected_revision),
                        ),
                        LearningDecisionAction::Defer => ("defer", None, None),
                        LearningDecisionAction::Reject => ("reject", None, None),
                    };
                    LearningDecisionView {
                        sequence: record.sequence,
                        at_ms: record.at_ms,
                        action,
                        update_preference,
                        update_revision,
                        reason: reason(&decision.reason),
                        policy_version: decision.policy_version.clone(),
                        memory_revision: decision.memory_revision,
                    }
                })
                .collect();
            Ok(LearningCandidateView {
                id: candidate.id.clone(),
                batch_id: candidate.batch_id.clone(),
                text: candidate.draft.text.clone(),
                confidence: candidate.draft.confidence,
                evidence_ids: candidate.draft.evidence_ids.clone(),
                created_at_ms: candidate.created_at_ms,
                expires_at_ms: candidate.expires_at_ms,
                expired: saved.is_none() && candidate.expires_at_ms <= now_ms,
                saved,
                decisions,
                decisions_total: history.len(),
            })
        })
        .collect::<PanelResult<Vec<_>>>()?;
    Ok(LearningView {
        scope: scope.clone(),
        autonomous: read.autonomous,
        jobs,
        candidates,
        decisions_total: records.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_learning_api::{
        CandidateDraft, LearningBatch, LearningDecision, LearningDecisionRecord, LearningJob,
        LearningOptions, LearningOutcome, LearningResult, LearningSnapshot, PreferenceCandidate,
        preference_id,
    };
    use eve_memory_api::{
        CompletedInteraction, EvidenceSource, InteractionEvidence, MemoryAdmin, MemoryResult,
        MemoryService, MemorySnapshot, Preference, PreferenceChange, PreferenceVersion,
    };
    use std::sync::Mutex;

    struct Memory(MemorySnapshot);
    impl MemoryService for Memory {
        fn snapshot(&self) -> MemoryResult<MemorySnapshot> {
            Ok(self.0.clone())
        }
    }
    impl MemoryAdmin for Memory {
        fn scopes(&self) -> MemoryResult<Vec<MemoryScope>> {
            Ok(vec![self.0.scope.clone()])
        }
        fn reader(&self, _: MemoryScope) -> MemoryResult<Arc<dyn MemoryService>> {
            Ok(Arc::new(Memory(self.0.clone())))
        }
        fn import_completed(
            &self,
            _: &MemoryScope,
            _: u64,
            _: CompletedInteraction,
        ) -> MemoryResult<MemorySnapshot> {
            panic!("面板不得写记忆");
        }
        fn update_preference(
            &self,
            _: &MemoryScope,
            _: u64,
            _: PreferenceChange,
        ) -> MemoryResult<MemorySnapshot> {
            panic!("面板不得写记忆");
        }
    }
    struct Learning {
        snapshot: LearningSnapshot,
        records: Mutex<Vec<LearningDecisionRecord>>,
    }
    impl LearningAdmin for Learning {
        fn snapshot(&self, _: &MemoryScope) -> LearningResult<LearningSnapshot> {
            Ok(self.snapshot.clone())
        }
        fn decisions(&self, _: &MemoryScope) -> LearningResult<Vec<LearningDecisionRecord>> {
            Ok(self.records.lock().unwrap().clone())
        }
        fn reserve(
            &self,
            _: &MemorySnapshot,
            _: u64,
            _: &str,
            _: &LearningOptions,
        ) -> LearningResult<Option<LearningBatch>> {
            panic!("面板不得预留批次");
        }
        fn finish(&self, _: &LearningBatch, _: u64, _: LearningOutcome) -> LearningResult<()> {
            panic!("面板不得结束批次");
        }
    }

    fn scope() -> MemoryScope {
        MemoryScope {
            channel: "qq".into(),
            session_id: "session".into(),
            user_id: "user".into(),
        }
    }
    fn interaction(id: &str) -> InteractionEvidence {
        InteractionEvidence {
            id: id.into(),
            revision: 1,
            at_ms: 1,
            source: EvidenceSource::CompletedInteraction {
                message_id: format!("message-{id}"),
                session_revision: 1,
                turn_id: 1,
                user_text: "先说结论".into(),
                assistant_text: "好的".into(),
            },
        }
    }
    fn job(index: u64, status: JobStatus, expires_at_ms: u64) -> LearningJob {
        let evidence = format!("completed-{index}");
        let candidates = if status == JobStatus::Completed {
            vec![PreferenceCandidate {
                id: format!("candidate-{index}"),
                batch_id: format!("batch-{index}"),
                draft: CandidateDraft {
                    text: format!("第 {index} 条候选"),
                    confidence: 80,
                    evidence_ids: vec![evidence.clone()],
                },
                created_at_ms: 2 + index,
                expires_at_ms,
            }]
        } else {
            vec![]
        };
        LearningJob {
            batch: LearningBatch {
                id: format!("batch-{index}"),
                scope: scope(),
                extractor_version: "fixture-v1".into(),
                started_at_ms: 1 + index,
                evidence: vec![interaction(&evidence)],
            },
            finished_at_ms: Some(2 + index),
            status,
            candidates,
        }
    }
    fn decision(
        sequence: u64,
        index: u64,
        action: LearningDecisionAction,
        reason: DecisionReason,
    ) -> LearningDecisionRecord {
        LearningDecisionRecord {
            sequence,
            at_ms: 3 + index,
            decision: LearningDecision {
                candidate_id: format!("candidate-{index}"),
                batch_id: format!("batch-{index}"),
                policy_version: "policy-v1".into(),
                memory_revision: 1,
                evidence_ids: vec![format!("completed-{index}")],
                action,
                reason,
            },
        }
    }
    /// candidate-0 已经自动确认保存；candidate-1 暂缓且已过首次确认期限；batch-2 超时失败。
    fn fixture() -> (LearningRead, MemoryView, Arc<Learning>) {
        let memory = MemorySnapshot {
            scope: scope(),
            revision: 2,
            evidence: vec![interaction("completed-0"), interaction("completed-1")],
            preferences: vec![Preference {
                id: preference_id("candidate-0"),
                text: "第 0 条候选".into(),
                status: PreferenceStatus::Confirmed,
                revision: 1,
                history: vec![PreferenceVersion {
                    revision: 1,
                    evidence_id: "completed-0".into(),
                    at_ms: 4,
                    text: "第 0 条候选".into(),
                    status: PreferenceStatus::Confirmed,
                }],
            }],
        };
        let learning = Arc::new(Learning {
            snapshot: LearningSnapshot {
                scope: scope(),
                jobs: vec![
                    job(0, JobStatus::Completed, u64::MAX),
                    job(1, JobStatus::Completed, 5),
                    job(2, JobStatus::Failed(LearningFailure::Timeout), 0),
                ],
            },
            records: Mutex::new(vec![
                decision(
                    1,
                    0,
                    LearningDecisionAction::Confirm,
                    DecisionReason::Eligible,
                ),
                decision(
                    2,
                    1,
                    LearningDecisionAction::Defer,
                    DecisionReason::EvidenceThreshold,
                ),
            ]),
        });
        (
            LearningRead::new(learning.clone(), true),
            MemoryView::new(Arc::new(Memory(memory))),
            learning,
        )
    }

    #[test]
    fn candidates_show_decisions_and_saved_state_checked_against_memory_history() {
        let (read, memory, _) = fixture();
        let view = learning(&read, &memory, &scope(), 10).unwrap();
        assert!(view.autonomous);
        assert_eq!(view.decisions_total, 2);
        let jobs: Vec<_> = view
            .jobs
            .iter()
            .map(|job| {
                (
                    job.batch_id.as_str(),
                    job.status,
                    job.failure,
                    job.candidates,
                )
            })
            .collect();
        assert_eq!(
            jobs,
            [
                ("batch-2", "failed", Some("timeout"), 0),
                ("batch-1", "completed", None, 1),
                ("batch-0", "completed", None, 1),
            ]
        );
        let rows: Vec<_> = view
            .candidates
            .iter()
            .map(|c| {
                (
                    c.id.as_str(),
                    c.saved.is_some(),
                    c.expired,
                    c.decisions_total,
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                ("candidate-1", false, true, 1),
                ("candidate-0", true, false, 1)
            ]
        );
        let saved = view.candidates[1].saved.as_ref().unwrap();
        assert_eq!(
            (
                saved.preference_id.as_str(),
                saved.revision,
                saved.status,
                saved.effective
            ),
            ("learned-candidate-0", 1, "confirmed", true)
        );
        let decision = &view.candidates[0].decisions[0];
        assert_eq!(
            (decision.sequence, decision.action, decision.reason),
            (2, "defer", "evidence_threshold")
        );
        assert_eq!(view.candidates[1].decisions[0].action, "confirm");
        assert_eq!(view.candidates[1].confidence, 80);
    }

    #[test]
    fn decision_alone_never_counts_as_saved_and_bad_ledgers_are_unavailable() {
        let (read, memory, admin) = fixture();
        // 候选 1 只有确认意图、没有记忆历史：仍显示未保存。
        admin.records.lock().unwrap().push(decision(
            3,
            1,
            LearningDecisionAction::Confirm,
            DecisionReason::Eligible,
        ));
        let view = learning(&read, &memory, &scope(), 1).unwrap();
        let pending = view
            .candidates
            .iter()
            .find(|c| c.id == "candidate-1")
            .unwrap();
        assert!(pending.saved.is_none());
        assert!(!pending.expired, "尚未过期");
        assert_eq!(pending.decisions[0].action, "confirm");

        admin.records.lock().unwrap().push(decision(
            9,
            1,
            LearningDecisionAction::Reject,
            DecisionReason::Duplicate,
        ));
        assert!(matches!(
            learning(&read, &memory, &scope(), 1),
            Err(PanelError::Unavailable)
        ));
        let other = MemoryScope {
            user_id: "other".into(),
            ..scope()
        };
        assert!(matches!(
            learning(&read, &memory, &other, 1),
            Err(PanelError::NotFound)
        ));
    }
}
