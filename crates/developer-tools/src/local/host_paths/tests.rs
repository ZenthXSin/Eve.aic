use super::*;
use eve_llm_api::ContextScope;
use std::sync::{Arc, Mutex, atomic::AtomicBool};

struct Reviewer {
    requests: Mutex<Vec<ActionReviewRequest>>,
    change: Option<Box<dyn Fn() + Send + Sync>>,
}
impl ActionReviewer for Reviewer {
    fn review(&self, request: ActionReviewRequest) -> ActionReviewFuture<'_> {
        self.requests.lock().unwrap().push(request);
        if let Some(change) = &self.change {
            change();
        }
        Box::pin(async {
            Ok(ActionReviewVerdict {
                risk: ReviewRisk::Medium,
                authorization: ReviewAuthorization::High,
                allow: true,
                rationale: "测试原话授权该路径".into(),
            })
        })
    }
}
fn config(root: &Path) -> DeveloperConfig {
    serde_json::from_value(json!({"workspace":root,"operators":["owner"],
        "host_filesystem":true,"commands":true,"self_upgrade":false}))
    .unwrap()
}
fn context(user: &str) -> ToolExecutionContext {
    ToolExecutionContext::new()
        .with_scope(Some(ContextScope {
            session_id: "s".into(),
            user_id: user.into(),
        }))
        .with_review_user_messages(Arc::new(vec!["读写并验证明确指定的工程外文件".into()]))
}
fn file(path: &Path) -> FileArguments {
    FileArguments {
        path: path.to_str().unwrap().into(),
        offset: 0,
        content: None,
        expected_sha256: None,
    }
}
fn write(path: &Path, text: &str) -> DeveloperOperation {
    let mut a = file(path);
    a.content = Some(text.into());
    a.expected_sha256 = Some(String::new());
    DeveloperOperation::Write(a)
}
fn reviewer() -> Arc<Reviewer> {
    Arc::new(Reviewer {
        requests: Mutex::new(vec![]),
        change: None,
    })
}

#[tokio::test]
async fn operator_reads_writes_searches_patches_and_runs_outside_default_project() {
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let journal = tempfile::tempdir().unwrap();
    let reviewed = reviewer();
    let backend = LocalDeveloper::new(config(project.path()), journal.path().into(), None)
        .unwrap()
        .with_action_reviewer(reviewed.clone());
    let first = outside.path().join("notes.txt");
    let result = backend
        .execute(write(&first, "original\n"), context("owner"))
        .await
        .unwrap();
    let read = backend
        .execute(DeveloperOperation::Read(file(&first)), context("owner"))
        .await
        .unwrap();
    assert_eq!(read["content"], "original\n");
    assert_eq!(read["sha256"], result["sha256"]);
    backend
        .execute(
            DeveloperOperation::Edit(EditArguments {
                path: first.to_str().unwrap().into(),
                old_string: "original".into(),
                new_string: "changed".into(),
                expected_sha256: read["sha256"].as_str().unwrap().into(),
            }),
            context("owner"),
        )
        .await
        .unwrap();
    let second = outside.path().join("moved.txt");
    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n*** Move to:{}\n@@\n-changed\n+finished\n*** End Patch",
        first.display(),
        second.display()
    );
    backend
        .execute(
            DeveloperOperation::Patch(PatchArguments { patch }),
            context("owner"),
        )
        .await
        .unwrap();
    assert!(!first.exists());
    assert_eq!(fs::read_to_string(&second).unwrap(), "finished\n");
    let nested = outside.path().join("nested");
    fs::create_dir(&nested).unwrap();
    let deeper = nested.join("deeper.txt");
    fs::write(&deeper, "finished\n").unwrap();
    let search = backend
        .execute(
            DeveloperOperation::Search(SearchArguments {
                path: outside.path().to_str().unwrap().into(),
                query: "finished".into(),
                max_results: 10,
            }),
            context("owner"),
        )
        .await
        .unwrap();
    let matches = search["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 2);
    let expected = [
        second.canonicalize().unwrap(),
        deeper.canonicalize().unwrap(),
    ];
    for item in matches {
        let found = Path::new(item["path"].as_str().unwrap());
        assert!(found.is_absolute());
        assert!(expected.iter().any(|path| path == found));
        assert_eq!(
            backend
                .execute(DeveloperOperation::Read(file(found)), context("owner"))
                .await
                .unwrap()["content"],
            "finished\n"
        );
    }
    let command = if cfg!(windows) {
        CommandArguments {
            program: "cmd".into(),
            args: vec!["/C".into(), "cd".into()],
            cwd: outside.path().to_str().unwrap().into(),
            timeout_ms: None,
            owner_confirmed: false,
        }
    } else {
        CommandArguments {
            program: "pwd".into(),
            args: vec![],
            cwd: outside.path().to_str().unwrap().into(),
            timeout_ms: None,
            owner_confirmed: false,
        }
    };
    let output = backend
        .execute(DeveloperOperation::Command(command), context("owner"))
        .await
        .unwrap();
    assert_eq!(output["exit_code"], 0);
    assert_eq!(
        Path::new(output["cwd"].as_str().unwrap()),
        outside.path().canonicalize().unwrap()
    );
    assert!(reviewed.requests.lock().unwrap().is_empty());
    assert!(fs::read_dir(project.path()).unwrap().next().is_none());
}

