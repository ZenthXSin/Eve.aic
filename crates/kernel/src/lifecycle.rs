use super::{Kernel, PluginSlot, PluginState};
use crate::panic_boundary::contain_panic;
use crate::scope::PluginScope;
use eve_plugin_api::{
    LifecycleRequest, PluginDependency, PluginError, PluginId, PluginResult, PluginStopError,
    StopStage,
};
use std::collections::HashSet;
use std::sync::Arc;

enum StartCheck {
    Enter(PluginId),
    Dependency {
        owner: PluginId,
        dependency: PluginDependency,
    },
    Leave(PluginId),
}

impl Kernel {
    /// 预检全部根后按 ID 顺序启动；准入后取消等待不取消操作，结果保留供查询。
    pub async fn start_all(&self) -> PluginResult<()> {
        self.run_lifecycle(LifecycleRequest::StartAll).await
    }

    /// 启动插件及依赖；正常返回时自动确认报告，取消等待时保留报告。
    pub async fn start(&self, id: &PluginId) -> PluginResult<()> {
        self.run_lifecycle(LifecycleRequest::Start(id.clone()))
            .await
    }

    pub(crate) async fn execute_lifecycle(&self, request: LifecycleRequest) -> PluginResult<()> {
        // 只从持有 owned 准入锁的 worker 调用，递归依赖不能再次提交操作。
        match request {
            LifecycleRequest::Start(id) => self.start_inner(&id).await,
            LifecycleRequest::StartAll => self.start_all_inner().await,
            LifecycleRequest::Stop(id) => self.stop_inner(&id, &mut HashSet::new()).await,
            LifecycleRequest::StopAll => self.stop_all_inner().await,
        }
    }

    async fn start_all_inner(&self) -> PluginResult<()> {
        let ids = self.plugin_ids();
        self.preflight_start(&ids)?;
        let mut started = Vec::new();
        for id in ids {
            let mut path = HashSet::new();
            if let Err(error) = self.start_with_path(&id, &mut path, &mut started).await {
                return Err(self.rollback_started(started, error).await);
            }
        }
        Ok(())
    }

    async fn start_inner(&self, id: &PluginId) -> PluginResult<()> {
        self.preflight_start(std::slice::from_ref(id))?;
        let mut started = Vec::new();
        let mut path = HashSet::new();
        match self.start_with_path(id, &mut path, &mut started).await {
            Ok(()) => Ok(()),
            Err(error) => Err(self.rollback_started(started, error).await),
        }
    }

    /// 与实际启动共用准入锁；整个请求通过之前不改状态、不调用插件代码。
    fn preflight_start(&self, roots: &[PluginId]) -> PluginResult<()> {
        let mut checked = HashSet::new();
        let mut path = HashSet::new();
        // 显式栈保留根节点及清单依赖顺序，避免深层图消耗同步调用栈。
        let mut pending = roots
            .iter()
            .rev()
            .cloned()
            .map(StartCheck::Enter)
            .collect::<Vec<_>>();
        while let Some(check) = pending.pop() {
            match check {
                StartCheck::Enter(id) => {
                    if checked.contains(&id) {
                        continue;
                    }
                    if !path.insert(id.clone()) {
                        return Err(PluginError::DependencyCycle(id));
                    }
                    let slot = self.slot(&id)?;
                    let state = *slot.state.lock().expect("state lock poisoned");
                    match state {
                        PluginState::Active => {
                            path.remove(&id);
                            checked.insert(id);
                            continue;
                        }
                        PluginState::Starting | PluginState::Stopping => {
                            return Err(PluginError::InvalidLifecycle {
                                plugin: id,
                                state: format!("{state:?}"),
                            });
                        }
                        PluginState::Failed => {
                            return Err(PluginError::InvalidLifecycle {
                                plugin: id,
                                state: "Failed; restart is not automatic".into(),
                            });
                        }
                        PluginState::Registered
                        | PluginState::WaitingDependencies
                        | PluginState::Stopped => {}
                    }
                    pending.push(StartCheck::Leave(id.clone()));
                    for dependency in slot.manifest.dependencies.iter().rev() {
                        pending.push(StartCheck::Dependency {
                            owner: id.clone(),
                            dependency: dependency.clone(),
                        });
                    }
                }
                StartCheck::Dependency { owner, dependency } => {
                    // 每条边的要求都要检查，不能因共享节点已访问或 Active 而跳过。
                    self.checked_dependency(&owner, &dependency)?;
                    pending.push(StartCheck::Enter(dependency.id));
                }
                StartCheck::Leave(id) => {
                    path.remove(&id);
                    checked.insert(id);
                }
            }
        }
        Ok(())
    }

