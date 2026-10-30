use eve_learning_api::*;
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
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
fn valid(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
/// 每个调用重新读取并校验；不将冷正文缓存在该对象中，不提供删除入口。
pub struct FileLearningArchive {
    directory: PathBuf,
}
impl FileLearningArchive {
    pub fn open(directory: impl AsRef<Path>) -> LearningResult<Self> {
        std::fs::create_dir_all(&directory).map_err(|_| LearningError::Storage)?;
        Ok(Self {
            directory: directory
                .as_ref()
                .canonicalize()
                .map_err(|_| LearningError::Storage)?,
        })
    }
    fn path(&self, key: &str) -> LearningResult<PathBuf> {
        if !valid(key) {
            return Err(LearningError::InvalidInput);
        }
        Ok(self.directory.join(format!("{key}.json")))
    }
}
impl LearningArchiveStore for FileLearningArchive {
    fn get(&self, hash: &str) -> LearningResult<Vec<u8>> {
        let path = self.path(hash)?;
        let meta = path
            .symlink_metadata()
            .map_err(|_| LearningError::Storage)?;
        if !meta.is_file() || meta.len() > MAX_ARCHIVE_RECORD_BYTES as u64 {
            return Err(LearningError::CorruptState);
        }
        let mut bytes = Vec::new();
        File::open(path)
            .map_err(|_| LearningError::Storage)?
            .take((MAX_ARCHIVE_RECORD_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| LearningError::Storage)?;
        if bytes.len() > MAX_ARCHIVE_RECORD_BYTES || key(&bytes) != hash {
            return Err(LearningError::CorruptState);
        }
        Ok(bytes)
    }
    fn put(&self, hash: &str, bytes: &[u8]) -> LearningResult<()> {
        let target = self.path(hash)?;
        if bytes.len() > MAX_ARCHIVE_RECORD_BYTES || key(bytes) != hash {
            return Err(LearningError::InvalidInput);
        }
        if target.symlink_metadata().is_ok() {
            self.get(hash)?;
            File::open(&target)
                .and_then(|f| f.sync_all())
                .map_err(|_| LearningError::Storage)?;
            #[cfg(unix)]
            File::open(&self.directory)
                .and_then(|f| f.sync_all())
                .map_err(|_| LearningError::Storage)?;
            return Ok(());
        }
        let mut temp =
            tempfile::NamedTempFile::new_in(&self.directory).map_err(|_| LearningError::Storage)?;
        temp.write_all(bytes).map_err(|_| LearningError::Storage)?;
        temp.as_file()
            .sync_all()
            .map_err(|_| LearningError::Storage)?;
        match temp.persist_noclobber(&target) {
            Ok(_) => {}
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                self.get(hash)?;
            }
            Err(_) => return Err(LearningError::Storage),
        }
        #[cfg(unix)]
        File::open(&self.directory)
            .and_then(|f| f.sync_all())
            .map_err(|_| LearningError::Storage)?;
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordRef {
    pub hash: String,
    pub bytes: usize,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Index {
    pub format_version: u32,
    pub kind: String,
    pub records: Vec<RecordRef>,
}
pub(crate) fn load_value(
    store: &dyn LearningArchiveStore,
    record: &RecordRef,
) -> LearningResult<serde_json::Value> {
    if !valid(&record.hash) || record.bytes > MAX_ARCHIVE_RECORD_BYTES {
        return Err(LearningError::CorruptState);
    }
    let bytes = store.get(&record.hash)?;
    if bytes.len() != record.bytes || key(&bytes) != record.hash {
        return Err(LearningError::CorruptState);
    }
    crate::strict_json::from_slice(&bytes).map_err(|_| LearningError::CorruptState)
}
pub(crate) fn freeze<T: Serialize>(
    store: &dyn LearningArchiveStore,
    kind: &str,
    records: &[T],
) -> LearningResult<(Vec<u8>, Index)> {
    let mut index = Index {
        format_version: 2,
        kind: kind.into(),
        records: vec![],
    };
    let mut total = 0usize;
    for record in records {
        let bytes = serde_json::to_vec(record).map_err(|_| LearningError::InvalidInput)?;
        if bytes.len() > MAX_ARCHIVE_RECORD_BYTES {
            return Err(LearningError::LimitReached);
        }
        total = total
            .checked_add(bytes.len())
            .ok_or(LearningError::LimitReached)?;
        if total > MAX_ARCHIVE_RESTORE_BYTES {
            return Err(LearningError::LimitReached);
        }
        let hash = key(&bytes);
        store.put(&hash, &bytes)?;
        index.records.push(RecordRef {
            hash,
            bytes: bytes.len(),
        });
    }
    let bytes = serde_json::to_vec(&index).map_err(|_| LearningError::InvalidInput)?;
    if bytes.len() > MAX_STATE_BYTES {
        return Err(LearningError::LimitReached);
    }
    Ok((bytes, index))
}
pub(crate) fn thaw<T: serde::de::DeserializeOwned>(
    store: &dyn LearningArchiveStore,
    index: &Index,
    kind: &str,
) -> LearningResult<Vec<T>> {
    if index.format_version != 2 || index.kind != kind {
        return Err(LearningError::CorruptState);
    }
    let total = index
        .records
        .iter()
        .try_fold(0usize, |total, record| total.checked_add(record.bytes))
        .ok_or(LearningError::LimitReached)?;
    if total > MAX_ARCHIVE_RESTORE_BYTES {
        return Err(LearningError::LimitReached);
    }
    index
        .records
        .iter()
        .map(|r| {
            if !valid(&r.hash) || r.bytes > MAX_ARCHIVE_RECORD_BYTES {
                return Err(LearningError::CorruptState);
            }
            let bytes = store.get(&r.hash)?;
            if bytes.len() != r.bytes || key(&bytes) != r.hash {
                return Err(LearningError::CorruptState);
            }
            serde_json::from_value(
                crate::strict_json::from_slice(&bytes).map_err(|_| LearningError::CorruptState)?,
            )
            .map_err(|_| LearningError::CorruptState)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    struct NoReads;
    impl LearningArchiveStore for NoReads {
        fn get(&self, _: &str) -> LearningResult<Vec<u8>> {
            panic!("oversized index must fail before reading")
        }
        fn put(&self, _: &str, _: &[u8]) -> LearningResult<()> {
            panic!("read check must not write")
        }
    }
    #[test]
    fn restore_budget_and_wrong_kind_fail_before_storage_access() {
        let index = Index {
            format_version: 2,
            kind: "jobs".into(),
            records: vec![RecordRef {
                hash: "0".repeat(64),
                bytes: MAX_ARCHIVE_RESTORE_BYTES + 1,
            }],
        };
        assert!(matches!(
            thaw::<serde_json::Value>(&NoReads, &index, "jobs"),
            Err(LearningError::LimitReached)
        ));
        assert!(matches!(
            thaw::<serde_json::Value>(&NoReads, &index, "decisions"),
            Err(LearningError::CorruptState)
        ));
    }
    #[test]
    fn immutable_files_are_idempotent_and_tampering_never_gets_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let store = FileLearningArchive::open(root.path()).unwrap();
        let data = b"{\"value\":1}";
        let hash = key(data);
        store.put(&hash, data).unwrap();
        store.put(&hash, data).unwrap();
        assert_eq!(store.get(&hash).unwrap(), data);
        assert!(store.get("../outside").is_err());
        let path = store.path(&hash).unwrap();
        std::fs::write(&path, b"damaged").unwrap();
        assert!(store.get(&hash).is_err());
        assert!(store.put(&hash, data).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"damaged");
    }
    #[cfg(unix)]
    #[test]
    fn symbolic_link_record_is_not_followed() {
        let root = tempfile::tempdir().unwrap();
        let store = FileLearningArchive::open(root.path().join("records")).unwrap();
        let bytes = b"{}";
        let hash = key(bytes);
        let outside = root.path().join("outside.json");
        std::fs::write(&outside, bytes).unwrap();
        std::os::unix::fs::symlink(&outside, store.path(&hash).unwrap()).unwrap();
        assert!(store.get(&hash).is_err());
        assert!(store.put(&hash, bytes).is_err());
        assert_eq!(std::fs::read(outside).unwrap(), bytes);
    }
}
