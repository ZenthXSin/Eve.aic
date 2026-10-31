use eve_context_api::*;
use ring::digest::{SHA256, digest};
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
pub struct FileContextArchive {
    directory: PathBuf,
}
impl FileContextArchive {
    pub fn open(directory: impl AsRef<Path>) -> ContextResult<Self> {
        std::fs::create_dir_all(&directory).map_err(|_| ContextError::Storage)?;
        Ok(Self {
            directory: directory
                .as_ref()
                .canonicalize()
                .map_err(|_| ContextError::Storage)?,
        })
    }
    fn path(&self, key: &str) -> ContextResult<PathBuf> {
        if !valid(key) {
            return Err(ContextError::InvalidInput);
        }
        Ok(self.directory.join(format!("{key}.json")))
    }
}
impl ContextArchiveStore for FileContextArchive {
    fn get(&self, hash: &str) -> ContextResult<Vec<u8>> {
        let path = self.path(hash)?;
        let meta = path.symlink_metadata().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ContextError::CorruptState
            } else {
                ContextError::Storage
            }
        })?;
        if !meta.is_file() || meta.len() > MAX_CONTEXT_BODY_BYTES as u64 {
            return Err(ContextError::CorruptState);
        }
        let mut bytes = Vec::new();
        File::open(path)
            .map_err(|_| ContextError::Storage)?
            .take((MAX_CONTEXT_BODY_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| ContextError::Storage)?;
        if bytes.len() > MAX_CONTEXT_BODY_BYTES || key(&bytes) != hash {
            return Err(ContextError::CorruptState);
        }
        Ok(bytes)
    }
    fn put(&self, hash: &str, bytes: &[u8]) -> ContextResult<()> {
        let target = self.path(hash)?;
        if bytes.len() > MAX_CONTEXT_BODY_BYTES || key(bytes) != hash {
            return Err(ContextError::InvalidInput);
        }
        if target.symlink_metadata().is_ok() {
            self.get(hash)?;
            // Windows 的 FlushFileBuffers 要求写句柄；仅同步，绝不改写已验证的正文。
            std::fs::OpenOptions::new()
                .read(true)
                .write(cfg!(windows))
                .open(&target)
                .and_then(|f| f.sync_all())
                .map_err(|_| ContextError::Storage)?;
            #[cfg(unix)]
            File::open(&self.directory)
                .and_then(|f| f.sync_all())
                .map_err(|_| ContextError::Storage)?;
            return Ok(());
        }
        let mut temp =
            tempfile::NamedTempFile::new_in(&self.directory).map_err(|_| ContextError::Storage)?;
        temp.write_all(bytes).map_err(|_| ContextError::Storage)?;
        temp.as_file()
            .sync_all()
            .map_err(|_| ContextError::Storage)?;
        match temp.persist_noclobber(&target) {
            Ok(_) => {}
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                self.get(hash)?;
            }
            Err(_) => return Err(ContextError::Storage),
        }
        #[cfg(unix)]
        File::open(&self.directory)
            .and_then(|f| f.sync_all())
            .map_err(|_| ContextError::Storage)?;
        Ok(())
    }
}
