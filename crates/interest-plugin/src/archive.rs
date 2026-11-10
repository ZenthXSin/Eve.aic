//! 兴趣观察原件的不可变文件归档；只保全字节，不整理索引、不删除孤儿或重放观察。
use eve_interest_api::{
    InterestError, InterestJobArchiveStore, InterestResult, MAX_JOB_ARCHIVE_RECORD_BYTES,
};
use ring::digest::{SHA256, digest};
use std::path::Path;

const MAX_ARCHIVE_FILES: usize = 8192;
const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;

fn key(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn valid(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// 可信宿主装配的目录归档，完整原文只在本次 put/get 中持有，不驻留缓存。
/// 不提供删除入口；写入提交未知时保留临时原件，并在后续写入中计入物理配额。
pub struct FileInterestJobArchive {
    #[cfg(target_os = "linux")]
    inner: linux::Archive,
}
impl FileInterestJobArchive {
    pub fn open(directory: impl AsRef<Path>) -> InterestResult<Self> {
        #[cfg(target_os = "linux")]
        {
            Ok(Self {
                inner: linux::Archive::open(directory.as_ref())?,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = directory;
            Err(InterestError::Unavailable)
        }
    }
}
impl InterestJobArchiveStore for FileInterestJobArchive {
    fn is_empty(&self) -> InterestResult<bool> {
        #[cfg(target_os = "linux")]
        {
            self.inner.is_empty()
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(InterestError::Unavailable)
        }
    }
    fn put(&self, hash: &str, bytes: &[u8]) -> InterestResult<()> {
        if !valid(hash) {
            return Err(InterestError::InvalidInput);
        }
        if bytes.len() > MAX_JOB_ARCHIVE_RECORD_BYTES {
            return Err(InterestError::LimitReached);
        }
        if key(bytes) != hash {
            return Err(InterestError::Conflict);
        }
        #[cfg(target_os = "linux")]
        {
            self.inner.put(hash, bytes)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(InterestError::Unavailable)
        }
    }
    fn get(&self, hash: &str) -> InterestResult<Vec<u8>> {
        if !valid(hash) {
            return Err(InterestError::InvalidInput);
        }
        #[cfg(target_os = "linux")]
        {
            self.inner.get(hash)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(InterestError::Unavailable)
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use ring::rand::{SecureRandom, SystemRandom};
    use std::{
        ffi::{CString, OsStr},
        fs::{self, File, Metadata, OpenOptions},
        io::{Read, Write},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{
                ffi::OsStrExt,
                fs::{MetadataExt, OpenOptionsExt},
            },
        },
        path::{Component, PathBuf},
        sync::Mutex,
    };

    pub(super) struct Archive {
        path: PathBuf,
        directory: File,
        lock: File,
        operation: Mutex<()>,
    }
    impl Drop for Archive {
        fn drop(&mut self) {
            let _ = self.lock.unlock();
        }
    }
    fn storage(_: impl std::fmt::Debug) -> InterestError {
        InterestError::Storage
    }
    fn name(value: &OsStr) -> InterestResult<CString> {
        CString::new(value.as_bytes()).map_err(|_| InterestError::InvalidInput)
    }
    fn open_at(directory: &File, value: &OsStr, flags: i32, mode: u32) -> std::io::Result<File> {
        let value = CString::new(value.as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        // SAFETY: directory 是活的 fd，value 为 NUL 终止字符串；成功 fd 只转交一次。
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                value.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                mode,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    fn same_identity(a: &Metadata, b: &Metadata) -> bool {
        a.dev() == b.dev() && a.ino() == b.ino() && a.mode() == b.mode()
    }
    fn same_contents(a: &Metadata, b: &Metadata) -> bool {
        same_identity(a, b)
            && a.len() == b.len()
            && a.nlink() == b.nlink()
            && a.mtime() == b.mtime()
            && a.mtime_nsec() == b.mtime_nsec()
            && a.ctime() == b.ctime()
            && a.ctime_nsec() == b.ctime_nsec()
    }
    fn ordinary(meta: &Metadata) -> InterestResult<()> {
        if !meta.is_file() || meta.nlink() != 1 {
            return Err(InterestError::CorruptState);
        }
        Ok(())
    }
    /// 从 / 开始逐级打开，不跟随任一级链接；只为显式 open 创建缺失目录。
    fn directory_at(path: &Path, create: bool) -> InterestResult<File> {
        if !path.is_absolute() {
            return Err(InterestError::InvalidInput);
        }
        let mut directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/")
            .map_err(storage)?;
        for part in path.components() {
            let part = match part {
                Component::RootDir | Component::CurDir => continue,
                Component::Normal(part) => part,
                _ => return Err(InterestError::InvalidInput),
            };
            let next = match open_at(&directory, part, libc::O_RDONLY | libc::O_DIRECTORY, 0) {
                Ok(file) => file,
                Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                    let value = name(part)?;
                    // SAFETY: 父目录 fd 与组件 CString 有效，mode 不含文件类型。
                    let result =
                        unsafe { libc::mkdirat(directory.as_raw_fd(), value.as_ptr(), 0o700) };
                    if result < 0
                        && std::io::Error::last_os_error().kind()
                            != std::io::ErrorKind::AlreadyExists
                    {
                        return Err(InterestError::Storage);
                    }
                    let file = open_at(&directory, part, libc::O_RDONLY | libc::O_DIRECTORY, 0)
                        .map_err(storage)?;
                    directory.sync_all().map_err(storage)?;
                    file
                }
                Err(_) => return Err(InterestError::Storage),
            };
            directory = next;
        }
        Ok(directory)
    }
    fn selected(directory: &File, path: &OsStr) -> InterestResult<Option<File>> {
        match open_at(directory, path, libc::O_PATH, 0) {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(InterestError::Storage),
        }
    }
    /// 只重开已经由 O_PATH 确认的普通 inode；不再次跟随业务名称。
    fn reopen(selected: &File, write: bool) -> InterestResult<File> {
        let before = selected.metadata().map_err(storage)?;
        ordinary(&before)?;
        let file = OpenOptions::new()
            .read(true)
            .write(write)
            .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(format!("/proc/self/fd/{}", selected.as_raw_fd()))
            .map_err(storage)?;
        if !same_contents(&before, &file.metadata().map_err(storage)?) {
            return Err(InterestError::CorruptState);
        }
        Ok(file)
    }
    impl Archive {
        pub(super) fn open(directory: &Path) -> InterestResult<Self> {
            let path = if directory.is_absolute() {
                directory.to_path_buf()
            } else {
                std::env::current_dir().map_err(storage)?.join(directory)
            };
            let directory = directory_at(&path, true)?;
            let lock = match open_at(
                &directory,
                OsStr::new("archive.lock"),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                0o600,
            ) {
                Ok(file) => {
                    file.sync_all().map_err(storage)?;
                    directory.sync_all().map_err(storage)?;
                    file
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let selected = selected(&directory, OsStr::new("archive.lock"))?
                        .ok_or(InterestError::CorruptState)?;
                    reopen(&selected, true)?
                }
                Err(_) => return Err(InterestError::Storage),
            };
            ordinary(&lock.metadata().map_err(storage)?)?;
            lock.try_lock().map_err(|_| InterestError::Unavailable)?;
            let archive = Self {
                path,
                directory,
                lock,
                operation: Mutex::new(()),
            };
            archive.check_binding()?;
            Ok(archive)
        }
        fn check_binding(&self) -> InterestResult<()> {
            let current = directory_at(&self.path, false)?;
            if !same_identity(
                &self.directory.metadata().map_err(storage)?,
                &current.metadata().map_err(storage)?,
            ) {
                return Err(InterestError::CorruptState);
            }
            let lock = selected(&self.directory, OsStr::new("archive.lock"))?
                .ok_or(InterestError::CorruptState)?;
            let current = lock.metadata().map_err(storage)?;
            ordinary(&current)?;
            if !same_identity(&self.lock.metadata().map_err(storage)?, &current) {
                return Err(InterestError::CorruptState);
            }
            Ok(())
        }
        pub(super) fn is_empty(&self) -> InterestResult<bool> {
            let _operation = self
                .operation
                .lock()
                .map_err(|_| InterestError::Unavailable)?;
            self.check_binding()?;
            let mut empty = true;
            for entry in fs::read_dir(format!("/proc/self/fd/{}", self.directory.as_raw_fd()))
                .map_err(storage)?
            {
                let entry = entry.map_err(storage)?;
                if entry.file_name() != OsStr::new("archive.lock") {
                    // 孤儿、未知条目和特殊类型同样证明有旧现场；不读取正文、不删除。
                    empty = false;
                    break;
                }
            }
            self.check_binding()?;
            Ok(empty)
        }
        fn read_body(&self, hash: &str, sync: bool) -> InterestResult<Vec<u8>> {
            let path = format!("{hash}.json");
            let source =
                selected(&self.directory, OsStr::new(&path))?.ok_or(InterestError::Storage)?;
            let before = source.metadata().map_err(storage)?;
            ordinary(&before)?;
            if before.len() > MAX_JOB_ARCHIVE_RECORD_BYTES as u64 {
                return Err(InterestError::CorruptState);
            }
            let mut file = reopen(&source, false)?;
            let mut bytes = Vec::new();
            (&mut file)
                .take(MAX_JOB_ARCHIVE_RECORD_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(storage)?;
            let current =
                selected(&self.directory, OsStr::new(&path))?.ok_or(InterestError::CorruptState)?;
            if bytes.len() as u64 != before.len()
                || !same_contents(&before, &file.metadata().map_err(storage)?)
                || !same_contents(&before, &current.metadata().map_err(storage)?)
                || key(&bytes) != hash
            {
                return Err(InterestError::CorruptState);
            }
            if sync {
                file.sync_all().map_err(storage)?;
            }
            Ok(bytes)
        }
        fn capacity(&self, bytes: usize) -> InterestResult<()> {
            let mut count = 1usize; // 本次临时正文与最终名字共用一个文件名槽位。
            let mut used = bytes as u64;
            for entry in fs::read_dir(format!("/proc/self/fd/{}", self.directory.as_raw_fd()))
                .map_err(storage)?
            {
                let entry = entry.map_err(storage)?;
                let file = selected(&self.directory, &entry.file_name())?
                    .ok_or(InterestError::CorruptState)?;
                let meta = file.metadata().map_err(storage)?;
                // 标准块、锁、未知名字和先前提交未知的临时原件全计数；不预支清理空间。
                count = count.checked_add(1).ok_or(InterestError::LimitReached)?;
                used = used
                    .checked_add(meta.len())
                    .ok_or(InterestError::LimitReached)?;
                if count > MAX_ARCHIVE_FILES || used > MAX_ARCHIVE_BYTES {
                    return Err(InterestError::LimitReached);
                }
                // 平面归档不跟随链接或递归未知目录，以拒写保留现场，避免漏计隐藏空间。
                ordinary(&meta)?;
            }
            Ok(())
        }
        fn write_temp(&self, bytes: &[u8]) -> InterestResult<(CString, File)> {
            for _ in 0..16 {
                let mut random = [0u8; 16];
                SystemRandom::new()
                    .fill(&mut random)
                    .map_err(|_| InterestError::Unavailable)?;
                let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
                let filename = format!(".pending-{suffix}");
                let mut file = match open_at(
                    &self.directory,
                    OsStr::new(&filename),
                    libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                    0o600,
                ) {
                    Ok(file) => file,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(_) => return Err(InterestError::Storage),
                };
                // 提交失败留下这一唯一原件；没有在错误路径自动清理孤儿的行为。
                file.write_all(bytes).map_err(storage)?;
                file.sync_all().map_err(storage)?;
                return Ok((
                    CString::new(filename).map_err(|_| InterestError::Storage)?,
                    file,
                ));
            }
            Err(InterestError::Unavailable)
        }
        pub(super) fn put(&self, hash: &str, bytes: &[u8]) -> InterestResult<()> {
            let _operation = self
                .operation
                .lock()
                .map_err(|_| InterestError::Unavailable)?;
            self.check_binding()?;
            let target = format!("{hash}.json");
            if selected(&self.directory, OsStr::new(&target))?.is_some() {
                if self.read_body(hash, true)? != bytes {
                    return Err(InterestError::Conflict);
                }
                self.check_binding()?;
                self.directory.sync_all().map_err(storage)?;
                return Ok(());
            }
            self.capacity(bytes.len())?;
            let (temporary, file) = self.write_temp(bytes)?;
            let temp = selected(&self.directory, OsStr::from_bytes(temporary.as_bytes()))?
                .ok_or(InterestError::CorruptState)?;
            if !same_contents(
                &file.metadata().map_err(storage)?,
                &temp.metadata().map_err(storage)?,
            ) {
                return Err(InterestError::CorruptState);
            }
            self.check_binding()?;
            let destination = CString::new(target).map_err(|_| InterestError::InvalidInput)?;
            // SAFETY: 两个名字均为本目录的固定组件；NOREPLACE 保证绝不覆盖已有原件。
            let renamed = unsafe {
                libc::renameat2(
                    self.directory.as_raw_fd(),
                    temporary.as_ptr(),
                    self.directory.as_raw_fd(),
                    destination.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if renamed < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(InterestError::Storage);
            }
            // EEXIST 时也只核对既有原件，保留本次临时文件；完整相同才可幂等返回。
            if self.read_body(hash, true)? != bytes {
                return Err(InterestError::Conflict);
            }
            self.directory.sync_all().map_err(storage)?;
            self.check_binding()?;
            Ok(())
        }
        pub(super) fn get(&self, hash: &str) -> InterestResult<Vec<u8>> {
            let _operation = self
                .operation
                .lock()
                .map_err(|_| InterestError::Unavailable)?;
            self.check_binding()?;
            let bytes = self.read_body(hash, false)?;
            self.check_binding()?;
            Ok(bytes)
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{MetadataExt, symlink},
    };

    #[test]
    fn original_bytes_survive_idempotence_reopen_and_conflicting_put() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("jobs");
        let bytes = b"private original interaction evidence";
        let hash = key(bytes);
        let archive = FileInterestJobArchive::open(&directory).unwrap();
        assert!(archive.is_empty().unwrap());
        archive.put(&hash, bytes).unwrap();
        assert!(!archive.is_empty().unwrap());
        let path = directory.join(format!("{hash}.json"));
        let original = fs::metadata(&path).unwrap();
        archive.put(&hash, bytes).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().ino(), original.ino());
        assert_eq!(archive.get(&hash).unwrap(), bytes);
        assert_eq!(
            archive.put(&hash, b"different bytes"),
            Err(InterestError::Conflict)
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(
            FileInterestJobArchive::open(&directory).err(),
            Some(InterestError::Unavailable)
        );
        drop(archive);
        let reopened = FileInterestJobArchive::open(&directory).unwrap();
        assert!(!reopened.is_empty().unwrap());
        assert_eq!(reopened.get(&hash).unwrap(), bytes);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
    }

    #[test]
    fn missing_root_detection_counts_orphans_unknown_directories_and_links_without_deleting() {
        let root = tempfile::tempdir().unwrap();
        for kind in ["orphan", "directory", "symlink"] {
            let directory = root.path().join(kind);
            let archive = FileInterestJobArchive::open(&directory).unwrap();
            assert!(archive.is_empty().unwrap());
            let leftover = directory.join("retained-unknown");
            match kind {
                "orphan" => fs::write(&leftover, b"original unreferenced bytes").unwrap(),
                "directory" => fs::create_dir(&leftover).unwrap(),
                _ => symlink("missing-target", &leftover).unwrap(),
            }
            assert!(!archive.is_empty().unwrap());
            assert!(fs::symlink_metadata(&leftover).is_ok());
            assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
            drop(archive);
            let reopened = FileInterestJobArchive::open(&directory).unwrap();
            assert!(!reopened.is_empty().unwrap());
            assert!(fs::symlink_metadata(&leftover).is_ok());
        }
    }

    #[test]
    fn missing_corrupt_oversized_and_linked_originals_fail_without_repair() {
        let root = tempfile::tempdir().unwrap();
        let archive = FileInterestJobArchive::open(root.path()).unwrap();
        let hash = key(b"original");
        assert_eq!(archive.get(&hash), Err(InterestError::Storage));
        assert_eq!(
            archive.get("../arbitrary"),
            Err(InterestError::InvalidInput)
        );
        let path = root.path().join(format!("{hash}.json"));
        fs::write(&path, b"corrupt").unwrap();
        assert_eq!(archive.get(&hash), Err(InterestError::CorruptState));
        assert_eq!(
            archive.put(&hash, b"original"),
            Err(InterestError::CorruptState)
        );
        assert_eq!(fs::read(&path).unwrap(), b"corrupt");
        fs::write(&path, vec![0u8; MAX_JOB_ARCHIVE_RECORD_BYTES + 1]).unwrap();
        assert_eq!(archive.get(&hash), Err(InterestError::CorruptState));
        fs::remove_file(&path).unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), b"original").unwrap();
        symlink(outside.path(), &path).unwrap();
        assert_eq!(archive.get(&hash), Err(InterestError::CorruptState));
        assert_eq!(
            archive.put(&hash, b"original"),
            Err(InterestError::CorruptState)
        );
        fs::remove_file(&path).unwrap();
        fs::hard_link(outside.path(), &path).unwrap();
        assert_eq!(archive.get(&hash), Err(InterestError::CorruptState));
        assert_eq!(fs::read(outside.path()).unwrap(), b"original");
    }

    #[test]
    fn lock_and_directory_symlinks_or_replacements_cannot_redirect_archive() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("linked")).unwrap();
        assert!(FileInterestJobArchive::open(root.path().join("linked")).is_err());
        let lock_target = outside.path().join("lock-original");
        fs::write(&lock_target, b"unchanged").unwrap();
        let linked_lock = root.path().join("linked-lock");
        fs::create_dir(&linked_lock).unwrap();
        symlink(&lock_target, linked_lock.join("archive.lock")).unwrap();
        assert!(FileInterestJobArchive::open(&linked_lock).is_err());
        assert_eq!(fs::read(&lock_target).unwrap(), b"unchanged");
        let directory = root.path().join("archive");
        let archive = FileInterestJobArchive::open(&directory).unwrap();
        archive.put(&key(b"original"), b"original").unwrap();
        fs::rename(&directory, root.path().join("retained")).unwrap();
        fs::create_dir(&directory).unwrap();
        assert!(archive.get(&key(b"original")).is_err());
        assert!(archive.put(&key(b"new"), b"new").is_err());
        assert_eq!(
            fs::read(
                root.path()
                    .join("retained")
                    .join(format!("{}.json", key(b"original")))
            )
            .unwrap(),
            b"original"
        );
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
    }

    #[test]
    fn physical_orphans_count_toward_bytes_and_files_but_do_not_hide_existing_history() {
        let root = tempfile::tempdir().unwrap();
        let archive = FileInterestJobArchive::open(root.path()).unwrap();
        archive.put(&key(b"saved"), b"saved").unwrap();
        let orphan = root.path().join("unknown-orphan");
        fs::File::create(&orphan)
            .unwrap()
            .set_len(MAX_ARCHIVE_BYTES)
            .unwrap();
        assert_eq!(
            archive.put(&key(b"new"), b"new"),
            Err(InterestError::LimitReached)
        );
        assert_eq!(archive.get(&key(b"saved")).unwrap(), b"saved");
        archive.put(&key(b"saved"), b"saved").unwrap();
        assert_eq!(fs::metadata(&orphan).unwrap().len(), MAX_ARCHIVE_BYTES);
        fs::remove_file(&orphan).unwrap();
        for index in 2..MAX_ARCHIVE_FILES {
            fs::File::create(root.path().join(format!("orphan-{index}"))).unwrap();
        }
        assert_eq!(
            fs::read_dir(root.path()).unwrap().count(),
            MAX_ARCHIVE_FILES
        );
        assert_eq!(
            archive.put(&key(b"new"), b"new"),
            Err(InterestError::LimitReached)
        );
        assert_eq!(archive.get(&key(b"saved")).unwrap(), b"saved");
        assert_eq!(
            fs::read_dir(root.path()).unwrap().count(),
            MAX_ARCHIVE_FILES
        );
    }

    #[test]
    fn record_boundaries_and_unknown_nested_data_are_not_silently_trimmed() {
        let root = tempfile::tempdir().unwrap();
        let archive = FileInterestJobArchive::open(root.path()).unwrap();
        let maximum = vec![0u8; MAX_JOB_ARCHIVE_RECORD_BYTES];
        archive.put(&key(&maximum), &maximum).unwrap();
        assert_eq!(archive.get(&key(&maximum)).unwrap(), maximum);
        let too_large = vec![0u8; MAX_JOB_ARCHIVE_RECORD_BYTES + 1];
        assert_eq!(
            archive.put(&key(&too_large), &too_large),
            Err(InterestError::LimitReached)
        );
        fs::create_dir(root.path().join("unknown-nested")).unwrap();
        fs::write(root.path().join("unknown-nested/original"), b"retained").unwrap();
        assert_eq!(
            archive.put(&key(b"new"), b"new"),
            Err(InterestError::CorruptState)
        );
        assert_eq!(
            fs::read(root.path().join("unknown-nested/original")).unwrap(),
            b"retained"
        );
    }
}
