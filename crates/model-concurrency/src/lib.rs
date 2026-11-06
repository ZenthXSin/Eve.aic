//! 模型请求的共享并发准入；排队不创建新请求，取消与任何返回结局均释放位置。
#![forbid(unsafe_code)]
use eve_llm_api::{LlmError, LlmFuture, LlmProvider, ModelRequest, ModelResponse, ModelTextSink};
use std::{
    collections::{BTreeSet, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::sync::Notify;

struct State {
    revision: u64,
    limit: usize,
    next: u64,
    queued: VecDeque<u64>,
    active: BTreeSet<u64>,
}
/// 一个可信宿主范围中的并发通道；调用者不能从中取得额外权限或预算。
pub struct ConcurrencyGate {
    state: Mutex<State>,
    changed: Notify,
}
/// 瞬时准入统计；不包含请求正文、身份、端点或凭据。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GateStatus {
    pub revision: u64,
    pub limit: usize,
    pub active: usize,
    pub waiting: usize,
}
fn unavailable() -> LlmError {
    LlmError::Backend("模型并发准入状态不可用".into())
}
impl ConcurrencyGate {
    pub fn status(&self) -> Result<GateStatus, LlmError> {
        let state = self.state.lock().map_err(|_| unavailable())?;
        Ok(GateStatus {
            revision: state.revision,
            limit: state.limit,
            active: state.active.len(),
            waiting: state.queued.len(),
        })
    }
    pub fn new(limit: usize, revision: u64) -> Result<Arc<Self>, LlmError> {
        if limit == 0 {
            return Err(LlmError::Configuration("模型并发上限必须大于零".into()));
        }
        Ok(Arc::new(Self {
            state: Mutex::new(State {
                revision,
                limit,
                next: 0,
                queued: VecDeque::new(),
                active: BTreeSet::new(),
            }),
            changed: Notify::new(),
        }))
    }
    /// 只采纳更新修订。降低上限不终止在途请求，但须等空位；旧选择不能重新放宽上限。
    pub fn observe(&self, limit: usize, revision: u64) -> Result<(), LlmError> {
        if limit == 0 {
            return Err(LlmError::Configuration("模型并发上限必须大于零".into()));
        }
        let mut state = self.state.lock().map_err(|_| unavailable())?;
        if revision < state.revision {
            return Ok(());
        }
        if revision == state.revision && limit != state.limit {
            return Err(LlmError::Configuration(
                "同一配置修订的模型并发上限不一致".into(),
            ));
        }
        state.limit = limit;
        state.revision = revision;
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }
    pub fn wrap(self: &Arc<Self>, provider: Arc<dyn LlmProvider>) -> Arc<dyn LlmProvider> {
        Arc::new(LimitedProvider {
            gate: self.clone(),
            provider,
        })
    }
    /// 立即尝试准入；满额或已有排队者时返回 None，不排队，也不越过先到的请求。
    pub fn try_acquire(self: &Arc<Self>) -> Result<Option<GatePermit>, LlmError> {
        let mut state = self.state.lock().map_err(|_| unavailable())?;
        if !state.queued.is_empty() || state.active.len() >= state.limit {
            return Ok(None);
        }
        let id = state
            .next
            .checked_add(1)
            .ok_or_else(|| LlmError::Backend("模型并发调用编号已用尽".into()))?;
        state.next = id;
        state.active.insert(id);
        Ok(Some(GatePermit {
            gate: self.clone(),
            id,
        }))
    }
    /// 按先到顺序等待准入；调用者负责期限，取消等待会移除其位置。
    pub async fn acquire(self: &Arc<Self>) -> Result<GatePermit, LlmError> {
        let id = {
            let mut state = self.state.lock().map_err(|_| unavailable())?;
            let id = state
                .next
                .checked_add(1)
                .ok_or_else(|| LlmError::Backend("模型并发调用编号已用尽".into()))?;
            state.next = id;
            state.queued.push_back(id);
            id
        };
        let ticket = GatePermit {
            gate: self.clone(),
            id,
        };
        loop {
            // 先登记唤醒，再核对状态，避免检查与 await 之间丢失释放通知。
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let entered = {
                let mut state = self.state.lock().map_err(|_| unavailable())?;
                if state.queued.front() == Some(&id) && state.active.len() < state.limit {
                    state.queued.pop_front();
                    state.active.insert(id);
                    true
                } else {
                    false
                }
            };
            if entered {
                self.changed.notify_waiters();
                return Ok(ticket);
            }
            changed.await;
        }
    }
}
/// 准入凭据；持有期间计入在途请求，释放或取消时归还容量。
#[must_use]
pub struct GatePermit {
    gate: Arc<ConcurrencyGate>,
    id: u64,
}
impl Drop for GatePermit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.gate.state.lock()
            && !state.active.remove(&self.id)
        {
            state.queued.retain(|id| *id != self.id);
        }
        // 锁损坏也要唤醒排队者，让其报错关闭，而不是永久等待。
        self.gate.changed.notify_waiters();
    }
}
struct LimitedProvider {
    gate: Arc<ConcurrencyGate>,
    provider: Arc<dyn LlmProvider>,
}
impl LlmProvider for LimitedProvider {
    fn request_fingerprint(
        &self,
        request: &ModelRequest,
        mode: eve_llm_api::ResponseMode,
    ) -> Result<Option<eve_llm_api::ModelRequestFingerprint>, LlmError> {
        self.provider.request_fingerprint(request, mode)
    }

    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async move {
            let _permit = self.gate.acquire().await?;
            self.provider.complete(request).await
        })
    }
    fn complete_json(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async move {
            let _permit = self.gate.acquire().await?;
            self.provider.complete_json(request).await
        })
    }
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        sink: &'a dyn ModelTextSink,
    ) -> LlmFuture<'a, ModelResponse> {
        Box::pin(async move {
            let _permit = self.gate.acquire().await?;
            self.provider.stream(request, sink).await
        })
    }
}

#[cfg(test)]
mod tests;
