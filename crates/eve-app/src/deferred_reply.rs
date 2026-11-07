//! 同轮延后回复的装配设置；启动时捕获，不把保存设置伪装成在途任务热更新。
use eve_config_api::{ConfigField, ConfigKind, ConfigSchema, ConfigService, ConfigSnapshot};
use eve_llm_api::DeferredReplyPolicy;
use serde_json::json;
use std::{collections::BTreeMap, time::Duration};

pub(crate) const NAMESPACE: &str = "runtime.deferred_reply";

pub(crate) fn schema() -> ConfigSchema {
    let field = |kind, default, env: &str| {
        let mut field = ConfigField::new(kind, Some(default));
        field.environment = Some(env.into());
        field.restart_required = true;
        field
    };
    let integer = |default, min, max, env| {
        field(
            ConfigKind::Integer {
                minimum: Some(min),
                maximum: Some(max),
            },
            json!(default),
            env,
        )
    };
    ConfigSchema {
        namespace: NAMESPACE.into(),
        version: 1,
        fields: BTreeMap::from([
            (
                "enabled".into(),
                field(
                    ConfigKind::Boolean,
                    json!(false),
                    "EVE_DEFERRED_REPLY_ENABLED",
                ),
            ),
            (
                "max_retries".into(),
                integer(0, 0, 8, "EVE_DEFERRED_REPLY_MAX_RETRIES"),
            ),
            (
                "initial_delay_ms".into(),
                integer(5000, 1, 60_000, "EVE_DEFERRED_REPLY_INITIAL_DELAY_MS"),
            ),
            (
                "max_delay_ms".into(),
                integer(30_000, 1, 120_000, "EVE_DEFERRED_REPLY_MAX_DELAY_MS"),
            ),
            (
                "recovery_window_ms".into(),
                integer(600_000, 1, 900_000, "EVE_DEFERRED_REPLY_WINDOW_MS"),
            ),
        ]),
    }
}

pub(crate) fn read(
    service: &dyn ConfigService,
) -> Result<Option<DeferredReplyPolicy>, crate::AppError> {
    let snapshot = service.snapshot(NAMESPACE, 1)?;
    Ok(snapshot
        .get::<bool>("enabled")?
        .then_some(settings(&snapshot)?))
}

pub(crate) fn settings(snapshot: &ConfigSnapshot) -> Result<DeferredReplyPolicy, crate::AppError> {
    let policy = DeferredReplyPolicy {
        max_retries: snapshot.get("max_retries")?,
        initial_delay: Duration::from_millis(snapshot.get("initial_delay_ms")?),
        max_delay: Duration::from_millis(snapshot.get("max_delay_ms")?),
        recovery_window: Duration::from_millis(snapshot.get("recovery_window_ms")?),
    };
    policy.validate()?;
    Ok(policy)
}