    fn checked_dependency(
        &self,
        owner: &PluginId,
        dependency: &PluginDependency,
    ) -> PluginResult<Arc<PluginSlot>> {
        let slot = self
            .slot(&dependency.id)
            .map_err(|_| PluginError::MissingDependency {
                plugin: owner.clone(),
                dependency: dependency.id.clone(),
            })?;
        if let Some(requirement) = &dependency.requirement
            && !slot.manifest.version.matches_requirement(requirement)?
        {
            return Err(PluginError::DependencyVersionMismatch {
                plugin: owner.clone(),
                dependency: dependency.id.clone(),
                requirement: requirement.clone(),
                found: slot.manifest.version.clone(),
            });
        }
        Ok(slot)
    }

    async fn start_with_path(
        &self,
        id: &PluginId,
        path: &mut HashSet<PluginId>,
        started: &mut Vec<PluginId>,
    ) -> PluginResult<()> {
        if !path.insert(id.clone()) {
            return Err(PluginError::DependencyCycle(id.clone()));
        }

        let slot = match self.slot(id) {
            Ok(slot) => slot,
            Err(error) => {
                path.remove(id);
                return Err(error);
            }
        };

        let previous_state = {
            let mut state = slot.state.lock().expect("state lock poisoned");
            match *state {
                PluginState::Active => {
                    path.remove(id);
                    return Ok(());
                }
                PluginState::Starting | PluginState::Stopping => {
                    path.remove(id);
                    return Err(PluginError::InvalidLifecycle {
                        plugin: id.clone(),
                        state: format!("{:?}", *state),
                    });
                }
                PluginState::Failed => {
                    path.remove(id);
                    return Err(PluginError::InvalidLifecycle {
                        plugin: id.clone(),
                        state: "Failed; restart is not automatic".into(),
                    });
                }
                PluginState::Registered
                | PluginState::WaitingDependencies
                | PluginState::Stopped => {
                    let previous = *state;
                    *state = PluginState::WaitingDependencies;
                    previous
                }
            }
        };

        let dependencies = slot.manifest.dependencies.clone();
        for dependency in dependencies {
            let dependency_slot = match self.checked_dependency(id, &dependency) {
                Ok(slot) => slot,
                Err(error) => {
                    self.restore_waiting_state(&slot, previous_state);
                    path.remove(id);
                    return Err(error);
                }
            };
            if let Err(error) =
                Box::pin(self.start_with_path(&dependency_slot.manifest.id, path, started)).await
            {
                self.restore_waiting_state(&slot, previous_state);
                path.remove(id);
                return Err(error);
            }
        }
        path.remove(id);

        *slot.state.lock().expect("state lock poisoned") = PluginState::Starting;
        let scope = Arc::new(PluginScope::new());
        *slot.scope.lock().expect("scope lock poisoned") = Some(scope.clone());

        let result = {
            let mut plugin = slot.plugin.lock().await;
            contain_panic("插件启动", async {
                plugin.start(self.context(&slot, scope.clone())).await
            })
            .await
        };

        match result {
            Ok(explicit_cleanup) => {
                if let Some(cleanup) = explicit_cleanup
                    && let Err(error) = scope.register(cleanup)
                {
                    *slot.state.lock().expect("state lock poisoned") = PluginState::Failed;
                    scope.begin_stop();
                    let task_result = shutdown_tasks(self, id).await;
                    let cleanup_result = scope.cleanup().await;
                    self.clear_scope(&slot);
                    return Err(PluginError::PluginFailed {
                        plugin: id.clone(),
                        message: format_failure(
                            error,
                            merge_cleanup_results(task_result, cleanup_result),
                        ),
                    });
                }
                *slot.state.lock().expect("state lock poisoned") = PluginState::Active;
                started.push(id.clone());
                Ok(())
            }
            Err(error) => {
                *slot.state.lock().expect("state lock poisoned") = PluginState::Failed;
                scope.begin_stop();
                let task_result = shutdown_tasks(self, id).await;
                let cleanup_result = scope.cleanup().await;
                self.clear_scope(&slot);
                Err(PluginError::PluginFailed {
                    plugin: id.clone(),
                    message: format_failure(
                        error,
                        merge_cleanup_results(task_result, cleanup_result),
                    ),
                })
            }
        }
    }

