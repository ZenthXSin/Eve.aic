//! QQ 宿主接纳的验收观察。工具回执相等只是匹配机制，不能自动赋予字段证明力。
//! 新后端应在这里增加明确的观察契约；未知工具保留待验收，不能借任意 JSON 自证成功。
use eve_developer_api::DeveloperOperation;
use eve_task_api::{
    TaskCompletionContract, TaskError, TaskEvaluationContract, TaskReceiptCheck, TaskResult,
};
use ring::digest::{SHA256, digest};
use serde_json::{Value, json};
use std::{
    path::{Component, Path},
    sync::Arc,
};

pub(crate) const OBSERVATION_RULES: &str = "回执只接受宿主真实观察：read_file /sha256 为完整文件摘要；/content 精确全文须同参数配 /truncated=false 和该全文的 /sha256，offset 必须为0。path/cwd 可冻结工程相对路径或宿主绝对路径，实际执行仍须通过当前后端能力与身份授权。run_command 仅限固定cwd和参数的 cargo test/check/clippy、python3 -I -m unittest，检查 /exit_code=0，不接受stdout或任意脚本。send_message 须同参数配 /sent=true 与 /delivery_status=sent（仅当前活跃对话轮可用）；query_file_delivery 须按既有文件投递id配 /sent=true 与 /state=Sent。排队不代表送达，未知观察保留待验收。合同只证明明确覆盖的行为，不能将单项检查扩大为整个目标成功。";

/// 只验证合同具有可信观察来源；实际结果、最后批次与无工具的文本交付由任务验收器核对。
/// Unspecified 保留原目标为待验收，要求主动执行的入口可另外拒绝未指定合同。
pub(crate) fn validate_contract(contract: &TaskCompletionContract) -> TaskResult<()> {
    contract.validate()?;
    if let TaskCompletionContract::Receipts { contract } = contract {
        for check in &contract.checks {
            let valid = match check.tool.as_str() {
                "read_file" => file_observation(check, contract),
                "run_command" => {
                    check.output_pointer == "/exit_code"
                        && check.expected == json!(0)
                        && verification_command(&check.arguments)
                }
                "send_message" => {
                    valid_message_arguments(&check.arguments)
                        && paired_observation(
                            check,
                            contract,
                            "/sent",
                            &json!(true),
                            "/delivery_status",
                            &json!("sent"),
                        )
                }
                "query_file_delivery" => {
                    valid_delivery_query(&check.arguments)
                        && paired_observation(
                            check,
                            contract,
                            "/sent",
                            &json!(true),
                            "/state",
                            &json!("Sent"),
                        )
                }
                _ => false,
            };
            if !valid {
                return Err(TaskError::InvalidInput);
            }
        }
    }
    Ok(())
}

/// 描述可接纳的单项观察。配对要求仍由 validate_contract 跨检查强制执行。
/// 工具扩展须同步添加 schema 与实际回执来源，不能仅放宽 tool 或 pointer 的字符串范围。
pub(crate) fn receipt_check_schema() -> Value {
    let check = |tool: &str, pointers: &[&str], expected: Value| {
        json!({
            "type":"object",
            "properties":{
                "tool":{"const":tool}, "arguments":{"type":"object"},
                "output_pointer":{"enum":pointers}, "expected":expected
            },
            "required":["tool","arguments","output_pointer","expected"],
            "additionalProperties":false
        })
    };
    json!({"description":OBSERVATION_RULES,"oneOf":[
        check("read_file", &["/sha256"], json!({"type":"string","pattern":"^[a-fA-F0-9]{64}$"})),
        check("read_file", &["/content"], json!({"type":"string"})),
        check("read_file", &["/truncated"], json!({"const":false})),
        check("run_command", &["/exit_code"], json!({"const":0})),
        check("send_message", &["/sent"], json!({"const":true})),
        check("send_message", &["/delivery_status"], json!({"const":"sent"})),
        check("query_file_delivery", &["/sent"], json!({"const":true})),
        check("query_file_delivery", &["/state"], json!({"const":"Sent"}))
    ]})
}

