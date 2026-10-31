use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_llm_api::{ChatMessage, ChatRole};
use eve_plugin_api::{PluginError, PluginId, PluginResult, ServiceId, StateStore};
use eve_session_api::*;
use eve_session_plugin::{FileSessionArchive, SESSION_STATE_KEY, SessionPlugin};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[test]
fn idempotent_archive_put_validates_every_byte_and_never_repairs_corrupt_existing_files() {
    fn hash(bytes: &[u8]) -> String {
        ring::digest::digest(&ring::digest::SHA256, bytes)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
    let root = tempfile::tempdir().unwrap();
    let store = FileSessionArchive::open(root.path()).unwrap();
    for bytes in [vec![], "多字节原文与回执".repeat(100_000).into_bytes()] {
        let key = hash(&bytes);
        store.put(&key, &bytes).unwrap();
        store.put(&key, &bytes).unwrap();
        assert_eq!(store.get(&key).unwrap(), bytes);
        let path = root.path().join(format!("{key}.json"));
        let corrupt = if bytes.is_empty() {
            vec![1]
        } else {
            let mut b = bytes.clone();
            let last = b.len() - 1;
            b[last] ^= 1;
            b
        };
        std::fs::write(&path, &corrupt).unwrap();
        assert_eq!(store.put(&key, &bytes), Err(SessionError::CorruptState));
        assert_eq!(std::fs::read(&path).unwrap(), corrupt);
        std::fs::write(&path, &bytes).unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len((MAX_SESSION_ARCHIVE_RECORD_BYTES + 1) as u64)
            .unwrap();
        assert_eq!(store.put(&key, &bytes), Err(SessionError::CorruptState));
        assert_eq!(
            file.metadata().unwrap().len(),
            (MAX_SESSION_ARCHIVE_RECORD_BYTES + 1) as u64
        );
    }
}

#[derive(Default)]
struct FaultState {
    memory: MemoryStateStore,
    fail_after: AtomicBool,
}
impl StateStore for FaultState {
    fn get(&self, n: &PluginId, k: &str) -> PluginResult<Option<Vec<u8>>> {
        self.memory.get(n, k)
    }
    fn set(&self, n: &PluginId, k: String, v: Vec<u8>) -> PluginResult<()> {
        self.memory.set(n, k, v)?;
        if self.fail_after.load(Ordering::SeqCst) {
            return Err(PluginError::State("ack lost".into()));
        }
        Ok(())
    }
}
struct FaultArchive {
    file: FileSessionArchive,
    fail_put: AtomicBool,
    gets: AtomicUsize,
}
impl SessionArchiveStore for FaultArchive {
    fn put(&self, k: &str, b: &[u8]) -> SessionResult<()> {
        self.file.put(k, b)?;
        if self.fail_put.load(Ordering::SeqCst) {
            return Err(SessionError::Storage);
        }
        Ok(())
    }
    fn get(&self, k: &str) -> SessionResult<Vec<u8>> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        self.file.get(k)
    }
}
fn id() -> PluginId {
    PluginId::new(SESSION_PLUGIN_ID).unwrap()
}
fn key(s: &str) -> SessionKey {
    SessionKey::new(s, "owner").unwrap()
}
fn input(s: &str, text: &str) -> SessionInput {
    SessionInput {
        key: key(s),
        text: text.into(),
    }
}
fn completed(text: &str) -> Vec<ChatMessage> {
    vec![
        ChatMessage::text(ChatRole::User, text),
        ChatMessage::text(ChatRole::Assistant, "已完成"),
    ]
}
fn document(state: &dyn StateStore) -> Value {
    serde_json::from_slice(&state.get(&id(), SESSION_STATE_KEY).unwrap().unwrap()).unwrap()
}
fn kernel(
    state: Arc<dyn StateStore>,
    archive: Option<Arc<dyn SessionArchiveStore>>,
) -> (Kernel, Arc<dyn eve_plugin_api::ServiceRegistry>) {
    let services = KernelServices {
        state,
        ..KernelServices::default()
    };
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
    let mut plugin = SessionPlugin::new().unwrap();
    if let Some(archive) = archive {
        plugin = plugin.with_archive(archive);
    }
    kernel.register(Box::new(plugin)).unwrap();
    (kernel, registry)
}
fn service(registry: &dyn eve_plugin_api::ServiceRegistry) -> Arc<dyn SessionService> {
    registry
        .get(&ServiceId::new(SESSION_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<SessionServiceHandle>()
        .unwrap()
        .0
        .clone()
}
async fn open(
    state: Arc<dyn StateStore>,
    archive: Arc<dyn SessionArchiveStore>,
) -> (Kernel, Arc<dyn SessionService>) {
    let (k, r) = kernel(state, Some(archive));
    k.start(&id()).await.unwrap();
    let s = service(r.as_ref());
    (k, s)
}
fn archive(root: &tempfile::TempDir) -> Arc<FaultArchive> {
    Arc::new(FaultArchive {
        file: FileSessionArchive::open(root.path()).unwrap(),
        fail_put: AtomicBool::new(false),
        gets: AtomicUsize::new(0),
    })
}

#[tokio::test]
async fn cold_and_hot_history_keep_identical_complete_turns_and_failure_meaning_after_restart() {
    let completed = vec![
        ChatMessage::text(ChatRole::User, "完整工具轮次"),
        ChatMessage::assistant_tool_calls(vec![eve_llm_api::ToolCall {
            id: "original".into(),
            name: "echo".into(),
            arguments: json!({"text":"完整参数"}),
        }])
        .unwrap(),
        ChatMessage::tool_results(vec![
            eve_llm_api::ToolResult::success("original", json!("原始回执".repeat(20_000))).unwrap(),
        ])
        .unwrap(),
        ChatMessage::text(ChatRole::Assistant, "完成了"),
    ];
    let mut histories = vec![];
    for cold in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let state = Arc::new(FaultState::default());
        let archive_backend: Option<Arc<dyn SessionArchiveStore>> =
            cold.then(|| archive(&root) as Arc<dyn SessionArchiveStore>);
        let (host, registry) = kernel(state.clone(), archive_backend.clone());
        host.start(&id()).await.unwrap();
        let sessions = service(registry.as_ref());
        let started = sessions.begin(input("a", "完整工具轮次")).unwrap();
        sessions
            .complete(&started.lease, completed.clone())
            .unwrap();
        for (text, code, started_tools, evidence) in [
            (
                "未调用工具的截断",
                SessionFailureCode::Provider,
                Some(0),
                Some(SessionFailureEvidence {
                    provider_http_status: None,
                    provider_failure: Some(eve_llm_api::ProviderFailureKind::Truncated),
                    tool_names: vec![],
                    tool_results: vec![],
                }),
            ),
            (
                "已有副作用的失败",
                SessionFailureCode::ProviderTimeout,
                Some(1),
                Some(SessionFailureEvidence {
                    provider_http_status: Some(503),
                    provider_failure: None,
                    tool_names: vec!["write_file".into()],
                    tool_results: vec![
                        eve_llm_api::ToolResult::success(
                            "failed-turn-receipt",
                            json!("失败现场只存档不重放"),
                        )
                        .unwrap(),
                    ],
                }),
            ),
            ("执行数量未知", SessionFailureCode::Backend, None, None),
            (
                "已取消的输入不回放",
                SessionFailureCode::Cancelled,
                Some(1),
                None,
            ),
        ] {
            let started = sessions.begin(input("a", text)).unwrap();
            sessions
                .fail(
                    &started.lease,
                    SessionFailure {
                        code,
                        started_tools,
                        evidence,
                    },
                )
                .unwrap();
        }
        let next = sessions.begin(input("a", "检查历史")).unwrap();
        assert_eq!(&next.history[..completed.len()], completed.as_slice());
        let encoded = serde_json::to_string(&next.history).unwrap();
        for expected in [
            "没有启动工具调用",
            "回复达到单次输出上限",
            "已启动 1 次工具调用",
            "HTTP 503",
            "执行数量未知",
        ] {
            assert!(encoded.contains(expected));
        }
        for omitted in [
            "已取消的输入不回放",
            "失败现场只存档不重放",
            "failed-turn-receipt",
        ] {
            assert!(!encoded.contains(omitted));
        }
        let raw = sessions.snapshot(&key("a")).unwrap().unwrap();
        assert!(
            serde_json::to_string(&raw)
                .unwrap()
                .contains("失败现场只存档不重放")
        );
        let expected = next.history;
        host.stop_all().await.unwrap();
        let (host, registry) = kernel(state, archive_backend);
        host.start(&id()).await.unwrap();
        assert_eq!(
            service(registry.as_ref())
                .begin(input("a", "重启检查"))
                .unwrap()
                .history,
            expected
        );
        histories.push(expected);
        host.stop_all().await.unwrap();
    }
    assert_eq!(histories[0], histories[1]);
}

#[tokio::test]
async fn migrates_exact_legacy_history_scopes_reset_and_interrupts_only_pending() {
    let root = tempfile::tempdir().unwrap();
    let archive = archive(&root);
    let state = Arc::new(FaultState::default());
    let (old, r) = kernel(state.clone(), None);
    old.start(&id()).await.unwrap();
    let legacy = service(r.as_ref());
    let a = legacy
        .begin(input("a", "原始输入，不能被摘要替换"))
        .unwrap();
    legacy
        .complete(&a.lease, completed("原始输入，不能被摘要替换"))
        .unwrap();
    legacy.reset_context(Some(&key("a"))).unwrap();
    let b = legacy.begin(input("b", "尚未完成")).unwrap();
    let before = legacy.snapshot(&key("a")).unwrap();
    old.stop_all().await.unwrap();
    let (k, s) = open(state.clone(), archive.clone()).await;
    assert_eq!(document(state.as_ref())["format_version"], 2);
    assert_eq!(s.snapshot(&key("a")).unwrap(), before);
    assert_eq!(
        s.snapshot(&key("b")).unwrap().unwrap().turns[0].status,
        SessionTurnStatus::Interrupted
    );
    assert_eq!(
        s.complete(&b.lease, completed("尚未完成")),
        Err(SessionError::StaleTurn)
    );
    assert_eq!(
        s.snapshot(&SessionKey::new("a", "other").unwrap()),
        Err(SessionError::OwnerMismatch)
    );
    let next = s.begin(input("a", "新要求")).unwrap();
    assert!(next.history.is_empty());
    assert_eq!(s.snapshot(&key("a")).unwrap().unwrap().turns.len(), 2);
    k.stop_all().await.unwrap();
    let (legacy, _) = kernel(state, None);
    assert!(legacy.start(&id()).await.is_err());
}

#[tokio::test]
async fn pages_pin_exact_versions_and_read_errors_close_all_handles_without_erasing() {
    let root = tempfile::tempdir().unwrap();
    let archive = archive(&root);
    let state = Arc::new(FaultState::default());
    let (k, s) = open(state.clone(), archive.clone()).await;
    for i in 0..20 {
        let text = format!("轮次{i}");
        let t = s.begin(input("a", &text)).unwrap();
        s.complete(&t.lease, completed(&text)).unwrap();
    }
    let pending = s.begin(input("a", "稍后完成")).unwrap();
    let mut pinned = s.scan_turns(&key("a")).unwrap().unwrap();
    s.complete(&pending.lease, completed("稍后完成")).unwrap();
    s.begin(input("a", "新追加")).unwrap();
    let first = pinned.next_page().unwrap();
    assert_eq!(first.len(), 16);
    let last = pinned.next_page().unwrap();
    assert_eq!(last.len(), 5);
    assert_eq!(last[4].status, SessionTurnStatus::Pending);
    assert!(pinned.next_page().unwrap().is_empty());
    let index = document(state.as_ref());
    let digest = index["sessions"]["a"]["turns"][17]["hash"]
        .as_str()
        .unwrap();
    let path = root.path().join(format!("{digest}.json"));
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"corrupt").unwrap();
    let before = state.get(&id(), SESSION_STATE_KEY).unwrap();
    let mut broken = s.scan_turns(&key("a")).unwrap().unwrap();
    assert_eq!(broken.next_page().unwrap().len(), 16);
    assert_eq!(broken.next_page(), Err(SessionError::CorruptState));
    std::fs::write(&path, bytes).unwrap();
    assert_eq!(broken.next_page(), Err(SessionError::Unavailable));
    assert_eq!(
        s.begin(input("b", "不能覆盖")),
        Err(SessionError::Unavailable)
    );
    assert_eq!(pinned.next_page(), Err(SessionError::Unavailable));
    assert_eq!(state.get(&id(), SESSION_STATE_KEY).unwrap(), before);
    k.stop_all().await.unwrap();
    let (k, s) = open(state, archive).await;
    assert_eq!(s.snapshot(&key("a")).unwrap().unwrap().turns.len(), 22);
    k.stop_all().await.unwrap();
}

