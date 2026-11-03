//! 任务控制定义层；通道与判断插件只依赖这里，不依赖控制器或模型实现。
use eve_llm_api::{ChatMessage, LlmError, LlmFuture, ToolResult, TurnEvent, TurnEventSink};
use eve_session_api::{SessionError, SessionInput, SessionKey};
use serde::{Deserialize, Serialize};
use std::{fmt, future::Future, pin::Pin, sync::Arc};

pub const CONTROL_PLUGIN_ID: &str = "eve.control";
pub const CONTROL_SERVICE_ID: &str = "eve.control.requests";
pub type ControlResult<T> = Result<T, ControlError>;
pub type ControlFuture<'a, T> = Pin<Box<dyn Future<Output = ControlResult<T>> + Send + 'a>>;
pub type RunFuture<'a> = Pin<Box<dyn Future<Output = RunReport> + Send + 'a>>;

mod run_metadata;
pub use run_metadata::{ControlReviewEvidence, ControlRunMetadata};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationKey {
    pub session: SessionKey,
    pub task_id: String,
    /// 每次控制插件启动重新生成的随机标识，避免重启后旧通道事件撞号。
    pub controller_epoch: [u8; 16],
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlInput {
    pub session: SessionInput,
    pub task_id: String,
}
impl ControlInput {
    pub fn validate(&self) -> ControlResult<()> {
        self.session
            .key
            .validate()
            .map_err(|_| ControlError::InvalidInput)?;
        if self.session.text.trim().is_empty()
            || self.task_id.is_empty()
            || self.task_id.len() > 256
            || self.task_id.trim() != self.task_id
            || self.task_id.chars().any(char::is_control)
        {
            return Err(ControlError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ControlPhase {
    Starting,
    Generating,
    /// 含等待并发槽位/串行队列；不表示所有工具已经执行。
    Tools,
    Committing,
    Cancelling,
    Finished,
    /// 保存失败或执行器状态未知；禁止在这个控制器中开启替代轮次。
    Blocked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitState {
    NotStarted,
    Completed,
    Failed,
    Pending,
    Unknown,
}
#[derive(Clone, Debug, PartialEq)]
pub enum RunFailure {
    Execution(LlmError),
    Session(SessionError),
    Commit(SessionError),
    Delivery(LlmError),
    FailureRecord {
        storage: SessionError,
        execution: LlmError,
    },
    RunnerPanicked,
}
#[derive(Clone, Debug, PartialEq)]
pub struct RunReport {
    /// 取消发生在首个事件前时可能未知；代际标识始终完整。
    pub turn_id: Option<u64>,
    pub commit: CommitState,
    pub text: Option<String>,
    /// 完整生成结果；Commit 失败时保留原调用参数和配对回执，便于显式修复提交。
    pub transcript: Option<Vec<ChatMessage>>,
    /// None 表示未知，不能由此宣称没有副作用。
    pub started_tools: Option<u64>,
    pub tool_results: Vec<ToolResult>,
    pub failure: Option<RunFailure>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ControlReport {
    pub key: GenerationKey,
    /// 仅代表用户取消已准入；Completed/Delivery 仍表示回复已经提交。
    pub cancel_requested: bool,
    pub run: RunReport,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ControlSnapshot {
    pub key: GenerationKey,
    /// 当前代真实输入；补充/纠正组合不能让调用方冒充原任务上下文。
    pub input_text: String,
    pub phase: ControlPhase,
    pub cancel_requested: bool,
    /// 展示资格已撤销；完成后取消也撤销排队事件，但不改写执行结果。
    pub events_retired: bool,
    pub turn_id: Option<u64>,
    pub report: Option<ControlReport>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ControlEvent {
    pub key: GenerationKey,
    pub event: TurnEvent,
}

/// 有界通道应实现 closed。排队事件必须保留 key，在实际展示前调用 accepts；
/// 通道需串行处理“检查并展示”和取消/切换，不能在两者之间异步等待。
pub trait ControlEventSink: Send + Sync {
    fn emit(&self, event: ControlEvent) -> LlmFuture<'_, ()>;
    fn closed(&self) -> LlmFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
pub struct DiscardControlEvents;
impl ControlEventSink for DiscardControlEvents {
    fn emit(&self, _: ControlEvent) -> LlmFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

/// 组合层装配内置 SessionLlmHost；控制插件不持有模型厂商或 Runtime 私有类型。
/// run 必须观察 sink.closed，并在返回前等待工具析构及最终状态提交。
/// worker 被执行器直接丢弃时，上述返回路径可能没有执行；控制器发布的 Unknown
/// 只表示原始结果不可用，不保证 runner 派生的任务或工具已经完成析构。
pub trait ControlRunner: Send + Sync {
    fn run<'a>(&'a self, input: SessionInput, sink: &'a dyn TurnEventSink) -> RunFuture<'a>;
    /// 专用宿主通道；旧执行器必须拒绝带证据的请求，不能静默丢掉原话或消息标识。
    fn run_with_metadata<'a>(
        &'a self,
        input: SessionInput,
        metadata: Option<ControlRunMetadata>,
        sink: &'a dyn TurnEventSink,
    ) -> RunFuture<'a> {
        if metadata.is_none() {
            return self.run(input, sink);
        }
        Box::pin(async {
            RunReport {
                turn_id: None,
                commit: CommitState::NotStarted,
                text: None,
                transcript: None,
                started_tools: Some(0),
                tool_results: vec![],
                failure: Some(RunFailure::Execution(LlmError::Configuration(
                    "控制执行器不支持可信审核证据；未执行本轮".into(),
                ))),
            }
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelDisposition {
    Requested,
    AlreadyRequested,
    AlreadyFinished,
}
pub trait ControlService: Send + Sync {
    /// 分页列出本控制器已准入任务的完整可信会话身份，包括尚未进入 Session 的任务，
    /// 不返回正文或诊断，也不执行、恢复或重放任务。控制器重启后旧任务不重新准入。
    /// 按 session_id 升序，after 是独占游标，可不对应已有会话；Some 游标须非空、
    /// UTF-8 字节数不超过 256 且无控制字符，允许空格。limit 必须为 1..=100。
    /// 单页是读取时的快照，跨页不提供事务快照；调用者以末项 session_id 继续。
    /// 停止后的句柄不可用；旧的自定义实现默认不支持枚举。
    fn list_keys(&self, _after: Option<&str>, _limit: usize) -> ControlResult<Vec<SessionKey>> {
        Err(ControlError::Unavailable)
    }
    fn submit(
        &self,
        input: ControlInput,
        sink: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey>;
    /// 元数据不进入可反序列化输入；控制器在准入前核对完整作用域和任务标识。
    fn submit_with_metadata(
        &self,
        input: ControlInput,
        metadata: Option<ControlRunMetadata>,
        sink: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        if metadata.is_some() {
            return Err(ControlError::Unavailable);
        }
        self.submit(input, sink)
    }
    /// 目标已收尾后，原子比较完整代际并提交替代请求；不隐式取消。
    /// 比较与提交在同一临界区，避免过时分类覆盖刚完成的新任务。
    fn submit_if_current(
        &self,
        expected: &GenerationKey,
        input: ControlInput,
        sink: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey>;
    /// 与 submit_if_current 相同的代际 CAS，并原子冻结本次执行的授权来源。
    fn submit_if_current_with_metadata(
        &self,
        expected: &GenerationKey,
        input: ControlInput,
        metadata: Option<ControlRunMetadata>,
        sink: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        if metadata.is_some() {
            return Err(ControlError::Unavailable);
        }
        self.submit_if_current(expected, input, sink)
    }
    /// 只发送取消信号，不宣称工具已经停止；须读取 wait 报告判断实际结果。
    fn cancel(&self, key: &GenerationKey) -> ControlResult<CancelDisposition>;
    /// 创建时捕获该代完成通知；之后即使新代启动，这个 Future 仍返回原代报告。
    /// 每个会话只保留最新代；新代启动后再创建旧代 wait 返回 StaleGeneration。
    /// Unknown 表示 worker 的结果不可用，可能早于派生任务或工具析构完成；
    /// 不得将它视为取消完成、没有副作用或允许重试的依据。
    fn wait(&self, key: &GenerationKey) -> ControlFuture<'static, ControlReport>;
    fn snapshot(&self, key: &SessionKey) -> ControlResult<Option<ControlSnapshot>>;
    /// 可信通道已持久化回执后释放已结束代的内存报告，不修改 Session 历史。
    /// 精确代际必须正常结算为 Finished；在途、Blocked 和未知结局拒绝。
    /// 旧等待句柄保留原报告，新建的旧代读取与事件展示失效。
    fn retire_finished(&self, _key: &GenerationKey) -> ControlResult<()> {
        Err(ControlError::Unavailable)
    }
    /// 包括用户、会话、任务、控制器 epoch 和生成代；取消代及被替换代均不接受。
    fn accepts(&self, event: &ControlEvent) -> bool;
}
#[derive(Clone)]
pub struct ControlServiceHandle(pub Arc<dyn ControlService>);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlError {
    InvalidInput,
    OwnerMismatch,
    Busy,
    Blocked,
    StaleGeneration,
    Unavailable,
    LimitReached,
}
impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "任务输入或标识无效",
            Self::OwnerMismatch => "任务所属用户不匹配",
            Self::Busy => "会话任务仍在执行或取消收尾",
            Self::Blocked => "会话提交失败或状态未知，不能开启替代轮次",
            Self::StaleGeneration => "任务生成代已失效",
            Self::Unavailable => "控制服务不可用",
            Self::LimitReached => "任务生成计数已达上限",
        })
    }
}
impl std::error::Error for ControlError {}

#[cfg(test)]
mod enumeration_compatibility {
    use super::*;

    struct CustomControl;
    impl ControlService for CustomControl {
        fn submit(
            &self,
            _: ControlInput,
            _: Arc<dyn ControlEventSink>,
        ) -> ControlResult<GenerationKey> {
            panic!("旧接口不能静默接收新证据")
        }
        fn submit_if_current(
            &self,
            _: &GenerationKey,
            _: ControlInput,
            _: Arc<dyn ControlEventSink>,
        ) -> ControlResult<GenerationKey> {
            panic!("旧接口不能静默接收新证据")
        }
        fn cancel(&self, _: &GenerationKey) -> ControlResult<CancelDisposition> {
            Err(ControlError::Unavailable)
        }
        fn wait(&self, _: &GenerationKey) -> ControlFuture<'static, ControlReport> {
            Box::pin(async { Err(ControlError::Unavailable) })
        }
        fn snapshot(&self, _: &SessionKey) -> ControlResult<Option<ControlSnapshot>> {
            Ok(None)
        }
        fn accepts(&self, _: &ControlEvent) -> bool {
            false
        }
    }

    #[test]
    fn existing_custom_service_can_omit_enumeration() {
        let custom: &dyn ControlService = &CustomControl;
        assert_eq!(custom.list_keys(None, 10), Err(ControlError::Unavailable));
    }

    #[test]
    fn existing_custom_service_rejects_metadata_without_falling_back_to_legacy_submit() {
        let custom: &dyn ControlService = &CustomControl;
        let input = ControlInput {
            session: SessionInput {
                key: SessionKey::new("session", "user").unwrap(),
                text: "待执行任务".into(),
            },
            task_id: "task".into(),
        };
        let metadata = ControlRunMetadata {
            scope: eve_llm_api::ContextScope {
                session_id: "session".into(),
                user_id: "user".into(),
            },
            task_id: "task".into(),
            review: ControlReviewEvidence::ChannelAnchor("real-message".into()),
        };
        assert_eq!(
            custom.submit_with_metadata(
                input.clone(),
                Some(metadata.clone()),
                Arc::new(DiscardControlEvents)
            ),
            Err(ControlError::Unavailable)
        );
        let previous = GenerationKey {
            session: input.session.key.clone(),
            task_id: "previous-task".into(),
            controller_epoch: [1; 16],
            generation: 1,
        };
        assert_eq!(
            custom.submit_if_current_with_metadata(
                &previous,
                input,
                Some(metadata),
                Arc::new(DiscardControlEvents)
            ),
            Err(ControlError::Unavailable)
        );
    }
}
