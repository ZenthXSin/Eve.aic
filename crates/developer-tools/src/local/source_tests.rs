use super::*;
use eve_llm_api::ContextScope;
use std::{process::Command as GitCommand, sync::Arc, time::SystemTime};

struct Fixture {
    root: tempfile::TempDir,
    ledger: tempfile::TempDir,
    queue: tempfile::TempDir,
    backend: LocalDeveloper,
    baseline: String,
}

#[tokio::test]
async fn private_sessions_and_candidate_snapshots_preserve_shared_dirty_work_and_indices() {
    let mut fixture = Fixture::new(None);
    fixture.backend.config.host_filesystem = true;
    let external = tempfile::tempdir().unwrap();
    fs::write(external.path().join("external.txt"), "工程外资料\n").unwrap();
    let read: FileArguments =
        serde_json::from_value(json!({"path":external.path().join("external.txt")})).unwrap();
    assert_eq!(
        fixture
            .backend
            .execute(DeveloperOperation::Read(read), context())
            .await
            .unwrap()["content"],
        "工程外资料\n"
    );
    fs::write(fixture.root.path().join("main.txt"), "另一会话还没做完\n").unwrap();
    fs::write(
        fixture.root.path().join("someone-new.txt"),
        "别人的未跟踪文件\n",
    )
    .unwrap();
    let shared_index = fs::read(fixture.root.path().join(".git/index")).unwrap();
    let first_context = context();
    let second_context = context().with_scope(Some(ContextScope {
        session_id: "another-session".into(),
        user_id: "owner".into(),
    }));
    let first = fixture
        .backend
        .session_backend(&first_context)
        .await
        .unwrap();
    let second = fixture
        .backend
        .session_backend(&second_context)
        .await
        .unwrap();
    assert_ne!(first.root, second.root);
    assert_eq!(
        fs::read_to_string(first.root.join("main.txt")).unwrap(),
        "baseline behavior\n"
    );
    assert!(!first.root.join("someone-new.txt").exists());
    fs::write(first.root.join("main.txt"), "本会话修复\n").unwrap();
    fs::write(first.root.join("new.rs"), "fn test() {}\n").unwrap();
    git(&first.root, &["add", "main.txt"]);
    let session_index = fs::read(first.root.join(".git/index")).unwrap();
    let snapshot = first.upgrade_snapshot(&first_context).await.unwrap();
    assert_eq!(
        fs::read_to_string(snapshot.root.join("main.txt")).unwrap(),
        "本会话修复\n"
    );
    assert_eq!(
        fs::read_to_string(snapshot.root.join("new.rs")).unwrap(),
        "fn test() {}\n"
    );
    assert_eq!(git(&snapshot.root, &["status", "--porcelain=v1"]), "");
    assert_eq!(
        git(&first.root, &["rev-parse", "HEAD"]).trim(),
        fixture.baseline
    );
    assert_eq!(
        fs::read(first.root.join(".git/index")).unwrap(),
        session_index
    );
    assert_eq!(
        fs::read_to_string(second.root.join("main.txt")).unwrap(),
        "baseline behavior\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("main.txt")).unwrap(),
        "另一会话还没做完\n"
    );
    assert!(fixture.root.path().join("someone-new.txt").exists());
    assert_eq!(
        fs::read(fixture.root.path().join(".git/index")).unwrap(),
        shared_index
    );
    let reused = first.upgrade_snapshot(&first_context).await.unwrap();
    assert_eq!(snapshot.root, reused.root);
    let evidence = snapshot.source_fingerprint(&first_context).await.unwrap();
    assert!(evidence.diff.contains("本会话修复"));
    assert!(!evidence.diff.contains("另一会话"));
    assert!(!evidence.diff.contains("工程外资料"));
    let reopened = fixture
        .backend
        .session_backend(&first_context)
        .await
        .unwrap();
    assert_eq!(reopened.root, first.root);
    assert_eq!(
        fs::read_to_string(reopened.root.join("main.txt")).unwrap(),
        "本会话修复\n"
    );
}

