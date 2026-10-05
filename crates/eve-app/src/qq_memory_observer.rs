//! 组合 QQ 已送达凭据与公开 Session/Memory 服务，不补采历史回执。
use crate::qq_memory;
use eve_memory_api::{CompletedInteraction, MemoryAdmin};
use eve_plugin_api::{PluginError, PluginResult};
use eve_qqbot_plugin::{QqInteraction, QqInteractionObserver};
use eve_session_api::SessionService;
use ring::digest::{Context, SHA256};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) struct Observer {
    pub memory: Arc<dyn MemoryAdmin>,
    pub sessions: Arc<dyn SessionService>,
}

fn failure() -> PluginError {
    PluginError::State("交互记忆保存无法确认；QQ Sent 已保留，不自动重试。".into())
}

impl QqInteractionObserver for Observer {
    fn observe(&self, interaction: &QqInteraction<'_>) -> PluginResult<()> {
        let scope = qq_memory::scope(interaction.session());
        let before = self
            .memory
            .reader(scope.clone())
            .and_then(|reader| reader.snapshot())
            .map_err(|_| failure())?;
        if before.scope != scope {
            return Err(failure());
        }
        let snapshot = self
            .sessions
            .snapshot(interaction.session())
            .map_err(|_| failure())?
            .ok_or_else(failure)?;
        let mut hash = Context::new(&SHA256);
        for part in [
            scope.channel.as_bytes(),
            scope.session_id.as_bytes(),
            scope.user_id.as_bytes(),
            interaction.message_id().as_bytes(),
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
        // QQ 契约没有平台时间；这里只记录宿主观察到已送达交互的时间。
        let at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|value| u64::try_from(value.as_millis()).ok())
            .ok_or_else(failure)?;
        let saved = self
            .memory
            .import_completed(
                &scope,
                before.revision,
                CompletedInteraction {
                    evidence_id: format!("qq-interaction-{id}"),
                    message_id: interaction.message_id().into(),
                    at_ms,
                    snapshot,
                    turn_id: interaction.turn_id(),
                },
            )
            .map_err(|_| failure())?;
        if saved.scope != scope {
            return Err(failure());
        }
        Ok(())
    }
}
