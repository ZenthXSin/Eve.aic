//! 原始证据归档：不可变分块的文件实现、分块格式与选择策略。
use crate::strict_json;
use eve_memory_api::*;
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

const CHUNK_FORMAT_VERSION: u32 = 1;
/// 单个分块文件的硬上限；超过视为损坏。
pub(crate) const MAX_CHUNK_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// 压缩分块解开后的上限；超过视为损坏，防止解压膨胀。
const MAX_DECODED_CHUNK_BYTES: u64 = 4 * 1024 * 1024;

/// 何时把热层较早的完成交互移入分块。默认值面向长期运行的 QQ 实例。
///
/// 只归档未被任何偏好版本引用、且不在作用域最近 `keep_recent` 条之内的完成交互；
/// 未达到 `pressure_*` 时还要求记录早于 `min_age_ms`，给学习候选（7 天有效）留出确认时间。
/// 达到 `pressure_*` 时每个作用域只保留最近 `pressure_keep_recent` 条，避免作用域多时
/// 各自的近期保留量之和就超过热层硬上限。
#[derive(Clone, Debug)]
pub struct ArchivePolicy {
    pub trigger_evidence: usize,
    pub trigger_bytes: usize,
    pub pressure_evidence: usize,
    pub pressure_bytes: usize,
    pub keep_recent: usize,
    pub pressure_keep_recent: usize,
    pub min_age_ms: u64,
    pub min_chunk: usize,
    pub max_chunk_evidence: usize,
    pub max_chunk_bytes: usize,
}

impl Default for ArchivePolicy {
    fn default() -> Self {
        Self {
            trigger_evidence: 1024,
            trigger_bytes: 2 * 1024 * 1024,
            pressure_evidence: MAX_EVIDENCE - 256,
            pressure_bytes: 3 * 1024 * 1024,
            keep_recent: 8,
            pressure_keep_recent: 1,
            min_age_ms: 8 * 24 * 60 * 60 * 1000,
            min_chunk: 16,
            max_chunk_evidence: 256,
            max_chunk_bytes: 1024 * 1024,
        }
    }
}

