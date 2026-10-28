//! QQ 宿主侧群访问：把持久白名单接到通道，并给对话一个按本轮会话定位群的白名单工具。
//!
//! 群号一律取自已校验的消息或回执，工具参数不接受群号；主人身份只看 `Admin` 的绑定判断。
//! 名单读不出来时既不猜放行也不猜拒绝，工具如实报告失败。
use crate::qq_admin::Admin;
use eve_llm_api::{
    ContextScope, Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext,
    ToolExecutionError, ToolFuture, ToolValidationError,
};
use eve_plugin_api::PluginResult;
use eve_qqbot_plugin::{
    QqConversations, QqGroupAccess, QqGroupWhitelist, QqOwnerDirectory, QqOwnerGroup,
};
use eve_session_api::SessionKey;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

/// 认识的群会话上限；与通道的会话目录同量级，超出时淘汰最久未用的。
const MAX_CLAIMS: usize = 4096;
/// 工具返回里最多列出的群号条数，其余只报总数。
const SHOWN: usize = 50;
/// 一次最多写入的群数；再多就不算“顺手放行”，要主人分批来。
const MAX_BATCH: usize = 20;

pub(crate) const ALLOW_TOOL: &str = "set_group_whitelist";

/// 主人目录：只认宿主绑定的主人账号，不看昵称或模型自报。
pub(crate) struct Owners {
    pub(crate) admin: Arc<Admin>,
    pub(crate) conversations: QqConversations,
}

impl QqOwnerDirectory for Owners {
    fn is_owner(&self, user_id: &str) -> PluginResult<bool> {
        // 群与私聊的 OpenID 属于不同命名空间；owner_principals 记的是群标识，
        // 只有能以群会话身份核对时才认，否则交给 owner_accounts 那条路径。
        let Ok(session) = SessionKey::new(user_id, user_id) else {
            return Ok(false);
        };
        let Some(identity) = self.conversations.identity(&session)? else {
            return Ok(false);
        };
        if identity.source != eve_qqbot_plugin::QqInteractionSource::GroupMessage {
            return Ok(false);
        }
        Ok(self.admin.is_owner(&ContextScope {
            session_id: session.session_id.clone(),
            user_id: session.user_id.clone(),
        }))
    }
}

