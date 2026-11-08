//! 子轮只投影工具名称和真实回执状态；对话和输出仍留在子轮。
use eve_llm_api::{
    DelegatedProgressObserver, DelegatedToolProgress, LlmFuture, ToolFailureCode, ToolOutput,
    TurnEvent, TurnEventKind, TurnEventSink,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

#[derive(Default)]
struct Progress {
    snapshot: DelegatedToolProgress,
    pending: BTreeMap<String, String>,
}

pub(super) struct StopSignal {
    stop: watch::Receiver<bool>,
    observer: Option<Arc<dyn DelegatedProgressObserver>>,
    tools: BTreeSet<String>,
    progress: Mutex<Progress>,
}
impl StopSignal {
    pub(super) fn new(
        stop: watch::Receiver<bool>,
        observer: Option<Arc<dyn DelegatedProgressObserver>>,
        tools: &[String],
    ) -> Self {
        Self {
            stop,
            observer,
            tools: tools.iter().cloned().collect(),
            progress: Mutex::default(),
        }
    }
}
impl TurnEventSink for StopSignal {
    fn emit(&self, event: TurnEvent) -> LlmFuture<'_, ()> {
        Box::pin(async move {
            let Some(observer) = &self.observer else {
                return Ok(());
            };
            if *self.stop.borrow() {
                return Ok(());
            }
            let snapshot = {
                let Ok(mut state) = self.progress.lock() else {
                    return Ok(());
                };
                match event.kind {
                    TurnEventKind::ToolBatchStarted { calls } => {
                        for call in calls {
                            // 未知工具名也不能被用来把任意模型正文带进观察事实。
                            if !self.tools.contains(&call.name) {
                                continue;
                            }
                            if state.pending.insert(call.id, call.name).is_none() {
                                state.snapshot.submitted_calls += 1;
                            }
                        }
                    }
                    TurnEventKind::ToolResult { result, .. } => {
                        let Some(name) = state.pending.remove(&result.call_id) else {
                            return Ok(());
                        };
                        state.snapshot.returned_calls += 1;
                        state.snapshot.last_returned_tool_name = Some(name);
                        if let ToolOutput::Failure { code, .. } = result.output {
                            state.snapshot.failed_calls += 1;
                            if matches!(
                                code,
                                ToolFailureCode::TimedOut
                                    | ToolFailureCode::Cancelled
                                    | ToolFailureCode::ExecutionFailed
                            ) {
                                state.snapshot.unknown_calls += 1;
                            }
                        }
                    }
                    _ => return Ok(()),
                }
                state.snapshot.pending_tool_names =
                    state.pending.values().take(16).cloned().collect();
                state.snapshot.clone()
            };
            // 回调异常与接收端离开均不能改变子执行的业务结果。
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                observer.observe(snapshot)
            }));
            Ok(())
        })
    }
    fn closed(&self) -> LlmFuture<'_, ()> {
        let mut receiver = self.stop.clone();
        Box::pin(async move {
            let _ = receiver.wait_for(|stop| *stop).await;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_llm_api::{ToolCall, ToolResult};
    use serde_json::json;

    #[derive(Default)]
    struct Seen(Mutex<Vec<DelegatedToolProgress>>);
    impl DelegatedProgressObserver for Seen {
        fn observe(&self, progress: DelegatedToolProgress) {
            self.0.lock().unwrap().push(progress);
        }
    }
    async fn send(sink: &StopSignal, kind: TurnEventKind) {
        sink.emit(TurnEvent {
            turn_id: None,
            kind,
        })
        .await
        .unwrap();
    }
    fn batch(names: &[(&str, &str)]) -> TurnEventKind {
        TurnEventKind::ToolBatchStarted {
            calls: names
                .iter()
                .map(|(id, name)| ToolCall {
                    id: (*id).into(),
                    name: (*name).into(),
                    arguments: json!({"command":"private-source-and-token"}),
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn delegated_progress_reports_receipts_without_text_or_business_success() {
        let (stop, signal) = watch::channel(false);
        let seen = Arc::new(Seen::default());
        let sink = StopSignal::new(
            signal,
            Some(seen.clone()),
            &["read_file".into(), "run_command".into()],
        );
        send(
            &sink,
            batch(&[
                ("read", "read_file"),
                ("run", "run_command"),
                ("bad", "private_model_text"),
            ]),
        )
        .await;
        send(
            &sink,
            TurnEventKind::TextDelta {
                request: 1,
                text: "private-thought".into(),
            },
        )
        .await;
        send(
            &sink,
            TurnEventKind::ToolResult {
                ordinal: 0,
                result: ToolResult::success("read", json!({"source":"private-source"})).unwrap(),
            },
        )
        .await;
        send(
            &sink,
            TurnEventKind::ToolResult {
                ordinal: 0,
                result: ToolResult::success("read", json!("duplicate")).unwrap(),
            },
        )
        .await;
        send(
            &sink,
            TurnEventKind::ToolResult {
                ordinal: 1,
                result: ToolResult::failure("run", ToolFailureCode::TimedOut, "private-error")
                    .unwrap(),
            },
        )
        .await;
        send(
            &sink,
            TurnEventKind::TurnCompleted {
                text: "private-final".into(),
            },
        )
        .await;
        let values = seen.0.lock().unwrap().clone();
        assert_eq!(values.len(), 3);
        assert_eq!(values[0].submitted_calls, 2);
        assert_eq!(values[1].pending_tool_names, ["run_command"]);
        assert_eq!(values[2].returned_calls, 2);
        assert_eq!(values[2].failed_calls, 1);
        assert_eq!(values[2].unknown_calls, 1);
        assert!(!serde_json::to_string(&values).unwrap().contains("private"));
        stop.send_replace(true);
        send(&sink, batch(&[("later", "run_command")])).await;
        assert_eq!(seen.0.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn failed_progress_observer_does_not_cancel_business() {
        struct Broken;
        impl DelegatedProgressObserver for Broken {
            fn observe(&self, _: DelegatedToolProgress) {
                panic!("observer unavailable")
            }
        }
        let (_stop, signal) = watch::channel(false);
        let sink = StopSignal::new(signal, Some(Arc::new(Broken)), &["read_file".into()]);
        send(&sink, batch(&[("read", "read_file")])).await;
        send(
            &sink,
            TurnEventKind::ToolResult {
                ordinal: 0,
                result: ToolResult::success("read", json!("actual receipt")).unwrap(),
            },
        )
        .await;
    }
}
