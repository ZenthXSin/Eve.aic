use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::{ChatMessage, ChatRole};
use eve_memory_api::{
    CompletedInteraction, MemoryAdmin, MemoryScope, PreferenceAction, PreferenceChange,
    PreferenceEvidence, UserStatement,
};
use eve_memory_plugin::MemoryPlugin;
use eve_plugin_api::ServiceId;
use eve_session_api::{SESSION_SERVICE_ID, SessionInput, SessionKey, SessionServiceHandle};
use eve_session_plugin::SessionPlugin;
use serde_json::Value;
use std::{
    ffi::OsString,
    io::{Read, Seek, SeekFrom},
    path::Path,
    process::{Child, Command, Output, Stdio},
    sync::Arc,
    time::Duration,
};

const CHANNEL: &str = "recall-fixture";
const SESSION: &str = "same-session";
const OLD_PREFERENCE: &str = "ALICE_PRIVATE obsoleteonly 已被更正的旧偏好。";
const ARCHIVE_USER: &str = "USER_PRIVATE archiveonly 过去的用户消息，仍是历史交互来源。";
const ARCHIVE_ASSISTANT: &str = "ASSISTANT_PRIVATE archiveanswer 过去的回答不是独立验证事实。";

fn scope(user: &str) -> MemoryScope {
    MemoryScope {
        channel: CHANNEL.into(),
        session_id: SESSION.into(),
        user_id: user.into(),
    }
}

fn current_preference() -> String {
    format!(
        "ALICE_CURRENT_PRIVATE {} currentword 知识图谱 İSTANBUL {}",
        "前置背景。".repeat(80),
        "后续细节。".repeat(80)
    )
}

fn statement(id: &str, text: &str, at_ms: u64) -> PreferenceEvidence {
    PreferenceEvidence::Statement(UserStatement {
        evidence_id: id.into(),
        message_id: format!("message-{id}"),
        text: text.into(),
        at_ms,
    })
}

async fn seed(directory: &Path) {
    let services = KernelServices {
        state: Arc::new(FileStateStore::open(directory).unwrap()),
        ..KernelServices::default()
    };
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
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
    let turn = sessions
        .begin(SessionInput {
            key: key.clone(),
            text: ARCHIVE_USER.into(),
        })
        .unwrap();
    sessions
        .complete(
            &turn.lease,
            vec![
                ChatMessage::text(ChatRole::User, ARCHIVE_USER),
                ChatMessage::text(ChatRole::Assistant, ARCHIVE_ASSISTANT),
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
                    text: OLD_PREFERENCE.into(),
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
                evidence: statement("correction", &current_preference(), 3),
                action: PreferenceAction::Correct {
                    id: "style".into(),
                    text: current_preference(),
                },
            },
        )
        .unwrap();
    admin
        .update_preference(
            &alice,
            3,
            PreferenceChange {
                operation_id: "confirm-revoked".into(),
                at_ms: 4,
                evidence: statement(
                    "withdrawn-statement",
                    "WITHDRAWN_PRIVATE withdrawnonly 原确认内容。",
                    4,
                ),
                action: PreferenceAction::Confirm {
                    id: "revoked".into(),
                    text: "WITHDRAWN_PRIVATE withdrawnonly 已撤销偏好。".into(),
                },
            },
        )
        .unwrap();
    admin
        .update_preference(
            &alice,
            4,
            PreferenceChange {
                operation_id: "revoke".into(),
                at_ms: 5,
                evidence: statement(
                    "revocation",
                    "REVOCATION_PRIVATE withdrawalcommand 撤销该偏好。",
                    5,
                ),
                action: PreferenceAction::Revoke {
                    id: "revoked".into(),
                },
            },
        )
        .unwrap();
    for index in 0..9 {
        admin
            .update_preference(
                &alice,
                5 + index,
                PreferenceChange {
                    operation_id: format!("confirm-limit-{index}"),
                    at_ms: 6 + index,
                    evidence: PreferenceEvidence::Existing("interaction".into()),
                    action: PreferenceAction::Confirm {
                        id: format!("limit-{index:02}"),
                        text: format!("LIMIT_PRIVATE limitprobe 记录 {index}"),
                    },
                },
            )
            .unwrap();
    }
    admin
        .update_preference(
            &scope("bob"),
            0,
            PreferenceChange {
                operation_id: "confirm-style".into(),
                at_ms: 1,
                evidence: statement("interaction", "BOB_ONLY_PRIVATE currentword", 1),
                action: PreferenceAction::Confirm {
                    id: "style".into(),
                    text: "BOB_ONLY_PRIVATE currentword".into(),
                },
            },
        )
        .unwrap();
    kernel.stop_all().await.unwrap();
    kernel.flush_logs().unwrap();
}

