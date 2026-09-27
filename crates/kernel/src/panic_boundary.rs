use eve_plugin_api::{PluginError, PluginResult};
use std::future::{Future, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::task::Poll;

/// 在每次轮询处隔离插件 unwind，不持有内核的同步锁。
/// 调用方须把插件函数调用也放入 async 块，使 Future 工厂 panic 同样被捕获。
/// 插件内部状态可能已部分修改；捕获后必须进入原有失败收尾路径。
pub(crate) async fn contain_panic<T>(
    stage: &'static str,
    future: impl Future<Output = PluginResult<T>>,
) -> PluginResult<T> {
    let mut future = Box::pin(future);
    poll_fn(
        |context| match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(context))) {
            Ok(result) => result,
            Err(payload) => {
                let message = if let Some(message) = payload.downcast_ref::<String>() {
                    message.as_str()
                } else if let Some(message) = payload.downcast_ref::<&str>() {
                    message
                } else {
                    "非字符串 panic 载荷"
                };
                Poll::Ready(Err(PluginError::Lifecycle(format!(
                    "{stage}发生 panic：{message}"
                ))))
            }
        },
    )
    .await
}
