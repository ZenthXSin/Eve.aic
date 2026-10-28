use crate::{Result, reject};
use eve_llm_api::{Tool, ToolCall, ToolExecutionContext, ToolFuture};
use std::{collections::BTreeMap, sync::Arc};

/// 必须与宿主直接工具调用使用同一策略。两个检查函数均不得产生业务副作用。
/// 工具列表来自可信装配，不能由脚本自行注册；dispatch 仍需检查实时权限撤回。
pub trait AuthorizedToolDispatch: Send + Sync {
    fn preflight_tool(&self, name: &str, context: &ToolExecutionContext) -> Result<()>;
    fn preflight_call(&self, call: &ToolCall, context: &ToolExecutionContext) -> Result<()>;
    /// 步数由脚本运行器统一预留，派发器不得重复扣除。
    fn dispatch(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_>;
}

pub trait ToolAuthorizer: Send + Sync {
    fn authorize(
        &self,
        name: &str,
        arguments: Option<&serde_json::Value>,
        context: &ToolExecutionContext,
    ) -> Result<()>;
}

/// 常规 Tool 装配适配器；不允许省略授权策略或默认 allow。
pub struct CheckedTools {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    authorizer: Arc<dyn ToolAuthorizer>,
}
impl CheckedTools {
    pub fn new(tools: Vec<Arc<dyn Tool>>, authorizer: Arc<dyn ToolAuthorizer>) -> Result<Self> {
        let mut entries = BTreeMap::new();
        for tool in tools {
            let name = tool.definition().name;
            if entries.insert(name, tool).is_some() {
                return Err(reject("重复叶子工具装配"));
            }
        }
        Ok(Self {
            tools: entries,
            authorizer,
        })
    }
}
impl AuthorizedToolDispatch for CheckedTools {
    fn preflight_tool(&self, name: &str, context: &ToolExecutionContext) -> Result<()> {
        if context.scope().is_none() || !self.tools.contains_key(name) {
            return Err(reject("缺少可信身份或叶子工具"));
        }
        self.authorizer.authorize(name, None, context)
    }
    fn preflight_call(&self, call: &ToolCall, context: &ToolExecutionContext) -> Result<()> {
        self.preflight_tool(&call.name, context)?;
        self.tools[&call.name]
            .validate_arguments(&call.arguments)
            .map_err(reject)?;
        self.authorizer
            .authorize(&call.name, Some(&call.arguments), context)
    }
    fn dispatch(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            self.preflight_call(&call, &context)?;
            self.tools[&call.name].execute(call, context).await
        })
    }
}