impl ArchivePolicy {
    pub(crate) fn validate(&self) -> MemoryResult<()> {
        if self.trigger_evidence == 0
            || self.trigger_evidence > self.pressure_evidence
            || self.pressure_evidence > MAX_EVIDENCE
            || self.trigger_bytes > self.pressure_bytes
            || self.pressure_bytes > MAX_STATE_BYTES
            || self.pressure_keep_recent > self.keep_recent
            || self.min_chunk == 0
            || self.min_chunk > self.max_chunk_evidence
            || self.max_chunk_evidence > 4096
            || self.max_chunk_bytes as u64 >= MAX_CHUNK_FILE_BYTES
        {
            return Err(MemoryError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct ArchiveConfig {
    pub store: Arc<dyn EvidenceArchiveStore>,
    pub policy: ArchivePolicy,
}

/// 热层索引中的一个分块：内容摘要、修订段与轮次范围，用于定位与核对。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChunkRecord {
    pub sequence: u64,
    pub sha256: String,
    pub bytes: u64,
    pub count: u64,
    /// 升序、互不相邻重叠的闭区间修订段。
    pub revisions: Vec<[u64; 2]>,
    pub first_turn: u64,
    pub last_turn: u64,
    /// 文件内容是无损压缩（zlib）后的分块 JSON；旧分块为未压缩 JSON，照常读取。
    #[serde(default, skip_serializing_if = "is_false")]
    pub compressed: bool,
}
fn is_false(value: &bool) -> bool {
    !*value
}

impl ChunkRecord {
    pub fn name(&self) -> String {
        chunk_name(self.sequence, &self.sha256, self.compressed)
    }
    pub fn contains_revision(&self, revision: u64) -> bool {
        self.revisions
            .iter()
            .any(|[start, end]| (*start..=*end).contains(&revision))
    }
    pub fn last_revision(&self) -> u64 {
        self.revisions.last().map_or(0, |run| run[1])
    }
    pub fn validate(&self) -> MemoryResult<()> {
        // 与归档写入策略的单块条数上限一致；坏索引不能宣称任意多条历史。
        if self.count == 0
            || self.count > 4096
            || self.revisions.is_empty()
            || self.revisions.len() as u64 > self.count
        {
            return Err(MemoryError::CorruptState);
        }
        let mut total = 0u64;
        let mut previous = 0u64;
        for [start, end] in &self.revisions {
            // 相邻段必须至少隔一个修订，否则应合并为一段。
            if *start == 0
                || start > end
                || (previous > 0 && previous.checked_add(1).is_none_or(|next| *start <= next))
            {
                return Err(MemoryError::CorruptState);
            }
            total = total
                .checked_add(end - start + 1)
                .filter(|total| *total <= self.count)
                .ok_or(MemoryError::CorruptState)?;
            previous = *end;
        }
        if self.count == 0
            || total != self.count
            || self.bytes == 0
            || self.bytes > MAX_CHUNK_FILE_BYTES
            || self.first_turn == 0
            || self.first_turn > self.last_turn
            || !is_sha256(&self.sha256)
        {
            return Err(MemoryError::CorruptState);
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChunkFile {
    format_version: u32,
    sequence: u64,
    scope: MemoryScope,
    evidence: Vec<InteractionEvidence>,
}

pub(crate) fn chunk_name(sequence: u64, sha256: &str, compressed: bool) -> String {
    let suffix = if compressed { ".json.zz" } else { ".json" };
    format!("evidence-{sequence:010}-{sha256}{suffix}")
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn sha256_hex(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 分块内证据的修订段、条数与轮次范围；证据须按修订升序、全是完成交互。
fn metadata(evidence: &[InteractionEvidence]) -> MemoryResult<(u64, Vec<[u64; 2]>, u64, u64)> {
    let mut revisions: Vec<[u64; 2]> = Vec::new();
    let (mut first_turn, mut last_turn) = (u64::MAX, 0);
    for item in evidence {
        let EvidenceSource::CompletedInteraction { turn_id, .. } = item.source else {
            return Err(MemoryError::InvalidInput);
        };
        first_turn = first_turn.min(turn_id);
        last_turn = last_turn.max(turn_id);
        match revisions.last_mut() {
            Some(run) if run[1] + 1 == item.revision => run[1] = item.revision,
            Some(run) if run[1] >= item.revision => return Err(MemoryError::InvalidInput),
            _ => revisions.push([item.revision, item.revision]),
        }
    }
    Ok((evidence.len() as u64, revisions, first_turn, last_turn))
}

/// 编码一组按修订升序的完成交互；返回索引记录与文件内容。
/// `compress` 为真时文件内容是 zlib 无损压缩后的 JSON，原文一字不改。
pub(crate) fn encode_chunk(
    sequence: u64,
    scope: &MemoryScope,
    evidence: Vec<InteractionEvidence>,
    compress: bool,
) -> MemoryResult<(ChunkRecord, Vec<u8>)> {
    let (count, revisions, first_turn, last_turn) = metadata(&evidence)?;
    let json = serde_json::to_vec(&ChunkFile {
        format_version: CHUNK_FORMAT_VERSION,
        sequence,
        scope: scope.clone(),
        evidence,
    })
    .map_err(|_| MemoryError::InvalidInput)?;
    let bytes = if compress {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder
            .write_all(&json)
            .map_err(|_| MemoryError::InvalidInput)?;
        encoder.finish().map_err(|_| MemoryError::InvalidInput)?
    } else {
        json
    };
    let record = ChunkRecord {
        sequence,
        sha256: sha256_hex(&bytes),
        bytes: bytes.len() as u64,
        count,
        revisions,
        first_turn,
        last_turn,
        compressed: compress,
    };
    record.validate().map_err(|_| MemoryError::InvalidInput)?;
    Ok((record, bytes))
}

/// 读取并逐项核对分块；任何不一致都返回 `CorruptState`，不跳过、不修复。
pub(crate) fn load_chunk(
    store: &dyn EvidenceArchiveStore,
    scope: &MemoryScope,
    record: &ChunkRecord,
) -> MemoryResult<Vec<InteractionEvidence>> {
    let bytes = store
        .get(&record.name())
        .map_err(|error| match error {
            MemoryError::CorruptState => MemoryError::CorruptState,
            _ => MemoryError::Storage,
        })?
        .ok_or(MemoryError::CorruptState)?;
    if bytes.len() as u64 != record.bytes || sha256_hex(&bytes) != record.sha256 {
        return Err(MemoryError::CorruptState);
    }
    let bytes = if record.compressed {
        let mut decoded = Vec::new();
        flate2::read::ZlibDecoder::new(bytes.as_slice())
            .take(MAX_DECODED_CHUNK_BYTES + 1)
            .read_to_end(&mut decoded)
            .map_err(|_| MemoryError::CorruptState)?;
        if decoded.len() as u64 > MAX_DECODED_CHUNK_BYTES {
            return Err(MemoryError::CorruptState);
        }
        decoded
    } else {
        bytes
    };
    let value = strict_json::from_slice(&bytes).map_err(|_| MemoryError::CorruptState)?;
    let file: ChunkFile = serde_json::from_value(value).map_err(|_| MemoryError::CorruptState)?;
    if file.format_version != CHUNK_FORMAT_VERSION
        || file.sequence != record.sequence
        || file.scope != *scope
    {
        return Err(MemoryError::CorruptState);
    }
    // 字节已按 SHA-256 核对；再按解开的原文重算索引字段，不依赖压缩器逐字节一致。
    let (count, revisions, first_turn, last_turn) =
        metadata(&file.evidence).map_err(|_| MemoryError::CorruptState)?;
    if (count, &revisions, first_turn, last_turn)
        != (
            record.count,
            &record.revisions,
            record.first_turn,
            record.last_turn,
        )
    {
        return Err(MemoryError::CorruptState);
    }
    for item in &file.evidence {
        crate::store::validate_evidence(item).map_err(|_| MemoryError::CorruptState)?;
    }
    Ok(file.evidence)
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_millis() as u64)
}

/// 目录中的不可变分块文件；先写临时文件并同步，再原子改名。
///
/// 只接受本模块生成的分块名；不提供删除，已存在的同名文件内容必须一致。
pub struct FileEvidenceArchive {
    directory: PathBuf,
    read_only: bool,
}

impl FileEvidenceArchive {
    pub fn open(directory: impl AsRef<Path>) -> MemoryResult<Self> {
        fs::create_dir_all(directory.as_ref()).map_err(|_| MemoryError::Storage)?;
        let directory = directory
            .as_ref()
            .canonicalize()
            .map_err(|_| MemoryError::Storage)?;
        Ok(Self {
            directory,
            read_only: false,
        })
    }

    /// 只读核对用：不创建目录；目录不存在时没有分块，`put` 一律失败。
    pub fn open_read_only(directory: impl AsRef<Path>) -> Self {
        Self {
            directory: directory.as_ref().to_path_buf(),
            read_only: true,
        }
    }

    fn path(&self, name: &str) -> MemoryResult<PathBuf> {
        if !valid_name(name) {
            return Err(MemoryError::InvalidInput);
        }
        Ok(self.directory.join(name))
    }
}

fn valid_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("evidence-").and_then(|rest| {
        rest.strip_suffix(".json.zz")
            .or_else(|| rest.strip_suffix(".json"))
    }) else {
        return false;
    };
    let Some((sequence, sha)) = rest.split_once('-') else {
        return false;
    };
    sequence.len() == 10 && sequence.bytes().all(|b| b.is_ascii_digit()) && is_sha256(sha)
}

impl EvidenceArchiveStore for FileEvidenceArchive {
    fn put(&self, name: &str, bytes: &[u8]) -> MemoryResult<()> {
        let target = self.path(name)?;
        if self.read_only {
            return Err(MemoryError::Storage);
        }
        if let Some(existing) = self.get(name)? {
            return if existing == bytes {
                Ok(())
            } else {
                Err(MemoryError::Conflict)
            };
        }
        let temporary =
            self.directory
                .join(format!(".{name}.{}-{}.tmp", std::process::id(), now_ms()));
        let written = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &target)?;
            sync_directory(&self.directory)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temporary);
            return Err(MemoryError::Storage);
        }
        Ok(())
    }

    fn get(&self, name: &str) -> MemoryResult<Option<Vec<u8>>> {
        let path = self.path(name)?;
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(MemoryError::Storage),
            Ok(metadata) if !metadata.is_file() || metadata.len() > MAX_CHUNK_FILE_BYTES => {
                return Err(MemoryError::CorruptState);
            }
            Ok(_) => {}
        }
        fs::read(&path).map(Some).map_err(|_| MemoryError::Storage)
    }

    fn list(&self) -> MemoryResult<Vec<String>> {
        let mut names = Vec::new();
        let entries = match fs::read_dir(&self.directory) {
            Err(error) if self.read_only && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(names);
            }
            entries => entries.map_err(|_| MemoryError::Storage)?,
        };
        for entry in entries {
            let entry = entry.map_err(|_| MemoryError::Storage)?;
            if let Some(name) = entry.file_name().to_str()
                && valid_name(name)
            {
                names.push(name.to_owned());
            }
        }
        names.sort();
        Ok(names)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, sync::Mutex};

    #[derive(Default)]
    struct Memory(Mutex<BTreeMap<String, Vec<u8>>>);
    impl EvidenceArchiveStore for Memory {
        fn put(&self, name: &str, bytes: &[u8]) -> MemoryResult<()> {
            self.0.lock().unwrap().insert(name.into(), bytes.to_vec());
            Ok(())
        }
        fn get(&self, name: &str) -> MemoryResult<Option<Vec<u8>>> {
            Ok(self.0.lock().unwrap().get(name).cloned())
        }
        fn list(&self) -> MemoryResult<Vec<String>> {
            Ok(self.0.lock().unwrap().keys().cloned().collect())
        }
    }

    fn scope() -> MemoryScope {
        MemoryScope {
            channel: "qq".into(),
            session_id: "s".into(),
            user_id: "u".into(),
        }
    }

    fn evidence(revision: u64) -> InteractionEvidence {
        InteractionEvidence {
            id: format!("e-{revision}"),
            revision,
            at_ms: 1,
            source: EvidenceSource::CompletedInteraction {
                message_id: format!("m-{revision}"),
                session_revision: revision * 2,
                turn_id: revision,
                user_text: format!("第 {revision} 次提问，{}", "重复的原文".repeat(20)),
                assistant_text: "好的。".into(),
            },
        }
    }

    #[test]
    fn compressed_and_plain_chunks_round_trip_losslessly() {
        let store = Memory::default();
        let items: Vec<_> = (1..=5).map(evidence).collect();
        for compress in [false, true] {
            let (record, bytes) = encode_chunk(7, &scope(), items.clone(), compress).unwrap();
            assert_eq!(record.compressed, compress);
            assert_eq!(record.name().ends_with(".json.zz"), compress);
            store.put(&record.name(), &bytes).unwrap();
            assert!(load_chunk(&store, &scope(), &record).unwrap() == items);
        }
        let (plain, _) = encode_chunk(7, &scope(), items.clone(), false).unwrap();
        let (packed, _) = encode_chunk(7, &scope(), items, true).unwrap();
        assert!(packed.bytes < plain.bytes, "压缩后更小");
    }

    #[test]
    fn oversized_decompression_is_corrupt_not_trusted() {
        let store = Memory::default();
        let (mut record, _) = encode_chunk(1, &scope(), vec![evidence(1)], true).unwrap();
        // 解开后超过上限的数据（例如解压膨胀）一律视为损坏。
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        encoder
            .write_all(&vec![b' '; MAX_DECODED_CHUNK_BYTES as usize + 1])
            .unwrap();
        let bomb = encoder.finish().unwrap();
        record.sha256 = sha256_hex(&bomb);
        record.bytes = bomb.len() as u64;
        store.put(&record.name(), &bomb).unwrap();
        assert!(matches!(
            load_chunk(&store, &scope(), &record),
            Err(MemoryError::CorruptState)
        ));
    }
    #[test]
    fn corrupt_revision_ranges_return_error_without_integer_panic() {
        let (mut record, _) = encode_chunk(1, &scope(), vec![evidence(1)], true).unwrap();
        record.revisions = vec![[1, u64::MAX], [1, 1]];
        assert!(record.validate().is_err());
    }
}
