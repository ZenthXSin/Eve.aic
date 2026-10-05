use eve_kernel::backends::FileStateStore;
use eve_memory_api::{EvidenceSource, InteractionEvidence, MemoryScope, MemorySnapshot};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

/// 真实文件库；提交前失败和提交成功但确认丢失分别注入。
pub struct RecordingStore {
    inner: FileStateStore,
    fail_before: AtomicBool,
    fail_after: AtomicBool,
    writes: AtomicUsize,
}
impl RecordingStore {
    pub fn open(path: &Path) -> Arc<Self> {
        Arc::new(Self {
            inner: FileStateStore::open(path).unwrap(),
            fail_before: AtomicBool::new(false),
            fail_after: AtomicBool::new(false),
            writes: AtomicUsize::new(0),
        })
    }
    pub fn fail_before(&self, value: bool) {
        self.fail_before.store(value, Ordering::SeqCst);
    }
    pub fn fail_after(&self, value: bool) {
        self.fail_after.store(value, Ordering::SeqCst);
    }
    pub fn writes(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }
}
impl StateStore for RecordingStore {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.inner.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
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

pub fn scope(session: &str, user: &str) -> MemoryScope {
    MemoryScope {
        channel: "qq".into(),
        session_id: session.into(),
        user_id: user.into(),
    }
}

/// 合法只读快照：原始 Memory 的导入和送达边界另由组合测试覆盖。
pub fn memory(scope: MemoryScope, count: usize) -> MemorySnapshot {
    MemorySnapshot {
        scope,
        revision: count as u64,
        evidence: (1..=count)
            .map(|number| InteractionEvidence {
                id: format!("e-{number}"),
                revision: number as u64,
                at_ms: number as u64,
                source: EvidenceSource::CompletedInteraction {
                    message_id: format!("message-{number}"),
                    session_revision: number as u64 * 2,
                    turn_id: number as u64,
                    user_text: format!("请先给结论，这是第 {number} 次交互。"),
                    assistant_text: format!("第 {number} 次已完成的回复。"),
                },
            })
            .collect(),
        preferences: vec![],
    }
}
