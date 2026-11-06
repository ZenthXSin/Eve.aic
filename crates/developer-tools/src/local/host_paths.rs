//! 宿主文件访问先绑定规范目标，再审核和执行；源码快照继续使用原工作区路径函数。
use super::*;
use std::collections::BTreeSet;

fn protected_name(path: &Path) -> bool {
    review::credential_path(&path.to_string_lossy())
        || path.components().any(|part| {
            matches!(part, Component::Normal(name)
            if matches!(name.to_string_lossy().to_ascii_lowercase().as_str(),
                ".git" | ".eve-developer"))
        })
        || [
            "/proc",
            "/sys",
            "/dev",
            "/etc/shadow",
            "/etc/gshadow",
            "/etc/ssh",
        ]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

fn canonical_or_missing(path: &Path, missing: bool) -> Result<PathBuf, ToolExecutionError> {
    match fs::canonicalize(path) {
        Ok(real) => Ok(real),
        Err(error) if missing && error.kind() == std::io::ErrorKind::NotFound => {
            // 悬空链接不能被解释成尚未创建的普通文件。
            if fs::symlink_metadata(path).is_ok() {
                return Err(failed("宿主路径包含无法解析的链接"));
            }
            let name = path.file_name().ok_or_else(|| failed("宿主路径无效"))?;
            let parent = path.parent().ok_or_else(|| failed("宿主路径无效"))?;
            Ok(canonical_or_missing(parent, true)?.join(name))
        }
        Err(_) => Err(failed("宿主路径不存在或不可读取")),
    }
}

/// 实现层共用的宿主文件解析；调用者必须先核验当前身份和后端能力。
/// 缺失路径只供创建/观察使用，访问拒绝与损坏链接不能伪装成 missing。
pub fn resolve_host_path(
    path: &Path,
    missing: bool,
    protected: &BTreeSet<PathBuf>,
) -> Result<PathBuf, ToolExecutionError> {
    if !path.is_absolute()
        || path
            .to_str()
            .is_none_or(|s| s.chars().any(char::is_control))
        || path.components().any(|c| matches!(c, Component::ParentDir))
        || protected_name(path)
    {
        return Err(failed(
            "宿主路径无效或属于凭据、设备、进程内存或工具管理区域",
        ));
    }
    let real = canonical_or_missing(path, missing)?;
    if protected_name(&real) {
        return Err(failed("宿主链接目标属于受保护区域"));
    }
    for entry in protected {
        let entry = canonical_or_missing(entry, true)?;
        if real.starts_with(entry) {
            return Err(failed("该宿主路径由运行器保护，不提供模型文件访问"));
        }
    }
    Ok(real)
}

pub(super) fn paths(operation: &DeveloperOperation) -> Vec<String> {
    match operation {
        DeveloperOperation::List(a)
        | DeveloperOperation::Read(a)
        | DeveloperOperation::Write(a) => vec![a.path.clone()],
        DeveloperOperation::Edit(a) => vec![a.path.clone()],
        DeveloperOperation::Search(a) => vec![a.path.clone()],
        DeveloperOperation::Command(a) => vec![a.cwd.clone()],
        DeveloperOperation::Patch(a) => eve_apply_patch::paths(&a.patch).unwrap_or_default(),
        _ => vec![],
    }
}

fn patch_path(line: &str) -> Option<&str> {
    [
        "*** Add File: ",
        "*** Delete File: ",
        "*** Update File: ",
        "*** Move to:",
    ]
    .iter()
    .find_map(|prefix| line.strip_prefix(prefix))
}

pub(super) fn only_absolute_paths(operation: &DeveloperOperation) -> bool {
    let paths = paths(operation);
    !paths.is_empty() && paths.iter().all(|p| Path::new(p).is_absolute())
}

/// 原子替换和补丁移动都保留原普通文件属性；不能保留就拒绝提交。
pub(super) fn preserve_file_metadata(
    file: &fs::File,
    path: &Path,
    source: &fs::Metadata,
) -> Result<(), ToolExecutionError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let current = file
            .metadata()
            .map_err(|_| failed("临时文件属性不可读取"))?;
        if current.uid() != source.uid() || current.gid() != source.gid() {
            std::os::unix::fs::chown(path, Some(source.uid()), Some(source.gid()))
                .map_err(|_| failed("无法保留原文件属主和属组，未提交"))?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    file.set_permissions(source.permissions())
        .map_err(|_| failed("无法保留原文件权限，未提交"))
}

impl LocalDeveloper {
    pub(super) fn protected_paths(&self) -> BTreeSet<PathBuf> {
        let mut paths = self.config.protected_paths.clone();
        for name in [
            "operations.jsonl",
            "operations.lock",
            "action-reviews.jsonl",
            "candidates",
            "leases",
            "source-snapshots",
        ] {
            paths.insert(self.journal.join(name));
        }
        if let Some(queue) = &self.config.live_upgrade {
            paths.insert(queue.clone());
        }
        if let Ok(entries) = fs::read_dir(self.journal.join("workspaces")) {
            for entry in entries.flatten() {
                paths.insert(entry.path().join("binding.json"));
            }
        }
        paths
    }

    pub(super) fn authorize_host_paths(
        &self,
        operation: &DeveloperOperation,
        context: &ToolExecutionContext,
    ) -> Result<(), ToolExecutionError> {
        if paths(operation).iter().any(|p| Path::new(p).is_absolute()) {
            if !self.config.host_filesystem || self.isolated {
                return Err(failed(
                    "当前身份未启用宿主文件访问；使用授权工程内的相对路径",
                ));
            }
            self.authorize(context)?;
        }
        Ok(())
    }

    pub(super) fn authorize_file_targets(
        &self,
        operation: &DeveloperOperation,
        context: &ToolExecutionContext,
    ) -> Result<(), ToolExecutionError> {
        if matches!(operation, DeveloperOperation::Command(_)) {
            return Ok(());
        }
        let write = matches!(
            operation,
            DeveloperOperation::Write(_)
                | DeveloperOperation::Edit(_)
                | DeveloperOperation::Patch(_)
        );
        if let (Some(authorization), Some(scope)) = (&self.authorization, context.scope()) {
            for path in paths(operation) {
                let target = if Path::new(&path).is_absolute() {
                    PathBuf::from(path)
                } else {
                    self.root.join(path)
                };
                if !authorization.permits_file(scope, &target, write) {
                    return Err(failed("当前身份不允许访问实际目标文件"));
                }
            }
        }
        Ok(())
    }

    pub(super) fn bind_model_paths(
        &self,
        mut operation: DeveloperOperation,
        context: &ToolExecutionContext,
    ) -> Result<DeveloperOperation, ToolExecutionError> {
        self.authorize_host_paths(&operation, context)?;
        let protected = self.protected_paths();
        let bind = |path: &mut String| -> Result<(), ToolExecutionError> {
            if Path::new(path).is_absolute() {
                *path = resolve_host_path(Path::new(path), true, &protected)?
                    .to_str()
                    .ok_or_else(|| failed("宿主路径不是 UTF-8"))?
                    .to_owned();
            } else if self.config.host_filesystem && !self.isolated {
                let missing = !self.root.join(&*path).exists();
                let resolved = self.path(path, missing)?;
                resolve_host_path(&resolved, true, &protected)?;
            }
            Ok(())
        };
        match &mut operation {
            DeveloperOperation::List(a)
            | DeveloperOperation::Read(a)
            | DeveloperOperation::Write(a) => bind(&mut a.path)?,
            DeveloperOperation::Edit(a) => bind(&mut a.path)?,
            DeveloperOperation::Search(a) => bind(&mut a.path)?,
            DeveloperOperation::Command(a) => bind(&mut a.cwd)?,
            DeveloperOperation::Patch(a) => {
                let mut lines = Vec::new();
                for line in a.patch.lines() {
                    if let Some(path) = patch_path(line.trim()) {
                        let mut real = path.trim().to_owned();
                        bind(&mut real)?;
                        let header = line.trim();
                        let prefix = &header[..header.len() - path.len()];
                        lines.push(format!("{} {real}", prefix.trim_end()));
                    } else {
                        lines.push(line.to_owned());
                    }
                }
                a.patch = lines.join("\n");
            }
            _ => {}
        }
        self.authorize_file_targets(&operation, context)?;
        Ok(operation)
    }

    /// 这里只接受已绑定的绝对路径；审核期间链接换目标会导致拒绝。
    pub(super) fn model_path(&self, text: &str, new: bool) -> Result<PathBuf, ToolExecutionError> {
        let path = Path::new(text);
        if !path.is_absolute() {
            return self.path(text, new);
        }
        if !self.config.host_filesystem || self.isolated {
            return Err(failed("宿主文件访问未启用"));
        }
        let real = resolve_host_path(path, new, &self.protected_paths())?;
        if real != path {
            return Err(failed("宿主目标在绑定后发生跳转，操作未执行；重新读取"));
        }
        if new && !real.parent().is_some_and(Path::is_dir) {
            return Err(failed("父目录不存在；先创建父目录"));
        }
        Ok(real)
    }

    pub(super) fn check_model_file(&self, path: &Path) -> Result<(), ToolExecutionError> {
        if self.config.host_filesystem && !self.isolated {
            resolve_host_path(path, true, &self.protected_paths())?;
        }
        Ok(())
    }

    pub(super) fn model_file_path(
        &self,
        text: &str,
        new: bool,
    ) -> Result<PathBuf, ToolExecutionError> {
        let path = self.model_path(text, new)?;
        self.check_model_file(&path)?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests;
