use super::*;
use eve_llm_api::{ChatMessage, ChatRole, ContextScope};
use std::sync::{Arc, Mutex as StdMutex, atomic::AtomicBool};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

pub(super) struct Allow;
impl ActionReviewer for Allow {
    fn review(&self, _: ActionReviewRequest) -> ActionReviewFuture<'_> {
        Box::pin(async { Ok(allow()) })
    }
}
fn allow() -> ActionReviewVerdict {
    ActionReviewVerdict {
        risk: ReviewRisk::Low,
        authorization: ReviewAuthorization::High,
        allow: true,
        rationale: "真实原话已授权".into(),
    }
}
struct Reviewer {
    seen: StdMutex<Vec<ActionReviewRequest>>,
    allow: bool,
    change: Option<Box<dyn Fn() + Send + Sync>>,
}
pub(super) struct UpgradeReviewer {
    pub(super) change: Box<dyn Fn() + Send + Sync>,
}
impl ActionReviewer for UpgradeReviewer {
    fn review(&self, request: ActionReviewRequest) -> ActionReviewFuture<'_> {
        assert_eq!(request.program, "eve.apply_self_upgrade");
        let action: Value = serde_json::from_str(&request.arguments[0]).unwrap();
        assert_eq!(action["source_diff_content"], "complete fixture diff");
        assert!(action["manifest"]["checks"].is_array());
        assert!(action["lease"]["manifest_sha256"].is_string());
        assert!(request.user_messages.is_empty());
        (self.change)();
        Box::pin(async { Ok(allow()) })
    }
}
impl ActionReviewer for Reviewer {
    fn review(&self, request: ActionReviewRequest) -> ActionReviewFuture<'_> {
        self.seen.lock().unwrap().push(request);
        if let Some(change) = &self.change {
            change();
        }
        Box::pin(async {
            let mut result = allow();
            result.allow = self.allow;
            Ok(result)
        })
    }
}
fn settings(root: &Path) -> DeveloperConfig {
    serde_json::from_value(json!({"workspace":root,"operators":["owner"],"commands":true,"github":true,"github_write_repositories":["test/repo"],"self_upgrade":false})).unwrap()
}
fn context() -> ToolExecutionContext {
    ToolExecutionContext::new()
        .with_scope(Some(ContextScope {
            session_id: "session".into(),
            user_id: "owner".into(),
        }))
        .with_turn_id(Some(1))
        .with_review_user_messages(Arc::new(vec![
            "请执行本次明确说明的配置修改、GitHub 删除与合并。".into(),
        ]))
}
fn github(method: &str, resource: &str) -> DeveloperOperation {
    DeveloperOperation::Github(GithubArguments {
        method: method.into(),
        repository: "test/repo".into(),
        resource: resource.into(),
        query: BTreeMap::new(),
        body: (method != "DELETE").then(|| json!({"sha":"abcdef", "message":"原文"})),
    })
}
fn write(path: &str, content: &str) -> DeveloperOperation {
    DeveloperOperation::Write(FileArguments {
        path: path.into(),
        offset: 0,
        content: Some(content.into()),
        expected_sha256: Some(String::new()),
    })
}

#[tokio::test]
async fn only_github_access_and_execution_changes_require_review_before_actual_http() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let reviewer = Arc::new(Reviewer {
        seen: StdMutex::new(vec![]),
        allow: false,
        change: None,
    });
    let mut backend = LocalDeveloper::new(
        settings(root.path()),
        ledger.path().into(),
        Some("host-token-fixture".into()),
    )
    .unwrap()
    .with_action_reviewer(reviewer.clone());
    backend.api = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    for (method, path) in [
        ("DELETE", "hooks/1"),
        ("PUT", "collaborators/new-member"),
        ("PUT", "actions/permissions"),
        ("POST", "keys"),
    ] {
        assert!(
            backend
                .execute(github(method, path), context())
                .await
                .is_err()
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
    assert_eq!(reviewer.seen.lock().unwrap().len(), 4);
    assert!(reviewer.seen.lock().unwrap().iter().all(|request| {
        request.program == "eve.github_api"
            && !serde_json::to_string(request)
                .unwrap()
                .contains("host-token-fixture")
    }));
    // 普通仓库写入即使审查服务拒绝所有请求也能执行；实际危险才调用它。
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = vec![0; 8192];
        let n = stream.read(&mut bytes).await.unwrap();
        assert!(
            String::from_utf8_lossy(&bytes[..n]).starts_with("PUT /repos/test/repo/pulls/1/merge")
        );
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .await
            .unwrap();
    });
    assert_eq!(
        backend
            .execute(github("PUT", "pulls/1/merge"), context())
            .await
            .unwrap()["success"],
        true
    );
    server.await.unwrap();
}

