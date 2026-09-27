use super::{Kernel, PluginState};
use crate::scope::PluginScope;
use eve_plugin_api::{PluginError, PluginId, PluginResult};
use std::collections::HashSet;
use std::sync::Arc;

impl Kernel {
    /// Start every registered plugin in deterministic id order.
    pub async fn start_all(&self) -> PluginResult<()> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        let mut started = Vec::new();
        for id in self.plugin_ids() {
            let mut path = HashSet::new();
            if let Err(error) = self.start_with_path(&id, &mut path, &mut started).await {
                return Err(self.rollback_started(started, error).await);
            }
        }
        Ok(())
    }

    /// Start one plugin and all of its dependencies.
    pub async fn start(&self, id: &PluginId) -> PluginResult<()> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        let mut started = Vec::new();
        let mut path = HashSet::new();
        match self.start_with_path(id, &mut path, &mut started).await {
            Ok(()) => Ok(()),
            Err(error) => Err(self.rollback_started(started, error).await),
        }
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

        {
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
                    *state = PluginState::WaitingDependencies;
                }
            }
        }

        let dependencies = slot.manifest.dependencies.clone();
        for dependency in dependencies {
            if !self.has(&dependency.id) {
                self.restore_registered(&slot);
                path.remove(id);
                return Err(PluginError::MissingDependency {
                    plugin: id.clone(),
                    dependency: dependency.id,
                });
            }
            if let Err(error) = Box::pin(self.start_with_path(&dependency.id, path, started)).await
            {
                self.restore_registered(&slot);
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
            plugin.start(self.context(&slot, scope.clone())).await
        };

        match result {
            Ok(explicit_cleanup) => {
                if let Some(cleanup) = explicit_cleanup
                    && let Err(error) = scope.register(cleanup)
                {
                    *slot.state.lock().expect("state lock poisoned") = PluginState::Failed;
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

    fn restore_registered(&self, slot: &super::PluginSlot) {
        let mut state = slot.state.lock().expect("state lock poisoned");
        if *state == PluginState::WaitingDependencies {
            *state = PluginState::Registered;
        }
    }

    fn clear_scope(&self, slot: &super::PluginSlot) {
        slot.scope.lock().expect("scope lock poisoned").take();
    }

    /// Stop every active plugin. Consumers are stopped before their providers.
    pub async fn stop_all(&self) -> PluginResult<()> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        let mut stopping = HashSet::new();
        let mut first_error = None;
        for id in self.plugin_ids() {
            if let Err(error) = self.stop_inner(&id, &mut stopping).await {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Stop one plugin and all active consumers that depend on it.
    pub async fn stop(&self, id: &PluginId) -> PluginResult<()> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.stop_inner(id, &mut HashSet::new()).await
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

        let mut first_error = None;
        for consumer in consumers {
            if let Err(error) = Box::pin(self.stop_inner(&consumer, stopping)).await {
                first_error.get_or_insert(error);
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
            return first_error.map_or(Ok(()), Err);
        }

        let scope = slot.scope.lock().expect("scope lock poisoned").take();
        let task_result = shutdown_tasks(self, id).await;
        let cleanup_result = match scope {
            Some(scope) => scope.cleanup().await,
            None => Ok(()),
        };
        *slot.state.lock().expect("state lock poisoned") = PluginState::Stopped;

        if let Err(error) = merge_cleanup_results(task_result, cleanup_result) {
            first_error.get_or_insert(error);
        }
        first_error.map_or(Ok(()), Err)
    }
}

async fn shutdown_tasks(kernel: &Kernel, id: &PluginId) -> PluginResult<()> {
    let report = kernel
        .inner
        .services
        .tasks
        .clone()
        .shutdown(id, kernel.inner.config.task_shutdown_timeout)
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