fn has_observation(
    contract: &TaskEvaluationContract,
    check: &TaskReceiptCheck,
    pointer: &str,
    expected: &Value,
) -> bool {
    contract.checks.iter().any(|other| {
        other.tool == check.tool
            && other.arguments == check.arguments
            && other.output_pointer == pointer
            && &other.expected == expected
    })
}

fn paired_observation(
    check: &TaskReceiptCheck,
    contract: &TaskEvaluationContract,
    first_pointer: &str,
    first_expected: &Value,
    second_pointer: &str,
    second_expected: &Value,
) -> bool {
    ((check.output_pointer == first_pointer && &check.expected == first_expected)
        || (check.output_pointer == second_pointer && &check.expected == second_expected))
        && has_observation(contract, check, first_pointer, first_expected)
        && has_observation(contract, check, second_pointer, second_expected)
}

fn observation_path(path: &str) -> bool {
    // 合同只冻结目标，不授予读取或执行权限；拒绝路径跳转等无效写法。
    // 绝对目标是否可访问由实际工具逐次核验，失败回执不能通过完成验收。
    if Path::new(path).is_absolute() {
        return !path.chars().any(char::is_control)
            && !Path::new(path).components().any(|part| {
                matches!(part, Component::ParentDir)
                    || matches!(part, Component::Normal(name) if name == ".git" || name == ".eve-developer")
            });
    }
    !path.is_empty()
        && !Path::new(path).has_root()
        && !path.chars().any(char::is_control)
        && !path.contains('\\')
        && !path.contains(':')
        && !path
            .split('/')
            .any(|part| matches!(part, ".." | ".git" | ".eve-developer"))
}

fn file_observation(check: &TaskReceiptCheck, contract: &TaskEvaluationContract) -> bool {
    let Some(Ok(DeveloperOperation::Read(arguments))) =
        eve_developer_tools::operation_for_tool(&check.tool, &check.arguments)
    else {
        return false;
    };
    if arguments.offset != 0 || !observation_path(&arguments.path) {
        return false;
    }
    match check.output_pointer.as_str() {
        // 后端先读取完整 bytes 再摘要；即使显示页截断，这个字段仍覆盖完整文件。
        "/sha256" => check
            .expected
            .as_str()
            .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())),
        "/content" => complete_file_content(check, contract),
        "/truncated" => {
            check.expected == json!(false)
                && contract.checks.iter().any(|other| {
                    other.tool == check.tool
                        && other.arguments == check.arguments
                        && other.output_pointer == "/content"
                        && complete_file_content(other, contract)
                })
        }
        _ => false,
    }
}

fn complete_file_content(check: &TaskReceiptCheck, contract: &TaskEvaluationContract) -> bool {
    let Some(content) = check.expected.as_str() else {
        return false;
    };
    let hash: String = digest(&SHA256, content.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    // content 会脱敏。完整字节摘要同时匹配原文，才不能把脱敏后的显示值当成文件原文。
    has_observation(contract, check, "/truncated", &json!(false))
        && has_observation(contract, check, "/sha256", &json!(hash))
}

fn verification_command(arguments: &Value) -> bool {
    let Some(Ok(DeveloperOperation::Command(command))) =
        eve_developer_tools::operation_for_tool("run_command", arguments)
    else {
        return false;
    };
    if !arguments.get("cwd").is_some_and(Value::is_string) || !observation_path(&command.cwd) {
        return false;
    }
    match command.program.as_str() {
        "cargo" => cargo_verification(&command.args),
        "python3" => {
            command.args.len() >= 4
                && command.args[0] == "-I"
                && command.args[1] == "-m"
                && command.args[2] == "unittest"
                && command.args[3..]
                    .iter()
                    .any(|argument| !argument.starts_with('-'))
                && command.args[3..].iter().all(|argument| {
                    matches!(argument.as_str(), "-v" | "-q" | "discover")
                        || (!argument.starts_with('-')
                            && !argument.is_empty()
                            && !argument.contains("..")
                            && argument
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"_.".contains(&b)))
                })
        }
        _ => false,
    }
}

