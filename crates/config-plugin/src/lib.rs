//! 普通配置的内置插件实现；密钥库、2FA 与凭据代理在后续切片交付。

mod store;

use eve_config_api::*;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    ServiceId, cleanup,
};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use store::FileConfigService;

/// 组合层仅指定存储位置、Schema 与可选测试环境，不解析业务配置。
pub struct ConfigBootstrap {
    pub directory: PathBuf,
    pub schemas: Vec<ConfigSchema>,
    pub environment: Option<BTreeMap<String, String>>,
}

impl ConfigBootstrap {
    pub fn new(directory: impl Into<PathBuf>, schemas: Vec<ConfigSchema>) -> Self {
        Self {
            directory: directory.into(),
            schemas,
            environment: None,
        }
    }
    pub fn with_environment(mut self, environment: BTreeMap<String, String>) -> Self {
        self.environment = Some(environment);
        self
    }
}

/// 宿主专用管理句柄；普通消费者只获得 ConfigServiceHandle。
#[derive(Clone, Default)]
pub struct ConfigController {
    inner: Arc<Mutex<Option<Arc<FileConfigService>>>>,
}

impl ConfigController {
    fn service(&self) -> ConfigResult<Arc<FileConfigService>> {
        self.inner
            .lock()
            .map_err(|_| ConfigError::Unavailable)?
            .clone()
            .ok_or(ConfigError::Unavailable)
    }
}

impl ConfigAdmin for ConfigController {
    fn current(&self) -> ConfigResult<ConfigDocument> {
        self.service()?.current()
    }
    fn replace(
        &self,
        expected_revision: u64,
        overrides: ConfigOverrides,
        mode: ApplyMode,
    ) -> ConfigResult<ConfigChange> {
        self.service()?.replace(expected_revision, overrides, mode)
    }
    fn backups(&self) -> ConfigResult<Vec<ConfigBackup>> {
        self.service()?.backups()
    }
    fn pin_backup(&self, revision: u64, pinned: bool) -> ConfigResult<()> {
        self.service()?.pin_backup(revision, pinned)
    }
    fn rollback(
        &self,
        expected_revision: u64,
        revision: u64,
        mode: ApplyMode,
    ) -> ConfigResult<ConfigChange> {
        self.service()?.rollback(expected_revision, revision, mode)
    }
}

pub struct ConfigPlugin {
    manifest: PluginManifest,
    bootstrap: ConfigBootstrap,
    controller: ConfigController,
}

impl ConfigPlugin {
    pub fn new(bootstrap: ConfigBootstrap) -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(CONFIG_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            bootstrap,
            controller: ConfigController::default(),
        })
    }
    pub fn controller(&self) -> ConfigController {
        self.controller.clone()
    }
}

impl Plugin for ConfigPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let service = Arc::new(FileConfigService::open(&self.bootstrap).map_err(plugin_error)?);
            let to_close = service.clone();
            let controller = self.controller.clone();
            ctx.cleanup(cleanup(move || async move {
                to_close.close().map_err(plugin_error)?;
                let mut slot = controller
                    .inner
                    .lock()
                    .map_err(|_| plugin_error(ConfigError::Unavailable))?;
                if slot
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &to_close))
                {
                    *slot = None;
                }
                Ok(())
            }))?;
            ctx.provide_service(
                ServiceId::new(CONFIG_SERVICE_ID)?,
                ConfigServiceHandle(service.clone()),
            )?;
            *self
                .controller
                .inner
                .lock()
                .map_err(|_| plugin_error(ConfigError::Unavailable))? = Some(service);
            Ok(None)
        })
    }
}

fn plugin_error(error: ConfigError) -> PluginError {
    PluginError::State(error.to_string())
}
