//! 受信宿主显式绑定一个本地文件，再把实际读取的数据交给认知公开契约。
//! 不接收模型路径、不扫描目录、不写文件；路径只保存在此实例内。
use eve_cognition_api::{
    FileObservationInput, MAX_FILE_OBSERVATION_BYTES, MAX_FILE_OBSERVATION_EXCERPT_BYTES,
};
use ring::digest::{SHA256, digest};
use std::{
    fmt,
    fs::{self, Metadata, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileReadError {
    Io(io::ErrorKind),
    NotRegular,
    TooLarge,
    InvalidText,
    Changed,
    InvalidObservation,
}
impl fmt::Display for FileReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 不把 OS 错误中的原始路径或读取正文带到日志。
        match self {
            Self::Io(kind) => write!(f, "本地观察文件读取失败：{kind:?}。"),
            Self::NotRegular => f.write_str("本地观察仅接受普通文件。"),
            Self::TooLarge => f.write_str("本地观察文件超过 65536 字节。"),
            Self::InvalidText => f.write_str("本地观察文件必须是无 NUL 的 UTF-8 文本。"),
            Self::Changed => f.write_str("本地观察文件在读取期间变化；未保存本次观察。"),
            Self::InvalidObservation => f.write_str("本地文件观察不符合认知输入契约。"),
        }
    }
}
impl std::error::Error for FileReadError {}
impl From<io::Error> for FileReadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

/// 绑定启动时解析的规范路径；后续允许该路径上的普通文件被原子替换。
/// 路径不是沙箱边界：调用者须显式选择可交给规划模型读取的文件。
pub struct BoundFileObserver {
    path: PathBuf,
    source_id: String,
}
impl fmt::Debug for BoundFileObserver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BoundFileObserver(<redacted>)")
    }
}
impl BoundFileObserver {
    pub fn bind(path: &Path) -> Result<Self, FileReadError> {
        let path = fs::canonicalize(path)?;
        check_file(&fs::metadata(&path)?)?;
        let mut source = b"eve.file-observation.path.v1\0".to_vec();
        source.extend_from_slice(path.as_os_str().as_encoded_bytes());
        Ok(Self {
            path,
            source_id: format!("file-source:{}", sha256(&source)),
        })
    }

    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    /// 摘要覆盖完整读到的字节；excerpt 只是有限前缀，不宣称语义正确或目标完成。
    /// 若检测到长度、修改时间或文件身份变化，整次读取失败，由宿主决定何时重试。
    pub fn read_input(
        &self,
        goal_id: &str,
        expected_goal_revision: u64,
        observed_at_ms: u64,
    ) -> Result<FileObservationInput, FileReadError> {
        let before = fs::symlink_metadata(&self.path)?;
        check_file(&before)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // 即使检查后路径被换成 FIFO，也不在 open 阻塞；末级符号链接不跟随。
            options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
        }
        let mut file = options.open(&self.path)?;
        let opened = file.metadata()?;
        check_file(&opened)?;
        if !same_file(&before, &opened)? {
            return Err(FileReadError::Changed);
        }
        let mut bytes = Vec::with_capacity(opened.len() as usize);
        (&mut file)
            .take(MAX_FILE_OBSERVATION_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_FILE_OBSERVATION_BYTES {
            return Err(FileReadError::TooLarge);
        }
        let after = file.metadata()?;
        let current = fs::symlink_metadata(&self.path)?;
        if bytes.len() as u64 != opened.len()
            || !same_file(&opened, &after)?
            || !same_file(&after, &current)?
        {
            return Err(FileReadError::Changed);
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| FileReadError::InvalidText)?;
        if text.contains('\0') {
            return Err(FileReadError::InvalidText);
        }
        let mut end = text.len().min(MAX_FILE_OBSERVATION_EXCERPT_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        // 控制字符会被 JSON 转义；给契约的其它字段留下确定的空间。
        while serde_json::to_string(&text[..end])
            .map_err(|_| FileReadError::InvalidObservation)?
            .len()
            > MAX_FILE_OBSERVATION_EXCERPT_BYTES
        {
            end /= 2;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
        }
        let input = FileObservationInput {
            goal_id: goal_id.into(),
            expected_goal_revision,
            observation_source_id: self.source_id.clone(),
            sha256: sha256(&bytes),
            byte_count: bytes.len() as u64,
            text_excerpt: text[..end].into(),
            text_truncated: end < bytes.len(),
            observed_at_ms,
        };
        input
            .validate()
            .map_err(|_| FileReadError::InvalidObservation)?;
        Ok(input)
    }
}

fn check_file(metadata: &Metadata) -> Result<(), FileReadError> {
    if !metadata.is_file() {
        return Err(FileReadError::NotRegular);
    }
    if metadata.len() > MAX_FILE_OBSERVATION_BYTES {
        return Err(FileReadError::TooLarge);
    }
    Ok(())
}

fn same_file(left: &Metadata, right: &Metadata) -> Result<bool, FileReadError> {
    if !right.is_file() || left.len() != right.len() || left.modified()? != right.modified()? {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if left.dev() != right.dev()
            || left.ino() != right.ino()
            || left.ctime() != right.ctime()
            || left.ctime_nsec() != right.ctime_nsec()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn sha256(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
