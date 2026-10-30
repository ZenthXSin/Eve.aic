//! QQ 个人记忆组合策略：平台身份与显式绑定决定读取范围，模型不能指定其他身份。
use eve_config_api::{
    CONFIG_SERVICE_ID, ConfigField, ConfigKind, ConfigSchema, ConfigServiceHandle, ConfigSnapshot,
};
use eve_memory_api::*;
use eve_plugin_api::{ServiceId, ServiceRegistry};
use eve_qqbot_plugin::{QqConversations, QqInteractionSource};
use eve_session_api::SessionKey;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub(crate) const NAMESPACE: &str = "runtime.memory_continuity";
pub(crate) fn schema() -> ConfigSchema {
    let flag = |value: bool, env: &str| {
        let mut f = ConfigField::new(ConfigKind::Boolean, Some(json!(value)));
        f.environment = Some(env.into());
        f
    };
    ConfigSchema {
        namespace: NAMESPACE.into(),
        version: 1,
        fields: BTreeMap::from([
            (
                "enabled".into(),
                flag(true, "EVE_MEMORY_CONTINUITY_ENABLED"),
            ),
            (
                "share_group_preferences".into(),
                flag(true, "EVE_MEMORY_CONTINUITY_GROUP_PREFERENCES"),
            ),
            (
                "share_private_to_groups".into(),
                flag(false, "EVE_MEMORY_CONTINUITY_PRIVATE_TO_GROUPS"),
            ),
            (
                "use_owner_binding".into(),
                flag(true, "EVE_MEMORY_CONTINUITY_OWNER_BINDING"),
            ),
            (
                "identity_groups".into(),
                ConfigField::new(ConfigKind::String, Some(json!("[]"))),
            ),
        ]),
    }
}
pub(crate) fn identity_groups(snapshot: &ConfigSnapshot) -> Result<Vec<BTreeSet<String>>, ()> {
    let raw: String = snapshot.get("identity_groups").map_err(|_| ())?;
    if raw.len() > 16_384 {
        return Err(());
    }
    let groups: Vec<Vec<String>> = serde_json::from_str(&raw).map_err(|_| ())?;
    if groups.len() > 32 {
        return Err(());
    }
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for group in groups {
        if !(2..=32).contains(&group.len())
            || group
                .iter()
                .any(|id| !eve_people_api::valid_identity(id) || !seen.insert(id.clone()))
        {
            return Err(());
        }
        result.push(group.into_iter().collect());
    }
    Ok(result)
}
#[derive(Clone)]
pub(crate) struct Policy {
    pub(crate) registry: Arc<dyn ServiceRegistry>,
    pub(crate) conversations: QqConversations,
    pub(crate) unified: Option<Arc<dyn UnifiedMemoryAdmin>>,
}
impl MemoryContinuityPolicy for Policy {
    fn grants(
        &self,
        target: &MemoryScope,
        candidates: &[MemoryScope],
    ) -> MemoryResult<Vec<MemoryScopeGrant>> {
        target.validate()?;
        if target.channel != "qq" {
            return Ok(vec![]);
        }
        let unavailable = || MemoryError::Unavailable;
        let service = self
            .registry
            .get(&ServiceId::new(CONFIG_SERVICE_ID).map_err(|_| unavailable())?)
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)?;
        let settings = service
            .value
            .downcast::<ConfigServiceHandle>()
            .map_err(|_| unavailable())?;
        let config = settings
            .0
            .snapshot(NAMESPACE, 1)
            .map_err(|_| unavailable())?;
        let flag = |name| config.get::<bool>(name).map_err(|_| unavailable());
        if !flag("enabled")? {
            return Ok(vec![]);
        }
        let groups = identity_groups(&config).map_err(|_| MemoryError::InvalidInput)?;
        let owners = if flag("use_owner_binding")? {
            let owner = settings
                .0
                .snapshot(crate::qq_people::NAMESPACE, 1)
                .map_err(|_| unavailable())?;
            Some(crate::qq_people::settings(&owner).map_err(|_| MemoryError::InvalidInput)?)
        } else {
            None
        };
        let target_key = SessionKey::new(target.session_id.clone(), target.user_id.clone())
            .map_err(|_| unavailable())?;
        let Some(current) = self
            .conversations
            .identity(&target_key)
            .map_err(|_| unavailable())?
        else {
            return Ok(vec![]);
        };
        let share_groups = flag("share_group_preferences")?;
        let share_private = flag("share_private_to_groups")?;
        let mut grants = Vec::new();
        for source in candidates {
            if source == target || source.channel != target.channel {
                continue;
            }
            let key = SessionKey::new(source.session_id.clone(), source.user_id.clone())
                .map_err(|_| unavailable())?;
            let Some(origin) = self
                .conversations
                .identity(&key)
                .map_err(|_| unavailable())?
            else {
                continue;
            };
            let linked = origin.subject == current.subject
                || owners.as_ref().is_some_and(|owner| {
                    owner.matches_identity(&origin) && owner.matches_identity(&current)
                })
                || groups.iter().any(|group| {
                    group.contains(&source.user_id) && group.contains(&target.user_id)
                });
            if origin.app_id != current.app_id || !linked {
                continue;
            }
            let interactions = match (current.source, origin.source) {
                (QqInteractionSource::DirectMessage, _) => true,
                (QqInteractionSource::GroupMessage, QqInteractionSource::DirectMessage)
                    if share_private =>
                {
                    true
                }
                (QqInteractionSource::GroupMessage, QqInteractionSource::GroupMessage)
                    if current.conversation == origin.conversation =>
                {
                    true
                }
                (QqInteractionSource::GroupMessage, QqInteractionSource::GroupMessage)
                    if share_groups =>
                {
                    false
                }
                _ => continue,
            };
            grants.push(MemoryScopeGrant {
                scope: source.clone(),
                interactions,
            });
        }
        // 同一 QQ 内的显式例外只对仍被宿主认证为同一人的两个窗口生效。
        if let Some(unified) = &self.unified {
            let resolver = PrincipalResolver(self.clone());
            if let Some(principal) = resolver.sharing_principal(target)? {
                let state = unified.snapshot()?;
                for binding in &state.bindings {
                    if binding.revoked
                        || binding.target != *target
                        || binding.source.channel != "qq"
                        || binding.principal != principal
                        || !candidates.contains(&binding.source)
                        || state
                            .bindings
                            .iter()
                            .any(|b| b.id == binding.id && b.revoked)
                        || resolver.sharing_principal(&binding.source)?.as_ref() != Some(&principal)
                    {
                        continue;
                    }
                    if let Some(grant) = grants.iter_mut().find(|g| g.scope == binding.source) {
                        grant.interactions |= binding.interactions;
                    } else {
                        grants.push(MemoryScopeGrant {
                            scope: binding.source.clone(),
                            interactions: binding.interactions,
                        });
                    }
                }
            }
        }
        // 其他通道仍由可信宿主单独认证并登记定向授权。
        if let Some(unified) = &self.unified {
            let cross_channel: Vec<_> = candidates
                .iter()
                .filter(|source| source.channel != target.channel)
                .cloned()
                .collect();
            grants.extend(unified.grants(target, &cross_channel)?);
        }
        Ok(grants)
    }
}

