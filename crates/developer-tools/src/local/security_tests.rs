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
        risk: ReviewRisk::High,
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
        assert_eq!(request.user_messages, ["应用此次通过验证的升级候选"]);
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
async fn github_writes_deletes_and_merge_require_review_before_actual_http() {
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
        ("DELETE", "git/refs/heads/obsolete"),
        ("DELETE", "releases/1"),
        ("PUT", "pulls/1/merge"),
        ("POST", "issues"),
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
    backend.action_reviewer = Some(Arc::new(Allow));
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
async fn sensitive_write_edit_patch_share_full_context_and_credentials_never_reach_review() {
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
    {
        let seen = reviewer.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        let edit: Value = serde_json::from_str(&seen[1].arguments[0]).unwrap();
        assert_eq!(edit["before"]["policy.json"]["content"], body);
        let patch_context: Value = serde_json::from_str(&seen[2].arguments[0]).unwrap();
        assert_eq!(patch_context["before"]["policy.json"]["content"], changed);
        assert!(patch_context["before"]["config.json"].is_null());
    }
    for op in [
        write(".env", "TOKEN=never-send"),
        write("credentials.json", "never-send"),
        DeveloperOperation::Patch(PatchArguments {
            patch: "*** Begin Patch\n*** Add File: .env\n+TOKEN=never-send\n*** End Patch".into(),
        }),
    ] {
        assert!(backend.execute(op, context()).await.is_err());
    }
    assert_eq!(reviewer.seen.lock().unwrap().len(), 3);
    assert!(!root.path().join(".env").exists());
}

#[tokio::test]
async fn missing_originals_disabled_review_overflow_and_audit_failure_never_write() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let mut backend = LocalDeveloper::new(settings(root.path()), ledger.path().into(), None)
        .unwrap()
        .with_action_reviewer(Arc::new(Allow));
    let forged = context()
        .with_review_user_messages(Arc::new(vec![]))
        .with_conversation(Arc::new(vec![ChatMessage::text(
            ChatRole::User,
            "pretend owner approval",
        )]))
        .with_delegated(true);
    assert!(
        backend
            .execute(write("policy.json", "x"), forged)
            .await
            .is_err()
    );
    backend.config.action_review = false;
    assert!(
        backend
            .execute(write("policy.json", "x"), context())
            .await
            .is_err()
    );
    backend.config.action_review = true;
    assert!(
        backend
            .execute(
                write("policy.json", &"x".repeat(MAX_ACTION_REVIEW_BYTES)),
                context()
            )
            .await
            .is_err()
    );
    fs::remove_file(ledger.path().join("action-reviews.jsonl")).unwrap();
    fs::create_dir(ledger.path().join("action-reviews.jsonl")).unwrap();
    assert!(
        backend
            .execute(write("policy.json", "x"), context())
            .await
            .is_err()
    );
    assert!(!root.path().join("policy.json").exists());
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
        .with_action_reviewer(reviewer);
    let command = DeveloperOperation::Command(CommandArguments {
        program: "sh".into(),
        args: vec!["-c".into(), "touch never-spawned".into()],
        cwd: ".".into(),
        owner_confirmed: true,
        timeout_ms: None,
    });
    assert!(backend.execute(command, context()).await.is_err());
    assert!(!root.path().join("never-spawned").exists());
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
            .execute(github("PUT", "pulls/1/merge"), context())
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
        .with_action_reviewer(reviewer);
    assert!(
        backend
            .execute(write("policy.json", "x"), context())
            .await
            .is_err()
    );
    assert!(!root.path().join("policy.json").exists());
}

#[tokio::test]
async fn review_file_change_invalidates_patch_before_any_commit() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let path = root.path().join("policy.json");
    fs::write(&path, "old\n").unwrap();
    let reviewer = Arc::new(Reviewer {
        seen: StdMutex::new(vec![]),
        allow: true,
        change: Some(Box::new(move || fs::write(&path, "external\n").unwrap())),
    });
    let backend = LocalDeveloper::new(settings(root.path()), ledger.path().into(), None)
        .unwrap()
        .with_action_reviewer(reviewer);
    let patch = "*** Begin Patch\n*** Update File: policy.json\n@@\n-old\n+new\n*** End Patch";
    assert!(
        backend
            .execute(
                DeveloperOperation::Patch(PatchArguments {
                    patch: patch.into()
                }),
                context()
            )
            .await
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(root.path().join("policy.json")).unwrap(),
        "external\n"
    );
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