#[tokio::test]
async fn ordinary_config_write_edit_and_patch_skip_review_but_credentials_stay_private() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let reviewer = Arc::new(Reviewer {
        seen: StdMutex::new(vec![]),
        allow: true,
        change: None,
    });
    let backend = LocalDeveloper::new(settings(root.path()), ledger.path().into(), None)
        .unwrap()
        .with_action_reviewer(reviewer.clone());
    let body = format!("review=true\n{}", "完整配置正文\n".repeat(1200));
    backend
        .execute(write("policy.json", &body), context())
        .await
        .unwrap();
    let changed = body.replacen("true", "enabled", 1);
    backend
        .execute(
            DeveloperOperation::Edit(EditArguments {
                path: "policy.json".into(),
                old_string: "review=true".into(),
                new_string: "review=enabled".into(),
                expected_sha256: hash(body.as_bytes()),
            }),
            context(),
        )
        .await
        .unwrap();
    let patch = "*** Begin Patch\n*** Update File: policy.json\n*** Move to: config.json\n@@\n-review=enabled\n+review=required\n*** End Patch";
    backend
        .execute(
            DeveloperOperation::Patch(PatchArguments {
                patch: patch.into(),
            }),
            context(),
        )
        .await
        .unwrap();
    assert!(reviewer.seen.lock().unwrap().is_empty());
    assert_eq!(
        fs::read_to_string(root.path().join("config.json")).unwrap(),
        changed.replacen("enabled", "required", 1)
    );
    for op in [
        write(".env", "TOKEN=never-send"),
        write("credentials.json", "never-send"),
        DeveloperOperation::Patch(PatchArguments {
            patch: "*** Begin Patch\n*** Add File: .env\n+TOKEN=never-send\n*** End Patch".into(),
        }),
    ] {
        assert!(backend.execute(op, context()).await.is_err());
    }
    assert!(reviewer.seen.lock().unwrap().is_empty());
    assert!(!root.path().join(".env").exists());
}

#[tokio::test]
async fn ordinary_source_access_reads_edits_and_searches_without_filename_false_positives() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let reviewer = Arc::new(Reviewer {
        seen: StdMutex::new(vec![]),
        allow: false,
        change: None,
    });
    let mut config = settings(root.path());
    config.ordinary_access = true;
    let backend = LocalDeveloper::new(config, ledger.path().into(), None)
        .unwrap()
        .with_action_reviewer(reviewer.clone());
    let guest = || {
        ToolExecutionContext::new().with_scope(Some(ContextScope {
            session_id: "source-reader".into(),
            user_id: "guest".into(),
        }))
    };
    let ordinary = [
        "Cargo.toml",
        "AGENTS.md",
        ".cargo/config.toml",
        "src/config.rs",
        "src/credentials.rs",
        "src/secret_parser.rs",
        "src/credentials/mod.rs",
        "tests/secrets/parser.rs",
        "src/credential_schema.json",
        "docs/secret-handling.txt",
        "fixtures/hosts.yml",
        "src/auth.json",
    ];
    let protected = [".env", "credentials.json", ".netrc", ".config/gh/hosts.yml"];
    for path in ordinary.iter().chain(&protected) {
        let actual = root.path().join(path);
        fs::create_dir_all(actual.parent().unwrap()).unwrap();
        fs::write(actual, "needle before\n").unwrap();
    }
    let read = |path: &str| {
        DeveloperOperation::Read(FileArguments {
            path: path.into(),
            offset: 0,
            content: None,
            expected_sha256: None,
        })
    };
    for path in ordinary {
        assert_eq!(
            backend.execute(read(path), guest()).await.unwrap()["content"],
            "needle before\n",
            "{path}"
        );
    }
    backend
        .execute(
            DeveloperOperation::Edit(EditArguments {
                path: "src/credentials.rs".into(),
                old_string: "before".into(),
                new_string: "after".into(),
                expected_sha256: hash(b"needle before\n"),
            }),
            guest(),
        )
        .await
        .unwrap();
    backend
        .execute(write("config.json", "needle new\n"), guest())
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(root.path().join("src/credentials.rs")).unwrap(),
        "needle after\n"
    );
    let result = backend
        .execute(
            DeveloperOperation::Search(SearchArguments {
                path: ".".into(),
                query: "needle".into(),
                max_results: 50,
            }),
            guest(),
        )
        .await
        .unwrap();
    let found: std::collections::BTreeSet<_> = result["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["path"].as_str().unwrap())
        .collect();
    let expected: std::collections::BTreeSet<_> =
        ordinary.into_iter().chain(["config.json"]).collect();
    assert_eq!(found, expected);
    for path in protected {
        assert!(backend.execute(read(path), guest()).await.is_err());
        assert!(
            backend
                .execute(write(path, "must stay private"), guest())
                .await
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(root.path().join(path)).unwrap(),
            "needle before\n"
        );
    }
    assert!(
        backend
            .execute(read("Cargo.toml"), ToolExecutionContext::new())
            .await
            .is_err()
    );
    assert!(reviewer.seen.lock().unwrap().is_empty());
    assert!(!ledger.path().join("action-reviews.jsonl").exists());
}

