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
        commands: true,
        github: true,
        github_write_repositories: BTreeSet::from(["test/repo".into()]),
        github_token_env: "EVE_GITHUB_TOKEN".into(),
        command_timeout_ms: 10_000,
        max_tool_rounds: 8,
        self_upgrade: false,
        cargo_program: "cargo".into(),
        cargo_arguments: vec![],
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
async fn tools_reject_identity_urls_and_extra_arguments_and_only_register_enabled_services() {
    let root = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let mut c = config(root.path());
    c.commands = false;
    c.github = false;
    let backend = Arc::new(LocalDeveloper::new(c.clone(), ledger.path().into(), None).unwrap());
    let tools = crate::tools(&c, backend);
    assert_eq!(tools.len(), 3);
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
    let backend = LocalDeveloper::new(c, ledger.path().into(), None).unwrap();
    let result = backend
        .execute(DeveloperOperation::PrepareUpgrade, context("owner"))
        .await
        .unwrap();
    assert_eq!(result["success"], true, "{result}");
    assert_eq!(result["prepared"], true);
    assert_eq!(result["applied"], false);
    assert_eq!(result["checks"].as_array().unwrap().len(), 4);
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
    fs::write(root.path().join("target/debug/eve-qqbot"), b"later build").unwrap();
    assert_eq!(
        hash(&fs::read(&candidate).unwrap()),
        result["binary_sha256"]
    );
    assert!(candidate.parent().unwrap().join("manifest.json").is_file());
}
