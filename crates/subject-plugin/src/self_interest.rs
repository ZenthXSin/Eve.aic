//! 兴趣修订只消费宿主冻结的经历；回顾自己的旧文字不能制造新经历。
use eve_subject_api::*;

pub(crate) fn validate_interest_updates(
    observation: &IdleObservation,
    result: &IdleReflection,
) -> SubjectResult<()> {
    for update in &result.interest_updates {
        let old = observation
            .self_interests
            .iter()
            .find(|old| old.current.key == update.key);
        let mut new_experience = false;
        for id in &update.evidence {
            let evidence = observation
                .evidence
                .iter()
                .find(|e| &e.id == id)
                .ok_or(SubjectError::InvalidInput)?;
            new_experience |= matches!(
                evidence.kind.as_str(),
                "task_result" | "completed_conversation"
            ) && old.is_none_or(|old| !old.consumed_evidence.contains(id))
                && observation
                    .interest_consumed
                    .get(&update.key)
                    .is_none_or(|used| !used.contains(id));
        }
        if old.is_none_or(|old| &old.current != update) && !new_experience {
            return Err(SubjectError::InvalidInput);
        }
        if old.is_some_and(|old| {
            self_interest_topic_key(&old.current.topic) != self_interest_topic_key(&update.topic)
        }) || observation.self_interests.iter().any(|old| {
            old.current.key != update.key
                && self_interest_topic_key(&old.current.topic)
                    == self_interest_topic_key(&update.topic)
        }) {
            return Err(SubjectError::InvalidInput);
        }
    }
    if let Some(key) = result.action.as_ref().and_then(|a| a.interest_key.as_ref()) {
        let interest = result
            .interest_updates
            .iter()
            .find(|interest| &interest.key == key)
            .or_else(|| {
                observation
                    .self_interests
                    .iter()
                    .map(|record| &record.current)
                    .find(|interest| &interest.key == key)
            })
            .ok_or(SubjectError::InvalidInput)?;
        if !interest.phase.motivates_action() {
            return Err(SubjectError::InvalidInput);
        }
    }
    Ok(())
}

pub(crate) fn revise_self_interests(
    mut previous: Vec<SelfInterestRecord>,
    updates: &[SelfInterest],
    now: u64,
) -> SubjectResult<Vec<SelfInterestRecord>> {
    validate_self_interests(&previous)?;
    for update in updates {
        update.validate()?;
        if let Some(old) = previous
            .iter_mut()
            .find(|old| old.current.key == update.key)
        {
            if old.current == *update {
                continue;
            }
            if now < old.updated_at_ms
                || self_interest_topic_key(&old.current.topic)
                    != self_interest_topic_key(&update.topic)
            {
                return Err(SubjectError::InvalidInput);
            }
            old.revision = old
                .revision
                .checked_add(1)
                .ok_or(SubjectError::LimitReached)?;
            old.updated_at_ms = now;
            old.current = update.clone();
            for id in &update.evidence {
                if !old.consumed_evidence.contains(id) {
                    old.consumed_evidence.push(id.clone());
                }
            }
        } else {
            previous.push(SelfInterestRecord {
                current: update.clone(),
                revision: 1,
                first_recorded_at_ms: now,
                updated_at_ms: now,
                consumed_evidence: update.evidence.clone(),
            });
        }
    }
    // 不用新兴趣静默覆盖旧兴趣或已放下的记录。
    validate_self_interests(&previous)?;
    Ok(previous)
}
