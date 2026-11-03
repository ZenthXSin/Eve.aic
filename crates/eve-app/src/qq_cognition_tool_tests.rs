use super::*;
use crate::qq_natural_goals::tests as fixture;

async fn commands() -> (Kernel, Arc<Commands>, CognitionController) {
    let kernel = Kernel::with_services(KernelServices::default());
    let plugin = CognitionPlugin::new("eve").unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel
        .start(&PluginId::new(COGNITION_PLUGIN_ID).unwrap())
        .await
        .unwrap();
    let commands = Arc::new(Commands {
        admin: Some(admin.clone()),
        sessions: None,
        accepting: Arc::new(AtomicBool::new(true)),
        planning_budget: None,
    });
    (kernel, commands, admin)
}
fn tool(commands: Arc<Commands>, review: Review) -> ManageTool {
    let late = Arc::new(OnceLock::new());
    assert!(late.set(commands).is_ok());
    ManageTool {
        commands: late,
        review,
    }
}
fn context(call: &ToolCall) -> ToolExecutionContext {
    fixture::context(
        call,
        "administrator",
        vec!["记录整理出行计划，并接受我接下来的纠正".into()],
    )
}

#[tokio::test]
async fn own_goal_and_feedback_use_durable_version_and_do_not_complete_work() {
    let (kernel, commands, cognition) = commands().await;
    let dir = tempfile::tempdir().unwrap();
    let reviewer = fixture::reviewer();
    let tool = tool(
        commands,
        Review {
            admin: fixture::admin(dir.path()),
            reviewer: Some(reviewer.clone()),
        },
    );
    let create = fixture::call(
        "manage_goal",
        "create",
        json!({"action":"create","text":"整理出行计划"}),
    );
    let result = tool
        .execute(create.clone(), context(&create))
        .await
        .unwrap();
    assert_eq!(result["stored"], true);
    assert_eq!(result["goal"]["status"], "Waiting");
    assert_eq!(result["goal"]["description"], "整理出行计划");
    assert_eq!(result["external_goal_completed"], false);
    let id = result["goal"]["id"].as_str().unwrap();
    let revision = result["goal"]["revision"].as_u64().unwrap();
    let before = cognition.snapshot().unwrap();
    let duplicate = tool
        .execute(create.clone(), context(&create))
        .await
        .unwrap();
    assert_eq!(duplicate["goal"], result["goal"]);
    assert_eq!(cognition.snapshot().unwrap(), before);
    let inspect = fixture::call(
        "manage_goal",
        "inspect",
        json!({"action":"inspect","goal_id":id}),
    );
    let view = tool
        .execute(inspect.clone(), context(&inspect))
        .await
        .unwrap();
    assert_eq!(view["goal"], result["goal"]);
    assert!(view["draft"].is_null());
    assert_eq!(reviewer.requests.lock().unwrap().len(), 2, "只读不送审");
    let other = fixture::context(&inspect, "other-administrator", vec!["查看对方目标".into()]);
    assert!(tool.execute(inspect.clone(), other).await.is_err());
    let feedback = fixture::call(
        "manage_goal",
        "feedback",
        json!({"action":"feedback","goal_id":id,
        "expected_revision":revision,"text":"出发时间改成上午九点"}),
    );
    let result = tool
        .execute(feedback.clone(), context(&feedback))
        .await
        .unwrap();
    assert_eq!(result["feedback"]["goal_revision"], revision + 1);
    assert_eq!(result["goal"]["revision"], revision + 1);
    assert_eq!(result["goal"]["status"], "Waiting");
    assert!(result["goal"]["completion"].is_null());
    let snapshot = cognition.snapshot().unwrap();
    let full = serde_json::to_value(&snapshot).unwrap();
    assert!(full.to_string().contains("出发时间改成上午九点"));
    assert!(
        tool.execute(feedback.clone(), context(&feedback))
            .await
            .is_err()
    );
    assert_eq!(cognition.snapshot().unwrap(), snapshot);
    assert_eq!(
        reviewer.requests.lock().unwrap().len(),
        3,
        "旧版本不能再次送审或写入"
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn feedback_changed_while_reviewing_preserves_newer_fact() {
    let (kernel, commands, cognition) = commands().await;
    let dir = tempfile::tempdir().unwrap();
    let admin = fixture::admin(dir.path());
    let first = tool(
        commands.clone(),
        Review {
            admin: admin.clone(),
            reviewer: Some(fixture::reviewer()),
        },
    );
    let create = fixture::call(
        "manage_goal",
        "create",
        json!({"action":"create","text":"整理出行计划"}),
    );
    let saved = first
        .execute(create.clone(), context(&create))
        .await
        .unwrap();
    let id = saved["goal"]["id"].as_str().unwrap().to_owned();
    let version = saved["goal"]["revision"].as_u64().unwrap();
    let mut reviewer = fixture::reviewer();
    let current = cognition.clone();
    let changed_id = id.clone();
    Arc::get_mut(&mut reviewer).unwrap().during = Some(Box::new(move || {
        let service = UserGoalFeedback::new(
            Arc::new(current.clone()),
            "eve".into(),
            "administrator".into(),
            CHANNEL.into(),
            "qq.goal.feedback".into(),
        )
        .unwrap();
        service
            .submit(GoalFeedbackInput {
                goal_id: changed_id.clone(),
                expected_goal_revision: version,
                feedback_id: "newer-fact".into(),
                text: "最新出发时间是十点".into(),
                at_ms: 99,
            })
            .unwrap();
    }));
    let tool = tool(
        commands,
        Review {
            admin,
            reviewer: Some(reviewer),
        },
    );
    let feedback = fixture::call(
        "manage_goal",
        "older-feedback",
        json!({"action":"feedback","goal_id":id,
        "expected_revision":version,"text":"旧消息说九点"}),
    );
    assert!(
        tool.execute(feedback.clone(), context(&feedback))
            .await
            .is_err()
    );
    let snapshot = cognition.snapshot().unwrap();
    assert_eq!(snapshot.state.goals[&id].revision, version + 1);
    assert!(
        snapshot.state.goals[&id]
            .wait_reason
            .as_ref()
            .unwrap()
            .contains("最新出发时间是十点")
    );
    assert!(
        !serde_json::to_value(snapshot)
            .unwrap()
            .to_string()
            .contains("旧消息说九点")
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn stopping_service_during_review_does_not_create_goal() {
    let (kernel, commands, cognition) = commands().await;
    let dir = tempfile::tempdir().unwrap();
    let mut reviewer = fixture::reviewer();
    let accepting = commands.accepting.clone();
    Arc::get_mut(&mut reviewer).unwrap().during =
        Some(Box::new(move || accepting.store(false, Ordering::SeqCst)));
    let tool = tool(
        commands,
        Review {
            admin: fixture::admin(dir.path()),
            reviewer: Some(reviewer),
        },
    );
    let create = fixture::call(
        "manage_goal",
        "create",
        json!({"action":"create","text":"整理出行计划"}),
    );
    assert!(
        tool.execute(create.clone(), context(&create))
            .await
            .is_err()
    );
    assert!(cognition.snapshot().unwrap().state.goals.is_empty());
    kernel.stop_all().await.unwrap();
}
