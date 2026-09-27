use eve_plugin_api::PluginResult;

#[tokio::main]
async fn main() -> PluginResult<()> {
    let report = eve_runtime::run_demo().await?;
    println!("插件协作验收通过：{}", report.message);
    println!("消费者与提供者已停止，监听器和服务已清理。");
    println!(
        "任务验收通过：前台立即、延迟、指定时间和重复任务共完成 {} 次；后台周期任务已停止。",
        report.task_runs
    );
    println!("预期失败已回滚：{}", report.expected_failure);
    Ok(())
}
