use eve_subject_api::*;
use eve_subject_plugin::{revise_idle_mind, validate_idle_reflection};
use serde_json::json;

fn interest(key: &str, topic: &str, evidence: &str) -> SelfInterest {
    SelfInterest {
        key: key.into(),
        topic: topic.into(),
        phase: SelfInterestPhase::Curious,
        strength: 55,
        reason: "更想弄清简单规则为何会形成复杂结构".into(),
        want_to_try: "用一个小实验比较不同规则产生的图案".into(),
        evidence: vec![evidence.into()],
    }
}

fn observation() -> IdleObservation {
    IdleObservation {
        scope: SubjectScope {
            channel: "qq".into(),
            conversation_id: "private-alice".into(),
            principal_id: "alice".into(),
        },
        revision: 0,
        interests: vec!["configured direction".into()],
        self_interests: vec![],
        interest_consumed: Default::default(),
        evidence: vec![
            IdleEvidence {
                id: "conversation-1".into(),
                kind: "completed_conversation".into(),
                text: "实际交流讨论了规则与图案".into(),
                excerpt: false,
            },
            IdleEvidence {
                id: "experiment-1".into(),
                kind: "task_result".into(),
                text: "宿主保存的实际实验回执".into(),
                excerpt: true,
            },
        ],
        judgments: vec![],
    }
}

fn reflection(updates: Vec<SelfInterest>) -> IdleReflection {
    IdleReflection {
        choice: IdleChoice::Reflect,
        thought: "想继续了解规则和图案".into(),
        reason: "从实际交流中形成的好奇".into(),
        judgments: vec![],
        interest_updates: updates,
        action: None,
        revisit_after_minutes: 30,
    }
}

fn formed() -> IdleMind {
    let result = reflection(vec![interest("patterns", "规则图案", "conversation-1")]);
    validate_idle_reflection(&observation(), &result).unwrap();
    revise_idle_mind(IdleMind::default(), &result, "review-1", 100).unwrap()
}

#[test]
fn own_interest_retains_identity_and_changes_only_from_new_experience() {
    let first = formed();
    let saved = serde_json::to_string(&first).unwrap();
    let restored: IdleMind = serde_json::from_str(&saved).unwrap();
    let mut observed = observation();
    observed.revision = restored.revision;
    observed.self_interests = restored.self_interests.clone();
    let mut update = observed.self_interests[0].current.clone();
    update.phase = SelfInterestPhase::Active;
    update.strength = 75;
    update.reason = "实际图案比预期更有变化，想继续比较".into();
    assert!(validate_idle_reflection(&observed, &reflection(vec![update.clone()])).is_err());
    update.evidence = vec!["experiment-1".into()];
    let result = reflection(vec![update]);
    validate_idle_reflection(&observed, &result).unwrap();
    let revised = revise_idle_mind(restored, &result, "review-2", 200).unwrap();
    let record = &revised.self_interests[0];
    assert_eq!(record.revision, 2);
    assert_eq!(record.first_recorded_at_ms, 100);
    assert_eq!(record.updated_at_ms, 200);
    assert_eq!(record.current.phase, SelfInterestPhase::Active);
    observed.self_interests = revised.self_interests.clone();
    let mut cycle = record.current.clone();
    cycle.strength = 90;
    cycle.evidence = vec!["conversation-1".into()];
    assert!(validate_idle_reflection(&observed, &reflection(vec![cycle])).is_err());
    let again = revise_idle_mind(revised, &reflection(vec![]), "review-3", 300).unwrap();
    assert_eq!(again.self_interests, observed.self_interests);
    assert_eq!(again.next_at_ms, 300 + AUTONOMY_INTERVAL_MS);
}

#[test]
fn own_text_focus_and_foreign_or_fabricated_evidence_cannot_create_an_interest() {
    for kind in ["self_reflection", "focus"] {
        let mut observed = observation();
        observed.evidence[0].kind = kind.into();
        let result = reflection(vec![interest("patterns", "规则图案", "conversation-1")]);
        assert!(
            validate_idle_reflection(&observed, &result).is_err(),
            "{kind}"
        );
    }
    for evidence in ["foreign-user-turn", "invented"] {
        let result = reflection(vec![interest("patterns", "规则图案", evidence)]);
        assert!(validate_idle_reflection(&observation(), &result).is_err());
    }
}

