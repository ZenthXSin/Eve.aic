//! cargo run -p eve-runtime --example configuration -- [独立配置目录]
use eve_config_api::*;
use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_example_plugins::{CONFIG_CONSUMER, CONFIG_REPORT, ConfigConsumerPlugin};
use eve_kernel::{Kernel, KernelServices};
use eve_plugin_api::{PluginId, ServiceId};
use std::{collections::BTreeMap, path::Path};

fn overrides(value: usize) -> ConfigOverrides {
    BTreeMap::from([(
        LLM_NAMESPACE.into(),
        NamespaceValues {
            schema_version: 1,
            values: BTreeMap::from([(MAX_PARALLEL_TOOL_CALLS.into(), serde_json::json!(value))]),
        },
    )])
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let temporary = tempfile::tempdir()?;
    let directory = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| temporary.path().to_path_buf());
    let backends = KernelServices::default();
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    let plugin = ConfigPlugin::new(ConfigBootstrap::new(&directory, vec![runtime_llm_schema()]))?;
    let admin = plugin.controller();
    kernel.register(Box::new(plugin))?;
    kernel.register(Box::new(ConfigConsumerPlugin::new()?))?;
    let result = async {
        kernel.start(&PluginId::new(CONFIG_CONSUMER)?).await?;
        let entry = registry.get(&ServiceId::new(CONFIG_SERVICE_ID)?)?.ok_or("配置服务缺失")?;
        let config = entry.value.downcast::<ConfigServiceHandle>().map_err(|_| "服务类型错误")?;
        let report = registry.get(&ServiceId::new(CONFIG_REPORT)?)?.ok_or("消费者报告缺失")?;
        let settings = report.value.downcast::<LlmRuntimeConfig>().map_err(|_| "消费者报告类型错误")?;
        let start = settings.max_parallel_tool_calls;
        let next = start.checked_add(1).ok_or("示例并发值溢出")?;
        let immediate_target = start.checked_add(2).ok_or("示例并发值溢出")?;
        let revision = admin.current()?.revision;
        let first = config.0.begin_request(LLM_NAMESPACE, 1)?;
        admin.replace(revision, overrides(next), ApplyMode::NewRequests)?;
        let pinned = LlmRuntimeConfig::try_from(&config.0.read_request(&first)?)?.max_parallel_tool_calls;
        let new = LlmRuntimeConfig::try_from(&config.0.snapshot(LLM_NAMESPACE, 1)?)?.max_parallel_tool_calls;
        assert_eq!(pinned, start);
        assert_eq!(new, next);
        admin.replace(revision + 1, overrides(immediate_target), ApplyMode::Immediate)?;
        let immediate = LlmRuntimeConfig::try_from(&config.0.read_request(&first)?)?.max_parallel_tool_calls;
        assert_eq!(immediate, immediate_target);
        kernel.stop_all().await?;
        assert_eq!(config.0.snapshot(LLM_NAMESPACE, 1), Err(ConfigError::Unavailable));
        kernel.start(&PluginId::new(CONFIG_CONSUMER)?).await?;
        let entry = registry.get(&ServiceId::new(CONFIG_REPORT)?)?.ok_or("重启后消费者报告缺失")?;
        let restored = entry.value.downcast::<LlmRuntimeConfig>().map_err(|_| "消费者报告类型错误")?.max_parallel_tool_calls;
        assert_eq!(restored, immediate);
        assert_eq!(admin.current()?.revision, revision + 2);
        assert!(Path::new(&directory).join("config.json").is_file());
        println!("启动并发：{start}；仅新请求：在途 {pinned} / 新请求 {new}；立即替换：{immediate}；重启恢复：{restored}。");
        Ok::<_, Box<dyn std::error::Error>>(())
    }.await;
    let stopped = kernel.stop_all().await;
    let flushed = kernel.flush_logs();
    result?;
    stopped?;
    flushed?;
    Ok(())
}
