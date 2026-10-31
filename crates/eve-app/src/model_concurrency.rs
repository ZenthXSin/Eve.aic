//! 核心模型角色的并发通道按配置服务身份共享，不能让不同用途各领一份并发。
use eve_config_api::{ConfigService, ModelRole};
use eve_llm_api::LlmError;
use eve_model_concurrency::ConcurrencyGate;
use std::sync::{Arc, Mutex, Weak};

struct Scope {
    settings: Weak<dyn ConfigService>,
    primary: Weak<ConcurrencyGate>,
    auxiliary: Weak<ConcurrencyGate>,
    global: Weak<ConcurrencyGate>,
}
static SCOPES: Mutex<Vec<Scope>> = Mutex::new(Vec::new());

/// 只读观测，不创建通道或更新限额；观测修订与配置修订分别返回。
pub(crate) fn status(
    settings: &Arc<dyn ConfigService>,
    role: ModelRole,
) -> Result<Option<eve_model_concurrency::GateStatus>, LlmError> {
    let scopes = SCOPES
        .lock()
        .map_err(|_| LlmError::Backend("模型并发目录不可用".into()))?;
    let identity = Arc::downgrade(settings);
    let gate = scopes
        .iter()
        .find(|scope| scope.settings.ptr_eq(&identity))
        .and_then(|scope| match role {
            ModelRole::Primary => scope.primary.upgrade(),
            ModelRole::AuxiliarySmall => scope.auxiliary.upgrade(),
            _ => None,
        });
    drop(scopes);
    gate.map(|gate| gate.status()).transpose()
}

pub(crate) fn gate(
    settings: &Arc<dyn ConfigService>,
    role: ModelRole,
    limit: usize,
    revision: u64,
) -> Result<Arc<ConcurrencyGate>, LlmError> {
    let limit = limit.min(5);
    let mut scopes = SCOPES
        .lock()
        .map_err(|_| LlmError::Backend("模型并发目录不可用".into()))?;
    scopes.retain(|scope| scope.settings.strong_count() != 0);
    let identity = Arc::downgrade(settings);
    let index = if let Some(index) = scopes
        .iter()
        .position(|scope| scope.settings.ptr_eq(&identity))
    {
        index
    } else {
        scopes.push(Scope {
            settings: identity,
            primary: Weak::new(),
            auxiliary: Weak::new(),
            global: Weak::new(),
        });
        scopes.len() - 1
    };
    let slot = match role {
        ModelRole::Primary => &mut scopes[index].primary,
        ModelRole::AuxiliarySmall => &mut scopes[index].auxiliary,
        _ => {
            return Err(LlmError::Configuration(
                "此并发通道只接受主/辅对话模型角色".into(),
            ));
        }
    };
    if let Some(gate) = slot.upgrade() {
        gate.observe(limit, revision)?;
        return Ok(gate);
    }
    let gate = ConcurrencyGate::new(limit, revision)?;
    *slot = Arc::downgrade(&gate);
    Ok(gate)
}

/// 所有主/辅用途共用五个实际请求槽，旧的大额配置仍可读取但不会绕过此上限。
pub(crate) fn global(settings: &Arc<dyn ConfigService>) -> Result<Arc<ConcurrencyGate>, LlmError> {
    let mut scopes = SCOPES
        .lock()
        .map_err(|_| LlmError::Backend("模型并发目录不可用".into()))?;
    scopes.retain(|scope| scope.settings.strong_count() != 0);
    let identity = Arc::downgrade(settings);
    let index = if let Some(index) = scopes
        .iter()
        .position(|scope| scope.settings.ptr_eq(&identity))
    {
        index
    } else {
        scopes.push(Scope {
            settings: identity,
            primary: Weak::new(),
            auxiliary: Weak::new(),
            global: Weak::new(),
        });
        scopes.len() - 1
    };
    if let Some(gate) = scopes[index].global.upgrade() {
        return Ok(gate);
    }
    let gate = ConcurrencyGate::new(5, 0)?;
    scopes[index].global = Arc::downgrade(&gate);
    Ok(gate)
}
