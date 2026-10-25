use super::*;
use eve_llm_api::{ContextScope, ToolCall, ToolExecutionContext};
use std::{collections::BTreeSet, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

fn config(root: &Path) -> DeveloperConfig {
    DeveloperConfig {
        workspace: root.into(),
        storage_directory: None,
        operators: BTreeSet::from(["owner".into()]),
        ordinary_access: false,
        commands: true,
        github: true,
        github_all_users: false,
        github_all_repositories: false,
        github_write_repositories: BTreeSet::from(["test/repo".into()]),
        github_token_env: "EVE_GITHUB_TOKEN".into(),
        command_timeout_ms: 10_000,
        max_tool_rounds: 8,
        self_upgrade: false,
        live_upgrade: None,
        command_policy: None,
        cargo_program: "cargo".into(),
        cargo_arguments: vec![],
        action_review: true,
    }
}
fn context(user: &str) -> ToolExecutionContext {
    ToolExecutionContext::new().with_scope(Some(ContextScope {
        session_id: "test-session".into(),
        user_id: user.into(),
    }))
}
fn file(path: &str) -> FileArguments {
    FileArguments {
        path: path.into(),
        offset: 0,
        content: None,
        expected_sha256: None,
    }
}

#[tokio::test]
async fn explicit_member_policy_isolates_project_and_does_not_grant_upgrade_or_shared_root() {
    struct Members;
    impl DeveloperAuthorization for Members {
        fn is_operator(&self, _: &ContextScope) -> bool {
            false
        }
        fn allows_workspace_development(&self, _: &ContextScope) -> bool {
            true
        }
    }
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let mut a = file("mod.hjson");
    a.content = Some("name: member".into());
    a.expected_sha256 = Some("".into());
    let without_runtime = LocalDeveloper::new(config(root.path()), ledger.path().into(), None)
        .unwrap()
        .with_authorization(Arc::new(Members));
    assert!(
        without_runtime
            .execute(DeveloperOperation::Write(a.clone()), context("a"))
            .await
            .is_err()
    );
    let backend = without_runtime.with_member_runtime(MemberRuntime::default());
    backend
        .execute(DeveloperOperation::Write(a), context("a"))
        .await
        .unwrap();
    assert!(!root.path().join("mod.hjson").exists());
    assert!(
        backend
            .execute(DeveloperOperation::Read(file("mod.hjson")), context("b"))
            .await
            .is_err()
    );
    assert!(
        backend
            .execute(DeveloperOperation::Read(file("../mod.hjson")), context("a"))
            .await
            .is_err()
    );
    assert!(
        backend
            .execute(DeveloperOperation::PrepareUpgrade, context("a"))
            .await
            .is_err()
    );
    let scope = ContextScope {
        session_id: "test-session".into(),
        user_id: "a".into(),
    };
    let project = member_workspace(root.path(), &scope);
    assert_eq!(
        std::fs::read_to_string(project.join("mod.hjson")).unwrap(),
        "name: member"
    );
    assert!(project.join("exports").is_dir());
}

#[tokio::test]
async fn actual_file_roundtrip_cas_paging_and_restart_never_replays() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let backend = LocalDeveloper::new(config(root.path()), ledger.path().into(), None).unwrap();
    let mut a = file("hello.txt");
    a.content = Some("你好 Eve".into());
    a.expected_sha256 = Some("".into());
    let written = backend
        .execute(DeveloperOperation::Write(a.clone()), context("owner"))
        .await
        .unwrap();
    assert_eq!(written["success"], true);
    let read = backend
        .execute(
            DeveloperOperation::Read(file("hello.txt")),
            context("owner"),
        )
        .await
        .unwrap();
    assert_eq!(read["sha256"], written["sha256"]);
    assert_eq!(read["content"], "你好 Eve");
    assert!(
        backend
            .execute(DeveloperOperation::Write(a.clone()), context("owner"))
            .await
            .is_err()
    );
    a.expected_sha256 = Some(read["sha256"].as_str().unwrap().into());
    a.content = Some("新的版本".into());
    backend
        .execute(DeveloperOperation::Write(a), context("owner"))
        .await
        .unwrap();
    fs::write(root.path().join("large.txt"), "界".repeat(20_000)).unwrap();
    let page = backend
        .execute(
            DeveloperOperation::Read(file("large.txt")),
            context("owner"),
        )
        .await
        .unwrap();
    assert_eq!(page["truncated"], true);
    let mut second = file("large.txt");
    second.offset = page["next_offset"].as_u64().unwrap() as usize;
    let next = backend
        .execute(DeveloperOperation::Read(second), context("owner"))
        .await
        .unwrap();
    assert_eq!(
        page["content"].as_str().unwrap().len() + next["content"].as_str().unwrap().len(),
        60_000
    );
    let before = fs::read(ledger.path().join("operations.jsonl")).unwrap();
    drop(backend);
    LocalDeveloper::new(config(root.path()), ledger.path().into(), None).unwrap();
    assert_eq!(
        before,
        fs::read(ledger.path().join("operations.jsonl")).unwrap()
    );
    assert_eq!(
        fs::read_to_string(root.path().join("hello.txt")).unwrap(),
        "新的版本"
    );
}
#[tokio::test]
async fn identity_paths_and_corrupt_journal_are_rejected_without_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let backend = LocalDeveloper::new(config(root.path()), ledger.path().into(), None).unwrap();
    assert!(
        backend
            .execute(DeveloperOperation::List(file(".")), context("foreign"))
            .await
            .is_err()
    );
    assert!(
        backend
            .execute(
                DeveloperOperation::List(file(".")),
                ToolExecutionContext::new()
            )
            .await
            .is_err()
    );
    assert!(!ledger.path().join("operations.jsonl").exists());
    for path in ["../private", "/etc/passwd", ".git/config", "C:/config.json"] {
        assert!(
            backend
                .execute(DeveloperOperation::Read(file(path)), context("owner"))
                .await
                .is_err()
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(ledger.path(), root.path().join("link")).unwrap();
        assert!(
            backend
                .execute(DeveloperOperation::List(file("link")), context("owner"))
                .await
                .is_err()
        );
    }
    let bytes = b"{broken";
    fs::write(ledger.path().join("operations.jsonl"), bytes).unwrap();
    assert!(LocalDeveloper::new(config(root.path()), ledger.path().into(), None).is_err());
    assert_eq!(
        fs::read(ledger.path().join("operations.jsonl")).unwrap(),
        bytes
    );
}
#[tokio::test]
async fn public_github_allows_guest_write_without_granting_local_tools() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut settings = config(root.path());
    settings.github_all_users = true;
    settings.github_all_repositories = true;
    let mut backend = LocalDeveloper::new(
        settings.clone(),
        ledger.path().into(),
        Some("SYNTHETIC_GITHUB_TOKEN".into()),
    )
    .unwrap();
    backend.api = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let write = GithubArguments {
        method: "PATCH".into(),
        repository: "foreign/repo".into(),
        resource: "issues/1".into(),
        query: BTreeMap::new(),
        body: Some(json!({"title":"updated"})),
    };
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 8192];
        let n = socket.read(&mut request).await.unwrap();
        let request = String::from_utf8_lossy(&request[..n]);
        assert!(request.starts_with("PATCH /repos/foreign/repo/issues/1 HTTP/1.1"));
        assert!(
            request
                .to_lowercase()
                .contains("authorization: bearer synthetic_github_token")
        );
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .await
            .unwrap();
    });
    let result = backend
        .execute(DeveloperOperation::Github(write.clone()), context("guest"))
        .await
        .unwrap();
    assert_eq!(result["success"], true);
    server.await.unwrap();
    for operation in [
        DeveloperOperation::Read(file("README.md")),
        DeveloperOperation::PrepareUpgrade,
        DeveloperOperation::UpgradeStatus,
        DeveloperOperation::Command(CommandArguments {
            owner_confirmed: false,
            program: "must-not-run".into(),
            args: vec![],
            cwd: ".".into(),
            timeout_ms: None,
        }),
    ] {
        assert!(backend.execute(operation, context("guest")).await.is_err());
    }
    assert!(
        backend
            .execute(
                DeveloperOperation::Github(write.clone()),
                ToolExecutionContext::new()
            )
            .await
            .is_err()
    );
    settings.github_all_repositories = false;
    let restricted = LocalDeveloper::new(
        settings.clone(),
        ledger.path().into(),
        Some("SYNTHETIC_GITHUB_TOKEN".into()),
    )
    .unwrap();
    assert!(
        restricted
            .execute(DeveloperOperation::Github(write.clone()), context("guest"))
            .await
            .is_err()
    );
    settings.github_all_users = false;
    settings.github_all_repositories = true;
    let owners = LocalDeveloper::new(
        settings.clone(),
        ledger.path().into(),
        Some("SYNTHETIC_GITHUB_TOKEN".into()),
    )
    .unwrap();
    assert!(
        owners
            .execute(DeveloperOperation::Github(write.clone()), context("guest"))
            .await
            .is_err()
    );
    settings.github_all_users = true;
    let no_token = LocalDeveloper::new(settings, ledger.path().into(), None).unwrap();
    assert!(
        no_token
            .execute(DeveloperOperation::Github(write), context("guest"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn github_actual_http_binds_host_token_decodes_source_and_redacts_result() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut backend = LocalDeveloper::new(
        config(root.path()),
        ledger.path().into(),
        Some("SYNTHETIC_GITHUB_TOKEN".into()),
    )
    .unwrap();
    backend.api = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 8192];
        let n = socket.read(&mut request).await.unwrap();
        let request = String::from_utf8_lossy(&request[..n]);
        assert!(request.starts_with("GET /repos/test/repo/contents/README.md?ref=main"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer synthetic_github_token")
        );
        let body = r#"{"path":"README.md","sha":"sha","encoding":"base64","content":"aGVsbG8gRXZl","message":"SYNTHETIC_GITHUB_TOKEN"}"#;
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
    });
    let a = GithubArguments {
        method: "GET".into(),
        repository: "test/repo".into(),
        resource: "contents/README.md".into(),
        query: BTreeMap::from([("ref".into(), "main".into())]),
        body: None,
    };
    let result = backend
        .execute(DeveloperOperation::Github(a.clone()), context("owner"))
        .await
        .unwrap();
    assert_eq!(result["success"], true);
    assert_eq!(result["data"]["text"], "hello Eve");
    assert!(!result.to_string().contains("SYNTHETIC_GITHUB_TOKEN"));
    server.await.unwrap();
    let mut write = a.clone();
    write.method = "PUT".into();
    write.repository = "foreign/repo".into();
    assert!(
        backend
            .execute(DeveloperOperation::Github(write), context("owner"))
            .await
            .is_err()
    );
    let mut secret = a;
    secret.resource = "actions/secrets".into();
    assert!(
        backend
            .execute(DeveloperOperation::Github(secret), context("owner"))
            .await
            .is_err()
    );
    assert!(
        !fs::read_to_string(ledger.path().join("operations.jsonl"))
            .unwrap()
            .contains("SYNTHETIC_GITHUB_TOKEN")
    );
}
#[tokio::test]
async fn github_prefix_and_query_path_reach_actual_source_without_expanding_permissions() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut backend = LocalDeveloper::new(config(root.path()), ledger.path().into(), None).unwrap();
    backend.api = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move {
        for expected in [
            "/repos/test/repo/contents/README.md?ref=main",
            "/repos/test/repo/git/blobs/abc",
            "/repos/test/repo",
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let n = socket.read(&mut request).await.unwrap();
            assert!(
                String::from_utf8_lossy(&request[..n])
                    .starts_with(&format!("GET {expected} HTTP/1.1"))
            );
            let body = r#"{"path":"README.md","encoding":"base64","content":"aGVsbG8gRXZl"}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    });
    let a = GithubArguments {
        method: "GET".into(),
        repository: "test/repo".into(),
        resource: "repos/test/repo/contents".into(),
        query: BTreeMap::from([
            ("path".into(), "README.md".into()),
            ("ref".into(), "main".into()),
        ]),
        body: None,
    };
    let tool = crate::tools(
        &backend.config,
        Arc::new(LocalDeveloper::new(backend.config.clone(), ledger.path().into(), None).unwrap()),
    )
    .into_iter()
    .find(|t| t.definition().name == "github_api")
    .unwrap();
    tool.validate_arguments(&serde_json::to_value(&a).unwrap())
        .unwrap();
    let result = backend
        .execute(DeveloperOperation::Github(a.clone()), context("owner"))
        .await
        .unwrap();
    assert_eq!(result["data"]["text"], "hello Eve");
    assert_eq!(result["resource"], "contents/README.md");
    for resource in ["repos/test/repo/git/blobs/abc", "repos/test/repo"] {
        let mut b = a.clone();
        b.resource = resource.into();
        b.query.clear();
        assert_eq!(
            backend
                .execute(DeveloperOperation::Github(b), context("owner"))
                .await
                .unwrap()["success"],
            true
        );
    }
    for resource in [
        "repos/foreign/repo/contents",
        "repos/test/repo/actions/secrets",
        "repos/test/repo/contents/../issues",
    ] {
        let mut b = a.clone();
        b.resource = resource.into();
        b.query.clear();
        assert!(
            backend
                .execute(DeveloperOperation::Github(b), context("owner"))
                .await
                .is_err()
        );
    }
    let mut write = a.clone();
    write.query.clear();
    write.method = "PUT".into();
    write.repository = "foreign/repo".into();
    write.resource = "repos/foreign/repo/contents/README.md".into();
    assert!(
        backend
            .execute(DeveloperOperation::Github(write), context("owner"))
            .await
            .is_err()
    );
    assert!(
        backend
            .execute(DeveloperOperation::Github(a), context("other-user"))
            .await
            .is_err()
    );
    server.await.unwrap();
}

#[tokio::test]
async fn tools_reject_identity_urls_and_extra_arguments_and_only_register_enabled_services() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let mut c = config(root.path());
    c.commands = false;
    c.github = false;
    let backend = Arc::new(LocalDeveloper::new(c.clone(), ledger.path().into(), None).unwrap());
    let tools = crate::tools(&c, backend);
    assert_eq!(tools.len(), 5);
    let read = tools
        .iter()
        .find(|t| t.definition().name == "read_file")
        .unwrap();
    assert!(
        read.validate_arguments(&json!({"path":"foo","user_id":"owner"}))
            .is_err()
    );
    assert!(
        read.validate_arguments(&json!({"path":"foo","content":"new"}))
            .is_err()
    );
    let _ = ToolCall {
        id: "one".into(),
        name: "read_file".into(),
        arguments: json!({"path":"foo"}),
    };
    let edit = tools
        .iter()
        .find(|t| t.definition().name == "edit_file")
        .unwrap();
    let digest = "0".repeat(64);
    assert!(
        edit.validate_arguments(
            &json!({"path":"a","old_string":"","new_string":"x","expected_sha256":digest})
        )
        .is_err()
    );
    assert!(
        edit.validate_arguments(
            &json!({"path":"a","old_string":"x","new_string":"x","expected_sha256":digest})
        )
        .is_err()
    );
    assert!(
        edit.validate_arguments(
            &json!({"path":"a","old_string":"x","new_string":"y","expected_sha256":"short"})
        )
        .is_err()
    );
    assert!(
        edit.validate_arguments(&json!({"path":"a","old_string":"x","new_string":"y","expected_sha256":digest,"user_id":"owner"}))
            .is_err()
    );
    assert!(
        edit.validate_arguments(
            &json!({"path":"a","old_string":"x","new_string":"y","expected_sha256":digest})
        )
        .is_ok()
    );
    let search = tools
        .iter()
        .find(|t| t.definition().name == "search_files")
        .unwrap();
    assert!(search.validate_arguments(&json!({"path":"src"})).is_err());
    assert!(search.validate_arguments(&json!({"query":""})).is_err());
    assert!(
        search
            .validate_arguments(&json!({"query":"x","max_results":0}))
            .is_err()
    );
    assert!(
        search
            .validate_arguments(&json!({"query":"x","max_results":101}))
            .is_err()
    );
    assert!(
        search
            .validate_arguments(&json!({"query":"x","path":"src"}))
            .is_ok()
    );
}

