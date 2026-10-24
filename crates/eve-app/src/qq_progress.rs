//! QQ 慢步骤进度的热配置：本轮某一步超过阈值仍未结束时，通道补发一条真实进度。
use eve_config_api::{ConfigField, ConfigKind, ConfigSchema, ConfigService};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
    time::Duration,
};

pub(crate) const NAMESPACE: &str = "runtime.progress";

pub(crate) fn schema() -> ConfigSchema {
    let mut field = ConfigField::new(
        ConfigKind::Integer {
            minimum: Some(0),
            maximum: Some(600_000),
        },
        Some(json!(60_000)),
    );
    field.environment = Some("EVE_PROGRESS_SLOW_AFTER_MS".into());
    let mut milestones = ConfigField::new(ConfigKind::Boolean, Some(json!(false)));
    milestones.environment = Some("EVE_PROGRESS_TOOL_MILESTONES".into());
    ConfigSchema {
        namespace: NAMESPACE.into(),
        version: 1,
        fields: BTreeMap::from([
            ("slow_after_ms".into(), field),
            ("tool_milestones".into(), milestones),
        ]),
    }
}

/// 配置服务在核心安装后才可用；之前和读取失败时都不发慢步骤进度。
#[derive(Default)]
pub(crate) struct SlowProgress {
    pub(crate) settings: OnceLock<Arc<dyn ConfigService>>,
}

impl eve_qqbot_plugin::QqSlowProgress for SlowProgress {
    fn tool_milestones(&self) -> bool {
        self.settings
            .get()
            .and_then(|settings| settings.snapshot(NAMESPACE, 1).ok())
            .and_then(|snapshot| snapshot.get("tool_milestones").ok())
            .unwrap_or(false)
    }

    fn after(&self) -> Option<Duration> {
        let snapshot = self.settings.get()?.snapshot(NAMESPACE, 1).ok()?;
        let millis: u64 = snapshot.get("slow_after_ms").ok()?;
        (millis > 0).then(|| Duration::from_millis(millis.max(1000)))
    }
}
