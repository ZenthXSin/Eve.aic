//! QQ 宿主到公开面板契约的组合；不从 StateStore 旁路读取或修改数据。
use eve_control_api::{
    CancelDisposition, CommitState, ControlError, ControlService, ControlSnapshot, GenerationKey,
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
fn text(value: &str) -> (String, bool) {
    let mut end = value.len().min(8192);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_owned(), end < value.len())
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
}
