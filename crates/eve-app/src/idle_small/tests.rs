use super::*;
use crate::http_support::{Reply, Server};
use serde_json::json;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct Main(Mutex<Vec<ModelRequest>>);
impl LlmProvider for Main {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        self.0.lock().unwrap().push(request);
        Box::pin(async {
            Ok(ModelResponse::Final {
                text: "主模型照常回顾".into(),
            })
        })
    }
}

struct Auxiliary {
    calls: AtomicUsize,
    reply: Result<ModelResponse, LlmError>,
}
impl LlmProvider for Auxiliary {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        assert!(request.tools.is_empty());
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { self.reply.clone() })
    }
}

fn input(id: &str) -> String {
    format!(
        "{}{}",
        eve_subject_api::IDLE_INPUT_PREFIX,
        json!({
            "scope":{"channel":"qq","conversation_id":"conversation","principal_id":"owner"},
            "revision":1,"interests":["自由探索"],"evidence":[{
                "id":id,"kind":"task_result","text":"真实经历\n完整保留","excerpt":false
            }],"judgments":[]
        })
    )
}

fn parent(id: &str) -> ModelSession {
    ModelSession::anchored(
        "idle-review",
        Some(&eve_llm_api::ContextScope {
            session_id: "conversation".into(),
            user_id: "owner".into(),
        }),
        id,
    )
    .unwrap()
}

fn selector(provider: Option<Arc<dyn LlmProvider>>, input: String) -> Arc<Selector> {
    Arc::new(Selector {
        provider,
        input,
        steps: SharedToolSteps::new(10).unwrap(),
        hints: Default::default(),
    })
}

fn request(session: ModelSession, input: &str) -> ModelRequest {
    ModelRequest {
        session: Some(session),
        messages: vec![ChatMessage::text(ChatRole::User, input)],
        tools: vec![],
    }
}

#[tokio::test]
async fn frozen_input_reuses_one_auxiliary_request_and_records_real_usage() {
    let mut server = Server::start(vec![Reply::json(json!({
        "choices":[{"index":0,"message":{"role":"assistant","content":"{\"selected_ids\":[\"e-1\"]}"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":2048,"completion_tokens":16,"prompt_tokens_details":{"cached_tokens":1024}}
    }))]).await;
    let health = Arc::new(eve_model_health::HealthBook::new());
    let config = OpenAiConfig::chat("fixture-only")
        .with_base_url(server.url.trim_end_matches("/responses"))
        .unwrap();
    let transport = OpenAiProvider::anonymous(config)
        .unwrap()
        .with_usage_observer(health.clone(), ROLE);
    let selector = selector(
        Some(observed(Arc::new(transport), health.clone()).unwrap()),
        input("e-1"),
    );
    let main = Arc::new(Main::default());
    let resolver = Resolver {
        inner: Arc::new(FixedProvider(main.clone())),
        selector: selector.clone(),
    };
    for turn in ["one", "two"] {
        resolver
            .resolve()
            .unwrap()
            .provider
            .complete(request(
                parent("task-1").with_turn(turn).unwrap(),
                &selector.input,
            ))
            .await
            .unwrap();
    }
    let captured = server.next().await;
    assert_eq!(captured.body["model"], "fixture-only");
    assert!(captured.body.get("temperature").is_none());
    assert_eq!(
        captured.body["prompt_cache_key"],
        parent("task-1")
            .fork("idle-selector", None)
            .unwrap()
            .prompt_cache_key()
    );
    assert_eq!(captured.body["messages"][1]["content"], selector.input);
    assert_eq!(selector.steps.usage().unwrap().model_requests, 1);
    assert!(server.requests.try_recv().is_err());
    let requests = main.0.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].messages[0].text.as_deref(),
        Some(selector.input.as_str())
    );
    assert_eq!(requests[0].messages, requests[1].messages);
    assert!(
        requests[0]
            .messages
            .last()
            .unwrap()
            .text
            .as_deref()
            .unwrap()
            .contains("e-1")
    );
    let stats = health.snapshot();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].0, ROLE);
    assert_eq!(stats[0].1.requests, 1);
    assert_eq!(stats[0].1.failures, 0);
    assert_eq!(stats[0].1.cache.local_requests, 1);
    assert_eq!(stats[0].1.cache.reported_requests, 1);
    assert_eq!(stats[0].1.cache.cached_tokens, 1024);
    assert_eq!(stats[0].1.cache.input_tokens, 2048);
}

#[tokio::test]
async fn auxiliary_failure_or_invalid_selection_never_retries_or_blocks_main() {
    for reply in [
        Err(LlmError::ProviderTimeout),
        Ok(ModelResponse::Final {
            text: "{\"selected_ids\":[\"foreign\"]}".into(),
        }),
        Ok(ModelResponse::Final {
            text: "invalid-json".into(),
        }),
    ] {
        let auxiliary = Arc::new(Auxiliary {
            calls: AtomicUsize::new(0),
            reply,
        });
        let selector = selector(Some(auxiliary.clone()), input("e-1"));
        let main = Arc::new(Main::default());
        let provider = Provider {
            inner: main.clone(),
            selector: selector.clone(),
        };
        for _ in 0..2 {
            provider
                .complete(request(parent("task-1"), &selector.input))
                .await
                .unwrap();
        }
        assert_eq!(auxiliary.calls.load(Ordering::SeqCst), 1);
        assert_eq!(selector.steps.usage().unwrap().model_requests, 1);
        let requests = main.0.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|r| r.messages.len() == 1));
    }
}

#[tokio::test]
async fn selector_reuse_is_scoped_to_parent_task_and_frozen_input() {
    let auxiliary = Arc::new(Auxiliary {
        calls: AtomicUsize::new(0),
        reply: Ok(ModelResponse::Final {
            text: "{\"selected_ids\":[\"e-1\"]}".into(),
        }),
    });
    let first = selector(Some(auxiliary.clone()), input("e-1"));
    assert!(first.hint(None).await.is_none());
    assert_eq!(first.steps.usage().unwrap().model_requests, 0);
    assert!(first.hint(Some(&parent("task-1"))).await.is_some());
    assert!(first.hint(Some(&parent("task-2"))).await.is_some());
    assert_eq!(auxiliary.calls.load(Ordering::SeqCst), 2);
    let next = selector(Some(auxiliary.clone()), input("e-2"));
    assert!(next.hint(Some(&parent("task-1"))).await.is_none());
    assert_eq!(auxiliary.calls.load(Ordering::SeqCst), 3);
    let disabled = selector(None, input("e-1"));
    assert!(disabled.hint(Some(&parent("task-1"))).await.is_none());
    assert_eq!(disabled.steps.usage().unwrap().model_requests, 0);
}

struct Pending(AtomicUsize);
impl LlmProvider for Pending {
    fn complete(&self, _: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn cancelling_auxiliary_request_keeps_the_one_attempt_marker() {
    let auxiliary = Arc::new(Pending(AtomicUsize::new(0)));
    let selector = selector(Some(auxiliary.clone()), input("e-1"));
    let session = parent("task-1");
    let mut pending = Box::pin(selector.hint(Some(&session)));
    std::future::poll_fn(|cx| {
        assert!(pending.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    drop(pending);
    assert!(selector.hint(Some(&session)).await.is_none());
    assert_eq!(auxiliary.0.load(Ordering::SeqCst), 1);
    assert_eq!(selector.steps.usage().unwrap().model_requests, 1);
}
