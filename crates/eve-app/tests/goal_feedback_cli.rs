#[path = "../../llm-openai/tests/support/mod.rs"]
mod http_support;
use http_support::{Reply, Server, final_response};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Child, Command, Output, Stdio},
    time::Duration,
};

fn command(root: &Path, url: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_eve-cognition"));
    cmd.arg("--state-dir")
        .arg(root.join("state"))
        .arg("--agent")
        .arg(root.join("AGENT.md"));
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(|name| name.starts_with("EVE_")) {
            cmd.env_remove(name);
        }
    }
    cmd.env("EVE_OPENAI_BASE_URL", url)
        .env("EVE_OPENAI_PROTOCOL", "responses")
        .env("EVE_OPENAI_MODEL", "feedback-fixture")
        .env("EVE_OPENAI_REASONING_EFFORT", "none");
    cmd
}
struct Process(Option<Child>);
impl Drop for Process {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
async fn run(mut command: Command) -> Output {
    let mut process = Process(Some(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    tokio::time::timeout(Duration::from_secs(15), async {
        while process.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    process.0.take().unwrap().wait_with_output().unwrap()
}
fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
async fn invoke(root: &Path, url: &str, args: &[&str]) -> Value {
    let mut cmd = command(root, url);
    cmd.args(args);
    success(run(cmd).await)
}
async fn execute(root: &Path, url: &str, expected_completed: usize) {
    let mut cmd = command(root, url);
    cmd.env("EVE_OPENAI_API_KEY", "feedback-synthetic-key")
        .args(["run", "--seconds", "30", "--max-executions", "1"]);
    let mut process = Process(Some(
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let outer: Value =
                serde_json::from_slice(&std::fs::read(root.join("state/state.json")).unwrap())
                    .unwrap();
            let bytes: Vec<u8> =
                serde_json::from_value(outer["entries"]["eve.cognition"]["cognition.v1"].clone())
                    .unwrap();
            let state: Value = serde_json::from_slice(&bytes).unwrap();
            let completed = state["state"]["goals"]
                .as_object()
                .unwrap()
                .values()
                .filter(|goal| {
                    goal["source"]["kind"] == "Inference"
                        && goal["source"]["reference"] == "goal"
                        && goal["status"] == "Completed"
                        && goal["feedback"]["verification_met"] == true
                })
                .count();
            if completed == expected_completed {
                break;
            }
            assert!(
                process.0.as_mut().unwrap().try_wait().unwrap().is_none(),
                "认知子进程在保存完成草稿前退出"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("认知子进程未在看门狗窗口内保存草稿");
    // 只在实际持久化终态之后停止本测试持有的 Child；随后使用新进程读取验证。
    process.0.as_mut().unwrap().kill().unwrap();
    let output = process.0.take().unwrap().wait_with_output().unwrap();
    for bytes in [&output.stdout, &output.stderr] {
        assert!(!String::from_utf8_lossy(bytes).contains("feedback-synthetic-key"));
    }
}
fn response(summary: &str) -> Reply {
    Reply::json(final_response(
        &json!({"summary":summary,"next_step":"按已提供约束整理下一步。","needs_user_input":false})
            .to_string(),
    ))
}

#[tokio::test]
async fn cli_feedback_is_offline_idempotent_and_requires_explicit_owner_and_revision() {
    let root = tempfile::tempdir().unwrap();
    let mut server = Server::start(vec![]).await;
    invoke(
        root.path(),
        &server.url,
        &["add", "--id", "goal", "--text", "写一份报告。"],
    )
    .await;
    let report = invoke(
        root.path(),
        &server.url,
        &[
            "feedback",
            "--id",
            "goal",
            "--revision",
            "1",
            "--feedback-id",
            "fact-a",
            "--text",
            "只允许一页。",
        ],
    )
    .await;
    assert_eq!(report["command"], "feedback");
    assert_eq!(report["goal_revision"], 2);
    assert_eq!(report["duplicate"], false);
    let disk = std::fs::read(root.path().join("state/state.json")).unwrap();
    let duplicate = invoke(
        root.path(),
        &server.url,
        &[
            "feedback",
            "--id",
            "goal",
            "--revision",
            "2",
            "--feedback-id",
            "fact-a",
            "--text",
            "只允许一页。",
        ],
    )
    .await;
    assert_eq!(duplicate["duplicate"], true);
    assert_eq!(
        std::fs::read(root.path().join("state/state.json")).unwrap(),
        disk
    );
    for args in [
        vec![
            "feedback",
            "--id",
            "goal",
            "--feedback-id",
            "fact-b",
            "--text",
            "缺修订",
        ],
        vec![
            "feedback",
            "--id",
            "goal",
            "--revision",
            "1",
            "--feedback-id",
            "fact-b",
            "--text",
            "旧修订",
        ],
        vec![
            "feedback",
            "--id",
            "goal",
            "--revision",
            "2",
            "--feedback-id",
            "fact-b",
            "--text",
            "不同用户",
            "--user",
            "bob",
        ],
        vec![
            "feedback",
            "--id",
            "goal",
            "--revision",
            "2",
            "--feedback-id",
            "fact-a",
            "--text",
            "冲突正文",
        ],
    ] {
        let mut cmd = command(root.path(), &server.url);
        cmd.args(args);
        assert!(!run(cmd).await.status.success());
    }
    assert_eq!(
        std::fs::read(root.path().join("state/state.json")).unwrap(),
        disk
    );
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn cli_marks_old_draft_stale_and_replans_current_revision_from_saved_user_fact() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("AGENT.md"), "你是 Eve，分清事实与建议。").unwrap();
    let mut server = Server::start(vec![
        response("旧稿：建议两页。"),
        response("新稿：按用户反馈整理为一页。"),
    ])
    .await;
    invoke(
        root.path(),
        &server.url,
        &["add", "--id", "goal", "--text", "整理报告。"],
    )
    .await;
    execute(root.path(), &server.url, 1).await;
    let first_request = server.next().await;
    assert!(
        first_request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer feedback-synthetic-key")
    );
    assert!(first_request.body["tools"].as_array().unwrap().is_empty());
    let shown = invoke(root.path(), &server.url, &["show", "--id", "goal"]).await;
    assert_eq!(shown["reflections"][0]["current"], true);
    assert_eq!(shown["reflections"][0]["stale"], false);
    let original_child = shown["reflections"][0]["goal"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    invoke(
        root.path(),
        &server.url,
        &[
            "feedback",
            "--id",
            "goal",
            "--revision",
            "1",
            "--feedback-id",
            "fact-a",
            "--text",
            "实际只允许一页，保留预算结论。",
        ],
    )
    .await;
    let stale = invoke(root.path(), &server.url, &["show", "--id", "goal"]).await;
    assert_eq!(stale["reflections"][0]["current"], false);
    assert_eq!(stale["reflections"][0]["stale"], true);
    assert_eq!(
        stale["reflections"][0]["artifact"]["summary"],
        "旧稿：建议两页。"
    );
    execute(root.path(), &server.url, 2).await;
    let request = server.next().await.body;
    assert!(
        request
            .to_string()
            .contains("实际只允许一页，保留预算结论。")
    );
    assert!(request.to_string().contains("整理报告。"));
    assert!(request["tools"].as_array().unwrap().is_empty());
    let shown = invoke(root.path(), &server.url, &["show", "--id", "goal"]).await;
    let drafts = shown["reflections"].as_array().unwrap();
    assert_eq!(drafts.len(), 2);
    let current: Vec<_> = drafts
        .iter()
        .filter(|draft| draft["current"] == true)
        .collect();
    assert_eq!(current.len(), 1);
    assert_eq!(
        current[0]["artifact"]["summary"],
        "新稿：按用户反馈整理为一页。"
    );
    assert_eq!(shown["goal"]["status"], "Waiting");
    let old = invoke(root.path(), &server.url, &["show", "--id", &original_child]).await;
    assert_eq!(old["reflections"][0]["current"], false);
    let mut idle = command(root.path(), &server.url);
    idle.args(["run", "--seconds", "1"]);
    assert_eq!(success(run(idle).await)["loop"]["submitted"], 0);
    assert!(server.requests.try_recv().is_err());
}
