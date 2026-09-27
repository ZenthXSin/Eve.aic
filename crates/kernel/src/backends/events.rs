use eve_plugin_api::{Cleanup, Event, EventBus, EventHandler, EventId, PluginResult, cleanup};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct SyncEventBus {
    listeners: Mutex<HashMap<EventId, Vec<Arc<Listener>>>>,
}

struct Listener {
    token: Arc<()>,
    handler: EventHandler,
}

impl EventBus for SyncEventBus {
    fn emit(&self, event: Event) -> PluginResult<()> {
        // 在锁外调用插件，允许监听器嵌套发布、订阅和写状态。
        let listeners = self
            .listeners
            .lock()
            .expect("监听器锁中毒")
            .get(&event.id)
            .cloned()
            .unwrap_or_default();
        let mut first_error = None;
        for listener in listeners {
            if let Err(error) = (listener.handler)(&event) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn subscribe(self: Arc<Self>, event: EventId, handler: EventHandler) -> PluginResult<Cleanup> {
        let token = Arc::new(());
        let listener = Arc::new(Listener {
            token: token.clone(),
            handler,
        });
        self.listeners
            .lock()
            .expect("监听器锁中毒")
            .entry(event.clone())
            .or_default()
            .push(listener.clone());
        let backend = Arc::downgrade(&self);
        Ok(cleanup(move || async move {
            if let Some(backend) = backend.upgrade() {
                let mut listeners = backend.listeners.lock().expect("监听器锁中毒");
                if let Some(entries) = listeners.get_mut(&event) {
                    entries.retain(|entry| !Arc::ptr_eq(&entry.token, &token));
                    if entries.is_empty() {
                        listeners.remove(&event);
                    }
                }
            }
            Ok(())
        }))
    }
}
