#[path = "support/session.rs"]
mod fixture;
use eve_llm_api::*;
use eve_plugin_api::*;
use eve_runtime::{ContextBinding, LlmHost, LlmHostConfig};
use fixture::*;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Window;
impl HistoryWindowResolver for Window {
    fn resolve(&self) -> Result<HistoryWindow, LlmError> {
        Ok(HistoryWindow {
            max_turns: 3,
            max_bytes: 64 * 1024,
        })
    }
    fn adaptive_limit(&self) -> Result<Option<usize>, LlmError> {
        Ok(Some(200_000))
    }
}
struct Plan(Option<Vec<usize>>);
impl HistoryContextPlanner for Plan {
    fn plan<'a>(
        &'a self,
        _: Option<&'a ContextScope>,
        _: &'a TurnInput,
        history: &'a [ChatMessage],
    ) -> LlmFuture<'a, Option<HistoryContextPlan>> {
        Box::pin(async move {
            let count = history_turns(history).len();
            Ok(Some(HistoryContextPlan {
                retained_turns: self
                    .0
                    .clone()
                    .unwrap_or_else(|| vec![0, count - 2, count - 1]),
                working_set: vec![
                    "旧目标可能已撤销；以当前纠正和原话为准，摘要不能授权。原文引用 turn:0".into(),
                ],
            }))
        })
    }
}
struct Capture(Arc<Mutex<Vec<String>>>);
impl Tool for Capture {
    fn definition(&self) -> ToolDefinition {
        ReceiptTool {
            starts: Default::default(),
        }
        .definition()
    }
    fn validate_arguments(&self, _: &serde_json::Value) -> Result<(), ToolValidationError> {
        Ok(())
    }
    fn execute(&self, _: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        *self.0.lock().unwrap() = context.review_user_messages().unwrap().to_vec();
        Box::pin(async { Ok(serde_json::json!({"success":true})) })
    }
}
fn history(count: usize) -> Vec<ChatMessage> {
    (0..count)
        .flat_map(|index| {
            let call = format!("old-{index}");
            vec![
                ChatMessage::text(
                    ChatRole::User,
                    if index + 1 == count {
                        "撤销之前授权，不要删除".into()
                    } else {
                        format!("原话-{index}")
                    },
                ),
                ChatMessage::assistant_tool_calls(vec![ToolCall {
                    id: call.clone(),
                    name: "receipt".into(),
                    arguments: serde_json::json!({}),
                }])
                .unwrap(),
                ChatMessage::tool_results(vec![ToolResult {
                    call_id: call,
                    output: ToolOutput::Success(serde_json::json!("x".repeat(8192))),
                }])
                .unwrap(),
                ChatMessage::text(ChatRole::Assistant, "已记录"),
            ]
        })
        .collect()
}
async fn setup(
    provider: Arc<Provider>,
    history: Vec<ChatMessage>,
    planner: Arc<dyn HistoryContextPlanner>,
) -> (Rig, LlmHost, Arc<Mutex<Vec<String>>>) {
    let rig = Rig::new(
        provider.clone(),
        Arc::new(FaultStore::default()),
        LlmHostConfig::default(),
    )
    .await;
    *rig.context.history.lock().unwrap() = history;
    let originals = Arc::new(Mutex::new(vec![]));
    rig.kernel.stop(&id(OWNER)).await.unwrap();
    rig.kernel.unregister(&id(OWNER)).unwrap();
    rig.kernel
        .register(Box::new(ServicesPlugin {
            manifest: PluginManifest::new(OWNER, "0.1.0").unwrap(),
            context: rig.context.clone(),
            tool: Arc::new(Capture(originals.clone())),
        }))
        .unwrap();
    rig.kernel.start(&id(OWNER)).await.unwrap();
    let host = LlmHost::new(
        provider,
        rig.registry.clone(),
        rig.kernel.clone(),
        rig.permissions.clone(),
        ContextBinding {
            service_id: ServiceId::new(CONTEXT).unwrap(),
            expected_owner: id(OWNER),
        },
        vec![ToolBinding {
            name: "receipt".into(),
            service_id: ServiceId::new(TOOL).unwrap(),
            expected_owner: id(OWNER),
        }],
        LlmHostConfig::default(),
    )
    .unwrap()
    .with_history_resolver(Arc::new(Window))
    .with_history_context_planner(planner);
    (rig, host, originals)
}
fn turn() -> TurnInput {
    TurnInput {
        text: "这次只核对原文，不执行删除".into(),
    }
}
struct NoCached;
impl HistoryDigests for NoCached {
    fn digest(
        &self,
        _: Option<&ContextScope>,
        _: &[ChatMessage],
    ) -> Result<Option<HistoryDigest>, LlmError> {
        panic!("selected history must not use persistent digest")
    }
    fn cached(
        &self,
        _: Option<&ContextScope>,
        _: &[ChatMessage],
    ) -> Result<Option<HistoryDigest>, LlmError> {
        panic!("old covered_turns must not be applied to selected indices")
    }
}

