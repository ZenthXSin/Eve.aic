//! 取消一次等待后，继续查询并等待由宿主管理的生命周期操作。

use eve_kernel::Kernel;
use eve_plugin_api::{
    Cleanup, LifecycleOperation, LifecycleOperationState, LifecycleRequest, Plugin, PluginContext,
    PluginError, PluginFuture, PluginManifest, PluginResult, RuntimeLifecycle, cleanup,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;

struct PausingCleanupPlugin {
    manifest: PluginManifest,
    cleanup_entered: Arc<Notify>,
    release_cleanup: Arc<Notify>,
    cleaned: Arc<AtomicBool>,
}

impl Plugin for PausingCleanupPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        let entered = self.cleanup_entered.clone();
        let release = self.release_cleanup.clone();
        let cleaned = self.cleaned.clone();
        Box::pin(async move {
            ctx.cleanup(cleanup(move || async move {
                entered.notify_one();
                release.notified().await;
                cleaned.store(true, Ordering::SeqCst);
                Ok(())
            }))?;
            Ok(None)
        })
    }
}

#[tokio::main]
async fn main() -> PluginResult<()> {
    let cleanup_entered = Arc::new(Notify::new());
    let release_cleanup = Arc::new(Notify::new());
    let cleaned = Arc::new(AtomicBool::new(false));
    let plugin = PausingCleanupPlugin {
        manifest: PluginManifest::new("demo.lifecycle-operations", "0.1.0")?,
        cleanup_entered: cleanup_entered.clone(),
        release_cleanup: release_cleanup.clone(),
        cleaned: cleaned.clone(),
    };
    let id = plugin.manifest.id.clone();
    let kernel = Kernel::new();
    kernel.register(Box::new(plugin))?;
    let lifecycle: &dyn RuntimeLifecycle = &kernel;

    let start = lifecycle
        .submit(LifecycleRequest::Start(id.clone()))
        .await?;
    require_success(lifecycle.wait(start).await?)?;
    verify(lifecycle.acknowledge(start)?, "启动报告未被确认移除")?;
    println!("启动操作 {start} 已完成并确认。");

    let stop = lifecycle.submit(LifecycleRequest::Stop(id)).await?;
    if tokio::time::timeout(Duration::from_secs(5), cleanup_entered.notified())
        .await
        .is_err()
    {
        release_cleanup.notify_one();
        return Err(PluginError::Lifecycle("清理动作未在验收期限内开始".into()));
    }

    // timeout 只丢弃这一次 wait，已经准入的停止操作由宿主继续持有。
    let first_wait = tokio::time::timeout(Duration::from_millis(20), lifecycle.wait(stop)).await;
    let pending = lifecycle.operations();
    // 即使后续验收发现异常，也先放行清理动作。
    release_cleanup.notify_one();
    verify(first_wait.is_err(), "暂停的清理动作应使本次等待超时")?;
    verify(
        pending?.iter().any(|operation| {
            operation.id == stop && operation.state == LifecycleOperationState::Running
        }),
        "取消等待后停止操作未保留运行记录",
    )?;
    println!("停止操作 {stop} 的一次等待已超时；超时后记录仍为 Running。");

    let completed = tokio::time::timeout(Duration::from_secs(5), lifecycle.wait(stop))
        .await
        .map_err(|_| PluginError::Lifecycle("放行后停止操作未在验收期限内完成".into()))??;
    require_success(completed.clone())?;
    verify(
        cleaned.load(Ordering::SeqCst),
        "停止操作返回后清理动作未完成",
    )?;
    verify(
        lifecycle.operations()?.contains(&completed),
        "读取终态后报告应继续保留",
    )?;
    verify(
        lifecycle.wait(stop).await? == completed,
        "重复等待应读取同一终态",
    )?;
    println!("再次等待取得实际终态：Completed(Ok)，Cleanup 已执行，报告仍可查询。");

    verify(lifecycle.acknowledge(stop)?, "停止报告未被确认移除")?;
    verify(lifecycle.operations()?.is_empty(), "确认后仍有操作记录")?;
    kernel.flush_logs()?;
    println!("已确认并移除停止报告；操作记录数为 0，日志已刷新。");
    Ok(())
}

fn require_success(operation: LifecycleOperation) -> PluginResult<()> {
    match operation.state {
        LifecycleOperationState::Completed(result) => result,
        LifecycleOperationState::Interrupted(error) => Err(error),
        LifecycleOperationState::Running => {
            Err(PluginError::Lifecycle("等待操作返回了非终态".into()))
        }
    }
}

fn verify(condition: bool, message: &str) -> PluginResult<()> {
    if condition {
        Ok(())
    } else {
        Err(PluginError::Lifecycle(message.into()))
    }
}
