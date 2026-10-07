//! QQ 宿主到公开面板契约的组合；不从 StateStore 旁路读取或修改数据。
use eve_cognition_api::CognitionReader;
use eve_control_api::{
    CancelDisposition, CommitState, ControlError, ControlService, ControlSnapshot, GenerationKey,
};
use eve_message_api::{
    FallbackReason, MessageIntent, RelationAttempt, RelationObservation, RelationOperation,
    RelationOutcome, RelationStage,
};
use eve_message_diagnostics::{
    DiagnosticCoverage, JudgmentResult, RecentRelationJudgments, RelationJudgmentRecord,
};
use eve_qqbot_plugin::{QqBotStatus, QqBotStatusHandle};
use eve_session_api::{
    SessionError, SessionFailureCode, SessionKey, SessionService, SessionTurnStatus,
};
use eve_web_panel_api::*;
use std::sync::Arc;

pub(crate) struct QqPanel {
    pub sessions: Arc<dyn SessionService>,
    pub control: Arc<dyn ControlService>,
    pub channel: Arc<QqBotStatusHandle>,
    pub started_at_unix_ms: u64,
    /// 本进程最近判断与启动时选择的判断方式；未开启面板时不记录。
    pub judgments: Option<(Arc<RecentRelationJudgments>, &'static str)>,
    /// 开启 --cognition 时绑定 Internal 的只读句柄；面板不持有认知管理能力。
    pub cognition: Option<Arc<dyn CognitionReader>>,
}
fn session_error(error: SessionError) -> PanelError {
    match error {
        SessionError::InvalidInput => PanelError::InvalidInput,
        SessionError::OwnerMismatch => PanelError::NotFound,
        _ => PanelError::Unavailable,
    }
}
fn control_error(error: ControlError) -> PanelError {
    match error {
        ControlError::InvalidInput => PanelError::InvalidInput,
        ControlError::OwnerMismatch => PanelError::NotFound,
        ControlError::StaleGeneration => PanelError::Stale,
        _ => PanelError::Unavailable,
    }
}
fn summary(snapshot: ControlSnapshot) -> TaskSummary {
    TaskSummary {
        key: snapshot.key,
        phase: snapshot.phase,
        cancel_requested: snapshot.cancel_requested,
        events_retired: snapshot.events_retired,
        turn_id: snapshot.turn_id,
        commit: snapshot
            .report
            .as_ref()
            .map(|report| match report.run.commit {
                CommitState::NotStarted => "NotStarted",
                CommitState::Completed => "Completed",
                CommitState::Failed => "Failed",
                CommitState::Pending => "Pending",
                CommitState::Unknown => "Unknown",
            }),
        started_tools: snapshot.report.and_then(|report| report.run.started_tools),
    }
}
/// 按 UTF-8 字符边界截取至多 max 字节，并报告是否截断。
pub(crate) fn clip(value: &str, max: usize) -> (String, bool) {
    let mut end = value.len().min(max);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_owned(), end < value.len())
}
fn text(value: &str) -> (String, bool) {
    clip(value, 8192)
}
fn cursor(keys: &[SessionKey], limit: usize) -> Option<String> {
    (keys.len() == limit).then(|| keys.last().expect("nonempty page").session_id.clone())
}
impl PanelService for QqPanel {
    fn status(&self) -> PanelResult<PanelStatus> {
        let QqBotStatus {
            ready,
            closed,
            terminal_error,
            received,
            completed,
            sent,
            failed,
        } = *self.channel.status.borrow();
        Ok(PanelStatus {
            started_at_unix_ms: self.started_at_unix_ms,
            qq: ChannelStatus {
                ready,
                closed,
                terminal_error,
                received,
                completed,
                sent,
                failed,
            },
        })
    }
    fn sessions(&self, after: Option<&str>, limit: usize) -> PanelResult<Page<SessionSummary>> {
        let keys = self
            .sessions
            .list_keys(after, limit)
            .map_err(session_error)?;
        let next_cursor = cursor(&keys, limit);
        let items = keys
            .into_iter()
            .map(|key| {
                self.sessions
                    .snapshot(&key)
                    .map_err(session_error)?
                    .map(|snapshot| SessionSummary {
                        key: snapshot.key,
                        revision: snapshot.revision,
                        turn_count: snapshot.turns.len(),
                    })
                    .ok_or(PanelError::Unavailable)
            })
            .collect::<PanelResult<Vec<_>>>()?;
        Ok(Page { items, next_cursor })
    }
    fn tasks(&self, after: Option<&str>, limit: usize) -> PanelResult<Page<TaskSummary>> {
        let keys = self
            .control
            .list_keys(after, limit)
            .map_err(control_error)?;
        let next_cursor = cursor(&keys, limit);
        let items = keys
            .into_iter()
            .map(|key| {
                self.control
                    .snapshot(&key)
                    .map_err(control_error)?
                    .map(summary)
                    .ok_or(PanelError::Unavailable)
            })
            .collect::<PanelResult<Vec<_>>>()?;
        Ok(Page { items, next_cursor })
    }
    fn session(
        &self,
        key: &SessionKey,
        before: Option<u64>,
        limit: usize,
    ) -> PanelResult<SessionDetail> {
        if !(1..=50).contains(&limit) || before == Some(0) {
            return Err(PanelError::InvalidInput);
        }
        let snapshot = self
            .sessions
            .snapshot(key)
            .map_err(session_error)?
            .ok_or(PanelError::NotFound)?;
        // 持久历史仍由 SessionService 唯一管理；这里只投影有界的最近轮次。
        let mut turns = snapshot
            .turns
            .iter()
            .rev()
            .filter(|turn| before.is_none_or(|before| turn.id < before))
            .take(limit + 1)
            .collect::<Vec<_>>();
        let next_before = if turns.len() > limit {
            turns.truncate(limit);
            turns.last().map(|turn| turn.id)
        } else {
            None
        };
        turns.reverse();
        let turns = turns
            .into_iter()
            .map(|turn| {
                let (input, input_truncated) = text(&turn.input);
                let status = match &turn.status {
                    SessionTurnStatus::Pending => PanelTurnStatus::Pending,
                    SessionTurnStatus::Interrupted => PanelTurnStatus::Interrupted,
                    SessionTurnStatus::Failed { failure } => PanelTurnStatus::Failed {
                        code: match failure.code {
                            SessionFailureCode::Context => "Context",
                            SessionFailureCode::Provider => "Provider",
                            SessionFailureCode::ProviderTimeout => "ProviderTimeout",
                            SessionFailureCode::Protocol => "Protocol",
                            SessionFailureCode::Unsupported => "Unsupported",
                            SessionFailureCode::RoundLimit => "RoundLimit",
                            SessionFailureCode::Backend => "Backend",
                            SessionFailureCode::Cancelled => "Cancelled",
                        },
                        started_tools: failure.started_tools,
                    },
                    SessionTurnStatus::Completed { messages } => {
                        // 只显示最终助手正文，不暴露工具入参/结果、系统上下文或原始模型诊断。
                        let output = messages
                            .last()
                            .and_then(|message| message.text.as_deref())
                            .unwrap_or("");
                        let (text, truncated) = text(output);
                        PanelTurnStatus::Completed {
                            messages: vec![PanelMessage {
                                role: "assistant",
                                text,
                                truncated,
                            }],
                        }
                    }
                };
                PanelTurn {
                    id: turn.id,
                    input,
                    input_truncated,
                    status,
                }
            })
            .collect();
        Ok(SessionDetail {
            session: SessionHistory {
                key: snapshot.key,
                revision: snapshot.revision,
                turns,
                next_before,
            },
            control: self
                .control
                .snapshot(key)
                .map_err(control_error)?
                .map(summary),
        })
    }
    fn cancel(&self, target: &GenerationKey) -> PanelResult<CancelStatus> {
        // 精确代际交给已有控制器比较；不重新取最新代替换用户提交的目标。
        match self.control.cancel(target).map_err(control_error)? {
            CancelDisposition::Requested => Ok(CancelStatus::Requested),
            CancelDisposition::AlreadyRequested => Ok(CancelStatus::AlreadyRequested),
            CancelDisposition::AlreadyFinished => Ok(CancelStatus::AlreadyFinished),
        }
    }
    fn judgments(&self, before: Option<u64>, limit: usize) -> PanelResult<JudgmentLog> {
        let (recent, mode) = self.judgments.as_ref().ok_or(PanelError::Unavailable)?;
        // 记录器只在锁中毒时不可读；此时明确报告不可用，不返回空列表冒充“没有判断”。
        let page = recent.page(before, limit).ok_or(PanelError::Unavailable)?;
        Ok(JudgmentLog {
            mode,
            capacity: page.capacity,
            recorded_total: page.recorded_total,
            evicted: page.evicted,
            items: page.records.into_iter().map(judgment).collect(),
            next_before: page.next_before,
        })
    }
    fn goals(&self, after: Option<&str>, limit: usize) -> PanelResult<GoalPage> {
        let reader = self.cognition.as_ref().ok_or(PanelError::Unavailable)?;
        crate::web_panel_cognition::goals(reader.as_ref(), after, limit)
    }
    fn goal(&self, id: &str) -> PanelResult<GoalDetail> {
        let reader = self.cognition.as_ref().ok_or(PanelError::Unavailable)?;
        crate::web_panel_cognition::goal(reader.as_ref(), self.sessions.as_ref(), id)
    }
}

