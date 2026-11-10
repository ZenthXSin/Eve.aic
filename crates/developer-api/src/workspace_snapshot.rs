//! 可信宿主采集的实际输入视图；不提供模型 JSON 编解码，不宣称全树原子时点。
use eve_llm_api::{ContextScope, ToolExecutionError};
use std::{fmt, future::Future, path::PathBuf, pin::Pin};

pub const WORKSPACE_SNAPSHOT_MAX_ENTRIES: usize = 4096;
pub const WORKSPACE_SNAPSHOT_MAX_BYTES: u64 = 64 * 1024 * 1024;
pub const WORKSPACE_SNAPSHOT_MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub const WORKSPACE_SNAPSHOT_MAX_DEPTH: usize = 32;

pub type WorkspaceSnapshotFuture<'a> =
    Pin<Box<dyn Future<Output = Result<WorkspaceSnapshot, ToolExecutionError>> + Send + 'a>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSnapshotRequest {
    /// 当前身份实际工作区内目录，或该身份已获准的宿主绝对目录。
    pub root: String,
}

#[derive(Clone, Eq, PartialEq)]
pub enum WorkspaceSnapshotContents {
    Directory,
    File(Vec<u8>),
}
impl fmt::Debug for WorkspaceSnapshotContents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Directory => f.write_str("Directory"),
            Self::File(bytes) => f.debug_struct("File").field("bytes", &bytes.len()).finish(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSnapshotEntry {
    /// 排序后的 UTF-8 相对路径；根目录为 "."。拒绝链接、特殊对象和不明确路径。
    pub path: String,
    /// Unix 执行位，范围 0..=0o111；文件和目录均保留。
    pub executable_bits: u32,
    pub contents: WorkspaceSnapshotContents,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceSnapshotExclusionReason {
    ProtectedMetadata,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSnapshotExclusion {
    pub path: String,
    pub reason: WorkspaceSnapshotExclusionReason,
}

/// 快照仅证明 entries 构成的输入视图；excluded 中的管理元数据不供运行器读取。
/// 采集会检查可观察变化，但多文件不具有统一原子时点；以后文件变化不改变本快照。
/// 文件字节只能交给受信运行器；Debug 不输出正文。不能把公开字段当作来源认证。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSnapshot {
    pub scope: ContextScope,
    pub workspace_root: PathBuf,
    pub root: PathBuf,
    pub entries: Vec<WorkspaceSnapshotEntry>,
    pub excluded: Vec<WorkspaceSnapshotExclusion>,
    pub total_bytes: u64,
    /// v1 域分离摘要：按名称排序的深度优先遍历，涵盖路径、类型、执行位、
    /// 文件长度/完整字节及排除路径/原因；不以返回 entries 的最终排序重算。
    /// 不包含宿主路径、身份、时间或 inode；相同输入视图可由不同目录获得相同摘要。
    pub sha256: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeveloperBackend, DeveloperOperation};
    use eve_llm_api::{ToolExecutionContext, ToolFuture};
    use std::task::{Context, Poll, Waker};

    struct Unsupported;
    impl DeveloperBackend for Unsupported {
        fn execute(&self, _: DeveloperOperation, _: ToolExecutionContext) -> ToolFuture<'_> {
            panic!("snapshot must not fall back to executing a tool");
        }
    }
    #[test]
    fn unsupported_backend_explicitly_rejects_without_fallback() {
        let backend = Unsupported;
        let mut future = backend.capture_workspace_snapshot(
            WorkspaceSnapshotRequest { root: ".".into() },
            ToolExecutionContext::new(),
        );
        assert!(matches!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Err(ToolExecutionError::Rejected(_)))
        ));
    }
    #[test]
    fn debug_does_not_expose_file_bytes() {
        let contents = WorkspaceSnapshotContents::File(b"private file content".to_vec());
        assert!(!format!("{contents:?}").contains("private file content"));
    }
}
