use crate::{AppError, ChatSummary, HELP, input::InputEvent};
use eve_control_api::*;
use eve_llm_api::LlmError;
use eve_session_api::{SessionInput, SessionKey};
use std::{collections::VecDeque, future::Future, io::Write, sync::Arc};
use tokio::sync::mpsc;

const MAX_PENDING: usize = 16;

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

fn finish(
    report: ControlReport,
    output: &mut impl Write,
    summary: &mut ChatSummary,
) -> Result<(), AppError> {
    if report.run.commit == CommitState::Completed {
        summary.completed_turns += 1;
        writeln!(
            output,
            "Eve：{}",
            report.run.text.as_deref().ok_or("完成报告缺少文本。")?
        )?;
        let cancelled_delivery = report.cancel_requested
            && report.run.failure == Some(RunFailure::Delivery(LlmError::Cancelled));
        if report.run.failure.is_some() && !cancelled_delivery {
            return Err(ChatRunError {
                report: Box::new(report),
            }
            .into());
        }
        if report.cancel_requested {
            writeln!(output, "Eve：本轮已完成并保存，取消未改写完成历史。")?;
        }
    } else if matches!(
        report.run.commit,
        CommitState::NotStarted | CommitState::Failed
    ) && report.cancel_requested
        && report.run.failure == Some(RunFailure::Execution(LlmError::Cancelled))
    {
        summary.cancelled_turns += 1;
        writeln!(output, "Eve：当前轮次已取消并完成收尾。")?;
    } else if report.run.commit == CommitState::Failed
        && matches!(report.run.failure, Some(RunFailure::Execution(_)))
    {
        summary.failed_turns += 1;
        writeln!(
            output,
            "Eve：本轮执行失败：{}。未自动重试。",
            ChatRunError {
                report: Box::new(report)
            }
        )?;
    } else {
        let displayed = if let Some(text) = &report.run.text {
            writeln!(output, "Eve：回复已生成但未保存：{text}").and_then(|()| output.flush())
        } else {
            Ok(())
        };
        let primary: AppError = ChatRunError {
            report: Box::new(report),
        }
        .into();
        return Err(match displayed {
            Ok(()) => primary,
            Err(error) => crate::AppFailure {
                primary,
                secondary: vec![error.into()],
            }
            .into(),
        });
    }
    output.flush()?;
    Ok(())
}
enum Next {
    Input(Option<InputEvent>),
    Done(ControlResult<ControlReport>),
    Shutdown(std::io::Result<()>),
}
pub(crate) async fn drive(
    control: Arc<dyn ControlService>,
    key: SessionKey,
    mut input: mpsc::Receiver<InputEvent>,
    output: &mut impl Write,
    shutdown: impl Future<Output = std::io::Result<()>>,
) -> Result<ChatSummary, AppError> {
    let mut pending = VecDeque::new();
    let mut active: Option<(GenerationKey, ControlFuture<'static, ControlReport>)> = None;
    let mut summary = ChatSummary::default();
    let mut eof = false;
    let mut quitting = false;
    let mut ordinal = 0u64;
    tokio::pin!(shutdown);
    loop {
        if active.is_none()
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
        if active.is_none() && (quitting || eof && pending.is_empty()) {
            break;
        }
        let next = tokio::select! {
            biased;
            report = async {
                match &mut active {
                    Some((_, wait)) => wait.await,
                    None => std::future::pending().await,
                }
            } => Next::Done(report),
            signal = &mut shutdown, if !quitting => Next::Shutdown(signal),
            event = input.recv(), if !eof && !quitting => Next::Input(event),
        };
        match next {
            Next::Done(report) => {
                active = None;
                finish(report?, output, &mut summary)?;
            }
            Next::Shutdown(signal) => {
                signal?;
                quitting = true;
                pending.clear();
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
                    } else {
                        writeln!(output, "Eve：当前没有在途任务；待处理输入已清空。")?;
                    }
                    output.flush()?;
                }
                "/quit" => {
                    quitting = true;
                    pending.clear();
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
    fn committed_reply_wins_cancel_race_without_becoming_cancelled_or_failed() {
        let mut output = Vec::new();
        let mut summary = ChatSummary::default();
        finish(report(CommitState::Completed), &mut output, &mut summary).unwrap();
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
        )
        .unwrap_err();
        let actual = error.downcast_ref::<ChatRunError>().unwrap();
        assert_eq!(*actual.report, expected);
    }
}
