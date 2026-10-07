use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::{ChatMessage, ChatRole};
use eve_memory_api::{
    CompletedInteraction, MEMORY_PLUGIN_ID, MemoryAdmin, MemoryScope, PreferenceAction,
    PreferenceChange, PreferenceEvidence, UserStatement,
};
use eve_memory_plugin::{MEMORY_STATE_KEY, MemoryPlugin};
use eve_plugin_api::{PluginId, ServiceId, StateStore};
use eve_session_api::{SESSION_SERVICE_ID, SessionInput, SessionKey, SessionServiceHandle};
use eve_session_plugin::SessionPlugin;
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    io::{Read, Seek, SeekFrom},
    path::Path,
    process::{Child, Command, Output, Stdio},
    sync::Arc,
    time::Duration,
};

const CHANNEL: &str = "qq-memory-fixture";
const SESSION: &str = "same-session";
const INITIAL: &str = "ALICE_INITIAL_PRIVATE：先给结论。";
const REVOKE: &str = "ALICE_REVOCATION_PRIVATE：撤销这项偏好。";
const BOB: &str = "BOB_ONLY_PRIVATE：这是另一个用户的偏好。";

fn scope(user: &str) -> MemoryScope {
    MemoryScope {
        channel: CHANNEL.into(),
        session_id: SESSION.into(),
        user_id: user.into(),
    }
}

fn long_preference() -> String {
    format!("ALICE_CORRECTION_PRIVATE：{}", "保留必要细节。".repeat(120))
}

fn user_text() -> String {
    format!(
        "ALICE_INTERACTION_PRIVATE：{}",
        "原始用户输入。".repeat(120)
    )
}

fn assistant_text() -> String {
    format!(
        "ASSISTANT_REPLY_PRIVATE：{}",
        "模型回答并非独立验证。".repeat(120)
    )
}

