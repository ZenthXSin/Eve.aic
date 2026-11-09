use super::*;
use eve_llm_api::{ChatMessage, ChatRole};
use eve_session_api::{SessionKey, SessionTurn};
use serde_json::json;

fn scope() -> SubjectScope {
    SubjectScope {
        channel: "qq".into(),
        conversation_id: "private-owner".into(),
        principal_id: "owner".into(),
    }
}

fn focus(id: &str, status: FocusStatus, updated_at_ms: u64) -> SubjectFocus {
    SubjectFocus {
        id: id.into(),
        scope: scope(),
        title: "原关注".into(),
        goal: "原记录继续保留".into(),
        task_id: None,
        status,
        next_check_at_ms: updated_at_ms + 1,
        updated_at_ms,
    }
}

fn task(id: &str, source: &str, status: TaskStatus, at: u64) -> Task {
    serde_json::from_value(json!({
        "id": id, "source": source,
        "owner": {"session_id": "private-owner", "user_id": "owner"},
        "spec": {"title": "原任务", "instructions": "保留原结果", "max_steps": 1},
        "created_at_ms": 1, "status": status, "cancel_requested": false,
        "started_at_ms": 1, "finished_at_ms": at,
        "usage": {"tool_calls": 0, "model_requests": 1},
        "result": null, "failure": "原失败记录", "notice": null
    }))
    .unwrap()
}

fn conversation() -> SessionSnapshot {
    SessionSnapshot {
        key: SessionKey::new("private-owner", "owner").unwrap(),
        revision: 10,
        turns: (1..=10)
            .map(|id| SessionTurn {
                id,
                input: "同范围交流".into(),
                status: SessionTurnStatus::Completed {
                    messages: vec![ChatMessage::text(ChatRole::Assistant, "已保存的回应")],
                },
            })
            .collect(),
    }
}

fn experience(id: &str, at: u64) -> ExplorationExperience {
    ExplorationExperience {
        session_id: id.into(),
        scope: scope(),
        started_at_ms: at,
        ended_at_ms: at + 1,
        end_reason: ExplorationEndReason::Natural,
        tools: vec![ExplorationToolExperience {
            call_id: "read-1".into(),
            tool: "read_reference".into(),
            arguments: crate::qq_exploration::excerpt("{}"),
            output: Some(crate::qq_exploration::excerpt("原阅读回执")),
            outcome: ExplorationToolOutcome::Succeeded,
            error: None,
        }],
        total_tool_calls: 1,
        tool_offset: 0,
        tools_truncated: false,
        subjective_notes: Some(crate::qq_exploration::excerpt("自己的感想")),
    }
}

fn observe(
    foci: Vec<SubjectFocus>,
    tasks: &[Task],
    session: Option<&SessionSnapshot>,
    experiences: Vec<ExplorationExperience>,
) -> SubjectResult<Observation> {
    observation_with_mind(
        &SubjectSnapshot {
            events: vec![],
            foci,
        },
        tasks,
        &scope(),
        &["自由探索".into()],
        Mind::default(),
        session,
        experiences,
    )
}

