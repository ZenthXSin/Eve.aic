//! 从已有检查点和工具回执生成可读诊断；不重放工具、不把回执误认为业务完成。
use eve_llm_api::{ToolFailureCode, ToolOutput, ToolResult, TurnCheckpoint};
use eve_runtime::TurnDiagnostics;
use eve_task_api::{Task, bounded};
use serde_json::{Value, json};

fn safe_detail(text: &str, limit: usize) -> String {
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

pub(crate) fn checkpoint_steps(checkpoint: &TurnCheckpoint) -> Vec<Value> {
    let results: std::collections::HashMap<_, _> = checkpoint
        .messages
        .iter()
        .flat_map(|m| &m.tool_results)
        .map(|r| (r.call_id.as_str(), r))
        .collect();
    checkpoint
        .messages
        .iter()
        .flat_map(|m| &m.tool_calls)
        .chain(&checkpoint.pending_calls)
        .map(|call| step(&call.id, &call.name, results.get(call.id.as_str()).copied()))
        .collect()
}

pub(crate) fn task_steps(task: &Task) -> Vec<Value> {
    task.execution
        .as_ref()
        .and_then(|e| e.checkpoint.as_ref())
        .map(checkpoint_steps)
        .unwrap_or_default()
}

fn brief(steps: &[Value], limit: usize) -> String {
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
        &format!("已有 {} 步记录；最近：{}", steps.len(), lines.join("；")),
        limit,
    )
}

pub(crate) fn failure(base: &str, diagnostics: &TurnDiagnostics) -> String {
    let results: std::collections::HashMap<_, _> = diagnostics
        .tool_results
        .iter()
        .map(|r| (r.call_id.as_str(), r))
        .collect();
    let steps: Vec<_> = diagnostics
        .calls
        .iter()
        .map(|call| step(&call.id, &call.name, results.get(call.id.as_str()).copied()))
        .collect();
    let detail = brief(&steps, 340);
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
        .map(|c| brief(&checkpoint_steps(c), 340))
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
