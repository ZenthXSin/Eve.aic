use super::*;
use crate::qq_natural_goals::tests as fixture;
use eve_cognition_api::{CognitiveSnapshot, ExecutionBudget, Source};
use eve_cognition_plugin::{CognitionController, CognitionPlugin, UserGoalFeedback};
use eve_kernel::{Kernel, KernelServices};
use eve_plugin_api::{PluginId, ServiceId};

struct Setup {
    kernel: Kernel,
    commands: Arc<Commands>,
    cognition: CognitionController,
    plans: eve_plan_plugin::PlanController,
    goal: Goal,
    plan: Plan,
}
async fn setup() -> Setup {
    let services = KernelServices::default();
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
    let cognition = CognitionPlugin::new("eve").unwrap();
    let controller = cognition.controller();
    kernel.register(Box::new(cognition)).unwrap();
    kernel
        .start(&PluginId::new(eve_cognition_api::COGNITION_PLUGIN_ID).unwrap())
        .await
        .unwrap();
    let plans = eve_plan_plugin::PlanPlugin::new("eve").unwrap();
    let plan_controller = plans.controller();
    kernel.register(Box::new(plans)).unwrap();
    kernel
        .start(&PluginId::new(PLAN_PLUGIN_ID).unwrap())
        .await
        .unwrap();
    kernel
        .register(Box::new(eve_session_plugin::SessionPlugin::new().unwrap()))
        .unwrap();
    kernel
        .start(&PluginId::new(eve_session_api::SESSION_PLUGIN_ID).unwrap())
        .await
        .unwrap();
    let sessions = registry
        .get(&ServiceId::new(eve_session_api::SESSION_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<eve_session_api::SessionServiceHandle>()
        .unwrap()
        .0
        .clone();
    let goal = Goal {
        id: "journey".into(),
        revision: 0,
        source: Source {
            kind: SourceKind::User,
            channel: GOAL_CHANNEL.into(),
            reference: "real-message".into(),
        },
        visibility: Visibility::User("administrator".into()),
        description: "阅读资料形成计划".into(),
        verification: USER_GOAL.into(),
        priority: 50,
        budget: ExecutionBudget {
            max_model_requests: 1,
            max_tool_calls: 0,
            max_attempts: 1,
            timeout_ms: 30000,
        },
        stop_condition: "user-confirmation".into(),
        expires_at_ms: None,
        status: GoalStatus::Waiting,
        wait_reason: Some("等候处理".into()),
        block_reason: None,
        execution: None,
        feedback: None,
        completion: None,
    };
    let CognitiveSnapshot {
        revision,
        mut state,
        ..
    } = controller.snapshot().unwrap();
    state.goals.insert(goal.id.clone(), goal);
    let saved = controller.replace(revision, state).unwrap();
    let goal = saved.state.goals["journey"].clone();
    let capabilities = capabilities(true, false);
    let reserved = plan_controller
        .reserve_proposal(&binding(&goal), "test-proposer", 1)
        .unwrap();
    let plan = plan_controller
        .finish_proposal(
            &reserved.record.id,
            ProposalOutcome::Steps(vec![StepSpec {
                id: "read".into(),
                title: "查阅受控资料".into(),
                capability: RESEARCH.into(),
                depends_on: vec![],
                max_attempts: 1,
                timeout_ms: RESEARCH_TIMEOUT_MS,
                effect: EffectCondition::Verified,
            }]),
            &capabilities,
            2,
        )
        .unwrap()
        .plan
        .unwrap();
    let (sender, _) = mpsc::unbounded_channel();
    let commands = Commands::enabled(
        Arc::new(plan_controller.clone()),
        Arc::new(controller.clone()),
        sessions,
        capabilities,
        "test-proposer".into(),
        sender,
    );
    Setup {
        kernel,
        commands,
        cognition: controller,
        plans: plan_controller,
        goal,
        plan,
    }
}
fn tool(commands: Arc<Commands>, review: Review) -> ManageTool {
    let late = Arc::new(OnceLock::new());
    assert!(late.set(commands).is_ok());
    ManageTool {
        commands: late,
        review,
    }
}
fn change(action: &str, plan: &Plan) -> ToolCall {
    fixture::call(
        "manage_plan",
        action,
        json!({"action":action,"goal_id":plan.binding.goal_id,
        "plan_id":plan.id,"expected_revision":plan.revision}),
    )
}
fn context(call: &ToolCall) -> ToolExecutionContext {
    fixture::context(call, "administrator", vec!["确认已经展示的研究计划".into()])
}

#[tokio::test]
async fn confirmation_reviews_complete_plan_and_actual_revision_before_activation() {
    let s = setup().await;
    let dir = tempfile::tempdir().unwrap();
    let reviewer = fixture::reviewer();
    let tool = tool(
        s.commands,
        Review {
            admin: fixture::admin(dir.path()),
            reviewer: Some(reviewer.clone()),
        },
    );
    let call = change("confirm", &s.plan);
    let result = tool.execute(call.clone(), context(&call)).await.unwrap();
    assert_eq!(result["plan"]["status"], "active");
    assert_eq!(result["goal_completed"], false);
    assert_eq!(
        s.cognition.snapshot().unwrap().state.goals[&s.goal.id],
        s.goal
    );
    let request = reviewer.requests.lock().unwrap()[0].clone();
    let reviewed: Value = serde_json::from_str(&request.arguments[0]).unwrap();
    assert_eq!(
        reviewed["operation"]["plan"],
        serde_json::to_value(&s.plan).unwrap()
    );
    assert_eq!(
        reviewed["operation"]["goal"],
        serde_json::to_value(&s.goal).unwrap()
    );
    assert_eq!(
        reviewed["operation"]["capabilities"][0]["max_timeout_ms"],
        RESEARCH_TIMEOUT_MS
    );
    let before = s.plans.snapshot().unwrap();
    assert!(tool.execute(call.clone(), context(&call)).await.is_err());
    assert_eq!(s.plans.snapshot().unwrap(), before);
    assert_eq!(reviewer.requests.lock().unwrap().len(), 1);
    let plan = &before.plans[0];
    let withdraw = change("withdraw", plan);
    let result = tool
        .execute(withdraw.clone(), context(&withdraw))
        .await
        .unwrap();
    assert_eq!(result["plan"]["status"], "withdrawn");
    s.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn goal_changed_during_plan_review_cannot_activate_old_steps() {
    use eve_cognition_api::GoalFeedbackService;
    let s = setup().await;
    let dir = tempfile::tempdir().unwrap();
    let mut reviewer = fixture::reviewer();
    let cognition = s.cognition.clone();
    let goal = s.goal.clone();
    Arc::get_mut(&mut reviewer).unwrap().during = Some(Box::new(move || {
        let service = UserGoalFeedback::new(
            Arc::new(cognition.clone()),
            "eve".into(),
            "administrator".into(),
            GOAL_CHANNEL.into(),
            "qq.goal.feedback".into(),
        )
        .unwrap();
        service
            .submit(eve_cognition_api::GoalFeedbackInput {
                goal_id: goal.id.clone(),
                expected_goal_revision: goal.revision,
                feedback_id: "changed-fact".into(),
                text: "研究范围改变，旧计划不要执行".into(),
                at_ms: 3,
            })
            .unwrap();
    }));
    let tool = tool(
        s.commands,
        Review {
            admin: fixture::admin(dir.path()),
            reviewer: Some(reviewer),
        },
    );
    let call = change("confirm", &s.plan);
    assert!(tool.execute(call.clone(), context(&call)).await.is_err());
    assert_eq!(s.plans.snapshot().unwrap().plans[0], s.plan);
    assert_eq!(
        s.cognition.snapshot().unwrap().state.goals[&s.goal.id].revision,
        s.goal.revision + 1
    );
    s.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn closed_goal_wrong_owner_and_changed_plan_are_rejected() {
    let s = setup().await;
    let dir = tempfile::tempdir().unwrap();
    let reviewer = fixture::reviewer();
    let tool = tool(
        s.commands.clone(),
        Review {
            admin: fixture::admin(dir.path()),
            reviewer: Some(reviewer.clone()),
        },
    );
    let call = change("confirm", &s.plan);
    let other = fixture::context(&call, "other-administrator", vec!["确认别人的计划".into()]);
    assert!(tool.execute(call.clone(), other).await.is_err());
    let mut snapshot = s.cognition.snapshot().unwrap();
    let goal = snapshot.state.goals.get_mut(&s.goal.id).unwrap();
    goal.status = GoalStatus::Cancelled;
    goal.wait_reason = None;
    s.cognition
        .replace(snapshot.revision, snapshot.state)
        .unwrap();
    assert!(tool.execute(call.clone(), context(&call)).await.is_err());
    assert!(reviewer.requests.lock().unwrap().is_empty());
    assert_eq!(s.plans.snapshot().unwrap().plans[0], s.plan);
    s.kernel.stop_all().await.unwrap();

    let s = setup().await;
    let mut reviewer = fixture::reviewer();
    let plans = s.plans.clone();
    let plan = s.plan.clone();
    Arc::get_mut(&mut reviewer).unwrap().during = Some(Box::new(move || {
        plans.withdraw(&plan.id, plan.revision, 3).unwrap();
    }));
    let tool = super::tests::tool(
        s.commands,
        Review {
            admin: fixture::admin(dir.path()),
            reviewer: Some(reviewer),
        },
    );
    let call = change("confirm", &s.plan);
    assert!(tool.execute(call.clone(), context(&call)).await.is_err());
    assert_eq!(
        s.plans.snapshot().unwrap().plans[0].status,
        PlanStatus::Withdrawn
    );
    s.kernel.stop_all().await.unwrap();
}

struct UnusedProposer;
impl PlanProposer for UnusedProposer {
    fn version(&self) -> &str {
        "unused"
    }
    fn propose(&self, _: ProposalRequest) -> ProposalFuture<'_> {
        Box::pin(async { panic!("执行准入不应重新请求计划") })
    }
}

#[tokio::test]
async fn restart_rechecks_revocation_before_step_and_research_admission() {
    let s = setup().await;
    let dir = tempfile::tempdir().unwrap();
    let admin = fixture::admin(dir.path());
    let tool = tool(
        s.commands,
        Review {
            admin: admin.clone(),
            reviewer: Some(fixture::reviewer()),
        },
    );
    let call = change("confirm", &s.plan);
    tool.execute(call.clone(), context(&call)).await.unwrap();
    let scope = context(&call).scope().unwrap().clone();
    assert_eq!(
        admin.business.reviewed_plan_scope(&s.plan.id).unwrap(),
        Some(scope.clone())
    );
    let mutate = |action: &str, turn, admin: &crate::qq_admin::Admin| {
        admin
            .business
            .mutate(crate::qq_business::Mutation {
                scope: scope.clone(),
                actor: scope.user_id.clone(),
                action: action.into(),
                target: None,
                at_ms: crate::qq_tasks::now_ms(),
                turn_id: Some(turn),
                user_messages: vec![format!("现在{action}当前业务权限")],
                verdict: None,
                outcome: "committed".into(),
                expected_revision: admin.business.revision().unwrap(),
            })
            .unwrap();
    };
    mutate("revoke", 18, &admin);
    let restarted_admin = fixture::admin(dir.path());
    let services = Services {
        plans: Arc::new(s.plans.clone()),
        cognition: Arc::new(s.cognition.clone()),
        knowledge: None,
        practice: None,
        proposer: Arc::new(UnusedProposer),
        capabilities: capabilities(true, false),
        authorization: restarted_admin.clone(),
    };
    let before = s.plans.snapshot().unwrap();
    settle(&services, now_ms().unwrap()).unwrap();
    assert_eq!(
        s.plans.snapshot().unwrap(),
        before,
        "撤权后未启动任何步骤，也没有抹掉活动计划"
    );
    assert!(!permitted(&restarted_admin, &s.goal, &s.plan.id));
    mutate("unlock", 19, &restarted_admin);
    settle(&services, now_ms().unwrap()).unwrap();
    let begun = s.plans.snapshot().unwrap();
    assert_eq!(begun.plans[0].steps[0].status, StepStatus::Executing);
    let cognition = s.cognition.snapshot().unwrap();
    assert_eq!(
        authorized_requests(&begun, &cognition, RESEARCH, &restarted_admin),
        vec![(s.goal.id.clone(), s.goal.revision)]
    );
    assert!(admitted(
        &s.goal,
        Some(&s.plans),
        RESEARCH,
        &restarted_admin
    ));
    mutate("revoke", 20, &restarted_admin);
    assert!(authorized_requests(&begun, &cognition, RESEARCH, &restarted_admin).is_empty());
    assert!(!admitted(
        &s.goal,
        Some(&s.plans),
        RESEARCH,
        &restarted_admin
    ));
    assert_eq!(
        s.plans.snapshot().unwrap(),
        begun,
        "已记录的步骤不能因撤权伪装未执行或清除"
    );
    s.kernel.stop_all().await.unwrap();
}
