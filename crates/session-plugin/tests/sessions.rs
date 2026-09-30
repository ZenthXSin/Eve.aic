use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_llm_api::{ChatMessage, ChatRole, ToolCall, ToolFailureCode, ToolResult};
use eve_plugin_api::{PluginError, PluginId, PluginResult, ServiceId, StateStore};
use eve_session_api::*;
use eve_session_plugin::{SESSION_STATE_KEY, SessionPlugin};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default)]
struct FaultStore {
    memory: MemoryStateStore,
    fail: AtomicBool,
}
impl StateStore for FaultStore {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.memory.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(PluginError::State("backend-secret".into()));
        }
        self.memory.set(namespace, key, value)
    }
}
fn id() -> PluginId {
    PluginId::new(SESSION_PLUGIN_ID).unwrap()
}
fn key(session: &str, user: &str) -> SessionKey {
    SessionKey::new(session, user).unwrap()
}
fn input(session: &str, user: &str, text: &str) -> SessionInput {
    SessionInput {
        key: key(session, user),
        text: text.into(),
    }
}
fn simple(text: &str) -> Vec<ChatMessage> {
    vec![
        ChatMessage::text(ChatRole::User, text),
        ChatMessage::text(ChatRole::Assistant, "完成"),
    ]
}
fn service(
    kernel: &Kernel,
    registry: &Arc<dyn eve_plugin_api::ServiceRegistry>,
) -> Arc<dyn SessionService> {
    assert!(kernel.state(&id()).is_some());
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
) -> (
    Kernel,
    Arc<dyn eve_plugin_api::ServiceRegistry>,
    Arc<dyn SessionService>,
) {
    let backends = KernelServices {
        state,
        ..KernelServices::default()
    };
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    kernel
        .register(Box::new(SessionPlugin::new().unwrap()))
        .unwrap();
    kernel.start(&id()).await.unwrap();
    let sessions = service(&kernel, &registry);
    (kernel, registry, sessions)
}

#[tokio::test]
async fn isolates_owners_sessions_and_inflight_turns() {
    let (kernel, _, sessions) = open(Arc::new(MemoryStateStore::default())).await;
    let started = sessions.begin(input("s1", "u1", "秘密一")).unwrap();
    assert_eq!(
        sessions.begin(input("s1", "u1", "并发")),
        Err(SessionError::Busy)
    );
    assert_eq!(
        sessions.begin(input("s1", "u2", "串用户")),
        Err(SessionError::OwnerMismatch)
    );
    assert_eq!(
        sessions.snapshot(&key("s1", "u2")),
        Err(SessionError::OwnerMismatch)
    );
    let other = sessions.begin(input("s2", "u2", "秘密二")).unwrap();
    assert!(other.history.is_empty());
    sessions.complete(&started.lease, simple("秘密一")).unwrap();
    assert_eq!(
        sessions.complete(&started.lease, simple("秘密一")),
        Err(SessionError::StaleTurn)
    );
    assert!(
        sessions
            .snapshot(&key("s2", "u2"))
            .unwrap()
            .unwrap()
            .history()
            .is_empty()
    );
    kernel.stop_all().await.unwrap();
    assert_eq!(
        sessions.snapshot(&key("s1", "u1")),
        Err(SessionError::Unavailable)
    );
}