#[tokio::test]
async fn exploration_without_user_words_or_review_service_writes_real_files() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let mut config = settings(root.path());
    config.action_review = false;
    let backend = LocalDeveloper::new(config, ledger.path().into(), None).unwrap();
    let curious = context()
        .with_review_user_messages(Arc::new(vec![]))
        .with_conversation(Arc::new(vec![ChatMessage::text(
            ChatRole::User,
            "model generated exploration",
        )]))
        .with_delegated(true);
    backend
        .execute(write("policy.json", "a freely chosen note"), curious)
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(root.path().join("policy.json")).unwrap(),
        "a freely chosen note"
    );
    assert!(!ledger.path().join("action-reviews.jsonl").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn exploration_runs_a_real_script_without_review_but_never_executes_system_destruction() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let mut config = settings(root.path());
    config.action_review = false;
    let backend = LocalDeveloper::new(config, ledger.path().into(), None).unwrap();
    let curious = context()
        .with_review_user_messages(Arc::new(vec![]))
        .with_delegated(true);
    let command = |script: &str| {
        DeveloperOperation::Command(CommandArguments {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: ".".into(),
            timeout_ms: Some(1_000),
            owner_confirmed: false,
        })
    };
    let result = backend
        .execute(
            command("printf 'curiosity survived' > discovery.txt; cat discovery.txt"),
            curious.clone(),
        )
        .await
        .unwrap();
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["stdout"], "curiosity survived");
    assert_eq!(
        fs::read_to_string(root.path().join("discovery.txt")).unwrap(),
        "curiosity survived"
    );
    assert!(!ledger.path().join("action-reviews.jsonl").exists());
    let rejected = backend
        .execute(command("touch must-not-run; reboot"), curious)
        .await
        .unwrap_err();
    assert!(rejected.to_string().contains("策略禁止"));
    assert!(!root.path().join("must-not-run").exists());
}

#[tokio::test]
async fn known_host_credentials_never_reach_github_even_when_ordinary_writes_skip_review() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut backend = LocalDeveloper::new(
        settings(root.path()),
        ledger.path().into(),
        Some("host-token-fixture".into()),
    )
    .unwrap();
    backend.api = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let DeveloperOperation::Github(mut operation) = github("POST", "issues") else {
        unreachable!()
    };
    operation.body = Some(json!({"body":"host-token-fixture"}));
    assert!(
        backend
            .execute(DeveloperOperation::Github(operation), context())
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn known_host_credentials_in_command_arguments_are_rejected_before_spawn() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let backend = LocalDeveloper::new(
        settings(root.path()),
        ledger.path().into(),
        Some("host-token-fixture".into()),
    )
    .unwrap();
    let operation = DeveloperOperation::Command(CommandArguments {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            "printf 'host-token-fixture' > must-not-leak".into(),
        ],
        cwd: ".".into(),
        timeout_ms: None,
        owner_confirmed: false,
    });
    assert!(backend.execute(operation, context()).await.is_err());
    assert!(!root.path().join("must-not-leak").exists());
    assert!(!ledger.path().join("action-reviews.jsonl").exists());
}

#[tokio::test]
async fn missing_identity_and_failed_execution_journal_still_prevent_writes() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let backend = LocalDeveloper::new(settings(root.path()), ledger.path().into(), None).unwrap();
    assert!(
        backend
            .execute(write("missing-identity", "x"), ToolExecutionContext::new())
            .await
            .is_err()
    );
    fs::remove_file(ledger.path().join("operations.jsonl")).unwrap();
    fs::create_dir(ledger.path().join("operations.jsonl")).unwrap();
    assert!(
        backend
            .execute(write("failed-journal", "x"), context())
            .await
            .is_err()
    );
    assert!(!root.path().join("missing-identity").exists());
    assert!(!root.path().join("failed-journal").exists());
}

struct Authorization(Arc<AtomicBool>);
impl DeveloperAuthorization for Authorization {
    fn is_operator(&self, _: &ContextScope) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    fn allows_workspace_development(&self, _: &ContextScope) -> bool {
        true
    }
}

