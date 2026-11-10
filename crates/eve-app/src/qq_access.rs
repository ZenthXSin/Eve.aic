//! QQ 组合层的统一管理授权；在实际工具入口检查，后台和子代理复用同一服务。
use crate::qq_admin::Admin;
use eve_developer_api::DeveloperAuthorization;
use eve_llm_api::{
    Tool, ToolCall, ToolDefinition, ToolExecutionContext, ToolExecutionError, ToolFuture,
    ToolValidationError,
};
use eve_plugin_api::PluginResult;
use eve_qqbot_plugin::{QqCommandHandler, QqCommandInput};
use eve_qqbot_plugin::{
    QqOutreach, QqOutreachFuture, QqOutreachMessage, QqOutreachMoment, QqOutreachPush,
    QqOutreachResult,
};
use serde_json::Value;
use std::sync::Arc;

fn public_tool(name: &str) -> bool {
    matches!(
        name,
        "echo"
            | "manage_business_permissions"
            | "outreach_status"
            | "set_outreach_enabled"
            | "inspect_tool_permission"
            | "inspect_model_runtime"
            | "inspect_capabilities"
            | "save_script_plugin"
            | "list_script_plugins"
            | "activate_script_plugin"
            | "run_script_plugin"
            | "run_tool_code"
            | "update_script_plugin"
            | "manage_failure_reproduction"
            | "send_message"
            | "wait_for_input"
            | "clear_context"
            | "context_notes"
            | "read_exploration"
            | "save_preference"
            | "read_attachment"
            | "describe_image"
            | "read_raw_message"
            | "group_digest"
            | "recall_group_messages"
            | "send_sticker"
            | "show_budget"
            | "sleep"
            | "task_progress"
            | "list_files"
            | "read_file"
            | "search_files"
            | "write_file"
            | "edit_file"
            | "apply_patch"
            | "run_command"
            | "send_file"
            | "query_file_delivery"
            | "check_practice_draft"
            | "run_practice"
            | "use_skill"
            | "search_reference"
            | "read_reference"
            | "read_source"
            | "search_memory"
            | "experience_memory"
            | "memory_maintenance"
            | "manage_memory_sharing"
            | "read_context_record"
            | "list_interests"
            | "list_goals"
            | "list_knowledge"
            | "list_practice"
            | "list_skills"
            | "list_forged_tools"
            | "list_plans"
            | "list_outreach"
            | "list_background_tasks"
            | "list_wakeup_triggers"
            | "cancel_background_task"
            | "cancel_wakeup_trigger"
            | "cancel_reminder"
            | "note_commitment"
            | "settle_commitment"
    )
}

/// 固定公开资料和公共网页查询不借用任何群成员的操作授权。
fn source_lookup_tool(name: &str) -> bool {
    matches!(name, "search_reference" | "read_reference" | "read_source")
}
pub(crate) fn owner_file(path: &str, _write: bool) -> bool {
    eve_developer_api::credential_path(path)
}
/// 补丁里的任一路径（含移动目标）属于主人专用文件，或补丁无法解析而无法核对时为真。
fn patch_touches_owner_file(patch: &str) -> bool {
    eve_apply_patch::paths(patch).is_none_or(|paths| paths.iter().any(|p| owner_file(p, true)))
}
pub(crate) struct GuardedTool {
    pub inner: Arc<dyn Tool>,
    pub admin: Arc<Admin>,
    /// 仅由宿主开发配置赋值；后端仍检查仓库与凭据。
    pub public_github: bool,
}
impl Tool for GuardedTool {
    fn definition(&self) -> ToolDefinition {
        let mut definition = self.inner.definition();
        if matches!(
            definition.name.as_str(),
            "list_files"
                | "read_file"
                | "search_files"
                | "write_file"
                | "edit_file"
                | "apply_patch"
                | "run_command"
        ) {
            definition.description.push_str(" 普通成员在自己当前会话的独立项目工作区执行，不需要管理员逐项授权。相对路径以此项目根目录为准；run_command 中工作区是 /work，HOME 是 /work/.home，Java 在 PATH，Mindustry 无头服务端只读位于 /opt/eve/mindustry-server.jar（宿主已配置时）。可编写模组、编译、导入并运行验证，交付文件可直接把项目内实际路径交给 send_file，宿主自动复制入库并处理重名。编译通过不能冒充已通过游戏运行验证；无图形客户端时如实区分无头验证与客户端验证。");
        }
        if definition.name == crate::qq_task_wait::TOOL_NAME {
            definition.description.push_str(
                " 仅已授权主人或管理员的当前后台任务可调用；身份与任务由宿主绑定，普通用户和子代理不能代办。",
            );
        } else if !(public_tool(&definition.name)
            || self.public_github && definition.name == "github_api")
        {
            definition.description.push_str(
                " 此管理能力依据宿主绑定的主人或管理员身份开放；后台和子代理沿用原会话身份及实际权限，不能自行扩权。",
            );
        }
        definition
    }
    fn validate_arguments(&self, args: &Value) -> Result<(), ToolValidationError> {
        self.inner.validate_arguments(args)
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            authorize_tool(
                &self.admin,
                self.public_github,
                &self.inner.definition().name,
                Some(&call.arguments),
                &context,
            )?;
            self.inner.execute(call, context).await
        })
    }
}

