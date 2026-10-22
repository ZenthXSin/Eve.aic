//! 有界、独立持久化的人名目录；不合并不同会话，不读取权限。
//! 开启印象学习时，另外缓冲此人谈及自己的原话，成批学习后只保存带原话摘录的印象。
use eve_people_api::*;
use eve_plugin_api::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
mod learner;
pub use learner::ModelImpressionLearner;

const KEY: &str = "people.v1";
/// 待学原话的字符上限；更长的不缓冲。
const MAX_PENDING_CHARS: usize = 200;
fn unavailable() -> PluginError {
    PluginError::State("人名目录不可用；保留原状态".into())
}
fn is_zero(value: &u64) -> bool {
    *value == 0
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u8,
    people: Vec<Person>,
    /// 印象与批次编号计数；只在格式 2 出现。
    #[serde(default, skip_serializing_if = "is_zero")]
    next: u64,
}
impl Document {
    /// 没有印象或待学原话时仍写格式 1，旧程序照常读取。
    fn bytes(&mut self) -> PluginResult<Vec<u8>> {
        let extended = self.next > 0
            || self
                .people
                .iter()
                .any(|p| !p.impressions.is_empty() || !p.pending.is_empty());
        self.version = if extended { 2 } else { 1 };
        let bytes = serde_json::to_vec(self).map_err(|_| unavailable())?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err(PluginError::Task("人名目录容量已满；原记录保留".into()));
        }
        Ok(bytes)
    }
    fn id(&mut self, prefix: char) -> PluginResult<String> {
        self.next = self.next.checked_add(1).ok_or_else(unavailable)?;
        Ok(format!("{prefix}{}", self.next))
    }
}
fn numbered(id: &str, prefix: char, next: u64) -> bool {
    id.strip_prefix(prefix)
        .and_then(|n| n.parse::<u64>().ok())
        .is_some_and(|n| n > 0 && n <= next && id == format!("{prefix}{n}"))
}
struct Loaded {
    document: Document,
    ctx: PluginContext,
    /// 已取出、尚未交回的批次；只在内存中，重启后不重试。
    inflight: BTreeMap<String, String>,
}
#[derive(Clone, Default)]
pub struct PeopleController {
    inner: Arc<Mutex<Option<Loaded>>>,
    impressions: bool,
}
impl PeopleController {
    /// 先写盘再更新内存；写入结果不能确认时关闭所有克隆句柄，重启后以实际磁盘为准。
    fn commit(inner: &mut Option<Loaded>, mut document: Document) -> PluginResult<()> {
        let bytes = document.bytes()?;
        let loaded = inner.as_ref().ok_or_else(unavailable)?;
        if loaded.ctx.state_set(KEY, bytes).is_err() {
            *inner = None;
            return Err(unavailable());
        }
        inner.as_mut().ok_or_else(unavailable)?.document = document;
        Ok(())
    }
}
impl PeopleDirectory for PeopleController {
    fn observe(&self, scope: PersonScope, name: Option<PersonName>) -> PluginResult<()> {
        if !valid_identity(&scope.conversation)
            || !valid_identity(&scope.participant)
            || name.as_ref().is_some_and(|n| !valid_name(&n.value))
        {
            return Err(unavailable());
        }
        let mut inner = self.inner.lock().map_err(|_| unavailable())?;
        let loaded = inner.as_ref().ok_or_else(unavailable)?;
        let mut document = loaded.document.clone();
        match document
            .people
            .iter_mut()
            .find(|p| p.scope.participant == scope.participant)
        {
            Some(person) => {
                if person.scope != scope {
                    return Err(unavailable());
                }
                let Some(name) = name else {
                    return Ok(());
                };
                if person.name.as_ref().is_some_and(|old| {
                    old == &name
                        || (old.source == NameSource::Declared
                            && name.source == NameSource::Platform)
                }) {
                    return Ok(());
                }
                person.name = Some(name);
                person.revision = person.revision.checked_add(1).ok_or_else(unavailable)?;
            }
            None => {
                if document.people.len() == MAX_PEOPLE {
                    return Err(PluginError::Task("人名目录已满；原记录保留".into()));
                }
                document.people.push(Person {
                    scope,
                    name,
                    revision: 1,
                    impressions: vec![],
                    pending: vec![],
                });
            }
        }
        Self::commit(&mut inner, document)
    }
    fn view(&self, participant: &str) -> PluginResult<Option<PeopleView>> {
        if !valid_identity(participant) {
            return Err(unavailable());
        }
        let inner = self.inner.lock().map_err(|_| unavailable())?;
        let loaded = inner.as_ref().ok_or_else(unavailable)?;
        let Some(current) = loaded
            .document
            .people
            .iter()
            .find(|p| p.scope.participant == participant)
        else {
            return Ok(None);
        };
        let mut others: Vec<_> = loaded
            .document
            .people
            .iter()
            .filter(|p| {
                p.scope.conversation == current.scope.conversation
                    && p.scope.participant != participant
                    && p.name.is_some()
            })
            .map(|p| Person {
                impressions: vec![],
                pending: vec![],
                ..p.clone()
            })
            .collect();
        let truncated = others.len() > MAX_VISIBLE_PEOPLE;
        others.truncate(MAX_VISIBLE_PEOPLE);
        Ok(Some(PeopleView {
            current: Person {
                pending: vec![],
                ..current.clone()
            },
            others,
            truncated,
        }))
    }
    fn note(&self, participant: &str, text: &str) -> PluginResult<()> {
        let text = text.trim();
        if !self.impressions
            || text.is_empty()
            || text.chars().count() > MAX_PENDING_CHARS
            || !valid_identity(participant)
        {
            return Ok(());
        }
        let mut inner = self.inner.lock().map_err(|_| unavailable())?;
        let loaded = inner.as_ref().ok_or_else(unavailable)?;
        let mut document = loaded.document.clone();
        let Some(person) = document
            .people
            .iter_mut()
            .find(|p| p.scope.participant == participant)
        else {
            return Ok(());
        };
        if person.pending.len() >= MAX_IMPRESSION_PENDING {
            person.pending.remove(0);
        }
        person.pending.push(text.into());
        match Self::commit(&mut inner, document) {
            // 容量已满只是不再缓冲，已有记录保留。
            Err(PluginError::Task(_)) => Ok(()),
            other => other,
        }
    }
    fn take_impression_batch(&self) -> PluginResult<Option<ImpressionBatch>> {
        if !self.impressions {
            return Ok(None);
        }
        let mut inner = self.inner.lock().map_err(|_| unavailable())?;
        let loaded = inner.as_ref().ok_or_else(unavailable)?;
        let busy: BTreeSet<&String> = loaded.inflight.values().collect();
        let Some(index) = loaded.document.people.iter().position(|p| {
            p.pending.len() >= IMPRESSION_BATCH && !busy.contains(&p.scope.participant)
        }) else {
            return Ok(None);
        };
        let mut document = loaded.document.clone();
        let id = document.id('b')?;
        let person = &mut document.people[index];
        let messages = std::mem::take(&mut person.pending);
        let batch = ImpressionBatch {
            id: id.clone(),
            participant: person.scope.participant.clone(),
            messages,
            known: person.impressions.clone(),
        };
        Self::commit(&mut inner, document)?;
        inner
            .as_mut()
            .ok_or_else(unavailable)?
            .inflight
            .insert(id, batch.participant.clone());
        Ok(Some(batch))
    }
    fn finish_impression_batch(
        &self,
        batch: &ImpressionBatch,
        drafts: Vec<ImpressionDraft>,
        now_ms: u64,
    ) -> PluginResult<usize> {
        let mut inner = self.inner.lock().map_err(|_| unavailable())?;
        let loaded = inner.as_mut().ok_or_else(unavailable)?;
        if loaded.inflight.remove(&batch.id).as_ref() != Some(&batch.participant) {
            return Err(PluginError::Task("印象批次不存在或已结束".into()));
        }
        let mut document = loaded.document.clone();
        let Some(index) = document
            .people
            .iter()
            .position(|p| p.scope.participant == batch.participant)
        else {
            return Err(unavailable());
        };
        let current: BTreeSet<String> = document.people[index]
            .impressions
            .iter()
            .map(|i| i.id.clone())
            .collect();
        let mut replaced = BTreeSet::new();
        if drafts.len() > MAX_IMPRESSION_DRAFTS
            || drafts.iter().any(|d| {
                !valid_impression_text(&d.text)
                    || !valid_impression_text(&d.quote)
                    || !batch.messages.iter().any(|m| m.contains(&d.quote))
                    || d.replaces
                        .as_ref()
                        .is_some_and(|id| !current.contains(id) || !replaced.insert(id.clone()))
            })
        {
            return Err(PluginError::Task("印象学习结果不合格；本批不保存".into()));
        }
        let mut changed = 0;
        for draft in drafts {
            let impression = Impression {
                id: document.id('i')?,
                text: draft.text,
                quote: draft.quote,
                updated_at_ms: now_ms,
            };
            let person = &mut document.people[index];
            if let Some(old) = draft.replaces {
                let slot = person
                    .impressions
                    .iter_mut()
                    .find(|i| i.id == old)
                    .ok_or_else(unavailable)?;
                *slot = impression;
            } else if person.impressions.len() < MAX_IMPRESSIONS {
                person.impressions.push(impression);
            } else {
                // 满了只保留现有印象；用户可删除旧的腾出位置。
                continue;
            }
            changed += 1;
        }
        if changed > 0 {
            Self::commit(&mut inner, document)?;
        }
        Ok(changed)
    }
    fn impressions(&self, participant: &str) -> PluginResult<Vec<Impression>> {
        let inner = self.inner.lock().map_err(|_| unavailable())?;
        let loaded = inner.as_ref().ok_or_else(unavailable)?;
        Ok(loaded
            .document
            .people
            .iter()
            .find(|p| p.scope.participant == participant)
            .map(|p| p.impressions.clone())
            .unwrap_or_default())
    }
    fn forget_impression(&self, participant: &str, id: &str) -> PluginResult<bool> {
        let mut inner = self.inner.lock().map_err(|_| unavailable())?;
        let loaded = inner.as_ref().ok_or_else(unavailable)?;
        let mut document = loaded.document.clone();
        let Some(person) = document
            .people
            .iter_mut()
            .find(|p| p.scope.participant == participant)
        else {
            return Ok(false);
        };
        let before = person.impressions.len();
        person.impressions.retain(|i| i.id != id);
        if person.impressions.len() == before {
            return Ok(false);
        }
        Self::commit(&mut inner, document)?;
        Ok(true)
    }
}
pub struct PeoplePlugin {
    manifest: PluginManifest,
    controller: PeopleController,
}
impl PeoplePlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(PEOPLE_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            controller: PeopleController::default(),
        })
    }
    /// 开启后缓冲谈及自己的原话，供宿主装配的印象学习器成批学习。须在取 controller 前调用。
    pub fn with_impression_learning(mut self) -> Self {
        self.controller.impressions = true;
        self
    }
    pub fn controller(&self) -> PeopleController {
        self.controller.clone()
    }
}
fn valid_document(document: &Document) -> bool {
    let mut ids = BTreeSet::new();
    let mut impression_ids = BTreeSet::new();
    let extended = document.version == 2;
    (document.version == 1 || extended)
        && (extended || document.next == 0)
        && document.people.len() <= MAX_PEOPLE
        && !document.people.iter().any(|p| {
            !valid_identity(&p.scope.conversation)
                || !valid_identity(&p.scope.participant)
                || p.revision == 0
                || !ids.insert(p.scope.participant.clone())
                || p.name.as_ref().is_some_and(|n| !valid_name(&n.value))
                || (!extended && (!p.impressions.is_empty() || !p.pending.is_empty()))
                || p.impressions.len() > MAX_IMPRESSIONS
                || p.pending.len() > MAX_IMPRESSION_PENDING
                || p.pending
                    .iter()
                    .any(|m| m.trim().is_empty() || m.chars().count() > MAX_PENDING_CHARS)
                || p.impressions.iter().any(|i| {
                    !valid_impression_text(&i.text)
                        || !valid_impression_text(&i.quote)
                        || !numbered(&i.id, 'i', document.next)
                        || !impression_ids.insert(i.id.clone())
                })
        })
}
impl Plugin for PeoplePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let document = match ctx.state_get(KEY)? {
                None => Document {
                    version: 1,
                    people: vec![],
                    next: 0,
                },
                Some(bytes) => {
                    if bytes.len() > MAX_STATE_BYTES {
                        return Err(unavailable());
                    }
                    let document: Document =
                        serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
                    if !valid_document(&document) {
                        return Err(unavailable());
                    }
                    document
                }
            };
            *self.controller.inner.lock().map_err(|_| unavailable())? = Some(Loaded {
                document,
                ctx,
                inflight: BTreeMap::new(),
            });
            let controller = self.controller.clone();
            Ok(Some(cleanup(move || async move {
                *controller.inner.lock().map_err(|_| unavailable())? = None;
                Ok(())
            })))
        })
    }
}