fn outcome(value: RelationOutcome) -> &'static str {
    match value {
        RelationOutcome::Completed => "completed",
        RelationOutcome::Unavailable => "unavailable",
        RelationOutcome::Protocol => "protocol",
        RelationOutcome::Timeout => "timeout",
        RelationOutcome::Panicked => "panicked",
        RelationOutcome::Dropped => "dropped",
    }
}
fn intent(value: MessageIntent) -> &'static str {
    match value {
        MessageIntent::Supplement => "supplement",
        MessageIntent::Correction => "correction",
        MessageIntent::Answer => "answer",
        MessageIntent::NewTask => "new_task",
        MessageIntent::Cancel => "cancel",
        MessageIntent::Continue => "continue",
        MessageIntent::Unrelated => "unrelated",
        MessageIntent::Ambiguous => "ambiguous",
        MessageIntent::Pause => "pause",
        MessageIntent::Resume => "resume",
    }
}
fn operation(value: RelationOperation) -> (&'static str, &'static str) {
    match value {
        RelationOperation::Stage(RelationStage::Rules) => ("stage", "rules"),
        RelationOperation::Stage(RelationStage::Auxiliary) => ("stage", "auxiliary"),
        RelationOperation::Stage(RelationStage::Primary) => ("stage", "primary"),
        RelationOperation::Attempt(RelationAttempt::ClassifierCall) => {
            ("attempt", "classifier_call")
        }
        RelationOperation::Attempt(RelationAttempt::ModelProviderCall) => {
            ("attempt", "model_provider_call")
        }
    }
}
fn fallback(value: FallbackReason) -> &'static str {
    match value {
        FallbackReason::Unavailable => "unavailable",
        FallbackReason::Protocol => "protocol",
        FallbackReason::Timeout => "timeout",
        FallbackReason::Panicked => "panicked",
        FallbackReason::InvalidDecision => "invalid_decision",
        FallbackReason::Ambiguous => "ambiguous",
        FallbackReason::LowConfidence => "low_confidence",
    }
}
/// 按开始顺序列出阶段与调用尝试；终态与最早未结束的同类开始配对，缺少终态时保持未知。
fn judgment(record: RelationJudgmentRecord) -> JudgmentView {
    let mut steps: Vec<(RelationOperation, JudgmentStep)> = Vec::new();
    let mut fallbacks = Vec::new();
    for event in &record.diagnostics.events {
        match *event {
            RelationObservation::Started { operation: op } => {
                let (kind, name) = operation(op);
                steps.push((
                    op,
                    JudgmentStep {
                        kind,
                        name,
                        outcome: None,
                        elapsed_micros: None,
                    },
                ));
            }
            RelationObservation::Finished {
                operation: op,
                outcome: finished,
                elapsed_micros,
            } => {
                if let Some((_, step)) = steps
                    .iter_mut()
                    .find(|(started, step)| *started == op && step.outcome.is_none())
                {
                    step.outcome = Some(outcome(finished));
                    step.elapsed_micros = Some(elapsed_micros);
                }
            }
            RelationObservation::Fallback { reason } => fallbacks.push(fallback(reason)),
            RelationObservation::Supported | RelationObservation::Unsupported => {}
        }
    }
    let (result, failure, intents) = match record.result {
        JudgmentResult::Decided { intents } => {
            ("decided", None, intents.into_iter().map(intent).collect())
        }
        JudgmentResult::Failed { outcome: failed } => ("failed", Some(outcome(failed)), vec![]),
        JudgmentResult::Dropped => ("dropped", None, vec![]),
    };
    JudgmentView {
        sequence: record.sequence,
        finished_at_unix_ms: record.finished_at_unix_ms,
        elapsed_micros: record.elapsed_micros,
        result,
        failure,
        intents,
        coverage: match record.diagnostics.coverage {
            DiagnosticCoverage::Complete => "complete",
            DiagnosticCoverage::Unsupported => "unsupported",
            DiagnosticCoverage::Overflow => "overflow",
            DiagnosticCoverage::Invalid => "invalid",
            DiagnosticCoverage::Unreported => "unreported",
        },
        counts: record.diagnostics.counts.map(|counts| JudgmentCounts {
            rules: counts.rules_started,
            auxiliary: counts.auxiliary_started,
            primary: counts.primary_started,
            classifier_calls: counts.classifier_calls,
            model_provider_calls: counts.model_provider_calls,
            fallbacks: counts.fallbacks,
        }),
        steps: steps.into_iter().map(|(_, step)| step).collect(),
        fallbacks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_message_diagnostics::{RelationDiagnosticCounts, RelationDiagnostics};

    fn stage(stage: RelationStage) -> RelationOperation {
        RelationOperation::Stage(stage)
    }
    fn record(result: JudgmentResult, diagnostics: RelationDiagnostics) -> RelationJudgmentRecord {
        RelationJudgmentRecord {
            sequence: 9,
            finished_at_unix_ms: 1,
            elapsed_micros: 2,
            result,
            diagnostics,
        }
    }

    #[test]
    fn steps_pair_in_start_order_and_unfinished_steps_stay_unknown() {
        let call = RelationOperation::Attempt(RelationAttempt::ModelProviderCall);
        let view = judgment(record(
            JudgmentResult::Decided {
                intents: vec![MessageIntent::NewTask, MessageIntent::Cancel],
            },
            RelationDiagnostics {
                coverage: DiagnosticCoverage::Complete,
                counts: Some(RelationDiagnosticCounts {
                    rules_started: 1,
                    auxiliary_started: 1,
                    primary_started: 1,
                    classifier_calls: 0,
                    model_provider_calls: 1,
                    fallbacks: 1,
                }),
                events: vec![
                    RelationObservation::Supported,
                    RelationObservation::Started {
                        operation: stage(RelationStage::Rules),
                    },
                    RelationObservation::Finished {
                        operation: stage(RelationStage::Rules),
                        outcome: RelationOutcome::Completed,
                        elapsed_micros: 5,
                    },
                    RelationObservation::Started {
                        operation: stage(RelationStage::Auxiliary),
                    },
                    RelationObservation::Started { operation: call },
                    RelationObservation::Finished {
                        operation: call,
                        outcome: RelationOutcome::Timeout,
                        elapsed_micros: 70,
                    },
                    RelationObservation::Finished {
                        operation: stage(RelationStage::Auxiliary),
                        outcome: RelationOutcome::Timeout,
                        elapsed_micros: 80,
                    },
                    RelationObservation::Fallback {
                        reason: FallbackReason::Timeout,
                    },
                    RelationObservation::Started {
                        operation: stage(RelationStage::Primary),
                    },
                ],
            },
        ));
        assert_eq!(view.result, "decided");
        assert_eq!(view.failure, None);
        assert_eq!(view.intents, ["new_task", "cancel"]);
        assert_eq!(view.coverage, "complete");
        assert_eq!(view.counts.unwrap().model_provider_calls, 1);
        assert_eq!(view.fallbacks, ["timeout"]);
        let steps: Vec<_> = view
            .steps
            .iter()
            .map(|step| (step.kind, step.name, step.outcome, step.elapsed_micros))
            .collect();
        assert_eq!(
            steps,
            [
                ("stage", "rules", Some("completed"), Some(5)),
                ("stage", "auxiliary", Some("timeout"), Some(80)),
                ("attempt", "model_provider_call", Some("timeout"), Some(70)),
                ("stage", "primary", None, None),
            ]
        );
    }

    #[test]
    fn failures_drops_and_incomplete_coverage_never_show_success_or_zero_counts() {
        for (result, coverage, name, failure) in [
            (
                JudgmentResult::Failed {
                    outcome: RelationOutcome::Panicked,
                },
                DiagnosticCoverage::Unsupported,
                "unsupported",
                Some("panicked"),
            ),
            (
                JudgmentResult::Dropped,
                DiagnosticCoverage::Overflow,
                "overflow",
                None,
            ),
            (
                JudgmentResult::Dropped,
                DiagnosticCoverage::Invalid,
                "invalid",
                None,
            ),
            (
                JudgmentResult::Dropped,
                DiagnosticCoverage::Unreported,
                "unreported",
                None,
            ),
        ] {
            let dropped = result == JudgmentResult::Dropped;
            let view = judgment(record(
                result,
                RelationDiagnostics {
                    coverage,
                    counts: None,
                    events: vec![RelationObservation::Unsupported],
                },
            ));
            assert_eq!(view.result, if dropped { "dropped" } else { "failed" });
            assert_eq!(view.failure, failure);
            assert!(view.intents.is_empty());
            assert_eq!(view.coverage, name);
            assert!(view.counts.is_none());
            assert!(view.steps.is_empty());
        }
    }
}
