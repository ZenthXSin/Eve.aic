//! 四类模型配置：真实文件更新、固定快照与独立进程恢复；不访问模型或解析密钥。
use eve_config_api::*;
use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_kernel::{Kernel, KernelServices};
use eve_plugin_api::{PluginId, ServiceId};
use serde_json::json;
use std::{collections::BTreeMap, error::Error};

fn initial_values() -> ConfigOverrides {
    let mut values = BTreeMap::new();
    for role in ModelRole::ALL {
        values.insert(role.field("enabled"), json!(true));
        values.insert(role.field("provider"), json!("demo"));
        values.insert(role.field("model"), json!(format!("{}-1", role.key())));
    }
    values.insert("semantic_dimensions".into(), json!(768));
    BTreeMap::from([(
        MODELS_NAMESPACE.into(),
        NamespaceValues {
            schema_version: MODELS_SCHEMA_VERSION,
            values,
        },
    )])
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let temporary = tempfile::tempdir()?;
    let mut args = std::env::args_os().skip(1);
    let directory = args
        .next()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| temporary.path().to_path_buf());
    if args.next().is_some() {
        return Err("仅支持 [独立配置目录]".into());
    }
    let backends = KernelServices::default();
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    let plugin = ConfigPlugin::new(
        ConfigBootstrap::new(&directory, vec![model_roles_schema()])
            .with_environment(BTreeMap::new()),
    )?;
    let admin = plugin.controller();
    kernel.register(Box::new(plugin))?;
    let result = async {
        kernel.start(&PluginId::new(CONFIG_PLUGIN_ID)?).await?;
        let service = registry
            .get(&ServiceId::new(CONFIG_SERVICE_ID)?)?
            .ok_or("配置服务缺失")?
            .value
            .downcast::<ConfigServiceHandle>()
            .map_err(|_| "配置服务类型错误")?;
        let restored_revision = admin.current()?.revision;
        if restored_revision == 0 {
            admin.replace(0, initial_values(), ApplyMode::NewRequests)?;
        }
        let captured = ModelRolesConfig::capture(service.0.as_ref())?;
        for role in ModelRole::ALL {
            captured.require(role)?;
        }
        let mut next = admin.current()?.namespaces;
        let next_model = format!("primary-{}", captured.revision() + 1);
        next.get_mut(MODELS_NAMESPACE)
            .ok_or("模型配置缺失")?
            .values
            .insert("primary_model".into(), json!(next_model));
        admin.replace(captured.revision(), next, ApplyMode::Immediate)?;
        let current = ModelRolesConfig::capture(service.0.as_ref())?;
        let old_primary = captured.require(ModelRole::Primary)?;
        let new_primary = current.require(ModelRole::Primary)?;
        if old_primary.model == new_primary.model
            || captured.revision() + 1 != current.revision()
            || captured.require(ModelRole::Semantic)? != current.require(ModelRole::Semantic)?
        {
            return Err("模型角色或快照隔离验收失败".into());
        }
        Ok::<_, Box<dyn Error>>(json!({
            "restored_revision": restored_revision,
            "captured_revision": captured.revision(),
            "current_revision": current.revision(),
            "captured_primary": old_primary.model,
            "current_primary": new_primary.model,
            "enabled_roles": ModelRole::ALL.len(),
            "semantic_dimensions": 768,
            "model_requests": 0
        }))
    }
    .await;
    let stopped = kernel.stop_all().await;
    let output = result?;
    stopped?;
    println!("{output}");
    Ok(())
}