#[tokio::test]
async fn candidate_merges_online_changes_without_moving_a_dirty_session_and_invalidates_cache() {
    let fixture = Fixture::new(None);
    let ctx = context();
    let first = fixture.backend.session_backend(&ctx).await.unwrap();
    fs::write(first.root.join("ours.txt"), "本会话\n").unwrap();
    let old = first.upgrade_snapshot(&ctx).await.unwrap();
    // 第二个会话的在线快照只存在于候选对象库，共享源码没有该提交。
    let other_ctx = context().with_scope(Some(ContextScope {
        session_id: "other".into(),
        user_id: "owner".into(),
    }));
    let other = fixture.backend.session_backend(&other_ctx).await.unwrap();
    fs::write(other.root.join("online.txt"), "已上线的其他修复\n").unwrap();
    let deployed = other.upgrade_snapshot(&other_ctx).await.unwrap();
    let online = git(&deployed.root, &["rev-parse", "HEAD"]);
    fixture.supervisor(Some(online.trim()));
    let before_index = fs::read(first.root.join(".git/index")).unwrap();
    let merged = first.upgrade_snapshot(&ctx).await.unwrap();
    assert_ne!(old.root, merged.root);
    assert_eq!(
        fs::read_to_string(merged.root.join("ours.txt")).unwrap(),
        "本会话\n"
    );
    assert_eq!(
        fs::read_to_string(merged.root.join("online.txt")).unwrap(),
        "已上线的其他修复\n"
    );
    assert!(!first.root.join("online.txt").exists());
    assert_eq!(
        git(&first.root, &["rev-parse", "HEAD"]).trim(),
        fixture.baseline
    );
    assert_eq!(
        fs::read(first.root.join(".git/index")).unwrap(),
        before_index
    );
    git(
        &merged.root,
        &["merge-base", "--is-ancestor", online.trim(), "HEAD"],
    );
    let evidence = merged.source_fingerprint(&ctx).await.unwrap();
    assert!(evidence.diff.contains("本会话"));
    assert!(!evidence.diff.contains("已上线的其他修复"));
    let new_ctx = context().with_scope(Some(ContextScope {
        session_id: "new-after-upgrade".into(),
        user_id: "owner".into(),
    }));
    let new = fixture.backend.session_backend(&new_ctx).await.unwrap();
    assert_eq!(git(&new.root, &["rev-parse", "HEAD"]), online);
    assert!(new.root.join("online.txt").exists());
}

#[tokio::test]
async fn candidate_conflicts_and_unsafe_files_preserve_all_original_work() {
    let fixture = Fixture::new(None);
    let ctx = context();
    let private = fixture.backend.session_backend(&ctx).await.unwrap();
    fs::write(private.root.join("main.txt"), "本会话未完成\n").unwrap();
    let online = fixture.commit_file("main.txt", "别人的在线修复\n");
    fixture.supervisor(Some(&online));
    let private = fixture.backend.session_backend(&ctx).await.unwrap();
    let before_index = fs::read(private.root.join(".git/index")).unwrap();
    let error = private.upgrade_snapshot(&ctx).await.err().unwrap();
    assert!(error.to_string().contains("main.txt"));
    assert_eq!(
        fs::read_to_string(private.root.join("main.txt")).unwrap(),
        "本会话未完成\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("main.txt")).unwrap(),
        "别人的在线修复\n"
    );
    assert_eq!(
        fs::read(private.root.join(".git/index")).unwrap(),
        before_index
    );
    fs::write(private.root.join(".env.private"), "sensitive\n").unwrap();
    let error = private.upgrade_snapshot(&ctx).await.err().unwrap();
    assert!(error.to_string().contains("凭据"));
    fixture.no_candidate_or_queue();
}

fn git(root: &Path, args: &[&str]) -> String {
    let result = GitCommand::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}

