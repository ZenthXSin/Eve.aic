use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use eve_practice_api::*;
use eve_practice_plugin::{PracticeController, PracticePlugin, Practitioner};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// 真实文件库；可注入提交前失败与提交后确认丢失。
struct Store {
    inner: FileStateStore,
    fail_before: AtomicBool,
    fail_after: AtomicBool,
}
impl Store {
    fn open(path: &Path) -> Arc<Self> {
        Arc::new(Self {
            inner: FileStateStore::open(path).unwrap(),
            fail_before: AtomicBool::new(false),
            fail_after: AtomicBool::new(false),
        })
    }
    fn raw(&self) -> Option<Vec<u8>> {
        self.inner.get(&owner(), PRACTICE_STATE_KEY).unwrap()
    }
    fn saved(&self) -> Option<Value> {
        self.raw()
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
    }
}
impl StateStore for Store {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.inner.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        if self.fail_before.load(Ordering::SeqCst) {
            return Err(PluginError::State("private-storage-detail".into()));
        }
        self.inner.set(namespace, key, value)?;
        if self.fail_after.load(Ordering::SeqCst) {
            return Err(PluginError::State("private-storage-detail".into()));
        }
        Ok(())
    }
}

fn owner() -> PluginId {
    PluginId::new(PRACTICE_PLUGIN_ID).unwrap()
}

async fn open(state: Arc<dyn StateStore>) -> (Kernel, PracticeController) {
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let plugin = PracticePlugin::new().unwrap();
    let admin = plugin.controller();
    assert_eq!(admin.snapshot().err(), Some(PracticeError::Unavailable));
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, admin)
}

fn task(goal: &str, revision: u64) -> PracticeTask {
    PracticeTask {
        goal_id: goal.into(),
        goal_revision: revision,
        owner: "user-a".into(),
        brief: "学习目标：模组创作".into(),
        brief_truncated: false,
        notes: vec![TaskNote {
            id: "knowledge-1".into(),
            text: "每个模组根目录需要 manifest".into(),
            source: Some("https://docs.example/start.html".into()),
            version: Some("v146".into()),
            source_quoted: true,
        }],
    }
}

fn profile() -> RunnerProfile {
    RunnerProfile {
        runner_id: "fake-runner:v1".into(),
        runtime: "fake runtime".into(),
        domain: "测试用数据文件".into(),
        layout: "根目录放 manifest".into(),
        properties: vec!["value".into()],
    }
}

/// 产物文件内容决定替身运行结果：`good` 通过，`warn` 产生警告，`bad` 结构检查不通过。
fn draft(content: &str) -> PracticeDraft {
    PracticeDraft {
        applicable: true,
        files: vec![ArtifactFile {
            path: "manifest.hjson".into(),
            content: content.into(),
        }],
        probes: vec![Probe {
            subject: "eve-item".into(),
            property: "value".into(),
            expected: "1".into(),
        }],
        rationale: format!("尝试 {content}"),
        notes_used: vec!["knowledge-1".into()],
    }
}

fn evidence(draft: &PracticeDraft, verified: bool) -> RunEvidence {
    RunEvidence {
        runtime_version: "fake build 1".into(),
        exit: RunExit::Completed,
        loaded: true,
        warnings: if verified {
            vec![]
        } else {
            vec!["defaulting to Block".into()]
        },
        probes: vec![ProbeResult {
            probe: draft.probes[0].clone(),
            actual: Some("1".into()),
            passed: true,
        }],
        log_excerpt: "1 mods loaded".into(),
        log_sha256: "a".repeat(64),
        log_bytes: 13,
        duration_ms: 10,
    }
}

struct FakeDrafter {
    replies: Mutex<VecDeque<Result<PracticeDraft, PracticeFailure>>>,
    requests: Mutex<Vec<(DraftRequest, Option<Value>)>>,
    store: Arc<Store>,
}
impl PracticeDrafter for FakeDrafter {
    fn version(&self) -> &str {
        "fake-drafter:v1"
    }
    fn draft(&self, request: DraftRequest) -> PracticeFuture<'_, PracticeDraft> {
        self.requests
            .lock()
            .unwrap()
            .push((request, self.store.saved()));
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected draft request");
        Box::pin(async move { reply.map_err(PracticeError::Practice) })
    }
}

