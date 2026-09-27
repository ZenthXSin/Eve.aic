use crate::panic_boundary::contain_panic;
use eve_plugin_api::{Cleanup, PluginError, PluginResult};
use std::sync::Mutex;

#[derive(Default)]
struct ScopeState {
    open: bool,
    accepting: bool,
    cleanups: Vec<Cleanup>,
}

/// Resources registered by one plugin instance.
///
/// A scope is closed before its cleanup futures are awaited. This prevents a
/// cleanup action from registering new resources while the scope is being
/// torn down, while still allowing every already registered action to finish.
pub(crate) struct PluginScope {
    state: Mutex<ScopeState>,
}

impl PluginScope {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(ScopeState {
                open: true,
                accepting: true,
                cleanups: Vec::new(),
            }),
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        self.state.lock().expect("scope lock poisoned").open
    }

    pub(crate) fn access<F, T>(&self, operation: F) -> PluginResult<T>
    where
        F: FnOnce() -> T,
    {
        let state = self.state.lock().expect("scope lock poisoned");
        if !state.open {
            return Err(PluginError::Lifecycle("plugin scope is closed".into()));
        }
        Ok(operation())
    }

    pub(crate) fn resource<F>(&self, create: F) -> PluginResult<()>
    where
        F: FnOnce() -> PluginResult<Cleanup>,
    {
        let mut state = self.state.lock().expect("scope lock poisoned");
        if !state.open || !state.accepting {
            return Err(PluginError::Lifecycle("plugin scope is closed".into()));
        }
        state.cleanups.push(create()?);
        Ok(())
    }

    /// 与停止入口共用锁，使登记任务与截取停止集合之间没有空隙。
    pub(crate) fn admit<F, T>(&self, create: F) -> PluginResult<T>
    where
        F: FnOnce() -> PluginResult<T>,
    {
        let state = self.state.lock().expect("scope lock poisoned");
        if !state.open || !state.accepting {
            return Err(PluginError::Lifecycle(
                "插件正在停止，不能创建新资源".into(),
            ));
        }
        create()
    }

    /// 先阻止新资源；现有任务仍可读写状态完成收尾。
    pub(crate) fn begin_stop(&self) {
        self.state.lock().expect("scope lock poisoned").accepting = false;
    }

    /// 执行器意外中断时同步撤销 Context，不声称异步清理已执行。
    pub(crate) fn revoke(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.open = false;
        state.accepting = false;
    }

    pub(crate) fn register(&self, cleanup: Cleanup) -> PluginResult<()> {
        self.resource(|| Ok(cleanup))
    }

    pub(crate) fn take_cleanups(&self) -> Vec<Cleanup> {
        let mut state = self.state.lock().expect("scope lock poisoned");
        state.open = false;
        std::mem::take(&mut state.cleanups)
    }

    pub(crate) async fn cleanup(&self) -> PluginResult<()> {
        let cleanups = self.take_cleanups();
        let mut errors = Vec::new();
        for cleanup in cleanups.into_iter().rev() {
            if let Err(error) = contain_panic("资源清理", async move { cleanup().await }).await
            {
                errors.push(error.to_string());
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(PluginError::Cleanup(errors.join("; ")))
        }
    }
}
