//! 宿主进程内最近若干次实际判断的有界记录；只在内存中，不落盘，进程重启后清空。
//! 记录只含枚举、计数与本地时间，不含会话、用户、任务、消息 ID、正文或模型解释。
use crate::{BoundedRelationDiagnostics, RelationDiagnostics};
use eve_message_api::{
    DiscardRelationObservations, MessageIntent, RelationFuture, RelationInput, RelationJudge,
    RelationObservation, RelationObserver, RelationOutcome, observe_relation,
};
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub const MAX_RECENT_JUDGMENTS: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JudgmentResult {
    /// 判断器返回了通过校验的决定；动作是否执行仍由路由器决定。
    Decided {
        intents: Vec<MessageIntent>,
    },
    Failed {
        outcome: RelationOutcome,
    },
    /// 调用方在判断结束前丢弃了 Future（期限、取消或停止）；不表示远端已停止。
    Dropped,
}

#[derive(Clone, Debug, Serialize)]
pub struct RelationJudgmentRecord {
    /// 本进程内单调递增，从 1 开始；重启后重新编号。
    pub sequence: u64,
    /// 宿主本地时钟记录的结束时间，不是平台或服务端时间。
    pub finished_at_unix_ms: u64,
    /// 本地从调用开始到结束或丢弃的经过时间。
    pub elapsed_micros: u64,
    pub result: JudgmentResult,
    pub diagnostics: RelationDiagnostics,
}

#[derive(Clone, Debug)]
pub struct RecentJudgmentPage {
    /// 新的在前。
    pub records: Vec<RelationJudgmentRecord>,
    /// 还有更早记录时，用作下一页的 `before`。
    pub next_before: Option<u64>,
    pub recorded_total: u64,
    /// 超出容量后从缓冲中移出的最早记录数。
    pub evicted: u64,
    pub capacity: usize,
}

struct Ring {
    records: VecDeque<RelationJudgmentRecord>,
    next_sequence: u64,
    evicted: u64,
}

/// 多个判断可以并发写入；读写都只持锁做内存拷贝，不执行 I/O。
pub struct RecentRelationJudgments {
    ring: Mutex<Ring>,
}
impl Default for RecentRelationJudgments {
    fn default() -> Self {
        Self {
            ring: Mutex::new(Ring {
                records: VecDeque::with_capacity(MAX_RECENT_JUDGMENTS),
                next_sequence: 1,
                evicted: 0,
            }),
        }
    }
}
impl RecentRelationJudgments {
    fn record(
        &self,
        result: JudgmentResult,
        elapsed_micros: u64,
        diagnostics: RelationDiagnostics,
    ) {
        // 锁中毒时放弃这条记录，不影响业务判断。
        let Ok(mut ring) = self.ring.lock() else {
            return;
        };
        let sequence = ring.next_sequence;
        ring.next_sequence = sequence.saturating_add(1);
        let finished_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis().min(u64::MAX as u128) as u64);
        ring.records.push_back(RelationJudgmentRecord {
            sequence,
            finished_at_unix_ms,
            elapsed_micros,
            result,
            diagnostics,
        });
        while ring.records.len() > MAX_RECENT_JUDGMENTS {
            ring.records.pop_front();
            ring.evicted += 1;
        }
    }
    /// 读取序号小于 `before` 的最近记录，新的在前；锁不可用时返回 None。
    pub fn page(&self, before: Option<u64>, limit: usize) -> Option<RecentJudgmentPage> {
        let ring = self.ring.lock().ok()?;
        let mut older = ring
            .records
            .iter()
            .rev()
            .filter(|record| before.is_none_or(|before| record.sequence < before));
        let records: Vec<_> = older.by_ref().take(limit).cloned().collect();
        let next_before = older
            .next()
            .and(records.last().map(|record| record.sequence));
        Some(RecentJudgmentPage {
            records,
            next_before,
            recorded_total: ring.next_sequence - 1,
            evicted: ring.evicted,
            capacity: MAX_RECENT_JUDGMENTS,
        })
    }
}

struct Tee {
    outer: Arc<dyn RelationObserver>,
    collector: Arc<BoundedRelationDiagnostics>,
}
impl RelationObserver for Tee {
    fn observe(&self, observation: RelationObservation) {
        self.collector.observe(observation);
        observe_relation(self.outer.as_ref(), observation);
    }
}

