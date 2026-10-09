use eve_cognition_api::{
    CognitionAdmin, CognitiveState, ExecutionBudget, Goal, GoalStatus, Source, SourceKind,
    Visibility,
};
use eve_cognition_plugin::{CognitionController, CognitionPlugin};
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_outreach_api::*;
use eve_outreach_plugin::{OutreachController, OutreachPlugin, RequestGoals};
use std::sync::Arc;

const OWNER: &str = "qq:owner";

async fn open() -> (Kernel, OutreachController, CognitionController) {
    let kernel = Kernel::with_services(KernelServices {
        state: Arc::new(MemoryStateStore::default()),
        ..KernelServices::default()
    });
    let outreach = OutreachPlugin::new().unwrap();
    let outreach_admin = outreach.controller();
    let cognition = CognitionPlugin::new("eve").unwrap();
    let cognition_admin = cognition.controller();
    kernel.register(Box::new(outreach)).unwrap();
    kernel.register(Box::new(cognition)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, outreach_admin, cognition_admin)
}

fn edit(cognition: &CognitionController, change: impl FnOnce(&mut CognitiveState)) {
    let snapshot = cognition.snapshot().unwrap();
    let mut state = snapshot.state.clone();
    change(&mut state);
    cognition.replace(snapshot.revision, state).unwrap();
}

/// 兴趣派生的学习目标；描述是上一目标的背景。
fn learning_goal(cognition: &CognitionController, id: &str) {
    edit(cognition, |state| {
        state.goals.insert(
            id.into(),
            Goal {
                id: id.into(),
                revision: 0,
                source: Source {
                    kind: SourceKind::Inference,
                    channel: "interest.learning".into(),
                    reference: "interest-1".into(),
                },
                visibility: Visibility::User(OWNER.into()),
                description: "学习目标背景：用户想学做游戏模组".into(),
                verification: "interest-learning:v1".into(),
                priority: 30,
                budget: ExecutionBudget {
                    max_model_requests: 1,
                    max_tool_calls: 0,
                    max_attempts: 1,
                    timeout_ms: 30_000,
                },
                stop_condition: "用户撤回该兴趣".into(),
                expires_at_ms: None,
                status: GoalStatus::Waiting,
                wait_reason: Some("等待研究与实践".into()),
                block_reason: None,
                execution: None,
                feedback: None,
            },
        );
    });
}

/// 为目标撰写、附带并送达一条邀请，用户回应后记下识别结论。
fn answered(
    admin: &OutreachController,
    goal: &str,
    at: u64,
    verdict: ResponseVerdict,
) -> Invitation {
    let invitation = admin
        .begin(
            OWNER,
            goal,
            Milestone {
                practice_run_id: format!("practice-{at}"),
                skill_id: None,
            },
            vec![Fact {
                kind: FactKind::Progress,
                text: "实际加载并通过探测".into(),
            }],
            "composer:v1",
            at,
        )
        .unwrap()
        .unwrap();
    admin
        .record_composition(&invitation.id, at + 1, Ok("做好了，你想加点什么？".into()))
        .unwrap();
    let message = format!("chat-{at}");
    admin
        .begin_judgement(&invitation.id, &message, at + 2)
        .unwrap();
    admin
        .record_judgement(&invitation.id, &message, at + 3, Ok(Verdict::Invite))
        .unwrap();
    admin
        .claim(
            &invitation.id,
            at + 4,
            DeliveryChannel::Passive {
                message_id: message,
            },
        )
        .unwrap();
    admin
        .record_delivery(
            &invitation.id,
            at + 5,
            AttemptResult::Sent {
                platform_message_id: None,
            },
        )
        .unwrap();
    let reply = verdict
        .message_id
        .clone()
        .unwrap_or_else(|| format!("reply-{at}"));
    admin
        .begin_response(
            &invitation.id,
            vec![ResponseTurn {
                evidence_id: format!("evidence-{at}"),
                message_id: reply,
                at_ms: at + 6,
            }],
            at + 7,
        )
        .unwrap();
    admin
        .record_response(&invitation.id, at + 8, Ok(verdict))
        .unwrap()
}

fn request(message: &str, quote: &str) -> ResponseVerdict {
    ResponseVerdict {
        kind: ResponseKind::Request,
        message_id: Some(message.into()),
        quote: Some(quote.into()),
    }
}