#[tokio::test]
async fn edit_replaces_one_exact_occurrence_and_refuses_stale_missing_or_ambiguous_text() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let backend = LocalDeveloper::new(config(root.path()), ledger.path().into(), None).unwrap();
    let mut create = file("notes.txt");
    create.content = Some("one\ntwo\nthree\n".into());
    create.expected_sha256 = Some("".into());
    backend
        .execute(DeveloperOperation::Write(create), context("owner"))
        .await
        .unwrap();
    let original = hash(b"one\ntwo\nthree\n");
    let edit = |path: &str, old: &str, new: &str, sha: &str| EditArguments {
        path: path.into(),
        old_string: old.into(),
        new_string: new.into(),
        expected_sha256: sha.into(),
    };
    let refused = |result: Result<Value, ToolExecutionError>| match result {
        Err(ToolExecutionError::Failed(message)) => message,
        Err(_) => panic!("拒绝类型不符"),
        Ok(_) => panic!("应当拒绝却成功"),
    };

    let stale = refused(
        backend
            .execute(
                DeveloperOperation::Edit(edit("notes.txt", "two", "2", &hash(b"stale"))),
                context("owner"),
            )
            .await,
    );
    assert!(stale.contains("文件已变化"));
    let missing = refused(
        backend
            .execute(
                DeveloperOperation::Edit(edit("notes.txt", "four", "4", &original)),
                context("owner"),
            )
            .await,
    );
    assert!(missing.contains("不存在"));
    // 重叠出现也算多处：aa 在 aaa 中出现两次，不能任选一处替换。
    let mut overlap = file("overlap.txt");
    overlap.content = Some("aaa".into());
    overlap.expected_sha256 = Some("".into());
    backend
        .execute(DeveloperOperation::Write(overlap), context("owner"))
        .await
        .unwrap();
    let ambiguous = refused(
        backend
            .execute(
                DeveloperOperation::Edit(edit("overlap.txt", "aa", "b", &hash(b"aaa"))),
                context("owner"),
            )
            .await,
    );
    assert!(ambiguous.contains("多次"));
    assert_eq!(
        std::fs::read_to_string(root.path().join("overlap.txt")).unwrap(),
        "aaa"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("notes.txt")).unwrap(),
        "one\ntwo\nthree\n"
    );

    let done = backend
        .execute(
            DeveloperOperation::Edit(edit("notes.txt", "two", "2", &original)),
            context("owner"),
        )
        .await
        .unwrap();
    let updated = "one\n2\nthree\n";
    assert_eq!(done["success"], true);
    assert_eq!(done["sha256"], hash(updated.as_bytes()));
    assert_eq!(
        std::fs::read_to_string(root.path().join("notes.txt")).unwrap(),
        updated
    );
    // 旧修订不能再次用于替换，必须重新读取。
    let replay = refused(
        backend
            .execute(
                DeveloperOperation::Edit(edit("notes.txt", "2", "two", &original)),
                context("owner"),
            )
            .await,
    );
    assert!(replay.contains("文件已变化"));
    let foreign = refused(
        backend
            .execute(
                DeveloperOperation::Edit(edit("notes.txt", "2", "x", &hash(updated.as_bytes()))),
                context("foreign"),
            )
            .await,
    );
    assert!(foreign.contains("仅供宿主配置的操作者"));
    let journal = std::fs::read_to_string(ledger.path().join("operations.jsonl")).unwrap();
    assert!(journal.contains("\"kind\":\"edit\",\"state\":\"started\""));
    assert!(journal.contains("\"kind\":\"edit\",\"state\":\"finished\""));
}