#[test]
fn dropped_interests_cannot_be_recreated_with_a_new_key_or_motivate_action() {
    let mut observed = observation();
    observed.self_interests = formed().self_interests;
    let mut dropped = observed.self_interests[0].current.clone();
    dropped.phase = SelfInterestPhase::Dropped;
    dropped.evidence = vec!["experiment-1".into()];
    dropped.want_to_try.clear();
    dropped.reason = "实验后发现这条方向吸引力不大，先放下".into();
    let mut result = reflection(vec![dropped]);
    validate_idle_reflection(&observed, &result).unwrap();
    result.choice = IdleChoice::Act;
    result.action = Some(serde_json::from_value(json!({
        "title":"测试兴趣行动","goal":"做一次小实验","interest_key":"patterns",
        "assessment":{"problem_key":"test","expected_gain":"学到新的规律","verification":"读实验结果",
            "reuse_plan":"保存实验","benefit":90,"confidence":90,"cost":10,"risk":0,"evidence":["experiment-1"]}
    })).unwrap());
    assert!(validate_idle_reflection(&observed, &result).is_err());
    result.interest_updates[0].phase = SelfInterestPhase::Active;
    result.interest_updates[0].want_to_try = "做一次小实验".into();
    validate_idle_reflection(&observed, &result).unwrap();
    result.action.as_mut().unwrap().interest_key = Some("invented".into());
    assert!(validate_idle_reflection(&observed, &result).is_err());
    let mut renamed = interest("different-key", " 规则 图案 ", "experiment-1");
    assert!(validate_idle_reflection(&observed, &reflection(vec![renamed.clone()])).is_err());
    renamed.key = "patterns".into();
    renamed.topic = "Unrelated topic".into();
    assert!(validate_idle_reflection(&observed, &reflection(vec![renamed])).is_err());
}

#[test]
fn legacy_records_default_to_no_interest_and_updates_do_not_reset_existing_interests() {
    let mind: IdleMind = serde_json::from_value(json!({
        "revision":1,"last_task":"old","next_at_ms":100,
        "thought":"旧回顾","reason":"旧理由","judgments":[]
    }))
    .unwrap();
    assert!(mind.self_interests.is_empty());
    let result: IdleReflection = serde_json::from_value(json!({
        "choice":"wait","thought":"等下一次真实经历","reason":"没有新材料",
        "judgments":[],"action":null,"revisit_after_minutes":30
    }))
    .unwrap();
    assert!(result.interest_updates.is_empty());
    let own = formed();
    let expected = own.self_interests.clone();
    let next = revise_idle_mind(own, &result, "review-2", 200).unwrap();
    assert_eq!(next.self_interests, expected);
    let mut forged = serde_json::to_value(&expected).unwrap();
    forged[0]["revision"] = json!(0);
    let forged: Vec<SelfInterestRecord> = serde_json::from_value(forged).unwrap();
    assert!(validate_self_interests(&forged).is_err());
}

#[test]
fn capacity_and_duplicate_topics_reject_updates_without_eviction() {
    let mut prior = IdleMind::default();
    for n in 0..MAX_SELF_INTERESTS {
        let mut update = interest(
            &format!("topic-{n}"),
            &format!("Topic {n}"),
            "conversation-1",
        );
        update.reason = "好奇".into();
        update.want_to_try = "实验".into();
        prior = revise_idle_mind(
            prior,
            &reflection(vec![update]),
            &format!("review-{n}"),
            100 + n as u64,
        )
        .unwrap();
    }
    let before = serde_json::to_string(&prior).unwrap();
    let overflow = reflection(vec![interest("overflow", "Overflow", "experiment-1")]);
    assert!(
        revise_idle_mind(
            serde_json::from_str(&before).unwrap(),
            &overflow,
            "overflow-review",
            200
        )
        .is_err()
    );
    let original: IdleMind = serde_json::from_str(&before).unwrap();
    assert_eq!(original.self_interests.len(), MAX_SELF_INTERESTS);
    let duplicate = reflection(vec![
        interest("a", "Topic", "conversation-1"),
        interest("b", " topic ", "experiment-1"),
    ]);
    assert!(duplicate.validate().is_err());
    let mut future = original;
    future.self_interests[0].updated_at_ms = 500;
    let update = interest("topic-0", "Topic 0", "experiment-1");
    assert!(revise_idle_mind(future, &reflection(vec![update]), "backwards", 200).is_err());
}