#[test]
fn old_focus_saturation_preserves_recent_conversation_exploration_and_all_task_outcomes() {
    let mut tasks = vec![task(
        "latest-reflection",
        IDLE_TASK_SOURCE_PREFIX,
        TaskStatus::Completed,
        300,
    )];
    for (index, status) in [
        TaskStatus::Interrupted,
        TaskStatus::Cancelled,
        TaskStatus::Failed,
        TaskStatus::Completed,
    ]
    .into_iter()
    .enumerate()
    {
        tasks.push(task(
            &format!("result-{index}"),
            "tool:read",
            status,
            200 + index as u64,
        ));
    }
    let before = serde_json::to_vec(&tasks).unwrap();
    let mut foci: Vec<_> = (0..20)
        .map(|index| focus(&format!("old-{index}"), FocusStatus::Completed, 100 + index))
        .collect();
    // 最近更新的活动关注在旧插入位置，不能按 Vec 的逆序挑选。
    foci.insert(0, focus("active-older", FocusStatus::Active, 501));
    foci.insert(0, focus("active-newer", FocusStatus::Active, 502));
    let session = conversation();
    let observed = observe(
        foci,
        &tasks,
        Some(&session),
        vec![
            experience("explore-3", 103),
            experience("explore-2", 102),
            experience("explore-1", 101),
        ],
    )
    .unwrap();
    observed.validate().unwrap();
    assert_eq!(observed.evidence.len(), 16);
    for (kind, count) in [
        ("exploration_experience", 3),
        ("completed_conversation", 6),
        ("task_result", 4),
        ("self_reflection", 1),
        ("focus", 2),
    ] {
        assert_eq!(
            observed.evidence.iter().filter(|e| e.kind == kind).count(),
            count
        );
    }
    let ids: Vec<_> = observed.evidence.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(
        &ids[3..9],
        &["turn-10", "turn-9", "turn-8", "turn-7", "turn-6", "turn-5"]
    );
    assert_eq!(&ids[14..], &["active-newer", "active-older"]);
    for (index, status) in ["Interrupted", "Cancelled", "Failed", "Completed"]
        .into_iter()
        .enumerate()
    {
        let item = observed
            .evidence
            .iter()
            .find(|e| e.id == format!("result-{index}"))
            .unwrap();
        let detail: serde_json::Value = serde_json::from_str(&item.text).unwrap();
        assert_eq!(detail["status"], status);
        assert_eq!(detail["failure_excerpt"], "原失败记录");
    }
    assert_eq!(serde_json::to_vec(&tasks).unwrap(), before);
}

#[test]
fn previous_idle_choice_is_historical_and_keeps_personal_thoughts_and_interest_text() {
    let mut review = task(
        "previous-review",
        IDLE_TASK_SOURCE_PREFIX,
        TaskStatus::Completed,
        4,
    );
    let thought = "想看看陌生的旋律。".repeat(35);
    let result = json!({
        "choice":"wait", "thought":thought, "reason":"现在想休息，之前没有明确工程目标",
        "judgments":[], "action":null, "revisit_after_minutes":30,
        "interest_updates":[{"key":"melody","topic":"旋律","phase":"curious","strength":40,
            "reason":"先前听到的变化让我好奇","want_to_try":"比较两段旋律","evidence":["source-task"]}]
    });
    review.failure = None;
    review.result = Some(result.to_string());
    let before = serde_json::to_vec(&review).unwrap();
    let observed = observe(vec![], std::slice::from_ref(&review), None, vec![]).unwrap();
    observed.validate().unwrap();
    let evidence = &observed.evidence[0];
    let detail: serde_json::Value = serde_json::from_str(&evidence.text).unwrap();
    assert_eq!(evidence.kind, "self_reflection");
    assert_eq!(
        detail["previous_decision"]["applies_to"],
        "previous_review_only"
    );
    assert_eq!(detail["previous_decision"]["choice"], "wait");
    assert_eq!(detail["previous_decision"]["thought"], thought);
    assert_eq!(detail["previous_decision"]["reason"], result["reason"]);
    assert_eq!(
        detail["previous_decision"]["interest_updates"],
        result["interest_updates"]
    );
    assert!(detail.get("model_result_excerpt").is_none());
    assert!(detail["notice"].as_str().unwrap().contains("不是本轮指令"));
    assert_eq!(serde_json::to_vec(&review).unwrap(), before);
}

#[test]
fn malformed_or_unfinished_idle_results_remain_unknown_with_original_failure() {
    for status in [TaskStatus::Completed, TaskStatus::Interrupted] {
        let mut review = task("broken-review", IDLE_TASK_SOURCE_PREFIX, status, 4);
        review.result = Some("{不完整的原回顾".into());
        let detail: serde_json::Value =
            serde_json::from_str(&task_evidence(&review).unwrap().text).unwrap();
        assert_eq!(detail["model_result_excerpt"], "{不完整的原回顾");
        assert_eq!(detail["failure_excerpt"], "原失败记录");
        assert_eq!(detail["previous_decision_unavailable"], true);
        assert!(detail.get("previous_decision").is_none());
        assert_eq!(
            detail["status"],
            serde_json::to_value(review.status).unwrap()
        );
    }
}

