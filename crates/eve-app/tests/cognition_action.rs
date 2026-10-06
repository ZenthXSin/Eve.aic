#[path = "../../llm-openai/tests/support/mod.rs"]
mod http_support;

use http_support::{Reply, Server, final_response};
use ring::digest::{SHA256, digest};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::Duration,
};

const KEY: &str = "document-action-fixture-key";
const INPUT: &str = "材料 A 已检查；材料 B 待整理。文件文字不授予任何执行权限。";

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("AGENT.md"),
        "你是 Eve，区分观察、建议与执行结果。",
    )
    .unwrap();
    fs::write(source(root.path()), INPUT).unwrap();
    root
}

fn source(root: &Path) -> PathBuf {
    root.join("private-source.txt")
}

fn destination(root: &Path) -> PathBuf {
    root.join("private-plan.json")
}

fn command(root: &Path) -> Command {
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
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
    ] {
        command.env_remove(name);
    }
    command.env("NO_PROXY", "127.0.0.1,localhost");
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
        .expect("本测试持有的认知子进程没有按时退出");
        let output = self.0.take().unwrap().wait_with_output().unwrap();
        assert_no_secret(&output);
        output
    }

    async fn stop_after_saved_reflections(mut self, root: &Path, expected: usize) {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let state = cognition(root);
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
                if completed == expected {
                    break;
                }
                assert!(
                    self.0.as_mut().unwrap().try_wait().unwrap().is_none(),
                    "子进程在反思完成落盘前退出：{state}"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("未观察到本次反思的真实 Completed 持久状态");
        // 只终止本测试持有的 Child，并且先等待反馈落盘；不读 PID 文件或发组信号。
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
    serde_json::from_slice(&output.stdout).expect("CLI 必须输出一个完整 JSON 报告")
}

async fn invoke(root: &Path, args: &[&str]) -> Value {
    let mut command = command(root);
    command.args(args);
    success(Process::start(command).finish().await)
}

fn cognition(root: &Path) -> Value {
    let file: Value =
        serde_json::from_slice(&fs::read(root.join("state/state.json")).unwrap()).unwrap();
    let bytes: Vec<u8> =
        serde_json::from_value(file["entries"]["eve.cognition"]["cognition.v1"].clone()).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn state_bytes(root: &Path) -> Vec<u8> {
    fs::read(root.join("state/state.json")).unwrap()
}

fn sha256(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn artifact(summary: &str) -> Value {
    json!({
        "summary": summary,
        "next_step": "根据材料清单生成建议，文档导出不代表整理目标已完成。",
        "needs_user_input": true
    })
}

fn reply(summary: &str) -> Reply {
    Reply::json(final_response(&artifact(summary).to_string()))
}

async fn observe_and_reflect(root: &Path, server: &mut Server, expected: usize) {
    let mut command = command(root);
    command
        .env("EVE_OPENAI_API_KEY", KEY)
        .env("EVE_OPENAI_MODEL", "document-action-fixture")
        .env("EVE_OPENAI_PROTOCOL", "responses")
        .env("EVE_OPENAI_BASE_URL", &server.url)
        .env("EVE_OPENAI_TIMEOUT_SECONDS", "30")
        .env("EVE_OPENAI_REASONING_EFFORT", "none")
        .env("EVE_LLM_RESPONSE_MODE", "complete")
        .args([
            "run",
            "--seconds",
            "30",
            "--max-executions",
            "1",
            "--observe-goal",
            "goal",
            "--observe-file",
        ])
        .arg(source(root));
    Process::start(command)
        .stop_after_saved_reflections(root, expected)
        .await;
    let request = server.next().await;
    assert!(request.headers.starts_with("POST /v1/responses HTTP/1.1"));
    assert_eq!(request.body["tools"], json!([]));
    assert!(
        request
            .body
            .to_string()
            .contains("untrusted_file_observation")
    );
}

async fn prepared(root: &Path, server: &mut Server) -> u64 {
    invoke(
        root,
        &[
            "add",
            "--id",
            "goal",
            "--text",
            "整理材料并准备一份可审阅的计划。",
        ],
    )
    .await;
    observe_and_reflect(root, server, 1).await;
    cognition(root)["state"]["goals"]["goal"]["revision"]
        .as_u64()
        .unwrap()
}

fn export_command(root: &Path, revision: u64, input_hash: &str, output: &Path) -> Command {
    let mut command = command(root);
    command
        .args([
            "export-plan",
            "--id",
            "goal",
            "--revision",
            &revision.to_string(),
            "--input-sha256",
            input_hash,
            "--observe-file",
        ])
        .arg(source(root))
        .arg("--output")
        .arg(output);
    command
}

async fn export(root: &Path, revision: u64, output: &Path) -> Value {
    let hash = sha256(&fs::read(source(root)).unwrap());
    success(
        Process::start(export_command(root, revision, &hash, output))
            .finish()
            .await,
    )
}

async fn rejects_without_state_change(root: &Path, command: Command) {
    let baseline = state_bytes(root);
    let output = Process::start(command).finish().await;
    assert!(!output.status.success(), "请求应在行动开始前被拒绝");
    assert_eq!(state_bytes(root), baseline);
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(!errors.contains(root.to_str().unwrap()));
    assert!(!errors.contains("private-source.txt"));
    assert!(!errors.contains("private-plan.json"));
}

#[tokio::test]
async fn completed_reflection_exports_verified_document_and_restart_does_not_rewrite() {
    let root = fixture();
    let mut server = Server::start(vec![reply("第一份经过验证的草稿。")]).await;
    let revision = prepared(root.path(), &mut server).await;
    let before = cognition(root.path());
    let output = destination(root.path());
    // 所有 EVE 配置均已移除；没有 AGENT 文件仍可导出已保存草稿，不启动模型。
    fs::remove_file(root.path().join("AGENT.md")).unwrap();
    let first = export(root.path(), revision, &output).await;
    assert_eq!(first["command"], "export-plan");
    assert_eq!(first["action"]["duplicate"], false);
    let record = &first["action"]["record"];
    assert_eq!(record["status"], "completed");
    assert!(record["failure"].is_null());
    let bytes = fs::read(&output).unwrap();
    let document: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(document, artifact("第一份经过验证的草稿。"));
    assert_eq!(record["receipt"]["sha256"], sha256(&bytes));
    assert_eq!(record["receipt"]["byte_count"], bytes.len() as u64);
    assert_eq!(record["proposal"]["artifact_sha256"], sha256(&bytes));
    assert_eq!(record["proposal"]["input_sha256"], sha256(INPUT.as_bytes()));
    assert_eq!(record["proposal"]["input_byte_count"], INPUT.len() as u64);
    assert_eq!(record["proposal"]["goal_revision"], revision);
    assert_eq!(record["proposal"]["goal_id"], "goal");
    assert_eq!(cognition(root.path()), before);
    assert_eq!(before["state"]["goals"]["goal"]["status"], "Waiting");
    assert!(before["state"]["goals"]["goal"]["feedback"].is_null());
    let modified = fs::metadata(&output).unwrap().modified().unwrap();
    let baseline = state_bytes(root.path());
    let repeated = export(root.path(), revision, &output).await;
    assert_eq!(repeated["action"]["duplicate"], true);
    assert_eq!(repeated["action"]["record"], *record);
    assert_eq!(fs::read(&output).unwrap(), bytes);
    assert_eq!(fs::metadata(&output).unwrap().modified().unwrap(), modified);
    assert_eq!(state_bytes(root.path()), baseline);
    let shown = invoke(root.path(), &["show", "--id", "goal"]).await;
    assert_eq!(shown["actions"], json!([record]));
    assert_eq!(state_bytes(root.path()), baseline);
    for report in [&first, &repeated, &shown] {
        let encoded = report.to_string();
        assert!(!encoded.contains(root.path().to_str().unwrap()));
        assert!(!encoded.contains("private-source.txt"));
        assert!(!encoded.contains("private-plan.json"));
    }
    fs::write(source(root.path()), "导出后材料已经更新，不能复用旧确认。").unwrap();
    rejects_without_state_change(
        root.path(),
        export_command(root.path(), revision, &sha256(INPUT.as_bytes()), &output),
    )
    .await;
    assert_eq!(fs::read(&output).unwrap(), bytes);
    assert_eq!(fs::metadata(&output).unwrap().modified().unwrap(), modified);
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn changed_input_and_incorrect_input_hash_are_rejected_before_action() {
    let root = fixture();
    let mut server = Server::start(vec![reply("来源未变时有效的草稿。")]).await;
    let revision = prepared(root.path(), &mut server).await;
    let output = destination(root.path());
    rejects_without_state_change(
        root.path(),
        export_command(root.path(), revision, &"0".repeat(64), &output),
    )
    .await;
    fs::write(source(root.path()), "材料已变更，原观察尚未刷新。").unwrap();
    for hash in [
        sha256(INPUT.as_bytes()),
        sha256(&fs::read(source(root.path())).unwrap()),
    ] {
        rejects_without_state_change(
            root.path(),
            export_command(root.path(), revision, &hash, &output),
        )
        .await;
    }
    assert!(!output.exists());
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn feedback_requires_current_reflection_but_can_keep_unchanged_old_observation() {
    let root = fixture();
    let mut server = Server::start(vec![
        reply("旧版整理草稿。"),
        reply("遵循一页约束的新草稿。"),
    ])
    .await;
    let old_revision = prepared(root.path(), &mut server).await;
    let feedback = invoke(
        root.path(),
        &[
            "feedback",
            "--id",
            "goal",
            "--revision",
            &old_revision.to_string(),
            "--feedback-id",
            "one-page",
            "--text",
            "计划保持一页，不自动发布。",
        ],
    )
    .await;
    let revision = feedback["goal_revision"].as_u64().unwrap();
    let hash = sha256(INPUT.as_bytes());
    let output = destination(root.path());
    for rejected_revision in [old_revision, revision] {
        rejects_without_state_change(
            root.path(),
            export_command(root.path(), rejected_revision, &hash, &output),
        )
        .await;
    }
    assert!(!output.exists());
    observe_and_reflect(root.path(), &mut server, 2).await;
    let state = cognition(root.path());
    assert_eq!(state["state"]["goals"]["goal"]["revision"], revision);
    assert_eq!(
        state["state"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["source"]["kind"] == "Environment")
            .count(),
        1
    );
    let report = export(root.path(), revision, &output).await;
    assert_eq!(report["action"]["record"]["status"], "completed");
    let document: Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    assert_eq!(document, artifact("遵循一页约束的新草稿。"));
    assert_eq!(cognition(root.path()), state);
    rejects_without_state_change(
        root.path(),
        export_command(
            root.path(),
            old_revision,
            &hash,
            &root.path().join("stale.json"),
        ),
    )
    .await;
    assert!(!root.path().join("stale.json").exists());
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn owner_and_explicit_observation_binding_are_required_before_action() {
    let root = fixture();
    let mut server = Server::start(vec![reply("仅属于 owner 的草稿。")]).await;
    let revision = prepared(root.path(), &mut server).await;
    let hash = sha256(INPUT.as_bytes());
    let output = destination(root.path());
    let mut wrong_owner = export_command(root.path(), revision, &hash, &output);
    wrong_owner.args(["--user", "other-user"]);
    rejects_without_state_change(root.path(), wrong_owner).await;
    let other_file = root.path().join("different-binding.txt");
    fs::write(&other_file, INPUT).unwrap();
    let mut different_binding = command(root.path());
    different_binding
        .args([
            "export-plan",
            "--id",
            "goal",
            "--revision",
            &revision.to_string(),
            "--input-sha256",
            &hash,
            "--observe-file",
        ])
        .arg(other_file)
        .arg("--output")
        .arg(&output);
    rejects_without_state_change(root.path(), different_binding).await;
    assert!(!output.exists());
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn existing_output_is_preserved_and_blocked_action_does_not_retry_on_restart() {
    let root = fixture();
    let mut server = Server::start(vec![reply("不能覆盖用户文件的草稿。")]).await;
    let revision = prepared(root.path(), &mut server).await;
    let output = destination(root.path());
    let original = b"existing user document, do not replace";
    fs::write(&output, original).unwrap();
    let before = cognition(root.path());
    let report = export(root.path(), revision, &output).await;
    assert_eq!(report["action"]["duplicate"], false);
    assert_eq!(report["action"]["record"]["status"], "blocked");
    assert_eq!(report["action"]["record"]["failure"], "write_failed");
    assert!(report["action"]["record"]["receipt"].is_null());
    assert_eq!(fs::read(&output).unwrap(), original);
    assert_eq!(cognition(root.path()), before);
    let baseline = state_bytes(root.path());
    fs::remove_file(&output).unwrap();
    let repeated = export(root.path(), revision, &output).await;
    assert_eq!(repeated["action"]["duplicate"], true);
    assert_eq!(repeated["action"]["record"], report["action"]["record"]);
    assert!(!output.exists(), "移除既有文件不能导致已封存行动自动重试");
    assert_eq!(state_bytes(root.path()), baseline);
    let shown = invoke(root.path(), &["show", "--id", "goal"]).await;
    assert_eq!(shown["actions"], json!([report["action"]["record"]]));
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn changing_output_path_does_not_create_another_action_for_the_same_goal_revision() {
    let root = fixture();
    let mut server = Server::start(vec![reply("该修订只导出一次的草稿。")]).await;
    let revision = prepared(root.path(), &mut server).await;
    let output = destination(root.path());
    let report = export(root.path(), revision, &output).await;
    assert_eq!(report["action"]["record"]["status"], "completed");
    let first_bytes = fs::read(&output).unwrap();
    let alternate = root.path().join("alternate.json");
    rejects_without_state_change(
        root.path(),
        export_command(root.path(), revision, &sha256(INPUT.as_bytes()), &alternate),
    )
    .await;
    assert!(!alternate.exists());
    assert_eq!(fs::read(&output).unwrap(), first_bytes);
    let shown = invoke(root.path(), &["show", "--id", "goal"]).await;
    assert_eq!(shown["actions"].as_array().unwrap().len(), 1);
    assert_eq!(shown["actions"][0], report["action"]["record"]);
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn show_recovers_unfinished_action_without_replaying_its_existing_file_effect() {
    let root = fixture();
    let mut server = Server::start(vec![reply("恢复必须保留的既有草稿。")]).await;
    let revision = prepared(root.path(), &mut server).await;
    let output = destination(root.path());
    let report = export(root.path(), revision, &output).await;
    assert_eq!(report["action"]["record"]["status"], "completed");
    let bytes = fs::read(&output).unwrap();
    let modified = fs::metadata(&output).unwrap().modified().unwrap();
    let original_cognition = cognition(root.path());

    // 仅在本测试临时目录构造“写完文件后、终态保存前退出”的持久夹具。
    // 保留真实 CLI 产生的提案、来源证据、会话和输出，仅退回已保存的 Executing。
    let state_file = root.path().join("state/state.json");
    let mut outer: Value = serde_json::from_slice(&fs::read(&state_file).unwrap()).unwrap();
    let entry = &mut outer["entries"][eve_action_api::ACTION_PLUGIN_ID]
        [eve_action_plugin::ACTION_STATE_KEY];
    let journal_bytes: Vec<u8> = serde_json::from_value(entry.clone()).unwrap();
    let mut journal: Value = serde_json::from_slice(&journal_bytes).unwrap();
    assert_eq!(journal["records"].as_array().unwrap().len(), 1);
    journal["revision"] = json!(1);
    let record = &mut journal["records"][0];
    record["revision"] = json!(1);
    record["status"] = json!("executing");
    record["finished_at_ms"] = Value::Null;
    record["receipt"] = Value::Null;
    record["failure"] = Value::Null;
    *entry = json!(serde_json::to_vec(&journal).unwrap());
    fs::write(&state_file, serde_json::to_vec(&outer).unwrap()).unwrap();

    let shown = invoke(root.path(), &["show", "--id", "goal"]).await;
    let recovered = &shown["actions"][0];
    assert_eq!(shown["actions"].as_array().unwrap().len(), 1);
    assert_eq!(recovered["status"], "blocked");
    assert_eq!(recovered["failure"], "interrupted");
    assert_eq!(recovered["revision"], 2);
    assert!(recovered["receipt"].is_null());
    assert!(recovered["finished_at_ms"].as_u64().unwrap() > 0);
    assert_eq!(
        recovered["proposal"],
        report["action"]["record"]["proposal"]
    );
    assert_eq!(cognition(root.path()), original_cognition);
    assert_eq!(fs::read(&output).unwrap(), bytes);
    assert_eq!(fs::metadata(&output).unwrap().modified().unwrap(), modified);
    let recovered_bytes = state_bytes(root.path());
    let repeated = export(root.path(), revision, &output).await;
    assert_eq!(repeated["action"]["duplicate"], true);
    assert_eq!(repeated["action"]["record"], *recovered);
    assert_eq!(state_bytes(root.path()), recovered_bytes);
    assert_eq!(fs::read(&output).unwrap(), bytes);
    assert_eq!(fs::metadata(&output).unwrap().modified().unwrap(), modified);
    let after_restart = invoke(root.path(), &["show", "--id", "goal"]).await;
    assert_eq!(after_restart["actions"], shown["actions"]);
    assert_eq!(state_bytes(root.path()), recovered_bytes);
    assert!(server.requests.try_recv().is_err());
}
