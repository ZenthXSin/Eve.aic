use eve_session_api::*;
use ring::digest::{Context, SHA256, digest};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub(crate) fn key(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub(crate) fn valid(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
/// 每个调用重新读取并校验；不将冷正文缓存在该对象中，不提供删除入口。
pub struct FileSessionArchive {
    directory: PathBuf,
}
impl FileSessionArchive {
    pub fn open(directory: impl AsRef<Path>) -> SessionResult<Self> {
        std::fs::create_dir_all(&directory).map_err(|_| SessionError::Storage)?;
        Ok(Self {
            directory: directory
                .as_ref()
                .canonicalize()
                .map_err(|_| SessionError::Storage)?,
        })
    }
    fn path(&self, key: &str) -> SessionResult<PathBuf> {
        if !valid(key) {
            return Err(SessionError::InvalidInput);
        }
        Ok(self.directory.join(format!("{key}.json")))
    }
    /// 幂等写入仍完整验证已有正文；不为只需摘要的检查再分配整份冷记录。
    fn verify_and_sync(&self, hash: &str) -> SessionResult<()> {
        let path = self.path(hash)?;
        let meta = path.symlink_metadata().map_err(|_| SessionError::Storage)?;
        if !meta.is_file() || meta.len() > MAX_SESSION_ARCHIVE_RECORD_BYTES as u64 {
            return Err(SessionError::CorruptState);
        }
        // Windows 的 FlushFileBuffers 要求写句柄；只同步，不改写原文。
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(cfg!(windows))
            .open(&path)
            .map_err(|_| SessionError::Storage)?;
        if !file
            .metadata()
            .map_err(|_| SessionError::Storage)?
            .is_file()
        {
            return Err(SessionError::CorruptState);
        }
        let mut source = (&mut file).take((MAX_SESSION_ARCHIVE_RECORD_BYTES + 1) as u64);
        let mut digest = Context::new(&SHA256);
        let mut size = 0usize;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = match source.read(&mut buffer) {
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(SessionError::Storage),
            };
            if count == 0 {
                break;
            }
            size += count;
            if size > MAX_SESSION_ARCHIVE_RECORD_BYTES {
                return Err(SessionError::CorruptState);
            }
            digest.update(&buffer[..count]);
        }
        let actual: String = digest
            .finish()
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if actual != hash {
            return Err(SessionError::CorruptState);
        }
        file.sync_all().map_err(|_| SessionError::Storage)
    }
}
impl SessionArchiveStore for FileSessionArchive {
    fn get(&self, hash: &str) -> SessionResult<Vec<u8>> {
        let path = self.path(hash)?;
        let meta = path.symlink_metadata().map_err(|_| SessionError::Storage)?;
        if !meta.is_file() || meta.len() > MAX_SESSION_ARCHIVE_RECORD_BYTES as u64 {
            return Err(SessionError::CorruptState);
        }
        let mut bytes = Vec::new();
        File::open(path)
            .map_err(|_| SessionError::Storage)?
            .take((MAX_SESSION_ARCHIVE_RECORD_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| SessionError::Storage)?;
        if bytes.len() > MAX_SESSION_ARCHIVE_RECORD_BYTES || key(&bytes) != hash {
            return Err(SessionError::CorruptState);
        }
        Ok(bytes)
    }
    fn put(&self, hash: &str, bytes: &[u8]) -> SessionResult<()> {
        let target = self.path(hash)?;
        if bytes.len() > MAX_SESSION_ARCHIVE_RECORD_BYTES || key(bytes) != hash {
            return Err(SessionError::InvalidInput);
        }
        if target.symlink_metadata().is_ok() {
            self.verify_and_sync(hash)?;
            #[cfg(unix)]
            File::open(&self.directory)
                .and_then(|f| f.sync_all())
                .map_err(|_| SessionError::Storage)?;
            return Ok(());
        }
        let mut temp =
            tempfile::NamedTempFile::new_in(&self.directory).map_err(|_| SessionError::Storage)?;
        temp.write_all(bytes).map_err(|_| SessionError::Storage)?;
        temp.as_file()
            .sync_all()
            .map_err(|_| SessionError::Storage)?;
        match temp.persist_noclobber(&target) {
            Ok(_) => {}
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                self.verify_and_sync(hash)?;
            }
            Err(_) => return Err(SessionError::Storage),
        }
        #[cfg(unix)]
        File::open(&self.directory)
            .and_then(|f| f.sync_all())
            .map_err(|_| SessionError::Storage)?;
        Ok(())
    }
}