#[tokio::test]
async fn search_is_literal_bounded_redacted_and_skips_version_control_build_and_links() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let secret = "github-secret-token-for-search";
    let backend = LocalDeveloper::new(
        config(root.path()),
        ledger.path().into(),
        Some(secret.into()),
    )
    .unwrap();
    std::fs::create_dir_all(root.path().join("src/nested")).unwrap();
    std::fs::create_dir_all(root.path().join("target")).unwrap();
    std::fs::create_dir_all(root.path().join(".git")).unwrap();
    std::fs::write(
        root.path().join("src/a.rs"),
        "fn needle() {}\nno match\nneedle again\n",
    )
    .unwrap();
    std::fs::write(root.path().join("src/nested/b.txt"), "needle\n").unwrap();
    std::fs::write(
        root.path().join("src/nested/c.txt"),
        format!("{secret} needle"),
    )
    .unwrap();
    std::fs::write(root.path().join("target/x.txt"), "needle").unwrap();
    std::fs::write(root.path().join(".git/config"), "needle").unwrap();
    std::fs::write(root.path().join("binary.dat"), b"\xff needle").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.path().join("src/a.rs"), root.path().join("link.rs")).unwrap();
    let search = |path: &str, query: &str, max_results: usize| SearchArguments {
        path: path.into(),
        query: query.into(),
        max_results,
    };

    let all = backend
        .execute(
            DeveloperOperation::Search(search(".", "needle", 50)),
            context("owner"),
        )
        .await
        .unwrap();
    let hits: Vec<(String, u64)> = all["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["path"].as_str().unwrap().to_string(),
                m["line"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        hits,
        vec![
            ("src/a.rs".to_string(), 1),
            ("src/a.rs".to_string(), 3),
            ("src/nested/b.txt".to_string(), 1),
            ("src/nested/c.txt".to_string(), 1),
        ]
    );
    assert_eq!(all["truncated"], false);
    assert_eq!(all["untrusted_data"], true);
    assert!(
        all["matches"][3]["text"]
            .as_str()
            .unwrap()
            .contains("[GITHUB_CREDENTIAL_REDACTED]")
    );
    assert!(!all.to_string().contains(secret));

    // 字面匹配：正则写法不被解释。
    let literal = backend
        .execute(
            DeveloperOperation::Search(search(".", r"fn\s+needle", 50)),
            context("owner"),
        )
        .await
        .unwrap();
    assert!(literal["matches"].as_array().unwrap().is_empty());

    let one = backend
        .execute(
            DeveloperOperation::Search(search(".", "needle", 1)),
            context("owner"),
        )
        .await
        .unwrap();
    assert_eq!(one["matches"].as_array().unwrap().len(), 1);
    assert_eq!(one["truncated"], true);

    let nested = backend
        .execute(
            DeveloperOperation::Search(search("src/nested", "needle", 50)),
            context("owner"),
        )
        .await
        .unwrap();
    assert_eq!(nested["matches"].as_array().unwrap().len(), 2);
    assert_eq!(nested["matches"][0]["path"], "src/nested/b.txt");
    assert_eq!(nested["matches"][1]["path"], "src/nested/c.txt");

    // 输出总量有上限：即使调用方要求很多，也在约 32 KiB 内截断。
    let mut many = String::new();
    for _ in 0..200 {
        many.push_str(&format!("needle {}\n", "x".repeat(300)));
    }
    std::fs::write(root.path().join("many.txt"), many).unwrap();
    let bounded = backend
        .execute(
            DeveloperOperation::Search(search("", "needle", 1000)),
            context("owner"),
        )
        .await
        .unwrap();
    assert!(bounded["matches"].as_array().unwrap().len() < 200);
    assert_eq!(bounded["truncated"], true);
    assert!(bounded.to_string().len() <= MAX_OUTPUT + 1024);

    let file_target = backend
        .execute(
            DeveloperOperation::Search(search("src/a.rs", "needle", 50)),
            context("owner"),
        )
        .await;
    assert!(
        matches!(file_target, Err(ToolExecutionError::Failed(message)) if message.contains("只接受"))
    );
    let escape = backend
        .execute(
            DeveloperOperation::Search(search("../", "needle", 50)),
            context("owner"),
        )
        .await;
    assert!(escape.is_err());
}
#[cfg(unix)]
#[tokio::test]
async fn real_command_status_output_limits_cancellation_and_timeout_kill_descendants() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let backend = LocalDeveloper::new(
        config(root.path()),
        ledger.path().into(),
        Some("SYNTHETIC_TOKEN".into()),
    )
    .unwrap();
    let command = |script: &str, limit| {
        DeveloperOperation::Command(CommandArguments {
            owner_confirmed: false,
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: ".".into(),
            timeout_ms: limit,
        })
    };
    let out = backend
        .execute(
            command("printf 'actual'; printf 'failure' >&2; exit 7", None),
            context("owner"),
        )
        .await
        .unwrap();
    assert_eq!(out["success"], false);
    assert_eq!(out["exit_code"], 7);
    assert_eq!(out["stdout"], "actual");
    assert_eq!(out["stderr"], "failure");
    let out=backend.execute(command("head -c 131072 /dev/zero | tr '\\0' x; printf '%s' \"$GH_TOKEN$EVE_OPENAI_API_KEY\"",None),context("owner")).await.unwrap();
    assert_eq!(out["stdout"].as_str().unwrap().len(), MAX_OUTPUT);
    assert_eq!(out["stdout_truncated"], true);
    assert!(
        backend
            .execute(
                command("(sleep 0.3; touch should-not-exist) & wait", Some(50)),
                context("owner")
            )
            .await
            .is_err()
    );
    let ctx = context("owner");
    let cancel = ctx.cancellation_handle();
    let execution = backend.execute(
        command("(sleep 0.3; touch cancelled-child) & wait", None),
        ctx,
    );
    let cancel_task = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
    };
    let (result, _) = tokio::join!(execution, cancel_task);
    assert!(matches!(result, Err(ToolExecutionError::Cancelled(_))));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!root.path().join("should-not-exist").exists());
    assert!(!root.path().join("cancelled-child").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn upgrade_runs_real_rust_checks_and_builds_an_immutable_candidate_without_activation() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("crates/eve-app/src/bin")).unwrap();
    fs::write(
        root.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/eve-app\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    fs::write(
        root.path().join("crates/eve-app/Cargo.toml"),
        "[package]\nname = \"eve-app\"\nversion = \"0.0.1\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        root.path().join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"eve-app\"\nversion = \"0.0.1\"\n",
    )
    .unwrap();
    fs::write(
        root.path().join("crates/eve-app/src/bin/eve-qqbot.rs"),
        "fn main() {\n    println!(\"synthetic candidate\");\n}\n",
    )
    .unwrap();
    fs::write(root.path().join(".gitignore"), "/target/\n").unwrap();
    fs::write(root.path().join("AGENT.md"), "合成候选身份资料").unwrap();
    fs::create_dir_all(root.path().join("connectors/qqbot")).unwrap();
    for name in [
        "bridge.mjs",
        "bridge-core.mjs",
        "package.json",
        "package-lock.json",
    ] {
        fs::write(root.path().join("connectors/qqbot").join(name), "{}").unwrap();
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let wrapper = root.path().join("cargo-fixture.sh");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexport CARGO_TARGET_DIR='{}'\nexec '{}' \"$@\"\n",
            root.path().join("target").display(),
            cargo.replace('\'', "'\\''")
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    }
    for args in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(root.path())
                .status()
                .unwrap()
                .success()
        );
    }
    let mut c = config(root.path());
    c.self_upgrade = true;
    c.cargo_program = wrapper.to_string_lossy().into_owned();
    c.command_timeout_ms = 60_000;
    // 工作区即使残留旧调试产物，也只能发布本次优化构建生成的候选。
    fs::create_dir_all(root.path().join("target/debug")).unwrap();
    fs::write(
        root.path().join("target/debug/eve-qqbot"),
        b"stale debug output",
    )
    .unwrap();
    let backend = LocalDeveloper::new(c, ledger.path().into(), None).unwrap();
    let result = backend
        .execute(DeveloperOperation::PrepareUpgrade, context("owner"))
        .await
        .unwrap();
    assert_eq!(result["success"], true, "{result}");
    assert_eq!(result["prepared"], true);
    assert_eq!(result["applied"], false);
    assert_eq!(result["checks"].as_array().unwrap().len(), 4);
    // 完整检查复用 release；自升级不能再生成第二套 debug 依赖与测试产物。
    assert_eq!(
        fs::read_dir(root.path().join("target/debug"))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(
        fs::read(root.path().join("target/debug/eve-qqbot")).unwrap(),
        b"stale debug output"
    );
    let candidate = PathBuf::from(result["candidate_path"].as_str().unwrap());
    assert_eq!(
        hash(&fs::read(&candidate).unwrap()),
        result["binary_sha256"]
    );
    assert!(
        std::process::Command::new(&candidate)
            .status()
            .unwrap()
            .success()
    );
    // 后来的构建输出变化不能改写已准备的候选。
    fs::write(root.path().join("target/release/eve-qqbot"), b"later build").unwrap();
    assert_eq!(
        hash(&fs::read(&candidate).unwrap()),
        result["binary_sha256"]
    );
    assert!(candidate.parent().unwrap().join("manifest.json").is_file());
    let resource = candidate.parent().unwrap().join("resources/AGENT.md");
    assert_eq!(fs::read_to_string(&resource).unwrap(), "合成候选身份资料");
    fs::write(root.path().join("AGENT.md"), "后来的源文件修改").unwrap();
    assert_eq!(fs::read_to_string(resource).unwrap(), "合成候选身份资料");
    // 结束上面的构建输出篡改场景；下一次只改变运行资源，保持原二进制。
    fs::copy(&candidate, root.path().join("target/release/eve-qqbot")).unwrap();
    let later = backend
        .execute(DeveloperOperation::PrepareUpgrade, context("owner"))
        .await
        .unwrap();
    assert_eq!(later["binary_sha256"], result["binary_sha256"]);
    assert_ne!(later["candidate_sha256"], result["candidate_sha256"]);
    // 真正的 Rust 生产者与 Python 消费者必须对候选/资源身份算出同一摘要。
    let verifier =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/self_upgrade_runtime.py");
    let request = json!({"format_version":1,"candidate_sha256":later["candidate_sha256"],
        "candidate_directory":Path::new(later["candidate_path"].as_str().unwrap()).parent().unwrap(),
        "session_id":"test-session","requested_by":"owner","turn_id":1});
    assert!(std::process::Command::new("python3").args(["-c",
        "import importlib.util,json,pathlib,sys; s=importlib.util.spec_from_file_location('upgrade',sys.argv[1]); m=importlib.util.module_from_spec(s); s.loader.exec_module(m); m.validate_candidate(json.loads(sys.argv[2]),pathlib.Path(sys.argv[3]))",
        verifier.to_str().unwrap(), &request.to_string(), ledger.path().join("candidates").to_str().unwrap()]).status().unwrap().success());
}

