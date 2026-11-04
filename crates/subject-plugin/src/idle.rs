//! 无通道、无模型依赖的回顾来源校验与认知修订；宿主负责绑定经历和持久提交。
use eve_subject_api::*;

pub fn validate_idle_reflection(
    observation: &IdleObservation,
    result: &IdleReflection,
) -> SubjectResult<()> {
    observation.validate()?;
    result.validate()?;
    crate::self_interest::validate_interest_updates(observation, result)?;
    let references = |ids: &[String]| {
        ids.iter()
            .all(|id| observation.evidence.iter().any(|e| &e.id == id))
    };
    for judgment in &result.judgments {
        if !references(&judgment.evidence) {
            return Err(SubjectError::InvalidInput);
        }
        if let Some(old) = observation
            .judgments
            .iter()
            .find(|old| old.key == judgment.key)
            && (old.statement != judgment.statement || old.confidence != judgment.confidence)
            && judgment.evidence.iter().all(|id| old.evidence.contains(id))
        {
            return Err(SubjectError::InvalidInput);
        }
    }
    if result
        .action
        .as_ref()
        .is_some_and(|a| !references(&a.assessment.evidence))
    {
        return Err(SubjectError::InvalidInput);
    }
    Ok(())
}

/// 当前认知为有界视图，旧完整判断由宿主原任务回执保存；调用者须先核对来源与修订。
pub fn revise_idle_mind(
    mut previous: IdleMind,
    result: &IdleReflection,
    task_id: &str,
    now: u64,
) -> SubjectResult<IdleMind> {
    result.validate()?;
    validate_judgments(&previous.judgments)?;
    validate_id(task_id)?;
    if now == 0 {
        return Err(SubjectError::InvalidInput);
    }
    previous.self_interests = crate::self_interest::revise_self_interests(
        previous.self_interests,
        &result.interest_updates,
        now,
    )?;
    for judgment in &result.judgments {
        previous.judgments.retain(|j| j.key != judgment.key);
        previous.judgments.push(judgment.clone());
    }
    if previous.judgments.len() > 6 {
        previous.judgments.drain(..previous.judgments.len() - 6);
    }
    previous.revision = previous
        .revision
        .checked_add(1)
        .ok_or(SubjectError::LimitReached)?;
    previous.last_task = task_id.to_owned();
    previous.next_at_ms = now
        .checked_add(u64::from(result.revisit_after_minutes) * 60_000)
        .ok_or(SubjectError::InvalidInput)?;
    previous.thought = result.thought.clone();
    previous.reason = result.reason.clone();
    loop {
        let encoded = serde_json::to_string(&previous).map_err(|_| SubjectError::InvalidInput)?;
        if encoded.len() <= MAX_PAYLOAD_BYTES {
            break;
        }
        if previous.judgments.is_empty() {
            return Err(SubjectError::LimitReached);
        }
        previous.judgments.remove(0);
    }
    Ok(previous)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn judgment(id: &str, statement: &str) -> IdleJudgment {
        IdleJudgment {
            key: "route".into(),
            statement: statement.into(),
            confidence: 70,
            evidence: vec![id.into()],
            revision_reason: "新实际结果改变预期".into(),
        }
    }
    fn reflection() -> IdleReflection {
        IdleReflection {
            choice: IdleChoice::Reflect,
            thought: "重新比较假设".into(),
            reason: "有新的实验结果".into(),
            judgments: vec![judgment("new", "原方案较慢")],
            interest_updates: vec![],
            action: None,
            revisit_after_minutes: 30,
        }
    }
    #[test]
    fn changing_judgments_needs_frozen_new_evidence_and_reducer_keeps_deadline_bounded() {
        let mut observation = IdleObservation {
            scope: SubjectScope {
                channel: "test".into(),
                conversation_id: "one".into(),
                principal_id: "owner".into(),
            },
            revision: 1,
            interests: vec!["Mindustry".into()],
            self_interests: vec![],
            interest_consumed: Default::default(),
            evidence: vec![IdleEvidence {
                id: "old".into(),
                kind: "task_result".into(),
                text: "旧结果".into(),
                excerpt: false,
            }],
            judgments: vec![judgment("old", "原方案可能更快")],
        };
        assert!(
            validate_idle_reflection(&observation, &reflection()).is_err(),
            "不能引用未给出的新证据"
        );
        let mut without_new = reflection();
        without_new.judgments[0].evidence = vec!["old".into()];
        assert!(
            validate_idle_reflection(&observation, &without_new).is_err(),
            "不能仅改口就当成有证据的修订"
        );
        observation.evidence.push(IdleEvidence {
            id: "new".into(),
            kind: "task_result".into(),
            text: "真实新结果".into(),
            excerpt: false,
        });
        validate_idle_reflection(&observation, &reflection()).unwrap();
        let previous = IdleMind {
            revision: 1,
            last_task: "prior".into(),
            next_at_ms: 1,
            thought: "旧想法".into(),
            reason: "旧理由".into(),
            judgments: observation.judgments.clone(),
            self_interests: vec![],
            archived: false,
        };
        let next = revise_idle_mind(previous, &reflection(), "review-2", 100).unwrap();
        assert_eq!(next.revision, 2);
        assert_eq!(next.next_at_ms, 100 + AUTONOMY_INTERVAL_MS);
        assert_eq!(next.judgments.len(), 1);
        assert_eq!(next.judgments[0].statement, "原方案较慢");
        let mut bad = reflection();
        bad.choice = IdleChoice::Act;
        assert!(
            validate_idle_reflection(&observation, &bad).is_err(),
            "行动必须有独立提议与验收"
        );
        assert!(
            revise_idle_mind(next, &reflection(), "review-3", u64::MAX).is_err(),
            "时间溢出不保存"
        );
    }
}
