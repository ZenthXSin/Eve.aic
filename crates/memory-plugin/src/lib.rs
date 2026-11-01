//! 有界交互证据与明确偏好；宿主保留管理能力，不发布通用管理服务。
mod experience;
pub use experience::{EXPERIENCE_STATE_KEY, ExperienceMemoryController, ExperienceMemoryPlugin};
mod unified;
pub use unified::{UnifiedMemoryController, UnifiedMemoryPlugin};
mod archive;
pub use archive::{ArchivePolicy, FileEvidenceArchive};
mod context;
mod continuity;
pub use continuity::{MemoryContinuity, MemoryContinuityContext};
mod distillation;
pub use distillation::ExtractiveConversationDistiller;
mod recall;
mod recall_context;
mod store;
mod strict_json;
mod summary;
pub use summary::{
    ConversationSummaryPlugin, FileSummaryArchive, ModelConversationSummarizer, SUMMARY_STATE_KEY,
    SummaryController, effective_points, effective_points_reader, validate_summary_points,
};

use archive::ArchiveConfig;
pub use context::MemoryContext;
use eve_memory_api::*;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
pub use recall::{LexicalMemoryRecall, ScopedLexicalRecall};
pub use recall_context::MemoryRecallContext;
use std::sync::{Arc, Mutex};
use store::StoredMemory;

pub const MEMORY_STATE_KEY: &str = "memory.v1";

/// 仅供可信宿主保留；停止后的读取句柄不会随下一次启动复活。
#[derive(Clone, Default)]
pub struct MemoryController {
    active: Arc<Mutex<Option<Arc<StoredMemory>>>>,
}
impl MemoryController {
    fn service(&self) -> MemoryResult<Arc<StoredMemory>> {
        self.active
            .lock()
            .map_err(|_| MemoryError::Unavailable)?
            .clone()
            .ok_or(MemoryError::Unavailable)
    }
}
impl MemoryAdmin for MemoryController {
    fn revisions(&self, channel: &str) -> MemoryResult<Vec<(MemoryScope, u64)>> {
        self.service()?.revisions(channel)
    }
    fn scopes(&self) -> MemoryResult<Vec<MemoryScope>> {
        self.service()?.scopes()
    }
    fn reader(&self, scope: MemoryScope) -> MemoryResult<Arc<dyn MemoryService>> {
        scope.validate()?;
        let stored = self.service()?;
        stored.revision(&scope)?;
        Ok(Arc::new(ScopedReader { stored, scope }))
    }
    fn archive_status(&self) -> MemoryResult<MemoryArchiveStatus> {
        self.service()?.archive_status()
    }
    fn import_completed(
        &self,
        scope: &MemoryScope,
        expected_revision: u64,
        interaction: CompletedInteraction,
    ) -> MemoryResult<MemorySnapshot> {
        self.service()?
            .import_completed(scope, expected_revision, interaction)
    }
    fn update_preference(
        &self,
        scope: &MemoryScope,
        expected_revision: u64,
        change: PreferenceChange,
    ) -> MemoryResult<MemorySnapshot> {
        self.service()?
            .update_preference(scope, expected_revision, change)
    }
}
struct ScopedReader {
    stored: Arc<StoredMemory>,
    scope: MemoryScope,
}
impl MemoryService for ScopedReader {
    fn revision(&self) -> MemoryResult<u64> {
        self.stored.revision(&self.scope)
    }
    fn scan(&self) -> MemoryResult<Box<dyn MemoryEvidenceCursor>> {
        self.stored.scan(&self.scope)
    }
    fn snapshot(&self) -> MemoryResult<MemorySnapshot> {
        self.stored.snapshot(&self.scope)
    }
    fn archived(&self, after_revision: u64, limit: usize) -> MemoryResult<ArchivedEvidencePage> {
        self.stored.archived(&self.scope, after_revision, limit)
    }
    fn complete_snapshot(&self) -> MemoryResult<MemorySnapshot> {
        self.stored.complete_snapshot(&self.scope)
    }
}

pub struct MemoryPlugin {
    manifest: PluginManifest,
    controller: MemoryController,
    archive: Option<ArchiveConfig>,
}
impl MemoryPlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(MEMORY_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: MemoryController::default(),
            archive: None,
        })
    }
    /// 装配不可变归档存储；热层超过策略阈值时把较早的完成交互移入分块。
    /// 已经归档过的状态必须继续装配同一存储，否则拒绝启动。
    pub fn with_archive(
        mut self,
        store: Arc<dyn EvidenceArchiveStore>,
        policy: ArchivePolicy,
    ) -> PluginResult<Self> {
        policy
            .validate()
            .map_err(|_| PluginError::State("交互记忆归档策略无效".into()))?;
        self.archive = Some(ArchiveConfig { store, policy });
        Ok(self)
    }
    pub fn controller(&self) -> MemoryController {
        self.controller.clone()
    }
}
impl Plugin for MemoryPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                StoredMemory::open(context.clone(), self.archive.clone()).map_err(|error| {
                    PluginError::State(match error {
                        MemoryError::Unavailable => {
                            "交互记忆已有归档分块，但未装配归档存储；未清空".into()
                        }
                        error => error.to_string(),
                    })
                })?,
            );
            let to_close = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("交互记忆管理句柄不可用".into()))?;
                if active
                    .as_ref()
                    .is_some_and(|value| Arc::ptr_eq(value, &to_close))
                {
                    *active = None;
                }
                Ok(())
            }))?;
            *self
                .controller
                .active
                .lock()
                .map_err(|_| PluginError::State("交互记忆管理句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