fn cargo_verification(arguments: &[String]) -> bool {
    let Some((subcommand, rest)) = arguments.split_first() else {
        return false;
    };
    if !matches!(subcommand.as_str(), "test" | "check" | "clippy") {
        return false;
    }
    let mut args = rest.iter();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--workspace"
            | "--all-targets"
            | "--all-features"
            | "--no-default-features"
            | "--lib"
            | "--bins"
            | "--tests"
            | "--examples"
            | "--release"
            | "--locked"
            | "--offline" => {}
            "-p" | "--package" | "--test" | "--bin" | "--example" | "--features" => {
                if !args.next().is_some_and(|value| {
                    !value.is_empty()
                        && !value.starts_with('-')
                        && value
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_-,:/".contains(&b))
                }) {
                    return false;
                }
            }
            "--" if subcommand == "clippy" => {
                return args.map(String::as_str).collect::<Vec<_>>() == ["-D", "warnings"];
            }
            _ => return false,
        }
    }
    true
}

fn valid_message_arguments(arguments: &Value) -> bool {
    // 复用真实工具参数契约，空的 LiveDialogue 仅用于无副作用校验。
    Arc::new(eve_qqbot_plugin::LiveDialogue::default())
        .send_tool()
        .validate_arguments(arguments)
        .is_ok()
        && arguments["text"]
            .as_str()
            .is_some_and(|text| text.trim() == text)
}