#[tokio::test]
async fn disabled_ordinary_member_and_revoked_operator_cannot_gain_host_paths() {
    struct Access(AtomicBool);
    impl DeveloperAuthorization for Access {
        fn is_operator(&self, _: &ContextScope) -> bool {
            self.0.load(Ordering::SeqCst)
        }
        fn allows_workspace_development(&self, _: &ContextScope) -> bool {
            true
        }
    }
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let journal = tempfile::tempdir().unwrap();
    let target = outside.path().join("outside.txt");
    fs::write(&target, "retained").unwrap();
    let mut settings = config(project.path());
    settings.host_filesystem = false;
    let disabled =
        LocalDeveloper::new(settings.clone(), journal.path().join("disabled"), None).unwrap();
    assert!(
        disabled
            .execute(DeveloperOperation::Read(file(&target)), context("owner"))
            .await
            .is_err()
    );
    settings.host_filesystem = true;
    settings.ordinary_access = true;
    let ordinary =
        LocalDeveloper::new(settings.clone(), journal.path().join("ordinary"), None).unwrap();
    assert!(
        ordinary
            .execute(DeveloperOperation::Read(file(&target)), context("guest"))
            .await
            .is_err()
    );
    let access = Arc::new(Access(AtomicBool::new(false)));
    let member = LocalDeveloper::new(settings, journal.path().join("member"), None)
        .unwrap()
        .with_authorization(access.clone())
        .with_member_runtime(MemberRuntime::default());
    assert!(
        member
            .execute(DeveloperOperation::Read(file(&target)), context("guest"))
            .await
            .is_err()
    );
    member
        .execute(
            write(Path::new("mine.txt"), "member file"),
            context("guest"),
        )
        .await
        .unwrap();
    assert!(
        member
            .execute(
                DeveloperOperation::Read(file(Path::new("../outside.txt"))),
                context("guest")
            )
            .await
            .is_err()
    );
    access.0.store(false, Ordering::SeqCst);
    assert!(
        member
            .execute(
                write(&outside.path().join("must-not-exist.txt"), "no"),
                context("owner")
            )
            .await
            .is_err()
    );
    assert!(!outside.path().join("must-not-exist.txt").exists());
    assert_eq!(fs::read_to_string(target).unwrap(), "retained");
}

#[tokio::test]
async fn absolute_operations_do_not_require_a_git_checkout_and_patch_aliases_are_rejected() {
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let journal = tempfile::tempdir().unwrap();
    let target = outside.path().join("one.txt");
    fs::write(&target, "one\n").unwrap();
    let backend = LocalDeveloper::new(config(project.path()), journal.path().into(), None)
        .unwrap()
        .with_session_workspaces(true);
    assert_eq!(
        backend
            .execute(DeveloperOperation::Read(file(&target)), context("owner"))
            .await
            .unwrap()["content"],
        "one\n"
    );
    assert!(!journal.path().join("workspaces").exists());
    fs::write(project.path().join("same.txt"), "one\n").unwrap();
    let backend = backend
        .with_session_workspaces(false)
        .with_action_reviewer(reviewer());
    let patch = format!(
        "*** Begin Patch\n*** Update File: same.txt\n@@\n-one\n+two\n*** Update File: {}\n@@\n-one\n+three\n*** End Patch",
        project.path().join("same.txt").display()
    );
    assert!(
        backend
            .execute(
                DeveloperOperation::Patch(PatchArguments { patch }),
                context("owner")
            )
            .await
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(project.path().join("same.txt")).unwrap(),
        "one\n"
    );
}

