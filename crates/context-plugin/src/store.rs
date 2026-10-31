use crate::strict_json;
use eve_context_api::*;
use eve_plugin_api::PluginContext;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
};

pub const CONTEXT_INDEX_KEY: &str = "index.v1";
pub const MAX_CONTEXT_INDEX_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_CONTEXT_OWNERS: usize = 1024;
pub const MAX_CONTEXT_VERSIONS: usize = 8192;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    format_version: u32,
    records: BTreeMap<String, Record>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    owner: ContextOwner,
    versions: Vec<ContextVersion>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    format_version: u32,
    owner: ContextOwner,
    text: String,
}
struct Inner {
    index: Index,
    context: Option<PluginContext>,
    archive: Arc<dyn ContextArchiveStore>,
}
pub(super) struct StoredContext {
    inner: Mutex<Inner>,
}

fn digest(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn owner_key(owner: &ContextOwner) -> ContextResult<String> {
    owner.validate()?;
    serde_json::to_vec(owner)
        .map(|bytes| digest(&bytes))
        .map_err(|_| ContextError::InvalidInput)
}
fn body(owner: &ContextOwner, text: &str) -> ContextResult<(String, Vec<u8>)> {
    owner.validate()?;
    validate_note(text)?;
    let bytes = serde_json::to_vec(&Body {
        format_version: 1,
        owner: owner.clone(),
        text: text.into(),
    })
    .map_err(|_| ContextError::InvalidInput)?;
    Ok((digest(&bytes), bytes))
}
fn empty(owner: &ContextOwner) -> ContextView {
    ContextView {
        owner: owner.clone(),
        revision: 0,
        text: String::new(),
        version: None,
    }
}
fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8], max: usize) -> ContextResult<T> {
    if bytes.len() > max {
        return Err(ContextError::CorruptState);
    }
    let value = strict_json::from_slice(bytes).map_err(|_| ContextError::CorruptState)?;
    if value.get("format_version").and_then(|v| v.as_u64()) != Some(1) {
        return Err(if value.get("format_version").is_some() {
            ContextError::UnsupportedVersion
        } else {
            ContextError::CorruptState
        });
    }
    serde_json::from_value(value).map_err(|_| ContextError::CorruptState)
}
fn read_body(
    inner: &Inner,
    owner: &ContextOwner,
    version: &ContextVersion,
) -> ContextResult<ContextView> {
    inner.context.as_ref().ok_or(ContextError::Unavailable)?;
    let bytes = inner.archive.get(&version.sha256)?;
    if digest(&bytes) != version.sha256 {
        return Err(ContextError::CorruptState);
    }
    let data: Body = decode(&bytes, MAX_CONTEXT_BODY_BYTES)?;
    if data.owner != *owner
        || data.text.len() != version.text_bytes
        || validate_note(&data.text).is_err()
    {
        return Err(ContextError::CorruptState);
    }
    Ok(ContextView {
        owner: owner.clone(),
        revision: version.revision,
        text: data.text,
        version: Some(version.clone()),
    })
}
fn read_at(
    inner: &Inner,
    owner: &ContextOwner,
    revision: Option<u64>,
) -> ContextResult<ContextView> {
    let key = owner_key(owner)?;
    if revision == Some(0) {
        return Ok(empty(owner));
    }
    let version = inner.index.records.get(&key).and_then(|r| match revision {
        Some(rev) => r.versions.get(usize::try_from(rev.checked_sub(1)?).ok()?),
        None => r.versions.last(),
    });
    match version {
        Some(version) => read_body(inner, owner, version),
        None if revision.is_none() => Ok(empty(owner)),
        None => Err(ContextError::NotFound),
    }
}
fn close_on_error<T>(inner: &mut Inner, result: ContextResult<T>) -> ContextResult<T> {
    if matches!(
        &result,
        Err(ContextError::Storage | ContextError::CorruptState | ContextError::UnsupportedVersion)
    ) {
        inner.context = None;
        inner.index.records.clear();
    }
    result
}
fn proposal(
    current: &ContextView,
    text: &str,
    change: ContextChange,
) -> ContextResult<ContextView> {
    validate_note(text)?;
    if current.text == text {
        return Ok(current.clone());
    }
    let revision = current
        .revision
        .checked_add(1)
        .ok_or(ContextError::LimitReached)?;
    let (sha256, _) = body(&current.owner, text)?;
    Ok(ContextView {
        owner: current.owner.clone(),
        revision,
        text: text.into(),
        version: Some(ContextVersion {
            revision,
            sha256,
            text_bytes: text.len(),
            change,
        }),
    })
}
fn validate_index(inner: &Inner) -> ContextResult<()> {
    if inner.index.records.len() > MAX_CONTEXT_OWNERS {
        return Err(ContextError::CorruptState);
    }
    let mut count = 0usize;
    for (key, record) in &inner.index.records {
        if owner_key(&record.owner).map_err(|_| ContextError::CorruptState)? != *key
            || record.versions.is_empty()
        {
            return Err(ContextError::CorruptState);
        }
        count = count
            .checked_add(record.versions.len())
            .ok_or(ContextError::CorruptState)?;
        if count > MAX_CONTEXT_VERSIONS {
            return Err(ContextError::CorruptState);
        }
        for (offset, version) in record.versions.iter().enumerate() {
            if version.revision != offset as u64 + 1
                || version.sha256.len() != 64
                || !version
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                || version.text_bytes > MAX_NOTE_BYTES
            {
                return Err(ContextError::CorruptState);
            }
            if offset > 0 && record.versions[offset - 1].sha256 == version.sha256 {
                return Err(ContextError::CorruptState);
            }
            if let ContextChange::Rollback { target_revision } = version.change {
                if target_revision >= version.revision {
                    return Err(ContextError::CorruptState);
                }
                let (expected, bytes) = if target_revision == 0 {
                    (body(&record.owner, "")?.0, 0)
                } else {
                    let target = &record.versions[target_revision as usize - 1];
                    (target.sha256.clone(), target.text_bytes)
                };
                if version.sha256 != expected || version.text_bytes != bytes {
                    return Err(ContextError::CorruptState);
                }
            }
            // 原文逐版本读取与释放；启动不把所有历史正文放入内存，也不修复/清空损坏记录。
            read_body(inner, &record.owner, version)?;
        }
    }
    Ok(())
}
impl StoredContext {
    pub(super) fn open(
        context: PluginContext,
        archive: Arc<dyn ContextArchiveStore>,
    ) -> ContextResult<Self> {
        let index = match context
            .state_get(CONTEXT_INDEX_KEY)
            .map_err(|_| ContextError::Storage)?
        {
            Some(bytes) => decode(&bytes, MAX_CONTEXT_INDEX_BYTES)?,
            None => Index {
                format_version: 1,
                ..Index::default()
            },
        };
        let inner = Inner {
            index,
            context: Some(context),
            archive,
        };
        validate_index(&inner)?;
        Ok(Self {
            inner: Mutex::new(inner),
        })
    }
    fn lock(&self) -> ContextResult<MutexGuard<'_, Inner>> {
        let inner = self.inner.lock().map_err(|_| ContextError::Unavailable)?;
        if inner.context.is_none() {
            return Err(ContextError::Unavailable);
        }
        Ok(inner)
    }
    pub(super) fn close(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.context = None;
            inner.index.records.clear();
        }
    }
    fn commit(
        &self,
        owner: &ContextOwner,
        expected_revision: u64,
        text: Option<&str>,
        target_revision: Option<u64>,
    ) -> ContextResult<ContextView> {
        let mut inner = self.lock()?;
        let current = read_at(&inner, owner, None);
        let current = close_on_error(&mut inner, current)?;
        if current.revision != expected_revision {
            return Err(ContextError::StaleRevision);
        }
        let next = match (text, target_revision) {
            (Some(text), None) => proposal(&current, text, ContextChange::Edit {})?,
            (None, Some(target_revision)) => {
                let target = read_at(&inner, owner, Some(target_revision));
                let target = close_on_error(&mut inner, target)?;
                proposal(
                    &current,
                    &target.text,
                    ContextChange::Rollback { target_revision },
                )?
            }
            _ => return Err(ContextError::InvalidInput),
        };
        if next.revision == current.revision {
            return Ok(current);
        }
        let key = owner_key(owner)?;
        if (!inner.index.records.contains_key(&key)
            && inner.index.records.len() >= MAX_CONTEXT_OWNERS)
            || inner
                .index
                .records
                .values()
                .map(|r| r.versions.len())
                .sum::<usize>()
                >= MAX_CONTEXT_VERSIONS
        {
            return Err(ContextError::LimitReached);
        }
        let version = next.version.as_ref().ok_or(ContextError::CorruptState)?;
        let mut index = inner.index.clone();
        index
            .records
            .entry(key)
            .or_insert_with(|| Record {
                owner: owner.clone(),
                versions: vec![],
            })
            .versions
            .push(version.clone());
        let encoded = serde_json::to_vec(&index).map_err(|_| ContextError::InvalidInput)?;
        if encoded.len() > MAX_CONTEXT_INDEX_BYTES {
            return Err(ContextError::LimitReached);
        }
        let result = (|| {
            let context = inner.context.as_ref().ok_or(ContextError::Unavailable)?;
            let (_, bytes) = body(owner, &next.text)?;
            inner.archive.put(&version.sha256, &bytes)?;
            // 正文独立读回一致，才发布索引。任何提交错误都关闭，不从 Err 猜测未生效。
            if read_body(&inner, owner, version)?.text != next.text {
                return Err(ContextError::CorruptState);
            }
            context
                .state_set(CONTEXT_INDEX_KEY, encoded)
                .map_err(|_| ContextError::Storage)
        })();
        close_on_error(&mut inner, result)?;
        inner.index = index;
        Ok(next)
    }
}
impl ContextAdmin for StoredContext {
    fn inspect(&self, owner: &ContextOwner) -> ContextResult<ContextView> {
        let mut inner = self.lock()?;
        let result = read_at(&inner, owner, None);
        close_on_error(&mut inner, result)
    }
    fn read_version(&self, owner: &ContextOwner, revision: u64) -> ContextResult<ContextView> {
        let mut inner = self.lock()?;
        let result = read_at(&inner, owner, Some(revision));
        close_on_error(&mut inner, result)
    }
    fn versions(
        &self,
        owner: &ContextOwner,
        before: Option<u64>,
        limit: usize,
    ) -> ContextResult<Vec<ContextVersion>> {
        let key = owner_key(owner)?;
        if !(1..=MAX_VERSION_PAGE).contains(&limit) || before == Some(0) {
            return Err(ContextError::InvalidInput);
        }
        Ok(self
            .lock()?
            .index
            .records
            .get(&key)
            .into_iter()
            .flat_map(|r| &r.versions)
            .rev()
            .filter(|v| before.is_none_or(|before| v.revision < before))
            .take(limit)
            .cloned()
            .collect())
    }
    fn preview(&self, owner: &ContextOwner, text: &str) -> ContextResult<ContextPreview> {
        let current = self.inspect(owner)?;
        let proposed = proposal(&current, text, ContextChange::Edit {})?;
        Ok(ContextPreview {
            base_revision: current.revision,
            changed: proposed.revision != current.revision,
            rendered: proposed.rendered()?,
            proposed,
        })
    }
    fn update(
        &self,
        owner: &ContextOwner,
        expected_revision: u64,
        text: &str,
    ) -> ContextResult<ContextView> {
        owner.validate()?;
        validate_note(text)?;
        self.commit(owner, expected_revision, Some(text), None)
    }
    fn rollback(
        &self,
        owner: &ContextOwner,
        expected_revision: u64,
        target_revision: u64,
    ) -> ContextResult<ContextView> {
        owner.validate()?;
        self.commit(owner, expected_revision, None, Some(target_revision))
    }
}
