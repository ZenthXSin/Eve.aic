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
        // 跨通道需要独立、定向、可撤销授权；同一 QQ 内仍由上面的实时隐私配置决定。
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