fn command(directory: &Path, scope: &MemoryScope) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_eve-memory"));
    command.arg("--state-dir").arg(directory).args([
        "--channel",
        &scope.channel,
        "--session",
        &scope.session_id,
        "--user",
        &scope.user_id,
    ]);
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(|name| name.starts_with("EVE_")) {
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
    .expect("本测试持有的记忆召回子进程未在看门狗窗口内退出");
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

async fn recall(directory: &Path, scope: &MemoryScope, query: &str, options: &[&str]) -> Value {
    let mut command = command(directory, scope);
    command.args(["recall", "--query", query]).args(options);
    let output = run(command).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["format_version"], 1);
    assert_eq!(report["command"], "recall");
    assert_eq!(report["read_only"], true);
    assert_eq!(report["ranking"], "lexical");
    assert_eq!(report["score_is_confidence"], false);
    assert_eq!(
        report["hit_count"],
        report["hits"].as_array().unwrap().len()
    );
    assert_eq!(report["scope"], serde_json::to_value(scope).unwrap());
    assert!(report.get("query").is_none(), "查询内容不应回显");
    report
}

fn no_content(value: &Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                assert!(
                    ![
                        "query",
                        "excerpt",
                        "text",
                        "content",
                        "user_text",
                        "assistant_text"
                    ]
                    .contains(&key.as_str())
                );
                no_content(value);
            }
        }
        Value::Array(values) => values.iter().for_each(no_content),
        Value::String(value) => assert!(!value.contains("PRIVATE")),
        _ => {}
    }
}

#[tokio::test]
async fn recall_uses_current_confirmed_versions_and_explicit_historical_sources() {
    let directory = tempfile::tempdir().unwrap();
    seed(directory.path()).await;
    let before = std::fs::read(directory.path().join("state.json")).unwrap();
    let alice = scope("alice");
    let current = recall(directory.path(), &alice, "currentword", &[]).await;
    assert_eq!(current["revision"], 14);
    assert_eq!(current["content_included"], false);
    let hits = current["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["source"]["kind"], "ConfirmedPreference");
    assert_eq!(hits[0]["source"]["preference_id"], "style");
    assert_eq!(hits[0]["source"]["preference_revision"], 2);
    assert_eq!(hits[0]["source"]["evidence_id"], "correction");
    assert_eq!(hits[0]["source"]["evidence_revision"], 3);
    assert!(hits[0]["score"].as_u64().unwrap() > 0);
    assert!(hits[0]["excerpt_bytes"].as_u64().unwrap() <= 512);
    assert_eq!(hits[0]["excerpt_truncated"], true);
    no_content(&current);
    for query in ["obsoleteonly", "withdrawnonly", "withdrawalcommand"] {
        assert!(
            recall(directory.path(), &alice, query, &["--include-content"]).await["hits"]
                .as_array()
                .unwrap()
                .is_empty(),
            "旧偏好/撤销来源不得成为当前可召回偏好: {query}"
        );
    }
    for (query, field, text) in [
        ("archiveonly", "User", ARCHIVE_USER),
        ("archiveanswer", "Assistant", ARCHIVE_ASSISTANT),
    ] {
        let report = recall(directory.path(), &alice, query, &["--include-content"]).await;
        assert_eq!(report["hits"].as_array().unwrap().len(), 1);
        let hit = &report["hits"][0];
        assert_eq!(hit["source"]["kind"], "CompletedInteraction");
        assert_eq!(hit["source"]["field"], field);
        assert_eq!(hit["source"]["evidence_id"], "interaction");
        assert_eq!(hit["source"]["turn_id"], 1);
        assert_eq!(hit["content"]["text"], text);
        assert_eq!(hit["content"]["truncated"], false);
        assert!(hit["source"].get("preference_id").is_none());
    }
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        before
    );
}

