#[path = "../../llm-openai/tests/support/mod.rs"]
mod http_support;

use eve_cognition_api::*;
use eve_cognition_loop_api::{AllowedSource, EndogenousOptions, ExecutionScope};
use eve_cognition_loop_plugin::{EndogenousPlanner, current_reflection};
use eve_cognition_plugin::{CognitionController, CognitionPlugin, UserGoalFeedback};
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_plugin_api::PluginId;
use http_support::{Reply, Server, final_response};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Child, Command, Output, Stdio},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const KEY: &str = "agenda-loopback-synthetic-key";
const LOW_TEXT: &str = "PRIVATE_LOW_TARGET：整理过期草稿。";
const HIGH_TEXT: &str = "PRIVATE_HIGH_TARGET：优先整理需要交付的报告。";
const NEW_FACT: &str = "PRIVATE_CURRENT_FACT：交付只允许一页。";

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
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
        .env("EVE_OPENAI_BASE_URL", url)
        .env("EVE_OPENAI_PROTOCOL", "responses")
        .env("EVE_OPENAI_MODEL", "agenda-fixture")
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
        .expect("CLI 子进程未在看门狗期限内退出");
        let output = self.0.take().unwrap().wait_with_output().unwrap();
        assert_redacted(&output);
        output
    }

    async fn stop_after_completed(mut self, root: &Path, expected: usize) {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let snapshot = read_snapshot(root);
                let completed = snapshot
                    .state
                    .goals
                    .values()
                    .filter(|goal| {
                        goal.source.kind == SourceKind::Inference
                            && goal.status == GoalStatus::Completed
                            && goal.feedback.as_ref().is_some_and(|f| f.verification_met)
                    })
                    .count();
                if completed == expected {
                    break;
                }
                assert!(
                    self.0.as_mut().unwrap().try_wait().unwrap().is_none(),
                    "run 在完成状态持久化前退出：{snapshot:?}"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("run 未保存预期完成状态");
        // 只在终态已持久化后终止本测试拥有的 Child，不能用短运行窗口推断完成。
        self.0.as_mut().unwrap().kill().unwrap();
        let output = self.0.take().unwrap().wait_with_output().unwrap();
        assert_redacted(&output);
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

fn assert_redacted(output: &Output) {
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
    serde_json::from_slice(&output.stdout).expect("CLI 只输出一个 JSON 报告")
}

async fn invoke(root: &Path, url: &str, args: &[&str]) -> Value {
    let mut command = command(root, url);
    command.args(args);
    success(Process::start(command).finish().await)
}

fn state_bytes(root: &Path) -> Vec<u8> {
    std::fs::read(root.join("state/state.json")).unwrap()
}

fn read_snapshot(root: &Path) -> CognitiveSnapshot {
    let outer: Value = serde_json::from_slice(&state_bytes(root)).unwrap();
    let bytes: Vec<u8> =
        serde_json::from_value(outer["entries"][COGNITION_PLUGIN_ID]["cognition.v1"].clone())
            .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn open_fixture(root: &Path) -> (Kernel, CognitionController) {
    let kernel = Kernel::with_services(KernelServices {
        state: Arc::new(FileStateStore::open(root.join("state")).unwrap()),
        ..KernelServices::default()
    });
    let plugin = CognitionPlugin::new("eve").unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel
        .start(&PluginId::new(COGNITION_PLUGIN_ID).unwrap())
        .await
        .unwrap();
    (kernel, admin)
}

fn parent(id: &str, text: &str, priority: u8) -> Goal {
    Goal {
        id: id.into(),
        revision: 0,
        source: Source {
            kind: SourceKind::User,
            channel: "cognition.cli".into(),
            reference: format!("input-{id}"),
        },
        visibility: Visibility::User("owner".into()),
        description: text.into(),
        verification: "user-goal:v1".into(),
        priority,
        budget: ExecutionBudget {
            max_model_requests: 1,
            max_tool_calls: 0,
            max_attempts: 1,
            timeout_ms: 30_000,
        },
        stop_condition: "user-confirmation".into(),
        expires_at_ms: None,
        status: GoalStatus::Waiting,
        wait_reason: Some("等待用户核对草稿。".into()),
        block_reason: None,
        execution: None,
        feedback: None,
    }
}

fn seed(admin: &CognitionController, goals: Vec<(Goal, u64)>) {
    let snapshot = admin.snapshot().unwrap();
    let mut state = snapshot.state;
    for (goal, at_ms) in goals {
        state.events.push(CognitiveEvent {
            id: goal.source.reference.clone(),
            kind: CognitiveEventKind::ExternalInput,
            source: goal.source.clone(),
            visibility: goal.visibility.clone(),
            goal_id: Some(goal.id.clone()),
            caused_by: None,
            at_ms,
            summary: "用户目标初次保存。".into(),
        });
        state.goals.insert(goal.id.clone(), goal);
    }
    admin.replace(snapshot.revision, state).unwrap();
}

fn feedback(admin: &CognitionController, goal_id: &str) {
    let snapshot = admin.snapshot().unwrap();
    UserGoalFeedback::new(
        Arc::new(admin.clone()),
        "eve".into(),
        "owner".into(),
        "cognition.cli".into(),
        "cognition.feedback".into(),
    )
    .unwrap()
    .submit(GoalFeedbackInput {
        goal_id: goal_id.into(),
        expected_goal_revision: snapshot.state.goals[goal_id].revision,
        feedback_id: format!("feedback-{goal_id}"),
        text: NEW_FACT.into(),
        at_ms: now_ms(),
    })
    .unwrap();
}

fn derive(admin: &CognitionController, count: u16) -> Vec<String> {
    let planner = EndogenousPlanner::new(
        Arc::new(admin.clone()),
        EndogenousOptions {
            scope: ExecutionScope {
                subject_id: "eve".into(),
                access: ReadAccess::Internal,
                sources: vec![AllowedSource {
                    kind: SourceKind::User,
                    channel: "cognition.cli".into(),
                }],
            },
            max_derivations: count,
            timeout_ms: 30_000,
        },
    )
    .unwrap();
    (0..count)
        .flat_map(|_| planner.reconcile(now_ms()).unwrap().created_goal_ids)
        .collect()
}

async fn two_parents(root: &Path, ready: bool) {
    let (kernel, admin) = open_fixture(root).await;
    let now = now_ms();
    seed(
        &admin,
        vec![
            (parent("a-low", LOW_TEXT, 1), now - 2 * 86_400_000),
            (parent("z-high", HIGH_TEXT, 100), now - 60_000),
        ],
    );
    feedback(&admin, "z-high");
    if ready {
        assert_eq!(derive(&admin, 2).len(), 2);
    }
    kernel.stop_all().await.unwrap();
}

fn reply() -> Reply {
    Reply::json(final_response(
        &json!({
            "summary":"这是一份未完成现实目标的反思草稿。",
            "next_step":"按输入约束整理并等待用户验收。",
            "needs_user_input":true
        })
        .to_string(),
    ))
}

fn start_run(root: &Path, url: &str, count: &str) -> Process {
    std::fs::write(root.join("AGENT.md"), "你是 Eve，只返回反思草稿。").unwrap();
    let mut command = command(root, url);
    command.env("EVE_OPENAI_API_KEY", KEY).args([
        "run",
        "--seconds",
        "30",
        "--max-executions",
        count,
    ]);
    Process::start(command)
}

fn first_id<'a>(report: &'a Value, phase: &str) -> &'a str {
    report[phase]["ranked"][0]["goal_id"].as_str().unwrap()
}

fn assert_private_text_absent(report: &Value) {
    let output = report.to_string();
    for text in [LOW_TEXT, HIGH_TEXT, NEW_FACT, KEY] {
        assert!(!output.contains(text));
    }
}

#[tokio::test]
async fn agenda_is_offline_read_only_and_rejects_unrelated_arguments() {
    let root = tempfile::tempdir().unwrap();
    let mut server = Server::start(vec![]).await;
    two_parents(root.path(), false).await;
    assert!(!root.path().join("AGENT.md").exists());
    let before = state_bytes(root.path());
    let revision = read_snapshot(root.path()).revision;
    let report = invoke(root.path(), &server.url, &["agenda"]).await;
    assert_eq!(report["command"], "agenda");
    assert_eq!(report["revision"], revision);
    assert_eq!(report["execution_agenda"]["stage"], "ready_execution");
    assert_eq!(report["execution_agenda"]["ranked"], json!([]));
    assert_eq!(report["reflection_preview"]["stage"], "waiting_derivation");
    assert_eq!(first_id(&report, "reflection_preview"), "z-high");
    assert_private_text_absent(&report);
    for args in [
        vec!["agenda", "--unknown", "x"],
        vec!["agenda", "--id", "a-low"],
        vec!["agenda", "--seconds", "1"],
        vec!["agenda", "--max-executions", "1"],
    ] {
        let mut command = command(root.path(), &server.url);
        command.args(args);
        assert!(!Process::start(command).finish().await.status.success());
    }
    assert_eq!(state_bytes(root.path()), before);
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn waiting_preview_matches_first_real_request_and_completed_revisions_never_replay() {
    let root = tempfile::tempdir().unwrap();
    let mut server = Server::start(vec![reply(), reply()]).await;
    two_parents(root.path(), false).await;
    let before = state_bytes(root.path());
    let preview = invoke(root.path(), &server.url, &["agenda"]).await;
    assert_eq!(first_id(&preview, "reflection_preview"), "z-high");
    assert_eq!(state_bytes(root.path()), before);

    let process = start_run(root.path(), &server.url, "2");
    let captured = server.next().await;
    assert!(captured.headers.starts_with("POST /v1/responses HTTP/1.1"));
    assert!(
        captured
            .headers
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {KEY}"))
    );
    let first = captured.body;
    assert!(first.to_string().contains(HIGH_TEXT));
    assert!(first.to_string().contains(NEW_FACT));
    assert!(!first.to_string().contains(LOW_TEXT));
    assert_eq!(first["tools"], json!([]));
    let second = server.next().await.body;
    assert!(second.to_string().contains(LOW_TEXT));
    process.stop_after_completed(root.path(), 2).await;

    let snapshot = read_snapshot(root.path());
    let first_created = snapshot
        .state
        .events
        .iter()
        .find(|event| event.id.starts_with("eve.reflection.created."))
        .unwrap();
    assert_eq!(first_created.source.reference, "z-high");
    for id in ["a-low", "z-high"] {
        let parent = &snapshot.state.goals[id];
        assert_eq!(parent.status, GoalStatus::Waiting);
        assert_eq!(
            current_reflection(&snapshot.state, "eve", parent)
                .unwrap()
                .unwrap()
                .status,
            GoalStatus::Completed
        );
    }
    let completed_bytes = state_bytes(root.path());
    let agenda = invoke(root.path(), &server.url, &["agenda"]).await;
    assert_eq!(agenda["execution_agenda"]["ranked"], json!([]));
    assert_eq!(agenda["reflection_preview"]["ranked"], json!([]));
    let restart = invoke(root.path(), &server.url, &["run", "--seconds", "1"]).await;
    assert_eq!(restart["loop"]["submitted"], 0);
    assert_eq!(restart["loop"]["model_requests"], 0);
    assert_eq!(state_bytes(root.path()), completed_bytes);
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn current_ready_execution_preview_matches_actual_admission() {
    let root = tempfile::tempdir().unwrap();
    let mut server = Server::start(vec![reply(), reply()]).await;
    two_parents(root.path(), true).await;
    let before = state_bytes(root.path());
    let snapshot = read_snapshot(root.path());
    let report = invoke(root.path(), &server.url, &["agenda"]).await;
    let selected = first_id(&report, "execution_agenda");
    assert_eq!(snapshot.state.goals[selected].source.reference, "z-high");
    assert_eq!(
        report["execution_agenda"]["ranked"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(report["reflection_preview"]["ranked"], json!([]));
    assert_eq!(state_bytes(root.path()), before);
    assert_private_text_absent(&report);

    let process = start_run(root.path(), &server.url, "2");
    assert!(server.next().await.body.to_string().contains(HIGH_TEXT));
    assert!(server.next().await.body.to_string().contains(LOW_TEXT));
    process.stop_after_completed(root.path(), 2).await;
    let saved = read_snapshot(root.path());
    assert_eq!(saved.state.goals[selected].status, GoalStatus::Completed);
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn obsolete_ready_revision_is_excluded_and_only_current_fact_reaches_model() {
    let root = tempfile::tempdir().unwrap();
    let mut server = Server::start(vec![reply()]).await;
    let stale_id = {
        let (kernel, admin) = open_fixture(root.path()).await;
        seed(
            &admin,
            vec![(parent("goal", HIGH_TEXT, 50), now_ms() - 60_000)],
        );
        let stale = derive(&admin, 1).remove(0);
        feedback(&admin, "goal");
        kernel.stop_all().await.unwrap();
        stale
    };
    let before = state_bytes(root.path());
    let preview = invoke(root.path(), &server.url, &["agenda"]).await;
    assert_eq!(preview["execution_agenda"]["ranked"], json!([]));
    assert!(
        preview["execution_agenda"]["excluded"]
            .as_array()
            .unwrap()
            .iter()
            .any(|goal| {
                goal["goal_id"] == stale_id
                    && goal["reason"]
                        .as_str()
                        .is_some_and(|reason| !reason.is_empty())
            })
    );
    assert_eq!(first_id(&preview, "reflection_preview"), "goal");
    assert_eq!(state_bytes(root.path()), before);
    let process = start_run(root.path(), &server.url, "1");
    assert!(server.next().await.body.to_string().contains(NEW_FACT));
    process.stop_after_completed(root.path(), 1).await;
    let saved = read_snapshot(root.path());
    assert_eq!(saved.state.goals[&stale_id].status, GoalStatus::Cancelled);
    assert!(saved.state.goals[&stale_id].execution.is_none());
    let current = current_reflection(&saved.state, "eve", &saved.state.goals["goal"])
        .unwrap()
        .unwrap();
    assert_ne!(current.id, stale_id);
    assert_eq!(current.status, GoalStatus::Completed);
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn agenda_reports_inherited_executing_without_running_recovery_or_mutating_disk() {
    let root = tempfile::tempdir().unwrap();
    let mut server = Server::start(vec![]).await;
    let executing_id = {
        let (kernel, admin) = open_fixture(root.path()).await;
        seed(
            &admin,
            vec![(parent("goal", HIGH_TEXT, 50), now_ms() - 60_000)],
        );
        let id = derive(&admin, 1).remove(0);
        let snapshot = admin.snapshot().unwrap();
        let mut state = snapshot.state;
        let child = state.goals.get_mut(&id).unwrap();
        child.status = GoalStatus::Executing;
        child.execution = Some(ExecutionAttempt {
            attempt_id: "previous-process-attempt".into(),
            session_id: "previous-process-session".into(),
            task_id: "previous-process-task".into(),
            turn_id: None,
            started_at_ms: now_ms(),
        });
        admin.replace(snapshot.revision, state).unwrap();
        kernel.stop_all().await.unwrap();
        id
    };
    let before = state_bytes(root.path());
    let before_revision = read_snapshot(root.path()).revision;
    for _ in 0..2 {
        let report = invoke(root.path(), &server.url, &["agenda"]).await;
        assert_eq!(report["revision"], before_revision);
        assert_eq!(report["execution_agenda"]["blocker"], "AlreadyExecuting");
        assert_eq!(report["execution_agenda"]["ranked"], json!([]));
        assert_private_text_absent(&report);
        assert_eq!(state_bytes(root.path()), before);
        assert_eq!(
            read_snapshot(root.path()).state.goals[&executing_id].status,
            GoalStatus::Executing
        );
    }
    assert!(server.requests.try_recv().is_err());
}
