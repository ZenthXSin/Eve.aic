use crate::{Kernel, PluginState};
use eve_plugin_api::{
    LifecycleOperation, LifecycleOperationId, LifecycleOperationState, LifecycleRequest,
    PluginError, PluginFuture, PluginResult, RuntimeLifecycle,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::{OwnedMutexGuard, watch};

struct OperationEntry {
    id: LifecycleOperationId,
    request: LifecycleRequest,
    state: watch::Sender<LifecycleOperationState>,
}

impl OperationEntry {
    fn snapshot(&self) -> LifecycleOperation {
        LifecycleOperation {
            id: self.id,
            request: self.request.clone(),
            state: self.state.borrow().clone(),
        }
    }
}

#[derive(Default)]
pub(crate) struct OperationRegistry {
    next_id: u64,
    entries: BTreeMap<LifecycleOperationId, Arc<OperationEntry>>,
    interrupted: Option<PluginError>,
}

/// 与 worker 一起拥有串行锁；即使 worker 未首次轮询就被执行器丢弃，也会记录中断。
struct OperationGuard {
    kernel: Option<Kernel>,
    entry: Arc<OperationEntry>,
    started: bool,
    completed: bool,
    admission: Option<OwnedMutexGuard<()>>,
}

impl OperationGuard {
    fn complete(&mut self, result: PluginResult<()>) {
        // 先释放 Kernel 和准入锁，再通知等待者；否则 wait 返回后后台 worker
        // 仍可能暂时持有状态后端，导致立即重建 Runtime 时出现锁竞争。
        self.kernel.take();
        self.admission.take();
        self.completed = true;
        self.entry
            .state
            .send_replace(LifecycleOperationState::Completed(result));
    }
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }

        if !self.started {
            self.kernel.take();
            self.admission.take();
            self.entry
                .state
                .send_replace(LifecycleOperationState::Interrupted(
                    PluginError::Lifecycle(format!(
                        "生命周期操作 {} 在开始执行前中断，插件未被该操作修改",
                        self.entry.id
                    )),
                ));
            return;
        }

        let error = PluginError::Lifecycle(format!(
            "生命周期操作 {} ({:?}) 意外中断，异步收尾未确认；请重建 Runtime",
            self.entry.id, self.entry.request
        ));
        // 不在持有报告锁时操作 Scope，避免与 Context 的能力访问形成锁顺序环。
        let Some(kernel) = self.kernel.as_ref() else {
            return;
        };
        kernel
            .inner
            .operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .interrupted = Some(error.clone());
        let slots = kernel
            .inner
            .plugins
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for slot in slots {
            let scope = slot
                .scope
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            if let Some(scope) = scope {
                scope.revoke();
            }
            let mut state = slot
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if matches!(
                *state,
                PluginState::WaitingDependencies
                    | PluginState::Starting
                    | PluginState::Active
                    | PluginState::Stopping
            ) {
                *state = PluginState::Failed;
            }
        }
        self.kernel.take();
        self.admission.take();
        self.entry
            .state
            .send_replace(LifecycleOperationState::Interrupted(error));
    }
}

impl Kernel {
    /// 串行准入后移交独立 worker；取消等待不会丢弃正在执行的生命周期 Future。
    pub async fn submit_lifecycle(
        &self,
        request: LifecycleRequest,
    ) -> PluginResult<LifecycleOperationId> {
        Ok(self.admit_lifecycle(request).await?.id)
    }

    async fn admit_lifecycle(
        &self,
        request: LifecycleRequest,
    ) -> PluginResult<Arc<OperationEntry>> {
        let executor = tokio::runtime::Handle::try_current()
            .map_err(|_| PluginError::Lifecycle("生命周期操作需要运行中的 Tokio 执行器".into()))?;
        self.ensure_lifecycle_healthy()?;
        let admission = self.inner.lifecycle.clone().lock_owned().await;
        // 从准入到 spawn 之间没有 await，同一次 poll 内完成登记及所有权转交。
        let entry = {
            let mut registry = self
                .inner
                .operations
                .lock()
                .map_err(|_| PluginError::Lifecycle("生命周期报告锁中毒".into()))?;
            if let Some(error) = &registry.interrupted {
                return Err(error.clone());
            }
            if registry.entries.len() >= self.inner.config.lifecycle_report_capacity {
                return Err(PluginError::Lifecycle(
                    "生命周期报告容量已满，请先确认已完成的操作".into(),
                ));
            }
            let next_id = registry
                .next_id
                .checked_add(1)
                .ok_or_else(|| PluginError::Lifecycle("生命周期操作 ID 已耗尽".into()))?;
            let id = LifecycleOperationId::new(next_id);
            let (state, _) = watch::channel(LifecycleOperationState::Running);
            let entry = Arc::new(OperationEntry { id, request, state });
            registry.entries.insert(id, entry.clone());
            registry.next_id = next_id;
            entry
        };
        let mut guard = OperationGuard {
            kernel: Some(self.clone()),
            entry: entry.clone(),
            started: false,
            completed: false,
            admission: Some(admission),
        };
        // 丢弃 JoinHandle 仅分离等待；worker 持有 Kernel 和准入锁直到操作结束。
        drop(executor.spawn(async move {
            guard.started = true;
            let result = guard
                .kernel
                .as_ref()
                .expect("生命周期 worker 缺少 Kernel")
                .execute_lifecycle(guard.entry.request.clone())
                .await;
            guard.complete(result);
        }));
        Ok(entry)
    }