struct FakeRunner {
    profile: RunnerProfile,
    runs: Mutex<Vec<(PathBuf, bool, Option<Value>)>>,
    store: Arc<Store>,
}
impl PracticeRunner for FakeRunner {
    fn profile(&self) -> &RunnerProfile {
        &self.profile
    }
    fn check(&self, draft: &PracticeDraft) -> Vec<String> {
        if draft.files[0].content == "bad" {
            vec!["manifest 缺少 name\n第二行".into()]
        } else {
            vec![]
        }
    }
    fn run<'a>(
        &'a self,
        draft: &'a PracticeDraft,
        workspace: &'a Path,
    ) -> BoxFuture<'a, RunEvidence> {
        let empty = std::fs::read_dir(workspace).unwrap().next().is_none();
        std::fs::write(workspace.join("written"), &draft.files[0].content).unwrap();
        self.runs
            .lock()
            .unwrap()
            .push((workspace.to_path_buf(), empty, self.store.saved()));
        let evidence = evidence(draft, draft.files[0].content == "good");
        Box::pin(async move { evidence })
    }
}

fn last_attempt(saved: &Option<Value>) -> (Value, Value) {
    let attempt = saved.as_ref().unwrap()["runs"][0]["attempts"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    (attempt["number"].clone(), attempt["stage"].clone())
}

#[tokio::test]
async fn attempts_are_saved_before_each_effect_and_repair_from_issues_and_evidence_until_verified()
{
    let directory = tempfile::tempdir().unwrap();
    let workspace_root = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let drafter = Arc::new(FakeDrafter {
        replies: Mutex::new(VecDeque::from([
            Ok(draft("bad")),
            Ok(draft("warn")),
            Ok(draft("good")),
        ])),
        requests: Mutex::new(vec![]),
        store: store.clone(),
    });
    let runner = Arc::new(FakeRunner {
        profile: profile(),
        runs: Mutex::new(vec![]),
        store: store.clone(),
    });
    let practitioner = Practitioner::new(Arc::new(admin.clone()), drafter.clone(), runner.clone());
    let run = admin
        .begin(
            task("goal-a", 1),
            &profile(),
            practitioner.drafter_version(),
            100,
        )
        .unwrap()
        .unwrap();
    assert!(
        admin
            .begin(task("goal-b", 1), &profile(), "v", 100)
            .unwrap()
            .is_none(),
        "一次只进行一个实践"
    );
    let finished = practitioner
        .practice(&run, workspace_root.path(), || 200)
        .await
        .unwrap();

    assert_eq!(finished.status, PracticeStatus::Verified);
    let outcomes: Vec<_> = finished
        .attempts
        .iter()
        .map(|attempt| attempt.outcome.clone())
        .collect();
    assert_eq!(
        outcomes,
        [
            Some(AttemptOutcome::Rejected),
            Some(AttemptOutcome::Failed),
            Some(AttemptOutcome::Verified)
        ]
    );
    assert_eq!(
        finished.attempts[0].issues,
        ["manifest 缺少 name 第二行"],
        "问题截成单行"
    );
    assert!(
        finished.attempts[0].evidence.is_none(),
        "结构检查不通过时不运行"
    );
    assert_eq!(finished.verified_attempt().unwrap().number, 3);

    let requests = drafter.requests.lock().unwrap().clone();
    for (index, (request, saved)) in requests.iter().enumerate() {
        assert_eq!(request.attempt, index + 1);
        assert_eq!(
            last_attempt(saved),
            (json!(index + 1), json!("Drafting")),
            "请求前已保存该次尝试"
        );
    }
    assert!(requests[0].0.previous.is_none());
    let second = requests[1].0.previous.as_ref().unwrap();
    assert_eq!(second.issues, ["manifest 缺少 name 第二行"]);
    let third = requests[2].0.previous.as_ref().unwrap();
    assert_eq!(
        third.evidence.as_ref().unwrap().warnings,
        ["defaulting to Block"]
    );
    let runs = runner.runs.lock().unwrap().clone();
    assert_eq!(runs.len(), 2);
    for (path, empty, saved) in runs.iter() {
        assert!(*empty, "每次尝试使用全新的空目录");
        assert!(path.starts_with(workspace_root.path()));
        assert!(!path.exists(), "运行后清理工作目录");
        assert_eq!(last_attempt(saved).1, json!("Running"), "运行前已保存草稿");
    }
    kernel.stop_all().await.unwrap();

    let (kernel, admin) = open(store.clone()).await;
    assert_eq!(admin.snapshot().unwrap().runs, vec![finished]);
    assert!(
        admin
            .begin(task("goal-a", 1), &profile(), "v", 300)
            .unwrap()
            .is_none(),
        "同一修订不再实践"
    );
    assert!(
        admin
            .begin(task("goal-a", 2), &profile(), "v", 300)
            .unwrap()
            .is_some()
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn exhausted_attempts_not_applicable_draft_failures_and_abandonment_are_terminal() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;

    let run = admin
        .begin(task("goal-a", 1), &profile(), "v", 100)
        .unwrap()
        .unwrap();
    for number in 1..=MAX_ATTEMPTS {
        let at = 100 + number as u64 * 10;
        let run = admin
            .record_draft(&run.id, at, Ok(draft("warn")), vec![])
            .unwrap();
        assert_eq!(run.attempts[number - 1].stage, AttemptStage::Running);
        let evidence = evidence(&draft("warn"), false);
        let run = admin.record_evidence(&run.id, at + 1, evidence).unwrap();
        assert_eq!(run.attempts.len(), (number + 1).min(MAX_ATTEMPTS));
    }
    let snapshot = admin.snapshot().unwrap();
    assert_eq!(snapshot.runs[0].status, PracticeStatus::Unverified);
    assert_eq!(snapshot.runs[0].finished_at_ms, Some(131));

    let run = admin
        .begin(task("goal-b", 1), &profile(), "v", 200)
        .unwrap()
        .unwrap();
    let not_applicable = PracticeDraft {
        applicable: false,
        files: vec![],
        probes: vec![],
        rationale: "运行器的领域与目标无关".into(),
        notes_used: vec![],
    };
    let run = admin
        .record_draft(&run.id, 201, Ok(not_applicable), vec![])
        .unwrap();
    assert_eq!(run.status, PracticeStatus::NotApplicable);

    let run = admin
        .begin(task("goal-c", 1), &profile(), "v", 300)
        .unwrap()
        .unwrap();
    let run = admin
        .record_draft(&run.id, 301, Err(PracticeFailure::InvalidOutput), vec![])
        .unwrap();
    assert_eq!(
        run.status,
        PracticeStatus::Failed(PracticeFailure::InvalidOutput)
    );
    assert_eq!(
        run.attempts[0].outcome,
        Some(AttemptOutcome::DraftFailed {
            failure: PracticeFailure::InvalidOutput
        })
    );

    let run = admin
        .begin(task("goal-d", 1), &profile(), "v", 400)
        .unwrap()
        .unwrap();
    admin
        .record_draft(&run.id, 401, Ok(draft("good")), vec![])
        .unwrap();
    let run = admin
        .abandon(&run.id, 402, PracticeFailure::Cancelled)
        .unwrap();
    assert_eq!(
        run.status,
        PracticeStatus::Failed(PracticeFailure::Cancelled)
    );
    assert!(run.attempts[0].evidence.is_none(), "放弃时不补写证据");

    // 结局只写一次；未到运行阶段不能保存证据；不合规草稿与证据被拒绝且零写入。
    assert_eq!(
        admin.abandon(&run.id, 403, PracticeFailure::Timeout).err(),
        Some(PracticeError::Conflict)
    );
    let run = admin
        .begin(task("goal-e", 1), &profile(), "v", 500)
        .unwrap()
        .unwrap();
    let before = store.raw();
    assert_eq!(
        admin
            .record_evidence(&run.id, 501, evidence(&draft("good"), true))
            .err(),
        Some(PracticeError::Conflict)
    );
    let mut escaping = draft("good");
    escaping.files[0].path = "../escape".into();
    assert_eq!(
        admin.record_draft(&run.id, 501, Ok(escaping), vec![]).err(),
        Some(PracticeError::InvalidInput)
    );
    let mut unknown = draft("good");
    unknown.notes_used = vec!["knowledge-9".into()];
    assert_eq!(
        admin.record_draft(&run.id, 501, Ok(unknown), vec![]).err(),
        Some(PracticeError::InvalidInput)
    );
    let run = admin
        .record_draft(&run.id, 501, Ok(draft("good")), vec![])
        .unwrap();
    let mut forged = evidence(&draft("good"), true);
    forged.probes[0].actual = None;
    assert_eq!(
        admin.record_evidence(&run.id, 502, forged).err(),
        Some(PracticeError::InvalidInput)
    );
    assert_ne!(store.raw(), before);
    // 每个目标的实践次数持久计数。
    assert!(
        admin
            .begin(task("goal-e", 2), &profile(), "v", 600)
            .unwrap()
            .is_none(),
        "goal-e 仍在进行"
    );
    admin
        .abandon(&run.id, 503, PracticeFailure::Timeout)
        .unwrap();
    assert!(
        admin
            .begin(task("goal-e", 2), &profile(), "v", 600)
            .unwrap()
            .is_some()
    );
    kernel.stop_all().await.unwrap();
    let (kernel, admin) = open(store.clone()).await;
    let runs = admin.snapshot().unwrap().runs;
    assert_eq!(runs.len(), 6);
    assert_eq!(runs[5].status, PracticeStatus::Interrupted);
    assert!(
        admin
            .begin(task("goal-e", 3), &profile(), "v", 700)
            .unwrap()
            .is_none(),
        "每个目标至多两次"
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn running_practice_is_interrupted_after_restart_and_never_replayed() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let run = admin
        .begin(task("goal-a", 1), &profile(), "v", 100)
        .unwrap()
        .unwrap();
    admin
        .record_draft(&run.id, 101, Ok(draft("good")), vec![])
        .unwrap();
    // 实际运行途中进程退出：不调用 stop，直接以新内核打开同一存储。
    drop(kernel);
    let (kernel, admin) = open(store.clone()).await;
    let run = &admin.snapshot().unwrap().runs[0];
    assert_eq!(run.status, PracticeStatus::Interrupted);
    assert_eq!(run.attempts[0].stage, AttemptStage::Running);
    assert_eq!(
        (run.attempts[0].outcome.clone(), run.finished_at_ms),
        (None, None)
    );
    assert_eq!(store.saved().unwrap()["runs"][0]["status"], "Interrupted");
    assert_eq!(
        admin
            .record_evidence(&run.id, 200, evidence(&draft("good"), true))
            .err(),
        Some(PracticeError::Conflict)
    );
    assert!(
        admin
            .begin(task("goal-a", 1), &profile(), "v", 200)
            .unwrap()
            .is_none()
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn unconfirmed_commits_close_the_handle_and_corrupt_state_is_refused_without_clearing() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    store.fail_before.store(true, Ordering::SeqCst);
    assert_eq!(
        admin.begin(task("goal-a", 1), &profile(), "v", 100).err(),
        Some(PracticeError::Storage)
    );
    store.fail_before.store(false, Ordering::SeqCst);
    assert_eq!(admin.snapshot().err(), Some(PracticeError::Unavailable));
    kernel.stop_all().await.unwrap();

    let (kernel, admin) = open(store.clone()).await;
    assert!(admin.snapshot().unwrap().runs.is_empty());
    let run = admin
        .begin(task("goal-a", 1), &profile(), "v", 100)
        .unwrap()
        .unwrap();
    admin
        .record_draft(&run.id, 101, Ok(draft("good")), vec![])
        .unwrap();
    store.fail_after.store(true, Ordering::SeqCst);
    assert_eq!(
        admin
            .record_evidence(&run.id, 102, evidence(&draft("good"), true))
            .err(),
        Some(PracticeError::Storage)
    );
    store.fail_after.store(false, Ordering::SeqCst);
    kernel.stop_all().await.unwrap();
    let (kernel, admin) = open(store.clone()).await;
    assert_eq!(
        admin.snapshot().unwrap().runs[0].status,
        PracticeStatus::Verified,
        "读到实际提交"
    );
    kernel.stop_all().await.unwrap();

    let original = store.saved().unwrap();
    let mut cases = Vec::new();
    let mut forged = original.clone();
    forged["runs"][0]["attempts"][0]["evidence"]["warnings"] = json!(["defaulting to Block"]);
    cases.push(serde_json::to_vec(&forged).unwrap());
    let mut escaped = original.clone();
    escaped["runs"][0]["attempts"][0]["draft"]["files"][0]["path"] = json!("../../etc/passwd");
    cases.push(serde_json::to_vec(&escaped).unwrap());
    let mut renamed = original.clone();
    renamed["runs"][0]["id"] = json!("practice-other");
    cases.push(serde_json::to_vec(&renamed).unwrap());
    cases.push(br#"{"format_version":1,"format_version":1,"runs":[]}"#.to_vec());
    cases.push(br#"{"format_version":2,"runs":[]}"#.to_vec());
    for (index, bytes) in cases.into_iter().enumerate() {
        store
            .inner
            .set(&owner(), PRACTICE_STATE_KEY.into(), bytes.clone())
            .unwrap();
        let kernel = Kernel::with_services(KernelServices {
            state: store.clone(),
            ..KernelServices::default()
        });
        kernel
            .register(Box::new(PracticePlugin::new().unwrap()))
            .unwrap();
        let error = kernel.start_all().await.unwrap_err().to_string();
        let expected = if index == 4 {
            "不支持该实践状态版本"
        } else {
            "实践状态损坏；未清空"
        };
        assert!(error.contains(expected), "{index}: {error}");
        assert!(!error.contains("private-storage-detail"));
        assert_eq!(store.raw().unwrap(), bytes, "损坏状态保留原字节");
    }
}
