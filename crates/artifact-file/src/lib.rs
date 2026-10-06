//! 受信宿主显式绑定一个本地文本输出文件；模型不能选择路径。
//!
//! 只在已存在的父目录中新建文件，不覆盖、不创建目录。写入后同步文件；Unix 还
//! 同步父目录，其他平台不承诺断电后的目录项持久性。独立的 `read_back` 重新打开
//! 文件取得实际回读凭据。任何写入失败都保留已经
//! 创建的文件，供宿主检查；不会把不完整的输出删除后自动重试。
//!
//! 路径只存在于此适配器中，公开来源标识只有带域分隔的路径摘要。此绑定不是敌对
//! 文件系统上的原子沙箱；父目录由宿主选择并管理。Unix 打开文件时不跟随末级
//! 符号链接，并使用非阻塞标志，避免路径被换成 FIFO 后阻塞在打开操作。

use eve_action_api::{
    ArtifactError, ArtifactReceipt, ArtifactResult, ArtifactTarget, MAX_ARTIFACT_BYTES,
};
use ring::digest::{SHA256, digest};
use std::{
    fmt,
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

/// 由宿主绑定的单个输出位置，不接受模型生成的路径。
pub struct BoundArtifactFile {
    path: PathBuf,
    parent: PathBuf,
    source_id: String,
}

impl fmt::Debug for BoundArtifactFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BoundArtifactFile(<redacted>)")
    }
}

impl BoundArtifactFile {
    /// 解析已存在的父目录，并保留一个合法的末级文件名。
    ///
    /// 已存在的普通文件也可以绑定，供宿主根据持久回执处理重复请求；
    /// `write_new` 对该文件始终返回 `AlreadyExists`，绝不覆盖。
    pub fn bind(path: &Path) -> ArtifactResult<Self> {
        let name = path.file_name().ok_or(ArtifactError::InvalidInput)?;
        let bytes = path.as_os_str().as_encoded_bytes();
        let raw_name = bytes.rsplit(|byte| is_separator(*byte)).next();
        if raw_name != Some(name.as_encoded_bytes()) || name.as_encoded_bytes().contains(&0) {
            return Err(ArtifactError::InvalidInput);
        }
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = fs::canonicalize(parent).map_err(map_io)?;
        if !fs::metadata(&parent).map_err(map_io)?.is_dir() {
            return Err(ArtifactError::InvalidInput);
        }
        let path = parent.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.is_file() => return Err(ArtifactError::InvalidInput),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(map_io(error)),
        }
        let mut source = b"eve.artifact-file.path.v1\0".to_vec();
        source.extend_from_slice(path.as_os_str().as_encoded_bytes());
        Ok(Self {
            path,
            parent,
            source_id: format!("artifact-file:{}", sha256(&source)),
        })
    }

    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    /// 新建并同步输出；出错后留下现存文件，不删除或覆盖来掩盖部分写入。
    pub fn write_new(&self, bytes: &[u8]) -> ArtifactResult<()> {
        validate_text(bytes)?;
        let directory = self.open_parent_directory()?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
        }
        let mut file = options.open(&self.path).map_err(map_io)?;
        let created = file.metadata().map_err(map_io)?;
        check_file(&created)?;
        file.write_all(bytes).map_err(map_io)?;
        file.sync_all().map_err(map_io)?;
        let written = file.metadata().map_err(map_io)?;
        let current = fs::symlink_metadata(&self.path).map_err(map_io)?;
        if written.len() != bytes.len() as u64
            || !same_identity(&created, &written)
            || !same_file(&written, &current)?
        {
            return Err(ArtifactError::Changed);
        }
        let opened_parent = directory.metadata().map_err(map_io)?;
        let current_parent = fs::symlink_metadata(&self.parent).map_err(map_io)?;
        if !current_parent.is_dir() || !same_identity(&opened_parent, &current_parent) {
            return Err(ArtifactError::Changed);
        }
        // Windows 的目录句柄不具备 FlushFileBuffers 要求的 GENERIC_WRITE 权限。
        // 保留上面的文件同步和目录核验；只在 Unix 上承诺目录项同步。
        #[cfg(unix)]
        directory.sync_all().map_err(map_io)?;
        Ok(())
    }

    /// 重新打开输出，凭完整读到的字节生成回执，绝不以原始写入缓冲区自证。
    ///
    /// 检测到身份、长度或修改时间变化时，本次回读失败。回执只证明回读时的
    /// 有界文本内容；不证明目标语义完成，也不保证之后没有外部修改。
    pub fn read_back(&self, verified_at_ms: u64) -> ArtifactResult<ArtifactReceipt> {
        if verified_at_ms == 0 {
            return Err(ArtifactError::InvalidInput);
        }
        let before = fs::symlink_metadata(&self.path).map_err(map_io)?;
        check_file(&before)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_FLAG_OPEN_REPARSE_POINT：打开末级重解析点本身，再由 metadata 拒绝。
            options.custom_flags(0x0020_0000);
        }
        let mut file = options.open(&self.path).map_err(map_io)?;
        let opened = file.metadata().map_err(map_io)?;
        check_file(&opened)?;
        if !same_file(&before, &opened)? {
            return Err(ArtifactError::Changed);
        }
        let mut bytes = Vec::with_capacity(opened.len() as usize);
        (&mut file)
            .take(MAX_ARTIFACT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(map_io)?;
        if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
            return Err(ArtifactError::LimitReached);
        }
        let after = file.metadata().map_err(map_io)?;
        let current = fs::symlink_metadata(&self.path).map_err(map_io)?;
        if bytes.len() as u64 != opened.len()
            || !same_file(&opened, &after)?
            || !same_file(&after, &current)?
        {
            return Err(ArtifactError::Changed);
        }
        validate_text(&bytes)?;
        let receipt = ArtifactReceipt {
            artifact_source_id: self.source_id.clone(),
            sha256: sha256(&bytes),
            byte_count: bytes.len() as u64,
            verified_at_ms,
        };
        receipt
            .validate()
            .map_err(|_| ArtifactError::InvalidInput)?;
        Ok(receipt)
    }

    fn open_parent_directory(&self) -> ArtifactResult<File> {
        let before = fs::symlink_metadata(&self.parent).map_err(map_io)?;
        if !before.is_dir() {
            return Err(ArtifactError::InvalidInput);
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_DIRECTORY | libc::O_NONBLOCK | libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_FLAG_BACKUP_SEMANTICS 是 CreateFile 打开目录的必要标志。
            // 零访问权限足以读取 metadata；OPEN_REPARSE_POINT 避免跟随末级重解析点。
            options
                .access_mode(0)
                .custom_flags(0x0200_0000 | 0x0020_0000);
        }
        let directory = options.open(&self.parent).map_err(map_io)?;
        let opened = directory.metadata().map_err(map_io)?;
        if !opened.is_dir() || !same_identity(&before, &opened) {
            return Err(ArtifactError::Changed);
        }
        Ok(directory)
    }
}

