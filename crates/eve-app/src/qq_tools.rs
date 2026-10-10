//! QQ 宿主的工具查看与管理：/tools 列出当前用户的锻造工具与最近的锻造，/tool 查看版本、规则、
//! 回放验证、启用记录与调用，并可停用、启用某个已验证版本或回退到上一版本。
//! 工具只属于来源用户；变更只改变启用版本并留下记录，不删除任何版本。
use crate::AppError;
use eve_plugin_api::{PluginError, PluginResult};
use eve_qqbot_plugin::{QqCommandHandler, QqCommandInput};
use eve_toolforge_api::{
    Actor, CheckRule, ForgeOutput, ForgeStatus, ForgedTool, MAX_CALLS, ToolAdmin, ToolError,
    ToolSnapshot, Verification,
};
use std::{
    fmt::Write,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

const DISABLED: &str = "工具锻造未启用。";
const EMPTY: &str = "还没有锻造的工具。同一运行环境中反复出现的问题，Eve 会尝试锻造成运行前检查，用实践中的真实草稿回放验证通过才启用。";
const HELP: &str = "用法：/tools 列出工具；/tool 工具ID 查看详情；/tool disable 工具ID 停用；/tool enable 工具ID 版本 启用某个已验证版本；/tool rollback 工具ID 回退到上一版本。";
const NOT_FOUND: &str = "没有这个工具。发送 /tools 查看工具 ID。";
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

impl QqCommandHandler for Commands {
    fn handle(&self, input: QqCommandInput<'_>) -> PluginResult<Option<String>> {
        let Some(command) = parse(input.text) else {
            return Ok(None);
        };
        let Some(tools) = &self.tools else {
            return Ok(Some(DISABLED.into()));
        };
        let owner = input.session.user_id.as_str();
        let snapshot = tools.snapshot().map_err(|_| failure_text())?;
        let owned = |id: &str| {
            snapshot
                .tool(id)
                .filter(|tool| tool.owner == owner)
                .cloned()
        };
        let change = |id: &str, enabled: Option<u32>| -> PluginResult<String> {
            let at = now_ms().map_err(|_| failure_text())?;
            match tools.set_enabled(id, at, Actor::Owner, enabled) {
                Ok(tool) => {
                    let name = title(&tool);
                    Ok(match tool.enabled {
                        Some(version) => format!("已启用「{name}」第 {version} 版。"),
                        None => format!(
                            "已停用「{name}」；之后的实践不会再用它检查，新验证的版本也不会自动启用。"
                        ),
                    })
                }
                Err(ToolError::InvalidInput) => Ok("没有这个已验证版本。".into()),
                Err(ToolError::LimitReached) => Ok("启用记录已满；保留原记录，未做更改。".into()),
                Err(_) => Err(failure_text()),
            }
        };
        let reply = match command {
            Command::Help => HELP.into(),
            Command::List => list(&snapshot, owner),
            Command::Show(id) => match owned(id) {
                Some(tool) => show(&snapshot, &tool),
                None => NOT_FOUND.into(),
            },
            Command::Disable(id) => match owned(id) {
                Some(_) => change(id, None)?,
                None => NOT_FOUND.into(),
            },
            Command::Enable(id, version) => match owned(id) {
                Some(_) => change(id, Some(version))?,
                None => NOT_FOUND.into(),
            },
            Command::Rollback(id) => match owned(id) {
                None => NOT_FOUND.into(),
                Some(tool) => {
                    let previous = tool.enabled.and_then(|current| {
                        tool.versions
                            .iter()
                            .rev()
                            .map(|version| version.version)
                            .find(|version| *version < current)
                    });
                    match previous {
                        Some(version) => change(id, Some(version))?,
                        None => "没有可以回退的更早版本。".into(),
                    }
                }
            },
        };
        Ok(Some(reply))
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
