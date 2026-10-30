//! 内容寻址的不可变冷记录；先落盘，再由原子账本索引引用。未引用的文件不能授权执行。
use crate::{Result, fail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub(crate) const MAX_LEDGER_BYTES: usize = 64 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BlobRef {
    pub digest: String,
    pub bytes: u64,
}
impl BlobRef {
    fn path(&self, directory: &Path) -> Result<PathBuf> {
        if self.bytes == 0
            || self.bytes > MAX_LEDGER_BYTES as u64
            || self.digest.len() != 64
            || !self
                .digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(fail("冷记录索引无效"));
        }
        Ok(directory
            .join("blobs")
            .join(format!("{}.json", self.digest)))
    }
}
fn hex(hash: ring::digest::Digest) -> String {
    hash.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path).map_err(fail)?.sync_all().map_err(fail)?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
pub(crate) fn put(directory: &Path, value: &impl Serialize) -> Result<BlobRef> {
    let blobs = directory.join("blobs");
    std::fs::create_dir_all(&blobs).map_err(fail)?;
    if !std::fs::symlink_metadata(&blobs).map_err(fail)?.is_dir() {
        return Err(fail("冷记录目录不是普通目录"));
    }
    let mut temp = tempfile::NamedTempFile::new_in(&blobs).map_err(fail)?;
    struct Writer<'a> {
        file: &'a mut File,
        hash: ring::digest::Context,
        bytes: u64,
    }
    impl Write for Writer<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let total = self
                .bytes
                .checked_add(bytes.len() as u64)
                .filter(|n| *n <= MAX_LEDGER_BYTES as u64)
                .ok_or_else(|| std::io::Error::other("冷记录容量超限"))?;
            self.file.write_all(bytes)?;
            self.hash.update(bytes);
            self.bytes = total;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.file.flush()
        }
    }
    let mut writer = Writer {
        file: temp.as_file_mut(),
        hash: ring::digest::Context::new(&ring::digest::SHA256),
        bytes: 0,
    };
    {
        let mut buffered = std::io::BufWriter::with_capacity(64 * 1024, &mut writer);
        serde_json::to_writer(&mut buffered, value).map_err(fail)?;
        buffered.flush().map_err(fail)?;
    }
    let reference = BlobRef {
        digest: hex(writer.hash.finish()),
        bytes: writer.bytes,
    };
    temp.as_file().sync_all().map_err(fail)?;
    let path = reference.path(directory)?;
    if path.try_exists().map_err(fail)? {
        // 相同路径若损坏必须保留并失败，不能用新候选覆盖“修复”旧证据。
        read_bytes(directory, &reference)?;
    } else {
        temp.persist_noclobber(path).map_err(fail)?;
        sync_directory(&blobs)?;
        sync_directory(directory)?;
    }
    Ok(reference)
}
fn read_bytes(directory: &Path, reference: &BlobRef) -> Result<Vec<u8>> {
    let path = reference.path(directory)?;
    let meta = std::fs::symlink_metadata(&path).map_err(fail)?;
    if !meta.is_file() || meta.len() != reference.bytes {
        return Err(fail("冷记录缺失或大小不符"));
    }
    let file = File::open(path).map_err(fail)?;
    let mut bytes = Vec::new();
    file.take(reference.bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(fail)?;
    if bytes.len() as u64 != reference.bytes
        || hex(ring::digest::digest(&ring::digest::SHA256, &bytes)) != reference.digest
    {
        return Err(fail("冷记录校验失败；原文件保留"));
    }
    Ok(bytes)
}
pub(crate) fn load<T: DeserializeOwned>(directory: &Path, reference: &BlobRef) -> Result<T> {
    let bytes = read_bytes(directory, reference)?;
    serde_json::from_value(crate::store::strict_json(&bytes)?).map_err(fail)
}

use crate::{Language, PublishedVersion, Receipt, ReceiptStatus, StateSchema, VersionRef};
use eve_llm_api::ContextScope;
use std::collections::BTreeMap;

/// 列表只暴露不可变版本元信息；全文通过同一身份范围的version读取。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionSummary {
    pub reference: VersionRef,
    pub owner: ContextScope,
    pub language: Language,
    pub state_schema: StateSchema,
    pub dependencies: BTreeMap<String, VersionRef>,
    pub tools: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VersionIndex {
    pub summary: VersionSummary,
    pub blob: BlobRef,
}
impl std::ops::Deref for VersionIndex {
    type Target = VersionSummary;
    fn deref(&self) -> &Self::Target {
        &self.summary
    }
}
impl VersionIndex {
    pub fn create(directory: &Path, version: &PublishedVersion) -> Result<Self> {
        Ok(Self {
            summary: Self::summary(version),
            blob: put(directory, version)?,
        })
    }
    fn summary(version: &PublishedVersion) -> VersionSummary {
        VersionSummary {
            reference: version.reference.clone(),
            owner: version.owner.clone(),
            language: version.spec.language.clone(),
            state_schema: version.spec.state_schema.clone(),
            dependencies: version.spec.dependencies.clone(),
            tools: version.spec.tools.clone(),
        }
    }
    pub fn load(&self, directory: &Path) -> Result<PublishedVersion> {
        let version: PublishedVersion = load(directory, &self.blob)?;
        version.spec.validate()?;
        if Self::summary(&version) != self.summary
            || version.reference.script != version.spec.name
            || crate::digest(&(&version.owner, &version.spec))? != version.reference.digest
        {
            return Err(fail("冷版本正文与索引身份或内容不符"));
        }
        Ok(version)
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReceiptIndex {
    pub call_id: String,
    pub parent_call_id: Option<String>,
    pub owner: ContextScope,
    pub version: VersionRef,
    pub status: ReceiptStatus,
    pub bytes: u64,
    pub blob: Option<BlobRef>,
    pub hot: Option<Receipt>,
}
impl ReceiptIndex {
    pub fn hot(receipt: Receipt) -> Result<Self> {
        let bytes = serde_json::to_vec(&receipt).map_err(fail)?.len() as u64;
        Ok(Self {
            call_id: receipt.call_id.clone(),
            parent_call_id: receipt.parent_call_id.clone(),
            owner: receipt.owner.clone(),
            version: receipt.version.clone(),
            status: receipt.status.clone(),
            bytes,
            blob: None,
            hot: Some(receipt),
        })
    }
    pub fn load(&self, directory: &Path) -> Result<Receipt> {
        let receipt = match (&self.hot, &self.blob) {
            (Some(hot), None)
                if !matches!(
                    self.status,
                    ReceiptStatus::Succeeded | ReceiptStatus::Failed
                ) =>
            {
                hot.clone()
            }
            (None, Some(blob))
                if matches!(
                    self.status,
                    ReceiptStatus::Succeeded | ReceiptStatus::Failed
                ) && blob.bytes == self.bytes =>
            {
                load(directory, blob)?
            }
            _ => return Err(fail("回执冷热索引不一致")),
        };
        if receipt.call_id != self.call_id
            || receipt.parent_call_id != self.parent_call_id
            || receipt.owner != self.owner
            || receipt.version != self.version
            || receipt.status != self.status
            || serde_json::to_vec(&receipt).map_err(fail)?.len() as u64 != self.bytes
        {
            return Err(fail("冷回执正文与索引身份或内容不符"));
        }
        Ok(receipt)
    }
    pub fn freeze(&mut self, directory: &Path) -> Result<()> {
        if matches!(
            self.status,
            ReceiptStatus::Succeeded | ReceiptStatus::Failed
        ) && let Some(hot) = &self.hot
        {
            let blob = put(directory, hot)?;
            if blob.bytes != self.bytes {
                return Err(fail("终态回执大小不符"));
            }
            self.blob = Some(blob);
            self.hot = None;
        }
        Ok(())
    }
}
