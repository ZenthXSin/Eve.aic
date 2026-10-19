//! 在线查询的非阻塞向量缓存。只缓存精确输入对应的向量，不缓存任何范围的记忆命中。
use eve_semantic_api::{
    EmbedFuture, EmbedResult, EmbeddingError, EmbeddingProfile, EmbeddingProvider, validate_inputs,
    validate_vectors,
};
use ring::digest::{Context, SHA256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::task::JoinHandle;

const CAPACITY: usize = 64;
const MAX_PENDING: usize = 4;
const TTL: Duration = Duration::from_secs(300);
const FAILURE_COOLDOWN: Duration = Duration::from_secs(30);

/// 未命中时登记一次后台请求并立即返回 `EmbeddingError::Provider`，
/// 供组合层选择词项回退。已有向量只在输入、模型、维度完全相同时复用。
/// 最多 64 条内存缓存、4 个在途查询；失败冷却 30 秒，成功缓存 5 分钟。
/// 不持久化、不自动重试；关闭/最终析构取消在途任务，重启不会重放查询。
#[derive(Clone)]
pub struct BackgroundEmbeddings {
    inner: Arc<dyn EmbeddingProvider>,
    state: Arc<Mutex<State>>,
}
#[derive(Default)]
struct State {
    closed: bool,
    entries: BTreeMap<[u8; 32], Entry>,
}
enum Entry {
    Pending(JoinHandle<()>),
    Ready {
        vectors: Vec<Vec<f32>>,
        until: Instant,
    },
    Failed {
        until: Instant,
    },
}
impl Drop for State {
    fn drop(&mut self) {
        for entry in self.entries.values() {
            if let Entry::Pending(task) = entry {
                task.abort();
            }
        }
    }
}
impl BackgroundEmbeddings {
    pub fn new(inner: Arc<dyn EmbeddingProvider>) -> Self {
        Self {
            inner,
            state: Arc::default(),
        }
    }

    /// 不再接收查询，取消并等待自己持有的任务退出。不会等待远端正常响应。
    pub async fn close(&self) -> EmbedResult<()> {
        let entries = {
            let mut state = self.state.lock().map_err(|_| EmbeddingError::Provider)?;
            state.closed = true;
            std::mem::take(&mut state.entries)
        };
        let mut tasks = Vec::new();
        for entry in entries.into_values() {
            if let Entry::Pending(task) = entry {
                task.abort();
                tasks.push(task);
            }
        }
        for task in tasks {
            let _ = task.await;
        }
        Ok(())
    }

    fn lookup(&self, inputs: &[String]) -> EmbedResult<Vec<Vec<f32>>> {
        validate_inputs(inputs)?;
        self.inner.profile().validate()?;
        let key = key(self.inner.profile(), inputs);
        let now = Instant::now();
        let mut state = self.state.lock().map_err(|_| EmbeddingError::Provider)?;
        if state.closed {
            return Err(EmbeddingError::Cancelled);
        }
        // 回收到期项；异常终止的替换实现也进入冷却，不能永久占用在途名额。
        state.entries.retain(|_, entry| match entry {
            Entry::Pending(task) if task.is_finished() => {
                *entry = Entry::Failed {
                    until: now + FAILURE_COOLDOWN,
                };
                true
            }
            Entry::Pending(_) => true,
            Entry::Ready { until, .. } | Entry::Failed { until } => *until > now,
        });
        if let Some(entry) = state.entries.get(&key) {
            return match entry {
                Entry::Ready { vectors, .. } => Ok(vectors.clone()),
                _ => Err(EmbeddingError::Provider),
            };
        }
        if state
            .entries
            .values()
            .filter(|entry| matches!(entry, Entry::Pending(_)))
            .count()
            >= MAX_PENDING
        {
            return Err(EmbeddingError::Provider);
        }
        if state.entries.len() >= CAPACITY {
            let oldest = state
                .entries
                .iter()
                .filter_map(|(key, entry)| match entry {
                    Entry::Ready { until, .. } | Entry::Failed { until } => Some((*until, *key)),
                    Entry::Pending(_) => None,
                })
                .min()
                .map(|(_, key)| key)
                .ok_or(EmbeddingError::Provider)?;
            state.entries.remove(&oldest);
        }
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| EmbeddingError::Provider)?;
        let cache = Arc::downgrade(&self.state);
        let provider = self.inner.clone();
        let inputs = inputs.to_vec();
        let task = runtime.spawn(async move {
            let result = provider.embed(&inputs).await.and_then(|vectors| {
                validate_vectors(provider.profile(), inputs.len(), &vectors)?;
                Ok(vectors)
            });
            if let Some(cache) = cache.upgrade()
                && let Ok(mut state) = cache.lock()
                && !state.closed
            {
                let entry = match result {
                    Ok(vectors) => Entry::Ready {
                        vectors,
                        until: Instant::now() + TTL,
                    },
                    Err(_) => Entry::Failed {
                        until: Instant::now() + FAILURE_COOLDOWN,
                    },
                };
                state.entries.insert(key, entry);
            }
        });
        state.entries.insert(key, Entry::Pending(task));
        Err(EmbeddingError::Provider)
    }
}
impl EmbeddingProvider for BackgroundEmbeddings {
    fn profile(&self) -> &EmbeddingProfile {
        self.inner.profile()
    }
    fn embed<'a>(&'a self, inputs: &'a [String]) -> EmbedFuture<'a> {
        Box::pin(async move { self.lookup(inputs) })
    }
}
fn key(profile: &EmbeddingProfile, inputs: &[String]) -> [u8; 32] {
    let mut hash = Context::new(&SHA256);
    hash.update(b"eve.query-embeddings.v1");
    hash.update(&profile.dimensions.to_be_bytes());
    for value in std::iter::once(&profile.model).chain(inputs.iter()) {
        hash.update(&(value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    let mut result = [0; 32];
    result.copy_from_slice(hash.finish().as_ref());
    result
}
