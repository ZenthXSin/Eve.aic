use super::*;
use eve_llm_api::{ChatMessage, ToolCall, ToolOutput, ToolResult};
use eve_task_api::TaskCompletionAssessment;
use serde_json::{Value, json};

fn input() -> String {
    format!(
        "{IDLE_INPUT_PREFIX}{}",
        json!({
            "scope":{"channel":"qq","conversation_id":"private-owner","principal_id":"owner"},
            "revision":0,"interests":["工程验证"],
            "evidence":[{"id":"evidence-1","kind":"task_result","text":"已有结果","excerpt":false}],
            "judgments":[]
        })
    )
}

#[test]
fn frozen_contract_authority_is_not_idle_evidence_or_part_of_the_focus_limit() {
    let scope = SubjectScope {
        channel: "qq".into(),
        conversation_id: "private-owner".into(),
        principal_id: "owner".into(),
    };
    let mut foci: Vec<_> = (0..6)
        .map(|index| SubjectFocus {
            id: format!("experience-{index}"),
            scope: scope.clone(),
            title: "本范围经历".into(),
            goal: "普通经历正文".into(),
            task_id: None,
            status: FocusStatus::Completed,
            next_check_at_ms: 1,
            updated_at_ms: 1,
        })
        .collect();
    for index in 0..8 {
        foci.push(SubjectFocus {
            id: format!("contract-auto-1-{index}"),
            scope: scope.clone(),
            title: "冻结合同".into(),
            goal: json!({"review_messages":["完整授权正文只能经专用执行审查路径传递"]}).to_string(),
            task_id: None,
            status: FocusStatus::Blocked,
            next_check_at_ms: 1,
            updated_at_ms: 1,
        });
    }
    let observed = observation(
        &SubjectSnapshot {
            events: vec![],
            foci,
        },
        &[],
        &scope,
        &["工程验证".into()],
    )
    .unwrap();
    assert_eq!(observed.evidence.len(), 6);
    assert!(
        observed
            .evidence
            .iter()
            .all(|evidence| evidence.id.starts_with("experience-"))
    );
    assert!(
        !serde_json::to_string(&observed)
            .unwrap()
            .contains("完整授权正文")
    );
}

fn completed_task(id: &str, source: &str, at: u64) -> Task {
    serde_json::from_value(json!({
        "id":id,"source":source,
        "owner":{"session_id":"private-owner","user_id":"owner"},
        "spec":{"title":"保留的真实记录","instructions":"记录一次结果","max_steps":1},
        "created_at_ms":1,"status":"Completed","cancel_requested":false,
        "started_at_ms":1,"finished_at_ms":at,"usage":{"tool_calls":0,"model_requests":1},
        "result":"已保存的结果","failure":null,"notice":null
    }))
    .unwrap()
}

#[test]
fn repeated_reflection_cannot_displace_real_action_evidence() {
    let scope = SubjectScope {
        channel: "qq".into(),
        conversation_id: "private-owner".into(),
        principal_id: "owner".into(),
    };
    let mut tasks = vec![completed_task("actual-result", "subject-action:one", 2)];
    for n in 0..12 {
        tasks.push(completed_task(
            &format!("review-{n}"),
            &format!("{IDLE_TASK_SOURCE_PREFIX}{n}"),
            10 + n,
        ));
        tasks.push(completed_task(
            &format!("plan-{n}"),
            &format!("{}{n}", crate::qq_subject::PLAN_TASK_SOURCE_PREFIX),
            30 + n,
        ));
    }
    let mut foreign = completed_task("other-window", "subject-action:other", 50);
    foreign.owner.session_id = "other".into();
    tasks.push(foreign);
    let snapshot = SubjectSnapshot {
        events: vec![],
        foci: vec![],
    };
    let observed = observation(&snapshot, &tasks, &scope, &["自己探索".into()]).unwrap();
    assert_eq!(observed.evidence.len(), 2);
    assert_eq!(observed.evidence[0].id, "actual-result");
    assert_eq!(observed.evidence[0].kind, "task_result");
    assert_eq!(observed.evidence[1].kind, "self_reflection");
}

