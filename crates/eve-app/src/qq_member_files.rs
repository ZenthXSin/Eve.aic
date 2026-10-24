//! 成员交付物先通过目录句柄逐级打开并快照，再交给原发送账本；拒绝链接和跨目录读取。
use eve_llm_api::{ContextScope, ToolExecutionError};
use std::path::{Path, PathBuf};

pub(crate) fn snapshot(
    root: &Path,
    scope: &ContextScope,
    name: &str,
    destination: &Path,
) -> Result<PathBuf, ToolExecutionError> {
    snapshot_inner(root, scope, name, destination)
}
fn failed() -> ToolExecutionError {
    ToolExecutionError::Failed(
        "交付文件必须是自己项目 exports/ 下的普通文件，不能使用链接或读取其他成员文件".into(),
    )
}
#[cfg(target_os = "linux")]
fn snapshot_inner(
    root: &Path,
    scope: &ContextScope,
    name: &str,
    destination: &Path,
) -> Result<PathBuf, ToolExecutionError> {
    use std::{
        ffi::CString,
        fs::{File, OpenOptions},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::OpenOptionsExt,
        },
    };
    if !eve_qqbot_plugin::valid_name(name) {
        return Err(failed());
    }
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)
        .map_err(|_| failed())?;
    let member = eve_developer_api::member_workspace(root, scope);
    let member_name = member
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(failed)?;
    for (index, component) in [".eve-members", member_name, "exports", name]
        .into_iter()
        .enumerate()
    {
        let value = CString::new(component).map_err(|_| failed())?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if index == 3 { 0 } else { libc::O_DIRECTORY };
        // SAFETY: fd 属于存活的 File，CString 有终止符，返回 fd 立即由 File 接管。
        let fd = unsafe { libc::openat(directory.as_raw_fd(), value.as_ptr(), flags) };
        if fd < 0 {
            return Err(failed());
        }
        // SAFETY: openat 返回的新 fd 只交给此 File 一次。
        directory = unsafe { File::from_raw_fd(fd) };
    }
    if !directory.metadata().is_ok_and(|m| m.is_file()) {
        return Err(failed());
    }
    let mut target = tempfile::NamedTempFile::new_in(destination).map_err(|_| failed())?;
    std::io::copy(&mut directory, target.as_file_mut()).map_err(|_| failed())?;
    target.as_file().sync_all().map_err(|_| failed())?;
    target.keep().map(|(_, path)| path).map_err(|_| failed())
}
#[cfg(not(target_os = "linux"))]
fn snapshot_inner(
    _: &Path,
    _: &ContextScope,
    _: &str,
    _: &Path,
) -> Result<PathBuf, ToolExecutionError> {
    Err(failed())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    #[test]
    fn member_export_is_snapshotted_and_leaf_or_parent_links_cannot_escape() {
        let root = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let scope = ContextScope {
            session_id: "g".into(),
            user_id: "member".into(),
        };
        let exports = eve_developer_api::member_workspace(root.path(), &scope).join("exports");
        std::fs::create_dir_all(&exports).unwrap();
        std::fs::write(exports.join("mod.jar"), b"verified artifact").unwrap();
        let saved = snapshot(root.path(), &scope, "mod.jar", target.path()).unwrap();
        std::fs::write(exports.join("mod.jar"), b"changed later").unwrap();
        assert_eq!(std::fs::read(saved).unwrap(), b"verified artifact");
        std::os::unix::fs::symlink("/etc/passwd", exports.join("linked.jar")).unwrap();
        assert!(snapshot(root.path(), &scope, "linked.jar", target.path()).is_err());
        let moved = exports.with_file_name("original");
        std::fs::rename(&exports, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &exports).unwrap();
        assert!(snapshot(root.path(), &scope, "mod.jar", target.path()).is_err());
    }
}
