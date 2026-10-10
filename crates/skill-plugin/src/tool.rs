//! 对话中可调用的技能工具：用当前用户自己已启用的技能按参数实例化产物，在全新目录中
//! 实际运行验证，并把结论与证据写入技能账本。先保存调用再运行，进程退出时记为中断、不重放。
use eve_llm_api::{
    Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext, ToolExecutionError,
    ToolFuture, ToolValidationError,
};
use eve_practice_api::PracticeRunner;
use eve_skill_api::{
    Arguments, InvocationOutcome, MAX_PARAMETERS, SkillAdmin, SkillRef, instantiate, tool_call_id,
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

pub const SKILL_TOOL_NAME: &str = "use_skill";
const MAX_ARGUMENT_BYTES: usize = 128;

/// 运行环境在宿主装配完成后才就绪，所以通过可后置设置的单元持有。
pub struct SkillTool {
    admin: Arc<dyn SkillAdmin>,
    runner: Arc<OnceLock<Arc<dyn PracticeRunner>>>,
    workspace_root: PathBuf,
}

impl SkillTool {
    pub fn new(
        admin: Arc<dyn SkillAdmin>,
        runner: Arc<OnceLock<Arc<dyn PracticeRunner>>>,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            admin,
            runner,
            workspace_root,
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(1, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
        .max(1)
}

fn failed(message: impl Into<String>) -> ToolExecutionError {
    ToolExecutionError::Failed(message.into())
}

impl Tool for SkillTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: SKILL_TOOL_NAME.into(),
            description: "用当前用户自己已验证并启用的技能，按参数做出一个产物，并在实际运行环境中验证。只有用户明确想要做某样东西、且上下文的 eve.skills.available 中有合适的技能时才调用；skill_id 与参数名、取值范围都以那里列出的为准。返回是否验证通过、每项探测的期望值与实际值、运行环境版本和产物文件；没有验证通过时如实告诉用户。".into(),
            argument_schema: json!({
                "type": "object",
                "properties": {
                    "skill_id": {"type": "string"},
                    "arguments": {"type": "object", "additionalProperties": {"type": "string"}}
                },
                "required": ["skill_id", "arguments"],
                "additionalProperties": false
            }),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial {
                scope: "eve.skill-tool".into(),
            }),
        }
    }

    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        let invalid = |message: &str| ToolValidationError {
            message: message.into(),
        };
        let object = value
            .as_object()
            .filter(|object| object.len() == 2)
            .ok_or_else(|| invalid("参数必须只包含 skill_id 与 arguments"))?;
        let skill_id = object
            .get("skill_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 256)
            .ok_or_else(|| invalid("skill_id 必须是非空字符串"))?;
        let _ = skill_id;
        let arguments = object
            .get("arguments")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("arguments 必须是对象"))?;
        if arguments.len() > MAX_PARAMETERS
            || arguments.values().any(|value| {
                value
                    .as_str()
                    .is_none_or(|text| text.is_empty() || text.len() > MAX_ARGUMENT_BYTES)
            })
        {
            return Err(invalid("arguments 的每个值都必须是非空的短字符串"));
        }
        Ok(())
    }

    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            // 用户身份只取宿主绑定的会话范围，模型参数不能指定别人的技能。
            let owner = context
                .scope()
                .map(|scope| scope.user_id.clone())
                .ok_or_else(|| failed("技能工具只能在有用户身份的对话中使用"))?;
            let runner = self
                .runner
                .get()
                .cloned()
                .ok_or_else(|| failed("实践运行环境尚未就绪"))?;
            let skill_id = call.arguments["skill_id"].as_str().unwrap_or_default();
            let arguments: Arguments = call.arguments["arguments"]
                .as_object()
                .map(|object| {
                    object
                        .iter()
                        .map(|(key, value)| {
                            (key.clone(), value.as_str().unwrap_or_default().into())
                        })
                        .collect()
                })
                .unwrap_or_default();
            let snapshot = self
                .admin
                .snapshot()
                .map_err(|error| failed(error.to_string()))?;
            let entry = snapshot
                .skill(skill_id)
                .filter(|skill| skill.owner == owner)
                .ok_or_else(|| failed("没有这个技能，或它不属于当前用户"))?;
            let version = entry
                .enabled
                .ok_or_else(|| failed("这个技能已停用；用户可以用 /skill 查看与启用"))?;
            let reference = SkillRef {
                skill_id: entry.id.clone(),
                version,
            };
            if entry.runner_id != runner.profile().runner_id {
                return Err(failed("这个技能属于另一个运行环境，当前不能运行"));
            }
            let summary = snapshot
                .summary(&reference)
                .ok_or_else(|| failed("技能模板缺失"))?;
            let template = snapshot
                .source(&reference)
                .and_then(|source| source.template())
                .ok_or_else(|| failed("技能模板缺失"))?;
            let draft = match instantiate(&template.template, &arguments) {
                Ok(draft) => draft,
                Err(issues) => return Err(failed(format!("参数不合规：{}", issues.join("；")))),
            };
            let started = now_ms();
            let id = tool_call_id(&owner, &call.id, started);
            self.admin
                .begin_tool_call(&id, &owner, reference.clone(), arguments.clone(), started)
                .map_err(|error| failed(error.to_string()))?;
            let issues = runner.check(&draft);
            if !issues.is_empty() {
                self.admin
                    .record_tool_call(
                        &id,
                        now_ms().max(started),
                        InvocationOutcome::Rejected,
                        None,
                    )
                    .map_err(|error| failed(error.to_string()))?;
                return Err(failed(format!("运行前检查未通过：{}", issues.join("；"))));
            }
            let workspace = self.workspace_root.join(&id);
            if std::fs::create_dir_all(&workspace).is_err() {
                self.admin
                    .record_tool_call(
                        &id,
                        now_ms().max(started),
                        InvocationOutcome::Abandoned,
                        None,
                    )
                    .map_err(|error| failed(error.to_string()))?;
                return Err(failed("无法创建工作目录"));
            }
            let evidence = runner.run(&draft, &workspace).await;
            let _ = std::fs::remove_dir_all(&workspace);
            let verified = evidence.verified(&draft);
            let outcome = if verified {
                InvocationOutcome::Verified
            } else {
                InvocationOutcome::Failed
            };
            self.admin
                .record_tool_call(&id, now_ms().max(started), outcome, Some(evidence.clone()))
                .map_err(|error| failed(error.to_string()))?;
            Ok(json!({
                "verified": verified,
                "skill": {"id": reference.skill_id, "version": reference.version, "title": summary.title},
                "runtime_version": evidence.runtime_version,
                "warnings": evidence.warnings,
                "probes": evidence.probes.iter().map(|result| json!({
                    "subject": result.probe.subject,
                    "property": result.probe.property,
                    "expected": result.probe.expected,
                    "actual": result.actual,
                    "passed": result.passed,
                })).collect::<Vec<_>>(),
                "files": draft.files.iter().map(|file| json!({"path": file.path, "content": file.content})).collect::<Vec<_>>(),
            }))
        })
    }
}
