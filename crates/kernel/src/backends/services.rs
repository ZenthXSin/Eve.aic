use eve_plugin_api::{
    Cleanup, PluginError, PluginId, PluginResult, ServiceEntry, ServiceId, ServiceRegistry,
    ServiceValue, cleanup,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct MemoryServiceRegistry {
    entries: Mutex<HashMap<ServiceId, Registration>>,
}

struct Registration {
    token: Arc<()>,
    entry: ServiceEntry,
}

impl ServiceRegistry for MemoryServiceRegistry {
    fn provide(
        self: Arc<Self>,
        owner: PluginId,
        id: ServiceId,
        value: ServiceValue,
    ) -> PluginResult<Cleanup> {
        let token = Arc::new(());
        let entry = Registration {
            token: token.clone(),
            entry: ServiceEntry { owner, value },
        };
        let mut entries = self.entries.lock().expect("服务锁中毒");
        if entries.contains_key(&id) {
            return Err(PluginError::ServiceConflict(id));
        }
        entries.insert(id.clone(), entry);
        let backend = Arc::downgrade(&self);
        Ok(cleanup(move || async move {
            if let Some(backend) = backend.upgrade() {
                let mut entries = backend.entries.lock().expect("服务锁中毒");
                if entries
                    .get(&id)
                    .is_some_and(|current| Arc::ptr_eq(&current.token, &token))
                {
                    entries.remove(&id);
                }
            }
            Ok(())
        }))
    }

    fn get(&self, id: &ServiceId) -> PluginResult<Option<ServiceEntry>> {
        Ok(self
            .entries
            .lock()
            .expect("服务锁中毒")
            .get(id)
            .map(|entry| entry.entry.clone()))
    }
}