#[tokio::test]
async fn committed_mind_repairs_its_projection_once_and_keeps_original_deadline() {
    use eve_kernel::{Kernel, KernelServices};
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        eve_subject_sqlite::SqliteIdleMemory::open(&temp.path().join("interests.sqlite3")).unwrap(),
    );
    let input_text = input();
    let observed: Observation =
        serde_json::from_str(input_text.strip_prefix(IDLE_INPUT_PREFIX).unwrap()).unwrap();
    let output = json!({"choice":"reflect","thought":"继续考虑图案规则","reason":"实际经历带来新想法","judgments":[],"action":null,"revisit_after_minutes":30,
        "interest_updates":[{"key":"patterns","topic":"图案规则","phase":"curious","strength":60,"reason":"想了解变化","want_to_try":"比较不同规则","evidence":["evidence-1"]}]});
    let reflection: IdleReflection = serde_json::from_value(output.clone()).unwrap();
    let committed = store
        .commit(&observed, &reflection, "review-1", 100)
        .unwrap();
    let plugin = eve_subject_plugin::SubjectPlugin::new()
        .unwrap()
        .with_idle_memory(store.clone());
    let admin: Arc<dyn SubjectAdmin> = Arc::new(plugin.controller());
    let kernel = Kernel::with_services(KernelServices::default());
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    // 模拟数据库已提交、主体关注还没落盘时进程中断。
    assert!(admin.snapshot_for(&observed.scope).unwrap().foci.is_empty());
    let mut task = completed_task("review-1", &format!("{IDLE_TASK_SOURCE_PREFIX}one"), 100);
    task.result = Some(output.to_string());
    task.spec.context = Some(eve_task_api::DelegatedContext {
        owner: task.owner.clone(),
        messages_json: serde_json::to_string(&vec![ChatMessage::text(
            eve_llm_api::ChatRole::Assistant,
            input_text,
        )])
        .unwrap(),
    });
    settle(&admin, &observed.scope, &task, 200).unwrap();
    settle(&admin, &observed.scope, &task, 1000).unwrap();
    let restored = mind(&admin.snapshot_for(&observed.scope).unwrap()).unwrap();
    assert_eq!(restored.revision, 1);
    assert!(restored.archived);
    assert_eq!(restored.next_at_ms, committed.next_at_ms);
    assert_eq!(
        store
            .history(&observed.scope, "patterns", 0, 8)
            .unwrap()
            .items
            .len(),
        1
    );
    kernel.stop_all().await.unwrap();
}

fn reflection() -> Value {
    json!({
        "choice":"act","thought":"已有真实记录支持继续验证","reason":"验证最终产物",
        "judgments":[],"revisit_after_minutes":30,
        "action":{
            "title":"核对最终文件","goal":"更新并验证最终文件",
            "assessment":{
                "problem_key":"final-artifact","expected_gain":"得到可复核结果",
                "verification":"核对完整文件SHA256","reuse_plan":"保存验证步骤",
                "benefit":90,"confidence":90,"cost":10,"risk":0,"evidence":["evidence-1"]
            }
        }
    })
}

fn receipts() -> Value {
    json!({"kind":"receipts","contract":{"checks":[{
        "tool":"read_file","arguments":{"path":"artifact.txt"},
        "output_pointer":"/sha256","expected":"a".repeat(64)
    }],"require_final_batch":true}})
}

fn with_contract(contract: Value) -> Value {
    let mut result = reflection();
    result["action"]["completion_contract"] = contract;
    result
}

