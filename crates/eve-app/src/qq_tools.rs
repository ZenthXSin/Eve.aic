//! QQ 宿主的工具查看与管理：聊天工具和兼容命令共用归属、版本与账本检查。
//! 可查看回放验证、启用记录与调用，停用、启用某个已验证版本或回退到上一版本。
//! 工具只属于来源用户；变更只改变启用版本并留下记录，不删除任何版本。
use crate::AppError;
use eve_llm_api::{
    Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext, ToolExecutionError,
    ToolFuture, ToolValidationError,
};
use eve_plugin_api::{PluginError, PluginResult};
use eve_qqbot_plugin::{QqCommandHandler, QqCommandInput};
use eve_toolforge_api::{
    Actor, CheckRule, ForgeOutput, ForgeStatus, ForgedTool, MAX_CALLS, ToolAdmin, ToolError,
    ToolSnapshot, Verification,
};
use serde_json::{Value, json};
use std::{
    fmt::Write,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

const DISABLED: &str = "工具锻造未启用。";
const EMPTY: &str = "还没有锻造的工具。同一运行环境中反复出现的问题，Eve 会尝试锻造成运行前检查，用实践中的真实草稿回放验证通过才启用。";
const HELP: &str = "直接告诉我你想查看、停用、启用或回退哪个锻造工具即可，我会先查真实版本再处理。也可使用 /tools、/tool 工具ID、/tool disable|rollback 工具ID、/tool enable 工具ID 版本。";
const NOT_FOUND: &str = "没有找到属于你的这个锻造工具；可以先让我查看你的工具列表。";
const CALLS_FULL: &str =
    "检查调用记录已满：锻造的检查暂停使用，实践照常实际运行验证；已有记录保留。";
const SHOWN_FORGES: usize = 3;
const SHOWN_CALLS: usize = 3;
const SHOWN_CHANGES: usize = 5;

enum Command<'a> {
    List,
    Show(&'a str),
    Disable(&'a str),
    Enable(&'a str, u32),
    Rollback(&'a str),
    Help,
}

fn parse(text: &str) -> Option<Command<'_>> {
    let mut words = text.split_whitespace();
    match words.next()? {
        "/tools" => Some(match words.next() {
            None => Command::List,
            Some(_) => Command::Help,
        }),
        "/tool" => {
            let words: Vec<&str> = words.collect();
            Some(match words.as_slice() {
                ["disable", id] => Command::Disable(id),
                ["rollback", id] => Command::Rollback(id),
                ["enable", id, version] => match version.parse() {
                    Ok(version) => Command::Enable(id, version),
                    Err(_) => Command::Help,
                },
                [id] if !matches!(*id, "disable" | "enable" | "rollback") => Command::Show(id),
                _ => Command::Help,
            })
        }
        _ => None,
    }
}

pub(crate) struct Commands {
    tools: Option<Arc<dyn ToolAdmin>>,
}
impl Commands {
    pub(crate) fn disabled() -> Arc<dyn QqCommandHandler> {
        Arc::new(Self { tools: None })
    }
    pub(crate) fn enabled(tools: Arc<dyn ToolAdmin>) -> Arc<dyn QqCommandHandler> {
        Arc::new(Self { tools: Some(tools) })
    }
}

fn failure_text() -> PluginError {
    PluginError::State("工具记录暂时不可用".into())
}

fn now_ms() -> Result<u64, AppError> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

#[derive(Clone, Copy)]
enum ManagementError {
    NotFound,
    InvalidVersion,
    NoPreviousVersion,
    LimitReached,
    Unavailable,
}
impl ManagementError {
    fn message(self) -> &'static str {
        match self {
            Self::NotFound => NOT_FOUND,
            Self::InvalidVersion => "没有这个已验证版本。",
            Self::NoPreviousVersion => "没有可以回退的更早版本。",
            Self::LimitReached => "启用记录已满；保留原记录，未做更改。",
            Self::Unavailable => "工具记录暂时不可用；未确认更改成功。",
        }
    }
}

/// 两个入口调用同一实现；归属只来自宿主身份，版本只来自真实的已验证账本。
fn execute_command(
    tools: &dyn ToolAdmin,
    owner: &str,
    command: Command<'_>,
) -> Result<Value, ManagementError> {
    let snapshot = tools.snapshot().map_err(|_| ManagementError::Unavailable)?;
    let owned = |id: &str| {
        snapshot
            .tool(id)
            .filter(|tool| tool.owner == owner)
            .ok_or(ManagementError::NotFound)
    };
    let change = |id: &str, enabled: Option<u32>| -> Result<Value, ManagementError> {
        let previous = owned(id)?;
        if enabled.is_some_and(|version| previous.version(version).is_none()) {
            return Err(ManagementError::InvalidVersion);
        }
        let at = now_ms().map_err(|_| ManagementError::Unavailable)?;
        let tool =
            tools
                .set_enabled(id, at, Actor::Owner, enabled)
                .map_err(|error| match error {
                    ToolError::InvalidInput => ManagementError::InvalidVersion,
                    ToolError::LimitReached => ManagementError::LimitReached,
                    _ => ManagementError::Unavailable,
                })?;
        let name = title(&tool);
        let message = match tool.enabled {
            Some(version) => format!("已启用「{name}」第 {version} 版。"),
            None => {
                format!("已停用「{name}」；之后的实践不会再用它检查，新验证的版本也不会自动启用。")
            }
        };
        Ok(json!({"changed": previous.enabled != tool.enabled, "tool": tool, "message": message}))
    };
    match command {
        Command::Help => Ok(json!({"message": HELP})),
        Command::List => Ok(json!({
            "tools": snapshot.tools_for(owner).collect::<Vec<_>>(),
            "message": list(&snapshot, owner),
        })),
        Command::Show(id) => {
            let tool = owned(id)?;
            Ok(json!({"tool": tool, "message": show(&snapshot, tool)}))
        }
        Command::Disable(id) => change(id, None),
        Command::Enable(id, version) => change(id, Some(version)),
        Command::Rollback(id) => {
            let tool = owned(id)?;
            let previous = tool.enabled.and_then(|current| {
                tool.versions
                    .iter()
                    .map(|version| version.version)
                    .filter(|version| *version < current)
                    .max()
            });
            change(
                id,
                Some(previous.ok_or(ManagementError::NoPreviousVersion)?),
            )
        }
    }
}

impl QqCommandHandler for Commands {
    fn handle(&self, input: QqCommandInput<'_>) -> PluginResult<Option<String>> {
        let Some(command) = parse(input.text) else {
            return Ok(None);
        };
        let Some(tools) = &self.tools else {
            return Ok(Some(DISABLED.into()));
        };
        let reply = match execute_command(tools.as_ref(), &input.session.user_id, command) {
            Ok(value) => value["message"].as_str().unwrap_or_default().to_owned(),
            Err(ManagementError::Unavailable) => return Err(failure_text()),
            Err(error) => error.message().into(),
        };
        Ok(Some(reply))
    }
}

/// 只在锻造工具账本已装配时注册；宿主沿用 /tool 的管理权限。
pub(crate) struct ManageTool {
    pub(crate) tools: Arc<dyn ToolAdmin>,
}
impl ManageTool {
    fn command<'a>(&self, value: &'a Value) -> Result<Command<'a>, ToolValidationError> {
        let invalid = || ToolValidationError {
            message: concat!(
                "action 须为 list、show、disable、enable 或 rollback；",
                "除 list 外须 tool_id，只有 enable 须正整数 version；不接受身份或额外字段",
            )
            .into(),
        };
        let object = value.as_object().ok_or_else(invalid)?;
        let action = object
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        if action == "list" && object.len() == 1 {
            return Ok(Command::List);
        }
        let id = object
            .get("tool_id")
            .and_then(Value::as_str)
            .filter(|id| eve_toolforge_api::validate_id(id).is_ok())
            .ok_or_else(invalid)?;
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "action" | "tool_id" | "version"))
        {
            return Err(invalid());
        }
        match (action, object.get("version"), object.len()) {
            ("show", None, 2) => Ok(Command::Show(id)),
            ("disable", None, 2) => Ok(Command::Disable(id)),
            ("rollback", None, 2) => Ok(Command::Rollback(id)),
            ("enable", Some(version), 3) => {
                let version = version
                    .as_u64()
                    .and_then(|version| u32::try_from(version).ok())
                    .filter(|version| *version > 0)
                    .ok_or_else(invalid)?;
                Ok(Command::Enable(id, version))
            }
            _ => Err(invalid()),
        }
    }
}
impl Tool for ManageTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "manage_forged_tool".into(),
            description: "用户直接聊天要求管理自己的锻造检查工具时使用。先 list 或 show 查看实际 ID、版本、回放验证和启用记录，再按用户意图 disable、enable 指定的已验证 version 或 rollback 到当前启用版本之前的最近一版。只管理当前可信用户自己的工具，不删除版本；检查规则不等于任意代码插件。不存在或没有更早版本时如实说明。与兼容命令共用真实账本和管理权限，用户无需输入命令。".into(),
            argument_schema: json!({"type":"object","properties":{
                "action":{"type":"string","enum":["list","show","disable","enable","rollback"]},
                "tool_id":{"type":"string"},
                "version":{"type":"integer","minimum":1,"maximum":u32::MAX}
            },"required":["action"],"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial { scope: "eve.tools.management".into() }),
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        self.command(value).map(|_| ())
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            let command = self
                .command(&call.arguments)
                .map_err(|error| ToolExecutionError::Failed(error.message))?;
            if context.is_cancel_requested() {
                return Err(ToolExecutionError::Cancelled("本轮已取消".into()));
            }
            let scope = context.scope().ok_or_else(|| {
                ToolExecutionError::Failed("工具管理需要宿主绑定的用户身份".into())
            })?;
            eve_session_api::SessionKey::new(&scope.session_id, &scope.user_id)
                .map_err(|_| ToolExecutionError::Failed("用户会话身份无效".into()))?;
            if scope.user_id == crate::qq_group::USER {
                return Err(ToolExecutionError::Failed(
                    "群体观察不能代替真实用户管理工具".into(),
                ));
            }
            execute_command(self.tools.as_ref(), &scope.user_id, command)
                .map_err(|error| ToolExecutionError::Failed(error.message().into()))
        })
    }
}