#[tokio::test]
async fn default_root_still_works_and_relative_protected_files_never_reach_review() {
    let project = tempfile::tempdir().unwrap();
    let journal = tempfile::tempdir().unwrap();
    fs::write(project.path().join("visible.txt"), "visible needle").unwrap();
    fs::create_dir(project.path().join("data")).unwrap();
    let protected = project.path().join("data/settings.json");
    fs::write(&protected, "protected needle\n").unwrap();
    let mut settings = config(project.path());
    settings.protected_paths.insert(protected.clone());
    let reviewed = reviewer();
    let backend = LocalDeveloper::new(settings, journal.path().into(), None)
        .unwrap()
        .with_action_reviewer(reviewed.clone());
    let listed = backend
        .execute(
            DeveloperOperation::List(file(Path::new("."))),
            context("owner"),
        )
        .await
        .unwrap();
    assert_eq!(listed["total"], 2);
    let search = backend
        .execute(
            DeveloperOperation::Search(SearchArguments {
                path: ".".into(),
                query: "needle".into(),
                max_results: 10,
            }),
            context("owner"),
        )
        .await
        .unwrap();
    assert_eq!(search["matches"].as_array().unwrap().len(), 1);
    assert_eq!(search["matches"][0]["path"], "visible.txt");
    let output = backend
        .execute(
            DeveloperOperation::Command(CommandArguments {
                program: if cfg!(windows) { "cmd" } else { "pwd" }.into(),
                args: if cfg!(windows) {
                    vec!["/C".into(), "cd".into()]
                } else {
                    vec![]
                },
                cwd: ".".into(),
                timeout_ms: None,
                owner_confirmed: false,
            }),
            context("owner"),
        )
        .await
        .unwrap();
    assert_eq!(output["exit_code"], 0);
    assert_eq!(
        Path::new(output["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        project.path().canonicalize().unwrap()
    );
    let reviews_before = reviewed.requests.lock().unwrap().len();
    for operation in [
        DeveloperOperation::Read(file(Path::new("data/settings.json"))),
        write(Path::new("data/settings.json"), "must not be written"),
        DeveloperOperation::Patch(PatchArguments { patch: "*** Begin Patch\n*** Update File: data/settings.json\n@@\n-protected needle\n+changed\n*** End Patch".into() }),
        DeveloperOperation::Patch(PatchArguments { patch: "*** Begin Patch\n*** Update File: visible.txt\n*** Move to:data/settings.json\n@@\n-visible needle\n+changed\n*** End Patch".into() }),
    ] {
        assert!(backend.execute(operation, context("owner")).await.is_err());
    }
    assert_eq!(reviewed.requests.lock().unwrap().len(), reviews_before);
    assert_eq!(fs::read_to_string(protected).unwrap(), "protected needle\n");
}

#[cfg(unix)]
#[tokio::test]
async fn canonical_file_authorization_cannot_be_bypassed_with_aliases() {
    struct Access;
    impl DeveloperAuthorization for Access {
        fn is_operator(&self, _: &ContextScope) -> bool {
            true
        }
        fn permits_file(&self, _: &ContextScope, path: &Path, write: bool) -> bool {
            !(write && path.file_name().is_some_and(|name| name == "AGENT.md"))
        }
    }
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let journal = tempfile::tempdir().unwrap();
    let target = outside.path().join("AGENT.md");
    fs::write(&target, "owner rules\n").unwrap();
    let alias = outside.path().join("notes.txt");
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    let reviewed = reviewer();
    let backend = LocalDeveloper::new(config(project.path()), journal.path().into(), None)
        .unwrap()
        .with_authorization(Arc::new(Access))
        .with_action_reviewer(reviewed.clone());
    let read = backend
        .execute(DeveloperOperation::Read(file(&alias)), context("operator"))
        .await
        .unwrap();
    assert_eq!(read["content"], "owner rules\n");
    for operation in [
        DeveloperOperation::Edit(EditArguments {
            path: alias.to_str().unwrap().into(),
            old_string: "owner".into(),
            new_string: "changed".into(),
            expected_sha256: read["sha256"].as_str().unwrap().into(),
        }),
        DeveloperOperation::Patch(PatchArguments {
            patch: format!(
                "*** Begin Patch\n*** Update File: {}\n@@\n-owner rules\n+changed\n*** End Patch",
                alias.display()
            ),
        }),
    ] {
        assert!(
            backend
                .execute(operation, context("operator"))
                .await
                .is_err()
        );
    }
    assert!(reviewed.requests.lock().unwrap().is_empty());
    assert_eq!(fs::read_to_string(target).unwrap(), "owner rules\n");
}

#[cfg(unix)]
#[tokio::test]
async fn external_edit_write_and_patch_move_preserve_permissions_and_owner() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let journal = tempfile::tempdir().unwrap();
    let backend = LocalDeveloper::new(config(project.path()), journal.path().into(), None)
        .unwrap()
        .with_action_reviewer(reviewer());
    for mode in [0o755, 0o640] {
        let original = outside.path().join(format!("script-{mode}"));
        let moved = outside.path().join(format!("moved-{mode}"));
        fs::write(&original, "first\n").unwrap();
        fs::set_permissions(&original, fs::Permissions::from_mode(mode)).unwrap();
        if fs::metadata(&original).unwrap().uid() == 0 {
            std::os::unix::fs::chown(&original, Some(65534), Some(65534)).unwrap();
        }
        let before = fs::metadata(&original).unwrap();
        let read = backend
            .execute(DeveloperOperation::Read(file(&original)), context("owner"))
            .await
            .unwrap();
        let edited = backend
            .execute(
                DeveloperOperation::Edit(EditArguments {
                    path: original.to_str().unwrap().into(),
                    old_string: "first".into(),
                    new_string: "second".into(),
                    expected_sha256: read["sha256"].as_str().unwrap().into(),
                }),
                context("owner"),
            )
            .await
            .unwrap();
        let assert_metadata = |path: &Path| {
            let current = fs::metadata(path).unwrap();
            assert_eq!(current.mode(), before.mode());
            assert_eq!(current.uid(), before.uid());
            assert_eq!(current.gid(), before.gid());
        };
        assert_metadata(&original);
        let mut args = file(&original);
        args.content = Some("third\n".into());
        args.expected_sha256 = Some(edited["sha256"].as_str().unwrap().into());
        backend
            .execute(DeveloperOperation::Write(args), context("owner"))
            .await
            .unwrap();
        assert_metadata(&original);
        backend.execute(DeveloperOperation::Patch(PatchArguments { patch: format!("*** Begin Patch\n*** Update File: {}\n*** Move to: {}\n@@\n-third\n+finished\n*** End Patch", original.display(), moved.display()) }), context("owner")).await.unwrap();
        assert!(!original.exists());
        assert_metadata(&moved);
        assert_eq!(fs::read_to_string(moved).unwrap(), "finished\n");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn host_links_bind_real_target_without_review_and_protected_targets_stay_private() {
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let journal = tempfile::tempdir().unwrap();
    let original = outside.path().join("original");
    let replacement = outside.path().join("replacement");
    fs::create_dir(&original).unwrap();
    fs::create_dir(&replacement).unwrap();
    let alias = outside.path().join("data");
    std::os::unix::fs::symlink(&original, &alias).unwrap();
    let reviewer = reviewer();
    let backend = LocalDeveloper::new(config(project.path()), journal.path().into(), None)
        .unwrap()
        .with_action_reviewer(reviewer.clone());
    backend
        .execute(
            write(&alias.join("created.txt"), "reviewed"),
            context("owner"),
        )
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(original.join("created.txt")).unwrap(),
        "reviewed"
    );
    assert!(!replacement.join("created.txt").exists());
    assert!(reviewer.requests.lock().unwrap().is_empty());
    let mut settings = config(project.path());
    let protected = outside.path().join("runtime.json");
    fs::write(&protected, "synthetic protected content").unwrap();
    settings.protected_paths.insert(protected.clone());
    let backend = LocalDeveloper::new(settings, journal.path().join("protected"), None).unwrap();
    let link = outside.path().join("ordinary.txt");
    std::os::unix::fs::symlink(&protected, &link).unwrap();
    for path in [
        protected,
        link,
        PathBuf::from("/proc/self/environ"),
        PathBuf::from("/etc/shadow"),
        backend.journal.join("operations.jsonl"),
    ] {
        assert!(
            backend
                .execute(DeveloperOperation::Read(file(&path)), context("owner"))
                .await
                .is_err()
        );
    }
    let result = backend
        .execute(
            DeveloperOperation::Search(SearchArguments {
                path: outside.path().to_str().unwrap().into(),
                query: "synthetic protected".into(),
                max_results: 10,
            }),
            context("owner"),
        )
        .await
        .unwrap();
    assert_eq!(result["matches"], json!([]));
}
