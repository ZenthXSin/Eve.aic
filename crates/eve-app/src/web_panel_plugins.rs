//! 宿主组合插件诊断、串行生命周期与页面服务，不把 Runtime 控制权交给插件。
#[cfg(test)]
mod tests;
use eve_plugin_api::{
    LifecycleOperationId, LifecycleOperationState, LifecycleRequest, PluginId, PluginState,
    RuntimeInspector, RuntimeLifecycle, ServiceId, ServiceRegistry,
};
use eve_web_panel_api::*;
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

pub(crate) struct PanelPlugins {
    inspector: Arc<dyn RuntimeInspector>,
    lifecycle: Arc<dyn RuntimeLifecycle>,
    registry: Arc<dyn ServiceRegistry>,
    permit: PageWritePermit,
    managed: BTreeSet<String>,
    instance: String,
    operations: Mutex<BTreeSet<u64>>,
    admission: tokio::sync::Mutex<()>,
}
impl PanelPlugins {
    pub(crate) fn new(
        inspector: Arc<dyn RuntimeInspector>,
        lifecycle: Arc<dyn RuntimeLifecycle>,
        registry: Arc<dyn ServiceRegistry>,
        permit: PageWritePermit,
        managed: BTreeSet<String>,
    ) -> PanelResult<Self> {
        use ring::rand::{SecureRandom, SystemRandom};
        let mut bytes = [0u8; 16];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| PanelError::Unavailable)?;
        Ok(Self {
            inspector,
            lifecycle,
            registry,
            permit,
            managed,
            instance: bytes.iter().map(|b| format!("{b:02x}")).collect(),
            operations: Mutex::new(BTreeSet::new()),
            admission: tokio::sync::Mutex::new(()),
        })
    }
    pub(crate) fn plugins(&self) -> PanelResult<PluginList> {
        let statuses = self
            .inspector
            .plugins()
            .map_err(|_| PanelError::Unavailable)?;
        if statuses.len() > 256 {
            return Err(PanelError::Unavailable);
        }
        let mut items = Vec::new();
        for status in &statuses {
            let manifest = self
                .inspector
                .plugin_manifest(&status.info.id)
                .map_err(|_| PanelError::Unavailable)?;
            let managed = self.managed.contains(status.info.id.as_str());
            // 停止会级联停止依赖者；不能经可管理插件停止宿主固定的插件。
            let mut affected = BTreeSet::from([status.info.id.clone()]);
            loop {
                let before = affected.len();
                for candidate in &statuses {
                    let m = self
                        .inspector
                        .plugin_manifest(&candidate.info.id)
                        .map_err(|_| PanelError::Unavailable)?;
                    if m.dependencies.iter().any(|d| affected.contains(&d.id)) {
                        affected.insert(candidate.info.id.clone());
                    }
                }
                if affected.len() == before {
                    break;
                }
            }
            let protected = affected
                .iter()
                .any(|id| !self.managed.contains(id.as_str()));
            items.push(PluginView {
                id: status.info.id.to_string(),
                version: status.info.version.to_string(),
                state: format!("{:?}", status.state),
                dependencies: manifest
                    .dependencies
                    .iter()
                    .map(|d| d.id.to_string())
                    .collect(),
                permissions: manifest
                    .permissions
                    .iter()
                    .map(|p| p.as_str().into())
                    .collect(),
                can_start: managed
                    && matches!(status.state, PluginState::Registered | PluginState::Stopped),
                can_stop: managed && !protected && status.state == PluginState::Active,
                reason: if !managed {
                    Some("host_bound")
                } else if protected {
                    Some("protected_dependency")
                } else if status.state == PluginState::Failed {
                    Some("failed")
                } else {
                    None
                },
            });
        }
        Ok(PluginList {
            instance: self.instance.clone(),
            items,
        })
    }
    pub(crate) async fn action(&self, request: PluginAction) -> PanelResult<OperationReceipt> {
        if !valid_panel_id(&request.plugin_id) {
            return Err(PanelError::InvalidInput);
        }
        let _admission = self.admission.lock().await;
        let list = self.plugins()?;
        if request.instance != list.instance {
            return Err(PanelError::Stale);
        }
        let target = list
            .items
            .into_iter()
            .find(|p| p.id == request.plugin_id)
            .ok_or(PanelError::NotFound)?;
        if request.expected_state != target.state {
            return Err(PanelError::Stale);
        }
        let allowed = match request.action {
            PluginActionKind::Start => target.can_start,
            PluginActionKind::Stop => target.can_stop,
        };
        if !allowed {
            return Err(PanelError::Forbidden);
        }
        let id = PluginId::new(request.plugin_id).map_err(|_| PanelError::InvalidInput)?;
        let operation = match request.action {
            PluginActionKind::Start => LifecycleRequest::Start(id),
            PluginActionKind::Stop => LifecycleRequest::Stop(id),
        };
        let id = self
            .lifecycle
            .submit(operation)
            .await
            .map_err(|_| PanelError::Unavailable)?
            .get();
        self.operations
            .lock()
            .map_err(|_| PanelError::Unavailable)?
            .insert(id);
        Ok(OperationReceipt { id })
    }
    pub(crate) fn operations(&self) -> PanelResult<Vec<PluginOperation>> {
        let records = self
            .lifecycle
            .operations()
            .map_err(|_| PanelError::Unavailable)?;
        let owned = self
            .operations
            .lock()
            .map_err(|_| PanelError::Unavailable)?;
        Ok(records
            .into_iter()
            .filter(|r| owned.contains(&r.id.get()))
            .filter_map(|record| {
                let (plugin_id, action) = match record.request {
                    LifecycleRequest::Start(id) => (id.to_string(), PluginActionKind::Start),
                    LifecycleRequest::Stop(id) => (id.to_string(), PluginActionKind::Stop),
                    _ => return None,
                };
                let state = match record.state {
                    LifecycleOperationState::Running => "running",
                    LifecycleOperationState::Completed(Ok(())) => "completed",
                    LifecycleOperationState::Completed(Err(_)) => "failed",
                    LifecycleOperationState::Interrupted(_) => "interrupted",
                };
                Some(PluginOperation {
                    id: record.id.get(),
                    plugin_id,
                    action,
                    state,
                })
            })
            .collect())
    }
    pub(crate) async fn acknowledge(&self, id: u64) -> PanelResult<bool> {
        if !self
            .operations
            .lock()
            .map_err(|_| PanelError::Unavailable)?
            .contains(&id)
        {
            return Err(PanelError::NotFound);
        }
        let removed = self
            .lifecycle
            .acknowledge(LifecycleOperationId::new(id))
            .map_err(|_| PanelError::Stale)?;
        self.operations
            .lock()
            .map_err(|_| PanelError::Unavailable)?
            .remove(&id);
        Ok(removed)
    }
    fn provider(&self, plugin: &str) -> PanelResult<Arc<PluginPagesHandle>> {
        if !valid_panel_id(plugin) {
            return Err(PanelError::InvalidInput);
        }
        let id = ServiceId::new(page_service_id(plugin)).map_err(|_| PanelError::InvalidInput)?;
        let entry = self
            .registry
            .get(&id)
            .map_err(|_| PanelError::Unavailable)?
            .ok_or(PanelError::NotFound)?;
        if entry.owner.as_str() != plugin {
            return Err(PanelError::Forbidden);
        }
        entry
            .value
            .downcast::<PluginPagesHandle>()
            .map_err(|_| PanelError::Unavailable)
    }
    pub(crate) fn pages(&self) -> PanelResult<Vec<PluginPageLink>> {
        let plugins = self
            .inspector
            .plugins()
            .map_err(|_| PanelError::Unavailable)?;
        if plugins.len() > 256 {
            return Err(PanelError::Unavailable);
        }
        let mut pages = Vec::new();
        for plugin in plugins {
            let provider = match self.provider(plugin.info.id.as_str()) {
                Ok(p) => p,
                Err(PanelError::NotFound) => continue,
                Err(e) => return Err(e),
            };
            let mut seen = BTreeSet::new();
            for page in provider.0.pages()? {
                if !valid_panel_id(&page.id)
                    || page.title.is_empty()
                    || page.title.len() > 256
                    || page.description.len() > 4096
                    || !seen.insert(page.id.clone())
                    || pages.len() >= MAX_PLUGIN_PAGES
                {
                    return Err(PanelError::Unavailable);
                }
                pages.push(PluginPageLink {
                    plugin_id: plugin.info.id.to_string(),
                    page,
                });
            }
        }
        Ok(pages)
    }
    pub(crate) fn read(&self, plugin: &str, page: &str) -> PanelResult<PluginPage> {
        if !valid_panel_id(page) {
            return Err(PanelError::InvalidInput);
        }
        let result = self.provider(plugin)?.0.read(page)?;
        result.validate()?;
        if result.descriptor.id != page {
            return Err(PanelError::Unavailable);
        }
        Ok(result)
    }
    pub(crate) fn save(&self, request: PageSaveRequest) -> PanelResult<PageSaved> {
        if !valid_panel_id(&request.plugin_id)
            || !valid_panel_id(&request.page_id)
            || request.instance.is_empty()
            || request.instance.len() > 256
            || request.values.len() > MAX_PAGE_FIELDS
        {
            return Err(PanelError::InvalidInput);
        }
        self.provider(&request.plugin_id)?
            .0
            .save(&request, &self.permit)
    }
}