async fn seed(directory: &Path) {
    let store = Arc::new(FileStateStore::open(directory).unwrap());
    let backends = KernelServices {
        state: store,
        ..KernelServices::default()
    };
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    let plugin = MemoryPlugin::new().unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel
        .register(Box::new(SessionPlugin::new().unwrap()))
        .unwrap();
    kernel.start_all().await.unwrap();
    let sessions = registry
        .get(&ServiceId::new(SESSION_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<SessionServiceHandle>()
        .unwrap()
        .0
        .clone();
    let alice = scope("alice");
    let key = SessionKey::new(&alice.session_id, &alice.user_id).unwrap();
    let input = user_text();
    let turn = sessions
        .begin(SessionInput {
            key: key.clone(),
            text: input.clone(),
        })
        .unwrap();
    sessions
        .complete(
            &turn.lease,
            vec![
                ChatMessage::text(ChatRole::User, input),
                ChatMessage::text(ChatRole::Assistant, assistant_text()),
            ],
        )
        .unwrap();
    admin
        .import_completed(
            &alice,
            0,
            CompletedInteraction {
                evidence_id: "interaction".into(),
                message_id: "message-interaction".into(),
                at_ms: 1,
                snapshot: sessions.snapshot(&key).unwrap().unwrap(),
                turn_id: turn.lease.turn_id,
            },
        )
        .unwrap();
    admin
        .update_preference(
            &alice,
            1,
            PreferenceChange {
                operation_id: "confirm-style".into(),
                at_ms: 2,
                evidence: PreferenceEvidence::Existing("interaction".into()),
                action: PreferenceAction::Confirm {
                    id: "style".into(),
                    text: INITIAL.into(),
                },
            },
        )
        .unwrap();
    admin
        .update_preference(
            &alice,
            2,
            PreferenceChange {
                operation_id: "correct-style".into(),
                at_ms: 3,
                evidence: PreferenceEvidence::Statement(UserStatement {
                    evidence_id: "correction".into(),
                    message_id: "message-correction".into(),
                    text: long_preference(),
                    at_ms: 3,
                }),
                action: PreferenceAction::Correct {
                    id: "style".into(),
                    text: long_preference(),
                },
            },
        )
        .unwrap();
    admin
        .update_preference(
            &alice,
            3,
            PreferenceChange {
                operation_id: "revoke-style".into(),
                at_ms: 4,
                evidence: PreferenceEvidence::Statement(UserStatement {
                    evidence_id: "revocation".into(),
                    message_id: "message-revocation".into(),
                    text: REVOKE.into(),
                    at_ms: 4,
                }),
                action: PreferenceAction::Revoke { id: "style".into() },
            },
        )
        .unwrap();
    admin
        .update_preference(
            &alice,
            4,
            PreferenceChange {
                operation_id: "confirm-active".into(),
                at_ms: 5,
                evidence: PreferenceEvidence::Existing("interaction".into()),
                action: PreferenceAction::Confirm {
                    id: "active".into(),
                    text: "ALICE_ACTIVE_PRIVATE：保持回答简洁。".into(),
                },
            },
        )
        .unwrap();
    admin
        .update_preference(
            &scope("bob"),
            0,
            PreferenceChange {
                operation_id: "confirm-style".into(),
                at_ms: 6,
                evidence: PreferenceEvidence::Statement(UserStatement {
                    evidence_id: "interaction".into(),
                    message_id: "message-interaction".into(),
                    text: BOB.into(),
                    at_ms: 6,
                }),
                action: PreferenceAction::Confirm {
                    id: "style".into(),
                    text: BOB.into(),
                },
            },
        )
        .unwrap();
    kernel.stop_all().await.unwrap();
    kernel.flush_logs().unwrap();
}

fn command(directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_eve-memory"));
    command.arg("--state-dir").arg(directory);
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(|value| value.starts_with("EVE_")) {
            command.env_remove(name);
        }
    }
    command
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
    // 输出写入本测试持有的临时文件，避免分页结果填满管道后误触发看门狗。
    let mut stdout = tempfile::tempfile().unwrap();
    let mut stderr = tempfile::tempfile().unwrap();
    let mut process = Process(Some(
        command
            .stdin(Stdio::null())
            .stdout(stdout.try_clone().unwrap())
            .stderr(stderr.try_clone().unwrap())
            .spawn()
            .unwrap(),
    ));
    tokio::time::timeout(Duration::from_secs(15), async {
        while process.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("只读记忆子进程未在看门狗窗口内退出");
    let status = process.0.take().unwrap().wait().unwrap();
    stdout.seek(SeekFrom::Start(0)).unwrap();
    stderr.seek(SeekFrom::Start(0)).unwrap();
    let mut output = Output {
        status,
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    stdout.read_to_end(&mut output.stdout).unwrap();
    stderr.read_to_end(&mut output.stderr).unwrap();
    output
}

async fn invoke(directory: &Path, identity: Option<&MemoryScope>, args: &[&str]) -> Value {
    let mut command = command(directory);
    if let Some(scope) = identity {
        command.args([
            "--channel",
            &scope.channel,
            "--session",
            &scope.session_id,
            "--user",
            &scope.user_id,
        ]);
    }
    command.args(args);
    let output = run(command).await;
    assert!(
        output.status.success(),
        "args={args:?}; {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["format_version"], 1);
    assert_eq!(value["command"], args[0]);
    assert_eq!(value["read_only"], true);
    if let Some(scope) = identity {
        assert_eq!(value["scope"], serde_json::to_value(scope).unwrap());
    }
    value
}

fn assert_no_content(value: &Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                assert!(
                    !["text", "user_text", "assistant_text", "content"].contains(&key.as_str()),
                    "默认输出包含正文键 {key}"
                );
                assert_no_content(value);
            }
        }
        Value::Array(values) => values.iter().for_each(assert_no_content),
        Value::String(value) => assert!(!value.contains("PRIVATE")),
        _ => {}
    }
}

fn assert_preview(content: &Value, original: &str) {
    let text = content["text"].as_str().unwrap();
    assert!(text.len() <= 512);
    assert!(original.starts_with(text));
    assert_eq!(content["original_bytes"], original.len());
    assert_eq!(content["truncated"], original.len() > text.len());
    if original.len() <= 512 {
        assert_eq!(text, original);
    } else {
        assert!(text.len() >= 509, "UTF-8 预览错误地提前截断");
    }
}

#[tokio::test]
async fn views_preserve_revision_history_provenance_and_all_scope_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    seed(directory.path()).await;
    let before = std::fs::read(directory.path().join("state.json")).unwrap();
    let alice = scope("alice");
    let status = invoke(directory.path(), Some(&alice), &["status"]).await;
    assert_eq!(status["revision"], 5);
    assert_eq!(status["evidence_count"], 3);
    assert_eq!(status["preference_count"], 2);
    assert_eq!(status["confirmed_count"], 1);
    assert_eq!(status["revoked_count"], 1);
    assert_eq!(status["history_count"], 4);
    assert_no_content(&status);

    let list = invoke(directory.path(), Some(&alice), &["list"]).await;
    assert_eq!(list["total"], 2);
    assert_eq!(list["page"], 1);
    assert_eq!(list["page_size"], 20);
    assert_eq!(list["pages"], 1);
    assert!(list["next_page"].is_null());
    assert_eq!(list["preferences"][0]["id"], "active");
    assert_eq!(list["preferences"][0]["current_effective"], true);
    assert_eq!(list["preferences"][1]["id"], "style");
    assert_eq!(list["preferences"][1]["status"], "Revoked");
    assert_eq!(list["preferences"][1]["current_effective"], false);
    assert_no_content(&list);

    let show = invoke(directory.path(), Some(&alice), &["show", "--id", "style"]).await;
    assert_eq!(show["preference"]["revision"], 3);
    assert_eq!(show["preference"]["current_evidence_id"], "revocation");
    assert_eq!(show["history_order"], "revision_ascending");
    assert_eq!(show["history"].as_array().unwrap().len(), 3);
    for (index, row) in show["history"].as_array().unwrap().iter().enumerate() {
        assert_eq!(row["revision"], index + 1);
        assert_eq!(row["is_current"], index == 2);
        assert_eq!(row["current_effective"], false);
        assert_eq!(row["evidence"]["source_present"], true);
    }
    assert_eq!(
        show["history"][0]["evidence"]["source"]["kind"],
        "CompletedInteraction"
    );
    assert_eq!(
        show["history"][1]["evidence"]["source"]["kind"],
        "UserStatement"
    );
    assert_no_content(&show);

    let evidence = invoke(
        directory.path(),
        Some(&alice),
        &["evidence", "--id", "interaction"],
    )
    .await;
    assert_eq!(
        evidence["evidence"]["source"]["kind"],
        "CompletedInteraction"
    );
    assert_eq!(evidence["evidence"]["source"]["turn_id"], 1);
    assert_eq!(evidence["references"]["total"], 2);
    assert_eq!(
        evidence["references"]["items"][0]["preference_id"],
        "active"
    );
    assert_eq!(
        evidence["references"]["items"][0]["current_effective"],
        true
    );
    assert_eq!(evidence["references"]["items"][1]["preference_id"], "style");
    assert_eq!(
        evidence["references"]["items"][1]["current_effective"],
        false
    );
    assert_no_content(&evidence);

    let bob = invoke(
        directory.path(),
        Some(&scope("bob")),
        &["show", "--id", "style", "--include-content"],
    )
    .await;
    assert_eq!(bob["revision"], 1);
    assert_eq!(bob["preference"]["status"], "Confirmed");
    assert_preview(&bob["preference"]["content"], BOB);
    assert!(!bob.to_string().contains("ALICE_"));
    let bob_evidence = invoke(
        directory.path(),
        Some(&scope("bob")),
        &["evidence", "--id", "interaction", "--include-content"],
    )
    .await;
    assert_eq!(bob_evidence["evidence"]["source"]["kind"], "UserStatement");
    assert_preview(&bob_evidence["evidence"]["source"]["content"], BOB);
    for other in [
        MemoryScope {
            channel: "other-channel".into(),
            ..alice.clone()
        },
        MemoryScope {
            session_id: "other-session".into(),
            ..alice.clone()
        },
        scope("unknown-user"),
    ] {
        let empty = invoke(directory.path(), Some(&other), &["list"]).await;
        assert_eq!(empty["revision"], 0);
        assert_eq!(empty["total"], 0);
        assert_eq!(empty["pages"], 0);
        assert!(empty["preferences"].as_array().unwrap().is_empty());
    }
    let scopes = invoke(directory.path(), None, &["scopes"]).await;
    assert_eq!(scopes["total"], 2, "只读未知作用域不得初始化持久记录");
    assert_eq!(scopes["scopes"], json!([scope("alice"), scope("bob")]));
    assert_no_content(&scopes);
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        before
    );
}

