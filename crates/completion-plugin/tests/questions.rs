use eve_completion_api::*;
use eve_completion_plugin::{CompletionController, CompletionPlugin};
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct Store {
    inner: FileStateStore,
    fail: AtomicBool,
}
impl StateStore for Store {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.inner.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(PluginError::State("private-storage-detail".into()));
        }
        self.inner.set(namespace, key, value)
    }
}

struct World {
    _directory: tempfile::TempDir,
    state: Arc<Store>,
}
impl World {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(Store {
            inner: FileStateStore::open(directory.path().join("state")).unwrap(),
            fail: AtomicBool::new(false),
        });
        Self {
            _directory: directory,
            state,
        }
    }
    async fn open(&self) -> PluginResult<(Kernel, CompletionController)> {
        let kernel = Kernel::with_services(KernelServices {
            state: self.state.clone(),
            ..KernelServices::default()
        });
        let plugin = CompletionPlugin::new()?;
        let controller = plugin.controller();
        kernel.register(Box::new(plugin))?;
        kernel.start_all().await?;
        Ok((kernel, controller))
    }
    fn raw(&self) -> Option<Vec<u8>> {
        self.state
            .get(
                &PluginId::new(COMPLETION_PLUGIN_ID).unwrap(),
                COMPLETION_STATE_KEY,
            )
            .unwrap()
    }
    fn write(&self, bytes: Vec<u8>) {
        self.state
            .inner
            .set(
                &PluginId::new(COMPLETION_PLUGIN_ID).unwrap(),
                COMPLETION_STATE_KEY.into(),
                bytes,
            )
            .unwrap();
    }
}

fn target(owner: &str, plan: &str) -> QuestionTarget {
    QuestionTarget {
        owner: owner.into(),
        goal_id: format!("goal-of-{plan}"),
        goal_revision: 2,
        plan_id: plan.into(),
        plan_revision: 5,
    }
}

#[tokio::test]
async fn each_plan_is_asked_once_retried_at_most_three_times_and_never_after_delivery() {
    let world = World::new();
    let (_kernel, ledger) = world.open().await.unwrap();
    let alice = target("alice", "plan:a");
    assert!(ledger.snapshot().unwrap().askable("alice", "plan:a"));

    let first = ledger.claim(&alice, "m1", 10).unwrap();
    assert!(first.id.starts_with(ID_PREFIX));
    assert_eq!(first.status, QuestionStatus::Delivering);
    // 发送中不能再认领，也不能对同一消息重复认领。
    assert_eq!(
        ledger.claim(&alice, "m2", 11).unwrap_err(),
        CompletionError::AlreadyAsked
    );
    let retry = ledger
        .record(&first.id, AttemptResult::NotSent, 12)
        .unwrap();
    assert_eq!(retry.status, QuestionStatus::Retry);
    assert!(ledger.snapshot().unwrap().askable("alice", "plan:a"));
    assert_eq!(
        ledger.claim(&alice, "m1", 13).unwrap_err(),
        CompletionError::InvalidInput,
        "同一条消息不再附带第二次"
    );
    let second = ledger.claim(&alice, "m2", 14).unwrap();
    assert_eq!(second.attempts.len(), 2);
    ledger
        .record(
            &second.id,
            AttemptResult::Failed {
                http_status: Some(500),
                biz_code: None,
            },
            15,
        )
        .unwrap();
    ledger.claim(&alice, "m3", 16).unwrap();
    let exhausted = ledger
        .record(&second.id, AttemptResult::NotSent, 17)
        .unwrap();
    assert_eq!(exhausted.status, QuestionStatus::Exhausted);
    assert_eq!(exhausted.closed_at_ms, Some(17));
    assert!(!ledger.snapshot().unwrap().askable("alice", "plan:a"));
    assert_eq!(
        ledger.claim(&alice, "m4", 18).unwrap_err(),
        CompletionError::AlreadyAsked
    );

    // 送达后不再问；不同用户与不同计划互不影响。
    let bob = target("bob", "plan:b");
    let asked = ledger.claim(&bob, "m1", 20).unwrap();
    let delivered = ledger
        .record(
            &asked.id,
            AttemptResult::Sent {
                platform_message_id: Some("platform-1".into()),
            },
            21,
        )
        .unwrap();
    assert_eq!(delivered.status, QuestionStatus::Delivered);
    assert!(!ledger.snapshot().unwrap().askable("bob", "plan:b"));
    assert!(ledger.snapshot().unwrap().askable("bob", "plan:a"));
    assert_eq!(
        ledger
            .record(&asked.id, AttemptResult::NotSent, 22)
            .unwrap_err(),
        CompletionError::InvalidTransition
    );
    assert_eq!(
        ledger
            .record("goal-done-missing", AttemptResult::NotSent, 22)
            .unwrap_err(),
        CompletionError::NotFound
    );
}

