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
mod profile;
pub use profile::ModelProfileConsolidator;

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
    /// 印象与批次编号计数；只在格式 2、3 出现。
    #[serde(default, skip_serializing_if = "is_zero")]
    next: u64,
}
impl Document {
    /// 没有印象或待学原话时仍写格式 1，有来访记录时写格式 3，有画像时写格式 4；
    /// 只在用到时升级，旧程序照常读取旧格式。
    fn bytes(&mut self) -> PluginResult<Vec<u8>> {
        let extended = self.next > 0
            || self
                .people
                .iter()
                .any(|p| !p.impressions.is_empty() || !p.pending.is_empty());
        self.version = if self.people.iter().any(|p| p.profile.is_some()) {
            4
        } else if self.people.iter().any(|p| p.presence.is_some()) {
            3
        } else if extended {
            2
        } else {
            1
        };
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
                    presence: None,
                    profile: None,
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
                presence: None,
                profile: None,
                ..p.clone()
            })
            .collect();
        let truncated = others.len() > MAX_VISIBLE_PEOPLE;
        others.truncate(MAX_VISIBLE_PEOPLE);
        Ok(Some(PeopleView {
            current: Person {
                pending: vec![],
                profile: None,
                ..current.clone()
            },
            others,
            truncated,
        }))
    }
    fn visit(&self, participant: &str, scene: Scene, now_ms: u64) -> PluginResult<()> {
        if !valid_identity(participant) {
            return Err(unavailable());
        }
        let mut inner = self.inner.lock().map_err(|_| unavailable())?;
        let loaded = inner.as_ref().ok_or_else(unavailable)?;
        let mut document = loaded.document.clone();
        let Some(person) = document
            .people
            .iter_mut()
            .find(|p| p.scope.participant == participant)
        else {
            return Err(unavailable());
        };
        person.presence = Some(match person.presence.take() {
            None => Presence {
                scene,
                first_seen_ms: now_ms,
                last_seen_ms: now_ms,
                previous_seen_ms: None,
                messages: 1,
            },
            // 场景随身份固定；不一致说明数据有误，保留原记录。
            Some(old) if old.scene != scene => return Err(unavailable()),
            // 时钟倒退时只计数，不让时间往回走。
            Some(old) => Presence {
                scene,
                first_seen_ms: old.first_seen_ms,
                last_seen_ms: old.last_seen_ms.max(now_ms),
                previous_seen_ms: Some(old.last_seen_ms),
                messages: old.messages.checked_add(1).ok_or_else(unavailable)?,
            },
        });
        Self::commit(&mut inner, document)
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
        // 画像可能含有这条印象整合出的说法，一并删除，待依据变化后重新整理。
        person.profile = None;
        Self::commit(&mut inner, document)?;
        Ok(true)
    }
    fn linked_impressions(
        &self,
        participant: &str,
        links: &dyn PersonLinks,
    ) -> PluginResult<LinkedImpressions> {
        let Some(found) = self.linked_sources(participant, links)? else {
            return Ok(LinkedImpressions::default());
        };
        let mut seen: BTreeSet<String> = found.own.iter().map(|i| i.text.clone()).collect();
        let mut items: Vec<LinkedImpression> = found
            .linked
            .into_iter()
            .map(|(scene, impression)| LinkedImpression {
                text: impression.text,
                quote: impression.quote,
                updated_at_ms: impression.updated_at_ms,
                scene,
            })
            .collect();
        // 新的在前；同一说法在多处出现只留最新一条，也不重复本会话已有的印象。
        items.sort_by(|a, b| {
            b.updated_at_ms
                .cmp(&a.updated_at_ms)
                .then_with(|| a.text.cmp(&b.text))
        });
        items.retain(|item| seen.insert(item.text.clone()));
        let truncated = items.len() > MAX_LINKED_IMPRESSIONS;
        items.truncate(MAX_LINKED_IMPRESSIONS);
        Ok(LinkedImpressions {
            items,
            sources: found.sources,
            truncated,
        })
    }
    fn profile_request(
        &self,
        participant: &str,
        links: &dyn PersonLinks,
    ) -> PluginResult<Option<ProfileRequest>> {
        let Some(found) = self.linked_sources(participant, links)? else {
            return Ok(None);
        };
        let mut items: Vec<(u64, ProfileSource)> = found
            .own
            .into_iter()
            .map(|i| (i.updated_at_ms, i, true, found.scene))
            .chain(
                found
                    .linked
                    .into_iter()
                    .map(|(scene, i)| (i.updated_at_ms, i, false, scene)),
            )
            .map(|(at, i, here, scene)| {
                (
                    at,
                    ProfileSource {
                        text: i.text,
                        quote: i.quote,
                        here,
                        scene,
                    },
                )
            })
            .collect();
        // 本会话的说法排在同一时刻的他处说法之前，去重时优先保留。
        items.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.here.cmp(&a.1.here))
                .then_with(|| a.1.text.cmp(&b.1.text))
        });
        let mut seen = BTreeSet::new();
        items.retain(|(_, item)| seen.insert(item.text.clone()));
        items.truncate(MAX_PROFILE_SOURCES);
        if items.len() < PROFILE_MIN_ITEMS {
            return Ok(None);
        }
        let sources: Vec<ProfileSource> = items.into_iter().map(|(_, item)| item).collect();
        let bytes = serde_json::to_vec(&sources).map_err(|_| unavailable())?;
        let mut fingerprint = String::from("sha256:");
        for byte in ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref() {
            use std::fmt::Write;
            write!(&mut fingerprint, "{byte:02x}").map_err(|_| unavailable())?;
        }
        Ok(Some(ProfileRequest {
            participant: participant.into(),
            fingerprint,
            sources,
        }))
    }
    fn profile(&self, participant: &str) -> PluginResult<Option<PersonProfile>> {
        let inner = self.inner.lock().map_err(|_| unavailable())?;
        let loaded = inner.as_ref().ok_or_else(unavailable)?;
        Ok(loaded
            .document
            .people
            .iter()
            .find(|p| p.scope.participant == participant)
            .and_then(|p| p.profile.clone()))
    }
    fn save_profile(
        &self,
        request: &ProfileRequest,
        facts: Vec<ProfileFact>,
        now_ms: u64,
    ) -> PluginResult<()> {
        if !valid_fingerprint(&request.fingerprint)
            || request.sources.len() > MAX_PROFILE_SOURCES
            || !valid_facts(&facts, request.sources.len())
        {
            return Err(PluginError::Task("画像整理结果不合格；未保存".into()));
        }
        let scenes = facts
            .iter()
            .map(|fact| {
                fact.from
                    .iter()
                    .map(|&i| request.sources[i].scene)
                    .collect()
            })
            .collect();
        let mut inner = self.inner.lock().map_err(|_| unavailable())?;
        let loaded = inner.as_ref().ok_or_else(unavailable)?;
        let mut document = loaded.document.clone();
        let person = document
            .people
            .iter_mut()
            .find(|p| p.scope.participant == request.participant)
            .ok_or_else(unavailable)?;
        person.profile = Some(PersonProfile {
            fingerprint: request.fingerprint.clone(),
            facts,
            scenes,
            updated_at_ms: now_ms,
        });
        Self::commit(&mut inner, document)
    }
}