#[tokio::test]
async fn recall_is_bound_to_all_scope_fields_and_never_initializes_missing_scopes() {
    let directory = tempfile::tempdir().unwrap();
    seed(directory.path()).await;
    let before = std::fs::read(directory.path().join("state.json")).unwrap();
    let alice = scope("alice");
    let bob = recall(
        directory.path(),
        &scope("bob"),
        "currentword",
        &["--include-content"],
    )
    .await;
    assert_eq!(bob["revision"], 1);
    assert_eq!(bob["hits"].as_array().unwrap().len(), 1);
    assert_eq!(bob["hits"][0]["source"]["preference_revision"], 1);
    assert_eq!(
        bob["hits"][0]["content"]["text"],
        "BOB_ONLY_PRIVATE currentword"
    );
    assert!(!bob.to_string().contains("ALICE_"));
    let current = recall(
        directory.path(),
        &alice,
        "currentword",
        &["--include-content"],
    )
    .await;
    assert!(!current.to_string().contains("BOB_ONLY_PRIVATE"));
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
        let empty = recall(directory.path(), &other, "currentword", &[]).await;
        assert_eq!(empty["revision"], 0);
        assert!(empty["hits"].as_array().unwrap().is_empty());
    }
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        before
    );
}

#[tokio::test]
async fn recalled_unicode_excerpts_are_bounded_and_results_are_stable_across_restarts() {
    let directory = tempfile::tempdir().unwrap();
    seed(directory.path()).await;
    let before = std::fs::read(directory.path().join("state.json")).unwrap();
    let alice = scope("alice");
    for query in ["知识图谱", "İSTANBUL"] {
        let first = recall(directory.path(), &alice, query, &["--include-content"]).await;
        assert_eq!(first["hits"].as_array().unwrap().len(), 1);
        let hit = &first["hits"][0];
        let excerpt = hit["content"]["text"].as_str().unwrap();
        assert!(excerpt.len() <= 512);
        assert!(excerpt.contains(query));
        // 词法召回在被裁掉的前后文处添加省略号；正文仍必须直接来自原文。
        assert!(current_preference().contains(excerpt.trim_matches('…')));
        assert_eq!(hit["excerpt_bytes"], excerpt.len());
        assert_eq!(hit["excerpt_truncated"], true);
        assert_eq!(hit["content"]["truncated"], true);
        assert!(hit["content"].get("original_bytes").is_none());
        assert!(hit.get("excerpt").is_none());
        let reopened = recall(directory.path(), &alice, query, &["--include-content"]).await;
        assert_eq!(reopened, first);
    }
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        before
    );
}