#[tokio::test]
async fn live_upgrade_is_authorized_bound_to_turn_and_idempotently_queued_without_applying() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let queue = tempfile::tempdir().unwrap();
    let mut c = config(root.path());
    c.self_upgrade = true;
    c.live_upgrade = Some(queue.path().into());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let status = UpgradeSupervisor {
        format_version: 1,
        pid: 1,
        heartbeat_ms: now,
        phase: "running".into(),
        active_sha256: "b".repeat(64),
        candidate_sha256: None,
        result: None,
        error: None,
    };
    fs::write(
        queue.path().join("supervisor.json"),
        serde_json::to_vec(&status).unwrap(),
    )
    .unwrap();
    let id = "a".repeat(64);
    let candidate = ledger.path().join("candidates").join(&id);
    fs::create_dir_all(&candidate).unwrap();
    fs::write(candidate.join("manifest.json"), json!({"format_version":1,"prepared":true,"success":true,
        "candidate_sha256":id,"checks":[{"success":true},{"success":true},{"success":true},{"success":true}]}).to_string()).unwrap();
    let backend = LocalDeveloper::new(c, ledger.path().into(), None).unwrap();
    let operation = DeveloperOperation::ApplyUpgrade(ApplyUpgradeArguments {
        candidate_sha256: id.clone(),
    });
    assert!(
        backend
            .execute(operation.clone(), context("stranger").with_turn_id(Some(1)))
            .await
            .is_err()
    );
    assert!(
        backend
            .execute(operation.clone(), context("owner"))
            .await
            .is_err()
    );
    assert!(!queue.path().join("request.json").exists());
    let result = backend
        .execute(operation.clone(), context("owner").with_turn_id(Some(1)))
        .await
        .unwrap();
    assert_eq!(result["queued"], true);
    assert_eq!(result["applied"], false);
    let before = fs::read(queue.path().join("request.json")).unwrap();
    let request: UpgradeRequest = serde_json::from_slice(&before).unwrap();
    assert_eq!(request.turn_id, 1);
    assert_eq!(request.requested_by, "owner");
    backend
        .execute(operation.clone(), context("owner").with_turn_id(Some(1)))
        .await
        .unwrap();
    assert_eq!(fs::read(queue.path().join("request.json")).unwrap(), before);
    assert!(
        backend
            .execute(operation, context("owner").with_turn_id(Some(2)))
            .await
            .is_err()
    );
    assert_eq!(fs::read(queue.path().join("request.json")).unwrap(), before);
    let mut stale = status;
    stale.heartbeat_ms = 0;
    fs::write(
        queue.path().join("supervisor.json"),
        serde_json::to_vec(&stale).unwrap(),
    )
    .unwrap();
    assert!(
        backend
            .execute(DeveloperOperation::UpgradeStatus, context("owner"))
            .await
            .is_err()
    );
}

