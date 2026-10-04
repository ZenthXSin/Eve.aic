//! 有界、按可信会话隔离的训练模式与可替换上下文实现。
use eve_llm_api::{
    ContextAssembler, ContextScope, ContextSnapshot, LlmError, LlmFuture, TurnInput,
};
use eve_plugin_api::*;
use eve_training_api::*;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

const KEY: &str = "modes.v1";
const MAX_MODES: usize = 256;
const MAX_BYTES: usize = 262_144;
const PROMPT_OFF: &str = "当前主动提问训练已关闭。按当前请求正常交流，不因为历史中的 /train start 继续训练问卷；如果用户希望恢复专门训练，提示使用 /train start。必要的任务澄清问题不受影响。";
const PROMPT: &str = include_str!("prompt-v2.txt");

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mode {
    scope: ContextScope,
    enabled: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u8,
    modes: Vec<Mode>,
}
struct Inner {
    snapshot: Snapshot,
    context: Option<PluginContext>,
}
struct StoredModes {
    inner: Mutex<Inner>,
    default_enabled: bool,
}
fn error(message: &str) -> PluginError {
    PluginError::State(message.into())
}
fn valid(scope: &ContextScope) -> bool {
    [&scope.session_id, &scope.user_id].iter().all(|id| {
        !id.is_empty()
            && id.len() <= 256
            && id.trim() == id.as_str()
            && !id.chars().any(char::is_control)
    })
}
impl TrainingService for StoredModes {
    fn enabled(&self, scope: &ContextScope) -> PluginResult<bool> {
        if !valid(scope) {
            return Err(error("训练会话作用域无效"));
        }
        let inner = self.inner.lock().map_err(|_| error("训练状态锁不可用"))?;
        if inner.context.is_none() {
            return Err(error("训练服务已停止"));
        }
        Ok(inner
            .snapshot
            .modes
            .iter()
            .find(|m| m.scope == *scope)
            .map_or(self.default_enabled, |m| m.enabled))
    }
    fn set_enabled(&self, scope: &ContextScope, enabled: bool) -> PluginResult<()> {
        if !valid(scope) {
            return Err(error("训练会话作用域无效"));
        }
        let mut inner = self.inner.lock().map_err(|_| error("训练状态锁不可用"))?;
        let ctx = inner
            .context
            .as_ref()
            .ok_or_else(|| error("训练服务已停止"))?;
        let mut next = inner.snapshot.clone();
        if let Some(mode) = next.modes.iter_mut().find(|m| m.scope == *scope) {
            mode.enabled = enabled;
        } else {
            if next.modes.len() >= MAX_MODES {
                return Err(error("训练作用域容量已满；保留状态"));
            }
            next.modes.push(Mode {
                scope: scope.clone(),
                enabled,
            });
        }
        let bytes = serde_json::to_vec(&next).map_err(|_| error("训练状态编码失败"))?;
        if bytes.len() > MAX_BYTES {
            return Err(error("训练状态容量已满；保留状态"));
        }
        ctx.state_set(KEY, bytes)?;
        inner.snapshot = next;
        Ok(())
    }
}

/// 可与其他 TrainingService 实现组合；不会从输入文本推断用户身份。
pub struct TrainingContext(pub Arc<dyn TrainingService>);
impl ContextAssembler for TrainingContext {
    fn assemble(&self, input: TurnInput) -> LlmFuture<'_, ContextSnapshot> {
        self.assemble_scoped(input, None)
    }
    fn assemble_scoped(
        &self,
        _: TurnInput,
        scope: Option<ContextScope>,
    ) -> LlmFuture<'_, ContextSnapshot> {
        Box::pin(async move {
            let scoped = scope.is_some();
            let enabled = scope
                .as_ref()
                .map(|s| self.0.enabled(s))
                .transpose()
                .map_err(|_| LlmError::Context("训练状态不可用；不自动回退".into()))?
                .unwrap_or(false);
            Ok(ContextSnapshot {
                revision: if enabled {
                    "eve-training-2"
                } else {
                    "eve-training-disabled-1"
                }
                .into(),
                profile: String::new(),
                memories: if enabled {
                    vec![PROMPT.trim().into()]
                } else if scoped {
                    vec![PROMPT_OFF.into()]
                } else {
                    vec![]
                },
                history: vec![],
            })
        })
    }
}

pub struct TrainingPlugin {
    manifest: PluginManifest,
    default_enabled: bool,
}
impl TrainingPlugin {
    pub fn new(default_enabled: bool) -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(TRAINING_PLUGIN_ID, "0.1.0")?,
            default_enabled,
        })
    }
}
impl Plugin for TrainingPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let snapshot = match context.state_get(KEY)? {
                None => Snapshot {
                    version: 1,
                    modes: vec![],
                },
                Some(bytes) => {
                    if bytes.len() > MAX_BYTES {
                        return Err(error("训练状态损坏；未清空"));
                    }
                    let snapshot: Snapshot = serde_json::from_slice(&bytes)
                        .map_err(|_| error("训练状态损坏；未清空"))?;
                    let mut seen = std::collections::BTreeSet::new();
                    if snapshot.version != 1
                        || snapshot.modes.len() > MAX_MODES
                        || snapshot.modes.iter().any(|m| {
                            !valid(&m.scope)
                                || !seen.insert((&m.scope.session_id, &m.scope.user_id))
                        })
                    {
                        return Err(error("训练状态损坏或版本不兼容；未清空"));
                    }
                    snapshot
                }
            };
            let modes = Arc::new(StoredModes {
                inner: Mutex::new(Inner {
                    snapshot,
                    context: Some(context.clone()),
                }),
                default_enabled: self.default_enabled,
            });
            context.provide_service(
                ServiceId::new(TRAINING_SERVICE_ID)?,
                TrainingServiceHandle(modes.clone()),
            )?;
            let cleanup: Cleanup = Box::new(move || {
                Box::pin(async move {
                    modes
                        .inner
                        .lock()
                        .map_err(|_| error("训练状态锁不可用"))?
                        .context = None;
                    Ok(())
                })
            });
            Ok(Some(cleanup))
        })
    }
}
