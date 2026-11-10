//! 子轮投影工具运行计数及自愿登记的最新阶段；阶段自述不是业务成功证据。
//! 仅 task_progress 的匹配成功回执可携带正文，其余对话和输出仍留在子轮。
use eve_llm_api::{
    DelegatedProgressObserver, DelegatedTaskProgressReport, DelegatedTaskProgressStatus,
    DelegatedToolProgress, LlmFuture, Tool, ToolFailureCode, ToolOutput, TurnEvent, TurnEventKind,
    TurnEventSink,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

const MAX_TRACKED_CALLS: usize = eve_task_api::MAX_STEPS as usize;

struct PendingCall {
    name: String,
    // 只保留参数摘要，不把多个阶段正文或普通工具参数积存在观察器里。
    progress_arguments: Option<ring::digest::Digest>,
}

#[derive(Default)]
struct Progress {
    snapshot: DelegatedToolProgress,
    pending: BTreeMap<String, PendingCall>,
    seen: BTreeSet<String>,
}

fn progress_arguments(arguments: &Value) -> Option<ring::digest::Digest> {
    eve_dialogue_tools::TaskProgressTool
        .validate_arguments(arguments)
        .ok()?;
    // 按固定次序摘要两个字段，避免 JSON 对象键序影响匹配。
    let bytes = serde_json::to_vec(&(&arguments["status"], &arguments["remaining"])).ok()?;
    Some(ring::digest::digest(&ring::digest::SHA256, &bytes))
}

fn recorded_progress(
    call_id: &str,
    pending: &PendingCall,
    output: &ToolOutput,
) -> Option<DelegatedTaskProgressReport> {
    if pending.name != "task_progress" {
        return None;
    }
    let expected = pending.progress_arguments.as_ref()?;
    let ToolOutput::Success(value) = output else {
        return None;
    };
    if value.as_object()?.len() != 3 || value.get("verified_by_host")?.as_bool()? {
        return None;
    }
    let status = value.get("status")?.as_str()?;
    let remaining = value.get("remaining")?.as_str()?;
    if status.len() > 16 || remaining.len() > 2048 {
        return None;
    }
    let arguments = json!({"status":status,"remaining":remaining});
    if progress_arguments(&arguments)?.as_ref() != expected.as_ref() {
        return None;
    }
    let status = match arguments["status"].as_str()? {
        "in_progress" => DelegatedTaskProgressStatus::InProgress,
        "completed" => DelegatedTaskProgressStatus::Completed,
        "blocked" => DelegatedTaskProgressStatus::Blocked,
        _ => return None,
    };
    Some(DelegatedTaskProgressReport {
        call_id: call_id.into(),
        status,
        remaining: arguments["remaining"].as_str()?.into(),
    })
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
                            if !self.tools.contains(&call.name) || call.id.trim().is_empty() {
                                continue;
                            }
                            if state.seen.contains(&call.id) {
                                // 重复登记不能覆盖先前关联。若重复编号的参数不一致，
                                // 后续回执来源有歧义，保留计数但不转发该编号的自述。
                                if let Some(original) = state.pending.get_mut(&call.id) {
                                    let incoming = (call.name == "task_progress")
                                        .then(|| progress_arguments(&call.arguments))
                                        .flatten();
                                    if original.name != call.name
                                        || original.progress_arguments.as_ref().map(|d| d.as_ref())
                                            != incoming.as_ref().map(|d| d.as_ref())
                                    {
                                        original.progress_arguments = None;
                                    }
                                }
                                continue;
                            }
                            if state.seen.len() >= MAX_TRACKED_CALLS {
                                continue;
                            }
                            let progress_arguments = (call.name == "task_progress")
                                .then(|| progress_arguments(&call.arguments))
                                .flatten();
                            state.seen.insert(call.id.clone());
                            state.pending.insert(
                                call.id,
                                PendingCall {
                                    name: call.name,
                                    progress_arguments,
                                },
                            );
                            state.snapshot.submitted_calls += 1;
                        }
                    }
                    TurnEventKind::ToolResult { result, .. } => {
                        let Some(pending) = state.pending.remove(&result.call_id) else {
                            return Ok(());
                        };
                        if let Some(report) =
                            recorded_progress(&result.call_id, &pending, &result.output)
                        {
                            state.snapshot.task_progress_report = Some(report);
                        }
                        state.snapshot.returned_calls += 1;
                        state.snapshot.last_returned_tool_name = Some(pending.name);
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
                state.snapshot.pending_tool_names = state
                    .pending
                    .values()
                    .take(16)
                    .map(|call| call.name.clone())
                    .collect();
                state.snapshot.clone()
            };
            if *self.stop.borrow() {
                return Ok(());
            }
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
    use eve_llm_api::{ToolCall, ToolExecutionContext, ToolResult};
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

    fn progress_call(id: &str, status: &str, remaining: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "task_progress".into(),
            arguments: json!({"status":status,"remaining":remaining}),
        }
    }

    async fn register(sink: &StopSignal, call: ToolCall) {
        send(sink, TurnEventKind::ToolBatchStarted { calls: vec![call] }).await;
    }

    async fn receipt(sink: &StopSignal, result: ToolResult) {
        send(sink, TurnEventKind::ToolResult { ordinal: 0, result }).await;
    }

    async fn actual_result(call: &ToolCall, context: ToolExecutionContext) -> ToolResult {
        let value = eve_dialogue_tools::TaskProgressTool
            .execute(call.clone(), context)
            .await
            .unwrap();
        ToolResult::success(&call.id, value).unwrap()
    }

    fn latest(seen: &Seen) -> DelegatedToolProgress {
        seen.0.lock().unwrap().last().unwrap().clone()
    }

    fn progress_sink() -> (watch::Sender<bool>, Arc<Seen>, StopSignal) {
        let (stop, signal) = watch::channel(false);
        let seen = Arc::new(Seen::default());
        let sink = StopSignal::new(
            signal,
            Some(seen.clone()),
            &["read_file".into(), "task_progress".into()],
        );
        (stop, seen, sink)
    }

    #[tokio::test]
    async fn actual_task_progress_keeps_only_latest_report_without_claiming_verification() {
        let (_stop, seen, sink) = progress_sink();
        let context = ToolExecutionContext::new();
        for (id, status, remaining, expected) in [
            (
                "inspect",
                "in_progress",
                "已读到配置差异，还需核对调用端。",
                DelegatedTaskProgressStatus::InProgress,
            ),
            (
                "done",
                "completed",
                "这一步核对完毕。",
                DelegatedTaskProgressStatus::Completed,
            ),
            (
                "blocked",
                "blocked",
                "等待原任务需要的版本信息。",
                DelegatedTaskProgressStatus::Blocked,
            ),
        ] {
            let call = progress_call(id, status, remaining);
            register(&sink, call.clone()).await;
            let result = actual_result(&call, context.clone()).await;
            let registered = context.continuation().remaining();
            assert_eq!(
                registered.as_deref(),
                (status == "in_progress").then_some(remaining)
            );
            assert!(
                matches!(&result.output, ToolOutput::Success(value) if value["verified_by_host"] == false)
            );
            receipt(&sink, result).await;
            assert_eq!(
                context.continuation().remaining(),
                registered,
                "观察不能修改子轮续行状态"
            );
            assert_eq!(
                latest(&seen).task_progress_report,
                Some(DelegatedTaskProgressReport {
                    call_id: id.into(),
                    status: expected,
                    remaining: remaining.into(),
                })
            );
        }
        let snapshot = latest(&seen);
        assert_eq!(snapshot.submitted_calls, 3);
        assert_eq!(snapshot.returned_calls, 3);
        assert_eq!(snapshot.failed_calls, 0);
        assert!(snapshot.pending_tool_names.is_empty());
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert!(!encoded.contains("配置差异"));
        assert!(!encoded.contains("核对完毕"));
    }

    #[tokio::test]
    async fn invalid_arguments_or_nonmatching_receipts_never_become_reports() {
        let (_stop, seen, sink) = progress_sink();
        let valid = json!({"status":"in_progress","remaining":"已读到配置差异，还需核对。"});
        let matching = json!({"status":"in_progress","remaining":"已读到配置差异，还需核对。","verified_by_host":false});
        let mut invalid_arguments = vec![
            json!({"status":"unknown","remaining":"不接受"}),
            json!({"status":"in_progress","remaining":"   "}),
            json!({"status":"in_progress","remaining":"x".repeat(2049)}),
            json!({"status":"in_progress","remaining":"不接受","extra":true}),
        ];
        invalid_arguments.push(json!({"status":"in_progress","remaining":12}));
        let mut cases: Vec<_> = invalid_arguments
            .into_iter()
            .map(|arguments| (arguments, matching.clone()))
            .collect();
        for output in [
            json!({"status":"completed","remaining":valid["remaining"],"verified_by_host":false}),
            json!({"status":"in_progress","remaining":"改写的发现","verified_by_host":false}),
            json!({"status":"in_progress","remaining":valid["remaining"],"verified_by_host":true}),
            json!({"status":"in_progress","remaining":valid["remaining"],"verified_by_host":null}),
            json!({"status":"in_progress","remaining":valid["remaining"]}),
            json!({"status":"in_progress","remaining":valid["remaining"],"verified_by_host":false,"private_output":"不得携带"}),
            json!("不是结构化回执"),
        ] {
            cases.push((valid.clone(), output));
        }
        for (index, (arguments, output)) in cases.into_iter().enumerate() {
            let call = ToolCall {
                id: format!("bad-{index}"),
                name: "task_progress".into(),
                arguments,
            };
            register(&sink, call.clone()).await;
            receipt(&sink, ToolResult::success(&call.id, output).unwrap()).await;
            assert!(latest(&seen).task_progress_report.is_none());
        }
        let boundary = progress_call("boundary", "in_progress", &"界".repeat(682));
        register(&sink, boundary.clone()).await;
        receipt(
            &sink,
            actual_result(&boundary, ToolExecutionContext::new()).await,
        )
        .await;
        assert_eq!(
            latest(&seen).task_progress_report.unwrap().remaining.len(),
            2046
        );
    }

    #[tokio::test]
    async fn unknown_failed_other_tool_and_model_text_do_not_replace_a_report() {
        let (_stop, seen, sink) = progress_sink();
        let valid = progress_call("real", "in_progress", "已确认入口位置，继续核对。 ");
        register(&sink, valid.clone()).await;
        let result = actual_result(&valid, ToolExecutionContext::new()).await;
        receipt(&sink, result.clone()).await;
        let recorded = latest(&seen).task_progress_report;
        let mut unknown = result.clone();
        unknown.call_id = "not-registered".into();
        receipt(&sink, unknown).await;
        let mut other = valid.clone();
        other.id = "read".into();
        other.name = "read_file".into();
        register(&sink, other.clone()).await;
        let mut other_output = result;
        other_output.call_id = other.id;
        receipt(&sink, other_output).await;
        for (index, code) in [
            ToolFailureCode::TimedOut,
            ToolFailureCode::Cancelled,
            ToolFailureCode::ExecutionFailed,
        ]
        .into_iter()
        .enumerate()
        {
            let call = progress_call(&format!("failed-{index}"), "completed", "未确认的结果");
            register(&sink, call.clone()).await;
            receipt(
                &sink,
                ToolResult::failure(call.id, code, "不得当成阶段自述").unwrap(),
            )
            .await;
        }
        send(
            &sink,
            TurnEventKind::TextDelta {
                request: 1,
                text: "模型正文不得转发".into(),
            },
        )
        .await;
        send(
            &sink,
            TurnEventKind::TurnCompleted {
                text: "模型最终正文不得转发".into(),
            },
        )
        .await;
        let snapshot = latest(&seen);
        assert_eq!(snapshot.task_progress_report, recorded);
        assert_eq!(snapshot.failed_calls, 3);
        assert_eq!(snapshot.unknown_calls, 3);
        assert!(!serde_json::to_string(&snapshot).unwrap().contains("不得"));
    }

    #[tokio::test]
    async fn duplicate_ids_cannot_rebind_or_replay_but_distinct_pending_calls_match_independently()
    {
        let (_stop, seen, sink) = progress_sink();
        let first = progress_call("same", "in_progress", "原阶段");
        register(&sink, first.clone()).await;
        register(&sink, first.clone()).await;
        receipt(
            &sink,
            actual_result(&first, ToolExecutionContext::new()).await,
        )
        .await;
        assert_eq!(latest(&seen).returned_calls, 1);
        let baseline = latest(&seen).task_progress_report;
        let replay = progress_call("same", "completed", "重放不能替换");
        register(&sink, replay.clone()).await;
        receipt(
            &sink,
            actual_result(&replay, ToolExecutionContext::new()).await,
        )
        .await;
        assert_eq!(latest(&seen).task_progress_report, baseline);
        assert_eq!(latest(&seen).submitted_calls, 1);

        let original = progress_call("conflict", "in_progress", "真正待确认");
        register(&sink, original.clone()).await;
        register(
            &sink,
            progress_call("conflict", "blocked", "重复编号造成歧义"),
        )
        .await;
        receipt(
            &sink,
            actual_result(&original, ToolExecutionContext::new()).await,
        )
        .await;
        assert_eq!(latest(&seen).task_progress_report, baseline);

        let left = progress_call("left", "in_progress", "左侧发现");
        let right = progress_call("right", "blocked", "右侧发现");
        send(
            &sink,
            TurnEventKind::ToolBatchStarted {
                calls: vec![left.clone(), right.clone()],
            },
        )
        .await;
        receipt(
            &sink,
            actual_result(&right, ToolExecutionContext::new()).await,
        )
        .await;
        assert_eq!(latest(&seen).task_progress_report.unwrap().call_id, "right");
        receipt(
            &sink,
            actual_result(&left, ToolExecutionContext::new()).await,
        )
        .await;
        let snapshot = latest(&seen);
        assert_eq!(snapshot.task_progress_report.unwrap().remaining, "左侧发现");
        assert_eq!(snapshot.submitted_calls, 4);
        assert_eq!(snapshot.returned_calls, 4);
    }

    #[tokio::test]
    async fn separate_child_runs_keep_identical_call_ids_and_cancellation_isolated() {
        let (left_stop, left_seen, left) = progress_sink();
        let (_right_stop, right_seen, right) = progress_sink();
        let left_call = progress_call("same-call-id", "in_progress", "左任务的发现");
        let right_call = progress_call("same-call-id", "blocked", "右任务的发现");
        tokio::join!(
            register(&left, left_call.clone()),
            register(&right, right_call.clone())
        );
        let left_result = actual_result(&left_call, ToolExecutionContext::new()).await;
        let right_result = actual_result(&right_call, ToolExecutionContext::new()).await;
        tokio::join!(receipt(&left, left_result), receipt(&right, right_result));
        assert_eq!(
            latest(&left_seen).task_progress_report.unwrap().remaining,
            "左任务的发现"
        );
        assert_eq!(
            latest(&right_seen).task_progress_report.unwrap().remaining,
            "右任务的发现"
        );
        let cancelled = progress_call("cancelled", "completed", "取消后不传递");
        register(&left, cancelled.clone()).await;
        let before_stop = left_seen.0.lock().unwrap().len();
        left_stop.send_replace(true);
        receipt(
            &left,
            actual_result(&cancelled, ToolExecutionContext::new()).await,
        )
        .await;
        register(&left, progress_call("later", "completed", "取消后不登记")).await;
        assert_eq!(left_seen.0.lock().unwrap().len(), before_stop);
        let later = progress_call("later", "completed", "右任务仍正常记录");
        register(&right, later.clone()).await;
        receipt(
            &right,
            actual_result(&later, ToolExecutionContext::new()).await,
        )
        .await;
        assert_eq!(
            latest(&right_seen).task_progress_report.unwrap().remaining,
            "右任务仍正常记录"
        );
    }

    #[tokio::test]
    async fn empty_call_ids_and_capacity_bound_observation_count() {
        let (_stop, seen, sink) = progress_sink();
        register(&sink, progress_call(" ", "in_progress", "不应登记")).await;
        send(
            &sink,
            TurnEventKind::ToolBatchStarted {
                calls: (0..=MAX_TRACKED_CALLS)
                    .map(|index| {
                        progress_call(&format!("call-{index}"), "in_progress", "阶段正文不累积")
                    })
                    .collect(),
            },
        )
        .await;
        let state = sink.progress.lock().unwrap();
        assert_eq!(state.seen.len(), MAX_TRACKED_CALLS);
        assert_eq!(state.pending.len(), MAX_TRACKED_CALLS);
        assert!(state.snapshot.task_progress_report.is_none());
        drop(state);
        assert_eq!(latest(&seen).pending_tool_names.len(), 16);
        assert_eq!(latest(&seen).submitted_calls, MAX_TRACKED_CALLS);
    }

    #[tokio::test]
    async fn absent_observer_does_not_change_real_task_progress_execution() {
        let (_stop, signal) = watch::channel(false);
        let sink = StopSignal::new(signal, None, &["task_progress".into()]);
        let call = progress_call("without-parent", "in_progress", "继续原有工作");
        let context = ToolExecutionContext::new();
        register(&sink, call.clone()).await;
        receipt(&sink, actual_result(&call, context.clone()).await).await;
        assert_eq!(
            context.continuation().remaining().as_deref(),
            Some("继续原有工作")
        );
        assert!(sink.progress.lock().unwrap().seen.is_empty());
    }

    #[tokio::test]
    async fn unavailable_tool_and_conflicting_tool_names_cannot_supply_reports() {
        let (_stop, signal) = watch::channel(false);
        let seen = Arc::new(Seen::default());
        let unavailable = StopSignal::new(signal, Some(seen.clone()), &["read_file".into()]);
        let call = progress_call("unavailable", "in_progress", "不可用工具不得转发");
        register(&unavailable, call.clone()).await;
        receipt(
            &unavailable,
            actual_result(&call, ToolExecutionContext::new()).await,
        )
        .await;
        assert!(latest(&seen).task_progress_report.is_none());
        assert_eq!(latest(&seen).submitted_calls, 0);

        let (_stop, seen, sink) = progress_sink();
        for progress_first in [true, false] {
            let id = if progress_first {
                "progress-first"
            } else {
                "read-first"
            };
            let progress = progress_call(id, "completed", "名字改绑不得转发");
            let mut read = progress.clone();
            read.name = "read_file".into();
            let calls = if progress_first {
                vec![progress.clone(), read]
            } else {
                vec![read, progress.clone()]
            };
            send(&sink, TurnEventKind::ToolBatchStarted { calls }).await;
            receipt(
                &sink,
                actual_result(&progress, ToolExecutionContext::new()).await,
            )
            .await;
            assert!(latest(&seen).task_progress_report.is_none());
        }
        assert_eq!(latest(&seen).submitted_calls, 2);
        assert_eq!(latest(&seen).returned_calls, 2);
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
        let sink = StopSignal::new(signal, Some(Arc::new(Broken)), &["task_progress".into()]);
        let call = progress_call("broken-parent", "in_progress", "父观察异常仍继续");
        let context = ToolExecutionContext::new();
        register(&sink, call.clone()).await;
        receipt(&sink, actual_result(&call, context.clone()).await).await;
        assert_eq!(
            context.continuation().remaining().as_deref(),
            Some("父观察异常仍继续")
        );
    }
}