/// 直接工具与脚本叶子共用同一身份/主人文件边界；None只做声明级检查。
pub(crate) fn authorize_tool(
    admin: &Admin,
    public_github: bool,
    name: &str,
    arguments: Option<&Value>,
    context: &ToolExecutionContext,
) -> Result<(), ToolExecutionError> {
    if context.is_cancel_requested() {
        return Err(ToolExecutionError::Cancelled("本轮已取消".into()));
    }
    let scope = context
        .scope()
        .ok_or_else(|| ToolExecutionError::Failed("管理工具需要可信平台身份".into()))?;
    if scope.user_id == crate::qq_group::USER && !source_lookup_tool(name) {
        return Err(ToolExecutionError::Failed(
            "群聊观察仅可交流与查询公开参考资料，不代办命令、写入或管理任务".into(),
        ));
    }
    // 身份与业务许可独立；主人身份或公共工具也不能越过持久急停。
    if !admin.business_permitted(scope) && !business_control_tool(name) {
        return Err(ToolExecutionError::Failed(
            "当前会话业务权限已撤销、急停或账本不可用；可直接聊天查询或重新解锁，已发生副作用不会自动撤回".into(),
        ));
    }
    let owner = admin.is_owner(scope);
    let administrator = admin.is_operator(scope);
    if !(owner || administrator || public_tool(name) || public_github && name == "github_api") {
        return Err(ToolExecutionError::Failed("此操作需要主人或管理员授权；普通成员的项目开发权限不包括宿主部署、全局管理、后台派发或定时主动消息".into()));
    }
    // 管理员不能借补丁改动实际身份绑定账本与私密凭据；补丁里的每个路径都要核对。
    if !owner
        && administrator
        && name == "apply_patch"
        && arguments
            .and_then(|args| args.get("patch"))
            .and_then(Value::as_str)
            .is_some_and(patch_touches_owner_file)
    {
        return Err(ToolExecutionError::Failed(
            "实际身份绑定账本及私密凭据仅限主人操作".into(),
        ));
    }
    if !owner
        && administrator
        && matches!(
            name,
            "read_file" | "write_file" | "edit_file" | "list_files" | "search_files"
        )
        && arguments
            .and_then(|args| args.get("path"))
            .and_then(Value::as_str)
            .is_some_and(|path| owner_file(path, matches!(name, "write_file" | "edit_file")))
    {
        return Err(ToolExecutionError::Failed(
            "实际身份绑定账本及私密凭据仅限主人操作".into(),
        ));
    }
    Ok(())
}

fn business_control_tool(name: &str) -> bool {
    matches!(
        name,
        "manage_business_permissions"
            | "inspect_tool_permission"
            | "inspect_model_runtime"
            | "inspect_capabilities"
            | "list_background_tasks"
            | "list_wakeup_triggers"
            | "cancel_background_task"
            | "cancel_wakeup_trigger"
            | "cancel_reminder"
            | "task_progress"
    )
}

