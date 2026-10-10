//! QQ 宿主的兴趣观察：低频扫描已送达交互，持久化准入后才调用观察器；
//! 学习目标派生只读写本地状态，不调用模型。
use crate::AppError;
use eve_cognition_api::{CognitionAdmin, GoalStatus};
use eve_interest_api::{
    InterestAdmin, InterestError, InterestGoalDeriver, InterestObserver, InterestRecord,
    InterestSettingsReader, InterestSettingsSnapshot, InterestStatus, ObservationFailure,
    ObservationOutcome, StatementKind, WithdrawalRequest,
};
use eve_interest_plugin::learning_goal_id;
use eve_memory_api::{MemoryAdmin, validate_id};
use eve_plugin_api::{PluginError, PluginResult};
use eve_qqbot_plugin::{QqCommandHandler, QqCommandInput};
use ring::digest::{Context, SHA256};
use std::{
    fmt::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::watch, task::JoinHandle};

const SUBJECT: &str = "eve";
const HELP: &str = "用法：/interests 查看本会话记录的兴趣，/forget-interest 兴趣ID 撤回一条兴趣。";
const DISABLED: &str = "兴趣观察未启用。";
const PREVIEW_BYTES: usize = 120;

fn now_ms() -> Result<u64, AppError> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

pub(crate) struct Background {
    active: watch::Sender<bool>,
    stop: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
    task: JoinHandle<Result<(), AppError>>,
}
impl Background {
    pub(crate) fn start(
        memory: Arc<dyn MemoryAdmin>,
        interests: Arc<dyn InterestAdmin>,
        observer: Arc<dyn InterestObserver>,
        deriver: Arc<dyn InterestGoalDeriver>,
        settings: Arc<dyn InterestSettingsReader>,
        dirty: Arc<AtomicBool>,
    ) -> Result<Self, AppError> {
        settings.snapshot()?.settings.validate()?;
        let (active, activated) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let (finished_sender, finished) = watch::channel(false);
        let task = tokio::spawn(async move {
            // Sender 在 panic 时也释放；宿主随后关闭通道，不假装后台仍正常。
            let result = run(
                memory, interests, observer, deriver, settings, dirty, activated, stopped,
            )
            .await;
            let _ = finished_sender.send(true);
            result
        });
        Ok(Self {
            active,
            stop,
            finished,
            task,
        })
    }
    pub(crate) fn activate(&self) {
        let _ = self.active.send(true);
    }
    pub(crate) fn finished(&self) -> watch::Receiver<bool> {
        self.finished.clone()
    }
    pub(crate) fn request_stop(&self) {
        let _ = self.stop.send(true);
    }
    pub(crate) async fn stop(self) -> Result<(), AppError> {
        self.request_stop();
        self.task
            .await
            .map_err(|_| "兴趣观察后台异常；已停止准入")?
    }
}