#[tokio::test]
async fn explicit_previews_are_utf8_bounded_and_keep_user_and_assistant_separate() {
    let directory = tempfile::tempdir().unwrap();
    seed(directory.path()).await;
    let before = std::fs::read(directory.path().join("state.json")).unwrap();
    let alice = scope("alice");
    let show = invoke(
        directory.path(),
        Some(&alice),
        &["show", "--id", "style", "--include-content"],
    )
    .await;
    assert_eq!(show["content_included"], true);
    assert_preview(&show["preference"]["content"], &long_preference());
    assert_preview(&show["history"][0]["content"], INITIAL);
    assert_preview(&show["history"][1]["content"], &long_preference());
    assert_preview(&show["history"][2]["content"], &long_preference());
    for row in show["history"].as_array().unwrap() {
        assert_no_content(&row["evidence"]);
    }
    let interaction = invoke(
        directory.path(),
        Some(&alice),
        &["evidence", "--id", "interaction", "--include-content"],
    )
    .await;
    assert_preview(
        &interaction["evidence"]["source"]["user"]["content"],
        &user_text(),
    );
    assert_preview(
        &interaction["evidence"]["source"]["assistant"]["content"],
        &assistant_text(),
    );
    assert!(!interaction.to_string().contains("BOB_ONLY_PRIVATE"));
    let correction = invoke(
        directory.path(),
        Some(&alice),
        &["evidence", "--id", "correction", "--include-content"],
    )
    .await;
    assert_preview(
        &correction["evidence"]["source"]["content"],
        &long_preference(),
    );
    let list = invoke(
        directory.path(),
        Some(&alice),
        &["list", "--include-content"],
    )
    .await;
    assert_preview(&list["preferences"][1]["content"], &long_preference());
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        before
    );
}

