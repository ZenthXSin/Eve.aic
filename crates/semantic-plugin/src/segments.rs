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
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

/// 每段覆盖的已归档证据条数；与记忆归档的单页上限一致。
pub const SEGMENT_EVIDENCE: usize = 64;
const FORMAT_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DECODED_BYTES: u64 = 32 * 1024 * 1024;
/// 解开后缓存的分段数上限；按最近使用淘汰，只影响读取速度。
const CACHED_SEGMENTS: usize = 256;

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

#[derive(Default)]
struct Cache {
    entries: BTreeMap<String, (Arc<Vec<IndexEntry>>, u64)>,
    tick: u64,
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
        Ok(self
            .names
            .lock()
            .map_err(|_| SegmentError::Storage)?
            .iter()
            .any(|name| name.starts_with(&prefix)))
    }

    /// 本作用域、本模型已写出的分段条目；每段都逐字节核对。同一区间有多段时只取内容摘要与 `wanted` 一致的。
    pub fn load(
        &self,
        profile: &EmbeddingProfile,
        scope: &str,
        wanted: &[SegmentKey],
    ) -> Result<Vec<Arc<Vec<IndexEntry>>>, SegmentError> {
        let names = self
            .names
            .lock()
            .map_err(|_| SegmentError::Storage)?
            .clone();
        let mut loaded = Vec::new();
        for key in wanted {
            if key.scope != scope || key.profile != profile_key(profile) {
                return Err(SegmentError::InvalidInput);
            }
            let prefix = key.prefix();
            let Some(name) = names.iter().find(|name| name.starts_with(&prefix)) else {
                continue;
            };
            loaded.push(self.read(name, key, profile)?);
        }
        Ok(loaded)
    }

    fn read(
        &self,
        name: &str,
        key: &SegmentKey,
        profile: &EmbeddingProfile,
    ) -> Result<Arc<Vec<IndexEntry>>, SegmentError> {
        if let Ok(mut cache) = self.cache.lock() {
            cache.tick += 1;
            let tick = cache.tick;
            if let Some((entries, used)) = cache.entries.get_mut(name) {
                *used = tick;
                return Ok(entries.clone());
            }
        }
        let path = self.directory.join(name);
        let metadata = fs::symlink_metadata(&path).map_err(|_| SegmentError::Storage)?;
        if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
            return Err(SegmentError::CorruptState);
        }
        let bytes = fs::read(&path).map_err(|_| SegmentError::Storage)?;
        let expected = name
            .strip_suffix(".json.zz")
            .and_then(|rest| rest.rsplit('-').next())
            .ok_or(SegmentError::CorruptState)?;
        if hex(digest(&SHA256, &bytes).as_ref()) != expected {
            return Err(SegmentError::CorruptState);
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
            let tick = cache.tick;
            cache.entries.insert(name.into(), (entries.clone(), tick));
            while cache.entries.len() > CACHED_SEGMENTS {
                let oldest = cache
                    .entries
                    .iter()
                    .min_by_key(|(_, (_, used))| *used)
                    .map(|(name, _)| name.clone());
                if let Some(name) = oldest {
                    cache.entries.remove(&name);
                }
            }
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
        if target.exists() {
            return if fs::read(&target).map_err(|_| SegmentError::Storage)? == bytes {
                Ok(())
            } else {
                Err(SegmentError::CorruptState)
            };
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |value| value.as_millis());
        let temporary = self
            .directory
            .join(format!(".{name}.{}-{stamp}.tmp", std::process::id()));
        let written = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &target)?;
            sync_directory(&self.directory)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temporary);
            return Err(SegmentError::Storage);
        }
        let mut names = self.names.lock().map_err(|_| SegmentError::Storage)?;
        names.push(name);
        names.sort();
        Ok(())
    }
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> std::io::Result<()> {
    fs::File::open(directory)?.sync_all()
}
#[cfg(not(unix))]
fn sync_directory(_: &Path) -> std::io::Result<()> {
    Ok(())
}