impl ArtifactTarget for BoundArtifactFile {
    fn source_id(&self) -> &str {
        self.source_id()
    }

    fn write_new(&self, bytes: &[u8]) -> ArtifactResult<()> {
        self.write_new(bytes)
    }

    fn read_back(&self, verified_at_ms: u64) -> ArtifactResult<ArtifactReceipt> {
        self.read_back(verified_at_ms)
    }
}

fn is_separator(byte: u8) -> bool {
    if byte == b'/' {
        return true;
    }
    #[cfg(windows)]
    if byte == b'\\' {
        return true;
    }
    false
}

fn validate_text(bytes: &[u8]) -> ArtifactResult<()> {
    if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
        return Err(ArtifactError::LimitReached);
    }
    if bytes.contains(&0) || std::str::from_utf8(bytes).is_err() {
        return Err(ArtifactError::InvalidInput);
    }
    Ok(())
}

fn check_file(metadata: &Metadata) -> ArtifactResult<()> {
    if !metadata.is_file() {
        return Err(ArtifactError::InvalidInput);
    }
    if metadata.len() > MAX_ARTIFACT_BYTES {
        return Err(ArtifactError::LimitReached);
    }
    Ok(())
}

fn same_file(left: &Metadata, right: &Metadata) -> ArtifactResult<bool> {
    if !right.is_file()
        || left.len() != right.len()
        || left.modified().map_err(map_io)? != right.modified().map_err(map_io)?
        || !same_identity(left, right)
    {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if left.ctime() != right.ctime() || left.ctime_nsec() != right.ctime_nsec() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn same_identity(left: &Metadata, right: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        left.dev() == right.dev() && left.ino() == right.ino()
    }
    #[cfg(not(unix))]
    {
        left.file_type() == right.file_type()
    }
}

fn sha256(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn map_io(error: io::Error) -> ArtifactError {
    // 公开错误只保留固定类别，不携带 OS 消息中的路径、文件正文或权限细节。
    match error.kind() {
        io::ErrorKind::AlreadyExists => ArtifactError::AlreadyExists,
        io::ErrorKind::NotFound => ArtifactError::NotFound,
        io::ErrorKind::PermissionDenied => ArtifactError::AccessDenied,
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => ArtifactError::InvalidInput,
        io::ErrorKind::WouldBlock => ArtifactError::Unavailable,
        _ => ArtifactError::Storage,
    }
}