    async fn rollback_started(&self, started: Vec<PluginId>, cause: PluginError) -> PluginError {
        let mut stopping = HashSet::new();
        let mut errors = Vec::new();
        for id in started.into_iter().rev() {
            if let Err(error) = self.stop_inner(&id, &mut stopping).await {
                errors.push(error);
            }
        }
        if errors.is_empty() {
            cause
        } else {
            PluginError::Rollback {
                cause: Box::new(cause),
                errors,
            }
        }
    }

    fn restore_waiting_state(&self, slot: &super::PluginSlot, previous: PluginState) {
        let mut state = slot.state.lock().expect("state lock poisoned");
        if *state == PluginState::WaitingDependencies {
            *state = previous;
        }
    }

    fn clear_scope(&self, slot: &super::PluginSlot) {
        slot.scope.lock().expect("scope lock poisoned").take();
    }

    /// 消费者先于提供者停止；准入后即使调用方取消等待，也继续执行收尾。
    pub async fn stop_all(&self) -> PluginResult<()> {
        self.run_lifecycle(LifecycleRequest::StopAll).await
    }

    async fn stop_all_inner(&self) -> PluginResult<()> {
        let mut stopping = HashSet::new();
        let mut errors = Vec::new();
        for id in self.plugin_ids() {
            if let Err(error) = self.stop_inner(&id, &mut stopping).await {
                append_stop_error(&mut errors, &id, error);
            }
        }
        stopped_result(errors)
    }

    /// 停止一个插件及其消费者；正常返回自动确认报告，取消等待不丢收尾结果。
    pub async fn stop(&self, id: &PluginId) -> PluginResult<()> {
        self.run_lifecycle(LifecycleRequest::Stop(id.clone())).await
    }

