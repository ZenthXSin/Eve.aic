//! 提醒账本。
mod store;
mod strict_json;

use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
use eve_reminder_api::*;
use std::sync::{Arc, Mutex};
use store::StoredReminders;

/// 管理能力只交给可信宿主，不发布到通用服务目录。
#[derive(Clone, Default)]
pub struct ReminderController {
    active: Arc<Mutex<Option<Arc<StoredReminders>>>>,
}
impl ReminderController {
    fn service(&self) -> ReminderResult<Arc<StoredReminders>> {
        self.active
            .lock()
            .map_err(|_| ReminderError::Unavailable)?
            .clone()
            .ok_or(ReminderError::Unavailable)
    }
}
impl ReminderAdmin for ReminderController {
    fn snapshot(&self) -> ReminderResult<ReminderSnapshot> {
        self.service()?.snapshot()
    }
    fn create(
        &self,
        owner: &str,
        text: &str,
        due_at_ms: u64,
        source_message_id: &str,
        now_ms: u64,
    ) -> ReminderResult<Reminder> {
        self.service()?
            .create(owner, text, due_at_ms, source_message_id, now_ms)
    }
    fn cancel(&self, owner: &str, id: &str, now_ms: u64) -> ReminderResult<Reminder> {
        self.service()?.cancel(owner, id, now_ms)
    }
    fn claim(&self, id: &str, channel: DeliveryChannel, now_ms: u64) -> ReminderResult<Reminder> {
        self.service()?.claim(id, channel, now_ms)
    }
    fn record_delivery(
        &self,
        id: &str,
        result: DeliveryResult,
        now_ms: u64,
    ) -> ReminderResult<Reminder> {
        self.service()?.record_delivery(id, result, now_ms)
    }
    fn expire(&self, id: &str, now_ms: u64) -> ReminderResult<Reminder> {
        self.service()?.expire(id, now_ms)
    }
}

pub struct ReminderPlugin {
    manifest: PluginManifest,
    controller: ReminderController,
}
impl ReminderPlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(REMINDER_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: ReminderController::default(),
        })
    }
    pub fn controller(&self) -> ReminderController {
        self.controller.clone()
    }
}
impl Plugin for ReminderPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                StoredReminders::open(context.clone())
                    .map_err(|error| PluginError::State(error.to_string()))?,
            );
            let to_close = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("提醒管理句柄不可用".into()))?;
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
                .map_err(|_| PluginError::State("提醒管理句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
