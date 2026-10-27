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
use std::{
    path::{Component, Path},
    sync::Arc,
};

fn public_tool(name: &str) -> bool {
    matches!(
        name,
        "echo"
            | "send_message"
            | "clear_context"
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
            | "check_practice_draft"
            | "run_practice"
            | "use_skill"
            | "search_reference"
            | "read_reference"
            | "read_source"
            | "search_memory"
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
fn owner_file(path: &str, write: bool) -> bool {
    Path::new(path).components().any(|c| match c {
        Component::Normal(s) => {
            let s = s.to_string_lossy().to_ascii_lowercase();
            write && matches!(s.as_str(), "agent.md" | "agents.md" | "rule.md" | "user.md")
                || matches!(s.as_str(), ".env" | "private")
                || s.contains("secret")
                || s.contains("credential")
                || s.ends_with(".pem")
                || s.ends_with(".key")
        }
        _ => false,
    })
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
        if !(public_tool(&definition.name) || self.public_github && definition.name == "github_api")
        {
            definition.description.push_str(
                " 此管理能力需要主人或管理员授权；普通用户不可调用，也不可通过后台或子代理代办。",
            );
        }
        definition
    }
    fn validate_arguments(&self, args: &Value) -> Result<(), ToolValidationError> {
        self.inner.validate_arguments(args)
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            if context.is_cancel_requested() {
                return Err(ToolExecutionError::Cancelled("本轮已取消".into()));
            }
            let scope = context
                .scope()
                .ok_or_else(|| ToolExecutionError::Failed("管理工具需要可信平台身份".into()))?;
            if scope.user_id == crate::qq_group::USER {
                return Err(ToolExecutionError::Failed(
                    "群聊观察只允许生成聊天回复，不执行工具或代办管理任务".into(),
                ));
            }
            let owner = self.admin.is_owner(scope);
            let administrator = self.admin.is_operator(scope);
            let name = self.inner.definition().name;
            if !(owner
                || administrator
                || public_tool(&name)
                || self.public_github && name == "github_api")
            {
                return Err(ToolExecutionError::Failed("此操作需要主人或管理员授权；普通成员的项目开发权限不包括宿主部署、全局管理、后台派发或定时主动消息".into()));
            }
            // 管理员不能借补丁改动身份规则文件与私密凭据；补丁里的每个路径都要核对。
            if !owner
                && administrator
                && name == "apply_patch"
                && call
                    .arguments
                    .get("patch")
                    .and_then(Value::as_str)
                    .is_some_and(patch_touches_owner_file)
            {
                return Err(ToolExecutionError::Failed(
                    "身份规则文件及私密凭据仅限主人操作".into(),
                ));
            }
            if !owner
                && administrator
                && matches!(
                    name.as_str(),
                    "read_file" | "write_file" | "edit_file" | "list_files" | "search_files"
                )
                && call
                    .arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .is_some_and(|path| {
                        owner_file(path, matches!(name.as_str(), "write_file" | "edit_file"))
                    })
            {
                return Err(ToolExecutionError::Failed(
                    "身份规则文件及私密凭据仅限主人操作".into(),
                ));
            }
            self.inner.execute(call, context).await
        })
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
impl GuardedOutreach {
    fn allowed(&self, session: &eve_session_api::SessionKey) -> bool {
        self.admin.is_operator(&eve_llm_api::ContextScope {
            session_id: session.session_id.clone(),
            user_id: session.user_id.clone(),
        })
    }
}
impl QqOutreach for GuardedOutreach {
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
        if !group_allowed
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
        let query = matches!(
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
        ) || words.clone().next().is_none()
            && matches!(
                head,
                "/budget"
                    | "/model"
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
    fn unknown_tools_and_persistent_side_effects_are_not_public() {
        for name in [
            "set_wakeup_trigger",
            "dispatch_background_task",
            "run_subagent",
            "set_reminder",
            "github_api",
            "new_management_tool",
        ] {
            assert!(!public_tool(name));
        }
        assert!(public_tool("read_reference"));
        for p in [
            "AGENT.md",
            "rules/RULE.md",
            "USER.md",
            "private/token",
            "tls/server.key",
        ] {
            assert!(owner_file(p, true));
        }
        assert!(!owner_file("src/config.rs", true));
        assert!(!owner_file("AGENT.md", false));
        let patch = |body: &str| format!("*** Begin Patch\n{body}\n*** End Patch");
        assert!(!patch_touches_owner_file(&patch(
            "*** Add File: src/a.rs\n+x"
        )));
        assert!(patch_touches_owner_file(&patch(
            "*** Add File: src/a.rs\n+x\n*** Delete File: AGENT.md"
        )));
        assert!(patch_touches_owner_file(&patch(
            "*** Update File: src/a.rs\n*** Move to: private/a.rs\n@@\n-x\n+y"
        )));
        assert!(patch_touches_owner_file("无法解析"));
    }
}
