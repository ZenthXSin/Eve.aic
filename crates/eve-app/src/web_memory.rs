//! 将已保存的 Web 最终回复与真实 Session 轮次核对后采集为交流记忆。
//!
//! 宿主只在 Web 完成回执持久化成功后调用；它证明回复已可恢复地提供给 Web，
//! 不证明浏览器已打开或用户已读。重启后读取同一回执可以补采，来源标识保持不变。
use eve_memory_api::{CompletedInteraction, MemoryAdmin, MemoryError, MemoryScope};
use eve_session_api::{SessionError, SessionService, SessionTurnStatus};
use eve_web_panel_api::{ChatLookup, ChatState};
use ring::digest::{Context, SHA256, digest};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) struct Observer {
    pub memory: Arc<dyn MemoryAdmin>,
    pub sessions: Arc<dyn SessionService>,
}

fn session_error(error: SessionError) -> MemoryError {
    match error {
        SessionError::InvalidInput | SessionError::OwnerMismatch | SessionError::StaleTurn => {
            MemoryError::InvalidInput
        }
        SessionError::CorruptState => MemoryError::CorruptState,
        SessionError::UnsupportedVersion => MemoryError::UnsupportedVersion,
        SessionError::Storage => MemoryError::Storage,
        SessionError::LimitReached => MemoryError::LimitReached,
        SessionError::Busy | SessionError::Unavailable => MemoryError::Unavailable,
    }
}

