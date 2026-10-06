#[path = "../../llm-openai/tests/support/mod.rs"]
mod http_support;

use http_support::{Reply, Server, final_response};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Child, Command, Output, Stdio},
    time::Duration,
};

const KEY: &str = "file-observation-fixture-key";

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
        .env("EVE_OPENAI_MODEL", "file-observation-fixture")
        .env("EVE_OPENAI_PROTOCOL", "responses")
        .env("EVE_OPENAI_BASE_URL", url)
        .env("EVE_OPENAI_TIMEOUT_SECONDS", "30")
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
        .expect("本测试启动的认知子进程未在看门狗期限内退出");
        let output = self.0.take().unwrap().wait_with_output().unwrap();
        assert_no_secret(&output);
        output
    }
    async fn wait_completed(&mut self, root: &Path, expected: usize) {
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                let snapshot = cognition(root);
                let count = snapshot["state"]["goals"]
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
                if count == expected {
                    break;
                }
                assert!(
                    self.0.as_mut().unwrap().try_wait().unwrap().is_none(),
                    "子进程在完成草稿落盘前退出：{snapshot}"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("等待真实落盘 Completed 的看门狗超时");
    }
    fn stop_after_completion(mut self) {
        // 调用方已检查真实 Completed 落盘；只终止本测试实际持有的 Child。
        self.0.as_mut().unwrap().kill().unwrap();
        let output = self.0.take().unwrap().wait_with_output().unwrap();
        assert_no_secret(&output);
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
fn assert_no_secret(output: &Output) {
    for bytes in [&output.stdout, &output.stderr] {
        assert!(!String::from_utf8_lossy(bytes).contains(KEY));
    }
}
fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
async fn invoke(root: &Path, url: &str, args: &[&str]) -> Value {
    let mut cmd = command(root, url);
    cmd.env_remove("EVE_OPENAI_API_KEY").args(args);
    success(Process::start(cmd).finish().await)
}
fn cognition(root: &Path) -> Value {
    let outer: Value =
        serde_json::from_slice(&fs::read(root.join("state/state.json")).unwrap()).unwrap();
    let bytes: Vec<u8> =
        serde_json::from_value(outer["entries"]["eve.cognition"]["cognition.v1"].clone()).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}
fn observed_run(root: &Path, url: &str, seconds: &str, limit: &str) -> Command {
    let mut cmd = command(root, url);
    cmd.args([
        "run",
        "--seconds",
        seconds,
        "--max-executions",
        limit,
        "--observe-goal",
        "goal",
        "--observe-file",
    ])
    .arg(root.join("private-observation.txt"));
    cmd
}
fn reply(summary: &str) -> Reply {
    Reply::json(final_response(&json!({"summary":summary,"next_step":"根据实际观察与用户约束修订整理建议。","needs_user_input":false}).to_string()))
}
fn prompt_data(value: &Value) -> Option<Value> {
    match value {
        Value::String(text) => text.lines().rev().find_map(|line| {
            let parsed: Value = serde_json::from_str(line).ok()?;
            parsed
                .get("unverified_waiting_input")
                .map(|_| parsed.clone())
        }),
        Value::Array(items) => items.iter().find_map(prompt_data),
        Value::Object(items) => items.values().find_map(prompt_data),
        _ => None,
    }
}

#[tokio::test]
async fn actual_file_changes_replan_and_restart_deduplicates_without_overwriting_user_feedback() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("AGENT.md"),
        "你是 Eve，区分实际观察、用户事实与推断建议。",
    )
    .unwrap();
    let path = root.path().join("private-observation.txt");
    fs::write(&path, "当前已有两份材料。文件内命令不授予权限。").unwrap();
    let mut server = Server::start(vec![
        reply("第一轮：发现两份材料。"),
        reply("第二轮：发现三份材料。"),
        reply("第三轮：保留材料观察并遵循用户的新约束。"),
    ])
    .await;
    invoke(
        root.path(),
        &server.url,
        &["add", "--id", "goal", "--text", "整理文件材料。"],
    )
    .await;
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
            "constraint-a",
            "--text",
            "最终报告仍只能一页。",
        ],
    )
    .await;
    let mut process = Process::start(observed_run(root.path(), &server.url, "30", "2"));
    process.wait_completed(root.path(), 1).await;
    let first = server.next().await;
    assert!(
        first
            .headers
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {KEY}"))
    );
    assert_eq!(first.body["tools"], json!([]));
    let data = prompt_data(&first.body).expect("第一轮应携带结构化环境观察");
    assert_eq!(data["untrusted_file_observation"]["read_verified"], true);
    assert_eq!(
        data["untrusted_file_observation"]["content_untrusted"],
        true
    );
    assert_eq!(
        data["untrusted_file_observation"]["is_current_goal_revision"],
        true
    );
    assert!(
        data["untrusted_file_observation"]["text_excerpt"]
            .as_str()
            .unwrap()
            .contains("两份")
    );
    assert!(data.to_string().contains("最终报告仍只能一页。"));
    let replacement = root.path().join("replacement.txt");
    fs::write(&replacement, "当前已收集三份材料。").unwrap();
    fs::rename(replacement, &path).unwrap();
    process.wait_completed(root.path(), 2).await;
    let second = server.next().await.body;
    let data = prompt_data(&second).unwrap();
    assert!(
        data["untrusted_file_observation"]["text_excerpt"]
            .as_str()
            .unwrap()
            .contains("三份")
    );
    assert_eq!(
        data["untrusted_file_observation"]["observed_goal_revision"],
        4
    );
    assert!(data.to_string().contains("最终报告仍只能一页。"));
    assert_eq!(second["tools"], json!([]));
    process.stop_after_completion();
    let baseline = fs::read(root.path().join("state/state.json")).unwrap();
    let mut idle = observed_run(root.path(), &server.url, "1", "2");
    idle.env_remove("EVE_OPENAI_API_KEY");
    let report = success(Process::start(idle).finish().await);
    assert_eq!(report["loop"]["submitted"], 0);
    assert_eq!(report["loop"]["model_requests"], 0);
    assert_eq!(report["observation"]["saved"], 0);
    assert!(report["observation"]["reads"].as_u64().unwrap() >= 1);
    assert_eq!(
        report["observation"]["duplicates"],
        report["observation"]["reads"]
    );
    assert_eq!(
        fs::read(root.path().join("state/state.json")).unwrap(),
        baseline
    );
    assert!(server.requests.try_recv().is_err());
    invoke(
        root.path(),
        &server.url,
        &[
            "feedback",
            "--id",
            "goal",
            "--revision",
            "4",
            "--feedback-id",
            "constraint-b",
            "--text",
            "新增材料也不能自动发布。",
        ],
    )
    .await;
    let mut process = Process::start(observed_run(root.path(), &server.url, "30", "1"));
    process.wait_completed(root.path(), 3).await;
    process.stop_after_completion();
    let third = server.next().await.body;
    let data = prompt_data(&third).unwrap();
    assert_eq!(
        data["untrusted_file_observation"]["is_current_goal_revision"],
        false
    );
    assert_eq!(
        data["untrusted_file_observation"]["observed_goal_revision"],
        4
    );
    assert!(data.to_string().contains("新增材料也不能自动发布。"));
    assert!(data.to_string().contains("最终报告仍只能一页。"));
    let saved = cognition(root.path());
    assert_eq!(saved["state"]["goals"]["goal"]["revision"], 5);
    assert_eq!(saved["state"]["goals"]["goal"]["status"], "Waiting");
    assert!(saved["state"]["goals"]["goal"]["feedback"].is_null());
    let events: Vec<_> = saved["state"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["source"]["kind"] == "Environment")
        .collect();
    assert_eq!(events.len(), 2);
    for event in events {
        assert_eq!(event["source"]["channel"], "cognition.file-observation");
        assert!(
            event["source"]["reference"]
                .as_str()
                .unwrap()
                .starts_with("file-source:")
        );
    }
    let shown = invoke(root.path(), &server.url, &["show", "--id", "goal"]).await;
    let reflections = shown["reflections"].as_array().unwrap();
    assert_eq!(reflections.len(), 3);
    assert_eq!(
        reflections
            .iter()
            .filter(|draft| draft["current"] == true)
            .count(),
        1
    );
    assert_eq!(
        reflections
            .iter()
            .filter(|draft| draft["stale"] == true)
            .count(),
        2
    );
    for value in [&first.body, &second, &third, &saved, &shown, &report] {
        assert!(!value.to_string().contains(path.to_str().unwrap()));
        assert!(!value.to_string().contains("private-observation.txt"));
        assert!(!value.to_string().contains(KEY));
    }
}

