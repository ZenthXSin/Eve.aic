#[path = "../../llm-openai/tests/support/mod.rs"]
mod http_support;

use http_support::{Captured, Reply, Server, final_response};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

const PRIMARY_KEY: &str = "evaluation-primary-fixture-key";
const JEV_KEY: &str = "evaluation-jev-fixture-key";

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned()
}

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_eve-message-evaluate"));
    command.current_dir(repository());
    // 只给所持子进程设置环境，不修改测试进程全局环境或读取真实凭据。
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(|name| name.starts_with("EVE_")) {
            command.env_remove(name);
        }
    }
    let temporary = root.join("temporary");
    std::fs::create_dir_all(&temporary).unwrap();
    command
        .env("TMPDIR", &temporary)
        .env("TMP", &temporary)
        .env("TEMP", &temporary);
    command
}

fn model_command(root: &Path, primary: &Server, jev: &Server) -> Command {
    let mut command = command(root);
    command
        .env("EVE_OPENAI_API_KEY", PRIMARY_KEY)
        .env("EVE_OPENAI_BASE_URL", &primary.url)
        .env("EVE_OPENAI_PROTOCOL", "responses")
        .env("EVE_OPENAI_REASONING_EFFORT", "none")
        .env("EVE_OPENAI_MODEL_ROLE", "primary")
        .env("EVE_MODELS_PRIMARY_ENABLED", "true")
        .env("EVE_MODELS_PRIMARY_PROVIDER", "openai")
        .env("EVE_MODELS_PRIMARY_MODEL", "evaluation-primary")
        .env("EVE_MODELS_PRIMARY_TIMEOUT_MS", "2000")
        .env("EVE_MODELS_PRIMARY_MAX_OUTPUT_TOKENS", "256")
        .env("EVE_JEV_API_KEY", JEV_KEY)
        .env(
            "EVE_JEV_BASE_URL",
            jev.url.trim_end_matches("/v1/responses"),
        )
        .env("EVE_MODELS_JEV_ENABLED", "true")
        .env("EVE_MODELS_JEV_PROVIDER", "jev")
        .env("EVE_MODELS_JEV_MODEL", "evaluation-jev")
        .env("EVE_MODELS_JEV_TIMEOUT_MS", "2000")
        .env("EVE_MESSAGE_JUDGE_TIMEOUT_MS", "4000");
    command
}

async fn run(mut command: Command) -> Output {
    tokio::task::spawn_blocking(move || {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        // 同时排空两个输出管道，报告较大时也不会在 wait 前卡住。
        let output_reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).unwrap();
            bytes
        });
        let error_reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).unwrap();
            bytes
        });
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                // 仅清理本测试实际持有的 Child；不用 PID 文件、信号广播或服务停止命令。
                let _ = child.kill();
                let _ = child.wait();
                let _ = output_reader.join();
                let _ = error_reader.join();
                panic!("消息评估子进程超过 15 秒，已收尾本次 Child");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        Output {
            status,
            stdout: output_reader.join().unwrap(),
            stderr: error_reader.join().unwrap(),
        }
    })
    .await
    .unwrap()
}

fn report(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let visible = String::from_utf8_lossy(&output.stdout);
    for marker in [PRIMARY_KEY, JEV_KEY, "PRIVATE_MODEL_EXPLANATION"] {
        assert!(!visible.contains(marker));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(marker));
    }
    report
}

fn case(id: &str, text: &str, accepted: Value) -> Value {
    json!({
        "id": id, "note": "公开测试标注。",
        "input": {
            "message": {
                "message_id": format!("message-{id}"),
                "target": {"session": {"session_id": "evaluation-session", "user_id": "evaluation-user"},
                    "task_id": "evaluation-task", "controller_epoch": [1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1], "generation": 1},
                "text": text, "reply_to": null
            },
            "phase": "Generating", "task_text": "撰写报告。", "cancel_requested": false,
            "started_tools": 0, "clarification": null
        },
        "accepted": accepted
    })
}

fn expected(intent: &str, span: Option<(usize, usize)>) -> Value {
    json!({"intent": intent, "span": span.map(|(start,end)| json!({"start":start,"end":end}))})
}

fn natural_case(id: &str, text: &str) -> Value {
    case(
        id,
        text,
        json!([[expected("correction", Some((0, text.len())))]]),
    )
}

fn dataset(root: &Path, cases: Vec<Value>) -> PathBuf {
    let path = root.join("dataset.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({"schema_version": 1, "cases": cases})).unwrap(),
    )
    .unwrap();
    path
}