/// 当前发言者本会话的印象与经授权的他处印象，尚未去重或截断。
struct LinkedSources {
    own: Vec<Impression>,
    scene: Option<Scene>,
    linked: Vec<(Option<Scene>, Impression)>,
    sources: usize,
}
impl PeopleController {
    /// 没见过此人时为空。先复制候选再释放锁：宿主判断身份时会读取配置与路由，不在持锁时调用。
    fn linked_sources(
        &self,
        participant: &str,
        links: &dyn PersonLinks,
    ) -> PluginResult<Option<LinkedSources>> {
        if !valid_identity(participant) {
            return Err(unavailable());
        }
        let (own, scene, candidates) = {
            let inner = self.inner.lock().map_err(|_| unavailable())?;
            let loaded = inner.as_ref().ok_or_else(unavailable)?;
            let people = &loaded.document.people;
            let Some(current) = people.iter().find(|p| p.scope.participant == participant) else {
                return Ok(None);
            };
            let candidates: BTreeMap<String, (Option<Scene>, Vec<Impression>)> = people
                .iter()
                .filter(|p| p.scope.participant != participant && !p.impressions.is_empty())
                .map(|p| {
                    (
                        p.scope.participant.clone(),
                        (p.presence.as_ref().map(|v| v.scene), p.impressions.clone()),
                    )
                })
                .collect();
            (
                current.impressions.clone(),
                current.presence.as_ref().map(|v| v.scene),
                candidates,
            )
        };
        if candidates.is_empty() {
            return Ok(Some(LinkedSources {
                own,
                scene,
                linked: vec![],
                sources: 0,
            }));
        }
        let ids: Vec<String> = candidates.keys().cloned().collect();
        let linked: BTreeSet<String> = links.linked(participant, &ids)?.into_iter().collect();
        if linked.iter().any(|id| !candidates.contains_key(id)) {
            return Err(PluginError::Task("跨会话印象授权返回了未知来源".into()));
        }
        let mut items = Vec::new();
        for id in &linked {
            let (scene, impressions) = &candidates[id];
            items.extend(impressions.iter().cloned().map(|i| (*scene, i)));
        }
        Ok(Some(LinkedSources {
            own,
            scene,
            linked: items,
            sources: linked.len(),
        }))
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
fn valid_fingerprint(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
}
fn valid_facts(facts: &[ProfileFact], sources: usize) -> bool {
    facts.len() <= MAX_PROFILE_FACTS
        && facts.iter().all(|fact| {
            let unique: BTreeSet<_> = fact.from.iter().collect();
            valid_impression_text(&fact.text)
                && !fact.from.is_empty()
                && unique.len() == fact.from.len()
                && fact.from.iter().all(|&i| i < sources)
        })
}
fn valid_profile(profile: &PersonProfile) -> bool {
    valid_fingerprint(&profile.fingerprint)
        && valid_facts(&profile.facts, MAX_PROFILE_SOURCES)
        && profile.scenes.len() == profile.facts.len()
        && profile
            .scenes
            .iter()
            .zip(&profile.facts)
            .all(|(scenes, fact)| scenes.len() == fact.from.len())
}
fn valid_document(document: &Document) -> bool {
    let mut ids = BTreeSet::new();
    let mut impression_ids = BTreeSet::new();
    let extended = matches!(document.version, 2..=4);
    let visits = matches!(document.version, 3 | 4);
    let profiles = document.version == 4;
    (extended || (document.version == 1 && document.next == 0))
        && (document.version != 3 || document.people.iter().any(|p| p.presence.is_some()))
        && (!profiles || document.people.iter().any(|p| p.profile.is_some()))
        && document.people.len() <= MAX_PEOPLE
        && !document.people.iter().any(|p| {
            !valid_identity(&p.scope.conversation)
                || !valid_identity(&p.scope.participant)
                || p.revision == 0
                || !ids.insert(p.scope.participant.clone())
                || p.name.as_ref().is_some_and(|n| !valid_name(&n.value))
                || (!extended && (!p.impressions.is_empty() || !p.pending.is_empty()))
                || p.presence.as_ref().is_some_and(|v| !visits || !v.valid())
                || p.profile
                    .as_ref()
                    .is_some_and(|v| !profiles || !valid_profile(v))
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
