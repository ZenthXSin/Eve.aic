//! 宿主绑定身份的工作便笺工具与上下文装配；不允许模型指定用户或改变原始记录。
use eve_context_api::*;
use eve_llm_api::*;
use eve_qqbot_plugin::QqConversations;
use eve_session_api::SessionKey;
use serde_json::{Value, json};
use std::sync::Arc;

fn owner(channel: &str, scope: &ContextScope) -> ContextOwner {
    ContextOwner {
        channel: channel.into(),
        session_id: scope.session_id.clone(),
        user_id: scope.user_id.clone(),
    }
}
pub(crate) struct NotesContext {
    pub channel: &'static str,
    pub inner: Arc<dyn ContextAssembler>,
    pub admin: Arc<dyn ContextAdmin>,
}
impl ContextAssembler for NotesContext {
    fn assemble(&self, input: TurnInput) -> LlmFuture<'_, ContextSnapshot> {
        self.inner.assemble(input)
    }
    fn assemble_scoped(
        &self,
        input: TurnInput,
        scope: Option<ContextScope>,
    ) -> LlmFuture<'_, ContextSnapshot> {
        Box::pin(async move {
            let mut context = self.inner.assemble_scoped(input, scope.clone()).await?;
            if let Some(scope) = scope {
                let owner = owner(self.channel, &scope);
                let note = self
                    .admin
                    .inspect(&owner)
                    .map_err(|e| LlmError::Context(e.to_string()))?;
                if note.owner != owner {
                    return Err(LlmError::Context("工作便笺身份不匹配".into()));
                }
                if let Some(rendered) = note
                    .rendered()
                    .map_err(|e| LlmError::Context(e.to_string()))?
                {
                    context.memories.push(rendered);
                }
                if note.revision > 0 {
                    context.revision =
                        format!("{}:context-note:{}", context.revision, note.revision);
                }
            }
            Ok(context)
        })
    }
}