fn failed(message: impl Into<String>) -> ToolExecutionError {
    ToolExecutionError::Failed(message.into())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// 宿主侧群访问：持久名单 + 主人群会话定位。
///
/// 通道用 `with_group_access` 拿到白名单与主人目录，用 `with_group_access_service`
/// 拿到同一个对象的 `QqGroupAccess`；工具也读同一个对象，三处共用一份名单。
pub(crate) struct GroupAccess {
    whitelist: Arc<dyn QqGroupWhitelist>,
    owners: Arc<dyn QqOwnerDirectory>,
    conversations: QqConversations,
    admin: Arc<Admin>,
    /// 主人本人在某个群里说过话的会话记录；只由通道用已校验的群号登记。
    claims: Mutex<HashMap<String, (u64, QqOwnerGroup)>>,
}

impl GroupAccess {
    pub(crate) fn new(
        whitelist: Arc<dyn QqGroupWhitelist>,
        owners: Arc<dyn QqOwnerDirectory>,
        conversations: QqConversations,
        admin: Arc<Admin>,
    ) -> Arc<Self> {
        Arc::new(Self {
            whitelist,
            owners,
            conversations,
            admin,
            claims: Mutex::default(),
        })
    }

    /// 巡检等只需要主人判断的地方共用一个入口。
    pub(crate) fn is_owner(&self, scope: &ContextScope) -> bool {
        self.admin.is_owner(scope)
    }

    pub(crate) fn whitelist(&self) -> Arc<dyn QqGroupWhitelist> {
        self.whitelist.clone()
    }

    pub(crate) fn owners(&self) -> Arc<dyn QqOwnerDirectory> {
        self.owners.clone()
    }

    /// 批量放行主人最近说过话的群：跳过当前会话、重复群号与认不出群号的会话。
    /// 返回（本次新增的群、是否被上限截断、是否有写入）；失败时已写入的不回滚。
    fn recent_owner_groups(&self, current: &SessionKey) -> PluginResult<(Vec<String>, bool, bool)> {
        let mut groups = Vec::new();
        let mut seen = BTreeSet::new();
        let mut changed = false;
        for identity in self.conversations.recent_identities()? {
            if groups.len() >= MAX_BATCH {
                return Ok((groups, true, changed));
            }
            let Some(group) = self.locate(&identity.session) else {
                continue;
            };
            if group.session == *current || !seen.insert(group.group_id.clone()) {
                continue;
            }
            changed = true;
            if self.whitelist.add(&group.group_id, now_ms())? {
                groups.push(group.group_id);
            }
        }
        Ok((groups, false, changed))
    }

    fn claim(&self, session: SessionKey, conversation: String, group_id: String, tick: u64) {
        let Ok(mut claims) = self.claims.lock() else {
            return;
        };
        claims.insert(
            session.session_id.clone(),
            (
                tick,
                QqOwnerGroup {
                    session,
                    conversation,
                    group_id,
                },
            ),
        );
        while claims.len() > MAX_CLAIMS {
            let Some(oldest) = claims
                .iter()
                .min_by_key(|(_, (tick, _))| *tick)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            claims.remove(&oldest);
        }
    }

    fn claimed(&self, session: &SessionKey) -> Option<QqOwnerGroup> {
        let claims = self.claims.lock().ok()?;
        claims
            .get(&session.session_id)
            .filter(|(_, group)| group.session == *session)
            .map(|(_, group)| group.clone())
    }

    /// 本轮会话对应的群：先看通道登记，再回退到已记录的群会话身份，两条路都要求主人本人。
    fn locate(&self, session: &SessionKey) -> Option<QqOwnerGroup> {
        if let Some(group) = self.claimed(session) {
            return Some(group);
        }
        let identity = self.conversations.identity(session).ok()??;
        if identity.source != eve_qqbot_plugin::QqInteractionSource::GroupMessage {
            return None;
        }
        self.owners.is_owner(&session.user_id).ok()?;
        self.group_route(&identity.conversation)
            .map(|group_id| QqOwnerGroup {
                conversation: identity.conversation.clone(),
                session: session.clone(),
                group_id,
            })
    }

    /// 群会话键到真实群号的反查；通道在记录消息时写入。
    fn group_route(&self, conversation: &str) -> Option<String> {
        self.conversations.group_route(conversation)
    }
}

impl crate::qq_health::OwnerCheck for GroupAccess {
    fn is_owner(&self, scope: &ContextScope) -> bool {
        GroupAccess::is_owner(self, scope)
    }
}

impl QqGroupAccess for GroupAccess {
    fn claim_owner_group(
        &self,
        session: SessionKey,
        conversation: String,
        group_id: String,
        tick: u64,
    ) -> PluginResult<()> {
        // 群号来自通道校验过的消息；格式再核一次，异常一律不登记。
        if group_id.is_empty()
            || group_id.len() > 64
            || !group_id
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return Ok(());
        }
        self.claim(session, conversation, group_id, tick);
        Ok(())
    }
    fn owner_group(&self, session: &SessionKey) -> Option<QqOwnerGroup> {
        self.locate(session)
    }
    fn contains(&self, group_id: &str) -> PluginResult<bool> {
        self.whitelist.contains(group_id)
    }
    fn add(&self, group_id: &str) -> PluginResult<bool> {
        self.whitelist.add(group_id, now_ms())
    }
    fn remove(&self, group_id: &str) -> PluginResult<bool> {
        self.whitelist.remove(group_id)
    }
    fn list(&self) -> PluginResult<Vec<String>> {
        self.whitelist.list()
    }
}

/// 对话侧的白名单工具：只在群聊里成立，群号取自本轮会话。
pub(crate) struct AllowTool {
    pub(crate) access: Arc<GroupAccess>,
    pub(crate) admin: Arc<Admin>,
}

