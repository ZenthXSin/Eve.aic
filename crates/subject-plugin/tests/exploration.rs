use eve_subject_api::*;
use eve_subject_plugin::{revise_idle_mind, validate_idle_reflection};
use serde_json::json;

fn observation() -> IdleObservation {
    IdleObservation {
        scope: SubjectScope {
            channel: "qq".into(),
            conversation_id: "private-one".into(),
            principal_id: "owner".into(),
        },
        revision: 0,
        interests: vec!["自由探索".into()],
        self_interests: vec![],
        interest_consumed: Default::default(),
        evidence: vec![],
        judgments: vec![],
    }
}

fn exploration() -> IdleReflection {
    serde_json::from_value(json!({
        "choice":"explore", "thought":"想随便看看", "reason":"有点好奇",
        "judgments":[], "action":null, "revisit_after_minutes":30
    }))
    .unwrap()
}

#[test]
fn exploration_needs_no_goal_contract_interest_or_new_external_evidence() {
    let observed = observation();
    let mut result = exploration();
    validate_idle_reflection(&observed, &result).unwrap();
    result.exploration = Some(IdleExploration::default());
    validate_idle_reflection(&observed, &result).unwrap();
    assert_eq!(
        serde_json::to_value(&result).unwrap()["exploration"],
        json!({})
    );
    result.exploration.as_mut().unwrap().cue = Some(String::new());
    validate_idle_reflection(&observed, &result).unwrap();
    result.exploration.as_mut().unwrap().interest_key = Some("invented".into());
    assert!(validate_idle_reflection(&observed, &result).is_err());
    result.exploration = None;
    result.choice = IdleChoice::Act;
    assert!(validate_idle_reflection(&observed, &result).is_err());
}

#[test]
fn exploration_wire_keeps_legacy_bytes_and_rejects_mixed_or_unknown_fields() {
    let old = r#"{"choice":"wait","thought":"先休息","reason":"想歇会儿","judgments":[],"action":null,"revisit_after_minutes":30}"#;
    let restored: IdleReflection = serde_json::from_str(old).unwrap();
    assert!(restored.exploration.is_none());
    assert_eq!(serde_json::to_string(&restored).unwrap(), old);
    for extra in [
        json!({"goal":"new goal"}),
        json!({"assessment":{}}),
        json!({"completion_contract":{}}),
        json!({"tool_permissions":["all"]}),
    ] {
        let mut wire = serde_json::to_value(exploration()).unwrap();
        wire["exploration"] = extra;
        assert!(serde_json::from_value::<IdleReflection>(wire).is_err());
    }
    for duplicate in [
        r#"{"cue":"first","cue":"second"}"#,
        r#"{"cue":null,"cue":"second"}"#,
        r#"{"interest_key":"first","interest_key":"second"}"#,
    ] {
        assert!(serde_json::from_str::<IdleExploration>(duplicate).is_err());
    }
    let mut result = exploration();
    result.exploration = Some(IdleExploration::default());
    result.choice = IdleChoice::Wait;
    assert!(result.validate().is_err());
    result.choice = IdleChoice::Reflect;
    assert!(result.validate().is_err());
    result.choice = IdleChoice::Explore;
    result.action = Some(
        serde_json::from_value(json!({
            "title":"a", "goal":"b", "assessment":{
                "problem_key":"p", "expected_gain":"g", "verification":"v",
                "reuse_plan":"r", "benefit":90, "confidence":90, "cost":1,
                "risk":0, "evidence":["e"]
            }
        }))
        .unwrap(),
    );
    assert!(result.validate().is_err());
}

fn formed() -> IdleMind {
    let mut observed = observation();
    observed.evidence.push(IdleEvidence {
        id: "experience-one".into(),
        kind: "exploration_experience".into(),
        text: "读到随机生成的规则，实际输出是不同颜色的图案".into(),
        excerpt: true,
    });
    let mut result = exploration();
    result.interest_updates.push(SelfInterest {
        key: "patterns".into(),
        topic: "图案".into(),
        phase: SelfInterestPhase::Curious,
        strength: 60,
        reason: "这些图案让我好奇".into(),
        want_to_try: "看看另一种排列".into(),
        evidence: vec!["experience-one".into()],
    });
    validate_idle_reflection(&observed, &result).unwrap();
    revise_idle_mind(IdleMind::default(), &result, "review-one", 100).unwrap()
}

#[test]
fn zero_tool_reflection_cannot_strengthen_an_interest_but_can_choose_rest() {
    let mind = formed();
    let mut observed = observation();
    observed.revision = mind.revision;
    observed.self_interests = mind.self_interests.clone();
    observed.evidence.push(IdleEvidence {
        id: "new-session-with-zero-tools".into(),
        kind: "exploration_reflection".into(),
        text: "想了想，也许还喜欢这些图案".into(),
        excerpt: false,
    });
    let mut result = exploration();
    let mut update = mind.self_interests[0].current.clone();
    update.strength += 1;
    update.evidence = vec![observed.evidence[0].id.clone()];
    result.interest_updates = vec![update.clone()];
    assert!(validate_idle_reflection(&observed, &result).is_err());
    update.phase = SelfInterestPhase::Resting;
    update.want_to_try.clear();
    update.evidence.clear();
    result.interest_updates = vec![update.clone()];
    assert!(
        validate_idle_reflection(&observed, &result).is_err(),
        "休息也不能假称强度增加"
    );
    update.strength = 40;
    result.interest_updates = vec![update];
    validate_idle_reflection(&observed, &result).unwrap();
    let next = revise_idle_mind(mind, &result, "review-two", 200).unwrap();
    assert_eq!(
        next.self_interests[0].current.phase,
        SelfInterestPhase::Resting
    );
    assert_eq!(next.self_interests[0].consumed_evidence, ["experience-one"]);
    observed.self_interests = next.self_interests;
    result.interest_updates.clear();
    result.exploration = Some(IdleExploration {
        cue: None,
        interest_key: Some("patterns".into()),
    });
    validate_idle_reflection(&observed, &result).unwrap();
}

#[test]
fn confirmed_experience_can_change_an_interest_but_relabelled_old_source_cannot() {
    let mind = formed();
    let mut observed = observation();
    observed.revision = mind.revision;
    observed.self_interests = mind.self_interests.clone();
    let mut result = exploration();
    let mut update = mind.self_interests[0].current.clone();
    update.strength = 70;
    observed.evidence.push(IdleEvidence {
        id: "experience-one".into(),
        kind: "exploration_experience".into(),
        text: "same receipt".into(),
        excerpt: true,
    });
    result.interest_updates = vec![update.clone()];
    assert!(validate_idle_reflection(&observed, &result).is_err());
    observed.evidence[0].id = "experience-two".into();
    observed.evidence[0].text = "实际读取了另一种图案".into();
    update.evidence = vec!["experience-two".into()];
    result.interest_updates = vec![update];
    validate_idle_reflection(&observed, &result).unwrap();
}