#[tokio::test]
async fn committed_but_unacknowledged_index_is_not_overwritten_or_replayed() {
    let root = tempfile::tempdir().unwrap();
    let archive = archive(&root);
    let state = Arc::new(FaultState::default());
    let (k, s) = open(state.clone(), archive.clone()).await;
    let a = s.begin(input("a", "实际完成")).unwrap();
    let mut cursor = s.scan_turns(&key("a")).unwrap().unwrap();
    state.fail_after.store(true, Ordering::SeqCst);
    assert_eq!(
        s.complete(&a.lease, completed("实际完成")),
        Err(SessionError::Storage)
    );
    let committed = state.get(&id(), SESSION_STATE_KEY).unwrap();
    assert_eq!(cursor.next_page(), Err(SessionError::Unavailable));
    assert_eq!(
        s.begin(input("b", "禁止覆盖")),
        Err(SessionError::Unavailable)
    );
    assert_eq!(state.get(&id(), SESSION_STATE_KEY).unwrap(), committed);
    k.stop_all().await.unwrap();
    state.fail_after.store(false, Ordering::SeqCst);
    let (k, s) = open(state, archive).await;
    assert!(matches!(
        s.snapshot(&key("a")).unwrap().unwrap().turns[0].status,
        SessionTurnStatus::Completed { .. }
    ));
    assert_eq!(
        s.complete(&a.lease, completed("实际完成")),
        Err(SessionError::StaleTurn)
    );
    k.stop_all().await.unwrap();
}

