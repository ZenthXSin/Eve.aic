use super::*;
use eve_llm_api::{ContextScope, SharedToolSteps};
use std::{
    os::unix::fs::{PermissionsExt, symlink},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

fn config(root: &Path) -> DeveloperConfig {
    serde_json::from_value(json!({"workspace":root,"operators":["owner"],"self_upgrade":false}))
        .unwrap()
}
fn context(user: &str) -> ToolExecutionContext {
    ToolExecutionContext::new().with_scope(Some(ContextScope {
        session_id: "snapshot-session".into(),
        user_id: user.into(),
    }))
}
fn request(root: &str) -> WorkspaceSnapshotRequest {
    WorkspaceSnapshotRequest { root: root.into() }
}
fn backend(root: &Path, journal: &Path) -> LocalDeveloper {
    LocalDeveloper::new(config(root), journal.into(), None).unwrap()
}
fn bytes(snapshot: &WorkspaceSnapshot, path: &str) -> Vec<u8> {
    match &snapshot
        .entries
        .iter()
        .find(|entry| entry.path == path)
        .unwrap()
        .contents
    {
        WorkspaceSnapshotContents::File(bytes) => bytes.clone(),
        WorkspaceSnapshotContents::Directory => panic!("expected file"),
    }
}

#[tokio::test]
async fn captures_complete_binary_input_empty_directories_modes_and_explicit_exclusions_without_budget_or_writes()
 {
    let root = tempfile::tempdir().unwrap();
    let journal = tempfile::tempdir().unwrap();
    for name in [
        "empty",
        "target",
        "node_modules/pkg",
        ".git",
        ".eve-developer",
    ] {
        fs::create_dir_all(root.path().join(name)).unwrap();
    }
    fs::write(root.path().join("target/local.bin"), [0, 255, 3]).unwrap();
    fs::write(
        root.path().join("node_modules/pkg/index.js"),
        b"uncommitted dependency",
    )
    .unwrap();
    fs::write(root.path().join("test.cjs"), b"untracked test").unwrap();
    fs::set_permissions(
        root.path().join("test.cjs"),
        fs::Permissions::from_mode(0o751),
    )
    .unwrap();
    fs::set_permissions(root.path().join("empty"), fs::Permissions::from_mode(0o710)).unwrap();
    fs::write(root.path().join(".git/config"), b"metadata not exported").unwrap();
    let backend = backend(root.path(), journal.path());
    let pool = SharedToolSteps::new(7).unwrap();
    let ctx = context("owner").with_shared_steps(pool.clone());
    let snapshot = backend
        .capture_workspace_snapshot(request("."), ctx)
        .await
        .unwrap();
    assert_eq!(snapshot.scope, context("owner").scope().unwrap().clone());
    assert_eq!(snapshot.workspace_root, root.path());
    assert_eq!(snapshot.root, root.path());
    assert_eq!(bytes(&snapshot, "target/local.bin"), [0, 255, 3]);
    assert_eq!(
        bytes(&snapshot, "node_modules/pkg/index.js"),
        b"uncommitted dependency"
    );
    assert_eq!(
        snapshot
            .entries
            .iter()
            .find(|e| e.path == "test.cjs")
            .unwrap()
            .executable_bits,
        0o111
    );
    let empty = snapshot.entries.iter().find(|e| e.path == "empty").unwrap();
    assert_eq!(empty.contents, WorkspaceSnapshotContents::Directory);
    assert_eq!(empty.executable_bits, 0o110);
    assert_eq!(snapshot.entries[0].path, ".");
    assert_eq!(
        snapshot
            .excluded
            .iter()
            .map(|e| e.path.as_str())
            .collect::<Vec<_>>(),
        [".eve-developer", ".git"]
    );
    assert_eq!(pool.remaining().unwrap(), 7);
    assert!(!journal.path().join("operations.jsonl").exists());
    fs::write(root.path().join("test.cjs"), b"new contents").unwrap();
    assert_eq!(bytes(&snapshot, "test.cjs"), b"untracked test");
    assert!(!format!("{snapshot:?}").contains("untracked test"));
}

#[tokio::test]
async fn digest_tracks_bytes_directory_members_and_execution_bits_not_host_paths_or_creation_order()
{
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let journal_a = tempfile::tempdir().unwrap();
    let journal_b = tempfile::tempdir().unwrap();
    fs::write(first.path().join("b"), b"two").unwrap();
    fs::write(first.path().join("a"), b"one").unwrap();
    fs::write(second.path().join("a"), b"one").unwrap();
    fs::write(second.path().join("b"), b"two").unwrap();
    let a = backend(first.path(), journal_a.path());
    let b = backend(second.path(), journal_b.path());
    let original = a
        .capture_workspace_snapshot(request("."), context("owner"))
        .await
        .unwrap();
    let other = b
        .capture_workspace_snapshot(request("."), context("owner"))
        .await
        .unwrap();
    assert_eq!(original.sha256, other.sha256);
    fs::write(second.path().join("a"), b"ONE").unwrap();
    let changed = b
        .capture_workspace_snapshot(request("."), context("owner"))
        .await
        .unwrap();
    assert_ne!(original.sha256, changed.sha256);
    fs::set_permissions(second.path().join("a"), fs::Permissions::from_mode(0o700)).unwrap();
    let executable = b
        .capture_workspace_snapshot(request("."), context("owner"))
        .await
        .unwrap();
    assert_ne!(changed.sha256, executable.sha256);
    fs::create_dir(second.path().join("empty")).unwrap();
    let directory = b
        .capture_workspace_snapshot(request("."), context("owner"))
        .await
        .unwrap();
    assert_ne!(executable.sha256, directory.sha256);
}

#[tokio::test]
async fn rejects_links_special_objects_credentials_and_protected_children_instead_of_omitting_them()
{
    let root = tempfile::tempdir().unwrap();
    let journal = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), b"outside").unwrap();
    let backend = backend(root.path(), journal.path());
    symlink(outside.path(), root.path().join("linked")).unwrap();
    assert!(
        backend
            .capture_workspace_snapshot(request("linked"), context("owner"))
            .await
            .is_err()
    );
    assert!(
        backend
            .capture_workspace_snapshot(request("."), context("owner"))
            .await
            .is_err()
    );
    fs::remove_file(root.path().join("linked")).unwrap();
    let fifo = std::ffi::CString::new(root.path().join("pipe").to_str().unwrap()).unwrap();
    // SAFETY: NUL terminated test-only path, fixed owner permissions.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(
        backend
            .capture_workspace_snapshot(request("."), context("owner"))
            .await
            .is_err()
    );
    fs::remove_file(root.path().join("pipe")).unwrap();
    fs::write(root.path().join(".env"), b"credential").unwrap();
    assert!(
        backend
            .capture_workspace_snapshot(request("."), context("owner"))
            .await
            .is_err()
    );
    fs::remove_file(root.path().join(".env")).unwrap();
    fs::hard_link(outside.path().join("secret"), root.path().join("hardlink")).unwrap();
    assert!(
        backend
            .capture_workspace_snapshot(request("."), context("owner"))
            .await
            .is_err()
    );
    fs::remove_file(root.path().join("hardlink")).unwrap();
    fs::write(root.path().join("data.bin"), b"protected data").unwrap();
    let mut cfg = config(root.path());
    cfg.protected_paths.insert(root.path().join("data.bin"));
    let guarded = LocalDeveloper::new(cfg, journal.path().into(), None).unwrap();
    assert!(
        guarded
            .capture_workspace_snapshot(request("."), context("owner"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn binary_host_credential_is_rejected_without_redaction_or_body_in_error() {
    let root = tempfile::tempdir().unwrap();
    let journal = tempfile::tempdir().unwrap();
    let token = "synthetic-token-not-a-real-credential";
    let mut data = vec![255];
    data.extend_from_slice(token.as_bytes());
    fs::write(root.path().join("input.bin"), data).unwrap();
    let backend = LocalDeveloper::new(
        config(root.path()),
        journal.path().into(),
        Some(token.into()),
    )
    .unwrap();
    let error = backend
        .capture_workspace_snapshot(request("."), context("owner"))
        .await
        .unwrap_err();
    assert!(!format!("{error:?}").contains(token));

    let access = Arc::new(Access {
        operator: false,
        ..Access::operator()
    });
    let backend = backend
        .with_authorization(access)
        .with_member_runtime(MemberRuntime::default());
    let ctx = context("member");
    let project = member_workspace(root.path(), ctx.scope().unwrap());
    fs::create_dir_all(&project).unwrap();
    let mut data = vec![255];
    data.extend_from_slice(token.as_bytes());
    fs::write(project.join("input.bin"), data).unwrap();
    let error = backend
        .capture_workspace_snapshot(request("."), ctx)
        .await
        .unwrap_err();
    assert!(!format!("{error:?}").contains(token));
}

#[tokio::test]
async fn all_fixed_limits_fail_without_partial_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let journal = tempfile::tempdir().unwrap();
    let backend = backend(root.path(), journal.path());
    let file = fs::File::create(root.path().join("large")).unwrap();
    file.set_len(WORKSPACE_SNAPSHOT_MAX_FILE_BYTES + 1).unwrap();
    assert!(
        backend
            .capture_workspace_snapshot(request("."), context("owner"))
            .await
            .is_err()
    );
    fs::remove_file(root.path().join("large")).unwrap();
    for i in 0..9 {
        fs::File::create(root.path().join(format!("size{i}")))
            .unwrap()
            .set_len(WORKSPACE_SNAPSHOT_MAX_FILE_BYTES)
            .unwrap();
    }
    assert!(
        backend
            .capture_workspace_snapshot(request("."), context("owner"))
            .await
            .is_err()
    );
    for i in 0..9 {
        fs::remove_file(root.path().join(format!("size{i}"))).unwrap();
    }
    let mut nested = root.path().to_path_buf();
    for _ in 0..WORKSPACE_SNAPSHOT_MAX_DEPTH {
        nested.push("d");
        fs::create_dir(&nested).unwrap();
    }
    fs::write(nested.join("input.bin"), [0, 255, 3]).unwrap();
    let at_limit = backend
        .capture_workspace_snapshot(request("."), context("owner"))
        .await
        .unwrap();
    assert_eq!(at_limit.total_bytes, 3);
    assert_eq!(at_limit.entries.len(), WORKSPACE_SNAPSHOT_MAX_DEPTH + 2);
    fs::create_dir(nested.join("d")).unwrap();
    assert!(
        backend
            .capture_workspace_snapshot(request("."), context("owner"))
            .await
            .is_err()
    );
    fs::remove_dir_all(root.path().join("d")).unwrap();
    for i in 0..WORKSPACE_SNAPSHOT_MAX_ENTRIES {
        fs::create_dir(root.path().join(format!("d{i:04}"))).unwrap();
    }
    assert!(
        backend
            .capture_workspace_snapshot(request("."), context("owner"))
            .await
            .is_err()
    );
}

type Hook = Box<dyn FnOnce() + Send>;
struct Access {
    enabled: AtomicBool,
    operator: bool,
    hook: Mutex<Option<(PathBuf, Hook)>>,
}
impl Access {
    fn operator() -> Self {
        Self {
            enabled: AtomicBool::new(true),
            operator: true,
            hook: Mutex::new(None),
        }
    }
}
impl DeveloperAuthorization for Access {
    fn is_operator(&self, _: &ContextScope) -> bool {
        self.operator && self.enabled.load(Ordering::SeqCst)
    }
    fn permits_business(&self, _: &ContextScope) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }
    fn allows_workspace_development(&self, _: &ContextScope) -> bool {
        !self.operator && self.enabled.load(Ordering::SeqCst)
    }
    fn permits_file(&self, _: &ContextScope, path: &Path, _: bool) -> bool {
        let hook = {
            let mut hook = self.hook.lock().unwrap();
            if hook.as_ref().is_some_and(|(target, _)| target == path) {
                hook.take()
            } else {
                None
            }
        };
        if let Some((_, hook)) = hook {
            hook();
        }
        self.enabled.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn detects_earlier_file_mutation_and_new_directory_member_during_collection() {
    for new_member in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let journal = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a"), b"old").unwrap();
        fs::write(root.path().join("b"), b"trigger").unwrap();
        let access = Arc::new(Access::operator());
        let path = root.path().join(if new_member { "new" } else { "a" });
        *access.hook.lock().unwrap() = Some((
            root.path().join("b"),
            Box::new(move || fs::write(path, b"NEW").unwrap()),
        ));
        let backend = backend(root.path(), journal.path()).with_authorization(access);
        assert!(
            backend
                .capture_workspace_snapshot(request("."), context("owner"))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn replaced_file_is_never_followed_and_revocation_is_rechecked_mid_capture() {
    for revoke in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let journal = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a"), b"original").unwrap();
        fs::write(outside.path().join("data"), b"outside").unwrap();
        let access = Arc::new(Access::operator());
        let state = access.clone();
        let path = root.path().join("a");
        let target = outside.path().join("data");
        *access.hook.lock().unwrap() = Some((
            path.clone(),
            Box::new(move || {
                if revoke {
                    state.enabled.store(false, Ordering::SeqCst);
                } else {
                    fs::remove_file(&path).unwrap();
                    symlink(target, path).unwrap();
                }
            }),
        ));
        let backend = backend(root.path(), journal.path()).with_authorization(access);
        assert!(
            backend
                .capture_workspace_snapshot(request("."), context("owner"))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn cancellation_expiry_identity_and_serial_wait_preserve_shared_budget() {
    let root = tempfile::tempdir().unwrap();
    let journal = tempfile::tempdir().unwrap();
    let backend = backend(root.path(), journal.path());
    assert!(
        backend
            .capture_workspace_snapshot(request("."), ToolExecutionContext::new())
            .await
            .is_err()
    );
    assert!(
        backend
            .capture_workspace_snapshot(request("."), context("stranger"))
            .await
            .is_err()
    );
    let pool = SharedToolSteps::new(5).unwrap();
    let expired = context("owner")
        .with_shared_steps(pool.clone())
        .with_deadline(Some(Instant::now() - Duration::from_secs(1)));
    assert!(matches!(
        backend
            .capture_workspace_snapshot(request("."), expired)
            .await,
        Err(ToolExecutionError::Cancelled(_))
    ));
    let ctx = context("owner").with_shared_steps(pool.clone());
    ctx.cancellation_handle().cancel();
    let _lock = backend.ledger.serial.lock().await;
    assert!(matches!(
        backend.capture_workspace_snapshot(request("."), ctx).await,
        Err(ToolExecutionError::Cancelled(_))
    ));
    assert_eq!(pool.remaining().unwrap(), 5);
}

#[tokio::test]
async fn reads_only_existing_private_and_member_workspaces_with_original_scope() {
    let root = tempfile::tempdir().unwrap();
    let journal = tempfile::tempdir().unwrap();
    fs::write(root.path().join("shared"), b"must not become private input").unwrap();
    let backend = backend(root.path(), journal.path()).with_session_workspaces(true);
    let ctx = context("owner");
    assert!(
        backend
            .capture_workspace_snapshot(request("."), ctx.clone())
            .await
            .is_err()
    );
    assert!(!journal.path().join("workspaces").exists());
    let private = journal
        .path()
        .join("workspaces")
        .join(session_workspace_key(ctx.scope().unwrap()));
    fs::create_dir_all(private.join("source")).unwrap();
    fs::write(
        private.join("binding.json"),
        json!({"version":1,"scope":ctx.scope().unwrap(),"source":root.path()}).to_string(),
    )
    .unwrap();
    fs::write(private.join("source/dirty"), b"actual uncommitted bytes").unwrap();
    let snapshot = backend
        .capture_workspace_snapshot(request("."), ctx.clone())
        .await
        .unwrap();
    assert_eq!(bytes(&snapshot, "dirty"), b"actual uncommitted bytes");
    assert_eq!(snapshot.workspace_root, private.join("source"));
    assert!(!snapshot.entries.iter().any(|e| e.path == "shared"));
    fs::write(
        private.join("binding.json"),
        json!({"version":1,"scope":context("other").scope().unwrap(),"source":root.path()})
            .to_string(),
    )
    .unwrap();
    assert!(
        backend
            .capture_workspace_snapshot(request("."), ctx)
            .await
            .is_err()
    );

    let access = Arc::new(Access {
        operator: false,
        ..Access::operator()
    });
    let member = LocalDeveloper::new(config(root.path()), journal.path().into(), None)
        .unwrap()
        .with_authorization(access)
        .with_member_runtime(MemberRuntime::default());
    let ctx = context("member");
    let project = member_workspace(root.path(), ctx.scope().unwrap());
    assert!(
        member
            .capture_workspace_snapshot(request("."), ctx.clone())
            .await
            .is_err()
    );
    assert!(!project.exists());
    fs::create_dir_all(project.join("nested")).unwrap();
    fs::write(project.join("nested/input"), b"member input").unwrap();
    let snapshot = member
        .capture_workspace_snapshot(request("nested"), ctx.clone())
        .await
        .unwrap();
    assert_eq!(snapshot.scope, ctx.scope().unwrap().clone());
    assert_eq!(snapshot.workspace_root, project);
    assert_eq!(bytes(&snapshot, "input"), b"member input");
    assert!(
        member
            .capture_workspace_snapshot(request(root.path().to_str().unwrap()), ctx)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn absolute_root_requires_existing_host_access_and_rejects_linked_components() {
    let root = tempfile::tempdir().unwrap();
    let journal = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("input"), b"host file").unwrap();
    let mut cfg = config(root.path());
    cfg.host_filesystem = true;
    let backend = LocalDeveloper::new(cfg, journal.path().into(), None).unwrap();
    let snapshot = backend
        .capture_workspace_snapshot(request(outside.path().to_str().unwrap()), context("owner"))
        .await
        .unwrap();
    assert_eq!(bytes(&snapshot, "input"), b"host file");
    symlink(outside.path(), root.path().join("jump")).unwrap();
    assert!(
        backend
            .capture_workspace_snapshot(
                request(root.path().join("jump").to_str().unwrap()),
                context("owner")
            )
            .await
            .is_err()
    );
    assert!(
        backend
            .capture_workspace_snapshot(request("../"), context("owner"))
            .await
            .is_err()
    );
}