fn primary_reply(parts: Value) -> Reply {
    Reply::json(final_response(
        &json!({"parts": parts, "explanation": "PRIVATE_MODEL_EXPLANATION"}).to_string(),
    ))
}

fn correction_reply(text: &str) -> Reply {
    primary_reply(json!([{"intent": "correction", "confidence": 95, "text": text}]))
}

fn jev_reply(confidence: f64) -> Reply {
    let probabilities: BTreeMap<_, _> = [
        "supplement",
        "correction",
        "answer",
        "new_task",
        "cancel",
        "continue",
        "unrelated",
        "ambiguous",
        "pause",
        "resume",
    ]
    .into_iter()
    .map(|label| (label, if label == "correction" { 1.0 } else { 0.0 }))
    .collect();
    Reply::json(json!({"answers": {
        "intent": {"type": "choice", "choice": "correction", "confidence": confidence, "probabilities": probabilities},
        "whole_message": {"type": "noul", "noul": 1.0}
    }}))
}

fn assert_request(
    request: Captured,
    path: &str,
    model: &str,
    own_key: &str,
    other_key: &str,
) -> Value {
    assert!(
        request
            .headers
            .starts_with(&format!("POST {path} HTTP/1.1\r\n"))
    );
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {own_key}\r\n"))
    );
    assert!(!request.headers.contains(other_key));
    assert_eq!(request.body["model"], model);
    let bytes = request.body.to_string();
    for marker in [
        own_key,
        other_key,
        "evaluation-session",
        "evaluation-user",
        "evaluation-task",
    ] {
        assert!(!bytes.contains(marker));
    }
    request.body
}

fn temporary_is_empty(root: &Path) {
    assert_eq!(
        std::fs::read_dir(root.join("temporary")).unwrap().count(),
        0
    );
}