#[tokio::test]
async fn body_write_error_preserves_index_and_startup_validates_every_body_before_recovery() {
    let root = tempfile::tempdir().unwrap();
    let archive = archive(&root);
    let state = Arc::new(FaultState::default());
    let (k, s) = open(state.clone(), archive.clone()).await;
    let a = s.begin(input("a", "已完成")).unwrap();
    s.complete(&a.lease, completed("已完成")).unwrap();
    let before = state.get(&id(), SESSION_STATE_KEY).unwrap();
    archive.fail_put.store(true, Ordering::SeqCst);
    assert_eq!(
        s.begin(input("b", "写了文件但未确认")),
        Err(SessionError::Storage)
    );
    assert_eq!(state.get(&id(), SESSION_STATE_KEY).unwrap(), before);
    assert_eq!(s.snapshot(&key("b")).unwrap(), None);
    archive.fail_put.store(false, Ordering::SeqCst);
    s.begin(input("b", "待恢复")).unwrap();
    k.stop_all().await.unwrap();
    let before = state.get(&id(), SESSION_STATE_KEY).unwrap();
    let index = document(state.as_ref());
    let digest = index["sessions"]["a"]["turns"][0]["hash"].as_str().unwrap();
    std::fs::write(root.path().join(format!("{digest}.json")), b"damaged").unwrap();
    let (k, _) = kernel(state.clone(), Some(archive));
    assert!(k.start(&id()).await.is_err());
    assert_eq!(state.get(&id(), SESSION_STATE_KEY).unwrap(), before);
}

