use super::*;
use eve_developer_api::DeveloperConfig;
use eve_developer_tools::LocalDeveloper;
use eve_llm_api::{ChatMessage, ChatRole, ContextScope, SharedToolSteps};
use eve_task_api::{validate_workspace_observation, workspace_node_checkpoint_sha256};
use std::{fs, os::unix::fs::PermissionsExt};

fn owner() -> TaskOwner {
    TaskOwner {
        session_id: "workspace-test-session".into(),
        user_id: "workspace-test-owner".into(),
    }
}
fn scope() -> ContextScope {
    ContextScope {
        session_id: owner().session_id,
        user_id: owner().user_id,
    }
}
fn session() -> ModelSession {
    ModelSession::scoped("coding", &scope())
        .unwrap()
        .with_turn("workspace-original-turn")
        .unwrap()
}
fn context(id: &str, pool: &SharedToolSteps) -> ToolExecutionContext {
    ToolExecutionContext::new()
        .with_scope(Some(scope()))
        .with_model_session(session())
        .with_call_id(id.into())
        .with_shared_steps(pool.clone())
}
fn call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: WORKSPACE_NODE_TEST_TOOL.into(),
        arguments: json!({"directory":"project","entry":"entry.cjs"}),
    }
}
fn make_frame(call: ToolCall, output: Value) -> TurnCheckpoint {
    let id = call.id.clone();
    TurnCheckpoint {
        version: 1,
        scope: scope(),
        model_session: Some(session()),
        pending_model_request: None,
        pending_request_fingerprint: None,
        messages: vec![
            ChatMessage::text(ChatRole::User, "验证工作区中的小型 JS 测试"),
            ChatMessage::assistant_tool_calls(vec![call]).unwrap(),
            ChatMessage::tool_results(vec![ToolResult::success(id, output).unwrap()]).unwrap(),
            ChatMessage::text(ChatRole::Assistant, "已执行，引用原运行回执。"),
        ],
        pending_calls: vec![],
        used_tool_calls: 1,
        delegated_tool_calls: 0,
        delegated_model_requests: 0,
        provider_requests: 1,
        tool_batches: 1,
        remaining: None,
        continuation_checks: 0,
        closing: false,
        result: Some("已执行，引用原运行回执。".into()),
    }
}

#[test]
fn selector_defaults_and_identity_do_not_trust_tool_json() {
    let request = WorkspaceNodeRequest::parse(&json!({"entry":"entry.cjs"})).unwrap();
    assert_eq!(request.directory, ".");
    assert!(request.args.is_empty());
    for arguments in [
        json!({"entry":"entry.cjs","owner":"other"}),
        json!({"entry":"entry.cjs","directory":"../other"}),
        json!({"entry":"entry.cjs","directory":"/tmp"}),
        json!({"entry":".git/entry.cjs"}),
        json!({"entry":"entry.cjs","args":null}),
        json!({"entry":"entry.cjs","directory":null}),
    ] {
        assert!(
            WorkspaceNodeRequest::parse(&arguments).is_err(),
            "{arguments}"
        );
    }
    let snapshot = |workspace: &str, root: &str, user: &str| WorkspaceSnapshot {
        scope: ContextScope {
            session_id: "same".into(),
            user_id: user.into(),
        },
        workspace_root: workspace.into(),
        root: root.into(),
        entries: vec![],
        excluded: vec![],
        total_bytes: 0,
        sha256: "0".repeat(64),
    };
    let original =
        workspace_identity(&snapshot("/workspace", "/workspace/project", "one")).unwrap();
    assert_ne!(
        original,
        workspace_identity(&snapshot("/workspace", "/workspace/project", "two")).unwrap()
    );
    assert_ne!(
        original,
        workspace_identity(&snapshot("/workspace", "/workspace/elsewhere", "one")).unwrap()
    );
    assert!(workspace_identity(&snapshot("/workspace", "/another", "one")).is_err());
}

