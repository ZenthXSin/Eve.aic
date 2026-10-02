#[path = "../../llm-openai/tests/support/mod.rs"]
mod http_support;
#[path = "../examples/support/model_check.rs"]
mod model_check;
use http_support::{Reply, Server, final_response};
use model_check::{CheckError, CheckOptions};
use serde_json::{Value, json};
use std::{path::Path, process::Command, time::Duration};

const MARKER: &str = "EVE_CORE_FIXTURE_731";
fn command(url: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_eve"));
    for name in [
        "EVE_OPENAI_API_KEY",
        "EVE_OPENAI_MODEL",
        "EVE_OPENAI_PROTOCOL",
        "EVE_OPENAI_BASE_URL",
        "EVE_OPENAI_REASONING_EFFORT",
        "EVE_OPENAI_TIMEOUT_SECONDS",
        "EVE_OPENAI_MAX_OUTPUT_TOKENS",
        "EVE_LLM_RESPONSE_MODE",
        "EVE_LLM_MAX_PARALLEL_TOOL_CALLS",
    ] {
        c.env_remove(name);
    }
    c.env("EVE_OPENAI_API_KEY", "fixture-secret")
        .env("EVE_OPENAI_MODEL", "fixture-model")
        .env("EVE_OPENAI_PROTOCOL", "responses")
        .env("EVE_OPENAI_BASE_URL", url)
        .env("EVE_OPENAI_REASONING_EFFORT", "none")
        .env("EVE_OPENAI_TIMEOUT_SECONDS", "2");
    c
}
fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("AGENT.md"), "你是 Eve。称呼用户主人。").unwrap();
    root
}
async fn check(root: &Path, url: &str) -> Result<Value, CheckError> {
    let root = root.to_path_buf();
    let url = url.to_string();
    tokio::task::spawn_blocking(move || {
        model_check::run(
            &CheckOptions {
                directory: root.join("check"),
                agent_path: root.join("AGENT.md"),
                request_timeout: Duration::from_secs(2),
            },
            MARKER,
            || command(&url),
        )
    })
    .await
    .unwrap()
}
fn tool() -> Reply {
    Reply::json(json!({"status":"completed","error":null,"output":[
        {"type":"function_call","call_id":"check-echo","name":"echo",
        "arguments":format!("{{\"text\":\"{MARKER}\"}}")}
    ]}))
}
fn report(root: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(root.join("check/acceptance.json")).unwrap()).unwrap()
}
fn sessions(root: &Path) -> Value {
    let outer: Value =
        serde_json::from_slice(&std::fs::read(root.join("check/state/state.json")).unwrap())
            .unwrap();
    let bytes: Vec<u8> = outer["entries"]["eve.session"]["sessions.v1"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u8)
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
async fn shared_check_verifies_two_processes_and_all_four_real_http_requests() {
    let root = fixture();
    let mut server = Server::start(vec![
        tool(),
        Reply::json(final_response(MARKER)),
        Reply::json(final_response(MARKER)),
        Reply::json(final_response(MARKER)),
    ])
    .await;
    let checked = check(root.path(), &server.url).await.unwrap();
    assert_eq!(checked["status"], "passed");
    assert_eq!(checked["completed_turns"], 3);
    assert_eq!(checked["revision_before"], 4);
    assert_eq!(checked["revision_after"], 6);
    assert_eq!(checked["restart_tool_calls"], 0);
    assert_eq!(report(root.path()), checked);
    let captured = server.next().await;
    assert!(captured.headers.starts_with("POST /v1/responses HTTP/1.1"));
    let first = captured.body;
    assert_eq!(user_texts(&first).len(), 1);
    assert!(user_texts(&first)[0].contains(MARKER));
    let returned = server.next().await.body;
    let output = returned["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["type"] == "function_call_output")
        .unwrap();
    assert_eq!(output["call_id"], "check-echo");
    assert_eq!(
        serde_json::from_str::<Value>(output["output"].as_str().unwrap()).unwrap(),
        json!({"echo":MARKER})
    );
    let second = server.next().await.body;
    let third = server.next().await.body;
    for (request, count) in [(&second, 2), (&third, 3)] {
        let users = user_texts(request);
        assert_eq!(users.len(), count);
        assert!(users[0].contains(MARKER));
        assert!(users[1..].iter().all(|t| !t.contains(MARKER)));
        assert_eq!(
            request["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["type"] == "function_call_output")
                .count(),
            1
        );
    }
    for request in [&returned, &second, &third] {
        assert_eq!(request["input"][0], first["input"][0]);
    }
    assert!(server.requests.try_recv().is_err());
    assert!(root.path().join("check/first.stdout").exists());
    assert!(root.path().join("check/restart.stdout").exists());
    assert!(!checked.to_string().contains(MARKER));
    assert!(!checked.to_string().contains("fixture-secret"));
    println!(
        "核心模型验收流程：真实 eve 子进程 2、HTTP 请求 4、echo 1、完成轮 3、revision 4→6；重启新轮零工具。"
    );
}
#[tokio::test]
async fn text_claiming_tool_success_does_not_pass_or_start_restart() {
    let root = fixture();
    let mut server = Server::start(vec![
        Reply::json(final_response(&format!("已执行 echo：{MARKER}"))),
        Reply::json(final_response(MARKER)),
    ])
    .await;
    let error = check(root.path(), &server.url).await.unwrap_err();
    assert_eq!(error.stage, "first_checkpoint");
    assert_eq!(error.code, "tool_round_trip");
    assert_eq!(report(root.path())["status"], "failed");
    server.next().await;
    server.next().await;
    assert!(server.requests.try_recv().is_err());
    assert!(!root.path().join("check/restart.stdout").exists());
    assert_eq!(
        sessions(root.path())["sessions"]["default"]["turns"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}
#[tokio::test]
async fn missing_second_reply_marker_preserves_state_and_stops_before_restart() {
    let root = fixture();
    let mut server = Server::start(vec![
        tool(),
        Reply::json(final_response(MARKER)),
        Reply::json(final_response("错误口令")),
    ])
    .await;
    let error = check(root.path(), &server.url).await.unwrap_err();
    assert_eq!(error.code, "reply_marker");
    assert_eq!(error.stage, "first_checkpoint");
    for _ in 0..3 {
        server.next().await;
    }
    assert!(server.requests.try_recv().is_err());
    assert!(!root.path().join("check/restart.stdout").exists());
    assert_eq!(sessions(root.path())["sessions"]["default"]["revision"], 4);
}
#[tokio::test]
async fn wrong_restart_reply_keeps_old_completed_prefix_and_records_failure() {
    let root = fixture();
    let mut server = Server::start(vec![
        tool(),
        Reply::json(final_response(MARKER)),
        Reply::json(final_response(MARKER)),
        Reply::json(final_response("未记住")),
    ])
    .await;
    let error = check(root.path(), &server.url).await.unwrap_err();
    assert_eq!(error.stage, "restart_checkpoint");
    assert_eq!(error.code, "reply_marker");
    for _ in 0..4 {
        server.next().await;
    }
    assert!(server.requests.try_recv().is_err());
    let state = sessions(root.path());
    let turns = &state["sessions"]["default"]["turns"];
    assert_eq!(turns.as_array().unwrap().len(), 3);
    for i in 0..2 {
        assert!(
            turns[i]["status"]["messages"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["text"]
                .as_str()
                .unwrap()
                .contains(MARKER)
        );
    }
    assert_eq!(report(root.path())["stage"], "restart_checkpoint");
}
#[tokio::test]
async fn http_failure_is_not_retried_and_raw_diagnostics_do_not_enter_report() {
    let root = fixture();
    let mut bad = Reply::json(json!({"error":"fixture-secret private body"}));
    bad.status = 500;
    let mut server = Server::start(vec![bad, Reply::json(final_response("后续输入"))]).await;
    let error = check(root.path(), &server.url).await.unwrap_err();
    assert_eq!(error.stage, "first_process");
    assert_eq!(error.code, "child_failed");
    assert!(error.to_string().contains("child_failed"));
    let first = server.next().await.body;
    let second = server.next().await.body;
    assert!(user_texts(&first)[0].contains(MARKER));
    assert!(!user_texts(&second)[0].contains(MARKER));
    assert!(server.requests.try_recv().is_err());
    let state = sessions(root.path());
    assert_eq!(
        state["sessions"]["default"]["turns"][0]["status"]["failure"]["code"],
        "Provider"
    );
    assert_eq!(
        state["sessions"]["default"]["turns"][1]["status"]["state"],
        "Completed"
    );
    let saved = report(root.path()).to_string();
    assert!(!saved.contains("fixture-secret"));
    assert!(!saved.contains(MARKER));
    assert!(!root.path().join("check/restart.stdout").exists());
}
#[tokio::test]
async fn watchdog_kills_stuck_process_and_preserves_pending_without_retry() {
    let root = fixture();
    let mut delayed = Reply::json(final_response(MARKER));
    delayed.body_delay = Duration::from_secs(60);
    let mut server = Server::start(vec![delayed]).await;
    let options = CheckOptions {
        directory: root.path().join("check"),
        agent_path: root.path().join("AGENT.md"),
        request_timeout: Duration::from_millis(1),
    };
    let url = server.url.clone();
    let attempt = tokio::task::spawn_blocking(move || {
        model_check::run(&options, MARKER, || {
            let mut c = command(&url);
            c.env("EVE_OPENAI_TIMEOUT_SECONDS", "60");
            c
        })
    });
    server.next().await;
    let error = tokio::time::timeout(Duration::from_secs(15), attempt)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, "deadline");
    assert_eq!(
        sessions(root.path())["sessions"]["default"]["turns"][0]["status"]["state"],
        "Pending"
    );
    assert!(server.requests.try_recv().is_err());
    assert_eq!(report(root.path())["code"], "deadline");
    assert!(!root.path().join("check/restart.stdout").exists());
}
#[tokio::test]
async fn existing_directory_is_untouched_and_never_launches_eve() {
    let root = fixture();
    let mut server = Server::start(vec![]).await;
    std::fs::create_dir(root.path().join("check")).unwrap();
    std::fs::write(
        root.path().join("check/acceptance.json"),
        b"existing record",
    )
    .unwrap();
    let error = check(root.path(), &server.url).await.unwrap_err();
    assert_eq!(error.code, "directory_exists");
    assert_eq!(
        std::fs::read(root.path().join("check/acceptance.json")).unwrap(),
        b"existing record"
    );
    assert!(!root.path().join("check/state").exists());
    assert!(server.requests.try_recv().is_err());
}

fn chat_user_texts(request: &Value) -> Vec<&str> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].as_str().unwrap())
        .collect()
}
#[tokio::test]
async fn chat_defaults_complete_tool_round_trip_and_restore_in_a_new_process() {
    let root = fixture();
    let chat_final = || {
        Reply::json(json!({"choices":[{"index":0,"finish_reason":"stop",
            "message":{"role":"assistant","content":MARKER}}]}))
    };
    let mut server = Server::start(vec![
        Reply::json(json!({"choices":[{"index":0,"finish_reason":"tool_calls",
        "message":{"role":"assistant","content":null,"tool_calls":[
            {"id":"check-echo","type":"function","function":{"name":"echo",
                "arguments":json!({"text":MARKER}).to_string()}}
        ]}}]})),
        chat_final(),
        chat_final(),
        chat_final(),
    ])
    .await;
    let options = CheckOptions {
        directory: root.path().join("check"),
        agent_path: root.path().join("AGENT.md"),
        request_timeout: Duration::from_secs(2),
    };
    let url = server.url.replace("/responses", "/chat/completions");
    let checked = tokio::task::spawn_blocking(move || {
        model_check::run(&options, MARKER, || {
            let mut c = command(&url);
            // 验证核心默认模型和协议，空参数在 Chat 下显式使用 none。
            c.env_remove("EVE_OPENAI_MODEL")
                .env_remove("EVE_OPENAI_PROTOCOL")
                .env_remove("EVE_OPENAI_REASONING_EFFORT");
            c
        })
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(checked["status"], "passed");
    assert_eq!(checked["completed_turns"], 3);
    assert_eq!(checked["first_process_tool_calls"], 1);
    assert_eq!(checked["restart_tool_calls"], 0);
    assert_eq!(checked["revision_before"], 4);
    assert_eq!(checked["revision_after"], 6);
    assert_eq!(checked["history_prefix_unchanged"], true);
    let initial = server.next().await;
    assert!(
        initial
            .headers
            .starts_with("POST /v1/chat/completions HTTP/1.1")
    );
    let first = initial.body;
    assert_eq!(first["model"], "deepseek-v4.1-flash");
    assert_eq!(first["tools"][0]["function"]["name"], "echo");
    assert!(first.get("input").is_none());
    assert_eq!(first["reasoning_effort"], "none");
    assert_eq!(first["max_tokens"], 2048);
    assert!(chat_user_texts(&first)[0].contains(MARKER));
    let returned = server.next().await.body;
    let output = returned["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap();
    assert_eq!(output["tool_call_id"], "check-echo");
    assert_eq!(
        serde_json::from_str::<Value>(output["content"].as_str().unwrap()).unwrap(),
        json!({"echo":MARKER})
    );
    let second = server.next().await.body;
    let third = server.next().await.body;
    for (request, count) in [(&second, 2), (&third, 3)] {
        let users = chat_user_texts(request);
        assert_eq!(users.len(), count);
        assert!(users[1..].iter().all(|text| !text.contains(MARKER)));
        assert_eq!(request["messages"][0], first["messages"][0]);
        assert_eq!(
            request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "tool")
                .count(),
            1
        );
        assert_eq!(request["model"], "deepseek-v4.1-flash");
    }
    assert!(server.requests.try_recv().is_err());
}
#[tokio::test]
async fn chat_incomplete_reply_never_executes_partial_tool_batch() {
    let root = fixture();
    let mut server = Server::start(vec![
        Reply::json(json!({"choices":[{"index":0,"finish_reason":"length",
        "message":{"role":"assistant","content":null,"tool_calls":[
            {"id":"check-echo","type":"function","function":{"name":"echo",
                "arguments":json!({"text":MARKER}).to_string()}}
        ]}}]})),
        Reply::json(json!({"choices":[{"index":0,"finish_reason":"stop",
            "message":{"role":"assistant","content":"后续输入"}}]})),
    ])
    .await;
    let options = CheckOptions {
        directory: root.path().join("check"),
        agent_path: root.path().join("AGENT.md"),
        request_timeout: Duration::from_secs(2),
    };
    let url = server.url.replace("/responses", "/chat/completions");
    let error = tokio::task::spawn_blocking(move || {
        model_check::run(&options, MARKER, || {
            let mut c = command(&url);
            c.env("EVE_OPENAI_PROTOCOL", "chat")
                .env_remove("EVE_OPENAI_REASONING_EFFORT");
            c
        })
    })
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.stage, "first_process");
    assert_eq!(error.code, "child_failed");
    server.next().await;
    let next = server.next().await.body;
    assert_eq!(
        next["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .count(),
        0
    );
    assert!(server.requests.try_recv().is_err());
    let state = sessions(root.path());
    assert_eq!(
        state["sessions"]["default"]["turns"][0]["status"]["failure"]["started_tools"],
        0
    );
    assert!(!root.path().join("check/restart.stdout").exists());
}
