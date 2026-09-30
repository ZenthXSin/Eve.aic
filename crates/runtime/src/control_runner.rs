//! 组合层：把内置会话执行连接到独立控制契约。
use crate::{SessionLlmHost, SessionRunError, TurnDiagnostics};
use eve_control_api::{CommitState, ControlRunner, RunFailure, RunFuture, RunReport};
use eve_llm_api::{LlmFuture, TurnEvent, TurnEventSink};
use eve_session_api::SessionInput;
use std::sync::{Arc, Mutex};

pub struct SessionControlRunner {
    host: Arc<SessionLlmHost>,
}
impl SessionControlRunner {
    pub fn new(host: Arc<SessionLlmHost>) -> Self {
        Self { host }
    }
}
struct ObservedSink<'a> {
    sink: &'a dyn TurnEventSink,
    turn_id: Mutex<Option<u64>>,
}
impl TurnEventSink for ObservedSink<'_> {
    fn emit(&self, event: TurnEvent) -> LlmFuture<'_, ()> {
        if let Ok(mut id) = self.turn_id.lock() {
            *id = event.turn_id.or(*id);
        }
        self.sink.emit(event)
    }
    fn closed(&self) -> LlmFuture<'_, ()> {
        self.sink.closed()
    }
}
fn diagnostics(report: &mut RunReport, value: TurnDiagnostics) {
    report.started_tools = Some(value.started_tools as u64);
    report.tool_results = value.tool_results;
}
impl ControlRunner for SessionControlRunner {
    fn run<'a>(&'a self, input: SessionInput, sink: &'a dyn TurnEventSink) -> RunFuture<'a> {
        Box::pin(async move {
            let observed = ObservedSink {
                sink,
                turn_id: Mutex::new(None),
            };
            let result = self.host.run_turn_with_events(input, &observed).await;
            let mut report = RunReport {
                turn_id: observed.turn_id.into_inner().unwrap_or(None),
                commit: CommitState::NotStarted,
                text: None,
                transcript: None,
                started_tools: Some(0),
                tool_results: vec![],
                failure: None,
            };
            match result {
                Ok(output) => {
                    report.turn_id = Some(output.turn_id);
                    report.commit = CommitState::Completed;
                    report.text = Some(output.output.text);
                    report.transcript = Some(output.output.transcript);
                    diagnostics(&mut report, output.output.diagnostics);
                }
                Err(SessionRunError::NotStarted(error)) => {
                    report.failure = Some(RunFailure::Execution(error));
                }
                Err(SessionRunError::Session(error)) => {
                    report.failure = Some(RunFailure::Session(error));
                }
                Err(SessionRunError::Turn(failure)) => {
                    report.commit = CommitState::Failed;
                    report.failure = Some(RunFailure::Execution(failure.error));
                    diagnostics(&mut report, failure.diagnostics);
                }
                Err(SessionRunError::Commit { error, output }) => {
                    report.commit = CommitState::Pending;
                    report.text = Some(output.text);
                    report.transcript = Some(output.transcript);
                    report.failure = Some(RunFailure::Commit(error));
                    diagnostics(&mut report, output.diagnostics);
                }
                Err(SessionRunError::Delivery { error, output }) => {
                    report.turn_id = Some(output.turn_id);
                    report.commit = CommitState::Completed;
                    report.text = Some(output.output.text);
                    report.transcript = Some(output.output.transcript);
                    report.failure = Some(RunFailure::Delivery(error));
                    diagnostics(&mut report, output.output.diagnostics);
                }
                Err(SessionRunError::FailureRecord { error, failure }) => {
                    report.commit = CommitState::Pending;
                    report.failure = Some(RunFailure::FailureRecord {
                        storage: error,
                        execution: failure.error,
                    });
                    diagnostics(&mut report, failure.diagnostics);
                }
            }
            report
        })
    }
}
