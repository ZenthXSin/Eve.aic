use super::*;
use eve_llm_api::{ChatMessage, ChatRole};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Semaphore, mpsc};

fn request(text: &str) -> ModelRequest {
    ModelRequest {
        messages: vec![ChatMessage::text(ChatRole::User, text)],
        tools: vec![],
    }
}
fn response(text: &str) -> ModelResponse {
    ModelResponse::Final { text: text.into() }
}
struct Held {
    started: mpsc::UnboundedSender<String>,
    release: Semaphore,
    active: AtomicUsize,
    peak: AtomicUsize,
}
struct Active<'a>(&'a AtomicUsize);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Held {
    fn fixture() -> (Arc<Self>, mpsc::UnboundedReceiver<String>) {
        let (started, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                started,
                release: Semaphore::new(0),
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
            }),
            rx,
        )
    }
}
impl LlmProvider for Held {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async move {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            let _active = Active(&self.active);
            self.peak.fetch_max(active, Ordering::SeqCst);
            let label = request.messages[0].text.clone().unwrap();
            self.started.send(label.clone()).unwrap();
            self.release.acquire().await.unwrap().forget();
            Ok(response(&label))
        })
    }
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        sink: &'a dyn ModelTextSink,
    ) -> LlmFuture<'a, ModelResponse> {
        Box::pin(async move {
            sink.text_delta("stream-started".into()).await?;
            self.complete(request).await
        })
    }
}
async fn received(rx: &mut mpsc::UnboundedReceiver<String>) -> String {
    tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap()
}
async fn queued(gate: &ConcurrencyGate, count: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if gate.state.lock().unwrap().queued.len() == count {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
fn spawn(
    provider: Arc<dyn LlmProvider>,
    label: &'static str,
) -> tokio::task::JoinHandle<Result<ModelResponse, LlmError>> {
    tokio::spawn(async move { provider.complete(request(label)).await })
}

#[tokio::test]
async fn shared_wrappers_preserve_fifo_and_cancelled_queue_head_cannot_block_or_reach_provider() {
    let gate = ConcurrencyGate::new(1, 1).unwrap();
    let (held, mut started) = Held::fixture();
    let first = spawn(gate.wrap(held.clone()), "first");
    assert_eq!(received(&mut started).await, "first");
    let second = spawn(gate.wrap(held.clone()), "cancelled");
    queued(&gate, 1).await;
    let third = spawn(gate.wrap(held.clone()), "third");
    queued(&gate, 2).await;
    second.abort();
    assert!(second.await.unwrap_err().is_cancelled());
    queued(&gate, 1).await;
    held.release.add_permits(1);
    first.await.unwrap().unwrap();
    assert_eq!(received(&mut started).await, "third");
    held.release.add_permits(1);
    third.await.unwrap().unwrap();
    assert!(started.try_recv().is_err());
    assert_eq!(held.peak.load(Ordering::SeqCst), 1);
    assert!(gate.state.lock().unwrap().active.is_empty());
    assert!(gate.state.lock().unwrap().queued.is_empty());
}

#[tokio::test]
async fn cancelling_active_request_releases_slot_without_replaying_it() {
    let gate = ConcurrencyGate::new(1, 1).unwrap();
    let (held, mut started) = Held::fixture();
    let first = spawn(gate.wrap(held.clone()), "cancelled-active");
    assert_eq!(received(&mut started).await, "cancelled-active");
    let next = spawn(gate.wrap(held.clone()), "next");
    queued(&gate, 1).await;
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert_eq!(received(&mut started).await, "next");
    held.release.add_permits(1);
    next.await.unwrap().unwrap();
    assert_eq!(held.peak.load(Ordering::SeqCst), 1);
    assert!(started.try_recv().is_err());
    assert!(gate.state.lock().unwrap().active.is_empty());
}

#[tokio::test]
async fn streaming_holds_the_slot_until_completion_or_cancellation() {
    let gate = ConcurrencyGate::new(1, 1).unwrap();
    let (held, mut started) = Held::fixture();
    let sink = Arc::new(Sink(Mutex::new(vec![])));
    let provider = gate.wrap(held.clone());
    let streaming = provider.clone();
    let stream_sink = sink.clone();
    let first = tokio::spawn(async move {
        streaming
            .stream(request("stream"), stream_sink.as_ref())
            .await
    });
    assert_eq!(received(&mut started).await, "stream");
    let next = spawn(provider, "after-stream");
    queued(&gate, 1).await;
    assert_eq!(*sink.0.lock().unwrap(), ["stream-started"]);
    assert!(started.try_recv().is_err());
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert_eq!(received(&mut started).await, "after-stream");
    held.release.add_permits(1);
    next.await.unwrap().unwrap();
    assert_eq!(held.peak.load(Ordering::SeqCst), 1);
    assert_eq!(gate.status().unwrap().active, 0);
}

#[tokio::test]
async fn queue_wait_uses_the_existing_deadline_and_timeout_never_reaches_provider() {
    let gate = ConcurrencyGate::new(1, 1).unwrap();
    let (held, mut started) = Held::fixture();
    let provider = gate.wrap(held.clone());
    let first = spawn(provider.clone(), "first");
    assert_eq!(received(&mut started).await, "first");
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(10),
            provider.complete(request("timed-out-waiter"))
        )
        .await
        .is_err()
    );
    assert!(started.try_recv().is_err());
    assert_eq!(gate.status().unwrap().active, 1);
    assert_eq!(gate.status().unwrap().waiting, 0);
    held.release.add_permits(1);
    first.await.unwrap().unwrap();
    assert_eq!(gate.status().unwrap().active, 0);
}

