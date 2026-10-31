//! 归档记忆的向量分段：每段对应一个作用域里修订连续的 64 条已归档证据，按模型与维度隔离。
//!
//! 归档证据不可变，分段一旦写出也不再改变；热层与尚未凑满一段的归档证据仍在 `index.v1`。
//! 分段文件是 zlib 压缩的 JSON。文件名带内容摘要（条目键与正文摘要）与文件字节的 SHA-256：
//! 判断某段是否已建索引只需列目录；读取时逐字节核对、解开并逐项核对，不一致即报告损坏，不跳过。
//! 分段是可由记忆重新计算的派生数据，但不自动删除或覆盖。
use crate::{store::IndexEntry, strict_json};
use eve_semantic_api::EmbeddingProfile;
use ring::digest::{Context, SHA256, digest};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// 每段覆盖的已归档证据条数；与记忆归档的单页上限一致。
pub const SEGMENT_EVIDENCE: usize = 64;
const FORMAT_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DECODED_BYTES: u64 = 32 * 1024 * 1024;
/// 解开后缓存的分段数上限；按最近使用淘汰，只影响读取速度。
const CACHED_SEGMENTS: usize = 256;
const CACHED_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentError {
    InvalidInput,
    Storage,
    CorruptState,
}

/// 一段在目录里的定位：模型、作用域、修订区间与内容摘要。
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SegmentKey {
    pub profile: String,
    pub scope: String,
    pub first_revision: u64,
    pub last_revision: u64,
    pub content: String,
}
impl SegmentKey {
    fn prefix(&self) -> String {
        format!(
            "vectors-{}-{}-{:010}-{:010}-{}-",
            self.profile, self.scope, self.first_revision, self.last_revision, self.content
        )
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SegmentFile {
    format_version: u32,
    model: String,
    dimensions: u32,
    scope: String,
    first_revision: u64,
    last_revision: u64,
    entries: Vec<IndexEntry>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 模型与维度的短摘要，用于文件名。
pub fn profile_key(profile: &EmbeddingProfile) -> String {
    let mut context = Context::new(&SHA256);
    for part in [profile.model.as_bytes(), &profile.dimensions.to_be_bytes()] {
        context.update(&(part.len() as u64).to_be_bytes());
        context.update(part);
    }
    hex(&context.finish().as_ref()[..8])
}

/// 一段应包含的条目（条目键与正文摘要）的摘要；条目按键排序后计算。
pub fn content_key(items: &[(String, String)]) -> String {
    let mut sorted: Vec<_> = items.iter().collect();
    sorted.sort();
    let mut context = Context::new(&SHA256);
    for (item, sha) in sorted {
        for part in [item.as_bytes(), sha.as_bytes()] {
            context.update(&(part.len() as u64).to_be_bytes());
            context.update(part);
        }
    }
    hex(&context.finish().as_ref()[..8])
}

fn valid_name(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix("vectors-")
        .and_then(|rest| rest.strip_suffix(".json.zz"))
    else {
        return false;
    };
    let parts: Vec<&str> = rest.split('-').collect();
    let hexed = |value: &str, len: usize| {
        value.len() == len
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    parts.len() == 6
        && hexed(parts[0], 16)
        && hexed(parts[1], 64)
        && parts[2].len() == 10
        && parts[3].len() == 10
        && parts[2..4]
            .iter()
            .all(|part| part.bytes().all(|b| b.is_ascii_digit()))
        && hexed(parts[4], 16)
        && hexed(parts[5], 64)
}

struct CachedSegment {
    entries: Arc<Vec<IndexEntry>>,
    profile: EmbeddingProfile,
    key: SegmentKey,
    used: u64,
    bytes: usize,
}
struct Cache {
    entries: BTreeMap<String, CachedSegment>,
    tick: u64,
    bytes: usize,
    budget: usize,
}
impl Default for Cache {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            tick: 0,
            bytes: 0,
            budget: CACHED_PAYLOAD_BYTES,
        }
    }
}
impl Cache {
    fn remove(&mut self, name: &str) {
        if let Some(entry) = self.entries.remove(name) {
            self.bytes -= entry.bytes;
        }
    }
    fn insert(
        &mut self,
        name: &str,
        profile: &EmbeddingProfile,
        key: &SegmentKey,
        entries: Arc<Vec<IndexEntry>>,
    ) {
        let name = name.to_owned();
        let mut entry = CachedSegment {
            entries,
            profile: profile.clone(),
            key: key.clone(),
            used: self.tick,
            bytes: 0,
        };
        // 计入 Vec 和字符串的实际 capacity；BTreeMap 节点及外部在途 Arc 不属于载荷预算。
        entry.bytes = name.capacity()
            + entry.profile.model.capacity()
            + entry.key.profile.capacity()
            + entry.key.scope.capacity()
            + entry.key.content.capacity()
            + std::mem::size_of::<Vec<IndexEntry>>()
            + 2 * std::mem::size_of::<usize>()
            + entry.entries.capacity() * std::mem::size_of::<IndexEntry>()
            + entry
                .entries
                .iter()
                .map(|item| {
                    item.model.capacity()
                        + item.scope.capacity()
                        + item.item.capacity()
                        + item.content_sha256.capacity()
                        + item.vector.capacity()
                })
                .sum::<usize>();
        self.remove(&name);
        if entry.bytes > self.budget {
            return;
        }
        self.bytes += entry.bytes;
        self.entries.insert(name, entry);
        while self.entries.len() > CACHED_SEGMENTS || self.bytes > self.budget {
            let Some(name) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(name, _)| name.clone())
            else {
                break;
            };
            self.remove(&name);
        }
    }
}

fn first_with_prefix<'a>(names: &'a [String], prefix: &str) -> Option<&'a String> {
    let position = names.partition_point(|name| name.as_str() < prefix);
    names.get(position).filter(|name| name.starts_with(prefix))
}

