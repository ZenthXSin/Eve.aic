//! 独立配置账本。页面只适配公开数据；后台通过只读契约订阅持久提交。
use crate::strict_json;
use eve_interest_api::*;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    ServiceId, cleanup,
};
use eve_web_panel_api::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

const PAGE: &str = "learning";
const MAX_BYTES: usize = 4096;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Overrides {
    enabled: Option<bool>,
    cooldown_ms: Option<u64>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    format_version: u32,
    revision: u64,
    paused_at_revision: u64,
    overrides: Overrides,
}
struct Inner {
    context: Option<PluginContext>,
    document: Document,
}
struct Settings {
    inner: Mutex<Inner>,
    defaults: InterestLearningSettings,
    instance: String,
    changes: watch::Sender<Option<InterestSettingsSnapshot>>,
}
impl Settings {
    fn open(context: PluginContext, defaults: InterestLearningSettings) -> InterestResult<Self> {
        defaults.validate()?;
        let document = match context
            .state_get(INTEREST_SETTINGS_STATE_KEY)
            .map_err(|_| InterestError::Storage)?
        {
            None => Document {
                format_version: 1,
                revision: 0,
                paused_at_revision: 0,
                overrides: Overrides::default(),
            },
            Some(bytes) => {
                if bytes.len() > MAX_BYTES {
                    return Err(InterestError::CorruptState);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| InterestError::CorruptState)?;
                if value.get("format_version").and_then(Value::as_u64) != Some(1) {
                    return Err(if value.get("format_version").is_some() {
                        InterestError::UnsupportedVersion
                    } else {
                        InterestError::CorruptState
                    });
                }
                let document: Document =
                    serde_json::from_value(value).map_err(|_| InterestError::CorruptState)?;
                if document.paused_at_revision > document.revision {
                    return Err(InterestError::CorruptState);
                }
                document
            }
        };
        let mut entropy = [0u8; 16];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut entropy)
            .map_err(|_| InterestError::Unavailable)?;
        let instance = entropy.iter().map(|byte| format!("{byte:02x}")).collect();
        let snapshot =
            Self::resolved(&defaults, &document).map_err(|_| InterestError::CorruptState)?;
        let (changes, _) = watch::channel(Some(snapshot));
        Ok(Self {
            inner: Mutex::new(Inner {
                context: Some(context),
                document,
            }),
            defaults,
            instance,
            changes,
        })
    }
    fn resolved(
        defaults: &InterestLearningSettings,
        document: &Document,
    ) -> InterestResult<InterestSettingsSnapshot> {
        let mut settings = defaults.clone();
        if let Some(enabled) = document.overrides.enabled {
            settings.enabled = enabled;
        }
        if let Some(cooldown_ms) = document.overrides.cooldown_ms {
            settings.observation.cooldown_ms = cooldown_ms;
        }
        settings.validate()?;
        Ok(InterestSettingsSnapshot {
            revision: document.revision,
            paused_at_revision: document.paused_at_revision,
            settings,
        })
    }
    fn snapshot(&self) -> InterestResult<InterestSettingsSnapshot> {
        let inner = self.inner.lock().map_err(|_| InterestError::Unavailable)?;
        if inner.context.is_none() {
            return Err(InterestError::Unavailable);
        }
        Self::resolved(&self.defaults, &inner.document)
    }
    fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("兴趣配置不可用".into()))?
            .context = None;
        self.changes.send_replace(None);
        Ok(())
    }
}

#[derive(Clone, Default)]
pub struct InterestSettingsController {
    active: Arc<Mutex<Option<Arc<Settings>>>>,
}
impl InterestSettingsController {
    fn service(&self) -> InterestResult<Arc<Settings>> {
        self.active
            .lock()
            .map_err(|_| InterestError::Unavailable)?
            .clone()
            .ok_or(InterestError::Unavailable)
    }
}
impl InterestSettingsReader for InterestSettingsController {
    fn snapshot(&self) -> InterestResult<InterestSettingsSnapshot> {
        self.service()?.snapshot()
    }
    fn changed(&self, revision: u64) -> InterestFuture<'_, InterestSettingsSnapshot> {
        Box::pin(async move {
            let service = self.service()?;
            let mut receiver = service.changes.subscribe();
            loop {
                let snapshot = receiver
                    .borrow_and_update()
                    .clone()
                    .ok_or(InterestError::Unavailable)?;
                if snapshot.revision != revision {
                    return Ok(snapshot);
                }
                receiver
                    .changed()
                    .await
                    .map_err(|_| InterestError::Unavailable)?;
            }
        })
    }
}

pub struct InterestSettingsPlugin {
    manifest: PluginManifest,
    defaults: InterestLearningSettings,
    controller: InterestSettingsController,
    permit: Option<PageWritePermit>,
}
impl InterestSettingsPlugin {
    pub fn new(defaults: InterestLearningSettings) -> PluginResult<Self> {
        defaults
            .validate()
            .map_err(|error| PluginError::State(error.to_string()))?;
        Ok(Self {
            manifest: PluginManifest::new(INTEREST_SETTINGS_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?,
            defaults,
            controller: InterestSettingsController::default(),
            permit: None,
        })
    }
    pub fn controller(&self) -> InterestSettingsController {
        self.controller.clone()
    }
    pub fn with_web_pages(mut self, permit: PageWritePermit) -> Self {
        self.permit = Some(permit);
        self
    }
}
impl Plugin for InterestSettingsPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let service = Arc::new(
                Settings::open(context.clone(), self.defaults.clone())
                    .map_err(|error| PluginError::State(error.to_string()))?,
            );
            let to_close = service.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("兴趣配置不可用".into()))?;
                if active
                    .as_ref()
                    .is_some_and(|value| Arc::ptr_eq(value, &to_close))
                {
                    *active = None;
                }
                Ok(())
            }))?;
            if let Some(permit) = &self.permit {
                context.provide_service(
                    ServiceId::new(page_service_id(INTEREST_SETTINGS_PLUGIN_ID))?,
                    PluginPagesHandle(Arc::new(SettingsPages {
                        service: service.clone(),
                        permit: permit.clone(),
                    })),
                )?;
            }
            *self
                .controller
                .active
                .lock()
                .map_err(|_| PluginError::State("兴趣配置不可用".into()))? = Some(service);
            Ok(None)
        })
    }
}