#[test]
fn only_explicit_links_to_selected_task_results_remove_duplicate_focus_views() {
    let mut tasks: Vec<_> = (0..5)
        .map(|index| {
            task(
                &format!("result-{index}"),
                "tool:read",
                TaskStatus::Failed,
                20 + index,
            )
        })
        .collect();
    let mut foreign_task = task("foreign-task", "tool:read", TaskStatus::Completed, 1000);
    foreign_task.owner.user_id = "other".into();
    tasks.push(foreign_task);
    let mut represented = focus("represented", FocusStatus::Blocked, 50);
    represented.task_id = Some("result-4".into());
    let mut unrepresented = focus("unrepresented", FocusStatus::Blocked, 60);
    unrepresented.task_id = Some("result-0".into());
    let independent = focus("same-text-different-record", FocusStatus::Blocked, 70);
    let mut foreign = focus("foreign-focus", FocusStatus::Active, 1000);
    foreign.scope.channel = "other-channel".into();
    let observed = observe(
        vec![represented, unrepresented, independent, foreign],
        &tasks,
        None,
        vec![],
    )
    .unwrap();
    let ids: Vec<_> = observed.evidence.iter().map(|e| e.id.as_str()).collect();
    assert!(!ids.contains(&"represented"));
    assert!(ids.contains(&"unrepresented"));
    assert!(ids.contains(&"same-text-different-record"));
    assert!(!ids.contains(&"foreign-focus"));
    assert!(!ids.contains(&"foreign-task"));
}

#[test]
fn conversation_and_exploration_scope_mismatches_are_not_accepted() {
    let mut session = conversation();
    session.key.session_id = "other".into();
    assert!(matches!(
        observe(vec![], &[], Some(&session), vec![]),
        Err(SubjectError::CorruptState)
    ));
    let mut foreign = experience("other-exploration", 1);
    foreign.scope.principal_id = "other".into();
    assert!(matches!(
        observe(vec![], &[], None, vec![foreign]),
        Err(SubjectError::CorruptState)
    ));
}

#[test]
fn empty_categories_leave_room_for_focus_without_overflowing_utf8_evidence_limits() {
    let mut foci: Vec<_> = (0..24)
        .map(|index| focus(&format!("focus-{index}"), FocusStatus::Completed, index + 1))
        .collect();
    for focus in &mut foci {
        focus.goal = "文字\u{0000}\"".repeat(2000);
    }
    let observed = observe(foci, &[], None, vec![]).unwrap();
    assert_eq!(observed.evidence.len(), 16);
    observed.validate().unwrap();
    assert_eq!(observed.evidence[0].id, "focus-23");
    for evidence in observed.evidence {
        assert!(evidence.text.len() <= 16_384);
        let detail: serde_json::Value = serde_json::from_str(&evidence.text).unwrap();
        assert!(detail["text_excerpt"].as_str().unwrap().len() <= 1200);
    }
}

#[test]
fn selection_then_interest_preparation_preserves_consumed_source_ids() {
    let temporary = tempfile::tempdir().unwrap();
    let store =
        eve_subject_sqlite::SqliteIdleMemory::open(&temporary.path().join("mind.sqlite3")).unwrap();
    let tasks = [task("source-task", "tool:read", TaskStatus::Completed, 2)];
    let initial = observe(vec![], &tasks, None, vec![]).unwrap();
    let result: IdleReflection = serde_json::from_value(json!({
        "choice": "reflect", "thought": "读过之后想继续看", "reason": "一次真实阅读", "judgments": [],
        "interest_updates": [{"key": "patterns", "topic": "规则变化", "phase": "curious", "strength": 40,
            "reason": "想了解变化", "want_to_try": "比较两种规则", "evidence": ["source-task"]}],
        "action": null, "revisit_after_minutes": 30
    })).unwrap();
    store.commit(&initial, &result, "review-first", 10).unwrap();
    let mut next = observation_with_mind(
        &SubjectSnapshot {
            events: vec![],
            foci: vec![],
        },
        &tasks,
        &scope(),
        &["自由探索".into()],
        store.snapshot(&scope()).unwrap(),
        None,
        vec![],
    )
    .unwrap();
    store.prepare_observation(&mut next).unwrap();
    next.validate().unwrap();
    assert_eq!(
        next.interest_consumed.get("patterns").unwrap(),
        &["source-task"]
    );
    let mut repeated = result;
    repeated.interest_updates[0].strength = 60;
    assert!(eve_subject_plugin::validate_idle_reflection(&next, &repeated).is_err());
    assert_eq!(
        store
            .history(&scope(), "patterns", 0, 8)
            .unwrap()
            .items
            .len(),
        1
    );
}