/// 目录中的不可变分段文件；先写临时文件并同步，再原子改名。只接受本模块生成的文件名，不提供删除。
pub struct SegmentStore {
    directory: PathBuf,
    names: Mutex<Vec<String>>,
    cache: Mutex<Cache>,
}

impl SegmentStore {
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, SegmentError> {
        fs::create_dir_all(directory.as_ref()).map_err(|_| SegmentError::Storage)?;
        let directory = directory
            .as_ref()
            .canonicalize()
            .map_err(|_| SegmentError::Storage)?;
        let mut names = Vec::new();
        for entry in fs::read_dir(&directory).map_err(|_| SegmentError::Storage)? {
            let entry = entry.map_err(|_| SegmentError::Storage)?;
            if let Some(name) = entry.file_name().to_str()
                && valid_name(name)
            {
                names.push(name.to_owned());
            }
        }
        names.sort();
        Ok(Self {
            directory,
            names: Mutex::new(names),
            cache: Mutex::new(Cache::default()),
        })
    }

    /// 这一段是否已经写出（按内容摘要匹配，不读文件）。
    pub fn contains(&self, key: &SegmentKey) -> Result<bool, SegmentError> {
        let prefix = key.prefix();
        let names = self.names.lock().map_err(|_| SegmentError::Storage)?;
        Ok(first_with_prefix(&names, &prefix).is_some())
    }

    /// 本作用域、本模型已写出的分段条目；每段都逐字节核对。同一区间有多段时只取内容摘要与 `wanted` 一致的。
    pub fn load(
        &self,
        profile: &EmbeddingProfile,
        scope: &str,
        wanted: &[SegmentKey],
    ) -> Result<Vec<Arc<Vec<IndexEntry>>>, SegmentError> {
        let selected = {
            let names = self.names.lock().map_err(|_| SegmentError::Storage)?;
            let profile = profile_key(profile);
            let mut selected = Vec::new();
            for key in wanted {
                if key.scope != scope || key.profile != profile {
                    return Err(SegmentError::InvalidInput);
                }
                if let Some(name) = first_with_prefix(&names, &key.prefix()) {
                    selected.push((key, name.clone()));
                }
            }
            selected
        };
        // 只复制实际匹配的文件名；读文件前释放目录锁，不克隆整份目录。
        let mut loaded = Vec::with_capacity(selected.len());
        for (key, name) in selected {
            loaded.push(self.read(&name, key, profile)?);
        }
        Ok(loaded)
    }