#[tokio::test]
async fn a_proposed_idea_becomes_one_waiting_follow_up_goal_and_chains_to_the_learning_goal() {
    let (kernel, outreach, cognition) = open().await;
    learning_goal(&cognition, "goal-learning");
    let deriver = RequestGoals::new(Arc::new(cognition.clone()), "eve").unwrap();
    let first = answered(
        &outreach,
        "goal-learning",
        100,
        request("idea-1", "加一个会发光的墙"),
    );
    let report = deriver
        .reconcile(&outreach.snapshot().unwrap(), 1_000)
        .unwrap();
    let id = request_goal_id("eve", &first.id);
    assert_eq!(report.created, vec![id.clone()]);
    let snapshot = cognition.snapshot().unwrap();
    let goal = &snapshot.state.goals[&id];
    assert_eq!(
        (
            goal.source.kind.clone(),
            goal.source.channel.as_str(),
            goal.source.reference.as_str()
        ),
        (
            SourceKind::Inference,
            REQUEST_GOAL_CHANNEL,
            first.id.as_str()
        ),
        "模型识别的类别不冒充用户指令"
    );
    assert_eq!(goal.visibility, Visibility::User(OWNER.into()));
    assert_eq!(
        (
            goal.status.clone(),
            goal.priority,
            goal.verification.as_str()
        ),
        (GoalStatus::Waiting, 40, REQUEST_GOAL_VERIFICATION)
    );
    for part in [
        "“加一个会发光的墙”（消息 idea-1）",
        "做好了，你想加点什么？",
        "学习目标背景",
    ] {
        assert!(goal.description.contains(part), "{part}");
    }
    let marker = RequestMarker::parse(goal.wait_reason.as_deref().unwrap()).unwrap();
    assert_eq!(
        (
            marker.invitation_id.as_str(),
            marker.parent_goal_id.as_str(),
            marker.learning_goal_id.as_str()
        ),
        (first.id.as_str(), "goal-learning", "goal-learning")
    );
    let event = snapshot.state.events.last().unwrap();
    assert_eq!(event.goal_id.as_deref(), Some(id.as_str()));
    assert!(event.summary.contains("\"user_instruction\":false"));

    // 重复同步不另建目标、不写状态。
    let revision = cognition.snapshot().unwrap().revision;
    assert_eq!(
        deriver
            .reconcile(&outreach.snapshot().unwrap(), 1_100)
            .unwrap(),
        RequestReport::default()
    );
    assert_eq!(cognition.snapshot().unwrap().revision, revision);

    // 后续创作做成后再次邀请，用户又提出想法：沿用最初的学习目标。
    let second = answered(&outreach, &id, 200, request("idea-2", "再加一个炮台"));
    let report = deriver
        .reconcile(&outreach.snapshot().unwrap(), 1_200)
        .unwrap();
    let next = request_goal_id("eve", &second.id);
    assert_eq!(report.created, vec![next.clone()]);
    let marker = RequestMarker::parse(
        cognition.snapshot().unwrap().state.goals[&next]
            .wait_reason
            .as_deref()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        (
            marker.parent_goal_id.as_str(),
            marker.learning_goal_id.as_str()
        ),
        (id.as_str(), "goal-learning")
    );

    // 其他回应不派生目标。
    learning_goal(&cognition, "goal-other");
    answered(
        &outreach,
        "goal-other",
        300,
        ResponseVerdict {
            kind: ResponseKind::Interested,
            message_id: Some("ok".into()),
            quote: Some("好啊".into()),
        },
    );
    assert_eq!(
        deriver
            .reconcile(&outreach.snapshot().unwrap(), 1_300)
            .unwrap(),
        RequestReport::default()
    );

    // 用户撤回兴趣、学习目标取消后，仍在等待的后续目标一并取消；已有的结论不改写。
    let cancel = |goal: &mut Goal| {
        goal.status = GoalStatus::Cancelled;
        goal.wait_reason = None;
    };
    edit(&cognition, |state| {
        cancel(state.goals.get_mut(&next).unwrap());
        cancel(state.goals.get_mut("goal-learning").unwrap());
    });
    let concluded = cognition.snapshot().unwrap().state.goals[&next].clone();
    let report = deriver
        .reconcile(&outreach.snapshot().unwrap(), 1_400)
        .unwrap();
    assert_eq!(report.cancelled, vec![id.clone()]);
    let state = cognition.snapshot().unwrap().state;
    assert_eq!(state.goals[&id].status, GoalStatus::Cancelled);
    assert_eq!(state.goals[&next], concluded);
    assert_eq!(
        deriver
            .reconcile(&outreach.snapshot().unwrap(), 1_500)
            .unwrap(),
        RequestReport::default()
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn closed_learning_goals_and_conflicting_goals_are_not_derived_or_overwritten() {
    let (kernel, outreach, cognition) = open().await;
    learning_goal(&cognition, "goal-closed");
    let invitation = answered(
        &outreach,
        "goal-closed",
        100,
        request("idea", "加一个会发光的墙"),
    );
    edit(&cognition, |state| {
        let goal = state.goals.get_mut("goal-closed").unwrap();
        goal.status = GoalStatus::Cancelled;
        goal.wait_reason = None;
    });
    let deriver = RequestGoals::new(Arc::new(cognition.clone()), "eve").unwrap();
    assert_eq!(
        deriver
            .reconcile(&outreach.snapshot().unwrap(), 1_000)
            .unwrap(),
        RequestReport::default(),
        "学习目标已关闭时不派生"
    );

    assert!(
        !cognition
            .snapshot()
            .unwrap()
            .state
            .goals
            .contains_key(&request_goal_id("eve", &invitation.id))
    );

    // 同名但来源不符的目标拒绝改写。
    learning_goal(&cognition, "goal-open");
    let other = answered(&outreach, "goal-open", 200, request("idea-2", "加一个炮台"));
    let id = request_goal_id("eve", &other.id);
    edit(&cognition, |state| {
        let mut goal = state.goals["goal-open"].clone();
        goal.id = id.clone();
        goal.revision = 0;
        state.goals.insert(id.clone(), goal);
    });
    assert_eq!(
        deriver.reconcile(&outreach.snapshot().unwrap(), 1_100),
        Err(OutreachError::Conflict)
    );
    assert!(RequestGoals::new(Arc::new(cognition.clone()), " bad").is_err());
    kernel.stop_all().await.unwrap();
}