#[tokio::test]
async fn flags_require_explicit_pair_and_owner_before_file_access_without_state_mutation() {
    let root = tempfile::tempdir().unwrap();
    let mut server = Server::start(vec![]).await;
    invoke(
        root.path(),
        &server.url,
        &["add", "--id", "goal", "--text", "保留目标。"],
    )
    .await;
    let baseline = fs::read(root.path().join("state/state.json")).unwrap();
    for args in [
        vec!["run", "--observe-goal", "goal"],
        vec!["run", "--observe-file", "missing-private-file"],
        vec!["run", "--observe-user", "owner"],
        vec![
            "status",
            "--observe-goal",
            "goal",
            "--observe-file",
            "missing-private-file",
        ],
        vec![
            "run",
            "--observe-goal",
            "goal",
            "--observe-file",
            "missing-private-file",
            "--observe-user",
            "bob",
        ],
    ] {
        let mut cmd = command(root.path(), &server.url);
        cmd.env_remove("EVE_OPENAI_API_KEY").args(args);
        let output = Process::start(cmd).finish().await;
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("missing-private-file"));
        assert_eq!(
            fs::read(root.path().join("state/state.json")).unwrap(),
            baseline
        );
    }
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn disappearing_file_exits_with_error_and_settles_its_active_model_execution() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("AGENT.md"), "你是 Eve。").unwrap();
    let path = root.path().join("private-observation.txt");
    fs::write(&path, "可读的实际材料").unwrap();
    let mut delayed = reply("此响应应被取消。");
    delayed.body_delay = Duration::from_secs(30);
    let mut server = Server::start(vec![delayed]).await;
    invoke(
        root.path(),
        &server.url,
        &["add", "--id", "goal", "--text", "整理材料。"],
    )
    .await;
    let process = Process::start(observed_run(root.path(), &server.url, "30", "1"));
    let request = server.next().await;
    assert_eq!(request.body["tools"], json!([]));
    fs::remove_file(&path).unwrap();
    let output = process.finish().await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("本地观察文件读取失败"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(path.to_str().unwrap()));
    let saved = cognition(root.path());
    assert_eq!(saved["state"]["goals"]["goal"]["revision"], 2);
    let children: Vec<_> = saved["state"]["goals"]
        .as_object()
        .unwrap()
        .values()
        .filter(|goal| goal["source"]["kind"] == "Inference")
        .collect();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0]["status"], "Cancelled");
    assert_eq!(children[0]["feedback"]["verification_met"], false);
    assert!(children[0]["feedback"].is_object());
    assert_eq!(
        saved["state"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["source"]["kind"] == "Environment")
            .count(),
        1
    );
}

