//! 组合层：装配默认后端与示例插件，运行第一阶段验收链。

use eve_example_plugins::{
    CONSUMER, EventConsumerPlugin, FAILING, FORMATTER, FailingPlugin, MESSAGE, PROVIDER,
    ServiceProviderPlugin, TRANSIENT, TRANSIENT_EVENT,
};
use eve_kernel::{Kernel, KernelServices, PluginState};
use eve_plugin_api::{Event, PluginError, PluginId, PluginResult, ServiceId};

pub struct DemoReport {
    pub message: String,
    pub expected_failure: String,
}

pub async fn run_demo() -> PluginResult<DemoReport> {
    let backends = KernelServices::default();
    let state = backends.state.clone();
    let events = backends.events.clone();
    let services = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    kernel.register(Box::new(ServiceProviderPlugin::new()?))?;
    kernel.register(Box::new(EventConsumerPlugin::new()?))?;
    kernel.register(Box::new(FailingPlugin::new()?))?;
    let consumer = PluginId::new(CONSUMER)?;
    let provider = PluginId::new(PROVIDER)?;
    let failing = PluginId::new(FAILING)?;

    let result = async {
        kernel.start(&consumer).await?;
        verify(
            kernel.state(&provider) == Some(PluginState::Active),
            "提供者未自动启动",
        )?;
        let message = state
            .get(&consumer, "last_message")?
            .ok_or_else(|| PluginError::State("消费者未写入状态".into()))?;
        verify(
            message == "已接收：你好，Eve.aic".as_bytes(),
            "事件或服务处理结果不符",
        )?;
        kernel.stop_all().await?;
        verify(
            kernel.state(&consumer) == Some(PluginState::Stopped),
            "消费者未停止",
        )?;
        verify(
            kernel.state(&provider) == Some(PluginState::Stopped),
            "提供者未停止",
        )?;
        verify(
            services.get(&ServiceId::new(FORMATTER)?)?.is_none(),
            "服务未清理",
        )?;
        events.emit(Event::new(MESSAGE, "停止后事件".as_bytes())?)?;
        verify(
            state.get(&consumer, "last_message")? == Some(message.clone()),
            "停止后监听器仍写状态",
        )?;

        // 提供者已停止；此次失败请求会重新启动它，随后必须一起回滚。
        let expected_failure = match kernel.start(&failing).await {
            Err(error) => error,
            Ok(()) => return Err(PluginError::Lifecycle("验收插件应启动失败".into())),
        };
        verify(
            kernel.state(&failing) == Some(PluginState::Failed),
            "失败状态不符",
        )?;
        verify(
            kernel.state(&provider) == Some(PluginState::Stopped),
            "新启动依赖未回滚",
        )?;
        for id in [FORMATTER, TRANSIENT] {
            verify(
                services.get(&ServiceId::new(id)?)?.is_none(),
                "失败后残留服务",
            )?;
        }
        events.emit(Event::new(TRANSIENT_EVENT, Vec::new())?)?;
        verify(
            state.get(&failing, "unexpected")?.is_none(),
            "失败后残留监听器",
        )?;
        Ok(DemoReport {
            message: String::from_utf8(message)
                .map_err(|error| PluginError::State(error.to_string()))?,
            expected_failure: expected_failure.to_string(),
        })
    }
    .await;
    // 验收中途出错也停止已启动插件。
    let stopped = kernel.stop_all().await;
    let report = result?;
    stopped?;
    Ok(report)
}

fn verify(condition: bool, message: &str) -> PluginResult<()> {
    if condition {
        Ok(())
    } else {
        Err(PluginError::Lifecycle(message.into()))
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn runs_the_full_acceptance_scenario() {
        let report = super::run_demo().await.unwrap();
        assert_eq!(report.message, "已接收：你好，Eve.aic");
        assert!(report.expected_failure.contains("验收用预期启动失败"));
    }
}