#[tokio::test]
async fn pagination_is_stable_and_out_of_range_requests_preserve_state() {
    let directory = tempfile::tempdir().unwrap();
    seed(directory.path()).await;
    let before = std::fs::read(directory.path().join("state.json")).unwrap();
    let alice = scope("alice");
    let first = invoke(
        directory.path(),
        Some(&alice),
        &["list", "--page-size", "1"],
    )
    .await;
    assert_eq!(first["preferences"][0]["id"], "active");
    assert_eq!(first["pages"], 2);
    assert_eq!(first["next_page"], 2);
    let second = invoke(
        directory.path(),
        Some(&alice),
        &["list", "--page-size", "1", "--page", "2"],
    )
    .await;
    assert_eq!(second["preferences"][0]["id"], "style");
    assert!(second["next_page"].is_null());
    let history = invoke(
        directory.path(),
        Some(&alice),
        &["show", "--id", "style", "--page-size", "1", "--page", "3"],
    )
    .await;
    assert_eq!(history["total"], 3);
    assert_eq!(history["history"][0]["revision"], 3);
    assert_eq!(history["history"][0]["status"], "Revoked");
    let scopes = invoke(
        directory.path(),
        None,
        &["scopes", "--page-size", "1", "--page", "2"],
    )
    .await;
    assert_eq!(scopes["scopes"], json!([scope("bob")]));
    for args in [
        vec!["list", "--page", "2"],
        vec![
            "list",
            "--page",
            "18446744073709551615",
            "--page-size",
            "50",
        ],
        vec!["show", "--id", "style", "--page-size", "1", "--page", "4"],
        vec!["show", "--id", "does-not-exist"],
        vec!["evidence", "--id", "does-not-exist"],
    ] {
        let mut command = command(directory.path());
        command.args([
            "--channel",
            CHANNEL,
            "--session",
            SESSION,
            "--user",
            "alice",
        ]);
        command.args(&args);
        let output = run(command).await;
        assert!(!output.status.success(), "错误参数应拒绝 {args:?}");
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE"));
    }
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        before
    );
}