/// 命令策略：禁止的不执行，需确认的没有明确同意时不执行，同意后照常执行。
#[cfg(unix)]
/// 测试用策略：脚本含 forbidden 为禁止，含 careful 为需确认。
struct Policy;
impl eve_developer_api::CommandPolicy for Policy {
    fn evaluate(&self, program: &str, args: &[String]) -> eve_developer_api::CommandEvaluation {
        use eve_developer_api::{CommandDecision, CommandEvaluation, CommandRuleMatch};
        let script = args.get(1).cloned().unwrap_or_default();
        let decision = if script.contains("forbidden") {
            Some(CommandDecision::Forbidden)
        } else if script.contains("careful") {
            Some(CommandDecision::Prompt)
        } else {
            None
        };
        CommandEvaluation {
            decision,
            matches: decision
                .map(|decision| CommandRuleMatch {
                    matched_prefix: vec![program.into()],
                    decision,
                    justification: Some("测试规则".into()),
                })
                .into_iter()
                .collect(),
        }
    }
}
#[tokio::test]
async fn command_policy_blocks_or_requires_confirmation_before_anything_runs() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let backend = LocalDeveloper::new(config(root.path()), ledger.path().into(), None)
        .unwrap()
        .with_command_policy(Arc::new(Policy));
    let command = |marker: &str, confirmed| {
        DeveloperOperation::Command(CommandArguments {
            owner_confirmed: confirmed,
            program: "sh".into(),
            args: vec!["-c".into(), format!("touch {marker}")],
            cwd: ".".into(),
            timeout_ms: None,
        })
    };
    let error = backend
        .execute(command("forbidden", true), context("owner"))
        .await
        .unwrap_err();
    assert!(format!("{error:?}").contains("禁止"), "{error:?}");
    let error = backend
        .execute(command("careful", false), context("owner"))
        .await
        .unwrap_err();
    assert!(
        format!("{error:?}").contains("owner_confirmed"),
        "{error:?}"
    );
    assert!(!root.path().join("forbidden").exists() && !root.path().join("careful").exists());
    let out = backend
        .execute(command("careful", true), context("owner"))
        .await
        .unwrap();
    assert_eq!(out["success"], true);
    assert!(root.path().join("careful").exists());
    backend
        .execute(command("plain", false), context("owner"))
        .await
        .unwrap();
    assert!(root.path().join("plain").exists());
}

