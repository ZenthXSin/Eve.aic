use crate::segment_commands::{CONSOLE_DISABLED, CONSOLE_STATE_FAILURE, SegmentCommand, execute};
use crate::{AppError, ChatSummary, HELP, input::InputEvent};
use eve_control_api::*;
use eve_llm_api::LlmError;
use eve_segment_api::{
    SegmentPlanner, SegmentPolicy, SegmentPreferences, SegmentScope, plan_with_preference,
};
use eve_session_api::{SessionInput, SessionKey};
use std::{collections::VecDeque, future::Future, io::Write, sync::Arc, time::Duration};
use tokio::{sync::mpsc, time::Instant};

const MAX_PENDING: usize = 16;

/// 终端分段显示：完整回复保存后，先显示首段，其余片段按停顿依次显示。
/// 每条回复显示前读取本会话设置；读取失败时整条显示。
pub(crate) struct Segmenter {
    pub planner: Arc<dyn SegmentPlanner>,
    pub policy: SegmentPolicy,
    pub preferences: Arc<dyn SegmentPreferences>,
    pub scope: SegmentScope,
}
/// 尚未显示的片段及其段前停顿；只是显示队列，不是新的会话输入。
type Later = VecDeque<(String, u64)>;
/// 剩余片段与原报告；显示失败时仍以 ChatOutputError 返回已保存的完整报告。
#[derive(Debug)]
struct Display {
    later: Later,
    report: Option<Box<ControlReport>>,
}

/// 保存完整控制报告，含生成输出、工具回执、提交状态和原始失败。
#[derive(Debug)]
pub struct ChatRunError {
    pub report: Box<ControlReport>,
}
impl std::fmt::Display for ChatRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.report.run.commit {
            CommitState::Pending => f.write_str("会话保存失败；原 Pending 保留，已停止接收输入"),
            CommitState::Unknown => f.write_str("任务状态未知；已停止接收输入，禁止自动重试"),
            CommitState::Completed if self.report.run.text.is_none() => {
                f.write_str("完成报告缺少文本。")
            }
            _ => match &self.report.run.failure {
                Some(RunFailure::Execution(error) | RunFailure::Delivery(error)) => error.fmt(f),
                Some(RunFailure::Session(error) | RunFailure::Commit(error)) => error.fmt(f),
                Some(RunFailure::FailureRecord { storage, .. }) => storage.fmt(f),
                _ => f.write_str("任务未正常完成"),
            },
        }
    }
}
impl std::error::Error for ChatRunError {}

/// 终端写入或刷新失败；保留已经收尾的原代报告，不改变提交结果。
pub struct ChatOutputError {
    pub output: std::io::Error,
    pub report: Box<ControlReport>,
}
impl std::fmt::Debug for ChatOutputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatOutputError")
            .field("output", &self.output)
            .field("commit", &self.report.run.commit)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for ChatOutputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "终端输出失败：{}", self.output)
    }
}
impl std::error::Error for ChatOutputError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.output)
    }
}

