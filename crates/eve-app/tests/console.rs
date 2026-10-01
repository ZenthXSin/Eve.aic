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
        "EVE_OPENAI_BASE_URL",
        "EVE_OPENAI_REASONING_EFFORT",
        "EVE_OPENAI_TIMEOUT_SECONDS",
        "EVE_OPENAI_MAX_OUTPUT_TOKENS",
        "EVE_LLM_RESPONSE_MODE",
        "EVE_LLM_MAX_PARALLEL_TOOL_CALLS",
    ] {
        command.env_remove(name);
    }
    command
        .env("EVE_OPENAI_API_KEY", "fixture-key")
        .env("EVE_OPENAI_MODEL", "fixture-model")
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
            stdin.write_all(&input).unwrap();
            drop(stdin);
            child.wait_with_output().unwrap()
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
        "请回显核心回执\n第二轮\n/quit\n",
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
    assert!(
        !std::fs::read_to_string(root.path().join("state/configuration/config.json"))
            .unwrap()
            .contains("fixture-key")
    );
    println!("Eve 核心进程验收：两轮聊天、一次工具往返、重启第三轮；历史修订 4→6，旧工具零重放。");
}
#[tokio::test]
async fn provider_failure_is_not_retried_or_added_to_completed_history() {
    let root = fixture();
    let mut failed = Reply::json(json!({"error":"fixture-key 不应外泄"}));
    failed.status = 500;
    let mut server = Server::start(vec![failed, Reply::json(final_response("下一轮成功"))]).await;
    let output = run(
        command(root.path(), &server.url),
        "失败输入\n新输入\n/quit\n",
    )
    .await;
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
    let text = format!(
        "{}\n/help\n\n有效输入\n/quit\n不应发送\n",
        "长".repeat(12000)
    );
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