pub(crate) struct ScriptAuthorizer {
    pub admin: Arc<Admin>,
    pub public_github: bool,
    pub developer: Option<Arc<dyn eve_developer_api::DeveloperBackend>>,
    pub outbox: Option<Arc<crate::qq_files::Outbox>>,
}
impl eve_script_runtime::ToolAuthorizer for ScriptAuthorizer {
    fn authorize(
        &self,
        name: &str,
        arguments: Option<&Value>,
        context: &ToolExecutionContext,
    ) -> Result<(), ToolExecutionError> {
        if name == "restart_self" {
            return Err(ToolExecutionError::Rejected(
                "主动重启必须由当前会话直接调用，不能通过脚本插件代办".into(),
            ));
        }
        authorize_tool(&self.admin, self.public_github, name, arguments, context)?;
        if name == "send_file"
            && let Some(arguments) = arguments
        {
            self.outbox
                .as_ref()
                .ok_or_else(|| ToolExecutionError::Rejected("当前通道未装配文件投递服务".into()))?
                .preflight_send(arguments, context)?;
        }
        if let Some(arguments) = arguments
            && let Some(operation) = eve_developer_tools::operation_for_tool(name, arguments)
        {
            let operation = operation.map_err(|e| ToolExecutionError::Rejected(e.message))?;
            self.developer
                .as_ref()
                .ok_or_else(|| ToolExecutionError::Rejected("开发后端未装配".into()))?
                .preflight(&operation, context)?;
        }
        Ok(())
    }
}

pub(crate) struct GuardedCommands {
    pub inner: Arc<dyn QqCommandHandler>,
    pub admin: Arc<Admin>,
}