fn title(tool: &ForgedTool) -> &str {
    tool.current()
        .or_else(|| tool.latest())
        .map_or(tool.id.as_str(), |version| version.spec.name.as_str())
}

fn status(tool: &ForgedTool) -> String {
    match tool.enabled {
        Some(version) => format!("启用第 {version} 版"),
        None => "已停用".into(),
    }
}

fn replay(verification: &Verification) -> String {
    let failing = verification
        .examples
        .iter()
        .filter(|example| example.expect_flag)
        .count();
    let caught = verification
        .examples
        .iter()
        .filter(|example| example.expect_flag && example.passed())
        .count();
    let passing = verification.examples.len() - failing;
    let clean = verification
        .examples
        .iter()
        .filter(|example| !example.expect_flag && example.passed())
        .count();
    format!(
        "回放 {} 份真实草稿：出现问题的 {failing} 份拦下 {caught} 份，验证通过的 {passing} 份没有误报 {clean} 份",
        verification.examples.len()
    )
}

fn list(snapshot: &ToolSnapshot, owner: &str) -> String {
    let tools: Vec<_> = snapshot.tools_for(owner).collect();
    let mut forges: Vec<_> = snapshot.forges_for(owner).collect();
    if tools.is_empty() && forges.is_empty() {
        return EMPTY.into();
    }
    let mut reply = format!(
        "Eve 锻造的工具（共 {} 个；每个版本都用实践中的真实草稿回放验证）：",
        tools.len()
    );
    for tool in &tools {
        let calls: Vec<_> = snapshot.calls_for(&tool.id).collect();
        let stopped = calls
            .iter()
            .filter(|call| !call.findings.is_empty())
            .count();
        let summary = tool
            .current()
            .or_else(|| tool.latest())
            .map_or("", |version| version.spec.summary.as_str());
        let _ = write!(
            reply,
            "\n- {}：{}｜{}｜{} 个版本｜检查 {} 次，拦下 {} 次\n  ID：{}",
            title(tool),
            summary,
            status(tool),
            tool.versions.len(),
            calls.len(),
            stopped,
            tool.id
        );
    }
    if snapshot.calls.len() >= MAX_CALLS {
        let _ = write!(reply, "\n{CALLS_FULL}");
    }
    forges.sort_by_key(|forge| std::cmp::Reverse(forge.started_at_ms));
    if !forges.is_empty() {
        reply.push_str("\n最近的锻造：");
    }
    for forge in forges.into_iter().take(SHOWN_FORGES) {
        let outcome = match (&forge.status, &forge.output) {
            (ForgeStatus::Running, _) => "进行中".to_string(),
            (ForgeStatus::Verified, _) => format!(
                "回放验证通过，成为第 {} 版",
                forge.tool.as_ref().map_or(0, |tool| tool.version)
            ),
            (ForgeStatus::Rejected, _) => "回放验证未通过，没有启用".into(),
            (ForgeStatus::Reused, _) => "已有工具即可覆盖，复用，没有新造".into(),
            (ForgeStatus::NotForgeable, Some(ForgeOutput::NotForgeable { reason })) => {
                format!("无法用只读规则识别：{reason}")
            }
            (ForgeStatus::NotForgeable, _) => "无法用只读规则识别".into(),
            (ForgeStatus::Failed(_), _) => "锻造请求失败，没有重试".into(),
            (ForgeStatus::Interrupted, _) => "因进程退出中断，不重放".into(),
        };
        let _ = write!(
            reply,
            "\n- 问题“{}”（出现 {} 次）：{outcome}",
            forge.gap.summary, forge.occurrences
        );
    }
    reply
}