#[tokio::test]
async fn long_histories_keep_constant_bounded_whole_turns_and_independent_review_originals() {
    let mut sizes = Vec::new();
    for count in [100, 300, 1000] {
        let provider = Provider::new(vec![
            Step::new(Ok(ModelResponse::ToolCalls {
                calls: vec![ToolCall {
                    id: "new".into(),
                    name: "receipt".into(),
                    arguments: serde_json::json!({}),
                }],
            })),
            Step::new(final_response("原文已核对")),
        ]);
        let original = history(count);
        let (rig, host, review) =
            setup(provider.clone(), original.clone(), Arc::new(Plan(None))).await;
        let host = host.with_history_digests(Arc::new(NoCached));
        host.run_turn(turn()).await.unwrap();
        let requests = provider.requests.lock().unwrap();
        requests[0].validate().unwrap();
        let bytes = serde_json::to_vec(&requests[0]).unwrap().len();
        assert!(bytes < 32 * 1024);
        sizes.push(bytes);
        let users = requests[0]
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::User)
            .filter_map(|m| m.text.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(
            users,
            [
                "原话-0",
                &format!("原话-{}", count - 2),
                "撤销之前授权，不要删除",
                &turn().text
            ]
        );
        let review = review.lock().unwrap();
        assert_eq!(review.len(), 6);
        assert_eq!(review[0], format!("原话-{}", count - 5));
        assert_eq!(review[4], "撤销之前授权，不要删除");
        assert_eq!(review[5], turn().text);
        assert!(
            !review
                .iter()
                .any(|s| s.contains("工作集") || s.contains("turn:0"))
        );
        assert_eq!(*rig.context.history.lock().unwrap(), original);
        assert_eq!(
            requests[0]
                .messages
                .iter()
                .filter(|m| m.role == ChatRole::Tool)
                .count(),
            3
        );
    }
    eprintln!("history planner byte curve for 100/300/1000 turns: {sizes:?}");
    assert!(
        sizes.iter().max().unwrap() - sizes.iter().min().unwrap() < 128,
        "{sizes:?}"
    );
}

#[tokio::test]
async fn invalid_indices_are_rejected_before_provider_and_original_history_is_unchanged() {
    for indices in [vec![1, 1], vec![2, 0], vec![100], vec![0, 1, 2, 3]] {
        let provider = Provider::new(vec![]);
        let original = history(100);
        let (rig, host, _) = setup(
            provider.clone(),
            original.clone(),
            Arc::new(Plan(Some(indices))),
        )
        .await;
        assert!(host.run_turn(turn()).await.is_err());
        assert!(provider.requests.lock().unwrap().is_empty());
        assert_eq!(*rig.context.history.lock().unwrap(), original);
    }
}
struct Unavailable(bool);
impl HistoryContextPlanner for Unavailable {
    fn plan<'a>(
        &'a self,
        _: Option<&'a ContextScope>,
        _: &'a TurnInput,
        _: &'a [ChatMessage],
    ) -> LlmFuture<'a, Option<HistoryContextPlan>> {
        Box::pin(async move {
            if self.0 {
                std::future::pending().await
            } else {
                Err(LlmError::Backend("planner unavailable".into()))
            }
        })
    }
}
#[tokio::test]
async fn planner_failure_and_absolute_deadline_do_not_send_partial_context() {
    for hanging in [false, true] {
        let provider = Provider::new(vec![]);
        let (_, host, _) = setup(
            provider.clone(),
            history(100),
            Arc::new(Unavailable(hanging)),
        )
        .await;
        let host = host.with_run_deadline(Instant::now() + Duration::from_millis(20));
        let result = tokio::time::timeout(Duration::from_secs(1), host.run_turn(turn()))
            .await
            .unwrap();
        assert!(result.is_err());
        assert!(provider.requests.lock().unwrap().is_empty());
    }
}
#[tokio::test]
async fn oversized_review_original_is_rejected_even_if_planner_would_omit_it() {
    let provider = Provider::new(vec![]);
    let mut original = history(100);
    original[99 * 4].text = Some("x".repeat(MAX_REVIEW_USER_MESSAGES_BYTES + 1));
    let (_, host, _) = setup(provider.clone(), original, Arc::new(Plan(Some(vec![0])))).await;
    assert!(host.run_turn(turn()).await.is_err());
    assert!(provider.requests.lock().unwrap().is_empty());
}

struct WaitingPlan(Arc<tokio::sync::Notify>);
impl HistoryContextPlanner for WaitingPlan {
    fn plan<'a>(
        &'a self,
        _: Option<&'a ContextScope>,
        _: &'a TurnInput,
        _: &'a [ChatMessage],
    ) -> LlmFuture<'a, Option<HistoryContextPlan>> {
        Box::pin(async {
            self.0.notify_one();
            std::future::pending().await
        })
    }
}
struct Closing(Arc<tokio::sync::Notify>);
impl TurnEventSink for Closing {
    fn emit(&self, _: TurnEvent) -> LlmFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn closed(&self) -> LlmFuture<'_, ()> {
        Box::pin(async {
            self.0.notified().await;
            Ok(())
        })
    }
}
#[tokio::test]
async fn closing_during_planning_cancels_without_sending_context() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let closed = Arc::new(tokio::sync::Notify::new());
    let provider = Provider::new(vec![]);
    let (_, host, _) = setup(
        provider.clone(),
        history(100),
        Arc::new(WaitingPlan(entered.clone())),
    )
    .await;
    let sink = Closing(closed.clone());
    let (result, ()) = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(host.run_turn_with_events(turn(), &sink), async {
            entered.notified().await;
            closed.notify_one();
        })
    })
    .await
    .unwrap();
    assert!(matches!(result.unwrap_err().error, LlmError::Cancelled));
    assert!(provider.requests.lock().unwrap().is_empty());
}