#[tokio::test]
async fn action_reviewer_decides_prompt_commands_from_user_words_and_fails_closed() {
    use eve_llm_api::{ChatMessage, ChatRole};
    use std::sync::Mutex;
    struct Reviewer {
        verdict: Result<ActionReviewVerdict, String>,
        seen: Mutex<Vec<ActionReviewRequest>>,
    }
    impl ActionReviewer for Reviewer {
        fn review(&self, request: ActionReviewRequest) -> ActionReviewFuture<'_> {
            self.seen.lock().unwrap().push(request);
            let verdict = self.verdict.clone();
            Box::pin(async move { verdict })
        }
    }
    let verdict = |risk, authorization, allow| {
        Ok(ActionReviewVerdict {
            risk,
            authorization,
            allow,
            rationale: "测试理由".into(),
        })
    };
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let with = |reviewer: Arc<Reviewer>| {
        LocalDeveloper::new(config(root.path()), ledger.path().into(), None)
            .unwrap()
            .with_command_policy(Arc::new(Policy))
            .with_action_reviewer(reviewer)
    };
    let command = |marker: &str| {
        DeveloperOperation::Command(CommandArguments {
            owner_confirmed: false,
            program: "sh".into(),
            args: vec!["-c".into(), format!("touch {marker}")],
            cwd: ".".into(),
            timeout_ms: None,
        })
    };
    let conversation = Arc::new(vec![
        ChatMessage::text(ChatRole::System, "系统提示不交给审查者"),
        ChatMessage::text(ChatRole::User, "把 careful 文件建出来"),
        ChatMessage::text(ChatRole::Assistant, "好"),
    ]);
    let talking = || context("owner").with_conversation(conversation.clone());

    // 审查放行：不需要模型自带 owner_confirmed。
    let allow = Arc::new(Reviewer {
        verdict: verdict(ReviewRisk::Medium, ReviewAuthorization::High, true),
        seen: Mutex::new(vec![]),
    });
    let out = with(allow.clone())
        .execute(command("careful-allowed"), talking())
        .await
        .unwrap();
    assert_eq!(out["success"], true);
    assert!(root.path().join("careful-allowed").exists());
    let seen = allow.seen.lock().unwrap().pop().unwrap();
    assert_eq!(
        seen.user_messages,
        ["把 careful 文件建出来"],
        "只交用户原话"
    );
    assert!(!seen.delegated && !seen.claimed_confirmation);
    assert!(
        seen.action.contains("touch careful-allowed"),
        "{}",
        seen.action
    );
    assert_eq!(seen.policy_reason, "sh（测试规则）");

    // 审查者同意但等级不够、审查者拒绝、审查失败：都不执行。
    for (name, reviewer_verdict) in [
        (
            "careful-high-risk",
            verdict(ReviewRisk::High, ReviewAuthorization::Medium, true),
        ),
        (
            "careful-denied",
            verdict(ReviewRisk::Low, ReviewAuthorization::High, false),
        ),
        ("careful-failed", Err("审查超时".into())),
    ] {
        let reviewer = Arc::new(Reviewer {
            verdict: reviewer_verdict,
            seen: Mutex::new(vec![]),
        });
        let error = with(reviewer)
            .execute(command(name), talking())
            .await
            .unwrap_err();
        assert!(format!("{error:?}").contains("命令未执行"), "{error:?}");
        assert!(!root.path().join(name).exists(), "{name}");
    }

    // 后台委托执行：没有人在场，用户角色消息不作为原话交给审查者。
    let delegated = Arc::new(Reviewer {
        verdict: verdict(ReviewRisk::Medium, ReviewAuthorization::Low, false),
        seen: Mutex::new(vec![]),
    });
    let error = with(delegated.clone())
        .execute(command("careful-delegated"), talking().with_delegated(true))
        .await
        .unwrap_err();
    assert!(format!("{error:?}").contains("没有人在场"), "{error:?}");
    let seen = delegated.seen.lock().unwrap().pop().unwrap();
    assert!(seen.delegated && seen.user_messages.is_empty());

    // 不需要确认的命令不经过审查；禁止的命令也不交给审查者放行。
    let untouched = Arc::new(Reviewer {
        verdict: verdict(ReviewRisk::Low, ReviewAuthorization::High, true),
        seen: Mutex::new(vec![]),
    });
    let backend = with(untouched.clone());
    backend.execute(command("plain"), talking()).await.unwrap();
    assert!(
        backend
            .execute(command("forbidden"), talking())
            .await
            .is_err()
    );
    assert!(untouched.seen.lock().unwrap().is_empty());
    assert!(!root.path().join("forbidden").exists());
}
