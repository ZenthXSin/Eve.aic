//! 同一目录连续启动两次，验证插件字节状态跨进程恢复。

use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
};
use std::path::PathBuf;
use std::sync::Arc;

struct CounterPlugin {
    manifest: PluginManifest,
}

impl Plugin for CounterPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let previous = ctx
                .state_get("starts")?
                .map(|bytes| {
                    let text = std::str::from_utf8(&bytes).map_err(|error| {
                        PluginError::State(format!("启动计数不是 UTF-8：{error}"))
                    })?;
                    text.parse::<u64>()
                        .map_err(|error| PluginError::State(format!("启动计数无效：{error}")))
                })
                .transpose()?;
            let next = previous
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(|| PluginError::State("启动计数已达到上限".into()))?;

            // 插件只依赖状态契约，不访问文件路径或具体存储实现。
            ctx.state_set("starts", next.to_string().into_bytes())?;
            match previous {
                Some(value) => println!("恢复前值：{value}；写入后值：{next}。"),
                None => println!("恢复前值：无；写入后值：{next}。"),
            }
            Ok(None)
        })
    }
}

#[tokio::main]
async fn main() -> PluginResult<()> {
    let mut arguments = std::env::args_os().skip(1);
    let directory = arguments.next().map(PathBuf::from).ok_or_else(|| {
        PluginError::State(
            "请指定状态目录：cargo run -p eve-runtime --example state_recovery -- <目录>".into(),
        )
    })?;
    if arguments.next().is_some() {
        return Err(PluginError::State("示例只接受一个状态目录参数".into()));
    }

    let store = FileStateStore::open(&directory)?;
    let kernel = Kernel::with_services(KernelServices {
        state: Arc::new(store),
        ..KernelServices::default()
    });
    let plugin = CounterPlugin {
        manifest: PluginManifest::new("demo.state-recovery", "0.1.0")?,
    };
    let id = plugin.manifest.id.clone();
    kernel.register(Box::new(plugin))?;

    let started = kernel.start(&id).await;
    // 启动出错时也完成停止和日志刷新，再返回原始错误。
    let stopped = kernel.stop_all().await;
    let flushed = kernel.flush_logs();
    started?;
    stopped?;
    flushed?;
    println!(
        "状态恢复验收通过。再次使用同一目录启动将读取本次写入值：{}",
        directory.display()
    );
    Ok(())
}
