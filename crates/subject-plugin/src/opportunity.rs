//! 候选随原关注账本持久化，不另建无限增长的内存索引。
use eve_subject_api::*;
use serde::{Deserialize, Serialize};

const PREFIX: &str = "eve-opportunity-v1\n";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Opportunity {
    pub goal: String,
    pub assessment: OpportunityAssessment,
}

pub fn encode_opportunity(value: &Opportunity) -> SubjectResult<String> {
    value.assessment.validate()?;
    if value.goal.trim().is_empty() {
        return Err(SubjectError::InvalidInput);
    }
    let encoded = format!(
        "{PREFIX}{}",
        serde_json::to_string(value).map_err(|_| SubjectError::InvalidInput)?
    );
    if encoded.len() > MAX_PAYLOAD_BYTES {
        return Err(SubjectError::InvalidInput);
    }
    Ok(encoded)
}

pub fn decode_opportunity(goal: &str) -> SubjectResult<Option<Opportunity>> {
    let Some(body) = goal.strip_prefix(PREFIX) else {
        return Ok(None);
    };
    let value: Opportunity = serde_json::from_str(body).map_err(|_| SubjectError::InvalidInput)?;
    value.assessment.validate()?;
    if value.goal.trim().is_empty() {
        return Err(SubjectError::InvalidInput);
    }
    Ok(Some(value))
}

/// 精确范围内选择正收益候选，已进入任何一轮自主执行的问题不自动重跑。
/// 旧格式候选保持兼容，但排序低于经过评估的正收益候选。
pub fn select_opportunity<'a>(
    foci: &'a [SubjectFocus],
    scope: &SubjectScope,
) -> SubjectResult<Option<&'a SubjectFocus>> {
    let mut used = Vec::new();
    for focus in foci
        .iter()
        .filter(|f| &f.scope == scope && f.id.starts_with("auto-"))
    {
        used.push((
            focus.goal.as_str(),
            decode_opportunity(&focus.goal)?.map(|o| o.assessment.problem_key),
        ));
    }
    let mut selected: Option<(&SubjectFocus, i32)> = None;
    for focus in foci.iter().filter(|f| {
        &f.scope == scope && f.id.starts_with("candidate-") && f.status == FocusStatus::Blocked
    }) {
        let candidate = decode_opportunity(&focus.goal)?;
        if used.iter().any(|(goal, key)| {
            *goal == focus.goal
                || candidate
                    .as_ref()
                    .is_some_and(|c| key.as_ref() == Some(&c.assessment.problem_key))
        }) {
            continue;
        }
        let score = candidate.as_ref().map_or(0, |c| c.assessment.score());
        if candidate.is_some() && score <= 0 {
            continue;
        }
        if selected.is_none_or(|(old, old_score)| {
            score > old_score
                || (score == old_score
                    && (focus.updated_at_ms, &focus.id) < (old.updated_at_ms, &old.id))
        }) {
            selected = Some((focus, score));
        }
    }
    Ok(selected.map(|(focus, _)| focus))
}