/// 宿主明确停止分段设置插件时，之后的回复整条发送；失败/停止中的实例仍明确报错。
pub(crate) struct ManagedSegmentPreferences {
    pub inspector: std::sync::Weak<dyn RuntimeInspector>,
    pub inner: Arc<dyn eve_segment_api::SegmentPreferences>,
}
impl eve_segment_api::SegmentPreferences for ManagedSegmentPreferences {
    fn get(
        &self,
        scope: &eve_segment_api::SegmentScope,
    ) -> eve_segment_api::SegmentPreferenceResult<eve_segment_api::SegmentPreference> {
        use eve_segment_api::{SegmentPreference, SegmentPreferenceError};
        let inspector = self
            .inspector
            .upgrade()
            .ok_or(SegmentPreferenceError::Unavailable)?;
        let plugins = inspector
            .plugins()
            .map_err(|_| SegmentPreferenceError::Unavailable)?;
        let state = plugins
            .into_iter()
            .find(|p| p.info.id.as_str() == eve_segment_plugin::SEGMENT_PREFERENCES_PLUGIN_ID)
            .ok_or(SegmentPreferenceError::Unavailable)?
            .state;
        match state {
            PluginState::Stopped | PluginState::Registered => Ok(SegmentPreference {
                enabled: Some(false),
                ..Default::default()
            }),
            PluginState::Active => self.inner.get(scope),
            _ => Err(SegmentPreferenceError::Unavailable),
        }
    }
    fn update(
        &self,
        scope: &eve_segment_api::SegmentScope,
        change: eve_segment_api::SegmentChange,
    ) -> eve_segment_api::SegmentPreferenceResult<eve_segment_api::SegmentPreference> {
        self.inner.update(scope, change)
    }
}
