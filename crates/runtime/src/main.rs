use eve_plugin_api::PluginResult;

#[tokio::main]
async fn main() -> PluginResult<()> {
    let report = eve_runtime::run_demo().await?;
    println!("插件协作验收通过：{}", report.message);
    println!("消费者与提供者已停止，监听器和服务已清理。");
    println!("预期失败已回滚：{}", report.expected_failure);
    Ok(())
}