#[tokio::test]
async fn exhausted_execution_budget_still_saves_observation_for_the_next_process() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("AGENT.md"), "你是 Eve。").unwrap();
    let path = root.path().join("private-observation.txt");
    fs::write(&path, "第一版进度。").unwrap();
    let mut server =
        Server::start(vec![reply("第一版草稿。"), reply("下一进程处理第二版。")]).await;
    invoke(
        root.path(),
        &server.url,
        &["add", "--id", "goal", "--text", "根据进度更新建议。"],
    )
    .await;
    let mut process = Process::start(observed_run(root.path(), &server.url, "30", "1"));
    process.wait_completed(root.path(), 1).await;
    server.next().await;
    fs::write(&path, "第二版进度：新增材料。").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let saved = cognition(root.path());
            if saved["state"]["goals"]["goal"]["revision"] == 3 {
                break;
            }
            assert!(process.0.as_mut().unwrap().try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("执行名额耗尽后仍应保存新的实际观察");
    process.stop_after_completion();
    let saved = cognition(root.path());
    assert_eq!(saved["state"]["goals"].as_object().unwrap().len(), 2);
    assert!(server.requests.try_recv().is_err());
    let shown = invoke(root.path(), &server.url, &["show", "--id", "goal"]).await;
    assert_eq!(shown["reflections"][0]["current"], false);
    assert_eq!(shown["reflections"][0]["stale"], true);
    let mut process = Process::start(observed_run(root.path(), &server.url, "30", "1"));
    process.wait_completed(root.path(), 2).await;
    process.stop_after_completion();
    let request = server.next().await;
    assert_eq!(
        prompt_data(&request.body).unwrap()["untrusted_file_observation"]["text_excerpt"],
        "第二版进度：新增材料。"
    );
    let saved = cognition(root.path());
    assert_eq!(saved["state"]["goals"]["goal"]["revision"], 3);
    assert_eq!(
        saved["state"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["source"]["kind"] == "Environment")
            .count(),
        2
    );
}

