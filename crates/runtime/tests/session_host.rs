#[path = "support/session.rs"]
mod support;
use eve_kernel::backends::MemoryStateStore;
use eve_llm_api::*;
use eve_plugin_api::*;
use eve_runtime::{LlmHostConfig, SessionBinding, SessionRunError};
use eve_session_api::*;
use eve_session_plugin::SessionPlugin;
use serde_json::json;
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use support::*;
use tokio::sync::Notify;

struct ProbeTool {
    concurrency: Option<ToolConcurrency>,
    starts: std::sync::atomic::AtomicUsize,
    started: Notify,
    release_first: Notify,
}
impl Tool for ProbeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            concurrency: self.concurrency.clone(),
            ..ReceiptTool {
                starts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
            .definition()
        }
    }
    fn validate_arguments(&self, _: &serde_json::Value) -> Result<(), ToolValidationError> {
        Ok(())
    }
    fn execute(&self, _: ToolCall, _: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            let ordinal = self.starts.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            if ordinal == 0 {
                self.release_first.notified().await;
            }
            Ok(json!({"ordinal":ordinal}))
        })
    }
}
async fn install_probe(rig: &Rig, concurrency: Option<ToolConcurrency>) -> Arc<ProbeTool> {
    let tool = Arc::new(ProbeTool {
        concurrency,
        starts: std::sync::atomic::AtomicUsize::new(0),
        started: Notify::new(),
        release_first: Notify::new(),
    });
    rig.kernel.stop(&id(OWNER)).await.unwrap();
    rig.kernel.unregister(&id(OWNER)).unwrap();
    rig.kernel
        .register(Box::new(ServicesPlugin {
            manifest: PluginManifest::new(OWNER, "0.1.0").unwrap(),
            context: rig.context.clone(),
            tool: tool.clone(),
        }))
        .unwrap();
    rig.kernel.start(&id(OWNER)).await.unwrap();
    tool
}
fn single_call() -> Result<ModelResponse, LlmError> {
    let mut response = calls().unwrap();
    if let ModelResponse::ToolCalls { calls } = &mut response {
        calls.truncate(1);
    }
    Ok(response)
}

#[tokio::test]
async fn serial_scope_and_concurrency_limit_apply_across_sessions_on_same_host() {
    for (concurrency, limit, parallel) in [
        (None, 10, false),
        (
            Some(ToolConcurrency::Serial {
                scope: "shared".into(),
            }),
            10,
            false,
        ),
        (Some(ToolConcurrency::ParallelSafe), 1, false),
        (Some(ToolConcurrency::ParallelSafe), 10, true),
    ] {
        let provider = Provider::new(vec![
            Step::new(single_call()),
            Step::new(single_call()),
            Step::new(final_response("完成")),
            Step::new(final_response("完成")),
        ]);
        let rig = Rig::new(
            provider.clone(),
            Arc::new(MemoryStateStore::default()),
            LlmHostConfig {
                max_parallel_tool_calls: limit,
                ..LlmHostConfig::default()
            },
        )
        .await;
        let tool = install_probe(&rig, concurrency).await;
        let host = rig.host.clone();
        let first = tokio::spawn(async move { host.run_turn(input("s1", "第一会话")).await });
        tokio::time::timeout(Duration::from_secs(3), tool.started.notified())
            .await
            .unwrap();
        let host = rig.host.clone();
        let second = tokio::spawn(async move { host.run_turn(input("s2", "第二会话")).await });
        provider.wait_requests(2).await;
        if parallel {
            tokio::time::timeout(Duration::from_secs(3), tool.started.notified())
                .await
                .unwrap();
            assert_eq!(tool.starts.load(Ordering::SeqCst), 2);
        } else {
            assert!(
                tokio::time::timeout(Duration::from_millis(30), tool.started.notified())
                    .await
                    .is_err()
            );
            assert_eq!(tool.starts.load(Ordering::SeqCst), 1);
        }
        tool.release_first.notify_one();
        tokio::time::timeout(Duration::from_secs(3), first)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), second)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(tool.starts.load(Ordering::SeqCst), 2);
        rig.stop().await;
    }
}

