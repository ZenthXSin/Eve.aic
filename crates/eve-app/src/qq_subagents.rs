//! 子代理角色的宿主装配；配置属于当前会话用户，不注册额外权限。
use crate::qq_tasks::Late;
use eve_llm_api::{
    Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext, ToolExecutionError,
    ToolFuture, ToolValidationError,
};
use eve_task_api::{SubagentProfile, SubagentProfileDraft};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) const PROFILE_TOOLS: [&str; 3] =
    ["create_subagent", "list_subagents", "delete_subagent"];
pub(crate) fn builtin(id: &str) -> Option<SubagentProfile> {
    let (name, instructions, readonly) = match id {
        "general" => (
            "通用",
            "在当前授权与预算内独立完成委托，先执行并核验，再简洁交付",
            false,
        ),
        "coding" => (
            "编码",
            "负责实现和修复代码：先读取现状，保留用户已有改动，在授权范围内编辑文件、运行构建和测试，核验后报告结果；不把计划当完成，不扩大当前任务授权",
            false,
        ),
        "research" => (
            "调研",
            "只读调研：搜索和读取资料，核对来源后给出结论；不得修改文件、运行命令或执行外部写操作",
            true,
        ),
        "review" => (
            "核验",
            "只读静态核验：检查证据、代码和已保存结果，指出缺失验证及具体问题，不修改文件或运行命令",
            true,
        ),
        _ => return None,
    };
    Some(SubagentProfile {
        id: id.into(),
        revision: 1,
        profile: SubagentProfileDraft {
            name: name.into(),
            instructions: instructions.into(),
            tools: readonly.then(|| {
                [
                    "echo",
                    "list_files",
                    "read_file",
                    "search_files",
                    "recall_memory",
                    "search_knowledge",
                    "list_knowledge",
                    "read_knowledge",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect()
            }),
        },
    })
}
pub(crate) struct ProfileTool {
    pub late: Arc<Late>,
    pub kind: usize,
}
fn invalid() -> ToolValidationError {
    ToolValidationError {
        message: "子代理配置参数无效，请使用当前工具声明的字段与类型".into(),
    }
}
fn failed(message: impl ToString) -> ToolExecutionError {
    ToolExecutionError::Failed(message.to_string())
}
impl Tool for ProfileTool {
    fn definition(&self) -> ToolDefinition {
        let (description, properties, required) = match self.kind {
            0 => (
                "创建一个可复用的自定义子代理角色。说明职责和工具范围，保存后用 run_subagent 的 agent_id 调用。只属于当前用户和会话；没有独立步数预算，不能扩大宿主权限或递归派发。同名且内容相同返回原配置，内容不同会拒绝，应另取名或明确删除后重建。",
                json!({"name":{"type":"string"},"instructions":{"type":"string"},"tools":{"type":["array","null"],"items":{"type":"string"},"uniqueItems":true}}),
                vec!["name", "instructions"],
            ),
            1 => (
                "列出当前会话的子代理角色：内置 general、coding、research、review 及自定义角色。自定义配置每页至多64条，next_after 非空时继续传 after。不会启动子代理。",
                json!({"after":{"type":"string"}}),
                vec![],
            ),
            _ => (
                "删除当前用户和会话的自定义子代理配置，需要 agent_id 和当前 revision；不会取消已冻结配置的在途子代理，不能删除内置角色。",
                json!({"agent_id":{"type":"string"},"revision":{"type":"integer","minimum":1}}),
                vec!["agent_id", "revision"],
            ),
        };
        ToolDefinition {
            name: PROFILE_TOOLS[self.kind].into(),
            description: description.into(),
            argument_schema: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial {
                scope: "subagent-profiles".into(),
            }),
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        let object = value.as_object().ok_or_else(invalid)?;
        match self.kind {
            0 => {
                let draft: SubagentProfileDraft =
                    serde_json::from_value(value.clone()).map_err(|_| invalid())?;
                draft.validate().map_err(|_| invalid())
            }
            1 => {
                if object.keys().any(|k| k != "after")
                    || object.get("after").is_some_and(|v| {
                        !v.as_str()
                            .is_some_and(|s| s.len() <= 64 && s.starts_with("agent-"))
                    })
                {
                    Err(invalid())
                } else {
                    Ok(())
                }
            }
            _ => {
                if object.keys().any(|k| k != "agent_id" && k != "revision")
                    || !object
                        .get("agent_id")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s.starts_with("agent-") && s.len() <= 64)
                    || object
                        .get("revision")
                        .and_then(Value::as_u64)
                        .is_none_or(|v| v == 0)
                {
                    Err(invalid())
                } else {
                    Ok(())
                }
            }
        }
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            let scope = context
                .scope()
                .ok_or_else(|| failed("子代理配置需要可信会话范围"))?;
            let store = self
                .late
                .profiles
                .get()
                .ok_or_else(|| failed("子代理配置服务尚未就绪"))?;
            match self.kind {
                0 => {
                    let draft: SubagentProfileDraft =
                        serde_json::from_value(call.arguments).map_err(failed)?;
                    if let Some(names) = &draft.tools {
                        let worker = self
                            .late
                            .worker
                            .get()
                            .ok_or_else(|| failed("子代理执行器尚未就绪"))?;
                        if names.iter().any(|n| !worker.leaf_tool_names.contains(n)) {
                            return Err(failed(
                                "指定工具未装配为子代理可用工具，请从当前工具列表选择",
                            ));
                        }
                    }
                    let profile = store.create(scope, draft).map_err(failed)?;
                    Ok(json!({"created":true,"profile":profile,"started":false}))
                }
                1 => {
                    let profiles = store
                        .list(scope, call.arguments.get("after").and_then(Value::as_str))
                        .map_err(failed)?;
                    let next_after =
                        (profiles.len() == 64).then(|| profiles.last().unwrap().id.clone());
                    Ok(
                        json!({"builtin":[builtin("general"),builtin("coding"),builtin("research"),builtin("review")],"profiles":profiles,"next_after":next_after}),
                    )
                }
                _ => {
                    let id = call.arguments["agent_id"].as_str().unwrap_or_default();
                    store
                        .delete(scope, id, call.arguments["revision"].as_u64().unwrap_or(0))
                        .map_err(failed)?;
                    Ok(json!({"deleted":true,"agent_id":id}))
                }
            }
        })
    }
}
