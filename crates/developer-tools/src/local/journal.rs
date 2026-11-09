//! 操作责任账本：只记录宿主身份、固定分类与指纹，不落盘命令、文件正文、凭据或错误正文。
//! `begin` 必须在副作用之前提交；`finish` 失败时结果不确定，调用方不得自动重试。
//! 启动先完整验证历史，再把未完成操作标为 unknown；本模块不执行或重放业务动作。
use super::{failed, hash};
use eve_developer_api::DeveloperOperation;
use eve_llm_api::{ContextScope, ToolExecutionContext, ToolExecutionError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_ENTRY_BYTES: usize = 32 * 1024;
const MAX_ID_BYTES: usize = 4096;

static JOURNALS: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Journal>>>> = OnceLock::new();

/// 同进程同目录复用共享序号与串行锁；OS 锁确保另一进程不能写入或误判重启。
pub(super) struct Journal {
    directory: PathBuf,
    pub(super) next: AtomicU64,
    pub(super) serial: tokio::sync::Mutex<()>,
    io: Mutex<()>,
    poisoned: AtomicBool,
    _writer: WriterLock,
}
impl Journal {
    pub(super) fn directory(&self) -> &Path {
        &self.directory
    }
}

struct WriterLock(File);
impl Drop for WriterLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn registry() -> &'static Mutex<BTreeMap<PathBuf, Weak<Journal>>> {
    JOURNALS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub(super) fn open(directory: &Path) -> Result<Arc<Journal>, ToolExecutionError> {
    let directory = directory
        .canonicalize()
        .map_err(|_| failed("操作账本目录不可读取"))?;
    let mut registry = registry()
        .lock()
        .map_err(|_| failed("操作账本共享状态不可用"))?;
    registry.retain(|_, journal| journal.strong_count() > 0);
    if let Some(journal) = registry.get(&directory).and_then(Weak::upgrade) {
        let _io = journal
            .io
            .lock()
            .map_err(|_| failed("操作账本写入状态不可用"))?;
        if journal.poisoned.load(Ordering::Acquire) {
            return Err(failed("操作账本上次提交结果不确定；拒绝继续写入"));
        }
        // 复用实例不能把仍在执行的 started 当成重启；只检验历史完整性。
        scan(&directory)?;
        return Ok(journal.clone());
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("operations.lock"))
        .map_err(|_| failed("操作账本独占锁无法打开"))?;
    file.try_lock()
        .map_err(|_| failed("操作账本被另一运行器占用或无法加锁"))?;
    let writer = WriterLock(file);
    // 只有获得独占写入权且不存在同进程所有者，才有未完成操作的恢复依据。
    let next = load(&directory)?;
    let journal = Arc::new(Journal {
        directory: directory.clone(),
        next: AtomicU64::new(next),
        serial: tokio::sync::Mutex::new(()),
        io: Mutex::new(()),
        poisoned: AtomicBool::new(false),
        _writer: writer,
    });
    registry.insert(directory, Arc::downgrade(&journal));
    Ok(journal)
}

fn owner(directory: &Path) -> Result<Arc<Journal>, ToolExecutionError> {
    let directory = directory
        .canonicalize()
        .map_err(|_| failed("操作账本目录不可读取"))?;
    registry()
        .lock()
        .map_err(|_| failed("操作账本共享状态不可用"))?
        .get(&directory)
        .and_then(Weak::upgrade)
        .ok_or_else(|| failed("操作账本没有独占写入所有者"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    List,
    Read,
    Write,
    Edit,
    Patch,
    Search,
    Command,
    Github,
    Upgrade,
    ApplyUpgrade,
    RestartSelf,
    UpgradeStatus,
}
impl Kind {
    fn intent(self) -> &'static str {
        match self {
            Self::List => "列出工作区目录",
            Self::Read => "读取工作区文件",
            Self::Write => "写入工作区文件",
            Self::Edit => "修改工作区文件",
            Self::Patch => "应用工作区补丁",
            Self::Search => "搜索工作区内容",
            Self::Command => "执行开发命令",
            Self::Github => "请求 GitHub 资源",
            Self::Upgrade => "构建并验证升级候选",
            Self::ApplyUpgrade => "请求监督器应用升级候选",
            Self::RestartSelf => "请求监督器重启当前程序",
            Self::UpgradeStatus => "读取升级监督器状态",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum State {
    PreflightRejected,
    Started,
    Finished,
    Failed,
    AwaitingConfirmation,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Decision {
    PreflightRejected,
    /// 仅代表宿主权限预检；危险动作仍须独立审核，不能解释为审核批准。
    PreflightPassed,
    ExecutionRejected,
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FailureKind {
    Rejected,
    Failed,
    Cancelled,
    UnsuccessfulResult,
    InvalidResult,
    RestartUnresolved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RecoveryAction {
    None,
    CorrectRequestBeforeRetry,
    InspectResultBeforeRetry,
    CheckUpgradeStatus,
    /// 已完成的恢复动作只到“标记不确定且禁止重放”；没有伪称完成业务核验或回滚。
    MarkedUnknownWithoutReplay,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    format_version: u8,
    id: u64,
    kind: Kind,
    state: State,
    at_ms: u64,
    /// 遗留操作缺少这些证据时保留 null，不根据当前用户或当前时间补造身份。
    scope: Option<ContextScope>,
    turn_id: Option<u64>,
    /// 宿主绑定的入口工具调用 ID；嵌套脚本沿用入口 ID，不能伪称叶子调用有独立 ID。
    call_id: Option<String>,
    delegated: Option<bool>,
    intent: String,
    action_sha256: Option<String>,
    target_sha256: Option<String>,
    authorization_messages_sha256: Option<String>,
    origin_version: u8,
    decision: Decision,
    success: Option<bool>,
    failure_kind: Option<FailureKind>,
    recovery_action: RecoveryAction,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyEntry {
    format_version: u8,
    id: u64,
    kind: Kind,
    state: State,
    success: Option<bool>,
}

/// 冻结的开始记录；完成时复用同一身份和指纹，不从改变后的上下文重新采集。
pub(super) struct OperationAudit {
    entry: Entry,
    journal: Arc<Journal>,
    finished: Mutex<bool>,
}
impl OperationAudit {
    pub(super) fn id(&self) -> u64 {
        self.entry.id
    }
}

fn timestamp() -> Result<u64, ToolExecutionError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| failed("操作审计时间无效；没有开始新操作"))?
        .as_millis();
    u64::try_from(millis).map_err(|_| failed("操作审计时间超限；没有开始新操作"))
}

fn digest(value: &impl Serialize) -> Result<String, ToolExecutionError> {
    serde_json::to_vec(value)
        .map(|bytes| hash(&bytes))
        .map_err(|_| failed("操作审计指纹无法生成；没有开始新操作"))
}

fn operation_data(operation: &DeveloperOperation) -> (Kind, Value, Value) {
    match operation {
        DeveloperOperation::List(a) => (Kind::List, json!(a), json!({"path": a.path})),
        DeveloperOperation::Read(a) => (Kind::Read, json!(a), json!({"path": a.path})),
        DeveloperOperation::Write(a) => (Kind::Write, json!(a), json!({"path": a.path})),
        DeveloperOperation::Edit(a) => (Kind::Edit, json!(a), json!({"path": a.path})),
        DeveloperOperation::Patch(a) => (
            Kind::Patch,
            json!(a),
            json!({"paths": eve_apply_patch::paths(&a.patch)}),
        ),
        DeveloperOperation::Search(a) => (Kind::Search, json!(a), json!({"path": a.path})),
        DeveloperOperation::Command(a) => (
            Kind::Command,
            json!(a),
            json!({"program": a.program, "cwd": a.cwd}),
        ),
        DeveloperOperation::Github(a) => (
            Kind::Github,
            json!(a),
            json!({"repository": a.repository, "resource": a.resource}),
        ),
        DeveloperOperation::PrepareUpgrade => (
            Kind::Upgrade,
            Value::Null,
            json!({"target": "running_application"}),
        ),
        DeveloperOperation::ApplyUpgrade(a) => (
            Kind::ApplyUpgrade,
            json!(a),
            json!({"candidate_sha256": a.candidate_sha256}),
        ),
        DeveloperOperation::UpgradeStatus => (
            Kind::UpgradeStatus,
            Value::Null,
            json!({"target": "running_application"}),
        ),
        DeveloperOperation::RestartSelf => (
            Kind::RestartSelf,
            Value::Null,
            json!({"target":"running_application"}),
        ),
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.trim() == value
        && value.len() <= MAX_ID_BYTES
        && !value.chars().any(char::is_control)
}

fn valid_digest(value: &Option<String>) -> bool {
    value.as_ref().is_none_or(|value| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn valid(entry: &Entry) -> bool {
    let metadata = entry.format_version == 2
        && entry.id > 0
        && entry.at_ms > 0
        && entry.intent == entry.kind.intent()
        && entry
            .scope
            .as_ref()
            .is_none_or(|scope| valid_id(&scope.session_id) && valid_id(&scope.user_id))
        && entry.call_id.as_ref().is_none_or(|id| valid_id(id))
        && valid_digest(&entry.action_sha256)
        && valid_digest(&entry.target_sha256)
        && valid_digest(&entry.authorization_messages_sha256)
        && match entry.origin_version {
            1 => {
                entry.state == State::Unknown
                    && entry.scope.is_none()
                    && entry.turn_id.is_none()
                    && entry.call_id.is_none()
                    && entry.delegated.is_none()
                    && entry.action_sha256.is_none()
                    && entry.target_sha256.is_none()
                    && entry.authorization_messages_sha256.is_none()
            }
            2 => {
                entry.action_sha256.is_some()
                    && entry.target_sha256.is_some()
                    && entry.delegated.is_some()
            }
            _ => false,
        };
    metadata
        && match entry.state {
            State::PreflightRejected => {
                entry.decision == Decision::PreflightRejected
                    && entry.success == Some(false)
                    && matches!(
                        entry.failure_kind,
                        Some(FailureKind::Rejected | FailureKind::Failed | FailureKind::Cancelled)
                    )
                    && entry.recovery_action == RecoveryAction::CorrectRequestBeforeRetry
            }
            State::Started => {
                entry.decision == Decision::PreflightPassed
                    && entry.success.is_none()
                    && entry.failure_kind.is_none()
                    && entry.recovery_action == RecoveryAction::None
            }
            State::Finished => {
                entry.decision == Decision::PreflightPassed
                    && entry.success == Some(true)
                    && entry.failure_kind.is_none()
                    && entry.recovery_action == RecoveryAction::None
            }
            State::Failed => {
                entry.success == Some(false)
                    && match entry.failure_kind {
                        Some(FailureKind::Rejected) => {
                            entry.decision == Decision::ExecutionRejected
                                && entry.recovery_action
                                    == RecoveryAction::CorrectRequestBeforeRetry
                        }
                        Some(
                            FailureKind::Failed
                            | FailureKind::Cancelled
                            | FailureKind::UnsuccessfulResult
                            | FailureKind::InvalidResult,
                        ) => {
                            entry.decision == Decision::PreflightPassed
                                && entry.recovery_action == RecoveryAction::InspectResultBeforeRetry
                        }
                        _ => false,
                    }
            }
            State::AwaitingConfirmation => {
                matches!(entry.kind, Kind::ApplyUpgrade | Kind::RestartSelf)
                    && entry.decision == Decision::PreflightPassed
                    && entry.success.is_none()
                    && entry.failure_kind.is_none()
                    && entry.recovery_action == RecoveryAction::CheckUpgradeStatus
            }
            State::Unknown => {
                entry.decision == Decision::OutcomeUnknown
                    && entry.success.is_none()
                    && entry.failure_kind == Some(FailureKind::RestartUnresolved)
                    && entry.recovery_action == RecoveryAction::MarkedUnknownWithoutReplay
            }
        }
}

fn append(directory: &Path, entry: &Entry) -> Result<(), ToolExecutionError> {
    if !valid(entry) {
        return Err(failed("操作审计记录无效；未追加记录"));
    }
    let mut bytes = serde_json::to_vec(entry).map_err(|_| failed("操作审计编码失败"))?;
    if bytes.len() + 1 > MAX_ENTRY_BYTES {
        return Err(failed("操作审计记录超限；未追加记录"));
    }
    bytes.push(b'\n');
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(directory.join("operations.jsonl"))
        .map_err(|_| failed("操作账本不可写；没有开始新操作"))?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| failed("操作账本提交失败；操作结果不确定，不能自动重试"))?;
    #[cfg(unix)]
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|_| failed("操作账本目录提交失败；操作结果不确定，不能自动重试"))?;
    Ok(())
}

fn commit(journal: &Journal, entry: &Entry) -> Result<(), ToolExecutionError> {
    let _io = journal
        .io
        .lock()
        .map_err(|_| failed("操作账本写入状态不可用"))?;
    if journal.poisoned.load(Ordering::Acquire) {
        return Err(failed("操作账本上次提交结果不确定；拒绝继续写入"));
    }
    let result = append(&journal.directory, entry);
    if result.is_err() {
        journal.poisoned.store(true, Ordering::Release);
    }
    result
}

fn snapshot(
    id: u64,
    operation: &DeveloperOperation,
    context: &ToolExecutionContext,
) -> Result<Entry, ToolExecutionError> {
    let (kind, arguments, target) = operation_data(operation);
    Ok(Entry {
        format_version: 2,
        id,
        kind,
        state: State::Started,
        at_ms: timestamp()?,
        scope: context
            .scope()
            .filter(|scope| valid_id(&scope.session_id) && valid_id(&scope.user_id))
            .cloned(),
        turn_id: context.turn_id(),
        call_id: context
            .call_id()
            .filter(|id| valid_id(id))
            .map(str::to_owned),
        delegated: Some(context.is_delegated()),
        intent: kind.intent().into(),
        action_sha256: Some(digest(&json!({"kind": kind, "arguments": arguments}))?),
        target_sha256: Some(digest(&json!({"kind": kind, "target": target}))?),
        authorization_messages_sha256: context
            .review_user_messages()
            .map(|messages| digest(&messages))
            .transpose()?,
        origin_version: 2,
        decision: Decision::PreflightPassed,
        success: None,
        failure_kind: None,
        recovery_action: RecoveryAction::None,
    })
}

/// 调用方完成宿主权限预检并持有操作串行锁后使用；这里只落审计，不授予任何权限。
pub(super) fn begin(
    directory: &Path,
    id: u64,
    operation: &DeveloperOperation,
    context: &ToolExecutionContext,
) -> Result<OperationAudit, ToolExecutionError> {
    let journal = owner(directory)?;
    let entry = snapshot(id, operation, context)?;
    commit(&journal, &entry)?;
    Ok(OperationAudit {
        entry,
        journal,
        finished: Mutex::new(false),
    })
}

/// 准入阶段的拒绝是独立事件，不能捏造 started 或预检成功；错误正文不落盘。
pub(super) fn reject(
    directory: &Path,
    id: u64,
    operation: &DeveloperOperation,
    context: &ToolExecutionContext,
    error: &ToolExecutionError,
) -> Result<(), ToolExecutionError> {
    let journal = owner(directory)?;
    let mut entry = snapshot(id, operation, context)?;
    entry.state = State::PreflightRejected;
    entry.decision = Decision::PreflightRejected;
    entry.success = Some(false);
    entry.failure_kind = Some(match error {
        ToolExecutionError::Rejected(_) => FailureKind::Rejected,
        ToolExecutionError::Unavailable(_) | ToolExecutionError::Failed(_) => FailureKind::Failed,
        ToolExecutionError::Cancelled(_) => FailureKind::Cancelled,
    });
    entry.recovery_action = RecoveryAction::CorrectRequestBeforeRetry;
    commit(&journal, &entry)
}

/// 读取类成功返回可以没有 success；写入和命令必须明确成功，不能只凭 Ok 伪造成功。
/// 升级排队只表示已提交请求，必须等待监督器状态，不宣称升级完成。
pub(super) fn finish(
    directory: &Path,
    audit: &OperationAudit,
    result: &Result<Value, ToolExecutionError>,
) -> Result<(), ToolExecutionError> {
    if directory.canonicalize().ok().as_deref() != Some(audit.journal.directory()) {
        return Err(failed("操作完成账本与开始账本不一致"));
    }
    let mut finished = audit
        .finished
        .lock()
        .map_err(|_| failed("操作完成记录状态不可用"))?;
    if *finished {
        return Err(failed("操作已有完成记录；拒绝重复提交"));
    }
    let mut entry = audit.entry.clone();
    entry.at_ms = timestamp()?.max(entry.at_ms);
    let failure = match result {
        Err(ToolExecutionError::Rejected(_)) => Some(FailureKind::Rejected),
        Err(ToolExecutionError::Unavailable(_) | ToolExecutionError::Failed(_)) => {
            Some(FailureKind::Failed)
        }
        Err(ToolExecutionError::Cancelled(_)) => Some(FailureKind::Cancelled),
        Ok(value) => match value.get("success") {
            Some(Value::Bool(false)) => Some(FailureKind::UnsuccessfulResult),
            Some(Value::Bool(true)) => None,
            None if value.is_object()
                && (matches!(
                    entry.kind,
                    Kind::List | Kind::Read | Kind::Search | Kind::UpgradeStatus
                ) || (entry.kind == Kind::ApplyUpgrade
                    && (value["applied"] == true
                        || (value["queued"] == true && value["applied"] == false)))
                    || (entry.kind == Kind::RestartSelf
                        && (value["restarted"] == true
                            || (value["queued"] == true && value["restarted"] == false)))) =>
            {
                None
            }
            _ => Some(FailureKind::InvalidResult),
        },
    };
    if let Some(failure) = failure {
        entry.state = State::Failed;
        entry.success = Some(false);
        entry.failure_kind = Some(failure);
        if failure == FailureKind::Rejected {
            entry.decision = Decision::ExecutionRejected;
            entry.recovery_action = RecoveryAction::CorrectRequestBeforeRetry;
        } else {
            entry.recovery_action = RecoveryAction::InspectResultBeforeRetry;
        }
    } else if (entry.kind == Kind::ApplyUpgrade
        && result.as_ref().is_ok_and(|value| value["applied"] != true))
        || (entry.kind == Kind::RestartSelf
            && result
                .as_ref()
                .is_ok_and(|value| value["restarted"] != true))
    {
        entry.state = State::AwaitingConfirmation;
        entry.recovery_action = RecoveryAction::CheckUpgradeStatus;
    } else {
        entry.state = State::Finished;
        entry.success = Some(true);
    }
    commit(&audit.journal, &entry)?;
    *finished = true;
    Ok(())
}

enum Started {
    Current(Box<Entry>),
    Legacy(LegacyEntry),
}

fn unknown(started: Started) -> Result<Entry, ToolExecutionError> {
    let now = timestamp()?;
    let mut entry = match started {
        Started::Current(entry) => *entry,
        Started::Legacy(entry) => Entry {
            format_version: 2,
            id: entry.id,
            kind: entry.kind,
            state: State::Unknown,
            at_ms: now,
            scope: None,
            turn_id: None,
            call_id: None,
            delegated: None,
            intent: entry.kind.intent().into(),
            action_sha256: None,
            target_sha256: None,
            authorization_messages_sha256: None,
            origin_version: 1,
            decision: Decision::OutcomeUnknown,
            success: None,
            failure_kind: Some(FailureKind::RestartUnresolved),
            recovery_action: RecoveryAction::MarkedUnknownWithoutReplay,
        },
    };
    entry.at_ms = entry.at_ms.max(now);
    entry.state = State::Unknown;
    entry.decision = Decision::OutcomeUnknown;
    entry.success = None;
    entry.failure_kind = Some(FailureKind::RestartUnresolved);
    entry.recovery_action = RecoveryAction::MarkedUnknownWithoutReplay;
    Ok(entry)
}

fn matches_started(started: &Started, finished: &Entry) -> bool {
    match started {
        Started::Current(started) => {
            started.id == finished.id
                && started.kind == finished.kind
                && started.at_ms <= finished.at_ms
                && started.scope == finished.scope
                && started.turn_id == finished.turn_id
                && started.call_id == finished.call_id
                && started.delegated == finished.delegated
                && started.intent == finished.intent
                && started.action_sha256 == finished.action_sha256
                && started.target_sha256 == finished.target_sha256
                && started.authorization_messages_sha256 == finished.authorization_messages_sha256
                && started.origin_version == finished.origin_version
        }
        Started::Legacy(started) => {
            started.id == finished.id
                && started.kind == finished.kind
                && finished.origin_version == 1
                && finished.state == State::Unknown
        }
    }
}

fn scan(directory: &Path) -> Result<(u64, BTreeMap<u64, Started>), ToolExecutionError> {
    let file = match File::open(directory.join("operations.jsonl")) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((1, BTreeMap::new()));
        }
        Err(_) => return Err(failed("开发工具账本无法读取；保留原文件")),
    };
    let mut reader = BufReader::new(file);
    let mut max = 0;
    let mut ids = BTreeSet::new();
    let mut pending = BTreeMap::new();
    loop {
        let mut line = Vec::new();
        let count = reader
            .by_ref()
            .take((MAX_ENTRY_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)
            .map_err(|_| failed("开发工具账本无法读取；保留原文件"))?;
        if count == 0 {
            break;
        }
        if count > MAX_ENTRY_BYTES || line.last() != Some(&b'\n') {
            return Err(failed("开发工具账本存在超限或不完整提交；保留原文件"));
        }
        let value: Value =
            serde_json::from_slice(&line).map_err(|_| failed("开发工具账本损坏；保留原文件"))?;
        match value["format_version"].as_u64() {
            Some(1) => {
                let entry: LegacyEntry = serde_json::from_value(value)
                    .map_err(|_| failed("开发工具旧账本记录无效；保留原文件"))?;
                if entry.format_version != 1 || entry.id == 0 {
                    return Err(failed("开发工具旧账本版本或 ID 无效；保留原文件"));
                }
                max = max.max(entry.id);
                match entry.state {
                    State::Started if entry.success.is_none() && ids.insert(entry.id) => {
                        pending.insert(entry.id, Started::Legacy(entry));
                    }
                    State::Finished | State::Failed => {
                        let Some(Started::Legacy(started)) = pending.remove(&entry.id) else {
                            return Err(failed("开发工具旧账本状态转换无效；保留原文件"));
                        };
                        if started.kind != entry.kind
                            || (entry.state == State::Failed && entry.success.is_some())
                        {
                            return Err(failed("开发工具旧账本结果无效；保留原文件"));
                        }
                    }
                    _ => return Err(failed("开发工具旧账本状态无效；保留原文件")),
                }
            }
            Some(2) => {
                let entry: Entry = serde_json::from_value(value)
                    .map_err(|_| failed("开发工具账本记录无效；保留原文件"))?;
                if !valid(&entry) {
                    return Err(failed("开发工具账本字段无效；保留原文件"));
                }
                max = max.max(entry.id);
                if matches!(entry.state, State::Started | State::PreflightRejected) {
                    if !ids.insert(entry.id) {
                        return Err(failed("开发工具账本 ID 重复；保留原文件"));
                    }
                    if entry.state == State::Started {
                        pending.insert(entry.id, Started::Current(Box::new(entry)));
                    }
                } else {
                    let started = pending
                        .remove(&entry.id)
                        .ok_or_else(|| failed("开发工具账本缺少开始记录；保留原文件"))?;
                    if !matches_started(&started, &entry) {
                        return Err(failed("开发工具账本身份或动作发生改变；保留原文件"));
                    }
                }
            }
            _ => return Err(failed("开发工具账本版本无效；保留原文件")),
        }
    }
    let next = max
        .checked_add(1)
        .ok_or_else(|| failed("开发工具账本 ID 已满；保留原文件"))?;
    Ok((next, pending))
}

/// 仅由获得独占锁的首次 open 调用；完整校验后才追加恢复记录。
/// 兼容 format_version 1，不改写旧证据，不重放业务动作。
fn load(directory: &Path) -> Result<u64, ToolExecutionError> {
    let (next, pending) = scan(directory)?;
    let recovered = pending
        .into_values()
        .map(unknown)
        .collect::<Result<Vec<_>, _>>()?;
    for entry in recovered {
        append(directory, &entry)?;
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_developer_api::{ApplyUpgradeArguments, CommandArguments, FileArguments};
    use std::fs;

    fn command(secret: &str) -> DeveloperOperation {
        DeveloperOperation::Command(CommandArguments {
            program: "sh".into(),
            args: vec!["-c".into(), format!("printf {secret}")],
            cwd: "sensitive-target".into(),
            timeout_ms: None,
            owner_confirmed: false,
        })
    }

    fn context() -> ToolExecutionContext {
        ToolExecutionContext::new()
            .with_scope(Some(ContextScope {
                session_id: "trusted-session".into(),
                user_id: "trusted-user".into(),
            }))
            .with_turn_id(Some(7))
            .with_call_id("entry-tool-call".into())
            .with_delegated(true)
            .with_review_user_messages(std::sync::Arc::new(vec!["private-user-instruction".into()]))
    }

    fn entries(directory: &Path) -> Vec<Value> {
        fs::read_to_string(directory.join("operations.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn audit_freezes_identity_hashes_actions_and_never_records_sensitive_body() {
        let directory = tempfile::tempdir().unwrap();
        let _journal = open(directory.path()).unwrap();
        let audit = begin(directory.path(), 1, &command("secret-token"), &context()).unwrap();
        assert_eq!(audit.id(), 1);
        finish(
            directory.path(),
            &audit,
            &Err(ToolExecutionError::Failed("secret-token in stderr".into())),
        )
        .unwrap();
        let rows = entries(directory.path());
        assert_eq!(rows[0]["scope"]["session_id"], "trusted-session");
        assert_eq!(rows[0]["scope"]["user_id"], "trusted-user");
        assert_eq!(rows[0]["turn_id"], 7);
        assert_eq!(rows[0]["delegated"], true);
        assert_eq!(rows[0]["call_id"], "entry-tool-call");
        assert_eq!(rows[0]["scope"], rows[1]["scope"]);
        assert_eq!(rows[0]["action_sha256"], rows[1]["action_sha256"]);
        assert_eq!(rows[1]["success"], false);
        assert_eq!(rows[1]["failure_kind"], "failed");
        assert_eq!(rows[1]["recovery_action"], "inspect_result_before_retry");
        let bytes = fs::read_to_string(directory.path().join("operations.jsonl")).unwrap();
        for secret in [
            "secret-token",
            "sensitive-target",
            "private-user-instruction",
            "printf",
            "stderr",
        ] {
            assert!(!bytes.contains(secret), "journal exposed {secret}");
        }
        assert_eq!(scan(directory.path()).unwrap().0, 2);
    }

    #[test]
    fn normal_results_explicit_failures_and_pending_upgrades_remain_distinct() {
        let directory = tempfile::tempdir().unwrap();
        let _journal = open(directory.path()).unwrap();
        for (index, result, expected_state, expected_success) in [
            (1, json!({"success": true}), "finished", json!(true)),
            (
                2,
                json!({"success": false, "exit_code": 9}),
                "failed",
                json!(false),
            ),
            (3, json!({"success": "true"}), "failed", json!(false)),
            (4, Value::Null, "failed", json!(false)),
            (5, json!({"stdout": "unverified"}), "failed", json!(false)),
        ] {
            let audit = begin(directory.path(), index, &command("x"), &context()).unwrap();
            finish(directory.path(), &audit, &Ok(result)).unwrap();
            let rows = entries(directory.path());
            let last = rows.last().unwrap();
            assert_eq!(last["state"], expected_state);
            assert_eq!(last["success"], expected_success);
        }
        let upgrade = DeveloperOperation::ApplyUpgrade(ApplyUpgradeArguments {
            candidate_sha256: "a".repeat(64),
        });
        let audit = begin(directory.path(), 6, &upgrade, &context()).unwrap();
        finish(
            directory.path(),
            &audit,
            &Ok(json!({"queued": true, "applied": false})),
        )
        .unwrap();
        let rows = entries(directory.path());
        assert_eq!(rows.last().unwrap()["state"], "awaiting_confirmation");
        assert_eq!(rows.last().unwrap()["success"], Value::Null);
        let listing = DeveloperOperation::List(FileArguments {
            path: ".".into(),
            offset: 0,
            content: None,
            expected_sha256: None,
        });
        let audit = begin(directory.path(), 7, &listing, &context()).unwrap();
        finish(directory.path(), &audit, &Ok(json!({"entries": []}))).unwrap();
        assert_eq!(entries(directory.path()).last().unwrap()["success"], true);
        assert_eq!(scan(directory.path()).unwrap().0, 8);
        let restart = begin(
            directory.path(),
            8,
            &DeveloperOperation::RestartSelf,
            &context(),
        )
        .unwrap();
        finish(
            directory.path(),
            &restart,
            &Ok(json!({"queued":true,"restarted":false})),
        )
        .unwrap();
        assert_eq!(
            entries(directory.path()).last().unwrap()["state"],
            "awaiting_confirmation"
        );
        assert_eq!(
            entries(directory.path()).last().unwrap()["success"],
            Value::Null
        );
    }

    #[test]
    fn restart_marks_unfinished_operations_unknown_once_without_replaying() {
        let directory = tempfile::tempdir().unwrap();
        let journal = open(directory.path()).unwrap();
        begin(directory.path(), 3, &command("must-not-run"), &context()).unwrap();
        let before = entries(directory.path());
        drop(journal);
        let journal = open(directory.path()).unwrap();
        assert_eq!(journal.next.load(Ordering::Acquire), 4);
        let after = entries(directory.path());
        assert_eq!(after.len(), 2);
        assert_eq!(after[1]["state"], "unknown");
        assert_eq!(after[1]["success"], Value::Null);
        assert_eq!(after[1]["failure_kind"], "restart_unresolved");
        assert_eq!(after[1]["recovery_action"], "marked_unknown_without_replay");
        assert_eq!(after[1]["scope"], before[0]["scope"]);
        assert_eq!(after[1]["action_sha256"], before[0]["action_sha256"]);
        assert_eq!(
            open(directory.path()).unwrap().next.load(Ordering::Acquire),
            4
        );
        assert_eq!(entries(directory.path()), after);
    }

    #[test]
    fn legacy_finished_and_unfinished_records_are_preserved_without_invented_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let legacy = concat!(
            "{\"format_version\":1,\"id\":1,\"kind\":\"read\",\"state\":\"started\",\"success\":null}\n",
            "{\"format_version\":1,\"id\":1,\"kind\":\"read\",\"state\":\"finished\",\"success\":null}\n",
            "{\"format_version\":1,\"id\":2,\"kind\":\"command\",\"state\":\"started\",\"success\":null}\n",
        );
        fs::write(directory.path().join("operations.jsonl"), legacy).unwrap();
        let journal = open(directory.path()).unwrap();
        assert_eq!(journal.next.load(Ordering::Acquire), 3);
        let bytes = fs::read_to_string(directory.path().join("operations.jsonl")).unwrap();
        assert!(bytes.starts_with(legacy));
        let rows = entries(directory.path());
        assert_eq!(rows[3]["format_version"], 2);
        assert_eq!(rows[3]["origin_version"], 1);
        assert_eq!(rows[3]["scope"], Value::Null);
        assert_eq!(rows[3]["action_sha256"], Value::Null);
        assert_eq!(rows[3]["state"], "unknown");
        assert_eq!(
            open(directory.path()).unwrap().next.load(Ordering::Acquire),
            3
        );
        assert_eq!(entries(directory.path()), rows);
    }

    #[test]
    fn corrupt_history_is_not_repaired_or_partially_recovered() {
        for corrupt in [
            "{broken",
            "{broken}\n",
            "{\"id\":1}\n",
            "{\"format_version\":3,\"id\":1}\n",
            "{\"format_version\":1,\"id\":9,\"kind\":\"read\",\"state\":\"finished\",\"success\":true}\n",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let journal = open(directory.path()).unwrap();
            begin(directory.path(), 1, &command("x"), &context()).unwrap();
            let path = directory.path().join("operations.jsonl");
            OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(corrupt.as_bytes())
                .unwrap();
            let before = fs::read(&path).unwrap();
            drop(journal);
            assert!(open(directory.path()).is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
        }
    }

    #[test]
    fn changed_identity_repeated_id_and_impossible_results_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let _journal = open(directory.path()).unwrap();
        let audit = begin(directory.path(), 1, &command("x"), &context()).unwrap();
        finish(directory.path(), &audit, &Ok(json!({"success": true}))).unwrap();
        let original = entries(directory.path());
        for variant in 0..3 {
            let mut rows = original.clone();
            match variant {
                0 => rows[1]["scope"]["user_id"] = json!("another-user"),
                1 => rows.push(rows[0].clone()),
                _ => rows[1]["success"] = json!(false),
            }
            let bytes: String = rows.into_iter().map(|row| format!("{row}\n")).collect();
            let path = directory.path().join("operations.jsonl");
            fs::write(&path, &bytes).unwrap();
            assert!(open(directory.path()).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn shared_owner_does_not_recover_live_work_and_finished_operation_cannot_repeat() {
        let directory = tempfile::tempdir().unwrap();
        let first = open(directory.path()).unwrap();
        let id = first.next.fetch_add(1, Ordering::SeqCst);
        let audit = begin(directory.path(), id, &command("x"), &context()).unwrap();
        let second = open(&directory.path().join(".")).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(second.next.fetch_add(1, Ordering::SeqCst), id + 1);
        assert_eq!(entries(directory.path()).len(), 1);
        let held = first.serial.try_lock().unwrap();
        assert!(second.serial.try_lock().is_err());
        drop(held);
        finish(directory.path(), &audit, &Ok(json!({"success": true}))).unwrap();
        let before = fs::read(directory.path().join("operations.jsonl")).unwrap();
        assert!(finish(directory.path(), &audit, &Ok(json!({"success": false}))).is_err());
        assert_eq!(
            fs::read(directory.path().join("operations.jsonl")).unwrap(),
            before
        );
        let separate = OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.path().join("operations.lock"))
            .unwrap();
        assert!(separate.try_lock().is_err());
        drop(audit);
        drop(first);
        drop(second);
        separate.try_lock().unwrap();
        separate.unlock().unwrap();
    }

    #[test]
    fn external_writer_lock_is_not_mistaken_for_a_stopped_process() {
        let directory = tempfile::tempdir().unwrap();
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(directory.path().join("operations.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        assert!(open(directory.path()).is_err());
        assert!(!directory.path().join("operations.jsonl").exists());
        lock.unlock().unwrap();
        assert!(open(directory.path()).is_ok());
    }

    #[test]
    fn preflight_rejection_preserves_missing_identity_without_a_started_record() {
        let directory = tempfile::tempdir().unwrap();
        let journal = open(directory.path()).unwrap();
        reject(
            directory.path(),
            journal.next.fetch_add(1, Ordering::SeqCst),
            &command("credential-secret"),
            &ToolExecutionContext::new(),
            &ToolExecutionError::Rejected("credential-secret missing-owner".into()),
        )
        .unwrap();
        let rows = entries(directory.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["state"], "preflight_rejected");
        assert_eq!(rows[0]["decision"], "preflight_rejected");
        assert_eq!(rows[0]["scope"], Value::Null);
        assert_eq!(rows[0]["call_id"], Value::Null);
        assert_eq!(rows[0]["success"], false);
        let before = fs::read_to_string(directory.path().join("operations.jsonl")).unwrap();
        assert!(!before.contains("credential-secret"));
        assert!(!before.contains("missing-owner"));
        drop(journal);
        let recovered = open(directory.path()).unwrap();
        assert_eq!(recovered.next.load(Ordering::Acquire), 2);
        assert_eq!(entries(directory.path()), rows);
    }
}