#[tokio::test]
async fn preserves_complete_tool_batches_and_structured_failures() {
    let (kernel, _, sessions) = open(Arc::new(MemoryStateStore::default())).await;
    let started = sessions.begin(input("s", "u", "工具问题")).unwrap();
    let calls = vec![
        ToolCall {
            id: "b".into(),
            name: "echo".into(),
            arguments: json!({"text":"中文"}),
        },
        ToolCall {
            id: "a".into(),
            name: "echo".into(),
            arguments: json!({}),
        },
    ];
    let messages = vec![
        ChatMessage::text(ChatRole::User, "工具问题"),
        ChatMessage::assistant_tool_calls(calls).unwrap(),
        ChatMessage::tool_results(vec![
            ToolResult::success("b", json!({"receipt":"本地回执"})).unwrap(),
            ToolResult::failure("a", ToolFailureCode::InvalidArguments, "参数拒绝").unwrap(),
        ])
        .unwrap(),
        ChatMessage::text(ChatRole::Assistant, "已处理"),
    ];
    sessions.complete(&started.lease, messages.clone()).unwrap();
    let next = sessions.begin(input("s", "u", "下一轮")).unwrap();
    assert_eq!(next.history, messages);
    assert_eq!(next.lease.turn_id, 2);
    assert_eq!(next.revision, 3);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn rejects_partial_foreign_and_mismatched_completed_history() {
    let (kernel, _, sessions) = open(Arc::new(MemoryStateStore::default())).await;
    let lease = sessions.begin(input("s", "u", "原输入")).unwrap().lease;
    for messages in [
        simple("其他输入"),
        vec![
            ChatMessage::text(ChatRole::User, "原输入"),
            ChatMessage::text(ChatRole::System, "系统注入"),
            ChatMessage::text(ChatRole::Assistant, "回复"),
        ],
        vec![ChatMessage::text(ChatRole::User, "原输入")],
    ] {
        assert_eq!(
            sessions.complete(&lease, messages),
            Err(SessionError::InvalidInput)
        );
    }
    let mut stale = lease.clone();
    stale.turn_id += 1;
    assert_eq!(
        sessions.complete(&stale, simple("原输入")),
        Err(SessionError::StaleTurn)
    );
    assert_eq!(sessions.snapshot(&lease.key).unwrap().unwrap().revision, 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn commits_before_memory_and_preserves_pending_when_write_fails() {
    let state = Arc::new(FaultStore::default());
    let (kernel, _, sessions) = open(state.clone()).await;
    state.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        sessions.begin(input("s", "u", "问题")),
        Err(SessionError::Storage)
    );
    assert!(sessions.snapshot(&key("s", "u")).unwrap().is_none());
    state.fail.store(false, Ordering::SeqCst);
    let lease = sessions.begin(input("s", "u", "问题")).unwrap().lease;
    let saved = state.get(&id(), SESSION_STATE_KEY).unwrap();
    state.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        sessions.complete(&lease, simple("问题")),
        Err(SessionError::Storage)
    );
    let snapshot = sessions.snapshot(&lease.key).unwrap().unwrap();
    assert_eq!(snapshot.revision, 1);
    assert_eq!(snapshot.turns[0].status, SessionTurnStatus::Pending);
    assert_eq!(state.get(&id(), SESSION_STATE_KEY).unwrap(), saved);
    state.fail.store(false, Ordering::SeqCst);
    sessions.complete(&lease, simple("问题")).unwrap();
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn recovery_marks_only_pending_interrupted_and_invalidates_old_handles() {
    let (kernel, registry, sessions) = open(Arc::new(MemoryStateStore::default())).await;
    let done = sessions.begin(input("s", "u", "已完成")).unwrap();
    sessions.complete(&done.lease, simple("已完成")).unwrap();
    let pending = sessions.begin(input("s", "u", "在途")).unwrap();
    kernel.stop_all().await.unwrap();
    kernel.start(&id()).await.unwrap();
    let recovered = service(&kernel, &registry);
    let snapshot = recovered.snapshot(&pending.lease.key).unwrap().unwrap();
    assert_eq!(snapshot.revision, 4);
    assert_eq!(snapshot.turns[1].status, SessionTurnStatus::Interrupted);
    assert_eq!(snapshot.history(), simple("已完成"));
    assert_eq!(
        recovered.complete(&pending.lease, simple("在途")),
        Err(SessionError::StaleTurn)
    );
    assert_eq!(
        sessions.snapshot(&pending.lease.key),
        Err(SessionError::Unavailable)
    );
    let next = recovered.begin(input("s", "u", "明确新请求")).unwrap();
    assert_eq!(next.lease.turn_id, 3);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn refuses_corrupt_versioned_or_duplicate_state_without_resetting_bytes() {
    let valid = json!({"format_version":1,"sessions":{}});
    let mut cases=vec![b"not json secret".to_vec(),serde_json::to_vec(&json!({"format_version":2,"sessions":{}})).unwrap(),b"{\"format_version\":1,\"format_version\":1,\"sessions\":{}}".to_vec(),serde_json::to_vec(&json!({"format_version":1,"sessions":{"s":{"key":{"session_id":"s","user_id":"u"},"revision":2,"turns":[{"id":1,"input":"问题","status":{"state":"Completed","messages":[{"role":"User","text":"问题","tool_calls":[],"tool_results":[]},{"role":"Assistant","text":"回答","tool_calls":[],"tool_results":[],"unknown":"secret"}]}}]}}})).unwrap()];
    let mut unknown = valid.clone();
    unknown["extra"] = json!(true);
    cases.push(serde_json::to_vec(&unknown).unwrap());
    let wrong = json!({"format_version":1,"sessions":{"s":{"key":{"session_id":"wrong","user_id":"u"},"revision":0,"turns":[]}}});
    cases.push(serde_json::to_vec(&wrong).unwrap());
    let pending = json!({"format_version":1,"sessions":{"s":{"key":{"session_id":"s","user_id":"u"},"revision":1,"turns":[{"id":1,"input":"问题","status":{"state":"Pending","unknown":"secret"}}]}}});
    cases.push(serde_json::to_vec(&pending).unwrap());
    for bytes in cases {
        let state = Arc::new(MemoryStateStore::default());
        state
            .set(&id(), SESSION_STATE_KEY.into(), bytes.clone())
            .unwrap();
        let kernel = Kernel::with_services(KernelServices {
            state: state.clone(),
            ..KernelServices::default()
        });
        kernel
            .register(Box::new(SessionPlugin::new().unwrap()))
            .unwrap();
        let error = kernel.start(&id()).await.unwrap_err();
        assert!(!error.to_string().contains("secret"));
        assert_eq!(state.get(&id(), SESSION_STATE_KEY).unwrap(), Some(bytes));
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn rejects_bad_tool_pairing_revision_and_ids_from_real_file_without_rewriting() {
    use eve_kernel::backends::FileStateStore;
    let messages = vec![
        ChatMessage::text(ChatRole::User, "问题"),
        ChatMessage::assistant_tool_calls(vec![
            ToolCall {
                id: "b".into(),
                name: "echo".into(),
                arguments: json!({}),
            },
            ToolCall {
                id: "a".into(),
                name: "echo".into(),
                arguments: json!({}),
            },
        ])
        .unwrap(),
        ChatMessage::tool_results(vec![
            ToolResult::success("b", json!(1)).unwrap(),
            ToolResult::success("a", json!(2)).unwrap(),
        ])
        .unwrap(),
        ChatMessage::text(ChatRole::Assistant, "完成"),
    ];
    let snapshot = SessionSnapshot {
        key: key("s", "u"),
        revision: 2,
        turns: vec![SessionTurn {
            id: 1,
            input: "问题".into(),
            status: SessionTurnStatus::Completed { messages },
        }],
    };
    let valid = json!({"format_version":1,"sessions":{"s":snapshot}});
    let mut cases = vec![];
    for location in ["swapped", "duplicate", "missing", "revision", "id"] {
        let mut document = valid.clone();
        let snapshot = &mut document["sessions"]["s"];
        match location {
            "swapped" => snapshot["turns"][0]["status"]["messages"][2]["tool_results"]
                .as_array_mut()
                .unwrap()
                .swap(0, 1),
            "duplicate" => {
                snapshot["turns"][0]["status"]["messages"][1]["tool_calls"][1]["id"] = json!("b")
            }
            "missing" => {
                snapshot["turns"][0]["status"]["messages"][2]["tool_results"]
                    .as_array_mut()
                    .unwrap()
                    .pop();
            }
            "revision" => snapshot["revision"] = json!(3),
            "id" => snapshot["turns"][0]["id"] = json!(2),
            _ => unreachable!(),
        }
        cases.push(document);
    }
    for document in cases {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStateStore::open(dir.path()).unwrap();
        store
            .set(
                &id(),
                SESSION_STATE_KEY.into(),
                serde_json::to_vec(&document).unwrap(),
            )
            .unwrap();
        drop(store);
        let before = std::fs::read(dir.path().join("state.json")).unwrap();
        let kernel = Kernel::with_services(KernelServices {
            state: Arc::new(FileStateStore::open(dir.path()).unwrap()),
            ..KernelServices::default()
        });
        kernel
            .register(Box::new(SessionPlugin::new().unwrap()))
            .unwrap();
        assert!(kernel.start(&id()).await.is_err());
        assert_eq!(
            std::fs::read(dir.path().join("state.json")).unwrap(),
            before
        );
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn failed_recovery_write_preserves_pending_and_can_be_retried() {
    let state = Arc::new(FaultStore::default());
    let (kernel, registry, sessions) = open(state.clone()).await;
    let pending = sessions.begin(input("s", "u", "在途")).unwrap();
    let bytes = state.get(&id(), SESSION_STATE_KEY).unwrap();
    kernel.stop_all().await.unwrap();
    state.fail.store(true, Ordering::SeqCst);
    assert!(kernel.start(&id()).await.is_err());
    assert_eq!(state.get(&id(), SESSION_STATE_KEY).unwrap(), bytes);
    state.fail.store(false, Ordering::SeqCst);
    // 沿用 Kernel 的 Failed 语义：明确重建插件，不能自动重新启动失败实例。
    kernel.unregister(&id()).unwrap();
    kernel
        .register(Box::new(SessionPlugin::new().unwrap()))
        .unwrap();
    kernel.start(&id()).await.unwrap();
    assert_eq!(
        service(&kernel, &registry)
            .snapshot(&pending.lease.key)
            .unwrap()
            .unwrap()
            .turns[0]
            .status,
        SessionTurnStatus::Interrupted
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn simultaneous_begin_has_exactly_one_winner() {
    let (kernel, _, sessions) = open(Arc::new(MemoryStateStore::default())).await;
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let service = sessions.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                service.begin(input("s", "u", "同时输入"))
            })
        })
        .collect();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| **r == Err(SessionError::Busy))
            .count(),
        1
    );
    kernel.stop_all().await.unwrap();
}
