//! 本地前缀复用探针：与同一用途最近的请求比较，估算本次请求从开头起有多少内容
//! 与已发出的请求完全相同（提供方前缀缓存能复用的上限）。只保存哈希，不保存正文。
use eve_llm_api::{
    LlmFuture, LlmProvider, ModelRequest, ModelResponse, ModelTextSink, ModelUsageObserver,
    PrefixReuse,
};
use std::{
    collections::{HashMap, VecDeque, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
};

/// 记住的最近请求数。
pub const PREFIX_MEMORY: usize = 64;

/// 与上下文压缩相同的保守估算：ASCII 每 4 字节一个 token，非 ASCII 每字符 2 个。
pub fn estimate_tokens(text: &str) -> u64 {
    let (ascii, other) = text.chars().fold((0u64, 0u64), |(a, o), c| {
        if c.is_ascii() { (a + 1, o) } else { (a, o + 1) }
    });
    ascii.div_ceil(4) + other * 2
}

fn serialized(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// 前缀哈希链：第 0 项是工具定义，第 i 项是工具定义加前 i 条消息。
fn chain(request: &ModelRequest) -> (Vec<u64>, Vec<u64>) {
    let mut hasher = DefaultHasher::new();
    let tools = serialized(&request.tools);
    tools.hash(&mut hasher);
    let mut hashes = vec![hasher.finish()];
    let mut tokens = vec![estimate_tokens(&tools)];
    for message in &request.messages {
        let text = serialized(message);
        text.hash(&mut hasher);
        // 图像不参与序列化；以字节摘要区分不同图像。
        for image in &message.images {
            image.data().hash(&mut hasher);
        }
        hashes.push(hasher.finish());
        tokens.push(estimate_tokens(&text) + message.images.len() as u64 * 1000);
    }
    (hashes, tokens)
}

#[derive(Default)]
struct Seen {
    order: VecDeque<Vec<u64>>,
    counts: HashMap<u64, u32>,
}

impl Seen {
    fn measure(&mut self, request: &ModelRequest) -> PrefixReuse {
        let (hashes, tokens) = chain(request);
        let known = hashes
            .iter()
            .take_while(|hash| self.counts.contains_key(hash))
            .count();
        let total: u64 = tokens.iter().sum();
        let reused: u64 = tokens.iter().take(known).sum();
        let reuse = PrefixReuse {
            estimated_tokens: total,
            reused_tokens: reused,
            messages: request.messages.len(),
            first_changed_message: (known < hashes.len()).then(|| known.saturating_sub(1)),
            tools_changed: known == 0 && !self.order.is_empty(),
        };
        for hash in &hashes {
            *self.counts.entry(*hash).or_default() += 1;
        }
        self.order.push_back(hashes);
        if self.order.len() > PREFIX_MEMORY
            && let Some(old) = self.order.pop_front()
        {
            for hash in old {
                if let Some(count) = self.counts.get_mut(&hash) {
                    *count -= 1;
                    if *count == 0 {
                        self.counts.remove(&hash);
                    }
                }
            }
        }
        reuse
    }
}

/// 包装实际发出请求的 Provider；在发出前测量，不改变请求、不重试。
pub struct PrefixProbe {
    inner: Arc<dyn LlmProvider>,
    role: String,
    observer: Arc<dyn ModelUsageObserver>,
    seen: Mutex<Seen>,
}

impl PrefixProbe {
    pub fn new(
        inner: Arc<dyn LlmProvider>,
        role: impl Into<String>,
        observer: Arc<dyn ModelUsageObserver>,
    ) -> Self {
        Self {
            inner,
            role: role.into(),
            observer,
            seen: Mutex::new(Seen::default()),
        }
    }

    fn measure(&self, request: &ModelRequest) {
        if let Ok(mut seen) = self.seen.lock() {
            let reuse = seen.measure(request);
            self.observer.prefix(&self.role, reuse);
        }
    }
}

impl LlmProvider for PrefixProbe {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        self.measure(&request);
        self.inner.complete(request)
    }
    fn complete_json(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        self.measure(&request);
        self.inner.complete_json(request)
    }
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        sink: &'a dyn ModelTextSink,
    ) -> LlmFuture<'a, ModelResponse> {
        self.measure(&request);
        self.inner.stream(request, sink)
    }
}