#[cfg(unix)]
#[tokio::test]
async fn revocation_during_command_review_prevents_process_spawn() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let allowed = Arc::new(AtomicBool::new(true));
    let revoked = allowed.clone();
    let reviewer = Arc::new(Reviewer {
        seen: StdMutex::new(vec![]),
        allow: true,
        change: Some(Box::new(move || revoked.store(false, Ordering::SeqCst))),
    });
    let backend = LocalDeveloper::new(settings(root.path()), ledger.path().into(), None)
        .unwrap()
        .with_authorization(Arc::new(Authorization(allowed)))
        .with_command_policy(Arc::new(super::tests::ScriptPolicy))
        .with_action_reviewer(reviewer);
    let command = DeveloperOperation::Command(CommandArguments {
        program: "sh".into(),
        args: vec!["-c".into(), "touch careful-never-spawned".into()],
        cwd: ".".into(),
        owner_confirmed: true,
        timeout_ms: None,
    });
    assert!(backend.execute(command, context()).await.is_err());
    assert!(!root.path().join("careful-never-spawned").exists());
}

#[tokio::test]
async fn public_github_cannot_bypass_business_pause_after_review() {
    struct Public(Arc<AtomicBool>);
    impl DeveloperAuthorization for Public {
        fn is_operator(&self, _: &ContextScope) -> bool {
            false
        }
        fn permits_business(&self, _: &ContextScope) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let allowed = Arc::new(AtomicBool::new(true));
    let revoked = allowed.clone();
    let reviewer = Arc::new(Reviewer {
        seen: StdMutex::new(vec![]),
        allow: true,
        change: Some(Box::new(move || revoked.store(false, Ordering::SeqCst))),
    });
    let mut config = settings(root.path());
    config.github_all_users = true;
    let mut backend = LocalDeveloper::new(
        config,
        ledger.path().into(),
        Some("host-token-fixture".into()),
    )
    .unwrap()
    .with_authorization(Arc::new(Public(allowed)))
    .with_action_reviewer(reviewer.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    backend.api = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    assert!(
        backend
            .execute(github("PUT", "actions/permissions"), context())
            .await
            .is_err()
    );
    assert_eq!(reviewer.seen.lock().unwrap().len(), 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}
#[cfg(unix)]
#[tokio::test]
async fn revocation_during_review_cannot_fall_back_to_configured_owner_or_host_workspace() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let allowed = Arc::new(AtomicBool::new(true));
    let revoked = allowed.clone();
    let reviewer = Arc::new(Reviewer {
        seen: StdMutex::new(vec![]),
        allow: true,
        change: Some(Box::new(move || revoked.store(false, Ordering::SeqCst))),
    });
    let backend = LocalDeveloper::new(settings(root.path()), ledger.path().into(), None)
        .unwrap()
        .with_authorization(Arc::new(Authorization(allowed)))
        .with_member_runtime(MemberRuntime::default())
        .with_command_policy(Arc::new(super::tests::ScriptPolicy))
        .with_action_reviewer(reviewer);
    let operation = DeveloperOperation::Command(CommandArguments {
        program: "sh".into(),
        args: vec!["-c".into(), "touch careful-never".into()],
        cwd: ".".into(),
        timeout_ms: None,
        owner_confirmed: false,
    });
    assert!(backend.execute(operation, context()).await.is_err());
    assert!(!root.path().join("careful-never").exists());
}

#[tokio::test]
async fn ordinary_edit_still_rejects_stale_file_hash_without_a_model_review() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let path = root.path().join("policy.json");
    fs::write(&path, "external\n").unwrap();
    let reviewer = Arc::new(Reviewer {
        seen: StdMutex::new(vec![]),
        allow: false,
        change: None,
    });
    let backend = LocalDeveloper::new(settings(root.path()), ledger.path().into(), None)
        .unwrap()
        .with_action_reviewer(reviewer.clone());
    let edit = DeveloperOperation::Edit(EditArguments {
        path: "policy.json".into(),
        old_string: "old".into(),
        new_string: "new".into(),
        expected_sha256: hash(b"old\n"),
    });
    assert!(backend.execute(edit, context()).await.is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), "external\n");
    assert!(reviewer.seen.lock().unwrap().is_empty());
}

#[test]
fn default_upgrade_permission_does_not_register_execution_when_commands_explicitly_disabled() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let config: DeveloperConfig = serde_json::from_value(
        json!({"workspace":root.path(),"operators":["owner"],"commands":false}),
    )
    .unwrap();
    assert!(config.self_upgrade);
    let backend =
        Arc::new(LocalDeveloper::new(config.clone(), ledger.path().into(), None).unwrap());
    assert!(crate::tools(&config, backend).iter().all(|tool| !matches!(
        tool.definition().name.as_str(),
        "run_command" | "prepare_self_upgrade" | "apply_self_upgrade"
    )));
}