#[tokio::test]
async fn default_rules_runs_all_public_cases_offline_and_reports_hash_and_utf8_source() {
    let root = tempfile::tempdir().unwrap();
    let mut primary = Server::start(vec![]).await;
    let mut jev = Server::start(vec![]).await;
    let mut cmd = command(root.path());
    cmd.env("EVE_OPENAI_BASE_URL", &primary.url)
        .env("EVE_JEV_BASE_URL", &jev.url)
        .env("EVE_OPENAI_MODEL_ROLE", "unsupported-model-role")
        .env("EVE_MODELS_JEV_ENABLED", "invalid-boolean");
    let result = report(&run(cmd).await);
    assert_eq!(result["schema_version"], 2);
    assert_eq!(result["mode"], "rules");
    assert!(result["started_at_unix_ms"].as_u64().unwrap() > 1_700_000_000_000);
    let config_hash = result["configuration_sha256"].as_str().unwrap();
    assert_eq!(config_hash.len(), 64);
    assert!(config_hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(result["summary"]["cases"], 30);
    assert_eq!(result["summary"]["successful_judgements"], 30);
    assert_eq!(result["summary"]["errors"], 0);
    assert_eq!(result["summary"]["false_cancellations"], 0);
    assert_eq!(result["summary"]["exact_matches"], 16);
    assert_eq!(result["summary"]["missed_corrections"], 4);
    assert_eq!(
        result["summary"]["diagnostic_counts"],
        json!({"rules_started":30,"auxiliary_started":0,"primary_started":0,"classifier_calls":0,"model_provider_calls":0,"fallbacks":0})
    );
    assert_eq!(result["summary"]["incomplete_diagnostics"], 0);
    assert!(
        result["cases"]
            .as_array()
            .unwrap()
            .iter()
            .all(|case| case["diagnostics"]["coverage"] == "complete")
    );
    let bytes = std::fs::read(repository().join("benchmarks/messages/cases.json")).unwrap();
    let digest = ring::digest::digest(&ring::digest::SHA256, &bytes);
    let expected_hash: String = digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(result["dataset_sha256"], expected_hash);
    let utf8 = result["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["id"] == "command_correct_utf8")
        .unwrap();
    assert_eq!(utf8["actual"][0]["raw_text"], "改成两页 📚");
    assert_eq!(utf8["actual"][0]["span"], json!({"start":11,"end":28}));
    assert_eq!(utf8["exact_match"], true);
    assert!(result["summary"].get("requests").is_none());
    assert!(result["summary"].get("fallbacks").is_none());
    assert!(primary.requests.try_recv().is_err());
    assert!(jev.requests.try_recv().is_err());
    let repeated = report(&run(command(root.path())).await);
    assert_eq!(repeated["configuration_sha256"], config_hash);
    assert_eq!(repeated["dataset_sha256"], result["dataset_sha256"]);
    let mut changed = command(root.path());
    changed.env("EVE_MESSAGE_JUDGE_TIMEOUT_MS", "2500");
    let changed = report(&run(changed).await);
    assert_eq!(changed["judge_timeout_ms"], 2500);
    assert_ne!(changed["configuration_sha256"], config_hash);
    assert_eq!(changed["dataset_sha256"], result["dataset_sha256"]);
    temporary_is_empty(root.path());
}

#[tokio::test]
async fn malformed_datasets_fail_before_credentials_network_or_state_creation() {
    let root = tempfile::tempdir().unwrap();
    let text = "修改报告。";
    let valid = json!({"schema_version": 1, "cases": [natural_case("one", text)]});
    let mut variants = Vec::new();
    let mut bad = valid.clone();
    bad["schema_version"] = json!(2);
    variants.push(bad.to_string());
    let mut bad = valid.clone();
    bad["unknown"] = json!("PRIVATE_BAD_DATA");
    variants.push(bad.to_string());
    let mut bad = valid.clone();
    bad["cases"]
        .as_array_mut()
        .unwrap()
        .push(natural_case("one", text));
    variants.push(bad.to_string());
    let mut bad = valid.clone();
    bad["cases"][0]["accepted"][0][0]["span"]["start"] = json!(1);
    variants.push(bad.to_string());
    let mut bad = valid.clone();
    bad["cases"][0]["accepted"][0][0]["span"]["end"] = json!(text.len() + 1);
    variants.push(bad.to_string());
    let mut bad = valid.clone();
    bad["cases"][0]["accepted"][0] = json!([expected("answer", Some((0, text.len())))]);
    variants.push(bad.to_string());
    variants.push(valid.to_string().replacen(
        "\"schema_version\":1",
        "\"schema_version\":1,\"schema_version\":1",
        1,
    ));
    variants.push(
        valid
            .to_string()
            .replacen("\"id\":\"one\"", "\"id\":\"one\",\"id\":\"two\"", 1),
    );
    let mut primary = Server::start(vec![]).await;
    let mut jev = Server::start(vec![]).await;
    for (index, data) in variants.into_iter().enumerate() {
        let path = root.path().join(format!("bad-{index}.json"));
        std::fs::write(&path, data).unwrap();
        let state = root.path().join(format!("state-{index}"));
        let mut cmd = model_command(root.path(), &primary, &jev);
        cmd.args(["--mode", "jev", "--dataset"])
            .arg(path)
            .arg("--state-dir")
            .arg(&state)
            .env_remove("EVE_OPENAI_API_KEY")
            .env_remove("EVE_JEV_API_KEY");
        let output = run(cmd).await;
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("评估样本")
                || error.contains("评估标注")
                || error.contains("评估 answer"),
            "{error}"
        );
        assert!(!error.contains("PRIVATE_BAD_DATA"));
        assert!(!state.exists());
    }
    assert!(primary.requests.try_recv().is_err());
    assert!(jev.requests.try_recv().is_err());
    temporary_is_empty(root.path());
}

#[tokio::test]
async fn explicit_state_directory_and_report_are_created_once_and_existing_data_is_preserved() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("isolated-evaluation");
    let destination = root.path().join("report.json");
    let mut cmd = command(root.path());
    cmd.arg("--state-dir")
        .arg(&state)
        .arg("--output")
        .arg(&destination);
    let output = run(cmd).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(state.join("config").is_dir());
    assert!(!state.join("state.json").exists());
    let original = std::fs::read(&destination).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&original).unwrap()["summary"]["cases"],
        30
    );
    std::fs::write(state.join("keep"), b"existing-state-marker").unwrap();
    let mut primary = Server::start(vec![]).await;
    let mut jev = Server::start(vec![]).await;
    let mut existing_state = model_command(root.path(), &primary, &jev);
    existing_state
        .args(["--mode", "jev", "--state-dir"])
        .arg(&state);
    let rejected = run(existing_state).await;
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("尚不存在"));
    assert_eq!(
        std::fs::read(state.join("keep")).unwrap(),
        b"existing-state-marker"
    );
    let new_state = root.path().join("must-not-be-created");
    let mut existing_report = model_command(root.path(), &primary, &jev);
    existing_report
        .args(["--mode", "jev", "--output"])
        .arg(&destination)
        .arg("--state-dir")
        .arg(&new_state);
    let rejected = run(existing_report).await;
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("拒绝覆盖"));
    assert_eq!(std::fs::read(&destination).unwrap(), original);
    assert!(!new_state.exists());
    assert!(primary.requests.try_recv().is_err());
    assert!(jev.requests.try_recv().is_err());
    temporary_is_empty(root.path());
}