fn context() -> ToolExecutionContext {
    ToolExecutionContext::new()
        .with_scope(Some(ContextScope {
            session_id: "source-review-session".into(),
            user_id: "owner".into(),
        }))
        .with_turn_id(Some(9))
        .with_call_id("source-review-call".into())
        .with_review_user_messages(Arc::new(vec!["完成验证后升级到新提交".into()]))
}

impl Fixture {
    fn new(token: Option<&str>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let ledger = tempfile::tempdir().unwrap();
        let queue = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        git(
            root.path(),
            &["config", "user.email", "source-tests@example.invalid"],
        );
        git(root.path(), &["config", "user.name", "Source Tests"]);
        git(root.path(), &["config", "core.autocrlf", "false"]);
        fs::write(root.path().join("main.txt"), "baseline behavior\n").unwrap();
        git(root.path(), &["add", "main.txt"]);
        git(root.path(), &["commit", "-qm", "baseline"]);
        let baseline = git(root.path(), &["rev-parse", "HEAD"]).trim().to_owned();
        let config: DeveloperConfig = serde_json::from_value(json!({
            "workspace":root.path(), "operators":["owner"], "commands":true,
            "self_upgrade":true,"live_upgrade":queue.path(),"command_timeout_ms":10000
        }))
        .unwrap();
        let backend =
            LocalDeveloper::new(config, ledger.path().into(), token.map(str::to_owned)).unwrap();
        let fixture = Self {
            root,
            ledger,
            queue,
            backend,
            baseline,
        };
        fixture.supervisor(Some(&fixture.baseline));
        fixture
    }

    fn supervisor(&self, revision: Option<&str>) {
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        fs::write(self.queue.path().join("supervisor.json"), json!({
            "format_version":LIVE_UPGRADE_VERSION,"pid":std::process::id(),"heartbeat_ms":now,
            "phase":"running","active_sha256":"a".repeat(64),"candidate_sha256":null,
            "result":null,"error":null,"restart_supported":true,"active_source_revision":revision,
            "active_candidate_sha256":null,"host_nonce":"b".repeat(64)
        }).to_string()).unwrap();
    }

    fn commit_file(&self, name: &str, bytes: impl AsRef<[u8]>) -> String {
        let path = self.root.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
        git(self.root.path(), &["add", "--", name]);
        git(self.root.path(), &["commit", "-qm", "candidate change"]);
        git(self.root.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned()
    }

    fn no_candidate_or_queue(&self) {
        assert!(!self.ledger.path().join("candidates").exists());
        assert!(!self.queue.path().join("request.json").exists());
    }
}

#[tokio::test]
async fn committed_changes_are_reviewed_against_the_running_revision_without_textconv() {
    let fixture = Fixture::new(None);
    fs::write(
        fixture.root.path().join(".gitattributes"),
        "*.txt diff=evidence-test\n",
    )
    .unwrap();
    git(fixture.root.path(), &["add", ".gitattributes"]);
    let target = fixture.commit_file("main.txt", "new committed behavior\n完整中文证据\n");
    git(
        fixture.root.path(),
        &["config", "diff.external", "must-not-execute-diff-helper"],
    );
    git(
        fixture.root.path(),
        &[
            "config",
            "diff.evidence-test.textconv",
            "must-not-execute-textconv-helper",
        ],
    );
    let evidence = fixture
        .backend
        .source_fingerprint(&context())
        .await
        .unwrap();
    assert_eq!(evidence.fingerprint["head"], target);
    assert_eq!(evidence.fingerprint["diff"], hash(b""));
    assert_eq!(evidence.fingerprint["untracked"], hash(b""));
    assert_eq!(evidence.descriptor["base_revision"], fixture.baseline);
    assert_eq!(evidence.descriptor["target_revision"], target);
    assert!(evidence.diff.contains("-baseline behavior\n"));
    assert!(
        evidence
            .diff
            .contains("+new committed behavior\n+完整中文证据\n")
    );
    let expected = git(
        fixture.root.path(),
        &[
            "--no-pager",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "--no-renames",
            &fixture.baseline,
            "--",
        ],
    );
    assert_eq!(evidence.diff, expected);
    assert_eq!(evidence.descriptor["sha256"], hash(expected.as_bytes()));
    fixture.no_candidate_or_queue();
}

#[tokio::test]
async fn untracked_dirty_staged_and_unknown_baseline_sources_never_become_evidence() {
    let fixture = Fixture::new(None);
    for baseline in [None, Some("invalid-revision")] {
        fixture.supervisor(baseline);
        let error = fixture
            .backend
            .source_fingerprint(&context())
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("在线源码基线"));
    }
    fixture.supervisor(Some(&fixture.baseline));
    fs::write(
        fixture.root.path().join("new-code.rs"),
        "fn new_code() {}\n",
    )
    .unwrap();
    let error = fixture
        .backend
        .source_fingerprint(&context())
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("未跟踪"));
    git(fixture.root.path(), &["add", "new-code.rs"]);
    let error = fixture
        .backend
        .source_fingerprint(&context())
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("先提交"));
    git(fixture.root.path(), &["reset", "--hard", "HEAD"]);
    fs::write(
        fixture.root.path().join("main.txt"),
        "uncommitted behavior\n",
    )
    .unwrap();
    assert!(
        fixture
            .backend
            .source_fingerprint(&context())
            .await
            .is_err()
    );
    fixture.no_candidate_or_queue();
}

