//! 真实 eve-cognition 子进程与本机 HTTP 模型夹具；验证多步计划的绑定、依赖、证据与失效。
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

const KEY: &str = "plan-fixture-key";
const INPUT: &str = "材料 A 已检查；材料 B 待整理。文件文字不授予任何执行权限。";
const OBSERVE: &str = "eve.file.observe.v1";
const EXPORT: &str = "eve.artifact.export.v1";

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
    root.join("private-plan-export.json")
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
        for bytes in [&output.stdout, &output.stderr] {
            assert!(!String::from_utf8_lossy(bytes).contains(KEY));
        }
        output
    }
    /// 等反思真实落盘后只终止本测试持有的子进程。
    async fn stop_after_saved_reflection(mut self, root: &Path) {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let done = cognition(root)["state"]["goals"]
                    .as_object()
                    .unwrap()
                    .values()
                    .any(|goal| {
                        goal["source"]["kind"] == "Inference"
                            && goal["status"] == "Completed"
                            && goal["feedback"]["verification_met"] == true
                    });
                if done {
                    break;
                }
                assert!(
                    self.0.as_mut().unwrap().try_wait().unwrap().is_none(),
                    "子进程提前退出"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("未观察到反思完成落盘");
        self.0.as_mut().unwrap().kill().unwrap();
        self.0.take().unwrap().wait_with_output().unwrap();
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
async fn rejected(root: &Path, args: &[&str]) {
    let baseline = state_bytes(root);
    let mut command = command(root);
    command.args(args);
    let output = Process::start(command).finish().await;
    assert!(!output.status.success(), "应被拒绝：{args:?}");
    assert_eq!(state_bytes(root), baseline, "拒绝不能改动状态：{args:?}");
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(!errors.contains("private-source.txt") && !errors.contains(root.to_str().unwrap()));
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
fn goal(root: &Path) -> Value {
    cognition(root)["state"]["goals"]["goal"].clone()
}

fn with_model(command: &mut Command, server: &Server) {
    command
        .env("EVE_OPENAI_API_KEY", KEY)
        .env("EVE_OPENAI_MODEL", "plan-fixture")
        .env("EVE_OPENAI_PROTOCOL", "responses")
        .env("EVE_OPENAI_BASE_URL", &server.url)
        .env("EVE_OPENAI_TIMEOUT_SECONDS", "30")
        .env("EVE_OPENAI_REASONING_EFFORT", "none")
        .env("EVE_LLM_RESPONSE_MODE", "complete");
}

/// 保存目标、观察输入文件并完成一次反思；返回当前目标修订与输入摘要。
async fn prepared(root: &Path, server: &mut Server) -> (u64, String) {
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
    let mut command = command(root);
    with_model(&mut command, server);
    command
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
        .stop_after_saved_reflection(root)
        .await;
    let request = server.next().await;
    assert!(request.headers.starts_with("POST /v1/responses HTTP/1.1"));
    assert_eq!(request.body["tools"], json!([]));
    (
        goal(root)["revision"].as_u64().unwrap(),
        sha256(&fs::read(source(root)).unwrap()),
    )
}

fn reflection() -> Reply {
    Reply::json(final_response(
        &json!({"summary": "先核对材料再导出清单。", "next_step": "导出后等待用户更新材料。",
            "needs_user_input": true})
        .to_string(),
    ))
}
fn steps_file(root: &Path, name: &str, steps: Value) -> String {
    let path = root.join(name);
    fs::write(&path, json!({"steps": steps}).to_string()).unwrap();
    path.to_str().unwrap().into()
}
fn three_steps(input: &str) -> Value {
    json!([
        {"id": "check", "title": "确认输入仍是计划依据", "capability": OBSERVE, "depends_on": [],
         "max_attempts": 1, "timeout_ms": 5000, "effect": {"kind": "digest_equals", "sha256": input}},
        {"id": "export", "title": "导出当前草稿", "capability": EXPORT, "depends_on": ["check"],
         "max_attempts": 1, "timeout_ms": 30000, "effect": {"kind": "verified"}},
        {"id": "wait", "title": "等待用户按清单更新材料", "capability": OBSERVE, "depends_on": ["export"],
         "max_attempts": 2, "timeout_ms": 5000, "effect": {"kind": "digest_differs", "sha256": input}}
    ])
}
async fn create(root: &Path, revision: u64, input: &str) -> Value {
    let steps = steps_file(root, "steps.json", three_steps(input));
    invoke(
        root,
        &[
            "plan-create",
            "--id",
            "goal",
            "--revision",
            &revision.to_string(),
            "--input-sha256",
            input,
            "--steps",
            &steps,
        ],
    )
    .await
}
async fn step(root: &Path, plan: &str, step: &str, output: bool) -> Value {
    let mut command = command(root);
    command
        .args([
            "plan-step",
            "--plan",
            plan,
            "--step",
            step,
            "--observe-file",
        ])
        .arg(source(root));
    if output {
        command.arg("--output").arg(destination(root));
    }
    success(Process::start(command).finish().await)
}
fn statuses(plan: &Value) -> Vec<String> {
    plan["plan"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["status"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn three_step_plan_waits_for_changed_input_and_completes_from_independent_evidence() {
    let root = fixture();
    let mut server = Server::start(vec![reflection()]).await;
    let (revision, input) = prepared(root.path(), &mut server).await;
    let created = create(root.path(), revision, &input).await;
    assert_eq!(created["duplicate"], false);
    let plan_id = created["plan"]["plan"]["id"].as_str().unwrap().to_owned();
    assert_eq!(created["plan"]["ready_steps"], json!(["check"]));
    assert_eq!(
        create(root.path(), revision, &input).await["duplicate"],
        true
    );

    let checked = step(root.path(), &plan_id, "check", false).await;
    assert_eq!(checked["step"]["status"], "satisfied");
    assert_eq!(checked["plan"]["ready_steps"], json!(["export"]));
    // 缺少导出授权参数时在保存 Executing 前拒绝，不消耗唯一一次尝试。
    rejected(
        root.path(),
        &["plan-step", "--plan", &plan_id, "--step", "export"],
    )
    .await;
    rejected(
        root.path(),
        &["plan-step", "--plan", &plan_id, "--step", "wait"],
    )
    .await;

    let exported = step(root.path(), &plan_id, "export", true).await;
    assert_eq!(exported["step"]["status"], "satisfied");
    let written = fs::read(destination(root.path())).unwrap();
    assert_eq!(
        exported["step"]["attempts"][0]["evidence"]["sha256"],
        sha256(&written),
        "效果证据来自独立回读的产物"
    );
    let unchanged = step(root.path(), &plan_id, "wait", false).await;
    assert_eq!(
        unchanged["step"]["status"], "pending",
        "输入尚未变化：效果未满足，可在剩余次数内重试"
    );
    assert_eq!(
        unchanged["step"]["attempts"][0]["failure"],
        "effect_not_met"
    );

    fs::write(source(root.path()), "材料 B 已按清单整理。").unwrap();
    let completed = step(root.path(), &plan_id, "wait", false).await;
    assert_eq!(completed["plan"]["plan"]["status"], "completed");
    assert_eq!(statuses(&completed["plan"]), ["satisfied"; 3]);
    let parent = goal(root.path());
    assert_eq!(parent["status"], "Waiting", "计划完成不改变父目标");
    assert_eq!(parent["revision"].as_u64().unwrap(), revision);

    let shown = invoke(root.path(), &["plan-show", "--id", "goal"]).await;
    assert_eq!(shown["plans"][0]["plan"]["status"], "completed");
    assert_eq!(shown["plans"][0]["binding_current"], true);
    assert!(server.requests.try_recv().is_err(), "计划命令不调用模型");
}

#[tokio::test]
async fn user_feedback_invalidates_pending_steps_and_keeps_satisfied_history() {
    let root = fixture();
    let mut server = Server::start(vec![reflection()]).await;
    let (revision, input) = prepared(root.path(), &mut server).await;
    let plan_id = create(root.path(), revision, &input).await["plan"]["plan"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    step(root.path(), &plan_id, "check", false).await;
    invoke(
        root.path(),
        &[
            "feedback",
            "--id",
            "goal",
            "--revision",
            &revision.to_string(),
            "--feedback-id",
            "fact-1",
            "--text",
            "只整理材料 B。",
        ],
    )
    .await;

    let stale = step(root.path(), &plan_id, "export", true).await;
    assert_eq!(stale["executed"], false);
    assert_eq!(stale["reason"], "binding_changed");
    assert_eq!(stale["plan"]["plan"]["status"], "stale");
    assert_eq!(
        statuses(&stale["plan"]),
        ["satisfied", "invalidated", "invalidated"]
    );
    assert!(!destination(root.path()).exists(), "过时计划不执行导出");
    rejected(
        root.path(),
        &[
            "plan-step",
            "--plan",
            &plan_id,
            "--step",
            "wait",
            "--observe-file",
            source(root.path()).to_str().unwrap(),
        ],
    )
    .await;

    // 新修订可以建立新计划；旧计划保留为历史。
    let next = create(root.path(), revision + 1, &input).await;
    assert_eq!(next["duplicate"], false);
    let shown = invoke(root.path(), &["plan-show", "--id", "goal"]).await;
    let plans = shown["plans"].as_array().unwrap();
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0]["binding_current"], false);
    assert_eq!(plans[1]["binding_current"], true);
}

#[tokio::test]
async fn invalid_or_mismatched_plans_are_rejected_without_state_change() {
    let root = fixture();
    let mut server = Server::start(vec![reflection()]).await;
    let (revision, input) = prepared(root.path(), &mut server).await;
    let revision = revision.to_string();
    let other = "f".repeat(64);
    let mut cycle = three_steps(&input);
    cycle[0]["depends_on"] = json!(["wait"]);
    let mut shell = three_steps(&input);
    shell[0]["capability"] = json!("eve.shell.v1");
    let mut short = three_steps(&input);
    short[1]["timeout_ms"] = json!(1000);
    let mut retried = three_steps(&input);
    retried[1]["max_attempts"] = json!(2);
    for (name, steps) in [
        ("cycle.json", cycle),
        ("shell.json", shell),
        ("short.json", short),
        ("retried.json", retried),
    ] {
        let path = steps_file(root.path(), name, steps);
        rejected(
            root.path(),
            &[
                "plan-create",
                "--id",
                "goal",
                "--revision",
                &revision,
                "--input-sha256",
                &input,
                "--steps",
                &path,
            ],
        )
        .await;
    }
    let valid = steps_file(root.path(), "valid.json", three_steps(&input));
    rejected(
        root.path(),
        &[
            "plan-create",
            "--id",
            "goal",
            "--revision",
            &revision,
            "--steps",
            &valid,
        ],
    )
    .await;
    rejected(
        root.path(),
        &[
            "plan-create",
            "--id",
            "goal",
            "--revision",
            &revision,
            "--input-sha256",
            &other,
            "--steps",
            &valid,
        ],
    )
    .await;
    rejected(
        root.path(),
        &[
            "plan-create",
            "--id",
            "goal",
            "--revision",
            "99",
            "--input-sha256",
            &input,
            "--steps",
            &valid,
        ],
    )
    .await;
    rejected(
        root.path(),
        &[
            "plan-create",
            "--id",
            "goal",
            "--revision",
            &revision,
            "--input-sha256",
            &input,
            "--steps",
            &valid,
            "--user",
            "intruder",
        ],
    )
    .await;
    fs::write(
        root.path().join("extra.json"),
        r#"{"steps":[],"binding":{}}"#,
    )
    .unwrap();
    rejected(
        root.path(),
        &[
            "plan-create",
            "--id",
            "goal",
            "--revision",
            &revision,
            "--input-sha256",
            &input,
            "--steps",
            root.path().join("extra.json").to_str().unwrap(),
        ],
    )
    .await;
    assert!(server.requests.try_recv().is_err(), "拒绝计划不调用模型");
}

fn proposal_reply(steps: Value) -> Reply {
    Reply::json(final_response(&json!({"steps": steps}).to_string()))
}
async fn propose(root: &Path, server: &Server, revision: u64) -> Value {
    let mut command = command(root);
    with_model(&mut command, server);
    command.args([
        "plan-propose",
        "--id",
        "goal",
        "--revision",
        &revision.to_string(),
    ]);
    success(Process::start(command).finish().await)
}
async fn feedback(root: &Path, revision: u64, id: &str) {
    invoke(
        root,
        &[
            "feedback",
            "--id",
            "goal",
            "--revision",
            &revision.to_string(),
            "--feedback-id",
            id,
            "--text",
            "只整理材料 B。",
        ],
    )
    .await;
}

#[tokio::test]
async fn model_proposal_waits_for_operator_confirmation_and_is_requested_once() {
    let root = fixture();
    let digest = sha256(INPUT.as_bytes());
    let mut server = Server::start(vec![reflection(), proposal_reply(three_steps(&digest))]).await;
    let (revision, input) = prepared(root.path(), &mut server).await;
    assert_eq!(input, digest);

    let proposed = propose(root.path(), &server, revision).await;
    assert_eq!(proposed["duplicate"], false);
    assert_eq!(proposed["model_request_attempted"], true);
    let result = &proposed["result"];
    assert_eq!(result["proposal"]["status"]["kind"], "proposed");
    assert_eq!(result["plan"]["plan"]["status"], "proposed");
    assert_eq!(result["plan"]["plan"]["origin"]["kind"], "model");
    assert_eq!(result["plan"]["ready_steps"], json!([]), "未确认不准入");
    assert_eq!(result["plan"]["plan"]["binding"]["input_sha256"], input);
    let plan_id = result["plan"]["plan"]["id"].as_str().unwrap().to_owned();

    let request = server.next().await;
    assert_eq!(request.body["tools"], json!([]), "建议请求不安装工具");
    let body = request.body.to_string();
    assert!(
        body.contains("整理材料并准备一份可审阅的计划。"),
        "目标描述作为数据"
    );
    assert!(
        body.contains("先核对材料再导出清单。"),
        "当前修订的已验证草稿作为数据"
    );
    assert!(body.contains(OBSERVE) && body.contains(EXPORT));
    assert!(
        !body.contains("private-source.txt") && !body.contains("材料 A 已检查"),
        "路径与文件正文不进入建议请求"
    );

    // 待确认的建议既不能执行，也挡住同一目标的其他计划；重复请求不再调用模型。
    rejected(
        root.path(),
        &[
            "plan-step",
            "--plan",
            &plan_id,
            "--step",
            "check",
            "--observe-file",
            source(root.path()).to_str().unwrap(),
        ],
    )
    .await;
    let steps = steps_file(root.path(), "steps.json", three_steps(&input));
    rejected(
        root.path(),
        &[
            "plan-create",
            "--id",
            "goal",
            "--revision",
            &revision.to_string(),
            "--input-sha256",
            &input,
            "--steps",
            &steps,
        ],
    )
    .await;
    let again = propose(root.path(), &server, revision).await;
    assert_eq!(again["duplicate"], true);
    assert_eq!(again["model_request_attempted"], false);
    assert_eq!(again["result"]["plan"]["plan"]["id"], plan_id);
    rejected(
        root.path(),
        &["plan-confirm", "--plan", &plan_id, "--user", "intruder"],
    )
    .await;

    let confirmed = invoke(root.path(), &["plan-confirm", "--plan", &plan_id]).await;
    assert_eq!(confirmed["confirmed"], true);
    assert_eq!(confirmed["plan"]["plan"]["status"], "active");
    assert_eq!(confirmed["plan"]["ready_steps"], json!(["check"]));
    rejected(root.path(), &["plan-confirm", "--plan", &plan_id]).await;
    let checked = step(root.path(), &plan_id, "check", false).await;
    assert_eq!(checked["step"]["status"], "satisfied");

    let withdrawn = invoke(root.path(), &["plan-withdraw", "--plan", &plan_id]).await;
    assert_eq!(withdrawn["plan"]["plan"]["status"], "withdrawn");
    assert_eq!(
        statuses(&withdrawn["plan"]),
        ["satisfied", "blocked", "blocked"]
    );
    assert!(!destination(root.path()).exists(), "撤销后不执行导出");
    rejected(root.path(), &["plan-withdraw", "--plan", &plan_id]).await;
    let shown = invoke(root.path(), &["plan-show", "--id", "goal"]).await;
    assert_eq!(shown["proposals"].as_array().unwrap().len(), 1);
    assert_eq!(goal(root.path())["status"], "Waiting", "父目标保持等待");
    assert!(server.requests.try_recv().is_err(), "每个修订只请求一次");
}

#[tokio::test]
async fn invalid_proposals_and_changed_goals_never_become_runnable_plans() {
    let root = fixture();
    let input = sha256(INPUT.as_bytes());
    let mut shell = three_steps(&input);
    shell[0]["capability"] = json!("eve.shell.v1");
    let observe_only = json!([
        {"id": "wait", "title": "等待材料更新", "capability": OBSERVE, "depends_on": [],
         "max_attempts": 2, "timeout_ms": 5000, "effect": {"kind": "digest_differs", "sha256": input}}
    ]);
    let mut server = Server::start(vec![
        reflection(),
        proposal_reply(shell),
        proposal_reply(observe_only),
    ])
    .await;
    let (revision, _) = prepared(root.path(), &mut server).await;

    // 缺少模型配置、修订不符或他人目标：保存请求记录之前拒绝，不消耗该修订的建议机会。
    rejected(
        root.path(),
        &[
            "plan-propose",
            "--id",
            "goal",
            "--revision",
            &revision.to_string(),
        ],
    )
    .await;
    let mut wrong = command(root.path());
    with_model(&mut wrong, &server);
    wrong.args(["plan-propose", "--id", "goal", "--revision", "99"]);
    let baseline = state_bytes(root.path());
    assert!(!Process::start(wrong).finish().await.status.success());
    let mut intruder = command(root.path());
    with_model(&mut intruder, &server);
    intruder.args([
        "plan-propose",
        "--id",
        "goal",
        "--revision",
        &revision.to_string(),
        "--user",
        "intruder",
    ]);
    assert!(!Process::start(intruder).finish().await.status.success());
    assert_eq!(state_bytes(root.path()), baseline);
    assert!(server.requests.try_recv().is_err());

    let invalid = propose(root.path(), &server, revision).await;
    assert_eq!(invalid["result"]["proposal"]["status"]["kind"], "failed");
    assert_eq!(
        invalid["result"]["proposal"]["status"]["failure"],
        "invalid_output"
    );
    assert_eq!(invalid["result"]["plan"], Value::Null);
    server.next().await;
    let again = propose(root.path(), &server, revision).await;
    assert_eq!(again["duplicate"], true, "失败也不重新请求同一修订");
    assert!(server.requests.try_recv().is_err());

    // 新修订可以再请求一次；当前修订尚无反思草稿时只交目标描述。
    feedback(root.path(), revision, "fact-1").await;
    let proposed = propose(root.path(), &server, revision + 1).await;
    let request = server.next().await;
    assert!(
        !request.body.to_string().contains("先核对材料再导出清单。"),
        "过时草稿不进入请求"
    );
    let plan_id = proposed["result"]["plan"]["plan"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(proposed["result"]["plan"]["plan"]["status"], "proposed");

    // 确认前目标又有新反馈：建议封存为过时，不执行任何步骤。
    feedback(root.path(), revision + 1, "fact-2").await;
    let late = invoke(root.path(), &["plan-confirm", "--plan", &plan_id]).await;
    assert_eq!(late["confirmed"], false);
    assert_eq!(late["reason"], "binding_changed");
    assert_eq!(late["plan"]["plan"]["status"], "stale");
    assert_eq!(statuses(&late["plan"]), ["invalidated"]);
    assert!(server.requests.try_recv().is_err());
}

fn ledger(root: &Path) -> Value {
    let file: Value = serde_json::from_slice(&state_bytes(root)).unwrap();
    let bytes: Vec<u8> =
        serde_json::from_value(file["entries"]["eve.plan"]["plans.v1"].clone()).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn proposal_record_is_saved_before_the_request_and_never_replayed_after_a_crash() {
    let root = fixture();
    let mut slow = proposal_reply(json!([]));
    slow.body_delay = Duration::from_secs(20);
    let mut server = Server::start(vec![reflection(), slow]).await;
    let (revision, _) = prepared(root.path(), &mut server).await;
    let mut command = command(root.path());
    with_model(&mut command, &server);
    command.args([
        "plan-propose",
        "--id",
        "goal",
        "--revision",
        &revision.to_string(),
    ]);
    let mut process = Process::start(command);
    // 模型收到请求时，请求记录已经落盘。
    server.next().await;
    let saved = ledger(root.path());
    assert_eq!(saved["schema_version"], 2);
    assert_eq!(saved["proposals"][0]["status"]["kind"], "requested");
    process.0.as_mut().unwrap().kill().unwrap();
    process.0.take().unwrap().wait_with_output().unwrap();

    let shown = invoke(root.path(), &["plan-show", "--id", "goal"]).await;
    assert_eq!(shown["proposals"][0]["status"]["kind"], "failed");
    assert_eq!(shown["proposals"][0]["status"]["failure"], "interrupted");
    let again = propose(root.path(), &server, revision).await;
    assert_eq!(again["duplicate"], true, "中断的请求不自动重放");
    assert_eq!(again["model_request_attempted"], false);
    assert!(server.requests.try_recv().is_err());
    assert!(shown["plans"].as_array().unwrap().is_empty());
}