#[tokio::test]
async fn cancelling_middle_serial_waiter_does_not_bypass_active_tool_or_stall_successor() {
    let provider = Provider::new(vec![
        Step::new(single_call()),
        Step::new(single_call()),
        Step::new(single_call()),
        Step::new(final_response("完成")),
        Step::new(final_response("完成")),
    ]);
    let rig = Rig::new(
        provider.clone(),
        Arc::new(MemoryStateStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let tool = install_probe(&rig, None).await;
    let host = rig.host.clone();
    let first = tokio::spawn(async move { host.run_turn(input("s1", "第一")).await });
    tokio::time::timeout(Duration::from_secs(3), tool.started.notified())
        .await
        .unwrap();
    let host = rig.host.clone();
    let middle = tokio::spawn(async move { host.run_turn(input("s2", "取消排队")).await });
    provider.wait_requests(2).await;
    let host = rig.host.clone();
    let last = tokio::spawn(async move { host.run_turn(input("s3", "第三")).await });
    provider.wait_requests(3).await;
    middle.abort();
    assert!(middle.await.unwrap_err().is_cancelled());
    assert!(
        tokio::time::timeout(Duration::from_millis(30), tool.started.notified())
            .await
            .is_err()
    );
    assert_eq!(tool.starts.load(Ordering::SeqCst), 1);
    tool.release_first.notify_one();
    tokio::time::timeout(Duration::from_secs(3), first)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), last)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(tool.starts.load(Ordering::SeqCst), 2);
    assert!(rig.snapshot("s2").history().is_empty());
    rig.stop().await;
}