/// 结束、失败、丢弃或 panic 时都只记录一次。
struct Pending {
    recent: Arc<RecentRelationJudgments>,
    collector: Arc<BoundedRelationDiagnostics>,
    started: Instant,
    result: Option<JudgmentResult>,
}
impl Drop for Pending {
    fn drop(&mut self) {
        let result = self.result.take().unwrap_or(if std::thread::panicking() {
            JudgmentResult::Failed {
                outcome: RelationOutcome::Panicked,
            }
        } else {
            JudgmentResult::Dropped
        });
        let elapsed = self.started.elapsed().as_micros().min(u64::MAX as u128) as u64;
        self.recent
            .record(result, elapsed, self.collector.snapshot());
    }
}

/// 组合层把最终判断器包一层：每次判断单独收集有界诊断，结束后写入最近记录。
/// 外层传入的观察器照常收到全部事件；不改变判断结果、期限或回退。
pub struct RecordingRelationJudge {
    inner: Arc<dyn RelationJudge>,
    recent: Arc<RecentRelationJudgments>,
}
impl RecordingRelationJudge {
    pub fn new(inner: Arc<dyn RelationJudge>, recent: Arc<RecentRelationJudgments>) -> Self {
        Self { inner, recent }
    }
}
impl RelationJudge for RecordingRelationJudge {
    fn judge(&self, input: RelationInput) -> RelationFuture<'_> {
        self.judge_observed(input, Arc::new(DiscardRelationObservations))
    }
    fn judge_observed(
        &self,
        input: RelationInput,
        observer: Arc<dyn RelationObserver>,
    ) -> RelationFuture<'_> {
        Box::pin(async move {
            let collector = Arc::new(BoundedRelationDiagnostics::default());
            let mut pending = Pending {
                recent: self.recent.clone(),
                collector: collector.clone(),
                started: Instant::now(),
                result: None,
            };
            let tee = Arc::new(Tee {
                outer: observer,
                collector,
            });
            let result = self.inner.judge_observed(input, tee).await;
            pending.result = Some(match &result {
                Ok(decision) => JudgmentResult::Decided {
                    intents: decision.parts.iter().map(|part| part.intent).collect(),
                },
                Err(error) => JudgmentResult::Failed {
                    outcome: (*error).into(),
                },
            });
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DiagnosticCoverage, RelationObservationGuard};
    use eve_control_api::{ControlPhase, GenerationKey};
    use eve_message_api::{
        IncomingMessage, IntentPart, RelationDecision, RelationError, RelationStage,
    };
    use eve_session_api::SessionKey;

    fn input() -> RelationInput {
        RelationInput {
            message: IncomingMessage {
                message_id: "secret-message".into(),
                text: "secret-text".into(),
                target: GenerationKey {
                    session: SessionKey::new("secret-session", "secret-user").unwrap(),
                    task_id: "secret-task".into(),
                    controller_epoch: [1; 16],
                    generation: 1,
                },
                reply_to: None,
            },
            phase: ControlPhase::Generating,
            task_text: "secret-task-text".into(),
            cancel_requested: false,
            started_tools: Some(0),
            clarification: None,
        }
    }
    /// 记录规则阶段；`mode` 决定返回决定、错误、永远等待或 panic。
    struct Staged(&'static str);
    impl RelationJudge for Staged {
        fn judge(&self, input: RelationInput) -> RelationFuture<'_> {
            self.judge_observed(input, Arc::new(DiscardRelationObservations))
        }
        fn judge_observed(
            &self,
            input: RelationInput,
            observer: Arc<dyn RelationObserver>,
        ) -> RelationFuture<'_> {
            Box::pin(async move {
                observe_relation(observer.as_ref(), RelationObservation::Supported);
                let guard = RelationObservationGuard::stage(observer, RelationStage::Rules);
                match self.0 {
                    "decide" => {
                        guard.finish(RelationOutcome::Completed);
                        Ok(RelationDecision {
                            target: input.message.target.clone(),
                            message_id: input.message.message_id.clone(),
                            parts: vec![IntentPart {
                                intent: MessageIntent::Cancel,
                                confidence: 100,
                                span: None,
                            }],
                            explanation: "secret-explanation".into(),
                        })
                    }
                    "fail" => {
                        guard.finish(RelationOutcome::Timeout);
                        Err(RelationError::Timeout)
                    }
                    "panic" => panic!("judge bug"),
                    _ => std::future::pending().await,
                }
            })
        }
    }
    struct Legacy;
    impl RelationJudge for Legacy {
        fn judge(&self, _: RelationInput) -> RelationFuture<'_> {
            Box::pin(async { Err(RelationError::Unavailable) })
        }
    }
    fn recording(
        mode: &'static str,
        recent: &Arc<RecentRelationJudgments>,
    ) -> RecordingRelationJudge {
        RecordingRelationJudge::new(Arc::new(Staged(mode)), recent.clone())
    }

    #[tokio::test]
    async fn decided_and_failed_judgments_are_recorded_and_outer_observer_still_sees_events() {
        let recent = Arc::new(RecentRelationJudgments::default());
        let outer = Arc::new(BoundedRelationDiagnostics::default());
        let decision = recording("decide", &recent)
            .judge_observed(input(), outer.clone())
            .await
            .unwrap();
        assert_eq!(decision.parts[0].intent, MessageIntent::Cancel);
        assert_eq!(outer.snapshot().coverage, DiagnosticCoverage::Complete);
        assert_eq!(
            recording("fail", &recent).judge(input()).await,
            Err(RelationError::Timeout)
        );
        let page = recent.page(None, 10).unwrap();
        assert_eq!(
            (page.recorded_total, page.evicted, page.next_before),
            (2, 0, None)
        );
        let [failed, decided] = page.records.as_slice() else {
            panic!("two records expected");
        };
        assert_eq!((failed.sequence, decided.sequence), (2, 1));
        assert_eq!(
            decided.result,
            JudgmentResult::Decided {
                intents: vec![MessageIntent::Cancel]
            }
        );
        assert_eq!(decided.diagnostics.coverage, DiagnosticCoverage::Complete);
        assert_eq!(
            decided.diagnostics.counts.as_ref().unwrap().rules_started,
            1
        );
        assert_eq!(
            failed.result,
            JudgmentResult::Failed {
                outcome: RelationOutcome::Timeout
            }
        );
        let json = serde_json::to_string(&page.records).unwrap();
        assert!(!json.contains("secret"), "{json}");
    }

    #[tokio::test]
    async fn dropped_panicked_and_unobservable_judgments_are_not_reported_as_success() {
        let recent = Arc::new(RecentRelationJudgments::default());
        let judge = recording("wait", &recent);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), judge.judge(input()))
                .await
                .is_err()
        );
        let panicking = Arc::new(recording("panic", &recent));
        let task = tokio::spawn({
            let judge = panicking.clone();
            async move { judge.judge(input()).await }
        });
        assert!(task.await.unwrap_err().is_panic());
        RecordingRelationJudge::new(Arc::new(Legacy), recent.clone())
            .judge(input())
            .await
            .unwrap_err();
        let page = recent.page(None, 10).unwrap();
        let [legacy, panicked, dropped] = page.records.as_slice() else {
            panic!("three records expected");
        };
        assert_eq!(dropped.result, JudgmentResult::Dropped);
        // 内层阶段因 Future 丢弃记录了 Dropped 终态，覆盖仍完整，但结果不是成功。
        assert_eq!(dropped.diagnostics.coverage, DiagnosticCoverage::Complete);
        assert!(
            dropped
                .diagnostics
                .events
                .contains(&RelationObservation::Finished {
                    operation: eve_message_api::RelationOperation::Stage(RelationStage::Rules),
                    outcome: RelationOutcome::Dropped,
                    elapsed_micros: match dropped.diagnostics.events[2] {
                        RelationObservation::Finished { elapsed_micros, .. } => elapsed_micros,
                        _ => panic!("finished event expected"),
                    },
                })
        );
        assert_eq!(
            panicked.result,
            JudgmentResult::Failed {
                outcome: RelationOutcome::Panicked
            }
        );
        assert_eq!(legacy.diagnostics.coverage, DiagnosticCoverage::Unsupported);
        assert_eq!(legacy.diagnostics.counts, None);
    }

    #[test]
    fn ring_keeps_the_newest_records_and_pages_backwards() {
        let recent = RecentRelationJudgments::default();
        for _ in 0..MAX_RECENT_JUDGMENTS + 2 {
            recent.record(
                JudgmentResult::Dropped,
                1,
                BoundedRelationDiagnostics::default().snapshot(),
            );
        }
        let first = recent.page(None, 50).unwrap();
        assert_eq!(
            (first.recorded_total, first.evicted, first.capacity),
            (130, 2, 128)
        );
        assert_eq!(first.records.first().unwrap().sequence, 130);
        assert_eq!(first.records.last().unwrap().sequence, 81);
        assert_eq!(first.next_before, Some(81));
        let rest = recent.page(first.next_before, 100).unwrap();
        assert_eq!(rest.records.len(), 78);
        assert_eq!(rest.records.last().unwrap().sequence, 3);
        assert_eq!(rest.next_before, None);
        assert!(recent.page(Some(3), 10).unwrap().records.is_empty());
    }
}
