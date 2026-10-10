//! 向量索引账本：记忆条目的量化向量，按模型、维度、范围与条目键唯一。它是可由记忆重新计算的
//! 派生数据，但同样严格读取：损坏时拒绝打开并保留原字节，不自动清空或重建。
use crate::strict_json;
use eve_plugin_api::{PluginContext, PluginError, PluginResult};
use eve_semantic_api::{EmbeddingProfile, MAX_DIMENSIONS, QuantizedVector};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fmt,
    sync::{Mutex, MutexGuard},
};

pub const SEMANTIC_PLUGIN_ID: &str = "eve.semantic";
pub const SEMANTIC_STATE_KEY: &str = "index.v1";
/// 全部模型合计的条目上限；满时先移出其他模型的条目，仍满则停止新增并明确报告。
pub const MAX_ENTRIES: usize = 2048;
pub const MAX_INDEX_STATE_BYTES: usize = 16 * 1024 * 1024;
const FORMAT_VERSION: u32 = 1;
const MAX_ITEM_BYTES: usize = 320;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexError {
    InvalidInput,
    Unavailable,
    CorruptState,
    UnsupportedVersion,
    /// 提交无法确认；实例已关闭，须重新打开核对。
    Storage,
    LimitReached,
}
impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "语义索引输入无效",
            Self::Unavailable => "语义索引不可用",
            Self::CorruptState => "语义索引损坏；未清空",
            Self::UnsupportedVersion => "不支持该语义索引版本",
            Self::Storage => "语义索引提交无法确认；须重新打开核对",
            Self::LimitReached => "语义索引已满；保留原记录",
        })
    }
}
impl std::error::Error for IndexError {}