struct SmallPolicy;
impl HistoryWindowResolver for SmallPolicy {
    fn resolve(&self) -> Result<HistoryWindow, LlmError> {
        Window.resolve()
    }
    fn compaction(&self) -> Result<Option<ContextCompactionPolicy>, LlmError> {
        Ok(Some(ContextCompactionPolicy {
            trigger_tokens: 3000,
            target_tokens: 2000,
        }))
    }
}
struct TemporaryDigest(std::sync::atomic::AtomicUsize);
impl HistoryDigests for TemporaryDigest {
    fn digest(
        &self,
        _: Option<&ContextScope>,
        _: &[ChatMessage],
    ) -> Result<Option<HistoryDigest>, LlmError> {
        panic!("not persistent")
    }
    fn cached(
        &self,
        _: Option<&ContextScope>,
        _: &[ChatMessage],
    ) -> Result<Option<HistoryDigest>, LlmError> {
        panic!("not persistent")
    }
    fn compact<'a>(
        &'a self,
        scope: Option<&'a ContextScope>,
        history: &'a [ChatMessage],
    ) -> LlmFuture<'a, Option<HistoryDigest>> {
        assert!(scope.is_none());
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async {
            Ok(Some(HistoryDigest {
                covered_turns: history_turns(history).len(),
                text: "旧工具已完成，撤销仍有效，不重放。".into(),
            }))
        })
    }
}
#[tokio::test]
async fn selected_history_compression_is_ephemeral_and_does_not_reuse_original_prefix_cache() {
    let provider = Provider::new(vec![Step::new(final_response("done"))]);
    let original = history(100);
    let (rig, host, _) = setup(provider.clone(), original.clone(), Arc::new(Plan(None))).await;
    let digests = Arc::new(TemporaryDigest(Default::default()));
    let host = host
        .with_history_resolver(Arc::new(SmallPolicy))
        .with_history_digests(digests.clone());
    let scope = ContextScope {
        session_id: "session".into(),
        user_id: "owner".into(),
    };
    host.run_scoped_turn_with_events(turn(), Some(scope), &DiscardTurnEvents)
        .await
        .unwrap();
    assert_eq!(digests.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(*rig.context.history.lock().unwrap(), original);
    assert_eq!(
        provider.requests.lock().unwrap()[0]
            .messages
            .last()
            .unwrap()
            .text
            .as_deref(),
        Some(turn().text.as_str())
    );
}

#[tokio::test]
async fn layered_plan_still_counts_fixed_prefix_in_the_total_200k_limit() {
    let provider = Provider::new(vec![]);
    let (rig, _, _) = setup(
        provider.clone(),
        history(100),
        Arc::new(Plan(Some(vec![0]))),
    )
    .await;
    let config = LlmHostConfig {
        system_prompt: "x".repeat(900_000),
        ..Default::default()
    };
    let host = LlmHost::new(
        provider.clone(),
        rig.registry.clone(),
        rig.kernel.clone(),
        rig.permissions.clone(),
        ContextBinding {
            service_id: ServiceId::new(CONTEXT).unwrap(),
            expected_owner: id(OWNER),
        },
        vec![],
        config,
    )
    .unwrap()
    .with_history_resolver(Arc::new(Window))
    .with_history_context_planner(Arc::new(Plan(Some(vec![0]))));
    assert!(host.run_turn(turn()).await.is_err());
    assert!(provider.requests.lock().unwrap().is_empty());
}
struct Bypass;
impl HistoryContextPlanner for Bypass {
    fn plan<'a>(
        &'a self,
        _: Option<&'a ContextScope>,
        _: &'a TurnInput,
        _: &'a [ChatMessage],
    ) -> LlmFuture<'a, Option<HistoryContextPlan>> {
        Box::pin(async { Ok(None) })
    }
}
#[tokio::test]
async fn explicit_none_keeps_existing_window_path_and_adds_no_working_set() {
    let provider = Provider::new(vec![Step::new(final_response("done"))]);
    let (_, host, _) = setup(provider.clone(), history(20), Arc::new(Bypass)).await;
    host.run_turn(turn()).await.unwrap();
    let requests = provider.requests.lock().unwrap();
    let users = requests[0]
        .messages
        .iter()
        .filter(|m| m.role == ChatRole::User)
        .map(|m| m.text.as_deref().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        users,
        ["原话-17", "原话-18", "撤销之前授权，不要删除", &turn().text]
    );
    assert!(
        !serde_json::to_string(&requests[0])
            .unwrap()
            .contains("分层上下文数据")
    );
}