#[tokio::test]
async fn assume_unchanged_and_skip_worktree_cannot_hide_modified_source() {
    let fixture = Fixture::new(None);
    for (enable, disable) in [
        ("--assume-unchanged", "--no-assume-unchanged"),
        ("--skip-worktree", "--no-skip-worktree"),
    ] {
        git(fixture.root.path(), &["update-index", enable, "main.txt"]);
        fs::write(
            fixture.root.path().join("main.txt"),
            "hidden source change\n",
        )
        .unwrap();
        assert_eq!(git(fixture.root.path(), &["diff", "HEAD", "--"]), "");
        let error = fixture
            .backend
            .source_fingerprint(&context())
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("源码索引含跳过检查"));
        git(fixture.root.path(), &["update-index", disable, "main.txt"]);
        git(fixture.root.path(), &["checkout", "--", "main.txt"]);
    }
    fixture.no_candidate_or_queue();
}

#[tokio::test]
async fn raw_known_tokens_and_private_key_variants_are_rejected_without_redacting_evidence() {
    let token = "host-token-for-source-evidence";
    let fixture = Fixture::new(Some(token));
    for body in [
        format!("committed data {token}\n"),
        "-----BEGIN PRIVATE KEY-----\nprivate\n".into(),
        "-----BEGIN RSA PRIVATE KEY-----\nprivate\n".into(),
        "-----BEGIN OPENSSH PRIVATE KEY-----\nprivate\n".into(),
        "-----BEGIN EC PRIVATE KEY-----\nprivate\n".into(),
        "-----BEGIN DSA PRIVATE KEY-----\nprivate\n".into(),
        "-----BEGIN ENCRYPTED PRIVATE KEY-----\nprivate\n".into(),
    ] {
        fixture.commit_file("main.txt", body);
        let error = fixture
            .backend
            .source_fingerprint(&context())
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("包含凭据"));
        assert!(!error.to_string().contains(token));
        assert!(!error.to_string().contains("PRIVATE KEY"));
    }
    fixture.no_candidate_or_queue();
}

#[tokio::test]
async fn credential_paths_binary_patches_and_gitlinks_are_not_reviewable_source_changes() {
    let fixture = Fixture::new(None);
    fixture.commit_file(".env.production", "private setting\n");
    let error = fixture
        .backend
        .source_fingerprint(&context())
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("凭据或身份授权账本"));
    let head = git(fixture.root.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    fixture.supervisor(Some(&head));
    fixture.commit_file("image.dat", b"binary\0payload\0bytes");
    let error = fixture
        .backend
        .source_fingerprint(&context())
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("二进制或子模块"));
    let head = git(fixture.root.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    fixture.supervisor(Some(&head));
    git(
        fixture.root.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            &head,
            "dependency",
        ],
    );
    git(fixture.root.path(), &["commit", "-qm", "add gitlink"]);
    fs::create_dir(fixture.root.path().join("dependency")).unwrap();
    let error = fixture
        .backend
        .source_fingerprint(&context())
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("二进制或子模块"));
    fixture.no_candidate_or_queue();
}

