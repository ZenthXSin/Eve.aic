#![allow(dead_code)]
use eve_interest_api::{
    InterestAdmin, InterestTarget, InterestUpdateDraft, ObservationBatch, ObservationOptions,
    StatementDraft, StatementKind,
};
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

pub const INTEREST: &str = "我喜欢 Mindustry 这个游戏的模组，但是不知道怎么创作。";

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

pub fn evidence(number: u64, user_text: &str) -> InteractionEvidence {
    InteractionEvidence {
        id: format!("e-{number}"),
        revision: number,
        at_ms: number * 10,
        source: EvidenceSource::CompletedInteraction {
            message_id: format!("message-{number}"),
            session_revision: number * 2,
            turn_id: number,
            user_text: user_text.into(),
            assistant_text: format!("助手第 {number} 次回复：Mindustry 模组很好玩。"),
        },
    }
}

/// 合法只读快照；原始 Memory 的导入和送达边界另由组合测试覆盖。
pub fn memory(scope: MemoryScope, texts: &[&str]) -> MemorySnapshot {
    MemorySnapshot {
        scope,
        revision: texts.len() as u64,
        evidence: texts
            .iter()
            .enumerate()
            .map(|(index, text)| evidence(index as u64 + 1, text))
            .collect(),
        preferences: vec![],
    }
}

pub fn options() -> ObservationOptions {
    ObservationOptions {
        cooldown_ms: 0,
        ..ObservationOptions::default()
    }
}

pub fn reserve(admin: &dyn InterestAdmin, memory: &MemorySnapshot, at_ms: u64) -> ObservationBatch {
    admin
        .reserve(memory, at_ms, "observer-v1", &options())
        .unwrap()
        .expect("new evidence reserves a batch")
}

pub fn statement(kind: StatementKind, quote: &str, evidence_id: &str) -> StatementDraft {
    StatementDraft {
        kind,
        quote: quote.into(),
        evidence_id: evidence_id.into(),
    }
}

pub fn new_interest(batch: &ObservationBatch, topic: &str) -> InterestUpdateDraft {
    let id = &batch.evidence[0].id;
    InterestUpdateDraft {
        target: InterestTarget::New {
            topic: topic.into(),
        },
        statements: vec![
            statement(
                StatementKind::Interest,
                "我喜欢 Mindustry 这个游戏的模组",
                id,
            ),
            statement(StatementKind::Difficulty, "不知道怎么创作", id),
        ],
        inferred_need: Some("可能希望学习如何制作模组".into()),
    }
}