#[test]
fn parser_rejects_missing_unknown_duplicate_or_inapplicable_arguments() {
    let base = [
        "--state-dir",
        "/unused",
        "--channel",
        CHANNEL,
        "--session",
        SESSION,
        "--user",
        "alice",
    ];
    for invalid in [
        vec![],
        vec!["invented"],
        vec!["show"],
        vec!["evidence"],
        vec!["status", "list"],
        vec!["status", "--include-content"],
        vec!["status", "--page", "1"],
        vec!["status", "--id", "style"],
        vec!["status", "--unknown", "value"],
        vec!["list", "--include-content", "--include-content"],
        vec!["list", "--page", "1", "--page", "2"],
        vec!["list", "--page-size", "20", "--page-size", "10"],
        vec!["list", "--page"],
        vec!["list", "--page", ""],
        vec!["list", "--page", "0"],
        vec!["list", "--page", "-1"],
        vec!["list", "--page", "+1"],
        vec!["list", "--page", "1.5"],
        vec!["list", "--page", "184467440737095516160"],
        vec!["list", "--page-size", "0"],
        vec!["list", "--page-size", "51"],
        vec!["list", "--page-size", "false"],
        vec!["show", "--id", "style", "--id", "style"],
        vec!["show", "--id", " spaced "],
        vec!["evidence", "--id", "interaction", "--page", "1"],
        vec!["status", "--state-dir", "/duplicate"],
        vec!["status", "--channel", "duplicate"],
        vec!["status", "--session", "duplicate"],
        vec!["status", "--user", "duplicate"],
        vec!["status", "--database-config"],
        vec!["status", "--database-config", "a", "--database-config", "b"],
        vec!["scopes"],
    ] {
        let args = base
            .iter()
            .copied()
            .chain(invalid.iter().copied())
            .map(OsString::from);
        assert!(
            eve_app::MemoryCliOptions::parse(args).is_err(),
            "应拒绝 {invalid:?}"
        );
    }
    for args in [
        vec!["status"],
        vec!["--state-dir", "/unused", "status"],
        vec!["--state-dir", "/unused", "--channel", CHANNEL, "status"],
        vec![
            "--state-dir",
            "/unused",
            "--channel",
            CHANNEL,
            "--session",
            SESSION,
            "status",
        ],
        vec!["--state-dir", "/unused", "scopes", "--include-content"],
        vec![
            "--state-dir",
            "/unused",
            "--channel",
            CHANNEL,
            "--session",
            SESSION,
            "--user",
            "--id",
            "status",
        ],
        vec!["--state-dir", "--database-config", "scopes"],
        vec![
            "--state-dir",
            "/unused",
            "--channel",
            CHANNEL,
            "--session",
            SESSION,
            "--user",
            "alice",
            "--database-config",
            "--id",
            "status",
        ],
    ] {
        assert!(eve_app::MemoryCliOptions::parse(args.into_iter().map(OsString::from)).is_err());
    }
    assert!(
        eve_app::MemoryCliOptions::parse([OsString::from("--help")])
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn readonly_commands_refuse_new_directories_and_allow_existing_unrelated_state() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing");
    let mut missing_command = command(&missing);
    missing_command.arg("scopes");
    assert!(!run(missing_command).await.status.success());
    assert!(!missing.exists());
    let empty = root.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    let mut empty_command = command(&empty);
    empty_command.arg("scopes");
    assert!(!run(empty_command).await.status.success());
    assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0);

    let existing = root.path().join("existing");
    let store = FileStateStore::open(&existing).unwrap();
    store
        .set(
            &PluginId::new("fixture.unrelated").unwrap(),
            "untouched".into(),
            b"unrelated bytes".to_vec(),
        )
        .unwrap();
    drop(store);
    let before = std::fs::read(existing.join("state.json")).unwrap();
    let scopes = invoke(&existing, None, &["scopes"]).await;
    assert_eq!(scopes["total"], 0);
    let status = invoke(&existing, Some(&scope("alice")), &["status"]).await;
    assert_eq!(status["revision"], 0);
    assert_eq!(status["preference_count"], 0);
    assert_eq!(std::fs::read(existing.join("state.json")).unwrap(), before);
}