#[tokio::test]
async fn recall_limits_are_applied_without_changing_the_saved_state() {
    let directory = tempfile::tempdir().unwrap();
    seed(directory.path()).await;
    let before = std::fs::read(directory.path().join("state.json")).unwrap();
    let alice = scope("alice");
    let default = recall(directory.path(), &alice, "limitprobe", &[]).await;
    assert_eq!(default["limit"], 5);
    assert_eq!(default["hits"].as_array().unwrap().len(), 5);
    let one = recall(directory.path(), &alice, "limitprobe", &["--limit", "1"]).await;
    let eight = recall(directory.path(), &alice, "limitprobe", &["--limit", "8"]).await;
    assert_eq!(one["hits"].as_array().unwrap().len(), 1);
    assert_eq!(eight["hits"].as_array().unwrap().len(), 8);
    assert_eq!(one["hits"][0], default["hits"][0]);
    assert_eq!(
        &eight["hits"].as_array().unwrap()[..5],
        default["hits"].as_array().unwrap()
    );
    no_content(&eight);
    let maximum_query = "x".repeat(1024);
    assert!(
        recall(directory.path(), &alice, &maximum_query, &[]).await["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let unicode_boundary = format!("{}a", "界".repeat(341));
    assert_eq!(unicode_boundary.len(), 1024);
    recall(directory.path(), &alice, &unicode_boundary, &[]).await;
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        before
    );
}

#[tokio::test]
async fn recall_rejects_bad_query_and_limit_without_echoing_query_or_touching_storage() {
    let directory = tempfile::tempdir().unwrap();
    seed(directory.path()).await;
    let before = std::fs::read(directory.path().join("state.json")).unwrap();
    let oversized = "x".repeat(1025);
    for args in [
        vec!["recall"],
        vec!["recall", "--query"],
        vec!["recall", "--query", ""],
        vec!["recall", "--query", "   "],
        vec!["recall", "--query", "...！？___"],
        vec!["recall", "--query", "😀"],
        vec!["recall", "--query", "QUERY_PRIVATE\nnext"],
        vec!["recall", "--query", "QUERY_PRIVATE\tvalue"],
        vec!["recall", "--query", &oversized],
        vec!["recall", "--query", "QUERY_PRIVATE", "--limit", "0"],
        vec!["recall", "--query", "QUERY_PRIVATE", "--limit", "9"],
        vec!["recall", "--query", "QUERY_PRIVATE", "--limit", "-1"],
        vec!["recall", "--query", "QUERY_PRIVATE", "--limit", "+1"],
        vec!["recall", "--query", "QUERY_PRIVATE", "--limit", "1.5"],
        vec![
            "recall",
            "--query",
            "QUERY_PRIVATE",
            "--limit",
            "184467440737095516160",
        ],
        vec!["recall", "--query", "QUERY_PRIVATE", "--limit"],
        vec!["recall", "--query", "QUERY_PRIVATE", "--page", "1"],
        vec!["recall", "--query", "QUERY_PRIVATE", "--page-size", "1"],
        vec!["recall", "--query", "QUERY_PRIVATE", "--id", "style"],
        vec!["recall", "--query", "QUERY_PRIVATE", "--query", "duplicate"],
        vec![
            "recall",
            "--query",
            "QUERY_PRIVATE",
            "--limit",
            "1",
            "--limit",
            "2",
        ],
        vec!["recall", "--query", "--include-content"],
    ] {
        let mut command = command(directory.path(), &scope("alice"));
        command.args(&args);
        let output = run(command).await;
        assert!(!output.status.success(), "应拒绝 {args:?}");
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("QUERY_PRIVATE"));
    }
    for args in [
        vec!["--state-dir", "/unused", "recall", "--query", "q"],
        vec![
            "--state-dir",
            "/unused",
            "--channel",
            CHANNEL,
            "--session",
            SESSION,
            "recall",
            "--query",
            "q",
        ],
        vec![
            "--state-dir",
            "/unused",
            "--channel",
            CHANNEL,
            "--user",
            "alice",
            "recall",
            "--query",
            "q",
        ],
    ] {
        assert!(eve_app::MemoryCliOptions::parse(args.into_iter().map(OsString::from)).is_err());
    }
    assert_eq!(
        std::fs::read(directory.path().join("state.json")).unwrap(),
        before
    );
}
