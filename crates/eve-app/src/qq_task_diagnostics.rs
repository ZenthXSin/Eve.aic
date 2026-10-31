//! 从已有检查点和工具回执生成可读诊断；不重放工具、不把回执误认为业务完成。
use eve_llm_api::{ToolFailureCode, ToolOutput, ToolResult, TurnCheckpoint};
use eve_runtime::TurnDiagnostics;
use eve_task_api::{Task, bounded};
use serde_json::{Value, json};

pub(crate) fn safe_detail(text: &str, limit: usize) -> String {
    let mut text = text.to_owned();
    for (key, value) in std::env::vars_os() {
        let (Some(key), Some(value)) = (key.to_str(), value.to_str()) else {
            continue;
        };
        if sensitive(key) && value.len() >= 8 {
            text = text.replace(value, "[凭据已隐藏]");
        }
    }
    let lines = text
        .lines()
        .map(|line| {
            if sensitive(line) {
                "[可能含凭据的内容已隐藏]".into()
            } else {
                line.chars().filter(|c| !c.is_control()).collect::<String>()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    bounded(&lines, limit)
}

fn sensitive(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "secret",
        "token",
        "password",
        "passwd",
        "authorization",
        "api_key",
        "api-key",
        "credential",
        "private key",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

fn step(id: &str, name: &str, result: Option<&ToolResult>) -> Value {
    let (status, detail, code) = match result.map(|r| &r.output) {
        None => (
            "未确认",
            "工具已登记，但没有确认回执；不能认定成功或自动重跑".into(),
            None,
        ),
        Some(ToolOutput::Failure { code, message }) => {
            let status = match code {
                ToolFailureCode::TimedOut => "超时",
                ToolFailureCode::Cancelled => "已取消",
                ToolFailureCode::PermissionDenied | ToolFailureCode::OwnerMismatch => "权限拒绝",
                ToolFailureCode::InvalidArguments => "参数错误",
                _ => "失败",
            };
            (status, safe_detail(message, 320), Some(format!("{code:?}")))
        }
        Some(ToolOutput::Success(value)) if name == "run_command" => {
            let exit = value.get("exit_code").and_then(Value::as_i64);
            let ok = value.get("success").and_then(Value::as_bool) == Some(true);
            let mut detail = exit.map_or("没有取得退出码".into(), |c| {
                format!("命令退出码 {c}")
            });
            if !ok
                && let Some(error) = value
                    .get("stderr")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
            {
                detail.push_str(&format!("；{}", safe_detail(error, 240)));
            }
            (if ok { "已执行" } else { "命令失败" }, detail, None)
        }
        Some(ToolOutput::Success(value))
            if value.get("success").and_then(Value::as_bool) == Some(false) =>
        {
            (
                "操作未成功",
                "工具已返回未成功回执，需核对具体结果".into(),
                None,
            )
        }
        Some(ToolOutput::Success(_)) => (
            "已返回结果",
            "已保存工具回执，业务目标是否完成仍需验收".into(),
            None,
        ),
    };
    json!({"call_id":id, "tool":bounded(name, 80), "status":status, "detail":detail, "error_code":code})
}

pub(crate) struct StepWindow {
    pub(crate) total: usize,
    pub(crate) offset: usize,
    pub(crate) steps: Vec<Value>,
}

fn selected_steps<'a>(
    calls: &[(&str, &str)],
    results: impl Iterator<Item = &'a ToolResult>,
) -> Vec<Value> {
    if calls.is_empty() {
        return vec![];
    }
    let ids: std::collections::HashSet<_> = calls.iter().map(|(id, _)| *id).collect();
    let results: std::collections::HashMap<_, _> = results
        .filter(|result| ids.contains(result.call_id.as_str()))
        .map(|result| (result.call_id.as_str(), result))
        .collect();
    calls
        .iter()
        .map(|(id, name)| step(id, name, results.get(id).copied()))
        .collect()
}
fn call_count(checkpoint: &TurnCheckpoint) -> usize {
    checkpoint
        .messages
        .iter()
        .map(|message| message.tool_calls.len())
        .sum::<usize>()
        .saturating_add(checkpoint.pending_calls.len())
}
fn checkpoint_window(checkpoint: &TurnCheckpoint, offset: usize, limit: usize) -> StepWindow {
    let total = call_count(checkpoint);
    let selected: Vec<_> = checkpoint
        .messages
        .iter()
        .flat_map(|message| &message.tool_calls)
        .chain(&checkpoint.pending_calls)
        .skip(offset)
        .take(limit)
        .map(|call| (call.id.as_str(), call.name.as_str()))
        .collect();
    let steps = selected_steps(
        &selected,
        checkpoint
            .messages
            .iter()
            .flat_map(|message| &message.tool_results),
    );
    StepWindow {
        total,
        offset,
        steps,
    }
}
fn checkpoint_tail(checkpoint: &TurnCheckpoint, limit: usize) -> StepWindow {
    checkpoint_window(
        checkpoint,
        call_count(checkpoint).saturating_sub(limit),
        limit,
    )
}
pub(crate) fn task_step_window(task: &Task, offset: usize, limit: usize) -> StepWindow {
    task.execution
        .as_ref()
        .and_then(|e| e.checkpoint.as_ref())
        .map(|checkpoint| checkpoint_window(checkpoint, offset, limit))
        .unwrap_or_else(|| StepWindow {
            total: 0,
            offset,
            steps: vec![],
        })
}
pub(crate) fn task_step_tail(task: &Task, limit: usize) -> StepWindow {
    task.execution
        .as_ref()
        .and_then(|execution| execution.checkpoint.as_ref())
        .map(|checkpoint| checkpoint_tail(checkpoint, limit))
        .unwrap_or_else(|| StepWindow {
            total: 0,
            offset: 0,
            steps: vec![],
        })
}

fn brief(steps: &[Value], total: usize, limit: usize) -> String {
    if steps.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = steps
        .iter()
        .rev()
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|s| {
            format!(
                "{}：{}{}",
                s["tool"].as_str().unwrap_or("工具"),
                s["status"].as_str().unwrap_or("未知"),
                if matches!(s["status"].as_str(), Some("已返回结果" | "已执行")) {
                    String::new()
                } else {
                    format!("（{}）", s["detail"].as_str().unwrap_or_default())
                }
            )
        })
        .collect();
    bounded(
        &format!("已有 {} 步记录；最近：{}", total, lines.join("；")),
        limit,
    )
}

pub(crate) fn failure(base: &str, diagnostics: &TurnDiagnostics) -> String {
    let total = diagnostics.calls.len();
    let calls: Vec<_> = diagnostics
        .calls
        .iter()
        .skip(total.saturating_sub(3))
        .map(|call| (call.id.as_str(), call.name.as_str()))
        .collect();
    let steps = selected_steps(&calls, diagnostics.tool_results.iter());
    let detail = brief(&steps, total, 340);
    if detail.is_empty() {
        base.into()
    } else {
        bounded(
            &format!("{}\n{detail}", bounded(base, 160)),
            eve_task_api::MAX_FAILURE_BYTES,
        )
    }
}

pub(crate) fn failure_checkpoint(base: &str, checkpoint: Option<&TurnCheckpoint>) -> String {
    let detail = checkpoint
        .map(|checkpoint| {
            let page = checkpoint_tail(checkpoint, 3);
            brief(&page.steps, page.total, 340)
        })
        .unwrap_or_default();
    if detail.is_empty() {
        base.into()
    } else {
        bounded(
            &format!("{}\n{detail}", bounded(base, 160)),
            eve_task_api::MAX_FAILURE_BYTES,
        )
    }
}

pub(crate) fn interruption(task: &Task) -> String {
    let Some(execution) = &task.execution else {
        return "进程重启中断；没有可接续的检查点".into();
    };
    let reason = if execution
        .checkpoint
        .as_ref()
        .is_some_and(|c| !c.pending_calls.is_empty())
    {
        "进程在工具执行期间中断，有工具没有确认回执"
    } else if execution
        .checkpoint
        .as_ref()
        .is_some_and(|c| !c.resumable())
    {
        "已有工具失败回执，需核对结果后再决定后续操作"
    } else if execution.recoveries >= eve_task_api::MAX_AUTO_RECOVERIES {
        "自动恢复次数已用尽"
    } else if execution
        .deadline_at_ms
        .is_some_and(|at| at <= super::qq_tasks::now_ms())
    {
        "任务总执行期限已到"
    } else {
        "没有可安全接续的执行检查点"
    };
    failure_checkpoint(reason, execution.checkpoint.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_runtime::ToolCallDiagnostic;

    fn checkpoint(complete: usize, pending: usize) -> TurnCheckpoint {
        use eve_llm_api::{ChatMessage, ChatRole, ContextScope, ToolCall};
        let call = |index: usize| ToolCall {
            id: format!("call-{index}"),
            name: if index + 1 == complete {
                "run_command".into()
            } else {
                "write_file".into()
            },
            arguments: json!({}),
        };
        let mut messages = vec![ChatMessage::text(ChatRole::User, "核对原始步骤")];
        if complete > 0 {
            messages.push(
                ChatMessage::assistant_tool_calls((0..complete).map(call).collect()).unwrap(),
            );
            messages.push(ChatMessage::tool_results((0..complete).map(|index| ToolResult::success(format!("call-{index}"), if index + 1 == complete {
                json!({"success":false,"exit_code":2,"stderr":"compile failed\nAuthorization: Bearer SHOULD_NOT_LEAK"})
            } else { json!({"saved":true}) }).unwrap()).collect()).unwrap());
        }
        let batches = usize::from(complete > 0) + usize::from(pending > 0);
        TurnCheckpoint {
            version: 2,
            scope: ContextScope {
                session_id: "s".into(),
                user_id: "u".into(),
            },
            messages,
            pending_calls: (complete..complete + pending).map(call).collect(),
            used_tool_calls: complete + pending,
            delegated_tool_calls: 3,
            delegated_model_requests: 1,
            provider_requests: batches.max(1),
            tool_batches: batches,
            remaining: None,
            continuation_checks: 0,
            closing: false,
            result: None,
        }
    }

    #[test]
    fn long_checkpoint_windows_keep_complete_counts_order_pending_status_and_redaction() {
        let checkpoint = checkpoint(4994, 3);
        checkpoint.validate(5000).unwrap();
        let original = serde_json::to_vec(&checkpoint).unwrap();
        let tail = checkpoint_tail(&checkpoint, 3);
        assert_eq!((tail.total, tail.offset, tail.steps.len()), (4997, 4994, 3));
        assert!(tail.steps.iter().all(|step| step["status"] == "未确认"));
        let page = checkpoint_window(&checkpoint, 4992, 12);
        assert_eq!((page.total, page.offset, page.steps.len()), (4997, 4992, 5));
        assert_eq!(
            page.steps
                .iter()
                .map(|step| step["call_id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "call-4992",
                "call-4993",
                "call-4994",
                "call-4995",
                "call-4996"
            ]
        );
        assert_eq!(page.steps[1]["status"], "命令失败");
        assert!(
            page.steps[1]["detail"]
                .as_str()
                .unwrap()
                .contains("退出码 2")
        );
        assert!(
            !serde_json::to_string(&page.steps)
                .unwrap()
                .contains("SHOULD_NOT_LEAK")
        );
        assert!(checkpoint_window(&checkpoint, 4997, 3).steps.is_empty());
        assert!(
            checkpoint_window(&checkpoint, usize::MAX, 3)
                .steps
                .is_empty()
        );
        assert_eq!(serde_json::to_vec(&checkpoint).unwrap(), original);
        assert!(!checkpoint.resumable());
    }

    #[test]
    fn each_selected_window_matches_original_full_trace_rendering() {
        let checkpoint = checkpoint(10, 2);
        let original: Vec<_> = checkpoint
            .messages
            .iter()
            .flat_map(|message| &message.tool_calls)
            .chain(&checkpoint.pending_calls)
            .map(|call| {
                let receipt = checkpoint
                    .messages
                    .iter()
                    .flat_map(|message| &message.tool_results)
                    .find(|result| result.call_id == call.id);
                step(&call.id, &call.name, receipt)
            })
            .collect();
        for offset in [0, 1, 8, 9, 10, 11, 12, 13] {
            for limit in [0, 1, 3, 12] {
                let page = checkpoint_window(&checkpoint, offset, limit);
                assert_eq!(page.total, original.len());
                assert_eq!(page.offset, offset);
                assert_eq!(
                    page.steps,
                    original
                        .iter()
                        .skip(offset)
                        .take(limit)
                        .cloned()
                        .collect::<Vec<_>>()
                );
            }
        }
        let empty = checkpoint_tail(&self::checkpoint(0, 0), 3);
        assert_eq!((empty.total, empty.offset, empty.steps.len()), (0, 0, 0));
    }

    #[test]
    fn failure_brief_uses_full_record_count_with_only_the_last_three_steps() {
        let diagnostics = TurnDiagnostics {
            calls: (0..5000)
                .map(|ordinal| ToolCallDiagnostic {
                    id: format!("call-{ordinal}"),
                    name: format!("tool-{ordinal}"),
                    ordinal,
                })
                .collect(),
            tool_results: (4997..5000)
                .map(|ordinal| {
                    ToolResult::success(format!("call-{ordinal}"), json!({"saved":true})).unwrap()
                })
                .collect(),
            ..Default::default()
        };
        let message = failure("本轮未完成", &diagnostics);
        assert!(message.contains("已有 5000 步记录"));
        for ordinal in 4997..5000 {
            assert!(message.contains(&format!("tool-{ordinal}：已返回结果")));
        }
        assert!(!message.contains("tool-4996"));
        assert!(message.len() <= eve_task_api::MAX_FAILURE_BYTES);
    }

    #[test]
    fn failure_retains_completed_step_and_concrete_error_without_command_arguments() {
        let diagnostics = TurnDiagnostics {
            calls: vec![
                ToolCallDiagnostic {
                    id: "one".into(),
                    name: "write_file".into(),
                    ordinal: 0,
                },
                ToolCallDiagnostic {
                    id: "two".into(),
                    name: "run_command".into(),
                    ordinal: 1,
                },
            ],
            tool_results: vec![
                ToolResult::success("one", json!({"content":"PRIVATE_FILE_CONTENT"})).unwrap(),
                ToolResult::failure(
                    "two",
                    ToolFailureCode::ExecutionFailed,
                    "程序无法启动；检查 program 与构建环境",
                )
                .unwrap(),
            ],
            ..Default::default()
        };
        let message = failure("工具执行未完成；未知结果不会自动重跑", &diagnostics);
        assert!(message.contains("write_file：已返回结果"), "{message}");
        assert!(message.contains("run_command：失败"), "{message}");
        assert!(message.contains("程序无法启动"), "{message}");
        assert!(!message.contains("PRIVATE_FILE_CONTENT"));
        assert!(message.len() <= eve_task_api::MAX_FAILURE_BYTES);
    }

    #[test]
    fn failed_command_is_not_reported_as_success_and_credential_lines_are_hidden() {
        let result = ToolResult::success(
            "compile",
            json!({"success":false,"exit_code":2,
            "stderr":"编译失败\nAuthorization: Bearer SHOULD_NOT_LEAK\nmissing module"}),
        )
        .unwrap();
        let view = step("compile", "run_command", Some(&result));
        assert_eq!(view["status"], "命令失败");
        assert!(view["detail"].as_str().unwrap().contains("退出码 2"));
        assert!(!view.to_string().contains("SHOULD_NOT_LEAK"));
    }
}