struct BlockingDropTool {
    started: Arc<Notify>,
    drop_store: Arc<FaultStore>,
}
struct BlockingDropFuture {
    started: Arc<Notify>,
    drop_store: Arc<FaultStore>,
}
impl std::future::Future for BlockingDropFuture {
    type Output = Result<serde_json::Value, ToolExecutionError>;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.started.notify_one();
        std::task::Poll::Pending
    }
}
impl Drop for BlockingDropFuture {
    fn drop(&mut self) {
        self.drop_store.pause_next.store(true, Ordering::SeqCst);
        let _ = self
            .drop_store
            .set(&id("tool-drop"), "dropped".into(), vec![]);
    }
}
impl Tool for BlockingDropTool {
    fn definition(&self) -> ToolDefinition {
        ReceiptTool {
            starts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
        .definition()
    }
    fn validate_arguments(&self, _: &serde_json::Value) -> Result<(), ToolValidationError> {
        Ok(())
    }
    fn execute(&self, _: ToolCall, _: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(BlockingDropFuture {
            started: self.started.clone(),
            drop_store: self.drop_store.clone(),
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn cancelling_tool_turn_keeps_lifecycle_admission_until_tool_future_is_dropped() {
    let mut response = calls().unwrap();
    if let ModelResponse::ToolCalls { calls } = &mut response {
        calls.truncate(1);
    }
    let provider = Provider::new(vec![Step::new(Ok(response))]);
    let rig = Rig::new(
        provider.clone(),
        Arc::new(MemoryStateStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let started = Arc::new(Notify::new());
    let drop_store = Arc::new(FaultStore::default());
    rig.kernel.stop(&id(OWNER)).await.unwrap();
    rig.kernel.unregister(&id(OWNER)).unwrap();
    rig.kernel
        .register(Box::new(ServicesPlugin {
            manifest: PluginManifest::new(OWNER, "0.1.0").unwrap(),
            context: rig.context.clone(),
            tool: Arc::new(BlockingDropTool {
                started: started.clone(),
                drop_store: drop_store.clone(),
            }),
        }))
        .unwrap();
    rig.kernel.start(&id(OWNER)).await.unwrap();
    let host = rig.host.clone();
    let running = tokio::spawn(async move { host.run_turn(input("s", "工具在途")).await });
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(3), drop_store.entered.notified())
        .await
        .unwrap();
    let kernel = rig.kernel.clone();
    let mut stopping = tokio::spawn(async move { kernel.stop_all().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut stopping)
            .await
            .is_err()
    );
    drop_store.release();
    tokio::time::timeout(Duration::from_secs(3), stopping)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn two_turns_replay_complete_tool_history_without_executing_old_calls() {
    let provider = Provider::new(vec![
        Step::new(calls()),
        Step::new(final_response("已处理")),
        Step::new(final_response("继续完成")),
    ]);
    let rig = Rig::new(
        provider.clone(),
        Arc::new(MemoryStateStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let first = rig.host.run_turn(input("s", "第一轮")).await.unwrap();
    assert_eq!(first.turn_id, 1);
    assert_eq!(first.output.transcript.len(), 4);
    let second = rig.host.run_turn(input("s", "第二轮")).await.unwrap();
    assert_eq!(second.turn_id, 2);
    assert_eq!(
        second.output.transcript,
        vec![
            ChatMessage::text(ChatRole::User, "第二轮"),
            ChatMessage::text(ChatRole::Assistant, "继续完成")
        ]
    );
    assert_eq!(rig.starts.load(Ordering::SeqCst), 1);
    let snapshot = rig.snapshot("s");
    assert_eq!(snapshot.revision, 4);
    assert_eq!(
        snapshot.history(),
        [first.output.transcript.clone(), second.output.transcript].concat()
    );
    let requests = provider.requests.lock().unwrap().clone();
    let prefix_len = requests[0].messages.len() - 1;
    assert_eq!(
        &requests[0].messages[..prefix_len],
        &requests[2].messages[..prefix_len]
    );
    assert_eq!(requests[0].tools, requests[2].tools);
    assert_eq!(
        &requests[2].messages[prefix_len..prefix_len + 4],
        first.output.transcript
    );
    assert_eq!(
        requests[2].messages[prefix_len + 1].tool_calls[0].id,
        "call-b"
    );
    assert_eq!(
        requests[2].messages[prefix_len + 2].tool_results[1].call_id,
        "call-a"
    );
    assert!(matches!(
        requests[2].messages[prefix_len + 2].tool_results[1].output,
        ToolOutput::Failure {
            code: ToolFailureCode::InvalidArguments,
            ..
        }
    ));
    rig.stop().await;
}

#[tokio::test]
async fn same_session_is_busy_different_sessions_are_parallel_and_history_is_isolated() {
    let gate = Arc::new(Notify::new());
    let provider = Provider::new(vec![
        Step::blocked(gate.clone()),
        Step::blocked(gate.clone()),
    ]);
    let rig = Rig::new(
        provider.clone(),
        Arc::new(MemoryStateStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    let h = rig.host.clone();
    let first = tokio::spawn(async move { h.run_turn(input("s1", "秘密一")).await });
    provider.wait_requests(1).await;
    assert_eq!(
        rig.host.run_turn(input("s1", "并发")).await.unwrap_err(),
        SessionRunError::Session(SessionError::Busy)
    );
    let h = rig.host.clone();
    let second = tokio::spawn(async move { h.run_turn(input("s2", "秘密二")).await });
    provider.wait_requests(2).await; // 第一轮尚未释放，第二会话必须已请求 Provider。
    gate.notify_waiters();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(rig.snapshot("s1").turns.len(), 1);
    assert_eq!(rig.snapshot("s2").turns.len(), 1);
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(
        requests[0].messages.last().unwrap().text.as_deref(),
        Some("秘密一")
    );
    assert_eq!(
        requests[1].messages.last().unwrap().text.as_deref(),
        Some("秘密二")
    );
    rig.stop().await;
}

#[tokio::test]
async fn owner_empty_input_and_unavailable_services_do_not_request_provider() {
    let provider = Provider::new(vec![Step::new(final_response("完成"))]);
    let rig = Rig::new(
        provider.clone(),
        Arc::new(MemoryStateStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    rig.host.run_turn(input("s", "第一轮")).await.unwrap();
    let mut foreign = input("s", "他人问题");
    foreign.key.user_id = "他人".into();
    assert_eq!(
        rig.host.run_turn(foreign).await.unwrap_err(),
        SessionRunError::Session(SessionError::OwnerMismatch)
    );
    assert_eq!(
        rig.host.run_turn(input("empty", "  ")).await.unwrap_err(),
        SessionRunError::Session(SessionError::InvalidInput)
    );
    for (binding, expected) in [
        (
            SessionBinding {
                expected_owner: id(OWNER),
                ..SessionBinding::builtin()
            },
            SessionError::OwnerMismatch,
        ),
        (
            SessionBinding {
                service_id: ServiceId::new("missing").unwrap(),
                ..SessionBinding::builtin()
            },
            SessionError::Unavailable,
        ),
        (
            SessionBinding {
                service_id: ServiceId::new(CONTEXT).unwrap(),
                expected_owner: id(OWNER),
            },
            SessionError::Unavailable,
        ),
    ] {
        let host = Rig::make_host(
            &rig.kernel,
            &rig.registry,
            &rig.permissions,
            &rig.logger,
            provider.clone(),
            LlmHostConfig::default(),
            binding,
        );
        assert_eq!(
            host.run_turn(input("s", "问题")).await.unwrap_err(),
            SessionRunError::Session(expected)
        );
    }
    rig.kernel.stop(&id(SESSION_PLUGIN_ID)).await.unwrap();
    assert_eq!(
        rig.host.run_turn(input("s", "停止后")).await.unwrap_err(),
        SessionRunError::Session(SessionError::Unavailable)
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    rig.stop().await;
}

#[tokio::test]
async fn context_history_conflict_is_recorded_before_network() {
    let provider = Provider::new(vec![]);
    let rig = Rig::new(
        provider.clone(),
        Arc::new(MemoryStateStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    *rig.context.history.lock().unwrap() = vec![ChatMessage::text(ChatRole::User, "重复历史")];
    let failure = rig.host.run_turn(input("s", "问题")).await.unwrap_err();
    assert!(matches!(failure, SessionRunError::Turn(_)));
    assert_eq!(
        rig.snapshot("s").turns[0].status,
        SessionTurnStatus::Failed {
            failure: SessionFailure {
                code: SessionFailureCode::Context,
                started_tools: Some(0)
            }
        }
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    rig.stop().await;
}

#[tokio::test]
async fn provider_protocol_unsupported_and_round_limit_failures_are_not_replayed() {
    for (responses, code, started) in [
        (
            vec![Err(LlmError::Provider("后端错误".into()))],
            SessionFailureCode::Provider,
            0,
        ),
        (
            vec![Err(LlmError::Unsupported("reasoning".into()))],
            SessionFailureCode::Unsupported,
            0,
        ),
        (
            vec![Ok(ModelResponse::Final { text: " ".into() })],
            SessionFailureCode::Protocol,
            0,
        ),
        (vec![calls(), calls()], SessionFailureCode::RoundLimit, 1),
        (
            vec![calls(), Err(LlmError::Provider("执行后失败".into()))],
            SessionFailureCode::Provider,
            1,
        ),
    ] {
        let mut steps: Vec<_> = responses.into_iter().map(Step::new).collect();
        steps.push(Step::new(final_response("明确的新请求")));
        let provider = Provider::new(steps);
        let rig = Rig::new(
            provider.clone(),
            Arc::new(MemoryStateStore::default()),
            LlmHostConfig::default(),
        )
        .await;
        assert!(matches!(
            rig.host.run_turn(input("s", "失败输入")).await,
            Err(SessionRunError::Turn(_))
        ));
        assert_eq!(
            rig.snapshot("s").turns[0].status,
            SessionTurnStatus::Failed {
                failure: SessionFailure {
                    code,
                    started_tools: Some(started)
                }
            }
        );
        assert!(rig.snapshot("s").history().is_empty());
        rig.host.run_turn(input("s", "明确新输入")).await.unwrap();
        assert_eq!(
            provider
                .requests
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .messages
                .last()
                .unwrap()
                .text
                .as_deref(),
            Some("明确新输入")
        );
        assert!(
            !provider
                .requests
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .messages
                .iter()
                .any(|m| m.text.as_deref() == Some("失败输入"))
        );
        assert_eq!(rig.starts.load(Ordering::SeqCst), started as usize);
        rig.stop().await;
    }
}

#[tokio::test]
async fn tool_failure_timeout_and_cancel_are_retained_in_completed_transcript() {
    for (text, expected) in [
        ("执行失败", ToolFailureCode::ExecutionFailed),
        ("超时", ToolFailureCode::TimedOut),
        ("取消", ToolFailureCode::Cancelled),
    ] {
        let mut response = calls().unwrap();
        if let ModelResponse::ToolCalls { calls } = &mut response {
            calls[0].arguments = json!({"text":text});
        }
        let provider = Provider::new(vec![
            Step::new(Ok(response)),
            Step::new(final_response("已说明工具失败")),
        ]);
        let rig = Rig::new(
            provider,
            Arc::new(MemoryStateStore::default()),
            LlmHostConfig {
                tool_timeout: Duration::from_millis(30),
                tool_cancellation_grace: Duration::from_millis(5),
                ..LlmHostConfig::default()
            },
        )
        .await;
        rig.host.run_turn(input("s", "问题")).await.unwrap();
        match &rig.snapshot("s").history()[2].tool_results[0].output {
            ToolOutput::Failure { code, .. } => assert_eq!(*code, expected),
            _ => panic!("工具失败必须保留"),
        }
        rig.stop().await;
    }
}

#[tokio::test]
async fn provider_timeout_records_failure_and_releases_admission() {
    let provider = Provider::new(vec![Step::blocked(Arc::new(Notify::new()))]);
    let rig = Rig::new(
        provider,
        Arc::new(MemoryStateStore::default()),
        LlmHostConfig {
            provider_timeout: Duration::from_millis(30),
            ..LlmHostConfig::default()
        },
    )
    .await;
    assert!(matches!(
        rig.host.run_turn(input("s", "问题")).await,
        Err(SessionRunError::Turn(_))
    ));
    assert_eq!(
        rig.snapshot("s").turns[0].status,
        SessionTurnStatus::Failed {
            failure: SessionFailure {
                code: SessionFailureCode::ProviderTimeout,
                started_tools: Some(0)
            }
        }
    );
    rig.stop().await;
}

#[tokio::test]
async fn begin_save_failure_never_requests_provider() {
    let store = Arc::new(FaultStore::default());
    store.fail.store(true, Ordering::SeqCst);
    let provider = Provider::new(vec![]);
    let rig = Rig::new(provider.clone(), store.clone(), LlmHostConfig::default()).await;
    assert_eq!(
        rig.host.run_turn(input("s", "问题")).await.unwrap_err(),
        SessionRunError::Session(SessionError::Storage)
    );
    assert!(rig.service().snapshot(&key("s")).unwrap().is_none());
    assert!(store.bytes().is_none());
    assert!(provider.requests.lock().unwrap().is_empty());
    rig.stop().await;
}

#[tokio::test]
async fn final_save_failure_preserves_reply_and_pending_without_retrying_tools() {
    let store = Arc::new(FaultStore::default());
    let provider = Provider::new(vec![
        Step::new(calls()),
        Step {
            fail_commit: Some(store.clone()),
            ..Step::new(final_response("已生成回执回复"))
        },
    ]);
    let rig = Rig::new(provider.clone(), store.clone(), LlmHostConfig::default()).await;
    let SessionRunError::Commit { error, output } =
        rig.host.run_turn(input("s", "问题")).await.unwrap_err()
    else {
        panic!("需要保留成功输出")
    };
    assert_eq!(error, SessionError::Storage);
    assert_eq!(output.text, "已生成回执回复");
    assert_eq!(output.diagnostics.started_tools, 1);
    assert_eq!(
        rig.snapshot("s").turns[0].status,
        SessionTurnStatus::Pending
    );
    assert_eq!(
        rig.host.run_turn(input("s", "自动重试")).await.unwrap_err(),
        SessionRunError::Session(SessionError::Busy)
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert_eq!(rig.starts.load(Ordering::SeqCst), 1);
    store.fail.store(false, Ordering::SeqCst);
    // 调用方明确只重试本地提交，不再调用模型或工具。
    rig.service()
        .complete(
            &TurnLease {
                key: key("s"),
                turn_id: 1,
            },
            output.transcript,
        )
        .unwrap();
    assert_eq!(rig.snapshot("s").history().len(), 4);
    rig.stop().await;
}

#[tokio::test]
async fn failed_failure_record_keeps_original_failure_and_pending() {
    let store = Arc::new(FaultStore::default());
    let provider = Provider::new(vec![Step {
        fail_commit: Some(store.clone()),
        ..Step::new(Err(LlmError::Provider("可分类错误".into())))
    }]);
    let rig = Rig::new(provider, store.clone(), LlmHostConfig::default()).await;
    let SessionRunError::FailureRecord { error, failure } =
        rig.host.run_turn(input("s", "问题")).await.unwrap_err()
    else {
        panic!("应保留两种错误")
    };
    assert_eq!(error, SessionError::Storage);
    assert!(matches!(failure.error, LlmError::Provider(_)));
    assert_eq!(
        rig.snapshot("s").turns[0].status,
        SessionTurnStatus::Pending
    );
    assert!(rig.logger.snapshot().unwrap().records.is_empty()); // 不是取消，不由 Drop 覆盖。
    rig.stop().await;
}

#[tokio::test]
async fn cancellation_records_unknown_side_effects_or_preserves_pending_with_diagnostic() {
    for fail_cancel in [false, true] {
        let store = Arc::new(FaultStore::default());
        let provider = Provider::new(vec![Step::blocked(Arc::new(Notify::new()))]);
        let rig = Rig::new(provider.clone(), store.clone(), LlmHostConfig::default()).await;
        let host = rig.host.clone();
        let running =
            tokio::spawn(async move { host.run_turn(input("private-session", "输入秘密")).await });
        provider.wait_requests(1).await;
        let bytes = store.bytes();
        store.fail.store(fail_cancel, Ordering::SeqCst);
        running.abort();
        assert!(running.await.unwrap_err().is_cancelled());
        if fail_cancel {
            assert_eq!(
                rig.snapshot("private-session").turns[0].status,
                SessionTurnStatus::Pending
            );
            assert_eq!(store.bytes(), bytes);
            let records = rig.logger.snapshot().unwrap().records;
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].entry.target, "session.cancel_commit");
            let text = format!("{:?}", records[0]);
            for secret in ["backend-secret", "输入秘密", "private-session", "用户一"] {
                assert!(!text.contains(secret));
            }
        } else {
            assert_eq!(
                rig.snapshot("private-session").turns[0].status,
                SessionTurnStatus::Failed {
                    failure: SessionFailure {
                        code: SessionFailureCode::Cancelled,
                        started_tools: None
                    }
                }
            );
        }
        rig.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_waits_for_final_commit_and_restart_invalidates_old_service() {
    let store = Arc::new(FaultStore::default());
    let provider = Provider::new(vec![
        Step {
            pause_commit: Some(store.clone()),
            ..Step::new(final_response("保存中"))
        },
        Step::new(final_response("新轮次")),
    ]);
    let rig = Rig::new(provider.clone(), store.clone(), LlmHostConfig::default()).await;
    let old = rig.service();
    let host = rig.host.clone();
    let running = tokio::spawn(async move { host.run_turn(input("s", "第一轮")).await });
    tokio::time::timeout(Duration::from_secs(3), store.entered.notified())
        .await
        .unwrap();
    let kernel = rig.kernel.clone();
    let mut stopping = tokio::spawn(async move { kernel.stop(&id(SESSION_PLUGIN_ID)).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut stopping)
            .await
            .is_err()
    );
    store.release();
    running.await.unwrap().unwrap();
    stopping.await.unwrap().unwrap();
    assert_eq!(old.snapshot(&key("s")), Err(SessionError::Unavailable));
    rig.kernel.start(&id(SESSION_PLUGIN_ID)).await.unwrap();
    assert_eq!(rig.snapshot("s").history().len(), 2);
    rig.host.run_turn(input("s", "第二轮")).await.unwrap();
    assert!(
        provider.requests.lock().unwrap()[1]
            .messages
            .iter()
            .any(|m| m.text.as_deref() == Some("保存中"))
    );
    rig.stop().await;
}

#[tokio::test]
async fn unstarted_session_plugin_is_unavailable_without_begin_or_model_request() {
    let provider = Provider::new(vec![]);
    let rig = Rig::new(
        provider.clone(),
        Arc::new(MemoryStateStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    rig.kernel.stop(&id(SESSION_PLUGIN_ID)).await.unwrap();
    rig.kernel.unregister(&id(SESSION_PLUGIN_ID)).unwrap();
    rig.kernel
        .register(Box::new(SessionPlugin::new().unwrap()))
        .unwrap();
    assert_eq!(
        rig.host.run_turn(input("s", "问题")).await.unwrap_err(),
        SessionRunError::Session(SessionError::Unavailable)
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    rig.stop().await;
}