#[tokio::test]
async fn real_workspace_runner_reuses_witness_and_observes_changes_without_rerunning() {
    let runner = super::super::frozen_node::tests::real_runner();
    let root = tempfile::tempdir().unwrap();
    let journal = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    fs::create_dir(&project).unwrap();
    fs::create_dir(project.join("empty")).unwrap();
    fs::set_permissions(project.join("empty"), fs::Permissions::from_mode(0o755)).unwrap();
    let config: DeveloperConfig =
        serde_json::from_value(json!({"workspace":root.path(),"operators":[owner().user_id]}))
            .unwrap();
    let backend =
        Arc::new(LocalDeveloper::new(config, journal.path().to_path_buf(), None).unwrap());
    let adapter = WorkspaceNodeAdapter::new(runner, backend);
    let tool = WorkspaceNodeTool(adapter.clone());
    let pool = SharedToolSteps::new(20).unwrap();
    pool.reserve(4).unwrap();
    pool.record_model_request().unwrap();
    let charged = pool.usage().unwrap();
    // 合同在实现之前就可以形成；入口尚未存在，不虚构预注册测试字节。
    let contract = adapter
        .prepare_contract("project", "entry.cjs", &[], context("prepare", &pool))
        .await
        .unwrap();
    assert!(!project.join("entry.cjs").exists());
    fs::write(project.join("entry.cjs"), b"const fs=require('node:fs'), assert=require('node:assert/strict'); assert.equal(fs.statSync('empty').isDirectory(),true); assert.equal(fs.statSync('empty').mode & 73,73); assert.equal(fs.statSync('entry.cjs').mode & 73,8); console.log('workspace assertions passed');").unwrap();
    fs::set_permissions(project.join("entry.cjs"), fs::Permissions::from_mode(0o654)).unwrap();
    let output = tool
        .execute(
            call("actual-test"),
            context("actual-test", &pool).with_model_session(session().for_request(2)),
        )
        .await
        .unwrap();
    assert_eq!(output["receipt"]["outcome"]["status"], "exited");
    assert_eq!(output["receipt"]["outcome"]["exit_code"], 0);
    assert!(
        output["receipt"]["stdout"]
            .as_str()
            .unwrap()
            .contains("workspace assertions passed")
    );
    assert_eq!(pool.usage().unwrap(), charged);
    let mut frame = make_frame(call("actual-test"), output);
    // 实际 QQ 子代理的执行会话继承父 request 1，测试工具来自子代理 request 2。
    frame.model_session = Some(session().for_request(1));
    let evidence = adapter.capture(&owner(), &frame).unwrap();
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].model_session, session().for_request(1));
    {
        // 与实际 LlmHost 一样：工具获得 request session，检查点保留 turn session。
        let ledger = adapter.ledger.lock().unwrap();
        let saved = ledger.receipts.values().next().unwrap();
        assert_eq!(saved.evidence.model_session, session().for_request(2));
        assert_eq!(
            saved.receipt.execution_id(),
            execution_id(&session().for_request(2))
        );
    }
    // 子会话的初始会话可能继承父请求序号；只改变序号仍能映射，不丢弃 turn/fork。
    for ordinal in [0, 7] {
        let mut inherited_ordinal = frame.clone();
        inherited_ordinal.model_session = Some(session().for_request(ordinal));
        let captured = adapter.capture(&owner(), &inherited_ordinal).unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].model_session, session().for_request(ordinal));
    }
    for other in [
        session().with_turn("different-turn").unwrap(),
        session().fork("subagent", Some("different-child")).unwrap(),
    ] {
        let mut foreign = frame.clone();
        foreign.model_session = Some(other);
        assert!(adapter.capture(&owner(), &foreign).unwrap().is_empty());
    }
    let mut wrong_owner = owner();
    wrong_owner.user_id = "different-owner".into();
    assert!(adapter.capture(&wrong_owner, &frame).is_err());
    let observed_owner = owner();
    let observed_session = session().for_request(1);
    let checkpoint_digest = workspace_node_checkpoint_sha256(&frame);
    let observe = || {
        adapter.observe_current(
            &contract,
            &observed_owner,
            &observed_session,
            &checkpoint_digest,
            context("observe", &pool).with_model_session(observed_session.clone()),
        )
    };
    // 下面各次 current 只读重新采集；相同 receipt 保持同一 call_id，不再执行测试。
    let current = observe().await.unwrap();
    validate_workspace_observation(&owner(), &frame, &current).unwrap();
    assert_eq!(current.input_sha256, evidence[0].input_sha256);
    fs::write(
        root.path().join("unrelated.txt"),
        "outside selected directory",
    )
    .unwrap();
    let after_unrelated = observe().await.unwrap();
    assert_eq!(after_unrelated.input_sha256, current.input_sha256);
    // 0755 -> 0751 只移除 other-read，执行位仍是 0111；这类位不属于冻结输入。
    fs::set_permissions(project.join("empty"), fs::Permissions::from_mode(0o751)).unwrap();
    assert_eq!(
        fs::metadata(project.join("empty"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0o111
    );
    assert_eq!(observe().await.unwrap().input_sha256, current.input_sha256);
    // 明确移除 other-execute，验证执行位进入快照和真实运行 bundle 的摘要。
    fs::set_permissions(project.join("empty"), fs::Permissions::from_mode(0o754)).unwrap();
    assert_eq!(
        fs::metadata(project.join("empty"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0o110
    );
    assert_ne!(observe().await.unwrap().input_sha256, current.input_sha256);
    fs::set_permissions(project.join("empty"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(project.join("dependency.cjs"), "module.exports=2;").unwrap();
    assert_ne!(observe().await.unwrap().input_sha256, current.input_sha256);
    fs::remove_file(project.join("dependency.cjs")).unwrap();
    fs::write(project.join("entry.cjs"), "process.exit(7);").unwrap();
    assert_ne!(observe().await.unwrap().input_sha256, current.input_sha256);
    assert_eq!(adapter.capture(&owner(), &frame).unwrap(), evidence);
    assert_eq!(adapter.ledger.lock().unwrap().receipts.len(), 1);
    assert_eq!(pool.usage().unwrap(), charged);
    assert!(
        tool.execute(
            call("actual-test"),
            context("actual-test", &pool).with_model_session(session().for_request(3)),
        )
        .await
        .is_err()
    );
    let failed = tool
        .execute(
            call("second-explicit-test"),
            context("second-explicit-test", &pool),
        )
        .await
        .unwrap();
    assert_eq!(failed["receipt"]["outcome"]["exit_code"], 7);
    assert_eq!(adapter.ledger.lock().unwrap().receipts.len(), 2);
    let cancelled = context("cancelled", &pool);
    cancelled.cancellation_handle().cancel();
    assert!(tool.execute(call("cancelled"), cancelled).await.is_err());
    let foreign_scope = ContextScope {
        session_id: scope().session_id,
        user_id: "unauthorized".into(),
    };
    let foreign = context("unauthorized", &pool)
        .with_scope(Some(foreign_scope.clone()))
        .with_model_session(
            ModelSession::scoped("coding", &foreign_scope)
                .unwrap()
                .with_turn("workspace-original-turn")
                .unwrap(),
        );
    assert!(tool.execute(call("unauthorized"), foreign).await.is_err());
    let mut altered = frame.clone();
    altered.messages[2] = ChatMessage::tool_results(vec![
        ToolResult::success(
            "actual-test",
            json!({"receipt":{"outcome":{"status":"exited","exit_code":0}}}),
        )
        .unwrap(),
    ])
    .unwrap();
    assert!(adapter.capture(&owner(), &altered).is_err());
    let forged = make_frame(
        call("never-executed"),
        json!({"receipt":{"outcome":{"status":"exited","exit_code":0}}}),
    );
    assert!(adapter.capture(&owner(), &forged).unwrap().is_empty());
}