/// 一条索引：范围以摘要表示，正文只以摘要表示；向量为量化后的十六进制。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexEntry {
    pub model: String,
    pub dimensions: u32,
    pub scope: String,
    pub item: String,
    pub content_sha256: String,
    pub scale: f32,
    pub vector: String,
    pub indexed_at_ms: u64,
}
impl IndexEntry {
    pub fn belongs_to(&self, profile: &EmbeddingProfile) -> bool {
        self.model == profile.model && self.dimensions == profile.dimensions
    }
    pub fn restore(&self) -> Option<Vec<f32>> {
        QuantizedVector::from_hex(self.scale, &self.vector).map(|vector| vector.restore())
    }
    fn identity(&self) -> (&str, u32, &str, &str) {
        (&self.model, self.dimensions, &self.scope, &self.item)
    }
    fn validate(&self) -> Result<(), IndexError> {
        let profile = EmbeddingProfile {
            model: self.model.clone(),
            dimensions: self.dimensions,
        };
        let hex = |value: &str| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        };
        let restored = QuantizedVector::from_hex(self.scale, &self.vector);
        if profile.validate().is_err()
            || self.dimensions > MAX_DIMENSIONS
            || !hex(&self.scope)
            || !hex(&self.content_sha256)
            || self.item.is_empty()
            || self.item.len() > MAX_ITEM_BYTES
            || self.item.chars().any(char::is_control)
            || self.indexed_at_ms == 0
            || restored.is_none_or(|vector| vector.values.len() != self.dimensions as usize)
        {
            return Err(IndexError::CorruptState);
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    format_version: u32,
    entries: Vec<IndexEntry>,
}

struct Inner {
    ledger: Ledger,
    context: Option<PluginContext>,
}
pub(crate) struct StoredIndex {
    inner: Mutex<Inner>,
}

impl StoredIndex {
    pub(crate) fn open(context: PluginContext) -> Result<Self, IndexError> {
        let ledger = match context
            .state_get(SEMANTIC_STATE_KEY)
            .map_err(|_| IndexError::Storage)?
        {
            None => Ledger {
                format_version: FORMAT_VERSION,
                entries: vec![],
            },
            Some(bytes) => {
                if bytes.len() > MAX_INDEX_STATE_BYTES {
                    return Err(IndexError::CorruptState);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| IndexError::CorruptState)?;
                if value.get("format_version").and_then(|value| value.as_u64())
                    != Some(u64::from(FORMAT_VERSION))
                {
                    return Err(if value.get("format_version").is_some() {
                        IndexError::UnsupportedVersion
                    } else {
                        IndexError::CorruptState
                    });
                }
                let ledger: Ledger =
                    serde_json::from_value(value).map_err(|_| IndexError::CorruptState)?;
                validate(&ledger)?;
                ledger
            }
        };
        Ok(Self {
            inner: Mutex::new(Inner {
                ledger,
                context: Some(context),
            }),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>, IndexError> {
        let inner = self.inner.lock().map_err(|_| IndexError::Unavailable)?;
        if inner.context.is_none() {
            return Err(IndexError::Unavailable);
        }
        Ok(inner)
    }

    pub(crate) fn entries(&self) -> Result<Vec<IndexEntry>, IndexError> {
        Ok(self.lock()?.ledger.entries.clone())
    }

    /// 一次提交新增或替换的条目与移除的条目。超出上限时先移出其他模型的旧条目；
    /// 仍超出则不做任何更改，返回 LimitReached。
    pub(crate) fn apply(
        &self,
        profile: &EmbeddingProfile,
        upserts: Vec<IndexEntry>,
        removals: Vec<(String, String)>,
    ) -> Result<(), IndexError> {
        for entry in &upserts {
            entry.validate().map_err(|_| IndexError::InvalidInput)?;
            if !entry.belongs_to(profile) {
                return Err(IndexError::InvalidInput);
            }
        }
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let replaced: BTreeSet<(String, String)> = upserts
            .iter()
            .map(|entry| (entry.scope.clone(), entry.item.clone()))
            .chain(removals)
            .collect();
        next.entries.retain(|entry| {
            !(entry.belongs_to(profile)
                && replaced.contains(&(entry.scope.clone(), entry.item.clone())))
        });
        next.entries.extend(upserts);
        if next.entries.len() > MAX_ENTRIES {
            let mut others: Vec<usize> = next
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| !entry.belongs_to(profile))
                .map(|(index, _)| index)
                .collect();
            others.sort_by_key(|index| next.entries[*index].indexed_at_ms);
            let excess = next.entries.len() - MAX_ENTRIES;
            if others.len() < excess {
                return Err(IndexError::LimitReached);
            }
            let evicted: BTreeSet<usize> = others.into_iter().take(excess).collect();
            let mut index = 0;
            next.entries.retain(|_| {
                let keep = !evicted.contains(&index);
                index += 1;
                keep
            });
        }
        persist(&mut inner, next)
    }

    pub(crate) fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("语义索引锁不可用".into()))?
            .context = None;
        Ok(())
    }
}

fn validate(ledger: &Ledger) -> Result<(), IndexError> {
    if ledger.entries.len() > MAX_ENTRIES {
        return Err(IndexError::CorruptState);
    }
    let mut seen = BTreeSet::new();
    for entry in &ledger.entries {
        entry.validate()?;
        if !seen.insert(entry.identity()) {
            return Err(IndexError::CorruptState);
        }
    }
    Ok(())
}

fn persist(inner: &mut Inner, next: Ledger) -> Result<(), IndexError> {
    validate(&next).map_err(|_| IndexError::InvalidInput)?;
    let bytes = serde_json::to_vec(&next).map_err(|_| IndexError::InvalidInput)?;
    if bytes.len() > MAX_INDEX_STATE_BYTES {
        return Err(IndexError::LimitReached);
    }
    let context = inner.context.as_ref().ok_or(IndexError::Unavailable)?;
    // 失败也可能已经提交；关闭整个实例，禁止旧缓存继续读取或覆盖后端。
    if context.state_set(SEMANTIC_STATE_KEY, bytes).is_err() {
        inner.context = None;
        return Err(IndexError::Storage);
    }
    inner.ledger = next;
    Ok(())
}