    pub fn lifecycle_operations(&self) -> PluginResult<Vec<LifecycleOperation>> {
        let registry = self
            .inner
            .operations
            .lock()
            .map_err(|_| PluginError::Lifecycle("生命周期报告锁中毒".into()))?;
        Ok(registry
            .entries
            .values()
            .map(|entry| entry.snapshot())
            .collect())
    }

    /// 查询结果不会消费记录，允许多个等待者独立获取同一个终态。
    pub async fn wait_lifecycle(
        &self,
        id: LifecycleOperationId,
    ) -> PluginResult<LifecycleOperation> {
        let entry = self
            .inner
            .operations
            .lock()
            .map_err(|_| PluginError::Lifecycle("生命周期报告锁中毒".into()))?
            .entries
            .get(&id)
            .cloned()
            .ok_or_else(|| PluginError::Lifecycle(format!("生命周期操作 {id} 不存在")))?;
        Self::wait_entry(entry).await
    }

    async fn wait_entry(entry: Arc<OperationEntry>) -> PluginResult<LifecycleOperation> {
        let mut state = entry.state.subscribe();
        loop {
            let current = state.borrow_and_update().clone();
            if current != LifecycleOperationState::Running {
                return Ok(LifecycleOperation {
                    id: entry.id,
                    request: entry.request.clone(),
                    state: current,
                });
            }
            state.changed().await.map_err(|_| {
                PluginError::Lifecycle(format!("生命周期操作 {} 的通知已关闭", entry.id))
            })?;
        }
    }

    /// 不清除运行中的记录，也不因其他读取者已经确认结果而报错。
    pub fn acknowledge_lifecycle(&self, id: LifecycleOperationId) -> PluginResult<bool> {
        let mut registry = self
            .inner
            .operations
            .lock()
            .map_err(|_| PluginError::Lifecycle("生命周期报告锁中毒".into()))?;
        let Some(entry) = registry.entries.get(&id) else {
            return Ok(false);
        };
        if *entry.state.borrow() == LifecycleOperationState::Running {
            return Err(PluginError::Lifecycle(format!(
                "生命周期操作 {id} 仍在运行，不能确认删除"
            )));
        }
        registry.entries.remove(&id);
        Ok(true)
    }

    pub(crate) async fn run_lifecycle(&self, request: LifecycleRequest) -> PluginResult<()> {
        // 保留提交时的记录引用，其他宿主并发确认报告不会使本调用丢失结果。
        let entry = self.admit_lifecycle(request).await?;
        let id = entry.id;
        let operation = Self::wait_entry(entry).await?;
        self.acknowledge_lifecycle(id)?;
        match operation.state {
            LifecycleOperationState::Completed(result) => result,
            LifecycleOperationState::Interrupted(error) => Err(error),
            LifecycleOperationState::Running => unreachable!("等待只返回终态"),
        }
    }

    pub(crate) fn ensure_lifecycle_healthy(&self) -> PluginResult<()> {
        let registry = self
            .inner
            .operations
            .lock()
            .map_err(|_| PluginError::Lifecycle("生命周期报告锁中毒".into()))?;
        match &registry.interrupted {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
}

impl RuntimeLifecycle for Kernel {
    fn submit(&self, request: LifecycleRequest) -> PluginFuture<'_, LifecycleOperationId> {
        Box::pin(self.submit_lifecycle(request))
    }

    fn operations(&self) -> PluginResult<Vec<LifecycleOperation>> {
        self.lifecycle_operations()
    }

    fn wait(&self, id: LifecycleOperationId) -> PluginFuture<'_, LifecycleOperation> {
        Box::pin(self.wait_lifecycle(id))
    }

    fn acknowledge(&self, id: LifecycleOperationId) -> PluginResult<bool> {
        self.acknowledge_lifecycle(id)
    }
}
