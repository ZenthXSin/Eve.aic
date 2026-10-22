//! QQ 聊天分条的热配置：开启分段时，短的闲聊回复按句拆成几条短消息。
use eve_config_api::{ConfigField, ConfigKind, ConfigSchema, ConfigService};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
};

pub(crate) const NAMESPACE: &str = "runtime.chat_style";

pub(crate) fn schema() -> ConfigSchema {
    let mut field = ConfigField::new(ConfigKind::Boolean, Some(json!(true)));
    field.environment = Some("EVE_CHAT_SENTENCE_SPLIT".into());
    ConfigSchema {
        namespace: NAMESPACE.into(),
        version: 1,
        fields: BTreeMap::from([("sentence_split".into(), field)]),
    }
}

/// 配置服务在核心安装后才可用；之前和读取失败时按自然段分段。
#[derive(Default)]
pub(crate) struct ChatStyle {
    pub(crate) settings: OnceLock<Arc<dyn ConfigService>>,
}

impl eve_segment_plugin::ChatSplitting for ChatStyle {
    fn sentences(&self) -> bool {
        self.settings
            .get()
            .and_then(|settings| settings.snapshot(NAMESPACE, 1).ok())
            .and_then(|snapshot| snapshot.get::<bool>("sentence_split").ok())
            .unwrap_or(false)
    }
}
