#[path = "../../llm-openai/tests/support/mod.rs"]
mod http_support;
use http_support::{Reply, Server, final_response};
use serde_json::{Value, json};
use std::{
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
    time::Duration,
};

fn command(root: &Path, url: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_eve"));
    command
        .args(["--state-dir"])
        .arg(root.join("state"))
        .arg("--agent")
        .arg(root.join("AGENT.md"));
    for name in [
        "EVE_OPENAI_API_KEY",
        "EVE_OPENAI_MODEL",
        "EVE_OPENAI_PROTOCOL",
        "EVE_OPENAI_MODEL_ROLE",
        "EVE_OPENAI_BASE_URL",
        "EVE_OPENAI_REASONING_EFFORT",
        "EVE_OPENAI_TIMEOUT_SECONDS",
        "EVE_OPENAI_MAX_OUTPUT_TOKENS",
        "EVE_LLM_RESPONSE_MODE",
        "EVE_LLM_MAX_PARALLEL_TOOL_CALLS",
    ] {
        command.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        if name
            .to_str()
            .is_some_and(|name| name.starts_with("EVE_MODELS_"))
        {
            command.env_remove(name);
        }
    }
    command
        .env("EVE_OPENAI_API_KEY", "fixture-key")
        .env("EVE_OPENAI_MODEL", "fixture-model")
        .env("EVE_OPENAI_PROTOCOL", "responses")
        .env("EVE_OPENAI_BASE_URL", url)
        .env("EVE_OPENAI_TIMEOUT_SECONDS", "2")
        .env("EVE_OPENAI_REASONING_EFFORT", "none");
    command
}
async fn run(mut command: Command, input: &str) -> Output {
    let input = input.as_bytes().to_vec();
    tokio::time::timeout(
        Duration::from_secs(15),
        tokio::task::spawn_blocking(move || {
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let write_result = stdin.write_all(&input);
            drop(stdin);
            let output = child.wait_with_output().unwrap();
            if let Err(error) = write_result {
                assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
                assert!(
                    !output.status.success(),
                    "successful process lost test input"
                );
            }
            output
        }),
    )
    .await
    .unwrap()
    .unwrap()
}
fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("AGENT.md"), "你是 Eve。称呼用户主人。").unwrap();
    root
}
fn sessions(root: &Path) -> Value {
    let outer: Value =
        serde_json::from_slice(&std::fs::read(root.join("state/state.json")).unwrap()).unwrap();
    let bytes: Vec<u8> = outer["entries"]["eve.session"]["sessions.v1"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| u8::try_from(v.as_u64().unwrap()).unwrap())
        .collect();
    serde_json::from_slice(&bytes).unwrap()
}
fn user_texts(request: &Value) -> Vec<&str> {
    request["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].as_str().unwrap())
        .collect()
}
#[tokio::test]
async fn real_process_chat_tool_round_trip_and_restart_preserve_history_without_replay() {
    let root = fixture();
    let mut server = Server::start(vec![
        Reply::json(json!({"status":"completed","error":null,"output":[
            {"type":"function_call","call_id":"echo-1","name":"echo","arguments":"{\"text\":\"核心回执\"}"}
        ]})),
        Reply::json(final_response("主人，核心回执。")),
        Reply::json(final_response("主人，第二轮已记住。")),
        Reply::json(final_response("主人，重启后仍有历史。")),
    ]).await;
    let first = run(
        command(root.path(), &server.url),
        "请回显核心回执\n第二轮\n",
    )
    .await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(
        String::from_utf8(first.stdout).unwrap(),
        "Eve：主人，核心回执。\nEve：主人，第二轮已记住。\n"
    );
    let initial = server.next().await;
    assert!(initial.headers.starts_with("POST /v1/responses HTTP/1.1"));
    assert!(
        initial
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer fixture-key")
    );
    assert_eq!(initial.body["model"], "fixture-model");
    assert_eq!(initial.body["store"], false);
    assert_eq!(initial.body["stream"], false);
    assert_eq!(initial.body["tools"].as_array().unwrap().len(), 1);
    assert!(
        initial.body["input"][0]["content"]
            .as_str()
            .unwrap()
            .contains("称呼用户主人")
    );
    let follow = server.next().await.body;
    let result = follow["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["type"] == "function_call_output")
        .unwrap();
    assert_eq!(result["call_id"], "echo-1");
    assert_eq!(
        serde_json::from_str::<Value>(result["output"].as_str().unwrap()).unwrap(),
        json!({"echo":"核心回执"})
    );
    let second = server.next().await.body;
    assert_eq!(user_texts(&second), ["请回显核心回执", "第二轮"]);
    let document = sessions(root.path());
    assert_eq!(document["sessions"]["default"]["revision"], 4);
    assert_eq!(
        document["sessions"]["default"]["turns"][0]["status"]["messages"]
            .as_array()
            .unwrap()
            .len(),
        4
    );

    // 真正新进程重新装配身份和 Provider；旧工具只作为配对历史传入。
    std::fs::write(root.path().join("AGENT.md"), "你是 Eve 新身份。").unwrap();
    let restarted = run(command(root.path(), &server.url), "重启轮\n").await;
    assert!(
        restarted.status.success(),
        "{}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    let recovered = server.next().await.body;
    assert!(
        recovered["input"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Eve 新身份")
    );
    assert_eq!(
        user_texts(&recovered),
        ["请回显核心回执", "第二轮", "重启轮"]
    );
    assert_eq!(
        recovered["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["type"] == "function_call_output")
            .count(),
        1
    );
    assert!(server.requests.try_recv().is_err());
    assert_eq!(sessions(root.path())["sessions"]["default"]["revision"], 6);
    // 身份和凭据不作为会话状态保存。
    let disk = std::fs::read_to_string(root.path().join("state/state.json")).unwrap();
    let inner = sessions(root.path()).to_string();
    assert!(!inner.contains("新身份"));
    assert!(!disk.contains("fixture-key"));
    // 仅从环境读取配置不会自动把配置或凭据写入文件。
    assert!(!root.path().join("state/configuration/config.json").exists());
    println!("Eve 核心进程验收：两轮聊天、一次工具往返、重启第三轮；历史修订 4→6，旧工具零重放。");
}
#[tokio::test]
async fn provider_failure_is_not_retried_or_added_to_completed_history() {
    let root = fixture();
    let mut failed = Reply::json(json!({"error":"fixture-key 不应外泄"}));
    failed.status = 500;
    let mut server = Server::start(vec![failed, Reply::json(final_response("下一轮成功"))]).await;
    let output = run(command(root.path(), &server.url), "失败输入\n新输入\n").await;
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-key"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fixture-key"));
    assert_eq!(user_texts(&server.next().await.body), ["失败输入"]);
    assert_eq!(user_texts(&server.next().await.body), ["新输入"]);
    assert!(server.requests.try_recv().is_err());
    let turns = sessions(root.path())["sessions"]["default"]["turns"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0]["status"]["state"], "Failed");
    assert_eq!(turns[0]["status"]["failure"]["code"], "Provider");
    assert_eq!(turns[1]["status"]["state"], "Completed");
}
#[tokio::test]
async fn missing_identity_or_credential_never_sends_http_or_creates_state() {
    let root = fixture();
    let mut server = Server::start(vec![]).await;
    std::fs::remove_file(root.path().join("AGENT.md")).unwrap();
    let output = run(command(root.path(), &server.url), "/quit\n").await;
    assert!(!output.status.success());
    assert!(!root.path().join("state").exists());
    std::fs::write(root.path().join("AGENT.md"), "Eve").unwrap();
    let mut missing = command(root.path(), &server.url);
    missing.env_remove("EVE_OPENAI_API_KEY");
    assert!(!run(missing, "/quit\n").await.status.success());
    assert!(!root.path().join("state").exists());
    assert!(server.requests.try_recv().is_err());
}
#[tokio::test]
async fn corrupt_state_is_preserved_and_session_owner_cannot_be_changed() {
    let root = fixture();
    std::fs::create_dir(root.path().join("state")).unwrap();
    let bad = b"corrupt private state";
    std::fs::write(root.path().join("state/state.json"), bad).unwrap();
    let mut server = Server::start(vec![
        Reply::json(final_response("第一轮")),
        Reply::json(final_response("恢复用户")),
    ])
    .await;
    let output = run(command(root.path(), &server.url), "不能发送\n").await;
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("private state"));
    assert_eq!(
        std::fs::read(root.path().join("state/state.json")).unwrap(),
        bad
    );
    // 仅测试夹具显式移除损坏文件，应用本身不会自动清空。
    std::fs::remove_file(root.path().join("state/state.json")).unwrap();
    assert!(
        run(command(root.path(), &server.url), "原用户\n")
            .await
            .status
            .success()
    );
    server.next().await;
    let before = std::fs::read(root.path().join("state/state.json")).unwrap();
    let mut other = command(root.path(), &server.url);
    other.args(["--user", "other"]);
    assert!(!run(other, "越权输入\n").await.status.success());
    assert_eq!(
        std::fs::read(root.path().join("state/state.json")).unwrap(),
        before
    );
    assert!(server.requests.try_recv().is_err());
    // 失败路径也释放锁并停止插件，原用户可以再次独立启动。
    assert!(
        run(command(root.path(), &server.url), "原用户继续\n")
            .await
            .status
            .success()
    );
    assert_eq!(
        user_texts(&server.next().await.body),
        ["原用户", "原用户继续"]
    );
}
#[tokio::test]
async fn oversized_input_and_local_commands_do_not_create_turns() {
    let root = fixture();
    let mut server = Server::start(vec![Reply::json(final_response("有效输入成功"))]).await;
    let text = format!("{}\n/help\n\n有效输入\n", "长".repeat(12000));
    let output = run(command(root.path(), &server.url), &text).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(user_texts(&server.next().await.body), ["有效输入"]);
    assert!(server.requests.try_recv().is_err());
    assert_eq!(
        sessions(root.path())["sessions"]["default"]["turns"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
#[tokio::test]
async fn final_save_failure_keeps_pending_and_stops_after_actual_tool_execution() {
    let root = fixture();
    let mut generated = Reply::json(final_response("已生成的完整回复"));
    generated.body_delay = Duration::from_secs(2);
    let mut server = Server::start(vec![
        Reply::json(json!({"status":"completed","error":null,"output":[
            {"type":"function_call","call_id":"commit-echo","name":"echo","arguments":"{\"text\":\"保存失败回执\"}"}
        ]})),
        generated,
    ])
    .await;
    let mut child = command(root.path(), &server.url);
    child.env("EVE_OPENAI_TIMEOUT_SECONDS", "5");
    let process =
        tokio::spawn(async move { run(child, "执行工具后保存\n不能再执行\n").await });
    server.next().await;
    let paired = server.next().await.body;
    assert!(paired["input"].as_array().unwrap().iter().any(|item| {
        item["type"] == "function_call_output" && item["call_id"] == "commit-echo"
    }));
    // Pending 已落盘、工具已真实执行；明确注入最终原子替换失败。
    let target = root.path().join("state/state.json");
    let preserved = root.path().join("state/preserved.json");
    let original = std::fs::read(&target).unwrap();
    std::fs::rename(&target, &preserved).unwrap();
    std::fs::create_dir(&target).unwrap();
    let output = process.await.unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("回复已生成但未保存：已生成的完整回复")
    );
    assert!(server.requests.try_recv().is_err());
    assert_eq!(std::fs::read(&preserved).unwrap(), original);
    std::fs::remove_dir(&target).unwrap();
    std::fs::rename(&preserved, &target).unwrap();
    let document = sessions(root.path());
    let turns = document["sessions"]["default"]["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0]["status"]["state"], "Pending");
}
#[test]
fn cli_requires_values_and_valid_session_identifiers() {
    use eve_app::ChatOptions;
    let parse = |args: &[&str]| {
        ChatOptions::parse(args.iter().map(|value| std::ffi::OsString::from(*value)))
    };
    assert!(parse(&["--agent"]).is_err());
    assert!(parse(&["--session", " "]).is_err());
    assert!(parse(&["--user", "bad\nuser"]).is_err());
    assert!(parse(&["--unknown"]).is_err());
    assert!(parse(&["--help"]).unwrap().is_none());
    let selected = parse(&["--session", "my-session", "--user", "my-user"])
        .unwrap()
        .unwrap();
    assert_eq!(selected.session_id, "my-session");
    assert_eq!(selected.user_id, "my-user");
}

struct Interactive {
    child: Option<std::process::Child>,
    stdin: Option<std::process::ChildStdin>,
}
impl Interactive {
    fn start(mut command: Command) -> Self {
        command.env("EVE_OPENAI_TIMEOUT_SECONDS", "30");
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        Self {
            child: Some(child),
            stdin,
        }
    }
    fn write(&mut self, text: &str) {
        self.write_bytes(text.as_bytes());
    }
    fn write_bytes(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(bytes).unwrap();
        stdin.flush().unwrap();
    }
    async fn finish(mut self) -> Output {
        // stdin 故意保持打开；退出不能等标准输入读线程结束。
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if self.child.as_mut().unwrap().try_wait().unwrap().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("命令退出/取消收尾超过期限");
        self.child.take().unwrap().wait_with_output().unwrap()
    }
}
impl Drop for Interactive {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
async fn completed(root: &Path, index: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if sessions(root)["sessions"]["default"]["turns"][index]["status"]["state"]
                == "Completed"
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
fn delayed(text: &str) -> Reply {
    let mut reply = Reply::json(final_response(text));
    reply.body_delay = Duration::from_secs(30);
    reply
}
#[tokio::test]
async fn cancel_clears_bounded_queue_then_new_input_and_restart_succeed() {
    let root = fixture();
    let mut server = Server::start(vec![
        delayed("不应显示的旧回复"),
        Reply::json(final_response("后续任务完成")),
        Reply::json(final_response("恢复完成")),
    ])
    .await;
    let mut process = Interactive::start(command(root.path(), &server.url));
    process.write("旧任务\n");
    assert_eq!(user_texts(&server.next().await.body), ["旧任务"]);
    process.write(&("不应启动的排队输入\n".repeat(18) + "/cancel\n新任务\n"));
    assert_eq!(user_texts(&server.next().await.body), ["新任务"]);
    completed(root.path(), 1).await;
    process.write("/quit\n");
    let output = process.finish().await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("当前轮次已取消并完成收尾"));
    assert!(stdout.contains("已有 16 条待处理输入"));
    assert!(stdout.contains("后续任务完成"));
    assert!(!stdout.contains("不应显示的旧回复"));
    let document = sessions(root.path());
    assert_eq!(
        document["sessions"]["default"]["turns"][0]["status"]["failure"]["code"],
        "Cancelled"
    );
    assert_eq!(
        document["sessions"]["default"]["turns"][0]["status"]["failure"]["started_tools"],
        0
    );
    assert_eq!(
        document["sessions"]["default"]["turns"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(
        run(command(root.path(), &server.url), "重启新轮\n")
            .await
            .status
            .success()
    );
    assert_eq!(
        user_texts(&server.next().await.body),
        ["新任务", "重启新轮"]
    );
    assert!(server.requests.try_recv().is_err());
}
#[tokio::test]
async fn quit_cancels_inflight_request_with_open_stdin_and_discards_queue() {
    let root = fixture();
    let mut server = Server::start(vec![delayed("不应显示")]).await;
    let mut process = Interactive::start(command(root.path(), &server.url));
    process.write("等待模型\n");
    server.next().await;
    process.write("排队输入\n/quit\n");
    let output = process.finish().await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        sessions(root.path())["sessions"]["default"]["turns"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        sessions(root.path())["sessions"]["default"]["turns"][0]["status"]["failure"]["code"],
        "Cancelled"
    );
    assert!(server.requests.try_recv().is_err());
}
#[tokio::test]
async fn quit_after_tool_result_waits_for_cancel_and_restart_never_replays_tool() {
    let root = fixture();
    let mut server = Server::start(vec![
        Reply::json(json!({"status":"completed","error":null,"output":[
            {"type":"function_call","call_id":"cancel-echo","name":"echo","arguments":"{\"text\":\"取消前回执\"}"}
        ]})),
        delayed("不应显示的工具最终回复"),
        Reply::json(final_response("新的独立任务")),
    ]).await;
    let mut process = Interactive::start(command(root.path(), &server.url));
    process.write("调用一次 echo\n");
    server.next().await;
    let request = server.next().await.body;
    let result = request["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["type"] == "function_call_output")
        .unwrap();
    assert_eq!(result["call_id"], "cancel-echo");
    assert_eq!(
        serde_json::from_str::<Value>(result["output"].as_str().unwrap()).unwrap(),
        json!({"echo":"取消前回执"})
    );
    process.write("/quit\n");
    assert!(process.finish().await.status.success());
    let failure =
        sessions(root.path())["sessions"]["default"]["turns"][0]["status"]["failure"].clone();
    assert_eq!(failure["code"], "Cancelled");
    assert_eq!(failure["started_tools"], 1);
    assert!(
        run(command(root.path(), &server.url), "独立新输入\n")
            .await
            .status
            .success()
    );
    assert_eq!(user_texts(&server.next().await.body), ["独立新输入"]);
    assert!(server.requests.try_recv().is_err());
}
#[tokio::test]
async fn invalid_utf8_during_request_cancels_and_saves_failure_before_exit() {
    let root = fixture();
    let mut server = Server::start(vec![delayed("不应显示")]).await;
    let mut process = Interactive::start(command(root.path(), &server.url));
    process.write("运行中的输入\n");
    server.next().await;
    process.write_bytes(&[0xff, b'\n']);
    let output = process.finish().await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("UTF-8"));
    assert_eq!(
        sessions(root.path())["sessions"]["default"]["turns"][0]["status"]["failure"]["code"],
        "Cancelled"
    );
}
#[tokio::test]
async fn cancellation_record_failure_keeps_pending_and_stops_new_input() {
    let root = fixture();
    let mut server = Server::start(vec![delayed("不应显示")]).await;
    let mut process = Interactive::start(command(root.path(), &server.url));
    process.write("取消保存失败\n");
    server.next().await;
    let target = root.path().join("state/state.json");
    let preserved = root.path().join("state/preserved.json");
    let original = std::fs::read(&target).unwrap();
    std::fs::rename(&target, &preserved).unwrap();
    std::fs::create_dir(&target).unwrap();
    process.write("/cancel\n不能启动新轮\n");
    let output = process.finish().await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Pending"));
    assert!(server.requests.try_recv().is_err());
    assert_eq!(std::fs::read(&preserved).unwrap(), original);
    std::fs::remove_dir(&target).unwrap();
    std::fs::rename(&preserved, &target).unwrap();
    assert_eq!(
        sessions(root.path())["sessions"]["default"]["turns"][0]["status"]["state"],
        "Pending"
    );
}
#[cfg(unix)]
#[tokio::test]
async fn actual_sigint_cancels_and_exits_even_when_stdin_remains_open() {
    let root = fixture();
    let mut server = Server::start(vec![delayed("不应显示")]).await;
    let mut process = Interactive::start(command(root.path(), &server.url));
    process.write("Ctrl+C 运行中轮次\n");
    server.next().await;
    let pid = process.child.as_ref().unwrap().id();
    assert!(
        Command::new("kill")
            .args(["-INT", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    assert!(process.finish().await.status.success());
    assert_eq!(
        sessions(root.path())["sessions"]["default"]["turns"][0]["status"]["failure"]["code"],
        "Cancelled"
    );
}

#[tokio::test]
async fn same_process_entry_releases_directory_before_second_invocation() {
    if let Some(root) = std::env::var_os("EVE_TEST_REENTER_ROOT") {
        let root = std::path::PathBuf::from(root);
        let options = eve_app::ChatOptions {
            state_directory: root.join("state"),
            agent_path: root.join("AGENT.md"),
            ..eve_app::ChatOptions::default()
        };
        let mut output = Vec::new();
        eve_app::run_console(
            options.clone(),
            std::io::Cursor::new("同进程第一轮"),
            &mut output,
        )
        .await
        .unwrap();
        eve_app::run_console(options, std::io::Cursor::new("同进程第二轮"), &mut output)
            .await
            .unwrap();
        assert!(String::from_utf8(output).unwrap().contains("第二轮完成"));
        return;
    }
    let root = fixture();
    let mut server = Server::start(vec![
        Reply::json(final_response("第一轮完成")),
        Reply::json(final_response("第二轮完成")),
    ])
    .await;
    // 在独立测试子进程注入环境；测试进程本身不修改全局环境。
    let configured = command(root.path(), &server.url);
    let mut harness = Command::new(std::env::current_exe().unwrap());
    for (name, value) in configured.get_envs() {
        if let Some(value) = value {
            harness.env(name, value);
        } else {
            harness.env_remove(name);
        }
    }
    harness.args([
        "--exact",
        "same_process_entry_releases_directory_before_second_invocation",
        "--nocapture",
    ]);
    harness.env("EVE_TEST_REENTER_ROOT", root.path());
    let result = run(harness, "").await;
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(user_texts(&server.next().await.body), ["同进程第一轮"]);
    assert_eq!(
        user_texts(&server.next().await.body),
        ["同进程第一轮", "同进程第二轮"]
    );
}

#[tokio::test]
async fn output_failure_preserves_committed_tool_report_and_allows_reopen() {
    if let Some(root) = std::env::var_os("EVE_TEST_OUTPUT_FAILURE_ROOT") {
        struct FailingOutput {
            fail_on_flush: bool,
        }
        impl Write for FailingOutput {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.fail_on_flush {
                    Ok(bytes.len())
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "测试终端写入失败",
                    ))
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                if self.fail_on_flush {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "测试终端刷新失败",
                    ))
                } else {
                    Ok(())
                }
            }
        }
        let root = std::path::PathBuf::from(root);
        let options = eve_app::ChatOptions {
            state_directory: root.join("state"),
            agent_path: root.join("AGENT.md"),
            ..eve_app::ChatOptions::default()
        };
        let fail_on_flush = std::env::var("EVE_TEST_OUTPUT_FAILURE_MODE").unwrap() == "flush";
        let error = eve_app::run_console(
            options.clone(),
            std::io::Cursor::new("请回显故障回执\n不能执行的排队输入\n"),
            FailingOutput { fail_on_flush },
        )
        .await
        .unwrap_err();
        let actual = error.downcast_ref::<eve_app::ChatOutputError>().unwrap();
        assert_eq!(actual.output.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(
            actual.report.run.commit,
            eve_control_api::CommitState::Completed
        );
        assert_eq!(actual.report.run.text.as_deref(), Some("故障回执已保存"));
        assert_eq!(actual.report.run.started_tools, Some(1));
        assert_eq!(actual.report.run.tool_results.len(), 1);
        assert_eq!(actual.report.run.transcript.as_ref().unwrap().len(), 4);
        assert!(actual.report.run.failure.is_none());
        let first = sessions(&root)["sessions"]["default"].clone();
        assert_eq!(first["revision"], 2);
        assert_eq!(first["turns"].as_array().unwrap().len(), 1);
        assert_eq!(first["turns"][0]["status"]["state"], "Completed");
        assert_eq!(
            first["turns"][0]["status"]["messages"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        // 输出失败没有改写已提交历史；同进程重新打开状态目录并继续。
        let mut output = Vec::new();
        let summary = eve_app::run_console(
            options,
            std::io::Cursor::new("输出故障后继续\n"),
            &mut output,
        )
        .await
        .unwrap();
        assert_eq!(summary.completed_turns, 1);
        assert_eq!(String::from_utf8(output).unwrap(), "Eve：恢复完成\n");
        let restored = sessions(&root)["sessions"]["default"].clone();
        assert_eq!(restored["revision"], 4);
        assert_eq!(restored["turns"].as_array().unwrap().len(), 2);
        assert_eq!(restored["turns"][0], first["turns"][0]);
        assert_eq!(restored["turns"][1]["status"]["state"], "Completed");
        return;
    }
    for mode in ["write", "flush"] {
        let root = fixture();
        let mut server = Server::start(vec![
            Reply::json(json!({"status":"completed","error":null,"output":[
                {"type":"function_call","call_id":"echo-fault","name":"echo","arguments":"{\"text\":\"故障回执\"}"}
            ]})),
            Reply::json(final_response("故障回执已保存")),
            Reply::json(final_response("恢复完成")),
        ]).await;
        let configured = command(root.path(), &server.url);
        let mut harness = Command::new(std::env::current_exe().unwrap());
        for (name, value) in configured.get_envs() {
            if let Some(value) = value {
                harness.env(name, value);
            } else {
                harness.env_remove(name);
            }
        }
        harness.args([
            "--exact",
            "output_failure_preserves_committed_tool_report_and_allows_reopen",
            "--nocapture",
        ]);
        harness
            .env("EVE_TEST_OUTPUT_FAILURE_ROOT", root.path())
            .env("EVE_TEST_OUTPUT_FAILURE_MODE", mode);
        let result = run(harness, "").await;
        assert!(
            result.status.success(),
            "{mode}: {}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(user_texts(&server.next().await.body), ["请回显故障回执"]);
        let follow = server.next().await.body;
        let result = follow["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["type"] == "function_call_output")
            .unwrap();
        assert_eq!(result["call_id"], "echo-fault");
        assert_eq!(
            serde_json::from_str::<Value>(result["output"].as_str().unwrap()).unwrap(),
            json!({"echo":"故障回执"})
        );
        let restored = server.next().await.body;
        assert_eq!(user_texts(&restored), ["请回显故障回执", "输出故障后继续"]);
        assert_eq!(
            restored["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["type"] == "function_call_output")
                .count(),
            1
        );
        assert!(server.requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn rejected_primary_role_never_calls_model_or_changes_completed_state() {
    let root = fixture();
    let mut server = Server::start(vec![Reply::json(final_response("已保存历史"))]).await;
    let first = run(command(root.path(), &server.url), "先保存一轮\n").await;
    assert!(first.status.success());
    server.next().await;
    let state_path = root.path().join("state/state.json");
    let before = std::fs::read(&state_path).unwrap();
    for invalid in ["disabled", "provider", "reference", "role", "deadline"] {
        let mut configured = command(root.path(), &server.url);
        configured.env("EVE_OPENAI_MODEL_ROLE", "primary");
        if invalid != "disabled" {
            configured
                .env("EVE_MODELS_PRIMARY_ENABLED", "true")
                .env("EVE_MODELS_PRIMARY_PROVIDER", "openai")
                .env("EVE_MODELS_PRIMARY_MODEL", "configured-primary");
        }
        match invalid {
            "provider" => {
                configured.env("EVE_MODELS_PRIMARY_PROVIDER", "unknown");
            }
            "reference" => {
                configured.env(
                    "EVE_MODELS_PRIMARY_CREDENTIAL_REF",
                    "PRIVATE_REFERENCE_VALUE",
                );
            }
            "role" => {
                configured.env("EVE_OPENAI_MODEL_ROLE", "jev");
            }
            "deadline" => {
                configured.env("EVE_MODELS_PRIMARY_TIMEOUT_MS", "600001");
            }
            _ => {}
        }
        let result = run(configured, "这条不能执行\n").await;
        assert!(!result.status.success(), "{invalid}");
        let diagnostics = String::from_utf8_lossy(&result.stderr);
        assert!(!diagnostics.contains("fixture-key"));
        assert!(!diagnostics.contains("PRIVATE_REFERENCE_VALUE"));
        assert_eq!(std::fs::read(&state_path).unwrap(), before, "{invalid}");
        assert!(server.requests.try_recv().is_err(), "{invalid}");
    }
}

const SEGMENTED: &str = "好的主人，结论是可以分段显示。\n\n完整回复仍然只保存一次，分段只决定在哪里断开，以及每段之前停顿多久。\n\n需要我再演示一次吗？";
const PARTS: [&str; 3] = [
    "好的主人，结论是可以分段显示。",
    "完整回复仍然只保存一次，分段只决定在哪里断开，以及每段之前停顿多久。",
    "需要我再演示一次吗？",
];
fn segmented(root: &Path, url: &str) -> Command {
    let mut command = command(root, url);
    command.arg("--segmented");
    command
}
/// 完整回复以未拆分的原文保存在完成轮次中。
fn saves_full_reply(turn: &Value) -> bool {
    let encoded = serde_json::to_string(SEGMENTED).unwrap();
    turn["status"]["state"] == "Completed"
        && turn["status"]
            .to_string()
            .contains(encoded.trim_matches('"'))
}
fn eve_lines(stdout: &[u8]) -> Vec<String> {
    String::from_utf8(stdout.to_vec())
        .unwrap()
        .split("Eve：")
        .skip(1)
        .map(|line| line.trim_end().to_owned())
        .collect()
}
#[tokio::test]
async fn segmented_reply_displays_parts_in_order_after_saving_the_full_reply_once() {
    let root = fixture();
    let mut server = Server::start(vec![
        Reply::json(final_response(SEGMENTED)),
        Reply::json(final_response("短回复整条显示。")),
    ])
    .await;
    let started = std::time::Instant::now();
    let output = run(segmented(root.path(), &server.url), "请分段\n下一轮\n").await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut expected: Vec<String> = PARTS.iter().map(|p| p.to_string()).collect();
    expected.push("短回复整条显示。".into());
    assert_eq!(eve_lines(&output.stdout), expected);
    // 两次段前停顿：400 ms 基础值加每字 25 ms，上限 2.5 秒。
    let pauses: u64 = PARTS[1..]
        .iter()
        .map(|p| (400 + 25 * p.chars().count() as u64).min(2500))
        .sum();
    assert!(started.elapsed() >= Duration::from_millis(pauses));
    let turns = &sessions(root.path())["sessions"]["default"]["turns"];
    assert_eq!(turns.as_array().unwrap().len(), 2);
    assert!(saves_full_reply(&turns[0]));
    assert_eq!(user_texts(&server.next().await.body), ["请分段"]);
    // 下一轮在剩余片段显示后才开始，历史中是完整回复而不是片段。
    let second = server.next().await.body;
    assert_eq!(user_texts(&second), ["请分段", "下一轮"]);
    assert!(
        second
            .to_string()
            .contains("以及每段之前停顿多久。\\n\\n需要我再演示一次吗？")
    );
}
// 测试线程会阻塞读取子进程输出；替身 HTTP 服务需要另一个运行时线程。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_between_displayed_parts_stops_the_rest_without_changing_history() {
    let root = fixture();
    let mut server = Server::start(vec![
        Reply::json(final_response(SEGMENTED)),
        Reply::json(final_response("新任务完成")),
    ])
    .await;
    let mut child = segmented(root.path(), &server.url)
        .env("EVE_OPENAI_TIMEOUT_SECONDS", "30")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (lines, received) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout).lines() {
            if lines.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let next = || received.recv_timeout(Duration::from_secs(10)).unwrap();
    stdin.write_all("请分段\n".as_bytes()).unwrap();
    stdin.flush().unwrap();
    assert_eq!(next(), format!("Eve：{}", PARTS[0]));
    stdin.write_all("/cancel\n新任务\n".as_bytes()).unwrap();
    stdin.flush().unwrap();
    assert_eq!(next(), "Eve：已停止显示剩余分段；完整回复已保存。");
    assert_eq!(next(), "Eve：新任务完成");
    stdin.write_all(b"/quit\n").unwrap();
    stdin.flush().unwrap();
    let status = child.wait().unwrap();
    reader.join().unwrap();
    assert!(status.success());
    assert!(received.try_recv().is_err());
    let turns = &sessions(root.path())["sessions"]["default"]["turns"];
    assert_eq!(turns.as_array().unwrap().len(), 2);
    assert_eq!(turns[0]["status"]["state"], "Completed");
    assert!(saves_full_reply(&turns[0]));
    server.next().await;
    assert_eq!(user_texts(&server.next().await.body), ["请分段", "新任务"]);
    assert!(server.requests.try_recv().is_err());
}

fn segment_preferences(root: &Path) -> Option<Value> {
    let outer: Value =
        serde_json::from_slice(&std::fs::read(root.join("state/state.json")).ok()?).unwrap();
    let bytes: Vec<u8> = outer["entries"]["eve.segment.preferences"]["preferences.v1"]
        .as_array()?
        .iter()
        .map(|v| u8::try_from(v.as_u64().unwrap()).unwrap())
        .collect();
    Some(serde_json::from_slice(&bytes).unwrap())
}
const OFF: &str = "已关闭本会话分段：下一条回复起整条发送；发送 /segment on 可重新开启。";

#[tokio::test]
async fn segment_settings_are_local_persist_per_session_and_apply_from_next_reply() {
    let root = fixture();
    let mut server = Server::start(
        (0..4)
            .map(|_| Reply::json(final_response(SEGMENTED)))
            .collect(),
    )
    .await;
    let output = run(
        segmented(root.path(), &server.url),
        "/segment off\n请分段\n/segment\n",
    )
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        eve_lines(&output.stdout),
        [
            OFF.to_string(),
            "本会话分段：关闭，整条发送（已设置）；最多 3 段（默认）；段间停顿 100%（默认），单次不超过 2.5 秒。设置只影响本会话，从下一条回复生效；发送 /segment help 查看命令。".to_string(),
            SEGMENTED.to_string(),
        ]
    );
    assert_eq!(
        segment_preferences(root.path()).unwrap(),
        json!({"version": 1, "scopes": [{"scope": {"channel": "console", "session_id": "default", "user_id": "owner"},
            "enabled": false, "max_segments": null, "pause_percent": null}]})
    );
    // 重启后设置仍然生效；其他会话不受影响。
    let output = run(segmented(root.path(), &server.url), "请分段\n").await;
    assert_eq!(eve_lines(&output.stdout), [SEGMENTED]);
    let mut other = segmented(root.path(), &server.url);
    other.args(["--session", "other"]);
    let output = run(other, "/segment parts 2\n请分段\n").await;
    assert_eq!(
        eve_lines(&output.stdout),
        [
            "已将本会话分段上限设为 2 段，从下一条回复生效。".to_string(),
            PARTS[0].to_string(),
            format!("{}\n\n{}", PARTS[1], PARTS[2]),
        ]
    );
    let output = run(
        segmented(root.path(), &server.url),
        "/segment reset\n请分段\n",
    )
    .await;
    let lines = eve_lines(&output.stdout);
    assert!(lines[0].starts_with("已恢复本会话默认分段"));
    assert_eq!(lines[1..], PARTS);
    assert_eq!(
        segment_preferences(root.path()).unwrap()["scopes"],
        json!([{"scope": {"channel": "console", "session_id": "other", "user_id": "owner"},
            "enabled": null, "max_segments": 2, "pause_percent": null}])
    );
    // 设置命令不进入会话历史，也不请求模型。
    for _ in 0..4 {
        let body = server.next().await.body;
        assert!(user_texts(&body).iter().all(|t| !t.starts_with("/segment")));
    }
    assert!(server.requests.try_recv().is_err());
    let turns = &sessions(root.path())["sessions"]["default"]["turns"];
    assert_eq!(turns.as_array().unwrap().len(), 3);
}
#[tokio::test]
async fn segment_commands_without_flag_never_reach_the_model_or_state() {
    let root = fixture();
    let mut server = Server::start(vec![]).await;
    let output = run(
        command(root.path(), &server.url),
        "/segment off\n/segment parts 4\n",
    )
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        eve_lines(&output.stdout),
        ["分段显示未开启：以 --segmented 启动后可按会话设置。"; 2]
    );
    assert!(segment_preferences(root.path()).is_none());
    assert!(server.requests.try_recv().is_err());
}
#[tokio::test]
async fn corrupt_segment_settings_refuse_startup_and_keep_bytes() {
    let root = fixture();
    let mut server = Server::start(vec![Reply::json(final_response(SEGMENTED))]).await;
    assert!(
        run(segmented(root.path(), &server.url), "/segment off\n")
            .await
            .status
            .success()
    );
    let path = root.path().join("state/state.json");
    let mut outer: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    outer["entries"]["eve.segment.preferences"]["preferences.v1"] =
        json!(br#"{"version":1,"scopes":[{"scope":{"channel":"console","session_id":"default","user_id":"owner"},"enabled":null,"max_segments":null,"pause_percent":null}]}"#.to_vec());
    std::fs::write(&path, serde_json::to_vec(&outer).unwrap()).unwrap();
    let before = std::fs::read(&path).unwrap();
    let output = run(segmented(root.path(), &server.url), "请分段\n").await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("分段设置状态损坏；未清空"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(server.requests.try_recv().is_err());
}