#[tokio::test]
async fn indexing_and_finishing_do_not_rehydrate_other_histories() {
    let root = tempfile::tempdir().unwrap();
    let archive = archive(&root);
    let state = Arc::new(FaultState::default());
    let (k, s) = open(state.clone(), archive.clone()).await;
    for i in 0..8 {
        let name = format!("session{i}");
        let t = s.begin(input(&name, &"原文".repeat(400_000))).unwrap();
        s.complete(&t.lease, completed(&"原文".repeat(400_000)))
            .unwrap();
    }
    assert!(state.get(&id(), SESSION_STATE_KEY).unwrap().unwrap().len() < 4_000);
    archive.gets.store(0, Ordering::SeqCst);
    assert_eq!(s.list_keys(None, 100).unwrap().len(), 8);
    assert_eq!(archive.gets.load(Ordering::SeqCst), 0);
    let t = s.begin(input("new", "新会话")).unwrap();
    assert!(t.history.is_empty());
    assert_eq!(archive.gets.load(Ordering::SeqCst), 0);
    s.complete(&t.lease, completed("新会话")).unwrap();
    assert_eq!(archive.gets.load(Ordering::SeqCst), 1);
    k.stop_all().await.unwrap();
    let (k, s) = open(state, archive).await;
    assert_eq!(
        s.snapshot(&key("session3")).unwrap().unwrap().turns[0].input,
        "原文".repeat(400_000)
    );
    k.stop_all().await.unwrap();
}