#[tokio::test]
async fn corrupt_memory_is_rejected_without_clearing_or_rewriting_bytes() {
    let directory = tempfile::tempdir().unwrap();
    seed(directory.path()).await;
    let store = FileStateStore::open(directory.path()).unwrap();
    store
        .set(
            &PluginId::new(MEMORY_PLUGIN_ID).unwrap(),
            MEMORY_STATE_KEY.into(),
            b"corrupt memory private body".to_vec(),
        )
        .unwrap();
    drop(store);
    let before = std::fs::read(directory.path().join("state.json")).unwrap();
    for _ in 0..2 {
        let mut command = command(directory.path());
        command.arg("scopes");
        let output = run(command).await;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private body"));
        assert_eq!(
            std::fs::read(directory.path().join("state.json")).unwrap(),
            before
        );
    }
}

#[tokio::test]
async fn active_backend_lock_is_respected_and_released_for_next_reader() {
    let directory = tempfile::tempdir().unwrap();
    seed(directory.path()).await;
    let before = std::fs::read(directory.path().join("state.json")).unwrap();
    let store = FileStateStore::open(directory.path()).unwrap();
    let mut command = command(directory.path());
    command.arg("scopes");
    let output = run(command).await;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    drop(store);
    let report = invoke(directory.path(), None, &["scopes"]).await;
    assert_eq!(report["total"], 2);
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        before
    );
}

#[tokio::test]
async fn evidence_references_report_truncation_at_the_fixed_limit() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(FileStateStore::open(directory.path()).unwrap());
    let kernel = Kernel::with_services(KernelServices {
        state: store,
        ..KernelServices::default()
    });
    let plugin = MemoryPlugin::new().unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let alice = scope("alice");
    for index in 0..51 {
        admin
            .update_preference(
                &alice,
                index,
                PreferenceChange {
                    operation_id: format!("confirm-{index:02}"),
                    at_ms: index + 1,
                    evidence: if index == 0 {
                        PreferenceEvidence::Statement(UserStatement {
                            evidence_id: "shared-evidence".into(),
                            message_id: "shared-message".into(),
                            text: "REF_PRIVATE：共同证据".into(),
                            at_ms: 1,
                        })
                    } else {
                        PreferenceEvidence::Existing("shared-evidence".into())
                    },
                    action: PreferenceAction::Confirm {
                        id: format!("preference-{index:02}"),
                        text: format!("REF_PRIVATE：偏好{index}"),
                    },
                },
            )
            .unwrap();
    }
    kernel.stop_all().await.unwrap();
    kernel.flush_logs().unwrap();
    drop(admin);
    drop(kernel);
    let before = std::fs::read(directory.path().join("state.json")).unwrap();
    let report = invoke(
        directory.path(),
        Some(&alice),
        &["evidence", "--id", "shared-evidence"],
    )
    .await;
    assert_eq!(report["references"]["total"], 51);
    assert_eq!(report["references"]["limit"], 50);
    assert_eq!(report["references"]["truncated"], true);
    assert_eq!(report["references"]["items"].as_array().unwrap().len(), 50);
    assert_eq!(
        report["references"]["items"][0]["preference_id"],
        "preference-00"
    );
    assert_eq!(
        report["references"]["items"][49]["preference_id"],
        "preference-49"
    );
    assert_no_content(&report);
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        before
    );
}