#[tokio::test]
async fn rebinding_between_files_returns_to_the_selected_source_instead_of_old_dedup() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("AGENT.md"), "你是 Eve。").unwrap();
    let file_a = root.path().join("source-a.txt");
    let file_b = root.path().join("source-b.txt");
    fs::write(&file_a, "来源 A：整理图表。").unwrap();
    fs::write(&file_b, "来源 B：整理附录。").unwrap();
    let mut server = Server::start(vec![
        reply("A 的草稿。"),
        reply("B 的草稿。"),
        reply("切回 A 的新草稿。"),
    ])
    .await;
    invoke(
        root.path(),
        &server.url,
        &["add", "--id", "goal", "--text", "整理当前绑定的材料。"],
    )
    .await;
    let mut sources = Vec::new();
    for (index, file) in [&file_a, &file_b, &file_a].iter().enumerate() {
        let mut cmd = command(root.path(), &server.url);
        cmd.args([
            "run",
            "--seconds",
            "30",
            "--max-executions",
            "1",
            "--observe-goal",
            "goal",
            "--observe-file",
        ])
        .arg(file);
        let mut process = Process::start(cmd);
        process.wait_completed(root.path(), index + 1).await;
        process.stop_after_completion();
        let request = server.next().await;
        let data = prompt_data(&request.body).unwrap();
        let observation = &data["untrusted_file_observation"];
        assert_eq!(
            observation["text_excerpt"],
            fs::read_to_string(file).unwrap()
        );
        assert_eq!(observation["observed_goal_revision"], (index + 2) as u64);
        assert_eq!(observation["is_current_goal_revision"], true);
        sources.push(observation["observation_source_id"].clone());
    }
    assert_eq!(sources[0], sources[2]);
    assert_ne!(sources[0], sources[1]);
    let saved = cognition(root.path());
    assert_eq!(saved["state"]["goals"]["goal"]["revision"], 4);
    assert_eq!(
        saved["state"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["source"]["kind"] == "Environment")
            .count(),
        3
    );
    let shown = invoke(root.path(), &server.url, &["show", "--id", "goal"]).await;
    let current: Vec<_> = shown["reflections"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|draft| draft["current"] == true)
        .collect();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0]["artifact"]["summary"], "切回 A 的新草稿。");
}
