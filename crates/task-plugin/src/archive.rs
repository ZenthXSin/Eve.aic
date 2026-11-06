use eve_task_api::*;
use ring::digest::{Context, SHA256, digest};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_ARCHIVE_FILES: usize = 8192;

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
pub struct FileTaskArchive {
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
impl FileTaskArchive {
    pub fn open(directory: impl AsRef<Path>) -> TaskResult<Self> {
        std::fs::create_dir_all(&directory).map_err(|_| TaskError::Storage)?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.as_ref().join("archive.lock"))
            .map_err(|_| TaskError::Storage)?;
        lock.try_lock().map_err(|_| TaskError::Unavailable)?;
        Ok(Self {
            directory: directory
                .as_ref()
                .canonicalize()
                .map_err(|_| TaskError::Storage)?,
            _lock: ArchiveLock(lock),
            operation: std::sync::Mutex::new(()),
        })
    }
    fn path(&self, key: &str) -> TaskResult<PathBuf> {
        if !valid(key) {
            return Err(TaskError::InvalidInput);
        }
        Ok(self.directory.join(format!("{key}.json")))
    }
    /// 幂等写入仍完整验证已有正文；不为只需摘要的检查再分配整份冷记录。
    fn verify_and_sync(&self, hash: &str) -> TaskResult<()> {
        self.verify(hash, true)
    }
    fn verify(&self, hash: &str, sync: bool) -> TaskResult<()> {
        let path = self.path(hash)?;
        let meta = path.symlink_metadata().map_err(|_| TaskError::Storage)?;
        if !meta.is_file() || meta.len() > MAX_STATE_BYTES as u64 {
            return Err(TaskError::CorruptState);
        }
        // Windows 的 FlushFileBuffers 要求写句柄；只同步，不改写原文。
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(sync && cfg!(windows))
            .open(&path)
            .map_err(|_| TaskError::Storage)?;
        if !file.metadata().map_err(|_| TaskError::Storage)?.is_file() {
            return Err(TaskError::CorruptState);
        }
        let mut source = (&mut file).take((MAX_STATE_BYTES + 1) as u64);
        let mut digest = Context::new(&SHA256);
        let mut size = 0usize;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = match source.read(&mut buffer) {
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(TaskError::Storage),
            };
            if count == 0 {
                break;
            }
            size += count;
            if size > MAX_STATE_BYTES {
                return Err(TaskError::CorruptState);
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
            return Err(TaskError::CorruptState);
        }
        if sync {
            file.sync_all().map_err(|_| TaskError::Storage)?;
        }
        Ok(())
    }
}
impl TaskArchiveStore for FileTaskArchive {
    fn check_capacity(&self, plan: &TaskArchivePlan<'_>) -> TaskResult<()> {
        let _guard = self.operation.lock().map_err(|_| TaskError::Unavailable)?;
        let references = |records: &[TaskArchiveRecord]| -> TaskResult<BTreeMap<String, usize>> {
            if records.len() > MAX_TASKS {
                return Err(TaskError::LimitReached);
            }
            let mut unique = BTreeMap::new();
            for record in records {
                if !valid(&record.hash) || record.bytes == 0 || record.bytes > MAX_STATE_BYTES {
                    return Err(TaskError::InvalidInput);
                }
                if unique
                    .insert(record.hash.clone(), record.bytes)
                    .is_some_and(|bytes| bytes != record.bytes)
                {
                    return Err(TaskError::CorruptState);
                }
            }
            Ok(unique)
        };
        let current = references(plan.current)?;
        let retained = references(plan.retained)?;
        let writes = references(plan.writes)?;
        if writes
            .iter()
            .any(|(hash, bytes)| retained.get(hash) != Some(bytes))
        {
            return Err(TaskError::InvalidInput);
        }
        let mut physical = BTreeMap::new();
        let mut used = 0usize;
        let mut files = 0usize;
        for entry in std::fs::read_dir(&self.directory).map_err(|_| TaskError::Storage)? {
            let entry = entry.map_err(|_| TaskError::Storage)?;
            let metadata = entry
                .path()
                .symlink_metadata()
                .map_err(|_| TaskError::Storage)?;
            let bytes = usize::try_from(metadata.len()).map_err(|_| TaskError::LimitReached)?;
            used = used.checked_add(bytes).ok_or(TaskError::LimitReached)?;
            files = files.checked_add(1).ok_or(TaskError::LimitReached)?;
            // 未知名称、备份、链接及锁也占物理配额，不假定 compact 能收回。
            if let Some(hash) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                && valid(hash)
            {
                physical.insert(hash.to_owned(), (bytes, metadata.is_file()));
            }
        }
        for (hash, bytes) in current.iter().chain(&retained) {
            match physical.get(hash) {
                Some((actual, true)) if actual == bytes => {}
                None if writes.get(hash) == Some(bytes) && !current.contains_key(hash) => {}
                _ => return Err(TaskError::CorruptState),
            }
        }
        let mut next = used;
        let mut next_files = files;
        for (hash, bytes) in &writes {
            if !physical.contains_key(hash) {
                next = next.checked_add(*bytes).ok_or(TaskError::LimitReached)?;
                next_files = next_files.checked_add(1).ok_or(TaskError::LimitReached)?;
            }
        }
        // 本批所有正文与已确认旧索引必须共存，不能用将来回收抵扣当前写入。
        if next > MAX_ARCHIVE_BYTES || next_files > MAX_ARCHIVE_FILES {
            return Err(TaskError::LimitReached);
        }
        let retired: BTreeSet<_> = current
            .keys()
            .filter(|hash| !retained.contains_key(*hash))
            .collect();
        if !retired.is_empty() {
            // compact 会先核对全部无引用标准文件。损坏旧文件会阻止回收，不能预支那份空间。
            for (hash, (_, regular)) in &physical {
                if *regular && !retained.contains_key(hash) {
                    self.verify(hash, false)?;
                }
            }
        }
        for hash in retired {
            let (bytes, _) = physical.get(hash).ok_or(TaskError::CorruptState)?;
            next = next.checked_sub(*bytes).ok_or(TaskError::CorruptState)?;
            next_files = next_files.checked_sub(1).ok_or(TaskError::CorruptState)?;
        }
        // 只有本次旧索引明确退役的 hash 可抵扣；共享 hash、未知和备份仍完整计数。
        if next
            .checked_add(plan.reserve_bytes)
            .is_none_or(|bytes| bytes > MAX_ARCHIVE_BYTES)
            || next_files
                .checked_add(plan.reserve_files)
                .is_none_or(|files| files > MAX_ARCHIVE_FILES)
        {
            return Err(TaskError::LimitReached);
        }
        Ok(())
    }
    fn get(&self, hash: &str) -> TaskResult<Vec<u8>> {
        let path = self.path(hash)?;
        let meta = path.symlink_metadata().map_err(|_| TaskError::Storage)?;
        if !meta.is_file() || meta.len() > MAX_STATE_BYTES as u64 {
            return Err(TaskError::CorruptState);
        }
        let mut bytes = Vec::new();
        File::open(path)
            .map_err(|_| TaskError::Storage)?
            .take((MAX_STATE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| TaskError::Storage)?;
        if bytes.len() > MAX_STATE_BYTES || key(&bytes) != hash {
            return Err(TaskError::CorruptState);
        }
        Ok(bytes)
    }
    fn put(&self, hash: &str, bytes: &[u8]) -> TaskResult<()> {
        let _guard = self.operation.lock().map_err(|_| TaskError::Unavailable)?;
        let target = self.path(hash)?;
        if bytes.len() > MAX_STATE_BYTES || key(bytes) != hash {
            return Err(TaskError::InvalidInput);
        }
        if target.symlink_metadata().is_ok() {
            self.verify_and_sync(hash)?;
            #[cfg(unix)]
            File::open(&self.directory)
                .and_then(|f| f.sync_all())
                .map_err(|_| TaskError::Storage)?;
            return Ok(());
        }
        let mut used = bytes.len() as u64;
        for (index, entry) in std::fs::read_dir(&self.directory)
            .map_err(|_| TaskError::Storage)?
            .enumerate()
        {
            if index >= MAX_ARCHIVE_FILES - 1 {
                return Err(TaskError::LimitReached);
            }
            let meta = entry
                .map_err(|_| TaskError::Storage)?
                .path()
                .symlink_metadata()
                .map_err(|_| TaskError::Storage)?;
            used = used
                .checked_add(meta.len())
                .ok_or(TaskError::LimitReached)?;
            if used > MAX_ARCHIVE_BYTES as u64 {
                return Err(TaskError::LimitReached);
            }
        }
        let mut temp =
            tempfile::NamedTempFile::new_in(&self.directory).map_err(|_| TaskError::Storage)?;
        temp.write_all(bytes).map_err(|_| TaskError::Storage)?;
        temp.as_file().sync_all().map_err(|_| TaskError::Storage)?;
        match temp.persist_noclobber(&target) {
            Ok(_) => {}
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                self.verify_and_sync(hash)?;
            }
            Err(_) => return Err(TaskError::Storage),
        }
        #[cfg(unix)]
        File::open(&self.directory)
            .and_then(|f| f.sync_all())
            .map_err(|_| TaskError::Storage)?;
        Ok(())
    }
    fn compact(&self, retained: &[String]) -> TaskResult<()> {
        let _guard = self.operation.lock().map_err(|_| TaskError::Unavailable)?;
        if retained.len() > MAX_TASKS || retained.iter().any(|hash| !valid(hash)) {
            return Err(TaskError::InvalidInput);
        }
        let mut paths = Vec::new();
        // 先验证完整目录，再删除；不跟随链接，不碰锁、备份或未知名字。
        for (index, entry) in std::fs::read_dir(&self.directory)
            .map_err(|_| TaskError::Storage)?
            .enumerate()
        {
            if index >= MAX_ARCHIVE_FILES {
                return Err(TaskError::LimitReached);
            }
            let entry = entry.map_err(|_| TaskError::Storage)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(hash) = name.strip_suffix(".json").filter(|hash| valid(hash)) else {
                continue;
            };
            if !retained.iter().any(|retained| retained == hash)
                && entry.file_type().map_err(|_| TaskError::Storage)?.is_file()
            {
                paths.push(entry.path());
            }
        }
        for path in &paths {
            let hash = path
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or(TaskError::CorruptState)?;
            // 无引用也不能销毁损坏现场；仅回收能完整核对摘要的旧正文。
            self.verify_and_sync(hash)?;
        }
        for path in paths {
            std::fs::remove_file(path).map_err(|_| TaskError::Storage)?;
        }
        #[cfg(unix)]
        File::open(&self.directory)
            .and_then(|file| file.sync_all())
            .map_err(|_| TaskError::Storage)?;
        Ok(())
    }
}