impl AllowTool {
    /// 只能由主人本人的群会话发起；群号来自会话，不接受参数。
    fn check(&self, context: &ToolExecutionContext) -> Result<QqOwnerGroup, ToolExecutionError> {
        let ContextScope {
            session_id,
            user_id,
        } = context
            .scope()
            .cloned()
            .ok_or_else(|| failed("群白名单只能在与具体某个人的对话里修改"))?;
        // 群聊观察生成的接话不代表任何个人，不能拿它改名单。
        if user_id == crate::qq_group::USER {
            return Err(failed("群聊观察里不能修改群白名单；请在群里由主人本人发起"));
        }
        let session = SessionKey::new(&session_id, &user_id).map_err(|_| failed("会话身份无效"))?;
        let scope = ContextScope {
            session_id,
            user_id,
        };
        if !self.admin.is_owner(&scope) {
            return Err(failed("只有主人本人能改群白名单；当前平台身份不是主人"));
        }
        self.access
            .locate(&session)
            .ok_or_else(|| failed("当前会话不是群聊，或还没登记过群号；请到群里再说一次"))
    }
}

impl Tool for AllowTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: ALLOW_TOOL.into(),
            description: "管理 QQ 群白名单：所有群默认禁用，群里只有主人本人能跟我说话；放行之后群友发消息我才会回应。action=list 列出已放行的群；action=add 或 remove 只作用于当前所在的这个群；action=add_recent 把主人最近说过话的群（最多 20 个）依次放行，不需要群号。群号一律由会话确定，不接受参数里的群号。只有主人本人能调用，add 与 remove 只能在群里用；结果以实际写入为准，失败时如实说明。".into(),
            argument_schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "add", "remove", "add_recent"]}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial {
                scope: "eve.group-whitelist".into(),
            }),
        }
    }

    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        value
            .as_object()
            .filter(|object| object.len() == 1)
            .and_then(|object| object.get("action"))
            .and_then(Value::as_str)
            .filter(|action| matches!(*action, "list" | "add" | "remove" | "add_recent"))
            .map(|_| ())
            .ok_or_else(|| ToolValidationError {
                message: "参数必须只包含 action，取值为 list、add、remove 或 add_recent".into(),
            })
    }

    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            self.validate_arguments(&call.arguments)
                .map_err(|e| ToolExecutionError::Rejected(e.message))?;
            if context.is_cancel_requested() {
                return Err(ToolExecutionError::Cancelled("白名单操作已取消".into()));
            }
            let action = call
                .arguments
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let whitelist = self.access.whitelist();
            // 列名单不需要定位当前群，但同样只给主人看。
            if action == "list" {
                let scope = context
                    .scope()
                    .cloned()
                    .ok_or_else(|| failed("群白名单只能在有用户身份的对话中查看"))?;
                if !self.admin.is_owner(&scope) {
                    return Err(failed("只有主人本人能看群白名单"));
                }
                return match whitelist.list() {
                    Ok(groups) if groups.is_empty() => Ok(json!({
                        "action": "list",
                        "groups": [],
                        "note": "群白名单现在为空：群里只有主人本人能跟我说话",
                    })),
                    Ok(groups) => {
                        let shown: Vec<_> = groups.iter().take(SHOWN).cloned().collect();
                        Ok(json!({
                            "action": "list",
                            "count": groups.len(),
                            "groups": shown,
                            "truncated": groups.len() > shown.len(),
                        }))
                    }
                    Err(_) => Err(failed("群白名单读取失败；现有名单未确认")),
                };
            }
            let group = self.check(&context)?;
            if action == "add_recent" {
                return match self.access.recent_owner_groups(&group.session) {
                    Ok((groups, truncated, changed)) => {
                        let shown: Vec<_> = groups.iter().take(SHOWN).cloned().collect();
                        Ok(json!({
                            "action": "add_recent",
                            "count": groups.len(),
                            "groups": shown,
                            "changed": changed,
                            "truncated": truncated,
                            "note": if groups.is_empty() {
                                "没有找到其他主人群会话；白名单没有变化"
                            } else {
                                "以下群现在放行了，群友发消息我才会回应"
                            },
                        }))
                    }
                    Err(_) => Err(failed("批量放行失败；已写入的群保持放行，其余未改动")),
                };
            }
            if action == "add" {
                return match whitelist.add(&group.group_id, now_ms()) {
                    Ok(true) => Ok(json!({
                        "action": "add",
                        "group_id": group.group_id,
                        "changed": true,
                        "note": "已放行这个群，群友之后跟我说话都会被回应",
                    })),
                    Ok(false) => Ok(json!({
                        "action": "add",
                        "group_id": group.group_id,
                        "changed": false,
                        "note": "这个群之前就放行了，名单没有变化",
                    })),
                    Err(_) => Err(failed("放行失败；白名单没有改动")),
                };
            }
            match whitelist.remove(&group.group_id) {
                Ok(true) => Ok(json!({
                    "action": "remove",
                    "group_id": group.group_id,
                    "changed": true,
                    "note": "已取消这个群的放行；之后群里只有主人本人能跟我说话",
                })),
                Ok(false) => Ok(json!({
                    "action": "remove",
                    "group_id": group.group_id,
                    "changed": false,
                    "note": "这个群本来就没放行，名单没有变化",
                })),
                Err(_) => Err(failed("取消放行失败；白名单没有改动")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_qqbot_plugin::QqInteractionSource;
    use std::collections::BTreeSet;

    struct MemoryWhitelist(Mutex<BTreeSet<String>>);
    impl QqGroupWhitelist for MemoryWhitelist {
        fn contains(&self, group_id: &str) -> PluginResult<bool> {
            Ok(self.0.lock().unwrap().contains(group_id))
        }
        fn add(&self, group_id: &str, _: u64) -> PluginResult<bool> {
            Ok(self.0.lock().unwrap().insert(group_id.into()))
        }
        fn remove(&self, group_id: &str) -> PluginResult<bool> {
            Ok(self.0.lock().unwrap().remove(group_id))
        }
        fn list(&self) -> PluginResult<Vec<String>> {
            Ok(self.0.lock().unwrap().iter().cloned().collect())
        }
    }

    struct Nobody;
    impl QqOwnerDirectory for Nobody {
        fn is_owner(&self, _: &str) -> PluginResult<bool> {
            Ok(false)
        }
    }

    fn access() -> Arc<GroupAccess> {
        Arc::new(GroupAccess {
            whitelist: Arc::new(MemoryWhitelist(Mutex::default())),
            owners: Arc::new(Nobody),
            conversations: QqConversations::default(),
            admin: crate::qq_admin::Admin::stub(true),
            claims: Mutex::default(),
        })
    }

    #[test]
    fn claimed_group_resolves_only_for_the_same_session() {
        let access = access();
        let session = SessionKey::new("qq:a", "qq:a").unwrap();
        assert!(access.owner_group(&session).is_none());
        access
            .claim_owner_group(session.clone(), "qq:conv".into(), "123456".into(), 1)
            .unwrap();
        assert_eq!(
            access.owner_group(&session).map(|g| g.group_id),
            Some("123456".into())
        );
        let other = SessionKey::new("qq:a", "qq:b").unwrap();
        assert!(access.owner_group(&other).is_none());
    }

    #[test]
    fn invalid_group_ids_are_not_claimed() {
        let access = access();
        let session = SessionKey::new("qq:a", "qq:a").unwrap();
        for bad in ["", "abc", "12 3", "<x>"] {
            access
                .claim_owner_group(session.clone(), "qq:conv".into(), bad.into(), 1)
                .unwrap();
        }
        assert!(access.owner_group(&session).is_none());
    }

    #[test]
    fn whitelist_writes_go_through_the_same_store() {
        let access = access();
        assert!(!access.contains("1").unwrap());
        assert!(access.add("1").unwrap());
        assert!(!access.add("1").unwrap());
        assert!(access.contains("1").unwrap());
        assert_eq!(access.list().unwrap(), vec!["1".to_string()]);
        assert!(access.remove("1").unwrap());
        assert!(!access.contains("1").unwrap());
    }

    #[test]
    fn conversation_fallback_requires_group_source() {
        let access = access();
        let session = SessionKey::new("qq:a", "qq:a").unwrap();
        access.conversations.record(&session, "c2c");
        assert!(access.locate(&session).is_none());
        let _ = QqInteractionSource::GroupMessage;
    }
}
