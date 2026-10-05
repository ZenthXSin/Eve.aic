//! 会话分段设置的持久化实现：严格加载、写入失败即关闭、不自动修复或清空。
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    cleanup,
};
use eve_segment_api::{
    SegmentChange, SegmentPreference, SegmentPreferenceError, SegmentPreferenceResult,
    SegmentPreferences, SegmentScope,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

/// 插件标识同时是状态命名空间；更改即视为格式迁移。
pub const SEGMENT_PREFERENCES_PLUGIN_ID: &str = "eve.segment.preferences";
pub const SEGMENT_PREFERENCE_STATE_KEY: &str = "preferences.v1";
pub const PREFERENCE_FORMAT_VERSION: u32 = 1;
pub const MAX_PREFERENCE_SCOPES: usize = 1024;
pub const MAX_PREFERENCE_STATE_BYTES: usize = 1_048_576;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredScope {
    channel: String,
    session_id: String,
    user_id: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    scope: StoredScope,
    enabled: Option<bool>,
    max_segments: Option<usize>,
    pause_percent: Option<u16>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    scopes: Vec<Record>,
}
/// 先只读版本号，使未来格式报告为版本不兼容而不是损坏。
#[derive(Deserialize)]
struct Header {
    version: u32,
}

fn decode(bytes: &[u8]) -> SegmentPreferenceResult<BTreeMap<SegmentScope, SegmentPreference>> {
    use SegmentPreferenceError::{CorruptState, UnsupportedVersion};
    if bytes.len() > MAX_PREFERENCE_STATE_BYTES {
        return Err(CorruptState);
    }
    let header: Header = serde_json::from_slice(bytes).map_err(|_| CorruptState)?;
    if header.version != PREFERENCE_FORMAT_VERSION {
        return Err(UnsupportedVersion);
    }
    let document: Document = serde_json::from_slice(bytes).map_err(|_| CorruptState)?;
    if document.scopes.len() > MAX_PREFERENCE_SCOPES {
        return Err(CorruptState);
    }
    let mut scopes = BTreeMap::new();
    for record in document.scopes {
        let scope = SegmentScope {
            channel: record.scope.channel,
            session_id: record.scope.session_id,
            user_id: record.scope.user_id,
        };
        let preference = SegmentPreference {
            enabled: record.enabled,
            max_segments: record.max_segments,
            pause_percent: record.pause_percent,
        };
        // 写入方从不保存全空记录；出现即视为外部改写或损坏。
        if scope.validate().is_err()
            || preference.validate().is_err()
            || preference.is_default()
            || scopes.insert(scope, preference).is_some()
        {
            return Err(CorruptState);
        }
    }
    Ok(scopes)
}
fn encode(scopes: &BTreeMap<SegmentScope, SegmentPreference>) -> SegmentPreferenceResult<Vec<u8>> {
    let document = Document {
        version: PREFERENCE_FORMAT_VERSION,
        scopes: scopes
            .iter()
            .map(|(scope, preference)| Record {
                scope: StoredScope {
                    channel: scope.channel.clone(),
                    session_id: scope.session_id.clone(),
                    user_id: scope.user_id.clone(),
                },
                enabled: preference.enabled,
                max_segments: preference.max_segments,
                pause_percent: preference.pause_percent,
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&document).map_err(|_| SegmentPreferenceError::Storage)?;
    if bytes.len() > MAX_PREFERENCE_STATE_BYTES {
        return Err(SegmentPreferenceError::LimitReached);
    }
    Ok(bytes)
}

struct Inner {
    scopes: BTreeMap<SegmentScope, SegmentPreference>,
    context: Option<PluginContext>,
}
struct Stored {
    inner: Mutex<Inner>,
}
impl Stored {
    fn open(context: PluginContext) -> SegmentPreferenceResult<Self> {
        let scopes = match context
            .state_get(SEGMENT_PREFERENCE_STATE_KEY)
            .map_err(|_| SegmentPreferenceError::Storage)?
        {
            None => BTreeMap::new(),
            Some(bytes) => decode(&bytes)?,
        };
        Ok(Self {
            inner: Mutex::new(Inner {
                scopes,
                context: Some(context),
            }),
        })
    }
    fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("分段设置状态锁不可用".into()))?
            .context = None;
        Ok(())
    }
    fn get(&self, scope: &SegmentScope) -> SegmentPreferenceResult<SegmentPreference> {
        scope.validate()?;
        let inner = self
            .inner
            .lock()
            .map_err(|_| SegmentPreferenceError::Unavailable)?;
        if inner.context.is_none() {
            return Err(SegmentPreferenceError::Unavailable);
        }
        Ok(inner.scopes.get(scope).copied().unwrap_or_default())
    }
    fn update(
        &self,
        scope: &SegmentScope,
        change: SegmentChange,
    ) -> SegmentPreferenceResult<SegmentPreference> {
        scope.validate()?;
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| SegmentPreferenceError::Unavailable)?;
        let Some(context) = inner.context.clone() else {
            return Err(SegmentPreferenceError::Unavailable);
        };
        let current = inner.scopes.get(scope).copied().unwrap_or_default();
        let next = current.apply(change)?;
        if next == current {
            return Ok(next);
        }
        let mut scopes = inner.scopes.clone();
        if next.is_default() {
            scopes.remove(scope);
        } else if scopes.insert(scope.clone(), next).is_none()
            && scopes.len() > MAX_PREFERENCE_SCOPES
        {
            return Err(SegmentPreferenceError::LimitReached);
        }
        let bytes = encode(&scopes)?;
        if context
            .state_set(SEGMENT_PREFERENCE_STATE_KEY, bytes)
            .is_err()
        {
            // 后端可能已经提交；旧缓存不能继续读写，须重新打开读取实际状态。
            inner.context = None;
            return Err(SegmentPreferenceError::Storage);
        }
        inner.scopes = scopes;
        Ok(next)
    }
}