    fn read(
        &self,
        name: &str,
        key: &SegmentKey,
        profile: &EmbeddingProfile,
    ) -> Result<Arc<Vec<IndexEntry>>, SegmentError> {
        let path = self.directory.join(name);
        let bytes = read_bounded(&path)?;
        let expected = name
            .strip_suffix(".json.zz")
            .and_then(|rest| rest.rsplit('-').next())
            .ok_or(SegmentError::CorruptState)?;
        if hex(digest(&SHA256, &bytes).as_ref()) != expected {
            return Err(SegmentError::CorruptState);
        }
        // 缓存只省去解压与反序列化；每次读仍核对文件类型、字节上限与完整 SHA-256。
        if let Ok(mut cache) = self.cache.lock() {
            cache.tick = cache.tick.saturating_add(1);
            let tick = cache.tick;
            if let Some(entry) = cache.entries.get_mut(name) {
                if entry.profile != *profile || entry.key != *key {
                    return Err(SegmentError::CorruptState);
                }
                entry.used = tick;
                return Ok(entry.entries.clone());
            }
        }
        let mut decoded = Vec::new();
        flate2::read::ZlibDecoder::new(bytes.as_slice())
            .take(MAX_DECODED_BYTES + 1)
            .read_to_end(&mut decoded)
            .map_err(|_| SegmentError::CorruptState)?;
        if decoded.len() as u64 > MAX_DECODED_BYTES {
            return Err(SegmentError::CorruptState);
        }
        let value = strict_json::from_slice(&decoded).map_err(|_| SegmentError::CorruptState)?;
        let file: SegmentFile =
            serde_json::from_value(value).map_err(|_| SegmentError::CorruptState)?;
        let pairs: Vec<(String, String)> = file
            .entries
            .iter()
            .map(|entry| (entry.item.clone(), entry.content_sha256.clone()))
            .collect();
        if file.format_version != FORMAT_VERSION
            || file.model != profile.model
            || file.dimensions != profile.dimensions
            || file.scope != key.scope
            || file.first_revision != key.first_revision
            || file.last_revision != key.last_revision
            || content_key(&pairs) != key.content
            || file.entries.iter().any(|entry| {
                entry.check().is_err() || !entry.belongs_to(profile) || entry.scope != key.scope
            })
        {
            return Err(SegmentError::CorruptState);
        }
        let entries = Arc::new(file.entries);
        if let Ok(mut cache) = self.cache.lock() {
            cache.tick = cache.tick.saturating_add(1);
            cache.insert(name, profile, key, entries.clone());
        }
        Ok(entries)
    }