#[test]
fn legacy_idle_action_remains_a_proposal_awaiting_a_frozen_contract() {
    for output in [reflection(), with_contract(Value::Null)] {
        let decoded = decode(&input(), &output.to_string()).unwrap();
        let action = decoded.action.unwrap();
        assert!(action.completion_contract.is_none());
        assert!(action_completion_contract(&action).unwrap().is_none());
        assert!(
            serde_json::to_value(action)
                .unwrap()
                .get("completion_contract")
                .is_none(),
            "旧提议不能被自动改写为 Unspecified 执行合同"
        );
    }
}

#[test]
fn corrupt_self_interest_state_is_not_treated_as_an_empty_mind() {
    let scope = SubjectScope {
        channel: "qq".into(),
        conversation_id: "private-owner".into(),
        principal_id: "owner".into(),
    };
    let interest = json!({
        "current":{"key":"patterns","topic":"规则图案","phase":"curious","strength":60,
            "reason":"想了解规律","want_to_try":"比较两个小实验","evidence":["evidence-1"]},
        "revision":1,"first_recorded_at_ms":100,"updated_at_ms":100,
        "consumed_evidence":["evidence-1"]
    });
    let valid = json!({"revision":1,"last_task":"review-1","next_at_ms":200,
        "thought":"想了解规律","reason":"实际交流带来的好奇","judgments":[],
        "self_interests":[interest]})
    .to_string();
    let snapshot = |goal: String| SubjectSnapshot {
        events: vec![],
        foci: vec![SubjectFocus {
            id: MIND_ID.into(),
            scope: scope.clone(),
            title: "自己的兴趣".into(),
            goal,
            task_id: None,
            status: FocusStatus::Blocked,
            next_check_at_ms: 200,
            updated_at_ms: 100,
        }],
    };
    let observed =
        observation(&snapshot(valid.clone()), &[], &scope, &["探索方向".into()]).unwrap();
    assert_eq!(observed.self_interests[0].current.topic, "规则图案");
    assert!(observed.evidence.is_empty(), "自己的兴趣账本不是一次新经历");
    let cases = [
        valid.replace("\"phase\":\"curious\"", "\"phase\":\"unknown\""),
        valid.replace("\"strength\":60", "\"strength\":101"),
        valid.replace("\"revision\":1", "\"revision\":0"),
        valid.replace(
            "\"consumed_evidence\":[\"evidence-1\"]",
            "\"consumed_evidence\":[]",
        ),
        valid.replace("\"strength\":60", "\"strength\":100,\"strength\":60"),
    ];
    for damaged in cases {
        assert_ne!(damaged, valid);
        assert!(matches!(
            mind(&snapshot(damaged)),
            Err(SubjectError::CorruptState)
        ));
    }
}

#[test]
fn idle_action_accepts_only_strict_observation_contracts() {
    let output = with_contract(receipts());
    let decoded = decode(&input(), &output.to_string()).unwrap();
    let frozen = action_completion_contract(decoded.action.as_ref().unwrap())
        .unwrap()
        .unwrap();
    assert!(serde_json::to_value(&frozen).unwrap() == receipts());
    let mut engineering = receipts();
    engineering["contract"]["checks"][0] = json!({
        "tool":"run_command","arguments":{"program":"cargo","args":["test","--lib","--locked"],"cwd":"."},
        "output_pointer":"/exit_code","expected":0
    });
    assert!(decode(&input(), &with_contract(engineering).to_string()).is_ok());

    let mut cases = vec![
        Value::String("receipts".into()),
        json!({"kind":"unspecified"}),
        json!({"kind":"text","valid_json":true}),
        json!({"kind":"receipts","contract":{"checks":[],"require_final_batch":true}}),
    ];
    let mut contract = receipts();
    contract["approved"] = json!(true);
    cases.push(contract);
    let mut contract = receipts();
    contract["contract"]["checks"][0] = json!({
        "tool":"run_command","arguments":{"program":"sh","args":["-c","exit 0"],"cwd":"."},
        "output_pointer":"/exit_code","expected":0
    });
    cases.push(contract);
    let mut contract = receipts();
    contract["contract"]["checks"][0]["authorized"] = json!(true);
    cases.push(contract);
    let mut contract = receipts();
    contract["contract"]["require_final_batch"] = json!(false);
    cases.push(contract);
    let mut contract = receipts();
    contract["contract"]["checks"][0] = json!({
        "tool":"run_command","arguments":{"program":"printf","args":["success"]},
        "output_pointer":"/stdout","expected":"success"
    });
    cases.push(contract);
    let mut contract = receipts();
    contract["contract"]["checks"][0]["arguments"]["path"] = json!("../foreign.txt");
    cases.push(contract);
    for contract in cases {
        assert!(
            decode(&input(), &with_contract(contract.clone()).to_string()).is_err(),
            "行动不能接受无效或自证合同：{contract}"
        );
    }

    let mut output = output;
    output["action"]["authorized"] = json!(true);
    assert!(decode(&input(), &output.to_string()).is_err());
}