#[tokio::test]
async fn delivering_becomes_unknown_after_restart_and_storage_failure_closes_the_instance() {
    let world = World::new();
    {
        let (_kernel, ledger) = world.open().await.unwrap();
        ledger.claim(&target("alice", "plan:a"), "m1", 10).unwrap();
    }
    let (_kernel, ledger) = world.open().await.unwrap();
    let snapshot = ledger.snapshot().unwrap();
    let question = snapshot.for_plan("alice", "plan:a").unwrap();
    assert_eq!(question.status, QuestionStatus::Unknown);
    assert_eq!(question.attempts[0].result, Some(AttemptResult::Unknown));
    assert!(!snapshot.askable("alice", "plan:a"), "结果未知不再问");

    world.state.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        ledger
            .claim(&target("bob", "plan:b"), "m1", 20)
            .unwrap_err(),
        CompletionError::Storage
    );
    world.state.fail.store(false, Ordering::SeqCst);
    assert_eq!(
        ledger.snapshot().unwrap_err(),
        CompletionError::Unavailable,
        "写入失败后实例关闭，不回退到旧缓存"
    );
}

#[tokio::test]
async fn corrupt_or_unknown_ledgers_refuse_to_start_and_are_preserved() {
    let world = World::new();
    for (bytes, expected) in [
        (
            br#"{"format_version":1,"format_version":1,"questions":[]}"#.to_vec(),
            "损坏",
        ),
        (br#"{"format_version":9,"questions":[]}"#.to_vec(), "版本"),
        (
            br#"{"format_version":1,"questions":[],"extra":1}"#.to_vec(),
            "损坏",
        ),
        (
            br#"{"format_version":1,"questions":[{"id":"goal-done-x","target":{"owner":"a","goal_id":"g","goal_revision":1,"plan_id":"p","plan_revision":1},"status":"Delivered","attempts":[],"created_at_ms":1,"closed_at_ms":1}]}"#.to_vec(),
            "损坏",
        ),
    ] {
        world.write(bytes.clone());
        let error = world.open().await.err().unwrap().to_string();
        assert!(error.contains(expected), "{error}");
        assert_eq!(world.raw().unwrap(), bytes, "原字节保留");
    }
}

#[tokio::test]
async fn full_ledger_drops_only_the_oldest_closed_record() {
    // 写入很多次；用内存存储，容量语义与后端无关。
    let kernel = Kernel::with_services(KernelServices {
        state: Arc::new(eve_kernel::backends::MemoryStateStore::default()),
        ..KernelServices::default()
    });
    let plugin = CompletionPlugin::new().unwrap();
    let ledger = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    // 第一条保持待重试（不会被删除），其余都已送达。
    let open = ledger.claim(&target("alice", "plan:open"), "m", 1).unwrap();
    ledger.record(&open.id, AttemptResult::NotSent, 2).unwrap();
    for index in 1..MAX_QUESTIONS {
        let plan = format!("plan:{index}");
        let question = ledger
            .claim(&target("alice", &plan), "m", 10 + index as u64)
            .unwrap();
        ledger
            .record(
                &question.id,
                AttemptResult::Sent {
                    platform_message_id: None,
                },
                10 + index as u64,
            )
            .unwrap();
    }
    assert_eq!(ledger.snapshot().unwrap().questions.len(), MAX_QUESTIONS);
    ledger
        .claim(&target("alice", "plan:new"), "m", 10_000)
        .unwrap();
    let snapshot = ledger.snapshot().unwrap();
    assert_eq!(snapshot.questions.len(), MAX_QUESTIONS);
    assert!(
        snapshot.for_plan("alice", "plan:1").is_none(),
        "最早结束的一条被腾出"
    );
    assert!(
        snapshot.for_plan("alice", "plan:open").is_some(),
        "待重试的记录不删除"
    );
}