async fn stop_requested(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

async fn observation_paused(
    settings: &dyn InterestSettingsReader,
    began: &InterestSettingsSnapshot,
) {
    let mut revision = began.revision;
    loop {
        match settings.changed(revision).await {
            Ok(next)
                if next.settings.enabled && next.paused_at_revision == began.paused_at_revision =>
            {
                revision = next.revision;
            }
            // 包含快速暂停再启用，以及配置提交未知时的关闭；都必须终结此批次。
            _ => return,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    memory: Arc<dyn MemoryAdmin>,
    interests: Arc<dyn InterestAdmin>,
    observer: Arc<dyn InterestObserver>,
    deriver: Arc<dyn InterestGoalDeriver>,
    settings: Arc<dyn InterestSettingsReader>,
    dirty: Arc<AtomicBool>,
    mut active: watch::Receiver<bool>,
    mut stopped: watch::Receiver<bool>,
) -> Result<(), AppError> {
    loop {
        if *active.borrow() {
            break;
        }
        tokio::select! {
            biased;
            _ = stop_requested(&mut stopped) => return Ok(()),
            changed = active.changed() => if changed.is_err() { return Ok(()); },
        }
    }
    // 启动后先核对一次，补齐上次进程在兴趣保存与目标同步之间退出留下的差异。
    dirty.store(true, Ordering::SeqCst);
    let mut observing = true;
    loop {
        if *stopped.borrow() {
            return Ok(());
        }
        let current = settings.snapshot()?;
        if !current.settings.enabled {
            tokio::select! {
                biased;
                _ = stop_requested(&mut stopped) => return Ok(()),
                changed = settings.changed(current.revision) => { changed?; },
            }
            continue;
        }
        if observing {
            let mut scopes = memory.scopes()?;
            if scopes.len() > eve_memory_api::MAX_EVIDENCE {
                return Err("兴趣观察范围超过上限".into());
            }
            scopes.sort();
            scopes.dedup();
            for scope in scopes {
                if *stopped.borrow() {
                    return Ok(());
                }
                // 此组合入口只绑定 QQ；其他受信宿主需自行装配跨通道读取权限。
                if scope.channel != "qq" {
                    continue;
                }
                let current = settings.snapshot()?;
                if !current.settings.enabled {
                    break;
                }
                let snapshot = memory.reader(scope.clone())?.snapshot()?;
                if snapshot.scope != scope {
                    return Err("兴趣观察来源作用域不匹配".into());
                }
                let batch = match interests.reserve(
                    &snapshot,
                    now_ms()?,
                    observer.version(),
                    &current.settings.observation,
                ) {
                    Ok(Some(batch)) => batch,
                    Ok(None) => continue,
                    // 容量满保留原记录：停止新观察，已有兴趣仍继续同步目标。
                    Err(InterestError::LimitReached) => {
                        observing = false;
                        break;
                    }
                    Err(error) => return Err(error.into()),
                };
                let began = batch.started_at_ms;
                let outcome = {
                    let request = observer.observe(batch.clone());
                    tokio::pin!(request);
                    tokio::select! {
                        biased;
                        _ = stop_requested(&mut stopped) => ObservationOutcome::Failed(ObservationFailure::Cancelled),
                        _ = observation_paused(settings.as_ref(), &current) => ObservationOutcome::Failed(ObservationFailure::Cancelled),
                        result = tokio::time::timeout(Duration::from_secs(30), &mut request) => match result {
                            Ok(Ok(updates)) => ObservationOutcome::Completed(updates),
                            Ok(Err(InterestError::Observation(failure))) => ObservationOutcome::Failed(failure),
                            Ok(Err(_)) => ObservationOutcome::Failed(ObservationFailure::Provider),
                            Err(_) => ObservationOutcome::Failed(ObservationFailure::Timeout),
                        },
                    }
                };
                // 远端请求可能已经发生；取消、超时或失败也消费这批证据，恢复不重试。
                let results = interests.finish(&batch, now_ms()?.max(began), outcome)?;
                if !results.is_empty() {
                    dirty.store(true, Ordering::SeqCst);
                }
            }
        }
        if settings.snapshot()?.settings.enabled && dirty.swap(false, Ordering::SeqCst) {
            let mut records = Vec::new();
            for scope in interests.scopes()? {
                records.extend(interests.snapshot(&scope)?.interests);
            }
            let report = deriver.reconcile(&records, now_ms()?)?;
            if !report.deferred.is_empty() {
                // 修订冲突或容量不足只暂缓；下一轮重新读取后再核对，不调用模型。
                dirty.store(true, Ordering::SeqCst);
            }
        }
        tokio::select! {
            biased;
            _ = stop_requested(&mut stopped) => return Ok(()),
            changed = settings.changed(current.revision) => { changed?; },
            _ = tokio::time::sleep(Duration::from_millis(250)) => {},
        }
    }
}

/// /interests 与 /forget-interest；只读写当前会话自己的兴趣。
pub(crate) struct Commands {
    enabled: Option<Enabled>,
}
struct Enabled {
    interests: Arc<dyn InterestAdmin>,
    deriver: Arc<dyn InterestGoalDeriver>,
    cognition: Arc<dyn CognitionAdmin>,
    dirty: Arc<AtomicBool>,
}
impl Commands {
    pub(crate) fn disabled() -> Arc<dyn QqCommandHandler> {
        Arc::new(Self { enabled: None })
    }
    pub(crate) fn enabled(
        interests: Arc<dyn InterestAdmin>,
        deriver: Arc<dyn InterestGoalDeriver>,
        cognition: Arc<dyn CognitionAdmin>,
        dirty: Arc<AtomicBool>,
    ) -> Arc<dyn QqCommandHandler> {
        Arc::new(Self {
            enabled: Some(Enabled {
                interests,
                deriver,
                cognition,
                dirty,
            }),
        })
    }
}

enum Command<'a> {
    List,
    Forget(&'a str),
    Help,
}

fn parse(text: &str) -> Option<Command<'_>> {
    let text = text.trim();
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    let (name, tail) = text.split_at(end);
    let tail = tail.trim();
    Some(match name {
        "/interests" if tail.is_empty() => Command::List,
        "/interests" => Command::Help,
        "/forget-interest"
            if validate_id(tail).is_ok() && !tail.chars().any(char::is_whitespace) =>
        {
            Command::Forget(tail)
        }
        "/forget-interest" => Command::Help,
        _ => return None,
    })
}

impl QqCommandHandler for Commands {
    fn handle(&self, input: QqCommandInput<'_>) -> PluginResult<Option<String>> {
        let Some(command) = parse(input.text) else {
            return Ok(None);
        };
        let Some(enabled) = &self.enabled else {
            return Ok(Some(DISABLED.into()));
        };
        let scope = crate::qq_memory::scope(input.session);
        match command {
            Command::Help => Ok(Some(HELP.into())),
            Command::List => {
                let snapshot = enabled.interests.snapshot(&scope).map_err(failure)?;
                let goals = enabled.cognition.snapshot().map_err(|_| failure_text())?;
                if snapshot.interests.is_empty() {
                    return Ok(Some("当前会话还没有记录兴趣。".into()));
                }
                let mut reply = String::from(
                    "已记录的兴趣（来自你的原话；可用 /forget-interest 兴趣ID 撤回）：",
                );
                for interest in &snapshot.interests {
                    let goal = goals
                        .state
                        .goals
                        .get(&learning_goal_id(SUBJECT, &interest.id))
                        .map_or("未派生", |goal| match goal.status {
                            GoalStatus::Waiting => "后台学习中",
                            GoalStatus::Cancelled => "已取消",
                            GoalStatus::Completed => "已完成",
                            GoalStatus::Blocked => "已阻塞",
                            GoalStatus::Ready | GoalStatus::Executing => "进行中",
                        });
                    let _ = write!(
                        reply,
                        "\n- {}｜{}｜{}｜修订 {}｜学习目标：{}｜最近原话：“{}”",
                        interest.id,
                        interest.topic,
                        match interest.status {
                            InterestStatus::Active => "有效",
                            InterestStatus::Withdrawn => "已撤回",
                        },
                        interest.revision,
                        goal,
                        preview(latest_quote(interest)),
                    );
                }
                Ok(Some(reply))
            }
            Command::Forget(id) => {
                let request = WithdrawalRequest {
                    evidence_id: format!("qq-interest-command-{}", digest(&input)),
                    message_id: input.message_id.into(),
                    text: input.text.trim().into(),
                    at_ms: now_ms().map_err(|_| failure_text())?,
                };
                let record = match enabled.interests.withdraw(&scope, id, request) {
                    Ok(record) => record,
                    Err(InterestError::NotFound) => {
                        return Ok(Some(
                            "当前会话没有这条兴趣。发送 /interests 查看兴趣 ID。".into(),
                        ));
                    }
                    Err(InterestError::InvalidInput) => return Ok(Some(HELP.into())),
                    Err(error) => return Err(failure(error)),
                };
                // 立即同步对应学习目标；冲突或失败时交给后台下一轮核对。
                let at_ms = now_ms().map_err(|_| failure_text())?;
                let cancelled = match enabled.deriver.reconcile(&[record], at_ms) {
                    Ok(report) if report.deferred.is_empty() => true,
                    Ok(_) => false,
                    Err(InterestError::Derivation) => return Err(failure_text()),
                    Err(error) => return Err(failure(error)),
                };
                enabled.dirty.store(true, Ordering::SeqCst);
                Ok(Some(if cancelled {
                    "已撤回这条兴趣；之后不会再据此后台学习或主动提起。".into()
                } else {
                    "已撤回这条兴趣；对应学习目标将在后台取消。".into()
                }))
            }
        }
    }
}

fn latest_quote(interest: &InterestRecord) -> &str {
    interest
        .statements
        .iter()
        .rev()
        .find(|statement| statement.kind != StatementKind::Withdrawal)
        .map_or("", |statement| statement.quote.as_str())
}

fn preview(text: &str) -> String {
    if text.len() <= PREVIEW_BYTES {
        return text.into();
    }
    let mut end = PREVIEW_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn digest(input: &QqCommandInput<'_>) -> String {
    let mut hash = Context::new(&SHA256);
    for part in [
        b"qq".as_slice(),
        input.session.session_id.as_bytes(),
        input.session.user_id.as_bytes(),
        input.message_id.as_bytes(),
    ] {
        hash.update(&(part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    hash.finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn failure_text() -> PluginError {
    PluginError::State("兴趣或学习目标状态无法确认；服务已停止，请重新打开后查看持久状态。".into())
}

fn failure(error: InterestError) -> PluginError {
    match error {
        InterestError::LimitReached => PluginError::State(error.to_string()),
        _ => failure_text(),
    }
}
