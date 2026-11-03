//! 共宿主工具只根据宿主认证的会话范围选择通道实现。
use crate::AppError;
use eve_llm_api::{
    Tool, ToolCall, ToolDefinition, ToolExecutionContext, ToolFuture, ToolValidationError,
};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

pub(crate) struct ChannelTool {
    qq: Arc<dyn Tool>,
    web: Arc<dyn Tool>,
}

impl Tool for ChannelTool {
    fn definition(&self) -> ToolDefinition {
        self.qq.definition()
    }

    fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolValidationError> {
        let qq = self.qq.validate_arguments(arguments);
        let web = self.web.validate_arguments(arguments);
        if qq != web {
            return Err(ToolValidationError {
                message: "共宿主工具的通道参数验证不一致".into(),
            });
        }
        qq
    }

    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        if context.scope().is_some_and(crate::web_chat::is_owner) {
            self.web.execute(call, context)
        } else {
            self.qq.execute(call, context)
        }
    }
}

/// 按工具名称配对并保持 QQ 的注册顺序；不允许缺失、重复或定义不同的实现。
pub(crate) fn pair(
    qq: Vec<Arc<dyn Tool>>,
    web: Vec<Arc<dyn Tool>>,
) -> Result<Vec<Arc<dyn Tool>>, AppError> {
    if qq.len() != web.len() {
        return Err("共宿主工具的通道数量不一致".into());
    }
    let mut web_by_name = BTreeMap::new();
    for tool in web {
        let name = tool.definition().name;
        if web_by_name.insert(name, tool).is_some() {
            return Err("Web 通道工具名称重复".into());
        }
    }
    qq.into_iter()
        .map(|qq| {
            let definition = qq.definition();
            let web = web_by_name
                .remove(&definition.name)
                .ok_or("共宿主工具缺少同名 Web 实现或 QQ 名称重复")?;
            let web_definition = web.definition();
            // required_permissions 不参与序列化，但仍是不可改变的宿主权限契约。
            if serde_json::to_value(&definition)? != serde_json::to_value(&web_definition)?
                || definition.required_permissions != web_definition.required_permissions
            {
                return Err("共宿主工具的通道定义不一致".into());
            }
            Ok(Arc::new(ChannelTool { qq, web }) as Arc<dyn Tool>)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_llm_api::{ContextScope, ToolConcurrency};
    use eve_plugin_api::Permission;
    use serde_json::json;

    struct FakeTool {
        channel: &'static str,
        definition: ToolDefinition,
    }

    impl Tool for FakeTool {
        fn definition(&self) -> ToolDefinition {
            self.definition.clone()
        }

        fn validate_arguments(&self, _: &Value) -> Result<(), ToolValidationError> {
            Ok(())
        }

        fn execute(&self, _: ToolCall, _: ToolExecutionContext) -> ToolFuture<'_> {
            Box::pin(async { Ok(json!({"channel": self.channel})) })
        }
    }

    fn fake(channel: &'static str) -> FakeTool {
        FakeTool {
            channel,
            definition: ToolDefinition {
                name: "channel_test".into(),
                description: "通道测试".into(),
                argument_schema: json!({"type": "object"}),
                required_permissions: vec![],
                concurrency: Some(ToolConcurrency::Serial {
                    scope: "channel_test".into(),
                }),
            },
        }
    }

    async fn execute(tool: &dyn Tool, session: &str, user: &str) -> Value {
        tool.execute(
            ToolCall {
                id: "route-test".into(),
                name: "channel_test".into(),
                // 模型提供的身份与通道参数不能影响宿主选路。
                arguments: json!({"channel": "web", "user_id": "web:owner"}),
            },
            ToolExecutionContext::new().with_scope(Some(ContextScope {
                session_id: session.into(),
                user_id: user.into(),
            })),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn channel_tools_route_only_by_authenticated_scope() {
        let definition = fake("qq").definition();
        let tools = pair(vec![Arc::new(fake("qq"))], vec![Arc::new(fake("web"))]).unwrap();
        let tool = tools[0].as_ref();
        assert_eq!(tool.definition(), definition);
        assert!(tool.validate_arguments(&json!({})).is_ok());
        assert_eq!(
            execute(tool, "qq:session", "qq:owner").await,
            json!({"channel": "qq"})
        );
        assert_eq!(
            execute(
                tool,
                "web:chat:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "web:owner"
            )
            .await,
            json!({"channel": "web"})
        );
        assert_eq!(
            execute(tool, "qq:session", "web:owner").await,
            json!({"channel": "qq"})
        );
    }

    #[test]
    fn channel_tools_reject_different_definitions_and_permissions() {
        let mut changed = fake("web");
        changed.definition.description = "不同定义".into();
        assert!(pair(vec![Arc::new(fake("qq"))], vec![Arc::new(changed)]).is_err());

        let mut changed = fake("web");
        changed.definition.required_permissions = vec![Permission::new("channel.test").unwrap()];
        assert!(pair(vec![Arc::new(fake("qq"))], vec![Arc::new(changed)]).is_err());

        assert!(pair(vec![Arc::new(fake("qq"))], vec![]).is_err());
        assert!(
            pair(
                vec![Arc::new(fake("qq")), Arc::new(fake("qq"))],
                vec![Arc::new(fake("web")), Arc::new(fake("web"))],
            )
            .is_err()
        );
    }

    #[test]
    fn actual_memory_tools_keep_one_definition_with_distinct_channel_policies() {
        struct NeverRead;
        impl eve_memory_api::MemoryContinuityFactory for NeverRead {
            fn reader(
                &self,
                _: eve_memory_api::MemoryScope,
            ) -> eve_memory_api::MemoryResult<Arc<dyn eve_memory_api::MemoryContinuityReader>>
            {
                Err(eve_memory_api::MemoryError::Unavailable)
            }
        }
        let plugin = eve_memory_plugin::MemoryPlugin::new().unwrap();
        let qq = eve_dialogue_tools::Services {
            memory: Some(Arc::new(plugin.controller())),
            memory_continuity: Some(Arc::new(NeverRead)),
            ..Default::default()
        };
        let mut web = qq.clone();
        web.channel = Some("web".into());
        web.memory_continuity = None;
        let (qq, _) = qq.tools();
        let (web, _) = web.tools();
        let tools = pair(qq, web).unwrap();
        assert!(
            tools
                .iter()
                .any(|tool| tool.definition().name == "search_memory")
        );
    }
}