fn manifest(diff: &[u8]) -> Value {
    json!({"candidate_sha256":"c".repeat(64),
        "source":{"head":"b".repeat(40),"diff":hash(b""),"untracked":hash(b"")},
        "source_diff":{"path":"source-diff.patch","sha256":hash(diff),
            "base_revision":"a".repeat(40),"target_revision":"b".repeat(40)}})
}

#[test]
fn frozen_source_diff_requires_exact_descriptor_hash_utf8_and_capacity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source-diff.patch");
    let diff = b"diff --git a/main.txt b/main.txt\n-old\n+new\n";
    let original = manifest(diff);
    fs::write(&path, diff).unwrap();
    assert_eq!(
        source::read_source_diff(directory.path(), &original)
            .unwrap()
            .as_bytes(),
        diff
    );
    fs::write(&path, b"changed after approval").unwrap();
    assert!(source::read_source_diff(directory.path(), &original).is_err());
    fs::write(&path, diff).unwrap();
    for (field, value) in [
        ("path", "../source-diff.patch"),
        ("sha256", "invalid"),
        ("base_revision", "HEAD"),
        ("target_revision", "a"),
    ] {
        let mut changed = original.clone();
        changed["source_diff"][field] = json!(value);
        assert!(source::read_source_diff(directory.path(), &changed).is_err());
    }
    for field in ["diff", "untracked"] {
        let mut changed = original.clone();
        changed["source"][field] = json!(hash(b"dirty"));
        assert!(source::read_source_diff(directory.path(), &changed).is_err());
    }
    let invalid_utf8 = [0xff, 0xfe];
    fs::write(&path, invalid_utf8).unwrap();
    assert!(source::read_source_diff(directory.path(), &manifest(&invalid_utf8)).is_err());
    let oversized = vec![b'x'; LIVE_UPGRADE_MAX_SOURCE_DIFF + 1];
    fs::write(&path, &oversized).unwrap();
    assert!(source::read_source_diff(directory.path(), &manifest(&oversized)).is_err());
    assert_eq!(fs::read(&path).unwrap(), oversized);
}

#[test]
fn stored_binary_and_submodule_diffs_are_rejected_even_with_matching_hashes() {
    let directory = tempfile::tempdir().unwrap();
    for marker in [
        "GIT binary patch",
        "Binary files a/x and b/x differ",
        "Submodule dependency changed",
        "index abc1234..def5678 160000",
        "new file mode 160000",
        "deleted file mode 160000",
        "old mode 160000",
        "new mode 160000",
    ] {
        let diff = format!("diff --git a/x b/x\n{marker}\n");
        fs::write(directory.path().join("source-diff.patch"), &diff).unwrap();
        assert!(source::read_source_diff(directory.path(), &manifest(diff.as_bytes())).is_err());
    }
}

