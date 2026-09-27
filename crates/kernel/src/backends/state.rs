use eve_plugin_api::{PluginId, PluginResult, StateStore};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct MemoryStateStore {
    entries: Mutex<HashMap<PluginId, HashMap<String, Vec<u8>>>>,
}

impl StateStore for MemoryStateStore {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        Ok(self
            .entries
            .lock()
            .expect("状态锁中毒")
            .get(namespace)
            .and_then(|entries| entries.get(key))
            .cloned())
    }

    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        self.entries
            .lock()
            .expect("状态锁中毒")
            .entry(namespace.clone())
            .or_default()
            .insert(key, value);
        Ok(())
    }
}