#[cfg(unix)]
#[tokio::test]
async fn dangling_output_symlink_is_rejected_before_any_model_request() {
    let root = tempfile::tempdir().unwrap();
    let text = "请修改报告。";
    let input = dataset(root.path(), vec![natural_case("never_send", text)]);
    let destination = root.path().join("existing-link.json");
    let missing_target = root.path().join("missing-target.json");
    std::os::unix::fs::symlink(&missing_target, &destination).unwrap();
    let mut primary = Server::start(vec![correction_reply(text)]).await;
    let mut jev = Server::start(vec![]).await;
    let state = root.path().join("must-not-be-created");
    let mut cmd = model_command(root.path(), &primary, &jev);
    cmd.args(["--mode", "primary", "--dataset"])
        .arg(input)
        .arg("--output")
        .arg(&destination)
        .arg("--state-dir")
        .arg(&state);
    let output = run(cmd).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("拒绝覆盖"));
    assert_eq!(std::fs::read_link(&destination).unwrap(), missing_target);
    assert!(!missing_target.exists());
    assert!(!state.exists());
    assert!(primary.requests.try_recv().is_err());
    assert!(jev.requests.try_recv().is_err());
    temporary_is_empty(root.path());
}

#[tokio::test]
async fn primary_scoring_distinguishes_labels_spans_cancellation_and_low_confidence() {
    let root = tempfile::tempdir().unwrap();
    let input = dataset(
        root.path(),
        vec![
            natural_case("utf8", "改成中文 📚。"),
            case(
                "span_mismatch",
                "甲乙",
                json!([[expected("correction", Some((0, 3)))]]),
            ),
            natural_case("false_cancel", "不要停止。"),
            natural_case("low_confidence", "请更正。"),
            case(
                "parts_unordered",
                "更正甲，补充乙。",
                json!([[
                    expected("correction", Some((0, 9))),
                    expected("supplement", Some((12, 21)))
                ]]),
            ),
        ],
    );
    let mut primary = Server::start(vec![
        correction_reply("改成中文 📚。"),
        correction_reply("甲乙"),
        primary_reply(json!([{"intent":"cancel","confidence":99,"text":null}])),
        primary_reply(json!([{"intent":"correction","confidence":60,"text":"请更正。"}])),
        primary_reply(json!([
            {"intent":"supplement","confidence":95,"text":"补充乙"},
            {"intent":"correction","confidence":95,"text":"更正甲"}
        ])),
    ])
    .await;
    let mut jev = Server::start(vec![]).await;
    let mut cmd = model_command(root.path(), &primary, &jev);
    cmd.args(["--mode", "primary", "--dataset"])
        .arg(&input)
        .env_remove("EVE_JEV_API_KEY");
    let result = report(&run(cmd).await);
    let summary = &result["summary"];
    assert_eq!(summary["cases"], 5);
    assert_eq!(
        summary["diagnostic_counts"],
        json!({"rules_started":5,"auxiliary_started":0,"primary_started":5,"classifier_calls":0,"model_provider_calls":5,"fallbacks":0})
    );
    assert_eq!(summary["exact_matches"], 3);
    assert_eq!(summary["label_matches"], 4);
    assert_eq!(summary["raw_span_matches"], 3);
    assert_eq!(summary["false_cancellations"], 1);
    assert_eq!(summary["missed_corrections"], 2);
    assert_eq!(summary["low_confidence_or_ambiguous"], 1);
    assert_eq!(result["cases"][0]["actual"][0]["raw_text"], "改成中文 📚。");
    for _ in 0..5 {
        let body = assert_request(
            primary.next().await,
            "/v1/responses",
            "evaluation-primary",
            PRIMARY_KEY,
            JEV_KEY,
        );
        assert_eq!(body["max_output_tokens"], 256);
        assert!(body["tools"].as_array().unwrap().is_empty());
    }
    assert!(primary.requests.try_recv().is_err());
    assert!(jev.requests.try_recv().is_err());
    temporary_is_empty(root.path());
}

