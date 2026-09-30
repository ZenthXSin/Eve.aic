//! 组合层会话宿主；状态业务由可替换 SessionService 实现，模型仍由 LlmHost 调用。
use crate::{LlmHost, TurnFailure, TurnOutput};
use eve_llm_api::{LlmError, TurnInput};
use eve_plugin_api::{LogEntry, LogLevel, LogRecord, Logger, PluginId, PluginState, ServiceId};
use eve_session_api::{
    SESSION_PLUGIN_ID, SESSION_SERVICE_ID, SessionError, SessionFailure, SessionFailureCode,
    SessionInput, SessionService, SessionServiceHandle, TurnLease,
};
use std::{fmt, sync::Arc, time::SystemTime};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionBinding {
    pub service_id: ServiceId,
    pub expected_owner: PluginId,
}
impl SessionBinding {
    pub fn builtin() -> Self {
        Self {
            service_id: ServiceId::new(SESSION_SERVICE_ID).expect("valid builtin ID"),
            expected_owner: PluginId::new(SESSION_PLUGIN_ID).expect("valid builtin ID"),
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct SessionTurnOutput {
    pub turn_id: u64,
    pub output: TurnOutput,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SessionRunError {
    Session(SessionError),
    Turn(TurnFailure),
    /// 已生成回复（可能已执行工具），但最终提交失败；调用方不得自动重试工具。
    Commit {
        error: SessionError,
        output: Box<TurnOutput>,
    },
    FailureRecord {
        error: SessionError,
        failure: TurnFailure,
    },
}
impl fmt::Display for SessionRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(error) => error.fmt(f),
            Self::Turn(failure) => failure.error.fmt(f),
            Self::Commit { error, .. } => write!(f, "回复已生成但会话保存失败：{error}"),
            Self::FailureRecord { error, .. } => write!(f, "执行失败且状态保存失败：{error}"),
        }
    }
}
impl std::error::Error for SessionRunError {}
impl From<SessionError> for SessionRunError {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}

struct LeaseGuard {
    service: Arc<dyn SessionService>,
    lease: TurnLease,
    armed: bool,
    logger: Arc<dyn Logger>,
    owner: PluginId,
}
impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if self.armed
            && self
                .service
                .fail(
                    &self.lease,
                    SessionFailure {
                        code: SessionFailureCode::Cancelled,
                        started_tools: None,
                    },
                )
                .is_err()
        {
            // Future 已被调用方丢弃，无法回传错误；原 Pending 仍保留，不释放为可重试。
            let entry = LogEntry::new(
                LogLevel::Error,
                "session.cancel_commit",
                "会话取消记录提交失败；原 Pending 保留，不能自动重试工具。",
            )
            .expect("固定日志目标有效")
            .with_field("turn_id", self.lease.turn_id.to_string());
            if self
                .logger
                .log(LogRecord {
                    timestamp: SystemTime::now(),
                    plugin: self.owner.clone(),
                    entry,
                })
                .is_err()
            {
                eprintln!("会话取消记录和诊断投递均失败；原 Pending 保留，不能自动重试工具。");
            }
        }
    }
}

pub struct SessionLlmHost {
    host: LlmHost,
    binding: SessionBinding,
    logger: Arc<dyn Logger>,
}
impl SessionLlmHost {
    pub fn new(host: LlmHost, binding: SessionBinding) -> Self {
        Self {
            host,
            binding,
            logger: Arc::new(eve_kernel::backends::StderrLogger::default()),
        }
    }
    /// 组合层可复用 Kernel 的日志后端；取消记录失败日志不包含输入、用户或后端错误。
    pub fn with_logger(mut self, logger: Arc<dyn Logger>) -> Self {
        self.logger = logger;
        self
    }
    fn service(&self) -> Result<Arc<dyn SessionService>, SessionError> {
        let entry = self
            .host
            .registry
            .get(&self.binding.service_id)
            .map_err(|_| SessionError::Unavailable)?
            .ok_or(SessionError::Unavailable)?;
        if entry.owner != self.binding.expected_owner {
            return Err(SessionError::OwnerMismatch);
        }
        if self.host.kernel.state(&entry.owner) != Some(PluginState::Active) {
            return Err(SessionError::Unavailable);
        }
        Arc::downcast::<SessionServiceHandle>(entry.value)
            .map(|handle| handle.0.clone())
            .map_err(|_| SessionError::Unavailable)
    }
    pub async fn run_turn(
        &self,
        input: SessionInput,
    ) -> Result<SessionTurnOutput, SessionRunError> {
        let admission = Arc::new(self.host.kernel.acquire_runtime_admission().await);
        let service = self.service()?;
        let text = input.text.clone();
        let started = service.begin(input)?;
        let mut guard = LeaseGuard {
            service: service.clone(),
            lease: started.lease.clone(),
            armed: true,
            logger: self.logger.clone(),
            owner: self.binding.expected_owner.clone(),
        };
        let result = self
            .host
            .run_turn_inner(TurnInput { text }, Some(started.history), admission.clone())
            .await;
        let result = match result {
            Ok(output) => match service.complete(&started.lease, output.transcript.clone()) {
                Ok(()) => Ok(SessionTurnOutput {
                    turn_id: started.lease.turn_id,
                    output,
                }),
                Err(error) => Err(SessionRunError::Commit {
                    error,
                    output: Box::new(output),
                }),
            },
            Err(failure) => {
                let summary = SessionFailure {
                    code: failure_code(&failure.error),
                    started_tools: Some(failure.diagnostics.started_tools as u64),
                };
                match service.fail(&started.lease, summary) {
                    Ok(()) => Err(SessionRunError::Turn(failure)),
                    Err(error) => Err(SessionRunError::FailureRecord { error, failure }),
                }
            }
        };
        // 显式提交失败仍保留 Pending；不能由析构的 Cancelled 覆盖真实结果。
        guard.armed = false;
        result
    }
}
fn failure_code(error: &LlmError) -> SessionFailureCode {
    match error {
        LlmError::Configuration(_) | LlmError::Context(_) => SessionFailureCode::Context,
        LlmError::Provider(_) => SessionFailureCode::Provider,
        LlmError::ProviderTimeout => SessionFailureCode::ProviderTimeout,
        LlmError::Protocol(_) => SessionFailureCode::Protocol,
        LlmError::Unsupported(_) => SessionFailureCode::Unsupported,
        LlmError::RoundLimit => SessionFailureCode::RoundLimit,
        LlmError::Backend(_) => SessionFailureCode::Backend,
        LlmError::Cancelled => SessionFailureCode::Cancelled,
    }
}
