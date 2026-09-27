//! 组合层：装配默认后端与示例插件，运行第一阶段验收链。

use eve_example_plugins::{
    CONSUMER, EventConsumerPlugin, FAILING, FORMATTER, FailingPlugin, MESSAGE, PROVIDER,
    ServiceProviderPlugin, TASK_DEMO, TRANSIENT, TRANSIENT_EVENT, TaskDemoPlugin,
};
use eve_kernel::{Kernel, KernelServices, PluginState};
use eve_plugin_api::{Event, PluginError, PluginId, PluginResult, ServiceId};

pub struct DemoReport {
    pub message: String,
    pub expected_failure: String,
    pub task_runs: u32,
    pub custom_task_runs: u32,
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
    kernel.register(Box::new(TaskDemoPlugin::new()?))?;
    let consumer = PluginId::new(CONSUMER)?;
    let provider = PluginId::new(PROVIDER)?;
    let failing = PluginId::new(FAILING)?;
    let task_demo = PluginId::new(TASK_DEMO)?;

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
        kernel.start(&task_demo).await?;
        verify(
            state.get(&task_demo, "foreground_runs")? == Some(b"6".to_vec()),
            "前台执行次数不符",
        )?;
        verify(
            state.get(&task_demo, "custom_task_runs")? == Some(b"3".to_vec())
                && state.get(&task_demo, "custom_task_value")?
                    == Some(b"custom task executed".to_vec()),
            "自定义类型或自定义调度未实际执行",
        )?;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if kernel.tasks(&task_demo)?.iter().any(|task| task.runs > 0) {
                    break Ok::<_, PluginError>(());
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| PluginError::Task("后台任务未开始".into()))??;
        verify(
            state.get(&task_demo, "background_started")? == Some(b"yes".to_vec()),
            "后台任务未写入状态",
        )?;
        verify(
            kernel
                .tasks(&task_demo)?
                .iter()
                .any(|task| task.task_type.as_str() == "demo.write-state" && task.runs > 0),
            "后台任务未保留自定义类型标识",
        )?;
        kernel.stop_all().await?;
        verify(
            kernel.tasks(&task_demo)?.is_empty(),
            "任务停止后仍有残留记录",
        )?;
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
            task_runs: 6,
            custom_task_runs: 3,
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
        assert_eq!(report.task_runs, 6);
        assert_eq!(report.custom_task_runs, 3);
    }
}