pub(crate) struct GuardedOutreach {
    pub inner: Arc<dyn QqOutreach>,
    pub admin: Arc<Admin>,
}
pub(crate) struct ChannelToolPolicy;
impl eve_llm_api::ToolScopePolicy for ChannelToolPolicy {
    fn tools_allowed(&self, _scope: &eve_llm_api::ContextScope) -> bool {
        true
    }
    fn tool_allowed(&self, scope: &eve_llm_api::ContextScope, name: &str) -> bool {
        if scope.user_id == crate::qq_group::USER {
            return source_lookup_tool(name);
        }
        !matches!(name, "send_message" | "wait_for_input" | "send_sticker")
            || (scope.session_id.starts_with("qq:") && scope.session_id == scope.user_id)
    }
    fn tool_allowed_without_review(&self, scope: &eve_llm_api::ContextScope, name: &str) -> bool {
        scope.session_id.starts_with("qq:")
            && scope.session_id == scope.user_id
            && (source_lookup_tool(name)
                || matches!(
                    name,
                    "read_raw_message"
                        | "inspect_capabilities"
                        | "inspect_tool_permission"
                        | "inspect_model_runtime"
                ))
    }
}
impl GuardedOutreach {
    fn allowed(&self, session: &eve_session_api::SessionKey) -> bool {
        self.admin.is_operator(&eve_llm_api::ContextScope {
            session_id: session.session_id.clone(),
            user_id: session.user_id.clone(),
        })
    }
}
impl GuardedOutreach {
    fn authored_allowed(&self, push: &QqOutreachPush) -> PluginResult<bool> {
        let Some(session) = push.session.as_ref() else {
            return Ok(false);
        };
        if push.user_id != "eve-chime"
            || session.user_id != "eve-chime"
            || !self.admin.business_permitted(&eve_llm_api::ContextScope {
                session_id: session.session_id.clone(),
                user_id: session.user_id.clone(),
            })
        {
            return Ok(false);
        }
        Ok(self
            .inner
            .group_chime(&push.message.id)?
            .is_some_and(|meta| {
                meta.session == *session
                    && meta
                        .anchor
                        .as_deref()
                        .is_some_and(eve_qqbot_plugin::valid_message_id)
                    && meta.parts.as_slice() == [(push.message.text.clone(), 0)]
            }))
    }
    fn candidate_allowed(&self, candidate: &eve_qqbot_plugin::QqOutreachCandidate) -> bool {
        let fallback =
            eve_session_api::SessionKey::new(&candidate.user_id, &candidate.user_id).ok();
        candidate
            .session
            .as_ref()
            .or(fallback.as_ref())
            .is_some_and(|session| {
                self.admin.business_permitted(&eve_llm_api::ContextScope {
                    session_id: session.session_id.clone(),
                    user_id: session.user_id.clone(),
                }) && (self.allowed(session)
                    || self.inner.ordinary_candidate(candidate).unwrap_or(false))
            })
    }
}
impl QqOutreach for GuardedOutreach {
    fn authored_pushes(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> PluginResult<eve_qqbot_plugin::QqAuthoredPushPage> {
        let mut page = self.inner.authored_pushes(after, limit)?;
        if !page.valid(after, limit) {
            return Err(eve_plugin_api::PluginError::State("群发言分页无效".into()));
        }
        page.pushes
            .retain(|push| match self.authored_allowed(push) {
                Ok(allowed) => allowed,
                Err(_) => {
                    eprintln!("Eve 群发言资料暂不可用");
                    false
                }
            });
        Ok(page)
    }
    fn next_authored_push(&self) -> PluginResult<Option<QqOutreachPush>> {
        Ok(self.authored_pushes(None, 64)?.pushes.into_iter().next())
    }
    fn claim_authored_push(&self, expected: &QqOutreachPush) -> PluginResult<bool> {
        if !self.authored_allowed(expected)? {
            return Ok(false);
        }
        self.inner.claim_authored_push(expected)
    }
    fn candidates(&self) -> PluginResult<Vec<eve_qqbot_plugin::QqOutreachCandidate>> {
        Ok(self
            .inner
            .candidates()?
            .into_iter()
            .filter(|c| self.candidate_allowed(c))
            .collect())
    }
    fn passive_candidates(
        &self,
        session: &eve_session_api::SessionKey,
        anchor: &str,
    ) -> PluginResult<Vec<eve_qqbot_plugin::QqOutreachCandidate>> {
        if !self.allowed(session) {
            return Ok(vec![]);
        }
        Ok(self
            .inner
            .passive_candidates(session, anchor)?
            .into_iter()
            .filter(|c| c.session.as_ref() == Some(session) && self.candidate_allowed(c))
            .collect())
    }
    fn claim_passive_candidate(
        &self,
        candidate: &eve_qqbot_plugin::QqOutreachCandidate,
        text: &str,
        anchor: &str,
    ) -> PluginResult<Option<QqOutreachPush>> {
        if !candidate.session.as_ref().is_some_and(|s| self.allowed(s))
            || !self.candidate_allowed(candidate)
        {
            return Ok(None);
        }
        self.inner.claim_passive_candidate(candidate, text, anchor)
    }
    fn claim_candidate(
        &self,
        candidate: &eve_qqbot_plugin::QqOutreachCandidate,
        text: &str,
        _anchor: &str,
    ) -> PluginResult<Option<QqOutreachPush>> {
        if !self.candidate_allowed(candidate) {
            return Ok(None);
        }
        self.inner.claim_candidate(candidate, text, _anchor)
    }
    fn silence_candidate(
        &self,
        candidate: &eve_qqbot_plugin::QqOutreachCandidate,
    ) -> PluginResult<()> {
        if !self.candidate_allowed(candidate) {
            return Ok(());
        }
        self.inner.silence_candidate(candidate)
    }

    fn group_chime(
        &self,
        id: &str,
    ) -> PluginResult<Option<eve_qqbot_plugin::QqGroupChimeDelivery>> {
        self.inner.group_chime(id)
    }

    fn due(&self, session: &eve_session_api::SessionKey) -> PluginResult<bool> {
        if !self.allowed(session) {
            return Ok(false);
        }
        self.inner.due(session)
    }
    fn judge(&self, moment: QqOutreachMoment) -> QqOutreachFuture {
        if !self.allowed(&moment.session) {
            return Box::pin(async { Ok(false) });
        }
        self.inner.judge(moment)
    }
    fn attach(
        &self,
        session: &eve_session_api::SessionKey,
        message_id: &str,
    ) -> PluginResult<Option<QqOutreachMessage>> {
        if !self.allowed(session) {
            return Ok(None);
        }
        self.inner.attach(session, message_id)
    }
    fn next_push(&self) -> PluginResult<Option<QqOutreachPush>> {
        let Some(push) = self.inner.next_push()? else {
            return Ok(None);
        };
        let fallback = eve_session_api::SessionKey::new(&push.user_id, &push.user_id).ok();
        let group = self.inner.group_chime(&push.message.id)?;
        let group_allowed = group.as_ref().is_some_and(|meta| {
            meta.session.user_id == "eve-chime" && push.session.as_ref() == Some(&meta.session)
        });
        let ordinary = self.inner.ordinary_direct_invitation(&push.message.id)?;
        let ordinary_allowed = ordinary.as_ref().is_some_and(|session| {
            push.session.as_ref() == Some(session) && push.user_id == session.user_id
        });
        if !push
            .session
            .as_ref()
            .or(fallback.as_ref())
            .is_some_and(|session| {
                self.admin.business_permitted(&eve_llm_api::ContextScope {
                    session_id: session.session_id.clone(),
                    user_id: session.user_id.clone(),
                })
            })
        {
            self.inner
                .delivered(&push.message.id, QqOutreachResult::NotSent)?;
            return Ok(None);
        }
        if !group_allowed
            && !ordinary_allowed
            && !push
                .session
                .as_ref()
                .or(fallback.as_ref())
                .is_some_and(|s| self.allowed(s))
        {
            self.inner
                .delivered(&push.message.id, QqOutreachResult::NotSent)?;
            return Ok(None);
        }
        Ok(Some(push))
    }
    fn delivered(&self, id: &str, result: QqOutreachResult) -> PluginResult<()> {
        self.inner.delivered(id, result)
    }
}
impl QqCommandHandler for GuardedCommands {
    fn handle(&self, input: QqCommandInput<'_>) -> PluginResult<Option<String>> {
        let text = input.text.trim();
        let mut words = text.split_whitespace();
        let head = words.next().unwrap_or_default();
        let own_outreach_preference = head == "/outreach"
            && matches!(
                words.clone().collect::<Vec<_>>().as_slice(),
                ["off"] | ["on"]
            );
        let query = own_outreach_preference
            || matches!(
                head,
                "/admin"
                    | "/bind"
                    | "/help"
                    | "/status"
                    | "/name"
                    | "/cancel"
                    | "/mind"
                    | "/memories"
                    | "/recall"
                    | "/knowledge"
                    | "/practice"
                    | "/plans"
                    | "/memory-candidates"
                    | "/memory-decision"
                    | "/gaps"
            )
            || words.clone().next().is_none()
                && matches!(
                    head,
                    "/budget"
                        | "/model"
                        | "/effort"
                        | "/memories"
                        | "/recall"
                        | "/interests"
                        | "/knowledge"
                        | "/practice"
                        | "/skills"
                        | "/tools"
                        | "/goals"
                        | "/mind"
                        | "/plans"
                        | "/outreach"
                        | "/clarify"
                )
            || matches!(
                head,
                "/train" | "/self-learning" | "/outreach" | "/clarify" | "/segment" | "/budget"
            ) && words.collect::<Vec<_>>() == ["status"];
        let scope = eve_llm_api::ContextScope {
            session_id: input.session.session_id.clone(),
            user_id: input.session.user_id.clone(),
        };
        if head.starts_with('/')
            && !self.admin.business_permitted(&scope)
            && !matches!(head, "/admin" | "/help" | "/status" | "/cancel")
        {
            return Ok(Some(
                "当前会话业务权限已撤销或急停；请直接聊天查询或重新解锁，管理命令不能绕过暂停。"
                    .into(),
            ));
        }
        if head.starts_with('/') && !query && !self.admin.is_operator(&scope) {
            return Ok(Some(
                "此管理操作需要主人或管理员授权。普通聊天和只读查询仍可使用。".into(),
            ));
        }
        self.inner.handle(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_node_is_operator_only_and_respects_stop_and_business_failure() {
        let directory = tempfile::tempdir().unwrap();
        let admin = Admin::open(
            directory.path(),
            eve_qqbot_plugin::QqConversations::default(),
            std::collections::BTreeSet::from(["administrator".into()]),
            eve_kernel::KernelServices::default().registry,
        )
        .unwrap();
        let context = |user: &str| {
            ToolExecutionContext::new().with_scope(Some(eve_llm_api::ContextScope {
                session_id: "s".into(),
                user_id: user.into(),
            }))
        };
        let name = eve_task_api::WORKSPACE_NODE_TEST_TOOL;
        assert!(!public_tool(name));
        assert!(authorize_tool(&admin, false, name, None, &context("member")).is_err());
        let operator = context("administrator");
        assert!(authorize_tool(&admin, false, name, None, &operator).is_ok());
        operator.cancellation_handle().cancel();
        assert!(matches!(
            authorize_tool(&admin, false, name, None, &operator),
            Err(ToolExecutionError::Cancelled(_))
        ));
        admin.business.fail_closed();
        assert!(authorize_tool(&admin, false, name, None, &context("administrator")).is_err());
    }

    #[test]
    fn source_queries_remain_available_in_legacy_and_group_conversations() {
        use eve_llm_api::ToolScopePolicy;
        let directory = tempfile::tempdir().unwrap();
        let admin = Admin::open(
            directory.path(),
            eve_qqbot_plugin::QqConversations::default(),
            Default::default(),
            eve_kernel::KernelServices::default().registry,
        )
        .unwrap();
        let group = eve_llm_api::ContextScope {
            session_id: "qq:group".into(),
            user_id: crate::qq_group::USER.into(),
        };
        let legacy = eve_llm_api::ContextScope {
            session_id: "qq:user".into(),
            user_id: "qq:user".into(),
        };
        let policy = ChannelToolPolicy;
        assert!(policy.tools_allowed(&group));
        for name in ["search_reference", "read_reference", "read_source"] {
            assert!(policy.tool_allowed(&group, name));
            assert!(policy.tool_allowed_without_review(&legacy, name));
            assert!(
                authorize_tool(
                    &admin,
                    false,
                    name,
                    None,
                    &ToolExecutionContext::new().with_scope(Some(group.clone()))
                )
                .is_ok()
            );
        }
        for name in [
            "run_command",
            "write_file",
            "read_file",
            "run_tool_code",
            "dispatch_background_task",
            "send_message",
            "read_raw_message",
        ] {
            assert!(!policy.tool_allowed(&group, name));
            assert!(
                authorize_tool(
                    &admin,
                    false,
                    name,
                    None,
                    &ToolExecutionContext::new().with_scope(Some(group.clone()))
                )
                .is_err()
            );
        }
        assert!(policy.tool_allowed_without_review(&legacy, "read_raw_message"));
        assert!(!policy.tool_allowed_without_review(&legacy, "run_command"));
        assert!(!policy.tool_allowed_without_review(&group, "read_raw_message"));
    }
    #[test]
    fn resolved_file_permissions_allow_project_sources_and_keep_real_credentials_private() {
        let directory = tempfile::tempdir().unwrap();
        let admin = Admin::open(
            directory.path(),
            eve_qqbot_plugin::QqConversations::default(),
            std::collections::BTreeSet::from(["administrator".into()]),
            eve_kernel::KernelServices::default().registry,
        )
        .unwrap();
        let administrator = eve_llm_api::ContextScope {
            session_id: "s".into(),
            user_id: "administrator".into(),
        };
        let member = eve_llm_api::ContextScope {
            session_id: "s".into(),
            user_id: "member".into(),
        };
        for name in [
            "AGENT.md",
            "AGENTS.md",
            "USER.md",
            "rules/RULE.md",
            "src/credential.rs",
            "src/secret_parser.rs",
            "src/credentials/mod.rs",
            "src/credentials",
            "tests/secrets",
            "src/credential_schema.json",
            "docs/secret-handling.txt",
            "docs/secret-handling.md",
            ".cargo/config.toml",
        ] {
            let path = directory.path().join(name);
            assert!(admin.permits_file(&administrator, &path, true), "{name}");
            assert!(admin.permits_file(&administrator, &path, false));
            assert!(admin.permits_file(&member, &path, true));
        }
        for name in [
            ".env",
            ".ssh/id_ed25519",
            "credentials.json",
            "admin-bindings.json",
            "business-permissions.json",
            ".config/gh/hosts.yml",
            ".kube/config",
        ] {
            let path = directory.path().join(name);
            assert!(!admin.permits_file(&administrator, &path, true), "{name}");
            assert!(!admin.permits_file(&administrator, &path, false), "{name}");
        }
        assert!(admin.permits_file(&administrator, &directory.path().join("source.rs"), true));
    }

    #[test]
    fn script_preflight_reuses_backend_and_owner_file_rules_without_execution() {
        use eve_script_runtime::ToolAuthorizer;
        struct Deny;
        impl eve_developer_api::DeveloperBackend for Deny {
            fn preflight(
                &self,
                _: &eve_developer_api::DeveloperOperation,
                _: &ToolExecutionContext,
            ) -> Result<(), ToolExecutionError> {
                Err(ToolExecutionError::Rejected("实际开发后端拒绝".into()))
            }
            fn execute(
                &self,
                _: eve_developer_api::DeveloperOperation,
                _: ToolExecutionContext,
            ) -> ToolFuture<'_> {
                panic!("预检不能执行工具")
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let services = eve_kernel::KernelServices::default();
        let admin = Admin::open(
            directory.path(),
            eve_qqbot_plugin::QqConversations::default(),
            std::collections::BTreeSet::from(["administrator".into()]),
            services.registry,
        )
        .unwrap();
        let authorizer = ScriptAuthorizer {
            admin,
            public_github: false,
            developer: Some(Arc::new(Deny)),
            outbox: Some(Arc::new(
                crate::qq_files::Outbox::open(directory.path().join("outbox")).unwrap(),
            )),
        };
        let member = ToolExecutionContext::new().with_scope(Some(eve_llm_api::ContextScope {
            session_id: "s".into(),
            user_id: "member".into(),
        }));
        assert!(
            authorizer
                .authorize("save_script_plugin", None, &member)
                .is_ok()
        );
        assert!(authorizer.authorize("run_subagent", None, &member).is_err());
        let administrator =
            ToolExecutionContext::new().with_scope(Some(eve_llm_api::ContextScope {
                session_id: "s".into(),
                user_id: "administrator".into(),
            }));
        for arguments in [None, Some(serde_json::json!({}))] {
            assert!(
                authorizer
                    .authorize("restart_self", arguments.as_ref(), &administrator)
                    .is_err()
            );
        }
        let error = authorizer
            .authorize(
                "run_command",
                Some(&serde_json::json!({"program":"true"})),
                &administrator,
            )
            .unwrap_err();
        assert!(error.to_string().contains("实际开发后端拒绝"));
        let error = authorizer
            .authorize(
                "read_file",
                Some(&serde_json::json!({"path":"private/token"})),
                &administrator,
            )
            .unwrap_err();
        assert!(error.to_string().contains("仅限主人"));
    }

    #[test]
    fn unknown_tools_and_persistent_side_effects_are_not_public() {
        for name in [
            "set_wakeup_trigger",
            "dispatch_background_task",
            "run_subagent",
            "set_reminder",
            "github_api",
            "restart_self",
            "new_management_tool",
        ] {
            assert!(!public_tool(name));
        }
        assert!(public_tool("read_reference"));
        for p in [
            "private/token",
            "tls/server.key",
            "admin-bindings.json",
            "business-permissions.json",
            ".ssh/id_ed25519",
            "credentials.json",
        ] {
            assert!(owner_file(p, true));
        }
        for p in [
            "src/config.rs",
            "AGENT.md",
            "rules/RULE.md",
            "USER.md",
            "src/credential.rs",
            "src/secret_parser.rs",
            "src/credentials/mod.rs",
            "src/credentials",
            "tests/secrets",
            "src/credential_schema.json",
            "docs/secret-handling.txt",
            "tests/secrets/parser.rs",
        ] {
            assert!(!owner_file(p, true), "{p}");
            assert!(!owner_file(p, false), "{p}");
        }
        let patch = |body: &str| format!("*** Begin Patch\n{body}\n*** End Patch");
        assert!(!patch_touches_owner_file(&patch(
            "*** Add File: src/a.rs\n+x"
        )));
        assert!(!patch_touches_owner_file(&patch(
            "*** Add File: src/a.rs\n+x\n*** Delete File: AGENT.md"
        )));
        assert!(!patch_touches_owner_file(&patch(
            "*** Update File: src/a.rs\n*** Move to: src/credentials/mod.rs\n@@\n-x\n+y"
        )));
        assert!(patch_touches_owner_file(&patch(
            "*** Delete File: admin-bindings.json"
        )));
        assert!(patch_touches_owner_file(&patch(
            "*** Update File: src/a.rs\n*** Move to: private/a.rs\n@@\n-x\n+y"
        )));
        assert!(patch_touches_owner_file("无法解析"));
    }

    #[test]
    fn natural_management_tools_use_live_identity_and_own_outreach_preferences_are_public() {
        let directory = tempfile::tempdir().unwrap();
        let admin = Admin::open(
            directory.path(),
            eve_qqbot_plugin::QqConversations::default(),
            std::collections::BTreeSet::from(["administrator".into()]),
            eve_kernel::KernelServices::default().registry,
        )
        .unwrap();
        let member = ToolExecutionContext::new().with_scope(Some(eve_llm_api::ContextScope {
            session_id: "a".into(),
            user_id: "member".into(),
        }));
        for name in ["manage_skill", "manage_forged_tool"] {
            assert!(authorize_tool(&admin, false, name, None, &member).is_err());
        }
        for name in ["outreach_status", "set_outreach_enabled"] {
            assert!(authorize_tool(&admin, false, name, None, &member).is_ok());
        }
    }
}
