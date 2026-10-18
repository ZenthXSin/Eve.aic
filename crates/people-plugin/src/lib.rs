//! 有界、独立持久化的人名目录；不合并不同会话，不读取聊天正文或权限。
use eve_people_api::*;
use eve_plugin_api::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

const KEY: &str = "people.v1";
fn unavailable() -> PluginError {
    PluginError::State("人名目录不可用；保留原状态".into())
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u8,
    people: Vec<Person>,
}
struct Loaded {
    document: Document,
    ctx: PluginContext,
}
#[derive(Clone, Default)]
pub struct PeopleController(Arc<Mutex<Option<Loaded>>>);
impl PeopleDirectory for PeopleController {
    fn observe(&self, scope: PersonScope, name: Option<PersonName>) -> PluginResult<()> {
        if !valid_identity(&scope.conversation)
            || !valid_identity(&scope.participant)
            || name.as_ref().is_some_and(|n| !valid_name(&n.value))
        {
            return Err(unavailable());
        }
        let mut inner = self.0.lock().map_err(|_| unavailable())?;
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
                });
            }
        }
        let bytes = serde_json::to_vec(&document).map_err(|_| unavailable())?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err(PluginError::Task("人名目录容量已满；原记录保留".into()));
        }
        if loaded.ctx.state_set(KEY, bytes).is_err() {
            // 后端可能已经提交；关闭所有克隆句柄，重启后以实际磁盘为准。
            *inner = None;
            return Err(unavailable());
        }
        inner.as_mut().ok_or_else(unavailable)?.document = document;
        Ok(())
    }
    fn view(&self, participant: &str) -> PluginResult<Option<PeopleView>> {
        if !valid_identity(participant) {
            return Err(unavailable());
        }
        let inner = self.0.lock().map_err(|_| unavailable())?;
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
            .cloned()
            .collect();
        let truncated = others.len() > MAX_VISIBLE_PEOPLE;
        others.truncate(MAX_VISIBLE_PEOPLE);
        Ok(Some(PeopleView {
            current: current.clone(),
            others,
            truncated,
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
    pub fn controller(&self) -> PeopleController {
        self.controller.clone()
    }
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
                },
                Some(bytes) => {
                    if bytes.len() > MAX_STATE_BYTES {
                        return Err(unavailable());
                    }
                    let document: Document =
                        serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
                    let mut ids = BTreeSet::new();
                    if document.version != 1
                        || document.people.len() > MAX_PEOPLE
                        || document.people.iter().any(|p| {
                            !valid_identity(&p.scope.conversation)
                                || !valid_identity(&p.scope.participant)
                                || p.revision == 0
                                || !ids.insert(p.scope.participant.clone())
                                || p.name.as_ref().is_some_and(|n| !valid_name(&n.value))
                        })
                    {
                        return Err(unavailable());
                    }
                    document
                }
            };
            *self.controller.0.lock().map_err(|_| unavailable())? = Some(Loaded { document, ctx });
            let controller = self.controller.clone();
            Ok(Some(cleanup(move || async move {
                *controller.0.lock().map_err(|_| unavailable())? = None;
                Ok(())
            })))
        })
    }
}
