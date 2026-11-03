use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use eve_task_api::*;
use eve_task_plugin::{FileTaskArchive, TaskController, TaskPlugin};
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
            return Err(PluginError::State("injected storage failure".into()));
        }
        self.inner.set(namespace, key, value)
    }
}
async fn open(
    state: Arc<Store>,
    archive: Option<Arc<dyn TaskArchiveStore>>,
) -> (Kernel, TaskController) {
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let mut plugin = TaskPlugin::new().unwrap();
    if let Some(archive) = archive {
        plugin = plugin.with_archive(archive);
    }
    let tasks = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, tasks)
}
fn owner() -> TaskOwner {
    TaskOwner {
        user_id: "owner".into(),
        session_id: "trusted-session".into(),
    }
}
fn spec() -> TaskSpec {
    TaskSpec {
        title: "验证部署".into(),
        instructions: "按原授权范围核验部署".into(),
        max_steps: 100,
        context: None,
    }
}
fn originals() -> Vec<String> {
    (0..6)
        .map(|index| {
            format!(
                "第 {index} 条完整用户原话\n{}\n完整尾部",
                "范围保持不变。".repeat(300)
            )
        })
        .collect()
}
fn state(directory: &std::path::Path) -> Arc<Store> {
    Arc::new(Store {
        inner: FileStateStore::open(directory).unwrap(),
        fail: AtomicBool::new(false),
    })
}

#[tokio::test]
async fn authorization_and_contract_are_atomic_immutable_and_scope_bound_across_restart() {
    for cold in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let state = state(&directory.path().join("state"));
        let archive: Option<Arc<dyn TaskArchiveStore>> = cold.then(|| {
            Arc::new(FileTaskArchive::open(directory.path().join("archive")).unwrap())
                as Arc<dyn TaskArchiveStore>
        });
        let (kernel, tasks) = open(state.clone(), archive.clone()).await;
        let messages = originals();
        let created = tasks
            .create_authorized(
                &owner(),
                spec(),
                "dispatch",
                messages.clone(),
                Some(TaskCompletionContract::Unspecified),
                Some(u64::MAX),
                10,
            )
            .unwrap();
        let execution = created.execution.as_ref().unwrap();
        assert_eq!(execution.review_messages, messages);
        assert!(matches!(
            execution.completion_contract,
            Some(TaskCompletionContract::Unspecified)
        ));
        assert_eq!(execution.authorization_deadline_at_ms, Some(u64::MAX));
        if cold {
            let bytes = state
                .get(&PluginId::new(TASK_PLUGIN_ID).unwrap(), TASK_STATE_KEY)
                .unwrap()
                .unwrap();
            let index = String::from_utf8(bytes).unwrap();
            assert!(
                !index.contains("完整用户原话"),
                "冷任务目录不能复制授权正文"
            );
        }
        // 同一派发重试返回原任务，不把最新文字换成旧任务的授权。
        let duplicate = tasks
            .create_authorized(
                &owner(),
                spec(),
                "dispatch",
                vec!["后来的不同授权".into()],
                None,
                None,
                11,
            )
            .unwrap();
        assert_eq!(duplicate.execution.unwrap().review_messages, messages);
        let mut foreign = owner();
        foreign.session_id = "other-window".into();
        assert!(matches!(
            tasks.create_authorized(
                &foreign,
                spec(),
                "dispatch",
                messages.clone(),
                None,
                None,
                12
            ),
            Err(TaskError::NotFound)
        ));
        kernel.stop_all().await.unwrap();
        let (kernel, tasks) = open(state, archive).await;
        let restored = tasks.get_owned(&owner(), &created.id).unwrap().unwrap();
        assert_eq!(restored.execution.unwrap().review_messages, messages);
        assert!(tasks.get_owned(&foreign, &created.id).unwrap().is_none());
        assert_eq!(restored.status, TaskStatus::Queued);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn oversized_or_unsaved_authorization_never_creates_a_dispatchable_task() {
    let directory = tempfile::tempdir().unwrap();
    let state = state(&directory.path().join("state"));
    let (kernel, tasks) = open(state.clone(), None).await;
    for messages in [
        vec!["x".into(); 7],
        vec!["x".repeat(eve_llm_api::MAX_REVIEW_USER_MESSAGES_BYTES)],
    ] {
        assert!(matches!(
            tasks.create_authorized(&owner(), spec(), "invalid", messages, None, None, 10),
            Err(TaskError::InvalidInput)
        ));
    }
    assert!(tasks.snapshot().unwrap().tasks.is_empty());
    state.fail.store(true, Ordering::SeqCst);
    assert!(matches!(
        tasks.create_authorized(
            &owner(),
            spec(),
            "failed",
            originals(),
            Some(TaskCompletionContract::Unspecified),
            None,
            11
        ),
        Err(TaskError::Storage)
    ));
    state.fail.store(false, Ordering::SeqCst);
    kernel.stop_all().await.unwrap();
    let (kernel, tasks) = open(state, None).await;
    assert!(tasks.snapshot().unwrap().tasks.is_empty());
    let created = tasks
        .create_authorized(
            &owner(),
            spec(),
            "empty",
            vec![],
            Some(TaskCompletionContract::Unspecified),
            None,
            12,
        )
        .unwrap();
    assert!(created.execution.unwrap().review_messages.is_empty());
    kernel.stop_all().await.unwrap();
}
