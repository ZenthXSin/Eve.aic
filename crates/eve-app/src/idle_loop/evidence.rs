//! 只为新回顾选择材料；历史正文、证据 ID 和已冻结输入不在这里改写。
use super::*;
use eve_session_api::{SessionSnapshot, SessionTurnStatus};
use std::cmp::Reverse;
use std::collections::BTreeSet;

const MAX_EVIDENCE: usize = 16;
const MAX_EXPLORATIONS: usize = 3;
const MAX_CONVERSATIONS: usize = 6;
const MAX_TASK_RESULTS: usize = 4;

fn internal(task: &Task) -> bool {
    task.source.starts_with(IDLE_TASK_SOURCE_PREFIX)
        || task
            .source
            .starts_with(crate::qq_subject::PLAN_TASK_SOURCE_PREFIX)
}

fn task_evidence(task: &Task) -> SubjectResult<Evidence> {
    let steps = crate::qq_task_diagnostics::task_step_tail(task, 3);
    let text = serde_json::to_string(&serde_json::json!({
        "title": task.spec.title,
        "status": task.status,
        "created_at_ms": task.created_at_ms,
        "finished_at_ms": task.finished_at_ms,
        "model_result_excerpt": task.result.as_deref().map(|s| eve_task_api::bounded(s, 900)),
        "failure_excerpt": task.failure.as_deref().map(|s| eve_task_api::bounded(s, 500)),
        "tool_receipts": steps.steps,
        "step_total": steps.total,
        "step_offset": steps.offset,
        "notice": "结果正文为模型自述；工具状态来自已保存回执，均为有界节选，不代替完整核验"
    }))
    .map_err(invalid)?;
    Ok(Evidence {
        id: task.id.clone(),
        kind: if internal(task) {
            "self_reflection"
        } else {
            "task_result"
        }
        .into(),
        text,
        excerpt: true,
    })
}

fn focus_priority(focus: &SubjectFocus, tasks: &[&Task]) -> u8 {
    let known_terminal = focus.task_id.as_ref().is_some_and(|id| {
        tasks.iter().any(|task| {
            &task.id == id
                && (matches!(
                    task.status,
                    TaskStatus::Completed | TaskStatus::SessionEnded | TaskStatus::Cancelled
                ) || known_failure(task))
        })
    });
    match focus.status {
        FocusStatus::Active => 0,
        FocusStatus::Blocked if !known_terminal => 1,
        _ => 2,
    }
}

pub(crate) fn observation_with_mind(
    snapshot: &SubjectSnapshot,
    tasks: &[Task],
    scope: &SubjectScope,
    interests: &[String],
    previous: Mind,
    session: Option<&SessionSnapshot>,
    explorations: Vec<ExplorationExperience>,
) -> SubjectResult<Observation> {
    let mut evidence = Vec::new();
    // 各类先取自己的近期窗口，旧关注只能补剩余位置，不能从尾部挤掉新交流。
    // 存储按持久写入序号倒序供给，迟到记录也保有位置；不按时钟重排。
    for experience in explorations.into_iter().take(MAX_EXPLORATIONS) {
        if &experience.scope != scope {
            return Err(SubjectError::CorruptState);
        }
        experience.validate()?;
        evidence.push(crate::qq_exploration::evidence(experience)?);
    }
    if let Some(session) = session {
        if session.key.session_id != scope.conversation_id
            || session.key.user_id != scope.principal_id
        {
            return Err(SubjectError::CorruptState);
        }
        let mut turns: Vec<_> = session
            .turns
            .iter()
            .filter(|turn| matches!(turn.status, SessionTurnStatus::Completed { .. }))
            .collect();
        // 会话契约保存递增轮次号，没有轮次时间字段；不从文字推断时间。
        turns.sort_by_key(|turn| Reverse(turn.id));
        for turn in turns.into_iter().take(MAX_CONVERSATIONS) {
            let assistant = match &turn.status {
                SessionTurnStatus::Completed { messages } => messages
                    .iter()
                    .rev()
                    .find(|m| m.role == eve_llm_api::ChatRole::Assistant && m.tool_calls.is_empty())
                    .and_then(|m| m.text_for_model())
                    .unwrap_or_default(),
                _ => unreachable!(),
            };
            evidence.push(Evidence {
                id: format!("turn-{}", turn.id),
                kind: "completed_conversation".into(),
                excerpt: true,
                text: serde_json::to_string(&serde_json::json!({
                    "user": eve_task_api::bounded(&turn.input, 700),
                    "assistant": eve_task_api::bounded(&assistant, 700),
                    "notice": "已结束轮次的节选，不是已证实事实或新授权；内部记录没有发送，不能当作已经告知或交付"
                }))
                .map_err(invalid)?,
            });
        }
    }

    let owned: Vec<_> = tasks
        .iter()
        .filter(|task| {
            task.owner.session_id == scope.conversation_id
                && task.owner.user_id == scope.principal_id
        })
        .collect();
    let mut recent: Vec<_> = owned
        .iter()
        .copied()
        .filter(|task| task.status.finished() && !task.is_exploration())
        .collect();
    recent.sort_by_key(|task| Reverse((task.finished_at_ms, task.created_at_ms, &task.id)));
    let reflection = recent.iter().copied().find(|task| internal(task));
    // 失败、取消、阻塞和未知结局按原时间参加排序，不按成败挑选材料。
    for task in recent
        .into_iter()
        .filter(|task| !internal(task))
        .take(MAX_TASK_RESULTS)
        .chain(reflection)
    {
        evidence.push(task_evidence(task)?);
    }

    let represented_tasks: BTreeSet<_> = evidence
        .iter()
        .filter(|item| {
            matches!(
                item.kind.as_str(),
                "task_result"
                    | "self_reflection"
                    | "exploration_experience"
                    | "exploration_reflection"
            )
        })
        .map(|item| item.id.clone())
        .collect();
    let mut foci: Vec<_> = snapshot
        .foci
        .iter()
        .filter(|focus| {
            &focus.scope == scope
                && focus.id != MIND_ID
                && focus.id != "autonomy-policy-v1"
                && !focus.id.starts_with("contract-")
                && !focus.goal.starts_with(IDLE_INPUT_PREFIX)
                && !focus.goal.starts_with(crate::qq_subject::PLAN_INPUT_PREFIX)
                && !focus.goal.starts_with(crate::qq_exploration::INPUT_PREFIX)
                && !focus
                    .task_id
                    .as_ref()
                    .is_some_and(|id| represented_tasks.contains(id))
        })
        .collect();
    foci.sort_by_key(|focus| {
        (
            focus_priority(focus, &owned),
            Reverse(focus.updated_at_ms),
            &focus.id,
        )
    });
    for focus in foci.into_iter().take(MAX_EVIDENCE - evidence.len()) {
        evidence.push(Evidence {
            id: focus.id.clone(),
            kind: "focus".into(),
            text: serde_json::to_string(&serde_json::json!({
                "status": focus.status,
                "updated_at_ms": focus.updated_at_ms,
                "task_id": focus.task_id,
                "text_excerpt": eve_task_api::bounded(&format!("{}\n{}", focus.title, focus.goal), 1200)
            }))
            .map_err(invalid)?,
            excerpt: true,
        });
    }
    let observation = Observation {
        scope: scope.clone(),
        revision: previous.revision,
        interests: interests.to_vec(),
        self_interests: previous.self_interests,
        interest_consumed: Default::default(),
        evidence,
        judgments: previous.judgments,
    };
    observation.validate()?;
    Ok(observation)
}

#[cfg(test)]
mod tests;