#[test]
fn duplicate_contract_keys_and_trailing_json_are_rejected_before_value_conversion() {
    let valid = with_contract(receipts()).to_string();
    for (key, repeated) in [
        (
            "\"kind\":\"receipts\"",
            "\"kind\":\"text\",\"kind\":\"receipts\"",
        ),
        (
            "\"require_final_batch\":true",
            "\"require_final_batch\":false,\"require_final_batch\":true",
        ),
        (
            "\"path\":\"artifact.txt\"",
            "\"path\":\"../foreign.txt\",\"path\":\"artifact.txt\"",
        ),
    ] {
        assert!(valid.contains(key));
        assert!(decode(&input(), &valid.replace(key, repeated)).is_err());
    }
    assert!(decode(&input(), &format!("{valid} {{}}")).is_err());
}

fn read_call(id: &str, path: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "read_file".into(),
        arguments: json!({"path":path}),
    }
}

fn batch(calls: Vec<ToolCall>, sha256: &str) -> Vec<ChatMessage> {
    vec![
        ChatMessage::assistant_tool_calls(calls.clone()).unwrap(),
        ChatMessage::tool_results(
            calls
                .into_iter()
                .map(|call| ToolResult {
                    call_id: call.id,
                    output: ToolOutput::Success(json!({"sha256":sha256})),
                })
                .collect(),
        )
        .unwrap(),
    ]
}

#[test]
fn frozen_idle_action_requires_every_check_in_the_final_observation_batch() {
    let mut contract = receipts();
    let mut second = contract["contract"]["checks"][0].clone();
    second["arguments"]["path"] = json!("second.txt");
    contract["contract"]["checks"]
        .as_array_mut()
        .unwrap()
        .push(second);
    let decoded = decode(&input(), &with_contract(contract).to_string()).unwrap();
    let frozen = action_completion_contract(decoded.action.as_ref().unwrap())
        .unwrap()
        .unwrap();
    let hash = "a".repeat(64);
    let assess = |messages: &[ChatMessage]| {
        eve_task_plugin::evaluate_completion_contract(&frozen, messages, "已核对最终文件", None)
    };
    let calls = vec![
        read_call("first", "artifact.txt"),
        read_call("second", "second.txt"),
    ];
    let verified = batch(calls.clone(), &hash);
    assert_eq!(assess(&verified), TaskCompletionAssessment::Verified);
    assert!(!assess(&batch(vec![calls[0].clone()], &hash)).verified());
    assert!(!assess(&batch(calls.clone(), &"b".repeat(64))).verified());
    let mut later = verified;
    later.extend(batch(vec![read_call("later", "artifact.txt")], &hash));
    assert!(!assess(&later).verified(), "较早批次不满足最终产物验收");
    let mut extra = calls;
    extra.push(ToolCall {
        id: "write".into(),
        name: "write_file".into(),
        arguments: json!({"path":"artifact.txt","content":"changed","expected_sha256":hash}),
    });
    assert!(
        !assess(&batch(extra, &hash)).verified(),
        "末批不能混入额外修改动作"
    );
}
