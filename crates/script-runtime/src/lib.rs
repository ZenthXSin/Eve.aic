//! 不受信 Python / JavaScript / Rust 插件的显式版本执行器。
//! 仅在 OS 沙箱可用时启动进程；宿主权限始终由统一派发边界重新检查。
mod cargo_compiler;
mod cargo_model;
mod cold;
mod compiler;
mod container;
mod dispatch;
mod migration;
mod model;
mod runtime;
mod sandbox;
mod store;

pub use cargo_compiler::{CargoArtifact, CargoCompilation, CargoCompiler};
pub use cargo_model::{CargoCompilationFailure, CargoCompilerConfig, CargoProject, CargoSettings};
pub use cold::VersionSummary;
pub use container::ContainerSandbox;
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
pub(crate) fn archive_digest(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
/// 与旧JSON字节摘要兼容；通过固定缓冲写入哈希，不分配整份序列化副本。
pub(crate) fn digest(value: &impl serde::Serialize) -> Result<String> {
    use std::io::Write;
    struct HashWriter<'a>(&'a mut ring::digest::Context);
    impl Write for HashWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    {
        let mut writer = std::io::BufWriter::with_capacity(64 * 1024, HashWriter(&mut hash));
        serde_json::to_writer(&mut writer, value).map_err(fail)?;
        writer.flush().map_err(fail)?;
    }
    Ok(hash
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod digest_tests {
    #[test]
    fn streaming_digest_preserves_existing_serialized_checksums() {
        let cases = [
            serde_json::json!({"escaped":"\n\t\u{0001}","unicode":"原始状态","n":u64::MAX}),
            serde_json::json!({"large":"x".repeat(1024*1024),"bytes":(0..=255u8).collect::<Vec<_>>()}),
        ];
        for value in cases {
            let bytes = serde_json::to_vec(&value).unwrap();
            let old: String = ring::digest::digest(&ring::digest::SHA256, &bytes)
                .as_ref()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(super::digest(&value).unwrap(), old);
        }
    }
}