impl Observer {
    /// 非模型本地控制回复、失败或在途轮次不采集；存储错误原样交还宿主报告。
    /// 只重试确定没有提交的 CAS 冲突，最多三次。存储确认丢失时须重开后核对，
    /// 不能把一次不确定的提交当作成功，也不能重新执行模型或工具。
    /// `fingerprint` 必须来自宿主持久回执和 claim 中绑定的用户原文 SHA256；
    /// 不从待导入轮次反算后当作授权证据，避免同回复的其他轮次被错误关联。
    pub(crate) fn record(&self, state: &ChatState, fingerprint: &str) -> Result<(), MemoryError> {
        if state.status != "completed" {
            return Ok(());
        }
        let Some(generation) = &state.generation else {
            // Web 本地业务控制回复没有 Session 模型轮次。
            return Ok(());
        };
        ChatLookup {
            conversation_id: state.conversation_id.clone(),
            request_id: state.request_id.clone(),
        }
        .validate()
        .map_err(|_| MemoryError::InvalidInput)?;
        let key =
            crate::web_chat::key(&state.conversation_id).map_err(|_| MemoryError::InvalidInput)?;
        if generation.session != key
            || generation.generation == 0
            || generation.task_id != format!("web-request:{}", state.request_id)
            || state.failure.is_some()
        {
            return Err(MemoryError::InvalidInput);
        }
        let turn_id = state
            .turn_id
            .filter(|id| *id > 0)
            .ok_or(MemoryError::InvalidInput)?;
        let text = state.text.as_ref().ok_or(MemoryError::InvalidInput)?;
        let snapshot = self
            .sessions
            .snapshot(&key)
            .map_err(session_error)?
            .ok_or(MemoryError::NotFound)?;
        if snapshot.key != key {
            return Err(MemoryError::InvalidInput);
        }
        snapshot.validate().map_err(|_| MemoryError::CorruptState)?;
        let turn = snapshot
            .turns
            .iter()
            .find(|turn| turn.id == turn_id)
            .ok_or(MemoryError::InvalidInput)?;
        let SessionTurnStatus::Completed { messages } = &turn.status else {
            return Err(MemoryError::InvalidInput);
        };
        let input_fingerprint: String = digest(&SHA256, turn.input.as_bytes())
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if messages.last().and_then(|message| message.text.as_ref()) != Some(text)
            || fingerprint != input_fingerprint
        {
            return Err(MemoryError::InvalidInput);
        }

        let scope = MemoryScope {
            channel: "web".into(),
            session_id: key.session_id,
            user_id: key.user_id,
        };
        let mut hash = Context::new(&SHA256);
        for part in [
            scope.channel.as_bytes(),
            scope.session_id.as_bytes(),
            scope.user_id.as_bytes(),
            state.request_id.as_bytes(),
        ] {
            hash.update(&(part.len() as u64).to_be_bytes());
            hash.update(part);
        }
        let id: String = hash
            .finish()
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|time| u64::try_from(time.as_millis()).ok())
            .ok_or(MemoryError::Unavailable)?;
        let interaction = CompletedInteraction {
            evidence_id: format!("web-interaction-{id}"),
            message_id: generation.task_id.clone(),
            at_ms,
            snapshot,
            turn_id,
        };
        let reader = self.memory.reader(scope.clone())?;
        for _ in 0..3 {
            let before = reader.snapshot()?;
            if before.scope != scope {
                return Err(MemoryError::CorruptState);
            }
            match self
                .memory
                .import_completed(&scope, before.revision, interaction.clone())
            {
                Ok(saved) if saved.scope == scope => return Ok(()),
                Ok(_) => return Err(MemoryError::CorruptState),
                Err(MemoryError::StaleRevision) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(MemoryError::StaleRevision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_control_api::GenerationKey;
    use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
    use eve_llm_api::{ChatMessage, ChatRole, ToolCall, ToolResult};
    use eve_memory_api::{
        EvidenceSource, MemoryResult, MemoryService, MemorySnapshot, PreferenceChange,
    };
    use eve_memory_plugin::{MEMORY_STATE_KEY, MemoryPlugin};
    use eve_plugin_api::{PluginError, PluginId, PluginResult, ServiceId, StateStore};
    use eve_session_api::{
        SESSION_SERVICE_ID, SessionFailure, SessionFailureCode, SessionInput, SessionServiceHandle,
    };
    use eve_session_plugin::SessionPlugin;
    use std::{
        path::Path,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    const CONVERSATION: &str = "11111111111111111111111111111111";
    const OTHER_CONVERSATION: &str = "22222222222222222222222222222222";
    const REQUEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER_REQUEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn fingerprint(input: &str) -> String {
        digest(&SHA256, input.as_bytes())
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// 真实文件库，计数只针对记忆提交，可模拟落盘后确认丢失。
    struct Store {
        inner: FileStateStore,
        memory_writes: AtomicUsize,
        fail_after_memory_write: AtomicBool,
    }
    impl Store {
        fn open(path: &Path) -> Arc<Self> {
            Arc::new(Self {
                inner: FileStateStore::open(path).unwrap(),
                memory_writes: AtomicUsize::new(0),
                fail_after_memory_write: AtomicBool::new(false),
            })
        }
        fn bytes(&self) -> Vec<u8> {
            self.get(
                &PluginId::new(eve_memory_api::MEMORY_PLUGIN_ID).unwrap(),
                MEMORY_STATE_KEY,
            )
            .unwrap()
            .unwrap()
        }
        fn writes(&self) -> usize {
            self.memory_writes.load(Ordering::SeqCst)
        }
    }
    impl StateStore for Store {
        fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
            self.inner.get(namespace, key)
        }
        fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
            let is_memory = namespace == &PluginId::new(eve_memory_api::MEMORY_PLUGIN_ID).unwrap();
            if is_memory {
                self.memory_writes.fetch_add(1, Ordering::SeqCst);
            }
            self.inner.set(namespace, key, value)?;
            if is_memory && self.fail_after_memory_write.load(Ordering::SeqCst) {
                return Err(PluginError::State("测试：记忆提交确认丢失".into()));
            }
            Ok(())
        }
    }

    async fn open(store: Arc<dyn StateStore>) -> (Kernel, Observer) {
        let services = KernelServices {
            state: store,
            ..Default::default()
        };
        let registry = services.registry.clone();
        let kernel = Kernel::with_services(services);
        let memory = MemoryPlugin::new().unwrap();
        let admin = Arc::new(memory.controller());
        kernel.register(Box::new(memory)).unwrap();
        kernel
            .register(Box::new(SessionPlugin::new().unwrap()))
            .unwrap();
        kernel.start_all().await.unwrap();
        let sessions = registry
            .get(&ServiceId::new(SESSION_SERVICE_ID).unwrap())
            .unwrap()
            .unwrap()
            .value
            .downcast::<SessionServiceHandle>()
            .unwrap()
            .0
            .clone();
        (
            kernel,
            Observer {
                memory: admin,
                sessions,
            },
        )
    }
    fn completed(
        observer: &Observer,
        conversation: &str,
        request: &str,
        input: &str,
        reply: &str,
    ) -> ChatState {
        let key = crate::web_chat::key(conversation).unwrap();
        let turn = observer
            .sessions
            .begin(SessionInput {
                key: key.clone(),
                text: input.into(),
            })
            .unwrap();
        observer
            .sessions
            .complete(
                &turn.lease,
                vec![
                    ChatMessage::text(ChatRole::User, input),
                    ChatMessage::assistant_tool_calls(vec![ToolCall {
                        id: "test-tool".into(),
                        name: "echo".into(),
                        arguments: serde_json::json!({"private":"不采集工具参数"}),
                    }])
                    .unwrap(),
                    ChatMessage::tool_results(vec![
                        ToolResult::success("test-tool", serde_json::json!("不采集工具结果"))
                            .unwrap(),
                    ])
                    .unwrap(),
                    ChatMessage::text(ChatRole::Assistant, reply),
                ],
            )
            .unwrap();
        ChatState {
            conversation_id: conversation.into(),
            request_id: request.into(),
            status: "completed".into(),
            generation: Some(GenerationKey {
                session: key,
                task_id: format!("web-request:{request}"),
                controller_epoch: [1; 16],
                generation: turn.lease.turn_id,
            }),
            turn_id: Some(turn.lease.turn_id),
            text: Some(reply.into()),
            failure: None,
            memory_error: None,
            started_tools: Some(1),
            tool_receipts: 1,
        }
    }
    fn read(observer: &Observer, conversation: &str) -> MemorySnapshot {
        let key = crate::web_chat::key(conversation).unwrap();
        observer
            .memory
            .reader(MemoryScope {
                channel: "web".into(),
                session_id: key.session_id,
                user_id: key.user_id,
            })
            .unwrap()
            .snapshot()
            .unwrap()
    }

    #[tokio::test]
    async fn exact_completed_reply_is_saved_once_and_replays_after_file_store_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path());
        let (kernel, observer) = open(store.clone()).await;
        let input = " 请记住这次真实对话。\n保留空白。 ";
        let reply = " 已保存回复。\n末尾空白也保留。 ";
        let mut state = completed(&observer, CONVERSATION, REQUEST, input, reply);
        // 回执记录的前次采集错误不能阻止依据真实 Session 补采。
        state.memory_error = Some("交互记忆服务不可用".into());
        observer.record(&state, &fingerprint(input)).unwrap();
        let saved = read(&observer, CONVERSATION);
        assert_eq!(saved.revision, 1);
        assert_eq!(saved.evidence.len(), 1);
        assert_eq!(
            saved.evidence[0].source,
            EvidenceSource::CompletedInteraction {
                message_id: format!("web-request:{REQUEST}"),
                session_revision: 2,
                turn_id: 1,
                user_text: input.into(),
                assistant_text: reply.into(),
            }
        );
        let bytes = store.bytes();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(!text.contains("不采集工具参数"));
        assert!(!text.contains("不采集工具结果"));
        for _ in 0..3 {
            observer.record(&state, &fingerprint(input)).unwrap();
        }
        assert_eq!(store.writes(), 1);
        assert_eq!(store.bytes(), bytes);
        kernel.stop_all().await.unwrap();
        drop(observer);
        drop(kernel);
        drop(store);

        let store = Store::open(directory.path());
        let (kernel, observer) = open(store.clone()).await;
        // 新轮次改变 Session 修订，不能把旧来源更新为新来源或重复计数。
        let _later = completed(
            &observer,
            CONVERSATION,
            OTHER_REQUEST,
            "后来问题",
            "后来回复",
        );
        observer.record(&state, &fingerprint(input)).unwrap();
        assert_eq!(read(&observer, CONVERSATION), saved);
        assert_eq!(store.writes(), 0);
        assert_eq!(store.bytes(), bytes);
        kernel.stop_all().await.unwrap();
    }

    #[tokio::test]
    async fn request_ids_are_scoped_and_inconsistent_receipts_never_write() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path());
        let (kernel, observer) = open(store.clone()).await;
        let one = completed(&observer, CONVERSATION, REQUEST, "第一会话", "第一回复");
        let two = completed(
            &observer,
            OTHER_CONVERSATION,
            REQUEST,
            "第二会话",
            "第二回复",
        );
        observer.record(&one, &fingerprint("第一会话")).unwrap();
        observer.record(&two, &fingerprint("第二会话")).unwrap();
        assert_ne!(
            read(&observer, CONVERSATION).evidence[0].id,
            read(&observer, OTHER_CONVERSATION).evidence[0].id
        );
        let bytes = store.bytes();
        for variant in 0..11 {
            let mut invalid = one.clone();
            match variant {
                0 => invalid.conversation_id = OTHER_CONVERSATION.into(),
                1 => invalid.generation.as_mut().unwrap().session.user_id = "qq:owner".into(),
                2 => invalid.generation.as_mut().unwrap().task_id = "wrong-request".into(),
                3 => invalid.text = Some("未实际保存的回复".into()),
                4 => invalid.turn_id = Some(999),
                5 => invalid.turn_id = None,
                6 => invalid.failure = Some("存在失败".into()),
                7 => invalid.generation.as_mut().unwrap().generation = 0,
                8 => invalid.request_id = "不是请求编号".into(),
                9 => invalid.text = None,
                _ => invalid.turn_id = Some(0),
            }
            assert_eq!(
                observer.record(&invalid, &fingerprint("第一会话")),
                Err(MemoryError::InvalidInput)
            );
        }
        assert_eq!(store.writes(), 2);
        assert_eq!(store.bytes(), bytes);
        assert_eq!(observer.memory.scopes().unwrap().len(), 2);
        assert!(observer.memory.revisions("qq").unwrap().is_empty());
        kernel.stop_all().await.unwrap();
    }

    #[tokio::test]
    async fn pending_failed_and_local_control_results_are_not_completed_interactions() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path());
        let (kernel, observer) = open(store.clone()).await;
        let mut state = completed(&observer, CONVERSATION, REQUEST, "真实输入", "真实回复");
        for status in ["accepted", "generating", "failed", "unknown", "interrupted"] {
            state.status = status.into();
            observer.record(&state, &fingerprint("真实输入")).unwrap();
        }
        state.status = "completed".into();
        let mut local = state.clone();
        local.generation = None;
        local.turn_id = None;
        local.text = Some("业务权限已开启".into());
        observer.record(&local, &fingerprint("真实输入")).unwrap();
        let pending = observer
            .sessions
            .begin(SessionInput {
                key: crate::web_chat::key(CONVERSATION).unwrap(),
                text: "尚未生成回复".into(),
            })
            .unwrap();
        state.turn_id = Some(pending.lease.turn_id);
        assert_eq!(
            observer.record(&state, &fingerprint("真实输入")),
            Err(MemoryError::InvalidInput)
        );
        observer
            .sessions
            .fail(
                &pending.lease,
                SessionFailure {
                    code: SessionFailureCode::Provider,
                    started_tools: Some(0),
                    evidence: None,
                },
            )
            .unwrap();
        assert_eq!(
            observer.record(&state, &fingerprint("真实输入")),
            Err(MemoryError::InvalidInput)
        );
        assert_eq!(store.writes(), 0);
        assert!(observer.memory.scopes().unwrap().is_empty());
        kernel.stop_all().await.unwrap();
    }

    #[tokio::test]
    async fn lost_storage_confirmation_is_an_error_then_reopen_deduplicates_the_commit() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path());
        let (kernel, observer) = open(store.clone()).await;
        let state = completed(&observer, CONVERSATION, REQUEST, "真实输入", "真实回复");
        store.fail_after_memory_write.store(true, Ordering::SeqCst);
        assert_eq!(
            observer.record(&state, &fingerprint("真实输入")),
            Err(MemoryError::Storage)
        );
        assert_eq!(store.writes(), 1);
        let bytes = store.bytes();
        assert_eq!(
            observer.record(&state, &fingerprint("真实输入")),
            Err(MemoryError::Unavailable)
        );
        assert_eq!(store.writes(), 1);
        kernel.stop_all().await.unwrap();
        drop(observer);
        drop(kernel);
        drop(store);

        let store = Store::open(directory.path());
        let (kernel, observer) = open(store.clone()).await;
        observer.record(&state, &fingerprint("真实输入")).unwrap();
        let saved = read(&observer, CONVERSATION);
        assert_eq!(saved.revision, 1);
        assert_eq!(saved.evidence.len(), 1);
        assert_eq!(store.bytes(), bytes);
        assert_eq!(store.writes(), 0);
        kernel.stop_all().await.unwrap();
    }

    /// 真实记忆实现之前注入确定拒绝；调用计数用于验证重试边界。
    struct RejectingMemory {
        inner: Arc<dyn MemoryAdmin>,
        error: MemoryError,
        reject_calls: usize,
        calls: AtomicUsize,
    }
    impl MemoryAdmin for RejectingMemory {
        fn reader(&self, scope: MemoryScope) -> MemoryResult<Arc<dyn MemoryService>> {
            self.inner.reader(scope)
        }
        fn import_completed(
            &self,
            scope: &MemoryScope,
            revision: u64,
            interaction: CompletedInteraction,
        ) -> MemoryResult<MemorySnapshot> {
            if self.calls.fetch_add(1, Ordering::SeqCst) < self.reject_calls {
                return Err(self.error.clone());
            }
            self.inner.import_completed(scope, revision, interaction)
        }
        fn update_preference(
            &self,
            scope: &MemoryScope,
            revision: u64,
            change: PreferenceChange,
        ) -> MemoryResult<MemorySnapshot> {
            self.inner.update_preference(scope, revision, change)
        }
    }

    #[tokio::test]
    async fn only_cas_conflicts_retry_and_all_retries_have_a_fixed_limit() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path());
        let (kernel, observer) = open(store.clone()).await;
        let state = completed(&observer, CONVERSATION, REQUEST, "真实输入", "真实回复");
        for (error, reject_calls, expected_calls, expected) in [
            (MemoryError::StaleRevision, 2, 3, Ok(())),
            (
                MemoryError::StaleRevision,
                usize::MAX,
                3,
                Err(MemoryError::StaleRevision),
            ),
            (
                MemoryError::Conflict,
                usize::MAX,
                1,
                Err(MemoryError::Conflict),
            ),
            (
                MemoryError::LimitReached,
                usize::MAX,
                1,
                Err(MemoryError::LimitReached),
            ),
        ] {
            let memory = Arc::new(RejectingMemory {
                inner: observer.memory.clone(),
                error,
                reject_calls,
                calls: AtomicUsize::new(0),
            });
            let injected = Observer {
                memory: memory.clone(),
                sessions: observer.sessions.clone(),
            };
            assert_eq!(injected.record(&state, &fingerprint("真实输入")), expected);
            assert_eq!(memory.calls.load(Ordering::SeqCst), expected_calls);
        }
        assert_eq!(store.writes(), 1);
        assert_eq!(read(&observer, CONVERSATION).revision, 1);
        kernel.stop_all().await.unwrap();
    }

    #[tokio::test]
    async fn oversized_reply_is_not_truncated_and_original_completed_session_is_preserved() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path());
        let (kernel, observer) = open(store.clone()).await;
        let reply = "x".repeat(eve_memory_api::MAX_TEXT_BYTES + 1);
        let state = completed(&observer, CONVERSATION, REQUEST, "真实输入", &reply);
        let key = crate::web_chat::key(CONVERSATION).unwrap();
        let before = observer.sessions.snapshot(&key).unwrap().unwrap();
        assert_eq!(
            observer.record(&state, &fingerprint("真实输入")),
            Err(MemoryError::InvalidInput)
        );
        assert_eq!(store.writes(), 0);
        assert!(observer.memory.scopes().unwrap().is_empty());
        assert_eq!(observer.sessions.snapshot(&key).unwrap(), Some(before));
        assert_eq!(state.status, "completed");
        assert_eq!(state.text.as_deref(), Some(reply.as_str()));
        kernel.stop_all().await.unwrap();
    }

    #[tokio::test]
    async fn identical_assistant_replies_cannot_bind_another_requests_user_input() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path());
        let (kernel, observer) = open(store.clone()).await;
        let first = completed(
            &observer,
            CONVERSATION,
            REQUEST,
            "第一条用户原文",
            "相同回复",
        );
        let second = completed(
            &observer,
            CONVERSATION,
            OTHER_REQUEST,
            "另一条用户原文",
            "相同回复",
        );
        let mut wrong_turn = first.clone();
        wrong_turn.turn_id = second.turn_id;
        assert_eq!(
            observer.record(&wrong_turn, &fingerprint("第一条用户原文")),
            Err(MemoryError::InvalidInput)
        );
        for wrong_fingerprint in [
            "".to_string(),
            "0".repeat(64),
            fingerprint("另一条用户原文"),
        ] {
            assert_eq!(
                observer.record(&first, &wrong_fingerprint),
                Err(MemoryError::InvalidInput)
            );
        }
        assert_eq!(store.writes(), 0);
        observer
            .record(&first, &fingerprint("第一条用户原文"))
            .unwrap();
        assert_eq!(store.writes(), 1);
        assert!(matches!(
            &read(&observer, CONVERSATION).evidence[0].source,
            EvidenceSource::CompletedInteraction { user_text, turn_id: 1, .. }
                if user_text == "第一条用户原文"
        ));
        kernel.stop_all().await.unwrap();
    }
}
