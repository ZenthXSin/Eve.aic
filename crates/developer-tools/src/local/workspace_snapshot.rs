//! 受信目录输入采集，不执行 Git、命令、模型或任务，也不创建工作区。
use super::*;

impl LocalDeveloper {
    pub(super) fn workspace_snapshot(
        &self,
        request: WorkspaceSnapshotRequest,
        context: &ToolExecutionContext,
    ) -> Result<WorkspaceSnapshot, ToolExecutionError> {
        #[cfg(target_os = "linux")]
        {
            linux::capture(self, request, context)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (request, context);
            Err(rejected("可信目录快照需要 Linux 文件描述符锚定"))
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::{
        collections::{BTreeMap, BTreeSet},
        ffi::CString,
        fs::{File, Metadata},
        io::Read,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::{MetadataExt, OpenOptionsExt},
        },
    };

    fn error() -> ToolExecutionError {
        rejected("workspace_snapshot_changed_or_unreadable：目录输入变化或无法完整读取")
    }
    fn limit() -> ToolExecutionError {
        rejected("workspace_snapshot_limit：目录输入超过固定容量，未返回截断快照")
    }
    fn operation(path: String) -> DeveloperOperation {
        DeveloperOperation::Read(FileArguments {
            path,
            offset: 0,
            content: None,
            expected_sha256: None,
        })
    }
    fn open_at(parent: &File, name: &str, flags: i32) -> Result<File, ToolExecutionError> {
        let name = CString::new(name).map_err(|_| error())?;
        // SAFETY: parent 是活的目录 fd，CString 以 NUL 结尾；成功 fd 仅交给一个 File。
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(error());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    fn open_absolute(path: &Path) -> Result<File, ToolExecutionError> {
        if !path.is_absolute() {
            return Err(error());
        }
        let mut file = File::open("/").map_err(|_| error())?;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    file = open_at(
                        &file,
                        name.to_str().ok_or_else(error)?,
                        libc::O_PATH | libc::O_DIRECTORY,
                    )?;
                }
                _ => return Err(error()),
            }
        }
        Ok(file)
    }
    #[derive(Eq, PartialEq)]
    struct Stamp {
        dev: u64,
        ino: u64,
        mode: u32,
        len: u64,
        mtime: (i64, i64),
        ctime: (i64, i64),
    }
    fn stamp(meta: &Metadata) -> Stamp {
        Stamp {
            dev: meta.dev(),
            ino: meta.ino(),
            mode: meta.mode(),
            len: meta.len(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }
    fn metadata(file: &File) -> Result<Metadata, ToolExecutionError> {
        file.metadata().map_err(|_| error())
    }
    fn readable(observed: &File) -> Result<File, ToolExecutionError> {
        // 只从已 fstat 确认为普通文件的 O_PATH fd 取得读取句柄。
        // 不再次按业务名称打开，避免名称在核验后被替换成设备或管道。
        if !metadata(observed)?.is_file() {
            return Err(error());
        }
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(format!("/proc/self/fd/{}", observed.as_raw_fd()))
            .map_err(|_| error())
    }
    fn names(dir: &File) -> Result<Vec<String>, ToolExecutionError> {
        // proc 链接仅指向本实现持有的目录 fd；业务路径组件从不跟随链接。
        let mut names = Vec::new();
        for entry in
            fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd())).map_err(|_| error())?
        {
            if names.len() >= WORKSPACE_SNAPSHOT_MAX_ENTRIES {
                return Err(limit());
            }
            let name = entry
                .map_err(|_| error())?
                .file_name()
                .into_string()
                .map_err(|_| error())?;
            if name.is_empty() || name.contains(['\\', ':']) || name.chars().any(char::is_control) {
                return Err(rejected(
                    "workspace_snapshot_invalid_path：输入路径不是受支持的完整 UTF-8 路径",
                ));
            }
            names.push(name);
        }
        names.sort();
        Ok(names)
    }
    fn check(
        backend: &LocalDeveloper,
        context: &ToolExecutionContext,
        path: &Path,
        protected: &BTreeSet<PathBuf>,
    ) -> Result<(), ToolExecutionError> {
        if stopped(context) {
            return Err(ToolExecutionError::Cancelled(
                "工作区快照已取消或到期".into(),
            ));
        }
        let relative = path.strip_prefix(&backend.root).ok();
        let value = relative.unwrap_or(path).to_str().ok_or_else(error)?;
        let op = operation(if value.is_empty() {
            ".".into()
        } else {
            value.into()
        });
        backend.authorize_selected_backend(&op, context)?;
        let real = host_paths::resolve_host_path(path, false, protected)?;
        if real != path {
            return Err(error());
        }
        Ok(())
    }

    struct Selection {
        backend: LocalDeveloper,
        /// 私有会话绑定在两次采集前后都复核；不得调用会建立或同步目录的方法。
        binding: Option<(PathBuf, Vec<u8>)>,
    }
    fn read_binding(path: &Path) -> Result<Vec<u8>, ToolExecutionError> {
        let parent = open_absolute(path.parent().ok_or_else(error)?)?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(error)?;
        let observed = open_at(&parent, name, libc::O_PATH)?;
        let expected = metadata(&observed)?;
        if !expected.is_file() || expected.nlink() != 1 || expected.len() > 8192 {
            return Err(error());
        }
        let mut file = readable(&observed)?;
        let before = metadata(&file)?;
        if stamp(&before) != stamp(&expected) {
            return Err(error());
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(8193)
            .read_to_end(&mut bytes)
            .map_err(|_| error())?;
        if bytes.len() as u64 != before.len()
            || stamp(&before) != stamp(&metadata(&file)?)
            || stamp(&before) != stamp(&metadata(&open_at(&parent, name, libc::O_PATH)?)?)
        {
            return Err(error());
        }
        Ok(bytes)
    }
    fn select(
        original: &LocalDeveloper,
        request: &WorkspaceSnapshotRequest,
        context: &ToolExecutionContext,
    ) -> Result<Selection, ToolExecutionError> {
        let scope = context
            .scope()
            .filter(|scope| !scope.session_id.trim().is_empty() && !scope.user_id.trim().is_empty())
            .ok_or_else(|| rejected("工作区快照缺少可信完整身份"))?;
        let op = operation(request.root.clone());
        original.authorize_operation(&op, context)?;
        if original.member_operation(&op, context) {
            let root = member_workspace(&original.root, scope);
            open_absolute(&root)?;
            let mut backend = original.at_root(root);
            backend.isolated = true;
            // 此临时后端仅扫描，不执行命令/HTTP；保留令牌用于拒绝泄露的原始字节。
            return Ok(Selection {
                backend,
                binding: None,
            });
        }
        if original.session_workspaces && !Path::new(&request.root).is_absolute() {
            original.authorize(context)?;
            let directory = original
                .journal
                .join("workspaces")
                .join(session_workspace_key(scope));
            let path = directory.join("binding.json");
            let bytes = read_binding(&path)?;
            if serde_json::from_slice::<Value>(&bytes).ok()
                != Some(json!({"version":1,"scope":scope,"source":original.root}))
            {
                return Err(rejected("工作区快照身份绑定不匹配，未创建或同步目录"));
            }
            let root = directory.join("source");
            open_absolute(&root)?;
            return Ok(Selection {
                backend: original.at_root(root),
                binding: Some((path, bytes)),
            });
        }
        Ok(Selection {
            backend: original.at_root(original.root.clone()),
            binding: None,
        })
    }

    struct Scan {
        entries: Vec<WorkspaceSnapshotEntry>,
        excluded: Vec<WorkspaceSnapshotExclusion>,
        stamps: BTreeMap<String, Stamp>,
        total: u64,
        hash: ring::digest::Context,
        retain: bool,
    }
    impl Scan {
        fn new(retain: bool) -> Self {
            let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
            hash.update(b"eve.workspace-snapshot.v1\0");
            Self {
                entries: Vec::new(),
                excluded: Vec::new(),
                stamps: BTreeMap::new(),
                total: 0,
                hash,
                retain,
            }
        }
        fn field(&mut self, bytes: &[u8]) {
            self.hash.update(&(bytes.len() as u64).to_le_bytes());
            self.hash.update(bytes);
        }
        #[expect(
            clippy::too_many_arguments,
            reason = "遍历沿用同一受信后端、上下文、保护集和根目录"
        )]
        fn walk(
            &mut self,
            backend: &LocalDeveloper,
            context: &ToolExecutionContext,
            protected: &BTreeSet<PathBuf>,
            dir: &File,
            root: &Path,
            relative: &str,
            depth: usize,
        ) -> Result<(), ToolExecutionError> {
            if depth > WORKSPACE_SNAPSHOT_MAX_DEPTH {
                return Err(limit());
            }
            let path = if relative == "." {
                root.to_path_buf()
            } else {
                root.join(relative)
            };
            check(backend, context, &path, protected)?;
            let before = metadata(dir)?;
            if !before.is_dir() {
                return Err(error());
            }
            self.add(relative, &before, WorkspaceSnapshotContents::Directory)?;
            let children = names(dir)?;
            for name in &children {
                let relative = if relative == "." {
                    name.clone()
                } else {
                    format!("{relative}/{name}")
                };
                if relative.len() > 4096 {
                    return Err(limit());
                }
                let path = root.join(&relative);
                let observed = open_at(dir, name, libc::O_PATH)?;
                let before = metadata(&observed)?;
                if !(before.is_file() || before.is_dir()) {
                    return Err(rejected(
                        "workspace_snapshot_special_object：输入含链接或特殊对象",
                    ));
                }
                if before.is_file() && before.nlink() != 1 {
                    return Err(rejected(
                        "workspace_snapshot_hardlink：输入文件存在硬链接别名",
                    ));
                }
                if matches!(
                    name.to_ascii_lowercase().as_str(),
                    ".git" | ".eve-developer"
                ) {
                    self.reserve()?;
                    self.field(b"excluded:protected_metadata");
                    self.field(relative.as_bytes());
                    self.stamps.insert(relative.clone(), stamp(&before));
                    self.excluded.push(WorkspaceSnapshotExclusion {
                        path: relative,
                        reason: WorkspaceSnapshotExclusionReason::ProtectedMetadata,
                    });
                    continue;
                }
                check(backend, context, &path, protected)?;
                if before.is_dir() {
                    self.walk(
                        backend,
                        context,
                        protected,
                        &observed,
                        root,
                        &relative,
                        depth + 1,
                    )?;
                } else {
                    if before.len() > WORKSPACE_SNAPSHOT_MAX_FILE_BYTES
                        || self.total.saturating_add(before.len()) > WORKSPACE_SNAPSHOT_MAX_BYTES
                    {
                        return Err(limit());
                    }
                    let mut file = readable(&observed)?;
                    if stamp(&before) != stamp(&metadata(&file)?) {
                        return Err(error());
                    }
                    check(backend, context, &path, protected)?;
                    let mut bytes = Vec::with_capacity(before.len() as usize);
                    // walk 的每一层都会保留栈帧；读取缓冲必须在堆上，
                    // 否则即便遍历空目录也会为每层预留 64 KiB 栈空间。
                    let mut buffer = vec![0u8; 65536];
                    loop {
                        check(backend, context, &path, protected)?;
                        let n = match file.read(&mut buffer) {
                            Ok(n) => n,
                            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                                continue;
                            }
                            Err(_) => return Err(error()),
                        };
                        if n == 0 {
                            break;
                        }
                        if bytes.len() as u64 + n as u64 > before.len() {
                            return Err(error());
                        }
                        bytes.extend_from_slice(&buffer[..n]);
                    }
                    check(backend, context, &path, protected)?;
                    if bytes.len() as u64 != before.len()
                        || stamp(&before) != stamp(&metadata(&file)?)
                        || stamp(&before) != stamp(&metadata(&open_at(dir, name, libc::O_PATH)?)?)
                    {
                        return Err(error());
                    }
                    // 完整二进制字节保持原样；发现已知凭据时拒绝，不先脱敏再称完整。
                    if backend.token.as_ref().is_some_and(|token| {
                        bytes
                            .windows(token.len())
                            .any(|chunk| chunk == token.as_bytes())
                    }) {
                        return Err(rejected("目录输入包含宿主凭据，未返回快照"));
                    }
                    if let Ok(text) = std::str::from_utf8(&bytes) {
                        backend.reject_evidence_credentials(text)?;
                    }
                    self.total += bytes.len() as u64;
                    self.add(&relative, &before, WorkspaceSnapshotContents::File(bytes))?;
                }
            }
            check(backend, context, &path, protected)?;
            if children != names(dir)? || stamp(&before) != stamp(&metadata(dir)?) {
                return Err(error());
            }
            Ok(())
        }
        fn reserve(&self) -> Result<(), ToolExecutionError> {
            if self.entries.len() + self.excluded.len() >= WORKSPACE_SNAPSHOT_MAX_ENTRIES {
                Err(limit())
            } else {
                Ok(())
            }
        }
        fn add(
            &mut self,
            path: &str,
            meta: &Metadata,
            contents: WorkspaceSnapshotContents,
        ) -> Result<(), ToolExecutionError> {
            self.reserve()?;
            let executable_bits = meta.mode() & 0o111;
            self.field(path.as_bytes());
            self.field(&executable_bits.to_le_bytes());
            match &contents {
                WorkspaceSnapshotContents::Directory => self.field(b"directory"),
                WorkspaceSnapshotContents::File(bytes) => {
                    self.field(b"file");
                    self.field(bytes);
                }
            }
            self.stamps.insert(path.into(), stamp(meta));
            self.entries.push(WorkspaceSnapshotEntry {
                path: path.into(),
                executable_bits,
                contents: if self.retain {
                    contents
                } else {
                    match contents {
                        WorkspaceSnapshotContents::Directory => {
                            WorkspaceSnapshotContents::Directory
                        }
                        WorkspaceSnapshotContents::File(_) => {
                            WorkspaceSnapshotContents::File(Vec::new())
                        }
                    }
                },
            });
            Ok(())
        }
    }

    pub(super) fn capture(
        original: &LocalDeveloper,
        request: WorkspaceSnapshotRequest,
        context: &ToolExecutionContext,
    ) -> Result<WorkspaceSnapshot, ToolExecutionError> {
        if request.root.is_empty()
            || request.root.len() > 4096
            || request.root.contains(['\\', ':'])
            || request.root.chars().any(char::is_control)
            || Path::new(&request.root)
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::Prefix(_)))
        {
            return Err(rejected("工作区快照根目录无效"));
        }
        let selection = select(original, &request, context)?;
        let backend = &selection.backend;
        let requested = Path::new(&request.root);
        let root = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            backend.root.join(requested)
        };
        // 仅规整 '.'，不跟随业务链接。
        let root: PathBuf = root
            .components()
            .filter(|part| !matches!(part, Component::CurDir))
            .collect();
        let protected = backend.protected_paths();
        check(backend, context, &root, &protected)?;
        let directory = open_absolute(&root)?;
        let original_root = stamp(&metadata(&directory)?);
        let mut first = Scan::new(true);
        first.walk(backend, context, &protected, &directory, &root, ".", 0)?;
        let mut second = Scan::new(false);
        second.walk(backend, context, &protected, &directory, &root, ".", 0)?;
        let first_digest = first.hash.finish();
        if first_digest.as_ref() != second.hash.finish().as_ref()
            || first.stamps != second.stamps
            || original_root != stamp(&metadata(&open_absolute(&root)?)?)
        {
            return Err(error());
        }
        if let Some((path, expected)) = selection.binding
            && read_binding(&path)? != expected
        {
            return Err(error());
        }
        check(backend, context, &root, &protected)?;
        first.entries.sort_by(|a, b| a.path.cmp(&b.path));
        first.excluded.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(WorkspaceSnapshot {
            scope: context.scope().ok_or_else(error)?.clone(),
            workspace_root: backend.root.clone(),
            root,
            entries: first.entries,
            excluded: first.excluded,
            total_bytes: first.total,
            sha256: first_digest
                .as_ref()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        })
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
