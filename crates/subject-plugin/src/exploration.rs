//! 核对回顾中的经历是已保存真实片段的有界投影，不重新解释或编造回执。
use eve_subject_api::*;

pub fn validate_exploration_evidence(
    saved: &ExplorationExperience,
    evidence: &IdleEvidence,
) -> SubjectResult<()> {
    saved.validate()?;
    if evidence.id != saved.session_id || evidence.text.len() > 16384 {
        return Err(SubjectError::InvalidInput);
    }
    let view: ExplorationExperience =
        serde_json::from_str(&evidence.text).map_err(|_| SubjectError::InvalidInput)?;
    view.validate()?;
    let kind = if view.has_external_evidence() {
        "exploration_experience"
    } else {
        "exploration_reflection"
    };
    if evidence.kind != kind
        || view.session_id != saved.session_id
        || view.scope != saved.scope
        || view.started_at_ms != saved.started_at_ms
        || view.ended_at_ms != saved.ended_at_ms
        || view.end_reason != saved.end_reason
        || view.total_tool_calls != saved.total_tool_calls
        || view.subjective_notes != saved.subjective_notes
        || (view.tools_truncated && !evidence.excerpt)
        || view
            .tools
            .iter()
            .flat_map(|tool| {
                std::iter::once(&tool.arguments)
                    .chain(tool.output.iter())
                    .chain(tool.error.iter())
            })
            .chain(view.subjective_notes.iter())
            .any(|excerpt| excerpt.truncated && !evidence.excerpt)
    {
        return Err(SubjectError::InvalidInput);
    }
    let offset = view
        .tool_offset
        .checked_sub(saved.tool_offset)
        .and_then(|offset| usize::try_from(offset).ok())
        .ok_or(SubjectError::InvalidInput)?;
    let end = offset
        .checked_add(view.tools.len())
        .ok_or(SubjectError::InvalidInput)?;
    if saved.tools.get(offset..end) != Some(view.tools.as_slice()) {
        return Err(SubjectError::InvalidInput);
    }
    Ok(())
}
