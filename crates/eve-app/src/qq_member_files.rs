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
        "交付文件必须是自己项目里的普通文件，不能使用链接或读取其他成员文件".into(),
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
        io::Read,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::OpenOptionsExt,
        },
    };
    let path = Path::new(name);
    let relative = if path.is_absolute() {
        path.strip_prefix("/work")
            .map_err(|_| failed())?
            .to_path_buf()
    } else if path.components().count() == 1 {
        Path::new("exports").join(path)
    } else {
        path.to_path_buf()
    };
    let components = relative
        .components()
        .map(|part| match part {
            std::path::Component::Normal(value) => value.to_str().ok_or_else(failed),
            _ => Err(failed()),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if components.is_empty() {
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
    let parts: Vec<_> = [".eve-members", member_name]
        .into_iter()
        .chain(components)
        .collect();
    for (index, component) in parts.iter().enumerate() {
        let value = CString::new(*component).map_err(|_| failed())?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if index + 1 == parts.len() {
                0
            } else {
                libc::O_DIRECTORY
            };
        // SAFETY: fd 属于存活的 File，CString 有终止符，返回 fd 立即由 File 接管。
        let fd = unsafe { libc::openat(directory.as_raw_fd(), value.as_ptr(), flags) };
        if fd < 0 {
            return Err(failed());
        }
        // SAFETY: openat 返回的新 fd 只交给此 File 一次。
        directory = unsafe { File::from_raw_fd(fd) };
    }
    let before = directory.metadata().map_err(|_| failed())?;
    if !before.is_file() {
        return Err(failed());
    }
    let mut target = tempfile::NamedTempFile::new_in(destination).map_err(|_| failed())?;
    let copied = std::io::copy(
        &mut (&mut directory).take(before.len()),
        target.as_file_mut(),
    )
    .map_err(|_| failed())?;
    let after = directory.metadata().map_err(|_| failed())?;
    if copied != before.len()
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        return Err(failed());
    }
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

    #[test]
    fn arbitrary_project_artifacts_are_allowed_but_parent_paths_are_not() {
        let root = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let scope = ContextScope {
            session_id: "g".into(),
            user_id: "member".into(),
        };
        let member = eve_developer_api::member_workspace(root.path(), &scope);
        std::fs::create_dir_all(member.join("build/libs")).unwrap();
        std::fs::write(member.join("build/libs/报告.jar"), b"nested artifact").unwrap();
        std::fs::write(member.join("root.txt"), b"root artifact").unwrap();
        for name in ["build/libs/报告.jar", "/work/build/libs/报告.jar"] {
            let saved = snapshot(root.path(), &scope, name, target.path()).unwrap();
            assert_eq!(std::fs::read(saved).unwrap(), b"nested artifact");
        }
        let saved = snapshot(root.path(), &scope, "/work/root.txt", target.path()).unwrap();
        assert_eq!(std::fs::read(saved).unwrap(), b"root artifact");
        for name in [
            "/etc/passwd",
            "../member/root.txt",
            "/work/../root.txt",
            "build/../../root.txt",
        ] {
            assert!(
                snapshot(root.path(), &scope, name, target.path()).is_err(),
                "{name}"
            );
        }
        let other = ContextScope {
            session_id: "g".into(),
            user_id: "another".into(),
        };
        assert!(snapshot(root.path(), &other, "/work/root.txt", target.path()).is_err());
    }
}