struct Yielding {
    active: AtomicUsize,
    peak: AtomicUsize,
    calls: AtomicUsize,
}
impl LlmProvider for Yielding {
    fn complete(&self, _: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            let _active = Active(&self.active);
            self.peak.fetch_max(active, Ordering::SeqCst);
            tokio::task::yield_now().await;
            Ok(response("ok"))
        })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multithreaded_burst_keeps_the_shared_limit_and_settles_every_request_once() {
    let gate = ConcurrencyGate::new(3, 1).unwrap();
    let inner = Arc::new(Yielding {
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
    });
    let mut calls = tokio::task::JoinSet::new();
    for _ in 0..100 {
        let provider = gate.wrap(inner.clone());
        calls.spawn(async move { provider.complete(request("burst")).await });
    }
    while let Some(result) = calls.join_next().await {
        result.unwrap().unwrap();
    }
    assert_eq!(inner.calls.load(Ordering::SeqCst), 100);
    assert!(inner.peak.load(Ordering::SeqCst) <= 3);
    assert_eq!(gate.status().unwrap().active, 0);
    assert_eq!(gate.status().unwrap().waiting, 0);
}

#[tokio::test]
async fn decreasing_waits_for_existing_calls_and_increasing_wakes_waiters_but_stale_revision_cannot_reopen()
 {
    let gate = ConcurrencyGate::new(2, 1).unwrap();
    let (held, mut started) = Held::fixture();
    let first = spawn(gate.wrap(held.clone()), "a");
    assert_eq!(received(&mut started).await, "a");
    let second = spawn(gate.wrap(held.clone()), "b");
    assert_eq!(received(&mut started).await, "b");
    gate.observe(1, 2).unwrap();
    let third = spawn(gate.wrap(held.clone()), "c");
    queued(&gate, 1).await;
    held.release.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while gate.state.lock().unwrap().active.len() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(started.try_recv().is_err());
    held.release.add_permits(1);
    assert_eq!(received(&mut started).await, "c");
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    let fourth = spawn(gate.wrap(held.clone()), "d");
    queued(&gate, 1).await;
    gate.observe(2, 3).unwrap();
    assert_eq!(received(&mut started).await, "d");
    gate.observe(100, 1).unwrap();
    assert_eq!(gate.state.lock().unwrap().limit, 2);
    held.release.add_permits(2);
    third.await.unwrap().unwrap();
    fourth.await.unwrap().unwrap();
    assert_eq!(held.peak.load(Ordering::SeqCst), 2);
}

struct Modes;
impl LlmProvider for Modes {
    fn complete(&self, _: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async { Err(LlmError::Backend("synthetic-error".into())) })
    }
    fn complete_json(&self, _: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async { Ok(response("json")) })
    }
    fn stream<'a>(
        &'a self,
        _: ModelRequest,
        sink: &'a dyn ModelTextSink,
    ) -> LlmFuture<'a, ModelResponse> {
        Box::pin(async move {
            sink.text_delta("stream".into()).await?;
            Ok(response("stream"))
        })
    }
}
struct Panics;
impl LlmProvider for Panics {
    fn complete(&self, _: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async { panic!("synthetic-provider-panic") })
    }
}
#[tokio::test]
async fn provider_panic_releases_the_slot_without_poisoning_the_gate() {
    let gate = ConcurrencyGate::new(1, 1).unwrap();
    let failed = spawn(gate.wrap(Arc::new(Panics)), "panic");
    assert!(failed.await.unwrap_err().is_panic());
    assert_eq!(gate.status().unwrap().active, 0);
    assert_eq!(gate.status().unwrap().waiting, 0);
    let healthy = gate.wrap(Arc::new(Modes));
    assert_eq!(
        healthy.complete_json(request("after-panic")).await.unwrap(),
        response("json")
    );
}
struct Sink(Mutex<Vec<String>>);
impl ModelTextSink for Sink {
    fn text_delta(&self, text: String) -> LlmFuture<'_, ()> {
        Box::pin(async move {
            self.0.lock().unwrap().push(text);
            Ok(())
        })
    }
}
#[tokio::test]
async fn errors_release_capacity_and_json_and_stream_keep_their_original_protocol() {
    let gate = ConcurrencyGate::new(1, 1).unwrap();
    let provider = gate.wrap(Arc::new(Modes));
    assert!(provider.complete(request("failure")).await.is_err());
    assert_eq!(
        provider.complete_json(request("json")).await.unwrap(),
        response("json")
    );
    let sink = Sink(Mutex::new(vec![]));
    assert_eq!(
        provider.stream(request("stream"), &sink).await.unwrap(),
        response("stream")
    );
    assert_eq!(*sink.0.lock().unwrap(), ["stream"]);
    assert!(gate.state.lock().unwrap().active.is_empty());
}
#[tokio::test]
async fn invalid_limits_conflicting_revision_and_ticket_overflow_fail_without_queue_mutation() {
    assert!(ConcurrencyGate::new(0, 1).is_err());
    let gate = ConcurrencyGate::new(2, 1).unwrap();
    assert!(gate.observe(3, 1).is_err());
    assert!(gate.observe(0, 2).is_err());
    gate.state.lock().unwrap().next = u64::MAX;
    assert!(gate.acquire().await.is_err());
    let state = gate.state.lock().unwrap();
    assert_eq!((state.limit, state.revision), (2, 1));
    assert!(state.active.is_empty() && state.queued.is_empty());
}