fn valid_delivery_query(arguments: &Value) -> bool {
    // 对应 FileDeliveryStatusTool 的唯一 id 参数，并与持久账本 canonical id 一致。
    arguments
        .as_object()
        .is_some_and(|object| object.len() == 1)
        && arguments["id"].as_str().is_some_and(|id| {
            id.strip_prefix("file-")
                .and_then(|number| number.parse::<u64>().ok())
                .is_some_and(|number| number != 0 && id == format!("file-{number}"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(tool: &str, arguments: Value, pointer: &str, expected: Value) -> TaskReceiptCheck {
        TaskReceiptCheck {
            tool: tool.into(),
            arguments,
            output_pointer: pointer.into(),
            expected,
        }
    }
    fn receipts(checks: Vec<TaskReceiptCheck>) -> TaskCompletionContract {
        TaskCompletionContract::Receipts {
            contract: TaskEvaluationContract {
                checks,
                require_final_batch: true,
            },
        }
    }
    fn file_content(arguments: Value, content: &str) -> Vec<TaskReceiptCheck> {
        let hash: String = digest(&SHA256, content.as_bytes())
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        vec![
            check("read_file", arguments.clone(), "/content", json!(content)),
            check("read_file", arguments.clone(), "/truncated", json!(false)),
            check("read_file", arguments, "/sha256", json!(hash)),
        ]
    }
    fn command(program: &str, args: Value) -> TaskCompletionContract {
        receipts(vec![check(
            "run_command",
            json!({"program":program,"args":args,"cwd":"."}),
            "/exit_code",
            json!(0),
        )])
    }

    #[test]
    fn pending_and_pure_text_contracts_keep_their_original_semantics() {
        assert!(validate_contract(&TaskCompletionContract::Unspecified).is_ok());
        assert!(
            validate_contract(&TaskCompletionContract::Text {
                required_fragments: vec!["交付的报告".into()],
                json_equals: None,
                valid_json: false,
                json_required_pointers: vec![],
            })
            .is_ok()
        );
    }

    #[test]
    fn rejects_self_report_parameter_echo_and_arbitrary_command_output() {
        for receipt in [
            check("echo", json!({"text":"done"}), "/text", json!("done")),
            check(
                "task_progress",
                json!({"status":"done"}),
                "/status",
                json!("done"),
            ),
            check(
                "custom_json",
                json!({"success":true}),
                "/success",
                json!(true),
            ),
            check(
                "read_file",
                json!({"path":"artifact.txt"}),
                "/path",
                json!("artifact.txt"),
            ),
            check(
                "run_command",
                json!({"program":"cargo","args":["test"],"cwd":"."}),
                "/stdout",
                json!("passed"),
            ),
            check(
                "write_file",
                json!({"path":"artifact.txt","content":"done","expected_sha256":""}),
                "/success",
                json!(true),
            ),
        ] {
            assert!(validate_contract(&receipts(vec![receipt])).is_err());
        }
        for (program, arguments) in [
            ("echo", json!(["done"])),
            ("true", json!([])),
            ("sh", json!(["-c", "true"])),
            ("python3", json!(["-c", "print('passed')"])),
            ("python3", json!(["-m", "json.tool"])),
            ("python3", json!(["-m", "unittest", "--help"])),
            ("python3", json!(["-m", "unittest", "discover"])),
            ("python3", json!(["-I", "-m", "unittest"])),
            ("python3", json!(["-I", "-m", "unittest", "--help"])),
            ("python3", json!(["-I", "-m", "unittest", "-v"])),
            ("cargo", json!(["test", "--no-run"])),
            ("cargo", json!(["test", "-p", "--help"])),
            ("cargo", json!(["test", "--features", "--help"])),
            ("cargo", json!(["test", "--", "--list"])),
            (
                "cargo",
                json!(["test", "--config", "alias.test='echo passed'"]),
            ),
            ("cargo", json!(["test", ";", "true"])),
            ("/tmp/cargo", json!(["test"])),
        ] {
            assert!(validate_contract(&command(program, arguments)).is_err());
        }
    }

    #[test]
    fn accepts_only_explicit_test_invocations_with_frozen_workspace() {
        for (program, arguments) in [
            ("cargo", json!(["test"])),
            (
                "cargo",
                json!(["test", "-p", "eve-app", "--lib", "--locked"]),
            ),
            ("cargo", json!(["check", "--workspace", "--all-targets"])),
            (
                "cargo",
                json!(["clippy", "--workspace", "--", "-D", "warnings"]),
            ),
            (
                "python3",
                json!(["-I", "-m", "unittest", "test.delivery_test", "-v"]),
            ),
            ("python3", json!(["-I", "-m", "unittest", "discover", "-q"])),
        ] {
            assert!(validate_contract(&command(program, arguments)).is_ok());
        }
        for cwd in [
            None,
            Some(json!("../other")),
            Some(json!("/tmp/../other")),
            Some(json!("C:other")),
            Some(json!("C:/other/../elsewhere")),
            Some(json!("")),
            Some(json!(".git")),
        ] {
            let mut args = json!({"program":"cargo","args":["test"]});
            if let Some(cwd) = cwd {
                args["cwd"] = cwd;
            }
            assert!(
                validate_contract(&receipts(vec![check(
                    "run_command",
                    args,
                    "/exit_code",
                    json!(0)
                )]))
                .is_err()
            );
        }
    }

    #[test]
    fn absolute_targets_can_be_frozen_without_changing_the_original_arguments() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("artifact.txt");
        let contract = receipts(file_content(json!({"path": path}), "fixed artifact"));
        let saved = serde_json::to_value(&contract).unwrap();
        assert!(validate_contract(&contract).is_ok());
        assert_eq!(serde_json::to_value(contract).unwrap(), saved);
        let contract = receipts(vec![check(
            "run_command",
            json!({
                "program":"cargo", "args":["test","--locked"], "cwd":directory.path(),
            }),
            "/exit_code",
            json!(0),
        )]);
        assert!(validate_contract(&contract).is_ok());
        assert!(!observation_path(
            directory.path().join(".git/config").to_str().unwrap()
        ));
    }

    #[test]
    fn file_content_requires_complete_unredacted_bytes_from_the_same_call() {
        let args = json!({"path":"artifact.txt","offset":0});
        let checks = file_content(args.clone(), "完整产物\n");
        assert!(validate_contract(&receipts(checks.clone())).is_ok());
        assert!(validate_contract(&receipts(vec![checks[2].clone()])).is_ok());
        for remove in [1, 2] {
            let mut incomplete = checks.clone();
            incomplete.remove(remove);
            assert!(validate_contract(&receipts(incomplete)).is_err());
        }
        for (index, pointer, expected) in [
            (1, "/truncated", json!(true)),
            (2, "/sha256", json!("0".repeat(64))),
        ] {
            let mut forged = checks.clone();
            forged[index].output_pointer = pointer.into();
            forged[index].expected = expected;
            assert!(validate_contract(&receipts(forged)).is_err());
        }
        let mut other_file = checks;
        other_file[1].arguments = json!({"path":"other.txt","offset":0});
        assert!(validate_contract(&receipts(other_file)).is_err());
        for path in [
            "../artifact.txt",
            "/tmp/../artifact.txt",
            ".git/config",
            ".eve-developer/audit",
            "..\\artifact.txt",
            "C:artifact.txt",
            "C:/other/../artifact.txt",
        ] {
            assert!(
                validate_contract(&receipts(file_content(json!({"path":path}), "ok"))).is_err()
            );
        }
        assert!(
            validate_contract(&receipts(file_content(
                json!({"path":"artifact.txt","offset":1}),
                "ok"
            )))
            .is_err()
        );
    }

    #[test]
    fn delivery_requires_platform_success_and_rejects_queued_or_unbound_records() {
        for (tool, args, pointer, value) in [
            (
                "send_message",
                json!({"text":"已完成报告","purpose":"progress"}),
                "/delivery_status",
                "sent",
            ),
            (
                "query_file_delivery",
                json!({"id":"file-17"}),
                "/state",
                "Sent",
            ),
        ] {
            let paired = vec![
                check(tool, args.clone(), "/sent", json!(true)),
                check(tool, args.clone(), pointer, json!(value)),
            ];
            assert!(validate_contract(&receipts(paired.clone())).is_ok());
            assert!(validate_contract(&receipts(vec![paired[0].clone()])).is_err());
            let mut wrong_state = paired.clone();
            wrong_state[1].expected = json!("Pending");
            assert!(validate_contract(&receipts(wrong_state)).is_err());
            let mut other_call = paired;
            other_call[1].arguments = if tool == "send_message" {
                json!({"text":"别的正文"})
            } else {
                json!({"id":"file-18"})
            };
            assert!(validate_contract(&receipts(other_call)).is_err());
        }
        for (tool, args, pointer, expected) in [
            (
                "send_file",
                json!({"path":"report.txt"}),
                "/queued",
                json!(true),
            ),
            (
                "send_file",
                json!({"path":"report.txt"}),
                "/imported",
                json!(true),
            ),
            (
                "query_file_delivery",
                json!({"id":"file-17"}),
                "/id",
                json!("file-17"),
            ),
        ] {
            assert!(
                validate_contract(&receipts(vec![check(tool, args, pointer, expected)])).is_err()
            );
        }
        for id in ["file-0", "file-01", "file-+1", "file-1\n", "unbound"] {
            assert!(!valid_delivery_query(&json!({"id":id})));
        }
        assert!(!valid_message_arguments(&json!({"text":" 修改前后空白 "})));
        assert!(!valid_message_arguments(
            &json!({"text":"内容","session_id":"other"})
        ));
    }
}