/// 人物印象沿用同一套跨会话授权：QQ 参与者摘要同时是该会话记忆作用域的 session_id 与 user_id。
/// 授权里的“只读偏好”与“含经历”两档对印象不作区分：只要来源可见，就可读取其中此人自述的事。
pub(crate) struct PersonLinks(pub(crate) Policy);
impl eve_people_api::PersonLinks for PersonLinks {
    fn linked(
        &self,
        current: &str,
        candidates: &[String],
    ) -> eve_plugin_api::PluginResult<Vec<String>> {
        let scope = |id: &str| MemoryScope {
            channel: "qq".into(),
            session_id: id.into(),
            user_id: id.into(),
        };
        let candidates: Vec<_> = candidates.iter().map(|id| scope(id)).collect();
        let grants = self
            .0
            .grants(&scope(current), &candidates)
            .map_err(|error| eve_plugin_api::PluginError::Task(error.to_string()))?;
        Ok(grants
            .into_iter()
            .map(|grant| grant.scope.user_id)
            .collect())
    }
}

/// 经验记忆只接受显式可信绑定；QQ群/私聊openID或显示名称相同不构成跨窗口身份授权。
/// 群内读取个人经验还须显式开启私聊到群分享；默认只读当前scope和公开经验。
pub(crate) struct PrincipalResolver(pub(crate) Policy);
impl ExperiencePrincipalResolver for PrincipalResolver {
    fn resolve(&self, scope: &MemoryScope) -> MemoryResult<Option<String>> {
        self.resolve_principal(scope, true)
    }
}
impl PrincipalResolver {
    /// 只用于用户显式授权前的身份核对，不代表已有读取权限。
    pub(crate) fn sharing_principal(&self, scope: &MemoryScope) -> MemoryResult<Option<String>> {
        self.resolve_principal(scope, false)
    }
    fn resolve_principal(
        &self,
        scope: &MemoryScope,
        enforce_privacy: bool,
    ) -> MemoryResult<Option<String>> {
        use ring::digest::{SHA256, digest};
        scope.validate()?;
        if scope.channel != "qq" {
            return Ok(None);
        }
        let unavailable = || MemoryError::Unavailable;
        let service = self
            .0
            .registry
            .get(&ServiceId::new(CONFIG_SERVICE_ID).map_err(|_| unavailable())?)
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)?;
        let settings = service
            .value
            .downcast::<ConfigServiceHandle>()
            .map_err(|_| unavailable())?;
        let config = settings
            .0
            .snapshot(NAMESPACE, 1)
            .map_err(|_| unavailable())?;
        if !config.get::<bool>("enabled").map_err(|_| unavailable())? {
            return Ok(None);
        }
        let key = SessionKey::new(&scope.session_id, &scope.user_id)
            .map_err(|_| MemoryError::InvalidInput)?;
        let Some(identity) = self
            .0
            .conversations
            .identity(&key)
            .map_err(|_| unavailable())?
        else {
            return Ok(None);
        };
        if enforce_privacy
            && identity.source == QqInteractionSource::GroupMessage
            && !config
                .get::<bool>("share_private_to_groups")
                .map_err(|_| unavailable())?
        {
            return Ok(None);
        }
        let groups = identity_groups(&config).map_err(|_| MemoryError::InvalidInput)?;
        let label = if config
            .get::<bool>("use_owner_binding")
            .map_err(|_| unavailable())?
        {
            let owner = settings
                .0
                .snapshot(crate::qq_people::NAMESPACE, 1)
                .map_err(|_| unavailable())?;
            let bound =
                crate::qq_people::settings(&owner).map_err(|_| MemoryError::InvalidInput)?;
            if bound.matches_identity(&identity) {
                // 绑定对象变化时切换身份域，不能让新主人继承旧主人的个人经验。
                let canonical = |name| -> MemoryResult<BTreeSet<String>> {
                    let raw: String = owner.get(name).map_err(|_| unavailable())?;
                    Ok(raw
                        .split(|c: char| c.is_whitespace() || c == ',')
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect())
                };
                Some(
                    serde_json::to_string(&(
                        "owner",
                        canonical("owner_principals")?,
                        canonical("owner_accounts")?,
                    ))
                    .map_err(|_| MemoryError::InvalidInput)?,
                )
            } else {
                None
            }
        } else {
            None
        };
        let label = label.or_else(|| {
            groups
                .iter()
                .find(|g| g.contains(&scope.user_id))
                .map(|g| serde_json::to_string(g).expect("身份集合可序列化"))
        });
        let label =
            label.or_else(|| (!enforce_privacy).then(|| format!("platform:{}", identity.subject)));
        let Some(label) = label else {
            return Ok(None);
        };
        let encoded =
            serde_json::to_vec(&(identity.app_id, label)).map_err(|_| MemoryError::InvalidInput)?;
        let hash: String = digest(&SHA256, &encoded)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok(Some(format!("qq-bound-principal:{hash}")))
    }
}
