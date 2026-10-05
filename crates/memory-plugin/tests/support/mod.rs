use eve_kernel::backends::FileStateStore;
use eve_llm_api::{ChatMessage, ChatRole};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use eve_session_api::{SessionInput, SessionKey, SessionService, SessionSnapshot};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

/// 包装真实文件库，分别模拟提交前失败和已提交但确认丢失。
pub struct RecordingStore {
    inner: FileStateStore,
    fail_write: AtomicBool,
    fail_after_write: AtomicBool,
    writes: AtomicUsize,
}

impl RecordingStore {
    pub fn open(path: &Path) -> Arc<Self> {
        Arc::new(Self {
            inner: FileStateStore::open(path).unwrap(),
            fail_write: AtomicBool::new(false),
            fail_after_write: AtomicBool::new(false),
            writes: AtomicUsize::new(0),
        })
    }

    pub fn fail_writes(&self, value: bool) {
        self.fail_write.store(value, Ordering::SeqCst);
    }

    pub fn writes(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }

    pub fn fail_after_writes(&self, value: bool) {
        self.fail_after_write.store(value, Ordering::SeqCst);
    }
}

impl StateStore for RecordingStore {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.inner.get(namespace, key)
    }

    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        if self.fail_write.load(Ordering::SeqCst) {
            return Err(PluginError::State("test-storage-private-detail".into()));
        }
        self.inner.set(namespace, key, value)?;
        if self.fail_after_write.load(Ordering::SeqCst) {
            return Err(PluginError::State("test-storage-private-detail".into()));
        }
        Ok(())
    }
}

pub fn complete_session(
    sessions: &dyn SessionService,
    key: &SessionKey,
    input: &str,
    reply: &str,
) -> SessionSnapshot {
    let turn = sessions
        .begin(SessionInput {
            key: key.clone(),
            text: input.into(),
        })
        .unwrap();
    sessions
        .complete(
            &turn.lease,
            vec![
                ChatMessage::text(ChatRole::User, input),
                ChatMessage::text(ChatRole::Assistant, reply),
            ],
        )
        .unwrap();
    sessions.snapshot(key).unwrap().unwrap()
}