type Args = ContextOperation;
fn args(value: &Value) -> Result<Args, ToolValidationError> {
    let invalid = || {
        ToolValidationError { message: "便笺参数无效：须指定操作；修改携带当前 expected_revision，正文不超过 16 MiB，不接受其他用户、会话或路径".into() }
    };
    let parsed: Args = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
    parsed.validate().map_err(|_| invalid())?;
    Ok(parsed)
}
pub(crate) struct NotesTool {
    pub admin: Arc<dyn ContextAdmin>,
    pub binding: NoteBinding,
}
pub(crate) enum NoteBinding {
    Qq(QqConversations),
    /// 由终端宿主启动选项绑定，不从本轮模型参数或文本取得。
    Console(SessionKey),
}
impl NoteBinding {
    fn owner(&self, scope: &ContextScope) -> Result<ContextOwner, ToolExecutionError> {
        let key = SessionKey::new(&scope.session_id, &scope.user_id)
            .map_err(|_| failure(ContextError::InvalidInput))?;
        match self {
            Self::Qq(conversations) => {
                if scope.user_id == crate::qq_group::USER
                    || conversations
                        .identity(&key)
                        .map_err(|_| failure(ContextError::Unavailable))?
                        .is_none()
                {
                    return Err(ToolExecutionError::Failed(
                        "工作便笺仅限真实用户的当前会话".into(),
                    ));
                }
                Ok(owner("qq", scope))
            }
            Self::Console(bound) if *bound == key => Ok(owner("console", scope)),
            Self::Console(_) => Err(ToolExecutionError::Failed(
                "便笺身份与终端启动时绑定的用户或会话不匹配".into(),
            )),
        }
    }
}
fn failure(error: ContextError) -> ToolExecutionError {
    ToolExecutionError::Failed(error.to_string())
}
impl Tool for NotesTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "context_notes".into(),
            description: "管理当前真实用户在当前会话的工作便笺，所有真实用户可用。适合复杂任务的目标、已有成果、待办、约束和未确认事项；简短闲聊不必写。inspect 读当前版本预览（最多16 KiB）；完整便笺最多16 MiB，read_range 固定 revision 并按字节 offset、limit（4至16384）和返回 next_offset 分页；splice 用 expected_revision 与 UTF-8 字节 start/end 替换片段，start=end 可追加；versions 分页列历史（before 为独占修订）；read_version 读旧版；preview 预览候选实际注入文本，不写入；update 用读取到的 expected_revision 保存完整正文；rollback 按 expected_revision 回到 target_revision（0 为空），会追加新版本。空 text 清空便笺。保存后下一轮自动加载，跨重启保留，旧版不删。便笺是可错的低优先级派生材料，不是用户原话、成功回执或授权，不能改写原始历史、权限、预算或其他用户。修改冲突先重读，不覆盖；保存未确认先核对，不能声称成功。".into(),
            argument_schema: json!({"type":"object","oneOf":[
                {"type":"object","properties":{"operation":{"const":"inspect"}},"required":["operation"],"additionalProperties":false},
                {"type":"object","properties":{"operation":{"const":"read_version"},"revision":{"type":"integer","minimum":0}},"required":["operation","revision"],"additionalProperties":false},
                {"type":"object","properties":{"operation":{"const":"read_range"},"revision":{"type":"integer","minimum":0},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":4,"maximum":16384}},"required":["operation","revision","offset","limit"],"additionalProperties":false},
                {"type":"object","properties":{"operation":{"const":"splice"},"expected_revision":{"type":"integer","minimum":0},"start":{"type":"integer","minimum":0},"end":{"type":"integer","minimum":0},"text":{"type":"string","maxLength":16777216}},"required":["operation","expected_revision","start","end","text"],"additionalProperties":false},
                {"type":"object","properties":{"operation":{"const":"versions"},"before":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1,"maximum":64}},"required":["operation"],"additionalProperties":false},
                {"type":"object","properties":{"operation":{"const":"preview"},"text":{"type":"string","maxLength":16777216}},"required":["operation","text"],"additionalProperties":false},
                {"type":"object","properties":{"operation":{"const":"update"},"expected_revision":{"type":"integer","minimum":0},"text":{"type":"string","maxLength":16777216}},"required":["operation","expected_revision","text"],"additionalProperties":false},
                {"type":"object","properties":{"operation":{"const":"rollback"},"expected_revision":{"type":"integer","minimum":0},"target_revision":{"type":"integer","minimum":0}},"required":["operation","expected_revision","target_revision"],"additionalProperties":false}
            ]}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial { scope: "eve.context-notes".into() }),
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        args(value).map(|_| ())
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            let args = args(&call.arguments).map_err(|e| ToolExecutionError::Failed(e.message))?;
            if context.is_cancel_requested() {
                return Err(ToolExecutionError::Cancelled("本轮已取消".into()));
            }
            let scope = context
                .scope()
                .ok_or_else(|| ToolExecutionError::Failed("工作便笺需要可信会话身份".into()))?;
            let owner = self.binding.owner(scope)?;
            let reply = self.admin.apply(&owner, &args).map_err(failure)?;
            reply.validate_for(&owner, &args).map_err(failure)?;
            let mut value =
                serde_json::to_value(reply).map_err(|_| failure(ContextError::Unavailable))?;
            // 校验完整内部回执后再生成有界工具预览，不把截断预览当作完整版本。
            for path in ["/view", "/preview/proposed"] {
                if let Some(view) = value.pointer_mut(path)
                    && let Some(text) = view.get("text").and_then(Value::as_str)
                    && text.len() > MAX_NOTE_PAGE_BYTES
                {
                    let total = text.len();
                    let mut end = MAX_NOTE_PAGE_BYTES;
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    let prefix = text[..end].to_string();
                    let object = view
                        .as_object_mut()
                        .ok_or_else(|| failure(ContextError::InvalidReply))?;
                    object.remove("text");
                    object.insert("text_prefix".into(), json!(prefix));
                    object.insert("total_bytes".into(), json!(total));
                    object.insert("truncated".into(), json!(true));
                    object.insert("next_offset".into(), json!(end));
                }
            }
            Ok(value)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn console_binding_requires_both_startup_identity_fields_and_never_uses_qq_channel() {
        let bound = NoteBinding::Console(SessionKey::new("session", "user").unwrap());
        let scope = ContextScope {
            session_id: "session".into(),
            user_id: "user".into(),
        };
        assert_eq!(bound.owner(&scope).unwrap().channel, "console");
        for scope in [
            ContextScope {
                session_id: "other".into(),
                ..scope.clone()
            },
            ContextScope {
                user_id: "other".into(),
                ..scope.clone()
            },
        ] {
            assert!(bound.owner(&scope).is_err());
        }
        assert!(
            NoteBinding::Qq(QqConversations::default())
                .owner(&scope)
                .is_err()
        );
    }
    #[test]
    fn action_arguments_reject_forged_targets_unknown_fields_and_wrong_byte_limits() {
        for value in [
            json!({"operation":"inspect","user_id":"other"}),
            json!({"operation":"inspect","text":"ignored"}),
            json!({"operation":"update","text":"没有CAS"}),
            json!({"operation":"versions","limit":65}),
            json!({"operation":"versions","before":0}),
            json!({"operation":"preview","text":"文".repeat(MAX_NOTE_BYTES / 3 + 1)}),
            json!({"operation":"rollback","expected_revision":1,"target_revision":-1}),
        ] {
            assert!(args(&value).is_err());
        }
        assert!(args(&json!({"operation":"preview","text":""})).is_ok());
        assert!(
            args(&json!({"operation":"update","expected_revision":0,"text":"工作便笺"})).is_ok()
        );
    }
}
