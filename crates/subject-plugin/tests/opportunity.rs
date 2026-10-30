use eve_subject_api::*;
use eve_subject_plugin::*;

fn scope(user: &str) -> SubjectScope {
    SubjectScope {
        channel: "qq".into(),
        conversation_id: "group".into(),
        principal_id: user.into(),
    }
}
fn candidate(id: &str, key: &str, benefit: u8) -> SubjectFocus {
    SubjectFocus {
        id: id.into(),
        scope: scope("owner"),
        title: "检查模组构建流程".into(),
        goal: encode_opportunity(&Opportunity {
            goal: "复现并核验实际构建结果".into(),
            assessment: OpportunityAssessment {
                problem_key: key.into(),
                expected_gain: "避免重复构建失败".into(),
                verification: "构建并加载模组".into(),
                reuse_plan: "提炼参数化检查工具，再运行不同参数验证".into(),
                benefit,
                confidence: 100,
                cost: 10,
                risk: 0,
                evidence: vec![],
            },
        })
        .unwrap(),
        task_id: None,
        status: FocusStatus::Blocked,
        next_check_at_ms: 1,
        updated_at_ms: 1,
    }
}

#[test]
fn chooses_value_not_insertion_order_and_does_not_promote_permissions() {
    let low = candidate("candidate-a", "slow", 20);
    let high = candidate("candidate-b", "useful", 90);
    let mut foreign = candidate("candidate-c", "foreign", 100);
    foreign.scope = scope("stranger");
    let all = vec![low, foreign, high];
    assert_eq!(
        select_opportunity(&all, &scope("owner"))
            .unwrap()
            .unwrap()
            .id,
        "candidate-b"
    );
    assert_eq!(all[2].status, FocusStatus::Blocked);
}

#[test]
fn restart_and_new_grant_cannot_repeat_same_problem_with_new_title() {
    let mut old = candidate("auto-1-0", "same-problem", 80);
    old.status = FocusStatus::Completed;
    let mut next = candidate("candidate-new", "same-problem", 99);
    next.title = "改个标题仍是原问题".into();
    let bytes = serde_json::to_vec(&vec![old, next]).unwrap();
    let restored: Vec<SubjectFocus> = serde_json::from_slice(&bytes).unwrap();
    assert!(
        select_opportunity(&restored, &scope("owner"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn low_value_waits_and_corrupt_assessment_fails_closed() {
    let low = candidate("candidate-low", "negative", 5);
    assert!(
        select_opportunity(&[low], &scope("owner"))
            .unwrap()
            .is_none()
    );
    let mut broken = candidate("candidate-broken", "broken", 80);
    broken.goal = "eve-opportunity-v1\n{}".into();
    assert!(select_opportunity(&[broken], &scope("owner")).is_err());
}

#[test]
fn legacy_candidates_are_compatible_and_deduplicated_across_generations() {
    let mut legacy = candidate("candidate-old", "unused", 90);
    legacy.goal = "历史未结构化目标".into();
    assert!(
        select_opportunity(&[legacy.clone()], &scope("owner"))
            .unwrap()
            .is_some()
    );
    let mut used = legacy.clone();
    used.id = "auto-2-0".into();
    used.status = FocusStatus::Completed;
    assert!(
        select_opportunity(&[used, legacy], &scope("owner"))
            .unwrap()
            .is_none()
    );
}