fn descriptor() -> PageDescriptor {
    PageDescriptor { id: PAGE.into(), title: "兴趣学习".into(), description: "从已送达的聊天观察兴趣并派生学习目标。保存即时生效，无需重启 QQ；暂停会终结在途观察且保留历史，恢复后处理尚未观察的交互。已有研究和实践任务仍按各自配置推进。".into() }
}
struct SettingsPages {
    service: Arc<Settings>,
    permit: PageWritePermit,
}
impl PluginPages for SettingsPages {
    fn pages(&self) -> PanelResult<Vec<PageDescriptor>> {
        self.service
            .snapshot()
            .map_err(|_| PanelError::Unavailable)?;
        Ok(vec![descriptor()])
    }
    fn read(&self, page: &str) -> PanelResult<PluginPage> {
        if page != PAGE {
            return Err(PanelError::NotFound);
        }
        let inner = self
            .service
            .inner
            .lock()
            .map_err(|_| PanelError::Unavailable)?;
        if inner.context.is_none() {
            return Err(PanelError::Unavailable);
        }
        let snapshot = Settings::resolved(&self.service.defaults, &inner.document)
            .map_err(|_| PanelError::Unavailable)?;
        let make =
            |id: &str, label: &str, kind, value: Value, override_value: Option<Value>| PageField {
                id: id.into(),
                label: label.into(),
                kind,
                value: Some(value),
                source: if override_value.is_some() {
                    "override"
                } else {
                    "default"
                },
                override_value,
                restart_required: false,
            };
        Ok(PluginPage {
            descriptor: descriptor(),
            instance: self.service.instance.clone(),
            revision: snapshot.revision,
            fields: vec![
                make(
                    "enabled",
                    "启用兴趣学习",
                    PageFieldKind::Boolean,
                    snapshot.settings.enabled.into(),
                    inner.document.overrides.enabled.map(Value::from),
                ),
                make(
                    "cooldown_ms",
                    "观察间隔（毫秒）",
                    PageFieldKind::Integer {
                        minimum: Some(0),
                        maximum: Some(86_400_000),
                    },
                    snapshot.settings.observation.cooldown_ms.into(),
                    inner.document.overrides.cooldown_ms.map(Value::from),
                ),
            ],
        })
    }
    fn save(&self, request: &PageSaveRequest, permit: &PageWritePermit) -> PanelResult<PageSaved> {
        if !self.permit.same_grant(permit) {
            return Err(PanelError::Forbidden);
        }
        if request.plugin_id != INTEREST_SETTINGS_PLUGIN_ID || request.page_id != PAGE {
            return Err(PanelError::NotFound);
        }
        let mut inner = self
            .service
            .inner
            .lock()
            .map_err(|_| PanelError::Unavailable)?;
        if inner.context.is_none() {
            return Err(PanelError::Unavailable);
        }
        if request.instance != self.service.instance
            || request.expected_revision != inner.document.revision
        {
            return Err(PanelError::Stale);
        }
        let mut next = inner.document.clone();
        for (key, value) in &request.values {
            match key.as_str() {
                "enabled" => {
                    next.overrides.enabled = value
                        .as_ref()
                        .map(|v| v.as_bool().ok_or(PanelError::InvalidInput))
                        .transpose()?
                }
                "cooldown_ms" => {
                    next.overrides.cooldown_ms = value
                        .as_ref()
                        .map(|v| v.as_u64().ok_or(PanelError::InvalidInput))
                        .transpose()?
                }
                _ => return Err(PanelError::InvalidInput),
            }
        }
        let before = Settings::resolved(&self.service.defaults, &inner.document)
            .map_err(|_| PanelError::Unavailable)?;
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or(PanelError::Unavailable)?;
        let mut snapshot = Settings::resolved(&self.service.defaults, &next)
            .map_err(|_| PanelError::InvalidInput)?;
        if before.settings.enabled && !snapshot.settings.enabled {
            next.paused_at_revision = next.revision;
            snapshot.paused_at_revision = next.revision;
        }
        let bytes = serde_json::to_vec(&next).map_err(|_| PanelError::Unavailable)?;
        if inner
            .context
            .as_ref()
            .ok_or(PanelError::Unavailable)?
            .state_set(INTEREST_SETTINGS_STATE_KEY, bytes)
            .is_err()
        {
            // 无法判断持久提交是否已发生：关闭读写及订阅，不再用旧副本继续运行。
            inner.context = None;
            self.service.changes.send_replace(None);
            return Err(PanelError::Unavailable);
        }
        inner.document = next;
        self.service.changes.send_replace(Some(snapshot));
        Ok(PageSaved {
            revision: inner.document.revision,
            restart_required: vec![],
        })
    }
}
