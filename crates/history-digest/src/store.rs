//! 每个会话作用域一个摘要文件：先写临时文件并同步，再原子改名。
//!
//! 摘要是可由原始会话历史重新生成的派生数据；损坏文件原样保留、不覆盖、不自动删除。
//! 文件数达到上限时淘汰最久未写入的有效文件，原始会话历史不受影响。
use crate::{DigestError, DigestPoint, validate_points};
use eve_llm_api::ContextScope;
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const FORMAT_VERSION: u32 = 1;
/// 单个作用域保留的摘要版本数；窗口调小后仍可能用到覆盖较少的旧版本。
pub const MAX_VERSIONS: usize = 2;
/// 摘要文件数上限；超出时淘汰最久未写入的有效文件。
pub const MAX_SCOPE_FILES: usize = 4096;
const MAX_FILE_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigestVersion {
    /// 从最早一轮起连续覆盖的完整轮次数。
    pub covered_turns: usize,
    /// 被覆盖轮次原文的 SHA-256；原文不一致时不使用此版本。
    pub fingerprint: String,
    pub points: Vec<DigestPoint>,
    pub generator: String,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeFile {
    format_version: u32,
    session_id: String,
    user_id: String,
    versions: Vec<DigestVersion>,
}

pub struct DigestStore {
    directory: PathBuf,
}

impl DigestStore {
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, DigestError> {
        fs::create_dir_all(directory.as_ref()).map_err(|_| DigestError::Storage)?;
        let directory = directory
            .as_ref()
            .canonicalize()
            .map_err(|_| DigestError::Storage)?;
        Ok(Self { directory })
    }

    fn path(&self, scope: &ContextScope) -> PathBuf {
        let mut key = Vec::new();
        key.extend_from_slice(scope.session_id.as_bytes());
        key.push(0);
        key.extend_from_slice(scope.user_id.as_bytes());
        self.directory.join(format!(
            "scope-{}.json",
            hex(digest(&SHA256, &key).as_ref())
        ))
    }

    /// 读取作用域的摘要版本，按覆盖轮数升序；没有文件时为空。
    pub fn versions(&self, scope: &ContextScope) -> Result<Vec<DigestVersion>, DigestError> {
        let path = self.path(scope);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(_) => return Err(DigestError::Storage),
            Ok(metadata) if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES => {
                return Err(DigestError::Corrupt);
            }
            Ok(_) => {}
        }
        let bytes = fs::read(&path).map_err(|_| DigestError::Storage)?;
        let file: ScopeFile = serde_json::from_slice(&bytes).map_err(|_| DigestError::Corrupt)?;
        if file.format_version != FORMAT_VERSION {
            return Err(DigestError::UnsupportedVersion);
        }
        if file.session_id != scope.session_id
            || file.user_id != scope.user_id
            || file.versions.is_empty()
            || file.versions.len() > MAX_VERSIONS
            || file
                .versions
                .windows(2)
                .any(|pair| pair[0].covered_turns >= pair[1].covered_turns)
            || file.versions.iter().any(|version| {
                version.covered_turns == 0
                    || !is_sha256(&version.fingerprint)
                    || version.generator.is_empty()
                    || validate_points(&version.points, version.covered_turns).is_err()
            })
        {
            return Err(DigestError::Corrupt);
        }
        Ok(file.versions)
    }

    /// 加入新版本并保留最近写入的 `MAX_VERSIONS` 个；现有文件损坏时拒绝覆盖。
    pub fn put(&self, scope: &ContextScope, version: DigestVersion) -> Result<(), DigestError> {
        validate_points(&version.points, version.covered_turns)?;
        let path = self.path(scope);
        let mut versions = self.versions(scope)?;
        if versions.is_empty() {
            self.evict_for_new_file()?;
        }
        versions.retain(|existing| existing.covered_turns != version.covered_turns);
        versions.push(version);
        versions.sort_by_key(|version| std::cmp::Reverse(version.created_at_ms));
        versions.truncate(MAX_VERSIONS);
        versions.sort_by_key(|version| version.covered_turns);
        let bytes = serde_json::to_vec(&ScopeFile {
            format_version: FORMAT_VERSION,
            session_id: scope.session_id.clone(),
            user_id: scope.user_id.clone(),
            versions,
        })
        .map_err(|_| DigestError::Storage)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(DigestError::InvalidOutput);
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(DigestError::Storage)?;
        let temporary =
            self.directory
                .join(format!(".{name}.{}-{}.tmp", std::process::id(), now_ms()));
        let written = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &path)?;
            sync_directory(&self.directory)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temporary);
            return Err(DigestError::Storage);
        }
        Ok(())
    }

    fn evict_for_new_file(&self) -> Result<(), DigestError> {
        let mut files = Vec::new();
        for entry in fs::read_dir(&self.directory).map_err(|_| DigestError::Storage)? {
            let entry = entry.map_err(|_| DigestError::Storage)?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !valid_name(&name) {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .map_err(|_| DigestError::Storage)?;
            files.push((modified, name));
        }
        if files.len() < MAX_SCOPE_FILES {
            return Ok(());
        }
        files.sort();
        for (_, name) in files.iter().take(files.len() + 1 - MAX_SCOPE_FILES) {
            let path = self.directory.join(name);
            // 只淘汰能完整解析的文件；损坏文件保留给操作者检查。
            let readable = fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<ScopeFile>(&bytes).ok())
                .is_some();
            if readable {
                fs::remove_file(&path).map_err(|_| DigestError::Storage)?;
            }
        }
        Ok(())
    }
}

fn valid_name(name: &str) -> bool {
    name.strip_prefix("scope-")
        .and_then(|rest| rest.strip_suffix(".json"))
        .is_some_and(is_sha256)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(1)
        .max(1)
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> std::io::Result<()> {
    fs::File::open(directory)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_: &Path) -> std::io::Result<()> {
    Ok(())
}