#[test]
fn candidate_capacity_keeps_complete_action_without_repeating_user_history() {
    let fixture = Fixture::new(None);
    let diff = "a".repeat(96 * 1024);
    let mut candidate = manifest(diff.as_bytes());
    candidate["source_diff"]["base_revision"] = json!(fixture.baseline);
    let messages = (0..6)
        .map(|index| format!("用户正文 {index}：{}结束", "原话".repeat(1500)))
        .collect::<Vec<_>>();
    let ctx = context().with_review_user_messages(Arc::new(messages.clone()));
    assert!(
        fixture
            .backend
            .validate_candidate_review_capacity(&candidate, &diff, &ctx)
            .is_ok()
    );
    candidate["checks"] = json!([{"success":true,"stdout":"v".repeat(176 * 1024)}]);
    let unchanged = candidate.clone();
    let error = fixture
        .backend
        .validate_candidate_review_capacity(&candidate, &diff, &ctx)
        .err()
        .unwrap();
    assert!(error.to_string().contains("超过系统安全审查容量"));
    assert_eq!(candidate, unchanged);
    assert_eq!(ctx.review_user_messages().unwrap(), messages.as_slice());
    candidate.as_object_mut().unwrap().remove("checks");
    let long_messages = (0..6)
        .map(|index| format!("第 {index} 条{}尾部", "z".repeat(50 * 1024)))
        .collect::<Vec<_>>();
    let ctx = context().with_review_user_messages(Arc::new(long_messages.clone()));
    assert!(
        fixture
            .backend
            .validate_candidate_review_capacity(&candidate, "", &ctx)
            .is_ok()
    );
    assert_eq!(
        ctx.review_user_messages().unwrap(),
        long_messages.as_slice()
    );
    fixture.supervisor(Some(&"d".repeat(40)));
    assert!(
        fixture
            .backend
            .validate_candidate_review_capacity(&candidate, "", &context())
            .err()
            .unwrap()
            .to_string()
            .contains("在线源码基线改变")
    );
    fixture.no_candidate_or_queue();
}

#[cfg(unix)]
fn prepare_fixture() -> (Fixture, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let mut fixture = Fixture::new(None);
    fixture.commit_file(
        "crates/eve-app/Cargo.toml",
        "[package]\nname = \"eve-app\"\nversion = \"0.1.0\"\n",
    );
    let marker = fixture.root.path().join("cargo-was-executed");
    let wrapper = fixture.ledger.path().join("malicious-cargo");
    fs::write(
        &wrapper,
        "#!/bin/sh\nprintf '%s' \"$CARGO_BUILD_JOBS\" > cargo-was-executed\nexit 19\n",
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    fixture.backend.config.cargo_program = wrapper.to_string_lossy().into_owned();
    fixture.backend.config.cargo_arguments = vec!["--offline".into()];
    (fixture, marker)
}

#[cfg(unix)]
#[tokio::test]
async fn prepare_runs_real_checks_without_a_review_service_or_user_step_approval() {
    for with_originals in [false, true] {
        let (mut fixture, marker) = prepare_fixture();
        fixture.backend.config.action_review = false;
        let ctx = if with_originals {
            context()
        } else {
            context()
                .with_review_user_messages(Arc::new(vec![]))
                .with_delegated(true)
        };
        let result = fixture
            .backend
            .execute(DeveloperOperation::PrepareUpgrade, ctx)
            .await
            .unwrap();
        assert_eq!(result["prepared"], false);
        assert_eq!(result["checks"][0]["exit_code"], 19);
        let expected_jobs = std::env::var("CARGO_BUILD_JOBS").unwrap_or_else(|_| "1".into());
        assert_eq!(fs::read_to_string(marker).unwrap(), expected_jobs);
        assert!(!fixture.ledger.path().join("action-reviews.jsonl").exists());
        fixture.no_candidate_or_queue();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn prepare_freezes_source_and_rejects_a_changed_snapshot_before_build() {
    let (fixture, marker) = prepare_fixture();
    let frozen = fixture
        .backend
        .review_operation(42, &DeveloperOperation::PrepareUpgrade, &context())
        .await
        .unwrap()
        .into_source()
        .unwrap();
    fs::write(
        fixture.root.path().join("main.txt"),
        "changed after snapshot\n",
    )
    .unwrap();
    git(fixture.root.path(), &["add", "main.txt"]);
    git(
        fixture.root.path(),
        &["commit", "-qm", "change after snapshot"],
    );
    let error = fixture
        .backend
        .upgrade(&context(), Some(&frozen))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("冻结后源码已变化"));
    assert!(!marker.exists());
    fixture.no_candidate_or_queue();
}