#[tokio::test]
async fn jev_uses_its_own_key_and_falls_back_once_for_low_confidence_or_protocol_error() {
    let root = tempfile::tempdir().unwrap();
    let text = "请改成中文。";
    let input = dataset(
        root.path(),
        vec![
            case("explicit", "/cancel", json!([[expected("cancel", None)]])),
            natural_case("jev_success", text),
            natural_case("low_confidence", text),
            natural_case("bad_protocol", text),
        ],
    );
    let mut primary = Server::start(vec![correction_reply(text), correction_reply(text)]).await;
    let mut jev = Server::start(vec![
        jev_reply(0.99),
        jev_reply(0.2),
        Reply::json(json!({"PRIVATE_MODEL_EXPLANATION":true})),
    ])
    .await;
    let mut cmd = model_command(root.path(), &primary, &jev);
    cmd.args(["--mode", "jev", "--dataset"]).arg(input);
    let result = report(&run(cmd).await);
    assert_eq!(result["summary"]["exact_matches"], 4);
    assert_eq!(
        result["summary"]["diagnostic_counts"],
        json!({"rules_started":4,"auxiliary_started":3,"primary_started":2,"classifier_calls":3,"model_provider_calls":2,"fallbacks":2})
    );
    for (index, reason) in [(2, "low_confidence"), (3, "protocol")] {
        let events = result["cases"][index]["diagnostics"]["events"]
            .as_array()
            .unwrap();
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "fallback" && event["reason"] == reason)
        );
    }
    assert_eq!(result["summary"]["errors"], 0);
    for _ in 0..3 {
        let body = assert_request(
            jev.next().await,
            "/v1/systemone",
            "evaluation-jev",
            JEV_KEY,
            PRIMARY_KEY,
        );
        assert_eq!(body["state"]["message"], text);
    }
    for _ in 0..2 {
        assert_request(
            primary.next().await,
            "/v1/responses",
            "evaluation-primary",
            PRIMARY_KEY,
            JEV_KEY,
        );
    }
    assert!(primary.requests.try_recv().is_err());
    assert!(jev.requests.try_recv().is_err());
    temporary_is_empty(root.path());
}

#[tokio::test]
async fn jev_and_primary_share_total_timeout_and_record_the_failed_correction() {
    let root = tempfile::tempdir().unwrap();
    let text = "请修改报告。";
    let input = dataset(root.path(), vec![natural_case("timeout", text)]);
    let mut slow_primary = correction_reply(text);
    slow_primary.body_delay = Duration::from_millis(2000);
    let mut slow_jev = jev_reply(0.99);
    slow_jev.body_delay = Duration::from_millis(2000);
    let mut primary = Server::start(vec![slow_primary]).await;
    let mut jev = Server::start(vec![slow_jev]).await;
    let mut cmd = model_command(root.path(), &primary, &jev);
    cmd.args(["--mode", "jev", "--dataset"])
        .arg(input)
        .env("EVE_MESSAGE_JUDGE_TIMEOUT_MS", "300");
    let result = report(&run(cmd).await);
    assert_eq!(result["judge_timeout_ms"], 300);
    assert_eq!(result["summary"]["errors"], 1);
    assert_eq!(result["summary"]["timeouts"], 1);
    assert_eq!(result["summary"]["missed_corrections"], 1);
    assert_eq!(result["cases"][0]["error"], "timeout");
    assert_eq!(
        result["summary"]["diagnostic_counts"],
        json!({"rules_started":1,"auxiliary_started":1,"primary_started":1,"classifier_calls":1,"model_provider_calls":1,"fallbacks":1})
    );
    let events = result["cases"][0]["diagnostics"]["events"]
        .as_array()
        .unwrap();
    assert!(events.iter().any(|event| event["event"] == "finished"
        && event["operation"]["kind"] == "attempt"
        && event["operation"]["name"] == "classifier_call"
        && event["outcome"] == "dropped"));
    assert!(events.iter().any(|event| event["event"] == "finished"
        && event["operation"]["kind"] == "stage"
        && event["operation"]["name"] == "auxiliary"
        && event["outcome"] == "timeout"));
    assert!(result["cases"][0]["actual"].as_array().unwrap().is_empty());
    let elapsed = result["cases"][0]["latency_ms"].as_f64().unwrap();
    assert!((250.0..1500.0).contains(&elapsed), "{elapsed} ms");
    assert_request(
        jev.next().await,
        "/v1/systemone",
        "evaluation-jev",
        JEV_KEY,
        PRIMARY_KEY,
    );
    assert_request(
        primary.next().await,
        "/v1/responses",
        "evaluation-primary",
        PRIMARY_KEY,
        JEV_KEY,
    );
    assert!(primary.requests.try_recv().is_err());
    assert!(jev.requests.try_recv().is_err());
    temporary_is_empty(root.path());
}
