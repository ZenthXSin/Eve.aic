//! QQ 宿主的技能查看与管理：/skills 列出当前用户的技能，/skill 查看版本、验证证据、
//! 启用记录与调用结果，并可停用、启用某个已验证版本或回退到上一版本。
//! 技能只属于来源用户；变更只改变启用版本并留下记录，不删除任何版本。
use crate::AppError;
use eve_plugin_api::{PluginError, PluginResult};
use eve_qqbot_plugin::{QqCommandHandler, QqCommandInput};
use eve_skill_api::{
    Actor, DistillStatus, InvocationOutcome, ParameterKind, SkillAdmin, SkillError, SkillSnapshot,
};
use std::{
    fmt::Write,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

const DISABLED: &str = "技能固化未启用。";
const EMPTY: &str =
    "还没有固化的技能。Eve 只把实际验证通过、且用新参数再次实际运行通过的方法固化为技能。";
const HELP: &str = "用法：/skills 列出技能；/skill 技能ID 查看详情；/skill disable 技能ID 停用；/skill enable 技能ID 版本 启用某个已验证版本；/skill rollback 技能ID 回退到上一版本。";
const NOT_FOUND: &str = "没有这个技能。发送 /skills 查看技能 ID。";
const SHOWN_INVOCATIONS: usize = 3;
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
        "/skills" => Some(match words.next() {
            None => Command::List,
            Some(_) => Command::Help,
        }),
        "/skill" => {
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
    skills: Option<Arc<dyn SkillAdmin>>,
}
impl Commands {
    pub(crate) fn disabled() -> Arc<dyn QqCommandHandler> {
        Arc::new(Self { skills: None })
    }
    pub(crate) fn enabled(skills: Arc<dyn SkillAdmin>) -> Arc<dyn QqCommandHandler> {
        Arc::new(Self {
            skills: Some(skills),
        })
    }
}

fn failure_text() -> PluginError {
    PluginError::State("技能记录暂时不可用".into())
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
        let Some(skills) = &self.skills else {
            return Ok(Some(DISABLED.into()));
        };
        let owner = input.session.user_id.as_str();
        let snapshot = skills.snapshot().map_err(|_| failure_text())?;
        let owned = |id: &str| {
            snapshot
                .skill(id)
                .filter(|skill| skill.owner == owner)
                .cloned()
        };
        let change = |id: &str, enabled: Option<u32>| -> PluginResult<String> {
            let at = now_ms().map_err(|_| failure_text())?;
            match skills.set_enabled(id, at, Actor::Owner, enabled) {
                Ok(skill) => Ok(match skill.enabled {
                    Some(version) => format!("已启用「{}」第 {version} 版。", skill.name),
                    None => format!(
                        "已停用「{}」；之后的任务不会再使用它，新验证的版本也不会自动启用。",
                        skill.name
                    ),
                }),
                Err(SkillError::InvalidInput) => Ok("没有这个已验证版本。".into()),
                Err(SkillError::LimitReached) => Ok("启用记录已满；保留原记录，未做更改。".into()),
                Err(_) => Err(failure_text()),
            }
        };
        let reply = match command {
            Command::Help => HELP.into(),
            Command::List => list(&snapshot, owner),
            Command::Show(id) => match owned(id) {
                Some(_) => show(&snapshot, id),
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
                Some(skill) => {
                    let previous = skill.enabled.and_then(|current| {
                        skill
                            .versions
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

fn list(snapshot: &SkillSnapshot, owner: &str) -> String {
    let skills: Vec<_> = snapshot.skills_for(owner).collect();
    if skills.is_empty() {
        return EMPTY.into();
    }
    let mut reply = format!(
        "Eve 的技能（共 {} 个；每个版本都用新参数在实际运行环境中验证通过）：",
        skills.len()
    );
    for skill in skills {
        let title = skill
            .latest()
            .and_then(|latest| snapshot.distillation(&latest.distillation_id))
            .and_then(|entry| entry.template())
            .map_or(skill.name.as_str(), |proposal| {
                proposal.template.title.as_str()
            });
        let invocations: Vec<_> = snapshot.invocations(&skill.id).collect();
        let succeeded = invocations
            .iter()
            .filter(|selection| selection.outcome == Some(InvocationOutcome::Verified))
            .count();
        let _ = write!(
            reply,
            "\n- {}「{}」｜{}｜{} 个版本｜调用 {} 次，验证通过 {} 次\n  ID：{}",
            skill.name,
            title,
            match skill.enabled {
                Some(version) => format!("启用第 {version} 版"),
                None => "已停用".into(),
            },
            skill.versions.len(),
            invocations.len(),
            succeeded,
            skill.id
        );
    }
    reply
}

fn show(snapshot: &SkillSnapshot, id: &str) -> String {
    let Some(skill) = snapshot.skill(id) else {
        return NOT_FOUND.into();
    };
    let mut reply = format!(
        "技能 {}｜{}",
        skill.name,
        match skill.enabled {
            Some(version) => format!("启用第 {version} 版"),
            None => "已停用".into(),
        }
    );
    for version in skill.versions.iter().rev() {
        let Some(entry) = snapshot.distillation(&version.distillation_id) else {
            continue;
        };
        let Some(proposal) = entry.template() else {
            continue;
        };
        let template = &proposal.template;
        let _ = write!(
            reply,
            "\n- 第 {} 版「{}」：{}\n  来源：实践 {} 的第 {} 次尝试｜运行环境：{}",
            version.version,
            template.title,
            template.summary,
            entry.origin.practice_run_id,
            entry.origin.attempt,
            entry.runner.runtime
        );
        let parameters: Vec<String> = template
            .parameters
            .iter()
            .map(|parameter| match &parameter.kind {
                ParameterKind::Identifier => format!("{}（标识）", parameter.name),
                ParameterKind::Integer { min, max } => {
                    format!("{}（整数 {min}～{max}）", parameter.name)
                }
                ParameterKind::Choice { options } => {
                    format!("{}（{}）", parameter.name, options.join("/"))
                }
            })
            .collect();
        let _ = write!(reply, "\n  参数：{}", parameters.join("、"));
        if let Some(holdout) = &entry.holdout {
            let values: Vec<String> = holdout
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect();
            let _ = write!(reply, "\n  验证参数：{}", values.join("，"));
        }
        if let Some(evidence) = &entry.evidence {
            let _ = write!(
                reply,
                "｜{}｜{}",
                if entry.status == DistillStatus::Verified {
                    "已验证"
                } else {
                    "未通过"
                },
                evidence.runtime_version
            );
            for result in &evidence.probes {
                let _ = write!(
                    reply,
                    "\n    探测 {}.{}：期望 {}，实际 {}{}",
                    result.probe.subject,
                    result.probe.property,
                    result.probe.expected,
                    result.actual.as_deref().unwrap_or("无"),
                    if result.passed { " ✓" } else { " ✗" }
                );
            }
        }
    }
    let changes: Vec<String> = skill
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
    let mut invocations: Vec<_> = snapshot.invocations(id).collect();
    invocations.sort_by_key(|selection| std::cmp::Reverse(selection.started_at_ms));
    for selection in invocations.into_iter().take(SHOWN_INVOCATIONS) {
        let Some(choice) = &selection.choice else {
            continue;
        };
        let values: Vec<String> = choice
            .arguments
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        let _ = write!(
            reply,
            "\n调用：第 {} 版，实践 {}，参数 {}｜{}",
            choice.skill.version,
            selection.id,
            values.join("，"),
            match selection.outcome {
                None => "等待实际运行结果",
                Some(InvocationOutcome::Verified) => "实际运行验证通过",
                Some(InvocationOutcome::Failed) => "实际运行未通过",
                Some(InvocationOutcome::Rejected) => "结构检查未通过，没有运行",
                Some(InvocationOutcome::Abandoned) => "因停止或超时放弃，结果未知",
                Some(InvocationOutcome::Interrupted) => "因进程退出中断，不重放",
            }
        );
    }
    reply
}
