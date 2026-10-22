//! QQ 连发合并的热配置：同一会话里同一人连发的纯文字，在窗口内到达的合成一轮回复。
use eve_config_api::{ConfigField, ConfigKind, ConfigSchema, ConfigService};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
    time::Duration,
};

pub(crate) const NAMESPACE: &str = "runtime.chat_timing";

pub(crate) fn schema() -> ConfigSchema {
    let mut field = ConfigField::new(
        ConfigKind::Integer {
            minimum: Some(0),
            maximum: Some(10_000),
        },
        Some(json!(1500)),
    );
    field.environment = Some("EVE_DEBOUNCE_MS".into());
    ConfigSchema {
        namespace: NAMESPACE.into(),
        version: 1,
        fields: BTreeMap::from([("debounce_ms".into(), field)]),
    }
}

/// 配置服务在核心安装后才可用；之前和读取失败时不等待、不合并。
#[derive(Default)]
pub(crate) struct ChatTiming {
    pub(crate) settings: OnceLock<Arc<dyn ConfigService>>,
}

impl eve_qqbot_plugin::QqDebounce for ChatTiming {
    fn window(&self) -> Option<Duration> {
        let snapshot = self.settings.get()?.snapshot(NAMESPACE, 1).ok()?;
        let millis: u64 = snapshot.get("debounce_ms").ok()?;
        (millis > 0).then(|| Duration::from_millis(millis))
    }
}
