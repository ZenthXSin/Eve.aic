//! 不受信 Python / JavaScript 插件的显式版本执行器。
//! 仅在 OS 沙箱可用时启动进程；宿主权限始终由统一派发边界重新检查。
mod dispatch;
mod model;
mod runtime;
mod sandbox;
mod store;

pub use dispatch::{AuthorizedToolDispatch, CheckedTools, ToolAuthorizer};
pub use model::*;
pub use runtime::{BoundVersion, ScriptRuntime};
pub use sandbox::Bubblewrap;
pub use store::ScriptStore;

pub(crate) fn reject(message: impl ToString) -> eve_llm_api::ToolExecutionError {
    eve_llm_api::ToolExecutionError::Rejected(message.to_string())
}
pub(crate) fn fail(message: impl ToString) -> eve_llm_api::ToolExecutionError {
    eve_llm_api::ToolExecutionError::Failed(message.to_string())
}
pub(crate) fn digest(value: &impl serde::Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(value).map_err(fail)?;
    Ok(ring::digest::digest(&ring::digest::SHA256, &bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