fn rule_text(rule: &CheckRule) -> String {
    match rule {
        CheckRule::RequireFile { pattern } => format!("要有匹配 {pattern} 的文件"),
        CheckRule::RequireText { pattern, text } => format!("匹配 {pattern} 的文件要包含“{text}”"),
        CheckRule::ForbidText { pattern, text } => {
            format!("匹配 {pattern} 的文件不能包含“{text}”")
        }
    }
}

fn show(snapshot: &ToolSnapshot, tool: &ForgedTool) -> String {
    let mut reply = format!("工具 {}｜{}", title(tool), status(tool));
    if let Some(forge) = tool.latest().and_then(|latest| {
        snapshot
            .forges
            .iter()
            .find(|forge| forge.id == latest.forge_id)
    }) {
        let _ = write!(reply, "\n针对的问题：{}", forge.gap.summary);
    }
    for version in tool.versions.iter().rev() {
        let rules: Vec<String> = version.spec.rules.iter().map(rule_text).collect();
        let _ = write!(
            reply,
            "\n- 第 {} 版：{}\n  规则：{}\n  拦下时提示：{}\n  {}",
            version.version,
            version.spec.summary,
            rules.join("；"),
            version.spec.message,
            replay(&version.verification)
        );
    }
    let changes: Vec<String> = tool
        .changes
        .iter()
        .rev()
        .take(SHOWN_CHANGES)
        .map(|change| {
            format!(
                "{}{}",
                match change.actor {
                    Actor::Automatic => "验证通过后自动",
                    Actor::Owner => "用户",
                },
                match change.enabled {
                    Some(version) => format!("启用第 {version} 版"),
                    None => "停用".into(),
                }
            )
        })
        .collect();
    if !changes.is_empty() {
        let _ = write!(reply, "\n启用记录（最近在前）：{}", changes.join("；"));
    }
    let mut calls: Vec<_> = snapshot.calls_for(&tool.id).collect();
    calls.sort_by_key(|call| std::cmp::Reverse(call.at_ms));
    for call in calls.into_iter().take(SHOWN_CALLS) {
        let _ = write!(
            reply,
            "\n调用：第 {} 版，实践 {} 第 {} 次尝试｜{}",
            call.tool.version,
            call.run_id,
            call.attempt,
            if call.findings.is_empty() {
                "通过，交给运行环境实际运行".to_string()
            } else {
                format!("拦下，没有运行：{}", call.findings.join("；"))
            }
        );
    }
    reply
}
