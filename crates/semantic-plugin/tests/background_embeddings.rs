use eve_semantic_api::*;
use eve_semantic_plugin::BackgroundEmbeddings;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Semaphore;

struct Provider {
    profile: EmbeddingProfile,
    calls: AtomicUsize,
    active: Arc<AtomicUsize>,
    release: Semaphore,
    invalid: bool,
}
impl Provider {
    fn new(invalid: bool) -> Arc<Self> {
        Arc::new(Self {
            profile: EmbeddingProfile {
                model: "synthetic".into(),
                dimensions: 2,
            },
            calls: AtomicUsize::new(0),
            active: Arc::default(),
            release: Semaphore::new(0),
            invalid,
        })
    }
}
struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl EmbeddingProvider for Provider {
    fn profile(&self) -> &EmbeddingProfile {
        &self.profile
    }
    fn embed<'a>(&'a self, inputs: &'a [String]) -> EmbedFuture<'a> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.active.fetch_add(1, Ordering::SeqCst);
            let _active = Active(self.active.clone());
            self.release
                .acquire()
                .await
                .map_err(|_| EmbeddingError::Provider)?
                .forget();
            Ok(inputs
                .iter()
                .map(|_| {
                    if self.invalid {
                        vec![0.0]
                    } else {
                        vec![1.0, 0.5]
                    }
                })
                .collect())
        })
    }
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cold_and_duplicate_queries_never_wait_but_ready_exact_vectors_are_reused() {
    let provider = Provider::new(false);
    let cache = BackgroundEmbeddings::new(provider.clone());
    let query = vec!["synthetic query".into()];
    assert_eq!(cache.embed(&query).await, Err(EmbeddingError::Provider));
    assert_eq!(cache.embed(&query).await, Err(EmbeddingError::Provider));
    until(|| provider.calls.load(Ordering::SeqCst) == 1).await;
    assert_eq!(provider.active.load(Ordering::SeqCst), 1);
    provider.release.add_permits(1);
    until(|| provider.active.load(Ordering::SeqCst) == 0).await;
    assert_eq!(cache.embed(&query).await.unwrap(), vec![vec![1.0, 0.5]]);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        cache.embed(&["different query".into()]).await,
        Err(EmbeddingError::Provider)
    );
    until(|| provider.calls.load(Ordering::SeqCst) == 2).await;
    cache.close().await.unwrap();
    assert_eq!(provider.active.load(Ordering::SeqCst), 0);
    assert_eq!(cache.embed(&query).await, Err(EmbeddingError::Cancelled));
}

#[tokio::test]
async fn distinct_pending_queries_are_bounded_and_final_drop_cancels_them() {
    let provider = Provider::new(false);
    let cache = BackgroundEmbeddings::new(provider.clone());
    for number in 0..20 {
        assert!(cache.embed(&[format!("query {number}")]).await.is_err());
    }
    until(|| provider.active.load(Ordering::SeqCst) == 4).await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 4);
    drop(cache);
    until(|| provider.active.load(Ordering::SeqCst) == 0).await;
}

#[tokio::test]
async fn invalid_vectors_cool_down_without_retries_and_restart_discards_cache() {
    let provider = Provider::new(true);
    let cache = BackgroundEmbeddings::new(provider.clone());
    let query = vec!["query".into()];
    assert!(cache.embed(&query).await.is_err());
    until(|| provider.active.load(Ordering::SeqCst) == 1).await;
    provider.release.add_permits(1);
    until(|| provider.active.load(Ordering::SeqCst) == 0).await;
    for _ in 0..10 {
        assert!(cache.embed(&query).await.is_err());
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    cache.close().await.unwrap();
    let fresh = BackgroundEmbeddings::new(provider.clone());
    tokio::task::yield_now().await;
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "restart must not replay old work"
    );
    assert!(fresh.embed(&query).await.is_err());
    until(|| provider.calls.load(Ordering::SeqCst) == 2).await;
    fresh.close().await.unwrap();
}

#[tokio::test]
async fn invalid_queries_are_rejected_before_any_background_work() {
    let provider = Provider::new(false);
    let cache = BackgroundEmbeddings::new(provider.clone());
    assert_eq!(cache.embed(&[]).await, Err(EmbeddingError::InvalidInput));
    assert_eq!(
        cache.embed(&["\0".into()]).await,
        Err(EmbeddingError::InvalidInput)
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    cache.close().await.unwrap();
}
