use eve_memory_api::*;
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
pub struct FileSummaryArchive {
    directory: PathBuf,
    _lock: ArchiveLock,
    operation: std::sync::Mutex<()>,
}
struct ArchiveLock(File);
impl Drop for ArchiveLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
impl FileSummaryArchive {
    pub fn open(directory: impl AsRef<Path>) -> MemoryResult<Self> {
        std::fs::create_dir_all(&directory).map_err(|_| MemoryError::Storage)?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.as_ref().join("archive.lock"))
            .map_err(|_| MemoryError::Storage)?;
        lock.try_lock().map_err(|_| MemoryError::Unavailable)?;
        Ok(Self {
            directory: directory
                .as_ref()
                .canonicalize()
                .map_err(|_| MemoryError::Storage)?,
            _lock: ArchiveLock(lock),
            operation: std::sync::Mutex::new(()),
        })
    }
    fn path(&self, key: &str) -> MemoryResult<PathBuf> {
        if !valid(key) {
            return Err(MemoryError::InvalidInput);
        }
        Ok(self.directory.join(format!("{key}.json")))
    }
    /// 幂等写入仍完整验证已有正文；不为只需摘要的检查再分配整份冷记录。
    fn verify_and_sync(&self, hash: &str) -> MemoryResult<()> {
        let path = self.path(hash)?;
        let meta = path.symlink_metadata().map_err(|_| MemoryError::Storage)?;
        if !meta.is_file() || meta.len() > MAX_SUMMARY_ARCHIVE_BYTES as u64 {
            return Err(MemoryError::CorruptState);
        }
        // Windows 的 FlushFileBuffers 要求写句柄；只同步，不改写原文。
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(cfg!(windows))
            .open(&path)
            .map_err(|_| MemoryError::Storage)?;
        if !file.metadata().map_err(|_| MemoryError::Storage)?.is_file() {
            return Err(MemoryError::CorruptState);
        }
        let mut source = (&mut file).take((MAX_SUMMARY_ARCHIVE_BYTES + 1) as u64);
        let mut digest = Context::new(&SHA256);
        let mut size = 0usize;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = match source.read(&mut buffer) {
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(MemoryError::Storage),
            };
            if count == 0 {
                break;
            }
            size += count;
            if size > MAX_SUMMARY_ARCHIVE_BYTES {
                return Err(MemoryError::CorruptState);
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
            return Err(MemoryError::CorruptState);
        }
        file.sync_all().map_err(|_| MemoryError::Storage)
    }
}
impl SummaryArchiveStore for FileSummaryArchive {
    fn get(&self, hash: &str) -> MemoryResult<Vec<u8>> {
        let path = self.path(hash)?;
        let meta = path.symlink_metadata().map_err(|_| MemoryError::Storage)?;
        if !meta.is_file() || meta.len() > MAX_SUMMARY_ARCHIVE_BYTES as u64 {
            return Err(MemoryError::CorruptState);
        }
        let mut bytes = Vec::new();
        File::open(path)
            .map_err(|_| MemoryError::Storage)?
            .take((MAX_SUMMARY_ARCHIVE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| MemoryError::Storage)?;
        if bytes.len() > MAX_SUMMARY_ARCHIVE_BYTES || key(&bytes) != hash {
            return Err(MemoryError::CorruptState);
        }
        Ok(bytes)
    }
    fn put(&self, hash: &str, bytes: &[u8]) -> MemoryResult<()> {
        let _guard = self
            .operation
            .lock()
            .map_err(|_| MemoryError::Unavailable)?;
        let target = self.path(hash)?;
        if bytes.len() > MAX_SUMMARY_ARCHIVE_BYTES || key(bytes) != hash {
            return Err(MemoryError::InvalidInput);
        }
        if target.symlink_metadata().is_ok() {
            self.verify_and_sync(hash)?;
            #[cfg(unix)]
            File::open(&self.directory)
                .and_then(|f| f.sync_all())
                .map_err(|_| MemoryError::Storage)?;
            return Ok(());
        }
        let mut used = bytes.len() as u64;
        for (index, entry) in std::fs::read_dir(&self.directory)
            .map_err(|_| MemoryError::Storage)?
            .enumerate()
        {
            if index >= 65536 {
                return Err(MemoryError::LimitReached);
            }
            let meta = entry
                .map_err(|_| MemoryError::Storage)?
                .metadata()
                .map_err(|_| MemoryError::Storage)?;
            used = used
                .checked_add(meta.len())
                .ok_or(MemoryError::LimitReached)?;
            if used > 512 * 1024 * 1024 {
                return Err(MemoryError::LimitReached);
            }
        }
        let mut temp =
            tempfile::NamedTempFile::new_in(&self.directory).map_err(|_| MemoryError::Storage)?;
        temp.write_all(bytes).map_err(|_| MemoryError::Storage)?;
        temp.as_file()
            .sync_all()
            .map_err(|_| MemoryError::Storage)?;
        match temp.persist_noclobber(&target) {
            Ok(_) => {}
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                self.verify_and_sync(hash)?;
            }
            Err(_) => return Err(MemoryError::Storage),
        }
        #[cfg(unix)]
        File::open(&self.directory)
            .and_then(|f| f.sync_all())
            .map_err(|_| MemoryError::Storage)?;
        Ok(())
    }
}
