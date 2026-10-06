#[path = "../../llm-openai/tests/support/mod.rs"]
mod http_support;

use http_support::{Reply, Server, final_response};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Child, Command, Output, Stdio},
    time::Duration,
};

const KEY: &str = "cognition-fixture-secret";
const PARENT: &str = "尚未执行的私人目标：设计一项能由用户验收的整理计划。";

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("AGENT.md"), "你是 Eve，先分清事实与建议。").unwrap();
    root
}

fn command(root: &Path, url: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_eve-cognition"));
    command
        .arg("--state-dir")
        .arg(root.join("state"))
        .arg("--agent")
        .arg(root.join("AGENT.md"));
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(|name| name.starts_with("EVE_")) {
            command.env_remove(name);
        }
    }
    command
        .env("EVE_OPENAI_API_KEY", KEY)
        .env("EVE_OPENAI_MODEL", "reflection-fixture")
        .env("EVE_OPENAI_PROTOCOL", "responses")
        .env("EVE_OPENAI_BASE_URL", url)
        .env("EVE_OPENAI_TIMEOUT_SECONDS", "30")
        .env("EVE_OPENAI_MAX_OUTPUT_TOKENS", "256")
        .env("EVE_OPENAI_REASONING_EFFORT", "none")
        .env("EVE_LLM_RESPONSE_MODE", "complete");
    command
}

struct Process(Option<Child>);
impl Process {
    fn start(mut command: Command) -> Self {
        Self(Some(
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        ))
    }