    /// 写出一段；同名文件已存在时内容必须一致。
    pub fn put(
        &self,
        profile: &EmbeddingProfile,
        key: &SegmentKey,
        entries: Vec<IndexEntry>,
    ) -> Result<(), SegmentError> {
        let pairs: Vec<(String, String)> = entries
            .iter()
            .map(|entry| (entry.item.clone(), entry.content_sha256.clone()))
            .collect();
        if key.profile != profile_key(profile)
            || content_key(&pairs) != key.content
            || entries.iter().any(|entry| {
                entry.check().is_err() || !entry.belongs_to(profile) || entry.scope != key.scope
            })
        {
            return Err(SegmentError::InvalidInput);
        }
        let json = serde_json::to_vec(&SegmentFile {
            format_version: FORMAT_VERSION,
            model: profile.model.clone(),
            dimensions: profile.dimensions,
            scope: key.scope.clone(),
            first_revision: key.first_revision,
            last_revision: key.last_revision,
            entries,
        })
        .map_err(|_| SegmentError::InvalidInput)?;
        if json.len() as u64 > MAX_DECODED_BYTES {
            return Err(SegmentError::InvalidInput);
        }
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder
            .write_all(&json)
            .map_err(|_| SegmentError::InvalidInput)?;
        let bytes = encoder.finish().map_err(|_| SegmentError::InvalidInput)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(SegmentError::InvalidInput);
        }
        let name = format!(
            "{}{}.json.zz",
            key.prefix(),
            hex(digest(&SHA256, &bytes).as_ref())
        );
        let target = self.directory.join(&name);
        if target.symlink_metadata().is_ok() {
            if read_bounded(&target)? != bytes {
                return Err(SegmentError::CorruptState);
            }
        } else {
            let mut temporary = tempfile::NamedTempFile::new_in(&self.directory)
                .map_err(|_| SegmentError::Storage)?;
            temporary
                .write_all(&bytes)
                .map_err(|_| SegmentError::Storage)?;
            temporary
                .as_file()
                .sync_all()
                .map_err(|_| SegmentError::Storage)?;
            match temporary.persist_noclobber(&target) {
                Ok(_) => {}
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if read_bounded(&target)? != bytes {
                        return Err(SegmentError::CorruptState);
                    }
                }
                Err(_) => return Err(SegmentError::Storage),
            }
        }
        // 重复准备也确认正文与目录已同步；Windows FlushFileBuffers 要求写句柄。
        fs::OpenOptions::new()
            .read(true)
            .write(cfg!(windows))
            .open(&target)
            .and_then(|file| file.sync_all())
            .map_err(|_| SegmentError::Storage)?;
        sync_directory(&self.directory).map_err(|_| SegmentError::Storage)?;
        // 另一实例刚创建的相同正文也要进入当前目录视图，不能等重启才可召回。
        let mut names = self.names.lock().map_err(|_| SegmentError::Storage)?;
        if let Err(position) = names.binary_search(&name) {
            names.insert(position, name);
        }
        Ok(())
    }
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, SegmentError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| SegmentError::Storage)?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return Err(SegmentError::CorruptState);
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|_| SegmentError::Storage)?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| SegmentError::Storage)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(SegmentError::CorruptState);
    }
    Ok(bytes)
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> std::io::Result<()> {
    fs::File::open(directory)?.sync_all()
}
#[cfg(not(unix))]
fn sync_directory(_: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(label: &str, first: u64) -> (EmbeddingProfile, SegmentKey, Vec<IndexEntry>) {
        let profile = EmbeddingProfile {
            model: "cache-test".into(),
            dimensions: 2,
        };
        let entry = IndexEntry {
            model: profile.model.clone(),
            dimensions: 2,
            scope: "a".repeat(64),
            item: format!("int:{label}:user"),
            content_sha256: "b".repeat(64),
            scale: 1.0,
            vector: "7f01".into(),
            indexed_at_ms: 1,
        };
        let key = SegmentKey {
            profile: profile_key(&profile),
            scope: entry.scope.clone(),
            first_revision: first,
            last_revision: first + 63,
            content: content_key(&[(entry.item.clone(), entry.content_sha256.clone())]),
        };
        (profile, key, vec![entry])
    }
    fn load(
        store: &SegmentStore,
        profile: &EmbeddingProfile,
        key: &SegmentKey,
    ) -> Arc<Vec<IndexEntry>> {
        store
            .load(profile, &key.scope, std::slice::from_ref(key))
            .unwrap()
            .pop()
            .unwrap()
    }
    #[test]
    fn large_sorted_directory_keeps_first_matching_filename_and_missing_prefix_semantics() {
        let mut names = Vec::new();
        for index in 0..10_000 {
            let (_, key, _) = fixture("a", index * 64 + 1);
            for digest in ["f".repeat(64), "0".repeat(64)] {
                names.push(format!("{}{digest}.json.zz", key.prefix()));
            }
        }
        names.sort();
        for index in [0, 1, 128, 4096, 9999, 10_000] {
            let (_, key, _) = fixture("a", index * 64 + 1);
            let prefix = key.prefix();
            let expected = names.iter().find(|name| name.starts_with(&prefix));
            assert_eq!(first_with_prefix(&names, &prefix), expected);
        }
        assert!(first_with_prefix(&names, "a").is_none());
        assert!(first_with_prefix(&names, "z").is_none());
        assert!(first_with_prefix(&[], "vectors-").is_none());
    }
    #[test]
    fn byte_eviction_keeps_inflight_values_and_reload_uses_immutable_disk_content() {
        let root = tempfile::tempdir().unwrap();
        let store = SegmentStore::open(root.path()).unwrap();
        let (profile, first, first_entries) = fixture("a", 1);
        let (_, second, second_entries) = fixture("b", 65);
        store.put(&profile, &first, first_entries.clone()).unwrap();
        store.put(&profile, &second, second_entries).unwrap();
        let pinned = load(&store, &profile, &first);
        {
            let mut cache = store.cache.lock().unwrap();
            assert_eq!(cache.entries.len(), 1);
            cache.budget = cache.bytes;
        }
        let newer = load(&store, &profile, &second);
        assert_eq!(
            Arc::strong_count(&pinned),
            1,
            "淘汰缓存不能回收在途调用的值"
        );
        assert_eq!(pinned.as_ref(), &first_entries);
        assert!(Arc::ptr_eq(&newer, &load(&store, &profile, &second)));
        let reloaded = load(&store, &profile, &first);
        assert_eq!(reloaded.as_ref(), &first_entries);
        assert!(!Arc::ptr_eq(&pinned, &reloaded));
        let cache = store.cache.lock().unwrap();
        assert!(cache.bytes <= cache.budget);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }
    #[test]
    fn oversized_payload_is_readable_without_displacing_existing_cache() {
        let root = tempfile::tempdir().unwrap();
        let store = SegmentStore::open(root.path()).unwrap();
        let (profile, key, entries) = fixture("a", 1);
        store.put(&profile, &key, entries.clone()).unwrap();
        store.cache.lock().unwrap().budget = 1;
        let first = load(&store, &profile, &key);
        let second = load(&store, &profile, &key);
        assert_eq!(first.as_ref(), &entries);
        assert!(!Arc::ptr_eq(&first, &second));
        assert!(store.cache.lock().unwrap().entries.is_empty());
        assert_eq!(store.cache.lock().unwrap().bytes, 0);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }
    #[test]
    fn warm_cache_never_hides_corrupt_or_removed_files_and_idempotent_put_never_overwrites() {
        let root = tempfile::tempdir().unwrap();
        let store = SegmentStore::open(root.path()).unwrap();
        let (profile, key, entries) = fixture("a", 1);
        store.put(&profile, &key, entries.clone()).unwrap();
        let pinned = load(&store, &profile, &key);
        let path = root.path().join(&store.names.lock().unwrap()[0]);
        fs::write(&path, b"corrupt").unwrap();
        assert_eq!(
            store.load(&profile, &key.scope, std::slice::from_ref(&key)),
            Err(SegmentError::CorruptState)
        );
        assert_eq!(
            store.put(&profile, &key, entries.clone()),
            Err(SegmentError::CorruptState)
        );
        assert_eq!(fs::read(&path).unwrap(), b"corrupt");
        assert_eq!(pinned.as_ref(), &entries);
        fs::remove_file(path).unwrap();
        assert_eq!(
            store.load(&profile, &key.scope, std::slice::from_ref(&key)),
            Err(SegmentError::Storage)
        );
    }
    #[test]
    fn independent_writers_agree_and_existing_file_becomes_visible_without_restart() {
        let root = tempfile::tempdir().unwrap();
        let first = Arc::new(SegmentStore::open(root.path()).unwrap());
        let second = Arc::new(SegmentStore::open(root.path()).unwrap());
        let (profile, key, entries) = fixture("a", 1);
        std::thread::scope(|scope| {
            let one = scope.spawn(|| first.put(&profile, &key, entries.clone()));
            let two = scope.spawn(|| second.put(&profile, &key, entries.clone()));
            one.join().unwrap().unwrap();
            two.join().unwrap().unwrap();
        });
        assert_eq!(load(&first, &profile, &key).as_ref(), &entries);
        assert_eq!(load(&second, &profile, &key).as_ref(), &entries);
        let later = SegmentStore::open(root.path()).unwrap();
        later.put(&profile, &key, entries.clone()).unwrap();
        assert_eq!(load(&later, &profile, &key).as_ref(), &entries);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        assert_eq!(second.names.lock().unwrap().len(), 1);
    }
    #[test]
    fn oversized_disk_file_is_rejected_even_with_a_warm_cache() {
        let root = tempfile::tempdir().unwrap();
        let store = SegmentStore::open(root.path()).unwrap();
        let (profile, key, entries) = fixture("a", 1);
        store.put(&profile, &key, entries).unwrap();
        let _pinned = load(&store, &profile, &key);
        let path = root.path().join(&store.names.lock().unwrap()[0]);
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        assert_eq!(
            store.load(&profile, &key.scope, std::slice::from_ref(&key)),
            Err(SegmentError::CorruptState)
        );
        assert_eq!(path.metadata().unwrap().len(), MAX_FILE_BYTES + 1);
    }
    #[cfg(unix)]
    #[test]
    fn warm_cache_never_bypasses_symlink_rejection() {
        let root = tempfile::tempdir().unwrap();
        let store = SegmentStore::open(root.path()).unwrap();
        let (profile, key, entries) = fixture("a", 1);
        store.put(&profile, &key, entries).unwrap();
        let _pinned = load(&store, &profile, &key);
        let path = root.path().join(&store.names.lock().unwrap()[0]);
        let copy = root.path().join("other");
        fs::rename(&path, &copy).unwrap();
        std::os::unix::fs::symlink(copy, path).unwrap();
        assert_eq!(
            store.load(&profile, &key.scope, std::slice::from_ref(&key)),
            Err(SegmentError::CorruptState)
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod profile {
    use super::*;
    #[test]
    #[ignore = "独立子进程测量真实分段缓存，手动运行"]
    fn semantic_cache_memory_profile() {
        const GROUPS: usize = 128;
        fn profile() -> EmbeddingProfile {
            EmbeddingProfile {
                model: "cache-profile".into(),
                dimensions: 4096,
            }
        }
        fn keys() -> Vec<SegmentKey> {
            let profile = profile();
            (0..GROUPS)
                .map(|group| {
                    let entries = entries(group);
                    SegmentKey {
                        profile: profile_key(&profile),
                        scope: "a".repeat(64),
                        first_revision: group as u64 * 64 + 1,
                        last_revision: group as u64 * 64 + 64,
                        content: content_key(
                            &entries
                                .iter()
                                .map(|entry| (entry.item.clone(), entry.content_sha256.clone()))
                                .collect::<Vec<_>>(),
                        ),
                    }
                })
                .collect()
        }
        fn entries(group: usize) -> Vec<IndexEntry> {
            (0..128)
                .map(|item| IndexEntry {
                    model: "cache-profile".into(),
                    dimensions: 4096,
                    scope: "a".repeat(64),
                    item: format!("int:{group:04}:{item:04}:user"),
                    content_sha256: "b".repeat(64),
                    scale: 1.0,
                    vector: "7f01".repeat(2048),
                    indexed_at_ms: 1,
                })
                .collect()
        }
        if let Some(root) = std::env::var_os("EVE_SEMANTIC_CACHE_PROFILE_ROOT") {
            let mode = std::env::var("EVE_SEMANTIC_CACHE_PROFILE_MODE").unwrap();
            let store = SegmentStore::open(root).unwrap();
            if mode == "legacy" {
                store.cache.lock().unwrap().budget = usize::MAX;
            }
            let keys = keys();
            let mut count = 0;
            for _ in 0..2 {
                for key in &keys {
                    let loaded = store
                        .load(&profile(), &key.scope, std::slice::from_ref(key))
                        .unwrap();
                    assert_eq!(loaded.len(), 1);
                    assert_eq!(loaded[0].len(), 128);
                    for entry in loaded[0].iter() {
                        assert_eq!(entry.vector, "7f01".repeat(2048));
                    }
                    count += loaded[0].len();
                }
            }
            let status = fs::read_to_string("/proc/self/status").unwrap();
            let kib = |name: &str| -> u64 {
                status
                    .lines()
                    .find_map(|line| line.strip_prefix(name))
                    .unwrap()
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap()
            };
            let cache = store.cache.lock().unwrap();
            println!(
                "SEMANTIC_CACHE_PROFILE {}",
                serde_json::json!({"mode":mode,"groups":GROUPS,
                "entries_read":count,"cache_segments":cache.entries.len(),"cache_payload_bytes":cache.bytes,
                "rss_kib":kib("VmRSS:"),"peak_rss_kib":kib("VmHWM:")})
            );
            if mode == "bounded" {
                assert!(cache.bytes <= CACHED_PAYLOAD_BYTES);
            }
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let store = SegmentStore::open(root.path()).unwrap();
        for (group, key) in keys().iter().enumerate() {
            store.put(&profile(), key, entries(group)).unwrap();
        }
        for mode in ["legacy", "bounded"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "segments::profile::semantic_cache_memory_profile",
                    "--ignored",
                    "--nocapture",
                ])
                .env("EVE_SEMANTIC_CACHE_PROFILE_ROOT", root.path())
                .env("EVE_SEMANTIC_CACHE_PROFILE_MODE", mode)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(text.contains("SEMANTIC_CACHE_PROFILE"));
            print!("{text}");
        }
    }
}