/// 仅供可信宿主保留；停止后的句柄不会随下一次启动复活旧缓存，每次调用解析当前实例。
#[derive(Clone, Default)]
pub struct SegmentPreferenceController {
    active: Arc<Mutex<Option<Arc<Stored>>>>,
}
impl SegmentPreferenceController {
    fn stored(&self) -> SegmentPreferenceResult<Arc<Stored>> {
        self.active
            .lock()
            .map_err(|_| SegmentPreferenceError::Unavailable)?
            .clone()
            .ok_or(SegmentPreferenceError::Unavailable)
    }
}
impl SegmentPreferences for SegmentPreferenceController {
    fn get(&self, scope: &SegmentScope) -> SegmentPreferenceResult<SegmentPreference> {
        self.stored()?.get(scope)
    }
    fn update(
        &self,
        scope: &SegmentScope,
        change: SegmentChange,
    ) -> SegmentPreferenceResult<SegmentPreference> {
        self.stored()?.update(scope, change)
    }
}

pub struct SegmentPreferencePlugin {
    manifest: PluginManifest,
    controller: SegmentPreferenceController,
}
impl SegmentPreferencePlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(
                SEGMENT_PREFERENCES_PLUGIN_ID,
                env!("CARGO_PKG_VERSION"),
            )?,
            controller: SegmentPreferenceController::default(),
        })
    }
    pub fn controller(&self) -> SegmentPreferenceController {
        self.controller.clone()
    }
}
impl Plugin for SegmentPreferencePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let stored = Arc::new(
                Stored::open(context.clone())
                    .map_err(|error| PluginError::State(error.to_string()))?,
            );
            let to_close = stored.clone();
            let controller = self.controller.clone();
            context.cleanup(cleanup(move || async move {
                to_close.close()?;
                let mut active = controller
                    .active
                    .lock()
                    .map_err(|_| PluginError::State("分段设置管理句柄不可用".into()))?;
                if active
                    .as_ref()
                    .is_some_and(|value| Arc::ptr_eq(value, &to_close))
                {
                    *active = None;
                }
                Ok(())
            }))?;
            *self
                .controller
                .active
                .lock()
                .map_err(|_| PluginError::State("分段设置管理句柄不可用".into()))? = Some(stored);
            Ok(None)
        })
    }
}