    async fn stop_inner(
        &self,
        id: &PluginId,
        stopping: &mut HashSet<PluginId>,
    ) -> PluginResult<()> {
        if !stopping.insert(id.clone()) {
            return Ok(());
        }

        let slot = self.slot(id)?;
        let mut consumers = self
            .inner
            .plugins
            .lock()
            .expect("plugin lock poisoned")
            .iter()
            .filter_map(|(candidate_id, candidate)| {
                let active =
                    *candidate.state.lock().expect("state lock poisoned") == PluginState::Active;
                let depends = candidate
                    .manifest
                    .dependencies
                    .iter()
                    .any(|dependency| dependency.id == *id);
                active
                    .then_some(depends.then(|| candidate_id.clone()))
                    .flatten()
            })
            .collect::<Vec<_>>();
        consumers.sort();

        let mut errors = Vec::new();
        for consumer in consumers {
            if let Err(error) = Box::pin(self.stop_inner(&consumer, stopping)).await {
                append_stop_error(&mut errors, &consumer, error);
            }
        }

        let active = {
            let mut state = slot.state.lock().expect("state lock poisoned");
            if *state != PluginState::Active {
                false
            } else {
                *state = PluginState::Stopping;
                true
            }
        };
        if !active {
            return stopped_result(errors);
        }

        // 保留 Scope 到收尾结束，执行器意外中断时仍能同步撤销 Context。
        let scope = slot.scope.lock().expect("scope lock poisoned").clone();
        if let Some(scope) = &scope {
            scope.begin_stop();
        }
        let task_result = shutdown_tasks(self, id).await;
        let cleanup_result = match scope {
            Some(scope) => scope.cleanup().await,
            None => Ok(()),
        };
        self.clear_scope(&slot);
        for (stage, result) in [
            (StopStage::Tasks, task_result),
            (StopStage::Cleanup, cleanup_result),
        ] {
            if let Err(error) = result {
                errors.push(PluginStopError {
                    plugin: id.clone(),
                    stage,
                    error: Box::new(error),
                });
            }
        }
        // 未确认退出的任务可能仍在操作外部资源，不能把插件标为可安全重启。
        let exited = match self.inner.services.tasks.list(id) {
            Ok(tasks) if tasks.iter().all(|task| task.exited) => true,
            result => {
                let error = match result {
                    Err(error) => error,
                    Ok(tasks) => PluginError::Task(format!(
                        "退出确认仍有未结束任务：{}",
                        tasks
                            .iter()
                            .filter(|task| !task.exited)
                            .map(|task| task.id.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                };
                errors.push(PluginStopError {
                    plugin: id.clone(),
                    stage: StopStage::Inspection,
                    error: Box::new(error),
                });
                false
            }
        };
        *slot.state.lock().expect("state lock poisoned") = if exited {
            PluginState::Stopped
        } else {
            PluginState::Failed
        };

        stopped_result(errors)
    }
}

fn append_stop_error(errors: &mut Vec<PluginStopError>, plugin: &PluginId, error: PluginError) {
    match error {
        // 子插件错误保留原始归属，不重复包装成提供者的错误。
        PluginError::Shutdown(nested) => errors.extend(nested),
        error => errors.push(PluginStopError {
            plugin: plugin.clone(),
            stage: StopStage::Lifecycle,
            error: Box::new(error),
        }),
    }
}

fn stopped_result(errors: Vec<PluginStopError>) -> PluginResult<()> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(PluginError::Shutdown(errors))
    }
}

async fn shutdown_tasks(kernel: &Kernel, id: &PluginId) -> PluginResult<()> {
    let report = kernel
        .inner
        .services
        .tasks
        .clone()
        .shutdown(
            id,
            kernel.inner.config.task_shutdown_timeout,
            kernel.inner.config.task_abort_timeout,
        )
        .await?;
    if report.is_clean() {
        Ok(())
    } else {
        let mut details = Vec::new();
        if !report.timed_out.is_empty() {
            details.push(format!(
                "超时任务：{}",
                report
                    .timed_out
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !report.unfinished.is_empty() {
            details.push(format!(
                "尚未确认退出：{}",
                report
                    .unfinished
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        details.extend(report.errors);
        Err(PluginError::Task(details.join("；")))
    }
}

fn merge_cleanup_results(first: PluginResult<()>, second: PluginResult<()>) -> PluginResult<()> {
    let mut errors = Vec::new();
    if let Err(error) = first {
        errors.push(error.to_string());
    }
    if let Err(error) = second {
        errors.push(error.to_string());
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(PluginError::Cleanup(errors.join("；")))
    }
}

fn format_failure(error: PluginError, cleanup_result: PluginResult<()>) -> String {
    match cleanup_result {
        Ok(()) => error.to_string(),
        Err(cleanup_error) => format!("{error}; {cleanup_error}"),
    }
}