#[tokio::test]
async fn strict_schema_and_oversize_bodies_fail_without_migration_commit() {
    let root = tempfile::tempdir().unwrap();
    let archive = archive(&root);
    let state = Arc::new(FaultState::default());
    state
        .set(
            &id(),
            SESSION_STATE_KEY.into(),
            serde_json::to_vec(&json!({"format_version":2,"sessions":{},"unknown":true})).unwrap(),
        )
        .unwrap();
    let before = state.get(&id(), SESSION_STATE_KEY).unwrap();
    let (k, _) = kernel(state.clone(), Some(archive.clone()));
    assert!(k.start(&id()).await.is_err());
    assert_eq!(state.get(&id(), SESSION_STATE_KEY).unwrap(), before);
    let fresh = Arc::new(FaultState::default());
    let (k, s) = open(fresh.clone(), archive).await;
    assert_eq!(
        s.begin(input("a", &"a".repeat(MAX_SESSION_ARCHIVE_RECORD_BYTES))),
        Err(SessionError::LimitReached)
    );
    assert_eq!(fresh.get(&id(), SESSION_STATE_KEY).unwrap(), None);
    k.stop_all().await.unwrap();
}

#[tokio::test]
async fn unacknowledged_begin_recovers_interrupted_and_failed_evidence_stays_exact() {
    let root = tempfile::tempdir().unwrap();
    let archive = archive(&root);
    let state = Arc::new(FaultState::default());
    let (k, s) = open(state.clone(), archive.clone()).await;
    state.fail_after.store(true, Ordering::SeqCst);
    assert_eq!(
        s.begin(input("a", "未收到开始确认")),
        Err(SessionError::Storage)
    );
    assert_eq!(s.list_keys(None, 10), Err(SessionError::Unavailable));
    k.stop_all().await.unwrap();
    state.fail_after.store(false, Ordering::SeqCst);
    let (k, s) = open(state.clone(), archive.clone()).await;
    assert_eq!(
        s.snapshot(&key("a")).unwrap().unwrap().turns[0].status,
        SessionTurnStatus::Interrupted
    );
    let next = s.begin(input("a", "有副作用的失败")).unwrap();
    assert_eq!(next.lease.turn_id, 2);
    let failure = SessionFailure {
        code: SessionFailureCode::Provider,
        started_tools: Some(1),
        evidence: Some(SessionFailureEvidence {
            provider_http_status: Some(503),
            provider_failure: None,
            tool_names: vec!["write_file".into()],
            tool_results: vec![
                eve_llm_api::ToolResult::success("write-1", json!("已写入的原始回执")).unwrap(),
            ],
        }),
    };
    state.fail_after.store(true, Ordering::SeqCst);
    assert_eq!(
        s.fail(&next.lease, failure.clone()),
        Err(SessionError::Storage)
    );
    k.stop_all().await.unwrap();
    state.fail_after.store(false, Ordering::SeqCst);
    let (k, s) = open(state, archive).await;
    assert_eq!(
        s.snapshot(&key("a")).unwrap().unwrap().turns[1].status,
        SessionTurnStatus::Failed { failure }
    );
    let after = s.begin(input("a", "继续核对")).unwrap();
    assert_eq!(after.lease.turn_id, 3);
    assert_eq!(after.history.len(), 2);
    assert!(
        after.history[1]
            .text
            .as_ref()
            .unwrap()
            .contains("已启动 1 次工具调用")
    );
    assert!(after.history.iter().all(|m| m.tool_calls.is_empty()));
    k.stop_all().await.unwrap();
}