fn finish(
    report: ControlReport,
    output: &mut impl Write,
    summary: &mut ChatSummary,
    segmenter: Option<&Segmenter>,
) -> Result<Display, AppError> {
    let completed = report.run.commit == CommitState::Completed;
    let cancelled_delivery = report.cancel_requested
        && report.run.failure == Some(RunFailure::Delivery(LlmError::Cancelled));
    let cancelled = matches!(
        report.run.commit,
        CommitState::NotStarted | CommitState::Failed
    ) && report.cancel_requested
        && report.run.failure == Some(RunFailure::Execution(LlmError::Cancelled));
    let execution_failed = report.run.commit == CommitState::Failed
        && matches!(report.run.failure, Some(RunFailure::Execution(_)));
    let configuration_rejected = report.run.commit == CommitState::NotStarted
        && report.run.turn_id.is_none()
        && report.run.started_tools == Some(0)
        && report.run.text.is_none()
        && report.run.transcript.is_none()
        && report.run.tool_results.is_empty()
        && matches!(
            report.run.failure,
            Some(RunFailure::Execution(LlmError::Configuration(_)))
        );
    let fatal = if completed {
        report.run.text.is_none() || report.run.failure.is_some() && !cancelled_delivery
    } else {
        !cancelled && !execution_failed && !configuration_rejected
    };
    let mut later = Later::new();
    let displayed = (|| -> std::io::Result<()> {
        if completed {
            if let Some(text) = &report.run.text {
                summary.completed_turns += 1;
                // 规划失败、计划无效或已请求取消时整条显示。
                let plan = segmenter
                    .filter(|_| !fatal && !report.cancel_requested)
                    .and_then(|s| {
                        let preference = s.preferences.get(&s.scope).ok()?;
                        plan_with_preference(s.planner.as_ref(), text, &s.policy, &preference).ok()
                    })
                    .map(|(plan, _)| plan)
                    .filter(|plan| plan.segments.len() > 1);
                match plan {
                    Some(plan) => {
                        let mut parts = plan
                            .segments
                            .iter()
                            .map(|s| (text[s.start..s.end].to_owned(), s.pause_before_ms));
                        let (first, _) = parts.next().expect("multi-segment plan");
                        writeln!(output, "Eve：{first}")?;
                        later.extend(parts);
                    }
                    None => writeln!(output, "Eve：{text}")?,
                }
                if report.cancel_requested && !fatal {
                    writeln!(output, "Eve：本轮已完成并保存，取消未改写完成历史。")?;
                }
            }
        } else if cancelled {
            summary.cancelled_turns += 1;
            writeln!(output, "Eve：当前轮次已取消并完成收尾。")?;
        } else if (execution_failed || configuration_rejected)
            && let Some(RunFailure::Execution(error)) = &report.run.failure
        {
            summary.failed_turns += 1;
            writeln!(output, "Eve：本轮执行失败：{error}。未自动重试。")?;
        } else if let Some(text) = &report.run.text {
            writeln!(output, "Eve：回复已生成但未保存：{text}")?;
        }
        output.flush()
    })();
    match (fatal, displayed) {
        (false, Ok(())) => Ok(Display {
            report: (!later.is_empty()).then(|| Box::new(report)),
            later,
        }),
        (false, Err(output)) => Err(ChatOutputError {
            output,
            report: Box::new(report),
        }
        .into()),
        (true, result) => {
            let primary: AppError = ChatRunError {
                report: Box::new(report),
            }
            .into();
            Err(match result {
                Ok(()) => primary,
                Err(error) => crate::AppFailure {
                    primary,
                    secondary: vec![error.into()],
                }
                .into(),
            })
        }
    }
}
enum Next {
    Input(Option<InputEvent>),
    Done(Box<ControlResult<ControlReport>>),
    Shutdown(std::io::Result<()>),
    Segment,
}
pub(crate) async fn drive(
    control: Arc<dyn ControlService>,
    key: SessionKey,
    mut input: mpsc::Receiver<InputEvent>,
    output: &mut impl Write,
    shutdown: impl Future<Output = std::io::Result<()>>,
    segmenter: Option<Segmenter>,
) -> Result<ChatSummary, AppError> {
    let mut pending = VecDeque::new();
    let mut later = Later::new();
    let mut shown: Option<Box<ControlReport>> = None;
    let mut next_at: Option<Instant> = None;
    let mut active: Option<(GenerationKey, ControlFuture<'static, ControlReport>)> = None;
    let mut summary = ChatSummary::default();
    let mut eof = false;
    let mut quitting = false;
    let mut ordinal = 0u64;
    tokio::pin!(shutdown);
    loop {
        // 剩余片段显示完之前不开始下一轮；EOF 也先显示完已保存回复。
        if active.is_none()
            && later.is_empty()
            && !quitting
            && let Some(text) = pending.pop_front()
        {
            ordinal = ordinal.checked_add(1).ok_or("终端任务编号耗尽。")?;
            let target = control.submit(
                ControlInput {
                    session: SessionInput {
                        key: key.clone(),
                        text,
                    },
                    task_id: format!("console-{ordinal}"),
                },
                Arc::new(DiscardControlEvents),
            )?;
            let wait = control.wait(&target);
            active = Some((target, wait));
        }
        if active.is_none() && later.is_empty() && (quitting || eof && pending.is_empty()) {
            break;
        }
        let next = tokio::select! {
            biased;
            report = async {
                match &mut active {
                    Some((_, wait)) => wait.await,
                    None => std::future::pending().await,
                }
            } => Next::Done(Box::new(report)),
            signal = &mut shutdown, if !quitting => Next::Shutdown(signal),
            event = input.recv(), if !eof && !quitting => Next::Input(event),
            _ = tokio::time::sleep_until(next_at.unwrap_or_else(Instant::now)), if next_at.is_some() => Next::Segment,
        };
        match next {
            Next::Done(report) => {
                active = None;
                let display = finish((*report)?, output, &mut summary, segmenter.as_ref())?;
                (later, shown) = (display.later, display.report);
                next_at = later
                    .front()
                    .map(|(_, pause)| Instant::now() + Duration::from_millis(*pause));
            }
            Next::Segment => {
                if let Some((text, _)) = later.pop_front()
                    && let Err(error) =
                        writeln!(output, "Eve：{text}").and_then(|()| output.flush())
                {
                    return Err(ChatOutputError {
                        output: error,
                        report: shown.take().expect("segmented display keeps its report"),
                    }
                    .into());
                }
                next_at = later
                    .front()
                    .map(|(_, pause)| Instant::now() + Duration::from_millis(*pause));
            }
            Next::Shutdown(signal) => {
                signal?;
                quitting = true;
                pending.clear();
                later.clear();
                shown = None;
                next_at = None;
                input.close();
                if let Some((target, _)) = &active {
                    control.cancel(target)?;
                }
            }
            Next::Input(None) => eof = true,
            Next::Input(Some(InputEvent::Failed(error))) => return Err(error),
            Next::Input(Some(InputEvent::TooLarge)) => {
                writeln!(output, "Eve：输入超过 32768 字节，请缩短后重新提交。")?;
                output.flush()?;
            }
            // 本地命令：不进入队列、不创建会话轮次；未开启分段时也不交给模型。
            Next::Input(Some(InputEvent::Line(text))) if SegmentCommand::parse(&text).is_some() => {
                let command = SegmentCommand::parse(&text).expect("checked by guard");
                let reply = match &segmenter {
                    None => CONSOLE_DISABLED.to_owned(),
                    Some(s) => execute(s.preferences.as_ref(), &s.scope, &s.policy, command)
                        .map_err(|_| -> AppError { CONSOLE_STATE_FAILURE.into() })?,
                };
                writeln!(output, "Eve：{reply}")?;
                output.flush()?;
            }
            Next::Input(Some(InputEvent::Line(text))) => match text.trim() {
                "" => {}
                "/help" => {
                    writeln!(output, "{HELP}")?;
                    output.flush()?;
                }
                "/cancel" => {
                    pending.clear();
                    if let Some((target, _)) = &active {
                        control.cancel(target)?;
                        writeln!(output, "Eve：取消已请求，正在等待收尾。")?;
                    } else if !later.is_empty() {
                        later.clear();
                        shown = None;
                        next_at = None;
                        writeln!(output, "Eve：已停止显示剩余分段；完整回复已保存。")?;
                    } else {
                        writeln!(output, "Eve：当前没有在途任务；待处理输入已清空。")?;
                    }
                    output.flush()?;
                }
                "/quit" => {
                    quitting = true;
                    pending.clear();
                    later.clear();
                    shown = None;
                    next_at = None;
                    input.close();
                    if let Some((target, _)) = &active {
                        control.cancel(target)?;
                    }
                }
                _ => {
                    if pending.len() >= MAX_PENDING {
                        writeln!(output, "Eve：已有 16 条待处理输入，请等待后重新提交。")?;
                        output.flush()?;
                    } else {
                        pending.push_back(text);
                    }
                }
            },
        }
    }
    Ok(summary)
}

/// IO 或信号失败也先精确取消并确认收尾；禁止直接 stop 等待中的 Kernel。
pub(crate) async fn settle(
    control: &dyn ControlService,
    key: &SessionKey,
) -> Result<Option<ControlReport>, AppError> {
    let Some(snapshot) = control.snapshot(key)? else {
        return Ok(None);
    };
    if snapshot.report.is_some() {
        return Ok(None);
    }
    control.cancel(&snapshot.key)?;
    Ok(Some(control.wait(&snapshot.key).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_control_api::RunReport;

    fn report(commit: CommitState) -> ControlReport {
        ControlReport {
            key: GenerationKey {
                session: SessionKey::new("test-session", "test-user").unwrap(),
                task_id: "test-task".into(),
                controller_epoch: [1; 16],
                generation: 1,
            },
            cancel_requested: true,
            run: RunReport {
                turn_id: Some(1),
                commit,
                text: Some("已经生成".into()),
                transcript: Some(vec![]),
                started_tools: Some(1),
                tool_results: vec![],
                failure: Some(RunFailure::Delivery(LlmError::Cancelled)),
            },
        }
    }
    #[test]
    fn configuration_rejection_before_session_begin_allows_next_input() {
        let mut rejected = report(CommitState::NotStarted);
        rejected.cancel_requested = false;
        rejected.run.turn_id = None;
        rejected.run.text = None;
        rejected.run.transcript = None;
        rejected.run.started_tools = Some(0);
        let error = LlmError::Configuration("无效模型选择".into());
        rejected.run.failure = Some(RunFailure::Execution(error));
        let mut output = Vec::new();
        let mut summary = ChatSummary::default();
        finish(rejected.clone(), &mut output, &mut summary, None).unwrap();
        assert_eq!(summary.failed_turns, 1);
        assert_eq!(summary.completed_turns, 0);
        assert!(String::from_utf8(output).unwrap().contains("无效模型选择"));
        for uncertain_tools in [None, Some(1)] {
            let mut unsafe_report = rejected.clone();
            unsafe_report.run.started_tools = uncertain_tools;
            let mut output = Vec::new();
            let mut summary = ChatSummary::default();
            assert!(finish(unsafe_report, &mut output, &mut summary, None).is_err());
        }
    }

    #[test]
    fn committed_reply_wins_cancel_race_without_becoming_cancelled_or_failed() {
        let mut output = Vec::new();
        let mut summary = ChatSummary::default();
        finish(
            report(CommitState::Completed),
            &mut output,
            &mut summary,
            None,
        )
        .unwrap();
        assert_eq!(summary.completed_turns, 1);
        assert_eq!(summary.cancelled_turns, 0);
        assert_eq!(summary.failed_turns, 0);
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("取消未改写完成历史")
        );
    }
    #[test]
    fn pending_returns_original_output_and_tool_diagnostics() {
        let expected = report(CommitState::Pending);
        let error = finish(
            expected.clone(),
            &mut Vec::new(),
            &mut ChatSummary::default(),
            None,
        )
        .unwrap_err();
        let actual = error.downcast_ref::<ChatRunError>().unwrap();
        assert_eq!(*actual.report, expected);
    }
    #[derive(Clone, Copy)]
    enum OutputFault {
        Write,
        Flush,
    }
    struct FailingOutput(OutputFault);
    impl Write for FailingOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            match self.0 {
                OutputFault::Write => Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "测试写入失败",
                )),
                OutputFault::Flush => Ok(bytes.len()),
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            match self.0 {
                OutputFault::Write => Ok(()),
                OutputFault::Flush => Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "测试刷新失败",
                )),
            }
        }
    }
    fn completed_report() -> ControlReport {
        use eve_llm_api::{ChatMessage, ChatRole, ToolResult};
        let mut value = report(CommitState::Completed);
        value.cancel_requested = false;
        value.run.failure = None;
        value.run.transcript = Some(vec![
            ChatMessage::text(ChatRole::User, "原始输入"),
            ChatMessage::text(ChatRole::Assistant, "已经生成"),
        ]);
        value.run.tool_results =
            vec![ToolResult::success("echo-1", serde_json::json!({"echo":"回执"})).unwrap()];
        value
    }
    #[test]
    fn completed_output_failure_preserves_report_and_original_io_error() {
        for fault in [OutputFault::Write, OutputFault::Flush] {
            let expected = completed_report();
            let error = finish(
                expected.clone(),
                &mut FailingOutput(fault),
                &mut ChatSummary::default(),
                None,
            )
            .unwrap_err();
            let actual = error.downcast_ref::<ChatOutputError>().unwrap();
            assert_eq!(*actual.report, expected);
            assert_eq!(actual.output.kind(), std::io::ErrorKind::BrokenPipe);
            let source = std::error::Error::source(actual).unwrap();
            assert!(source.is::<std::io::Error>());
            assert!(!format!("{actual:?}").contains("原始输入"));
            assert!(!format!("{actual:?}").contains("已经生成"));
        }
    }
    #[test]
    fn cancelled_or_failed_output_failure_preserves_original_report() {
        for fault in [OutputFault::Write, OutputFault::Flush] {
            for cancelled in [false, true] {
                let mut expected = report(CommitState::Failed);
                expected.cancel_requested = cancelled;
                expected.run.failure = Some(RunFailure::Execution(if cancelled {
                    LlmError::Cancelled
                } else {
                    LlmError::Provider("模型故障".into())
                }));
                let error = finish(
                    expected.clone(),
                    &mut FailingOutput(fault),
                    &mut ChatSummary::default(),
                    None,
                )
                .unwrap_err();
                assert_eq!(
                    *error.downcast_ref::<ChatOutputError>().unwrap().report,
                    expected
                );
            }
        }
    }
    #[test]
    fn fatal_report_and_output_failure_are_both_retained() {
        for fault in [OutputFault::Write, OutputFault::Flush] {
            for commit in [
                CommitState::Pending,
                CommitState::Unknown,
                CommitState::Completed,
            ] {
                let mut expected = report(commit);
                expected.cancel_requested = false;
                let error = finish(
                    expected.clone(),
                    &mut FailingOutput(fault),
                    &mut ChatSummary::default(),
                    None,
                )
                .unwrap_err();
                let actual = error.downcast_ref::<crate::AppFailure>().unwrap();
                assert_eq!(
                    *actual
                        .primary
                        .downcast_ref::<ChatRunError>()
                        .unwrap()
                        .report,
                    expected
                );
                assert_eq!(actual.secondary.len(), 1);
                assert_eq!(
                    actual.secondary[0]
                        .downcast_ref::<std::io::Error>()
                        .unwrap()
                        .kind(),
                    std::io::ErrorKind::BrokenPipe
                );
            }
        }
    }
    #[test]
    fn completed_without_text_returns_original_report() {
        let mut expected = completed_report();
        expected.run.text = None;
        let error = finish(
            expected.clone(),
            &mut Vec::new(),
            &mut ChatSummary::default(),
            None,
        )
        .unwrap_err();
        assert_eq!(
            *error.downcast_ref::<ChatRunError>().unwrap().report,
            expected
        );
        assert_eq!(error.to_string(), "完成报告缺少文本。");
    }
}