    async fn finish(mut self) -> Output {
        tokio::time::timeout(Duration::from_secs(15), async {
            while self.0.as_mut().unwrap().try_wait().unwrap().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("认知子进程未在有界窗口内退出");
        let output = self.0.take().unwrap().wait_with_output().unwrap();
        for bytes in [&output.stdout, &output.stderr] {
            assert!(!String::from_utf8_lossy(bytes).contains(KEY));
        }
        output
    }

    async fn stop_after_saved_goal(mut self, root: &Path, parent: &str, expected: &str) {
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                let snapshot = cognition(root);
                if snapshot["state"]["goals"]
                    .as_object()
                    .unwrap()
                    .values()
                    .any(|goal| {
                        goal["source"]["kind"] == "Inference"
                            && goal["source"]["reference"] == parent
                            && goal["status"] == expected
                            && goal["feedback"].is_object()
                    })
                {
                    break;
                }
                assert!(
                    self.0.as_mut().unwrap().try_wait().unwrap().is_none(),
                    "认知子进程在保存期望终态前退出：{snapshot}"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("认知子进程未保存期望终态");
        // 仅终止自己持有的 Child，且必须先观察到反馈已持久化。随后新进程
        // 验证终态与会话记录，避免把运行窗口到时导致的取消误当成校验失败。
        self.0.as_mut().unwrap().kill().unwrap();
        let output = self.0.take().unwrap().wait_with_output().unwrap();
        for bytes in [&output.stdout, &output.stderr] {
            assert!(!String::from_utf8_lossy(bytes).contains(KEY));
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

async fn run(command: Command) -> Output {
    Process::start(command).finish().await
}

fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("CLI 应只输出一个 JSON 报告")
}

async fn add(root: &Path, url: &str, id: &str, text: &str, user: &str) -> Value {
    let mut command = command(root, url);
    command
        .env_remove("EVE_OPENAI_API_KEY")
        .args(["add", "--id", id, "--text", text, "--user", user]);
    let report = success(run(command).await);
    assert_eq!(report["command"], "add");
    assert_eq!(report["goal_id"], id);
    report
}

async fn execute(root: &Path, url: &str, limit: &str) -> Value {
    let mut command = command(root, url);
    command.args(["run", "--seconds", "1", "--max-executions", limit]);
    success(run(command).await)
}

async fn show(root: &Path, url: &str, id: &str) -> Value {
    let mut command = command(root, url);
    command
        .env_remove("EVE_OPENAI_API_KEY")
        .args(["show", "--id", id]);
    success(run(command).await)
}

fn artifact() -> Value {
    json!({
        "summary": "已识别目标所缺的输入，当前只能形成准备建议。",
        "next_step": "请用户说明验收范围和约束，然后据此细化执行计划。",
        "needs_user_input": true
    })
}

fn reply() -> Reply {
    Reply::json(final_response(&artifact().to_string()))
}

fn assert_no_tools(request: &Value) {
    assert_eq!(request["model"], "reflection-fixture");
    assert_eq!(request["tools"], json!([]));
    assert_eq!(request["max_output_tokens"], 256);
    assert_eq!(request["stream"], false);
    assert!(
        request["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["type"] != "function_call_output")
    );
}

fn cognition(root: &Path) -> Value {
    let file: Value =
        serde_json::from_slice(&std::fs::read(root.join("state/state.json")).unwrap()).unwrap();
    let bytes: Vec<u8> =
        serde_json::from_value(file["entries"]["eve.cognition"]["cognition.v1"].clone()).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn assert_zero_tools(report: &Value) {
    assert_eq!(report["loop"]["admitted_tool_calls"], 0);
    assert_eq!(report["loop"]["started_tools"], 0);
}

#[tokio::test]
async fn add_run_restart_produce_one_durable_reflection_and_keep_parent_waiting() {
    let root = fixture();
    let mut server = Server::start(vec![reply()]).await;
    let added = add(root.path(), &server.url, "parent", PARENT, "owner").await;
    assert_eq!(added["goals"]["waiting"], 1);
    assert!(server.requests.try_recv().is_err());

    let first = execute(root.path(), &server.url, "1").await;
    assert_eq!(first["loop"]["submitted"], 1);
    assert_eq!(first["loop"]["completed"], 1);
    assert_eq!(first["loop"]["model_requests"], 1);
    assert_zero_tools(&first);
    let request = server.next().await;
    assert!(request.headers.starts_with("POST /v1/responses HTTP/1.1"));
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {KEY}"))
    );
    assert_no_tools(&request.body);
    assert!(request.body.to_string().contains(PARENT));

    let before_restart = show(root.path(), &server.url, "parent").await;
    assert_eq!(before_restart["goal"]["status"], "Waiting");
    assert_eq!(
        before_restart["goal"]["visibility"],
        json!({"User": "owner"})
    );
    assert!(before_restart["goal"]["feedback"].is_null());
    let reflections = before_restart["reflections"].as_array().unwrap();
    assert_eq!(reflections.len(), 1);
    let child = &reflections[0]["goal"];
    assert_eq!(child["status"], "Completed");
    assert_eq!(child["visibility"], before_restart["goal"]["visibility"]);
    assert_eq!(child["budget"]["max_model_requests"], 1);
    assert_eq!(child["budget"]["max_tool_calls"], 0);
    assert_eq!(child["budget"]["max_attempts"], 1);
    assert_eq!(child["feedback"]["verification_met"], true);
    assert_eq!(reflections[0]["artifact"], artifact());

    // 第二次 run 是新进程、关闭标准输入；相同父修订不能再次派生或重放模型。
    let second = execute(root.path(), &server.url, "1").await;
    assert_eq!(second["loop"]["submitted"], 0);
    assert_eq!(second["loop"]["model_requests"], 0);
    assert_zero_tools(&second);
    assert!(server.requests.try_recv().is_err());
    assert_eq!(
        show(root.path(), &server.url, "parent").await,
        before_restart
    );

    let mut status = command(root.path(), &server.url);
    status.env_remove("EVE_OPENAI_API_KEY").arg("status");
    let output = run(status).await;
    let public_output = String::from_utf8_lossy(&output.stdout);
    assert!(!public_output.contains(PARENT));
    assert!(!public_output.contains(artifact()["summary"].as_str().unwrap()));
    assert_eq!(success(output)["goals"]["waiting"], 1);
    assert!(!cognition(root.path()).to_string().contains(KEY));
}

#[tokio::test]
async fn empty_background_window_needs_no_credentials_and_never_calls_model() {
    let root = fixture();
    let mut server = Server::start(vec![]).await;
    let mut command = command(root.path(), &server.url);
    command.env_remove("EVE_OPENAI_API_KEY").args([
        "run",
        "--seconds",
        "1",
        "--max-executions",
        "1",
    ]);
    let report = success(run(command).await);
    assert_eq!(report["goals"]["total"], 0);
    assert_eq!(report["loop"]["model_requests"], 0);
    assert_eq!(report["loop"]["submitted"], 0);
    assert_zero_tools(&report);
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn derived_reflections_inherit_user_visibility_without_cross_user_context() {
    let root = fixture();
    let mut server = Server::start(vec![reply(), reply()]).await;
    let alice = "Alice 的私人目标标记 ALPHA_ONLY";
    let bob = "Bob 的私人目标标记 BETA_ONLY";
    add(root.path(), &server.url, "alice-goal", alice, "alice").await;
    add(root.path(), &server.url, "bob-goal", bob, "bob").await;
    let first = execute(root.path(), &server.url, "1").await;
    assert_eq!(first["loop"]["submitted"], 1);
    assert_eq!(first["loop"]["completed"], 1);
    assert_eq!(first["loop"]["model_requests"], 1);
    assert_zero_tools(&first);
    let first_request = server.next().await.body;
    assert!(server.requests.try_recv().is_err());
    // 同时有两个父目标时全局上限仍为一次；新进程继续第二个，不带入前一用户。
    let second = execute(root.path(), &server.url, "1").await;
    assert_eq!(second["loop"]["submitted"], 1);
    assert_eq!(second["loop"]["completed"], 1);
    assert_eq!(second["loop"]["model_requests"], 1);
    assert_zero_tools(&second);
    let requests = [first_request, server.next().await.body];
    for request in &requests {
        assert_no_tools(request);
        let content = request.to_string();
        assert_ne!(
            content.contains("ALPHA_ONLY"),
            content.contains("BETA_ONLY")
        );
    }
    assert!(
        requests
            .iter()
            .any(|r| r.to_string().contains("ALPHA_ONLY"))
    );
    assert!(requests.iter().any(|r| r.to_string().contains("BETA_ONLY")));
    for (id, user) in [("alice-goal", "alice"), ("bob-goal", "bob")] {
        let view = show(root.path(), &server.url, id).await;
        assert_eq!(view["goal"]["status"], "Waiting");
        assert_eq!(view["goal"]["visibility"], json!({"User": user}));
        assert_eq!(view["reflections"].as_array().unwrap().len(), 1);
        assert_eq!(
            view["reflections"][0]["goal"]["visibility"],
            json!({"User": user})
        );
        assert_eq!(view["reflections"][0]["artifact"], artifact());
    }
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn malformed_or_empty_artifacts_block_child_and_are_not_retried_after_restart() {
    // 验证结构质量；不把合法 JSON 当作建议真实性或现实目标完成的证据。
    for (index, text) in [
        "这段不是 JSON",
        r#"{"summary":" ","next_step":"","needs_user_input":true}"#,
        r#"{"summary":"摘要","summary":"覆盖","next_step":"询问条件","needs_user_input":true}"#,
        r#"{"summary":"摘要","next_step":"询问条件","needs_user_input":true,"execute":true}"#,
    ]
    .into_iter()
    .enumerate()
    {
        let root = fixture();
        let mut response = Reply::json(final_response(text));
        if index == 0 {
            // 回归触发条件：响应晚于原测试的 1 秒运行窗口；完成门必须是
            // 已保存 Blocked，不能依赖机器能否在 1 秒内完成 HTTP 与验证。
            response.body_delay = Duration::from_millis(1500);
        }
        let mut server = Server::start(vec![response]).await;
        add(root.path(), &server.url, "invalid-parent", PARENT, "owner").await;
        let mut command = command(root.path(), &server.url);
        command.args(["run", "--seconds", "30", "--max-executions", "1"]);
        let process = Process::start(command);
        assert_no_tools(&server.next().await.body);
        process
            .stop_after_saved_goal(root.path(), "invalid-parent", "Blocked")
            .await;
        assert!(server.requests.try_recv().is_err());
        let view = show(root.path(), &server.url, "invalid-parent").await;
        assert_eq!(view["goal"]["status"], "Waiting");
        assert_eq!(view["reflections"].as_array().unwrap().len(), 1);
        assert_eq!(view["reflections"][0]["goal"]["status"], "Blocked");
        assert_eq!(
            view["reflections"][0]["goal"]["block_reason"],
            "Invalidated"
        );
        assert_eq!(
            view["reflections"][0]["goal"]["feedback"]["commit"],
            "Completed"
        );
        assert_eq!(
            view["reflections"][0]["goal"]["feedback"]["verification_met"],
            false
        );
        assert_eq!(
            view["reflections"][0]["goal"]["feedback"]["started_tools"],
            0
        );
        assert!(view["reflections"][0]["artifact"].is_null());
        let restarted = execute(root.path(), &server.url, "1").await;
        assert_eq!(restarted["loop"]["submitted"], 0);
        assert_eq!(restarted["loop"]["model_requests"], 0);
        assert!(server.requests.try_recv().is_err());
        assert_eq!(show(root.path(), &server.url, "invalid-parent").await, view);
    }
}

#[tokio::test]
async fn unsolicited_tool_call_is_never_executed_or_followed_by_another_model_request() {
    let root = fixture();
    let mut server = Server::start(vec![Reply::json(json!({
        "status": "completed", "error": null, "output": [{
            "type": "function_call", "call_id": "must-not-run", "name": "echo",
            "arguments": "{\"text\":\"不可执行的模型工具要求\"}"
        }]
    }))])
    .await;
    add(root.path(), &server.url, "tool-parent", PARENT, "owner").await;
    let report = execute(root.path(), &server.url, "1").await;
    assert_eq!(report["loop"]["model_requests"], 1);
    assert_eq!(report["loop"]["completed"], 0);
    assert_eq!(report["loop"]["blocked"], 1);
    assert_zero_tools(&report);
    assert_no_tools(&server.next().await.body);
    let view = show(root.path(), &server.url, "tool-parent").await;
    assert_eq!(view["goal"]["status"], "Waiting");
    assert_eq!(view["reflections"][0]["goal"]["status"], "Blocked");
    assert!(view["reflections"][0]["artifact"].is_null());
    let recovered = execute(root.path(), &server.url, "1").await;
    assert_eq!(recovered["loop"]["model_requests"], 0);
    assert!(server.requests.try_recv().is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_during_http_wait_settles_child_and_restart_never_replays() {
    let root = fixture();
    let mut delayed = reply();
    delayed.body_delay = Duration::from_secs(30);
    let mut server = Server::start(vec![delayed]).await;
    add(root.path(), &server.url, "cancel-parent", PARENT, "owner").await;
    let mut command = command(root.path(), &server.url);
    command.args(["run", "--seconds", "30", "--max-executions", "1"]);
    let process = Process::start(command);
    // 到达真实 HTTP 表明子目标已准入并保存；以此为门，不猜启动所需时间。
    assert_no_tools(&server.next().await.body);
    let pid = process.0.as_ref().unwrap().id();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    let report = success(process.finish().await);
    assert_eq!(report["interrupted"], true);
    assert_eq!(report["loop"]["model_requests"], 1);
    assert_zero_tools(&report);
    let view = show(root.path(), &server.url, "cancel-parent").await;
    assert_eq!(view["goal"]["status"], "Waiting");
    assert_eq!(view["reflections"].as_array().unwrap().len(), 1);
    assert!(matches!(
        view["reflections"][0]["goal"]["status"].as_str(),
        Some("Cancelled" | "Blocked")
    ));
    assert!(view["reflections"][0]["artifact"].is_null());
    let recovered = execute(root.path(), &server.url, "1").await;
    assert_eq!(recovered["loop"]["submitted"], 0);
    assert_eq!(recovered["loop"]["model_requests"], 0);
    assert!(server.requests.try_recv().is_err());
    assert_eq!(show(root.path(), &server.url, "cancel-parent").await, view);
}

#[tokio::test]
async fn corrupt_cognition_schema_is_rejected_without_overwriting_original_file() {
    let root = fixture();
    let mut server = Server::start(vec![]).await;
    add(
        root.path(),
        &server.url,
        "preserved-parent",
        PARENT,
        "owner",
    )
    .await;
    let target = root.path().join("state/state.json");
    let mut outer: Value = serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
    let mut inner = cognition(root.path());
    inner["format_version"] = json!(999);
    outer["entries"]["eve.cognition"]["cognition.v1"] =
        serde_json::to_value(serde_json::to_vec(&inner).unwrap()).unwrap();
    let preserved = serde_json::to_vec(&outer).unwrap();
    std::fs::write(&target, &preserved).unwrap();
    for args in [
        vec!["status"],
        vec!["show", "--id", "preserved-parent"],
        vec!["run", "--seconds", "1"],
    ] {
        let mut command = command(root.path(), &server.url);
        command.args(args);
        assert!(!run(command).await.status.success());
        assert_eq!(std::fs::read(&target).unwrap(), preserved);
    }
    assert!(server.requests.try_recv().is_err());
}
