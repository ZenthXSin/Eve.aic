//! 原会话条件唤醒：有限的本地观察、持久准入、原会话 Control 与真实平台回执。
use crate::AppError;
use eve_control_api::{ControlInput, ControlPhase, ControlService, DiscardControlEvents};
use eve_llm_api::ContextScope;
use eve_llm_api::{
    Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext, ToolExecutionError,
    ToolFuture, ToolValidationError,
};
use eve_plugin_api::{PluginError, PluginResult};
use eve_qqbot_plugin::{
    QqAdmission, QqAdmissionResult, QqConversations, QqOutreach, QqOutreachFuture,
    QqOutreachMessage, QqOutreachMoment, QqOutreachPush, QqOutreachResult,
};
use eve_session_api::{SessionInput, SessionKey, SessionService, SessionTurnStatus};
use eve_task_api::TaskAdmin;
use eve_trigger_api::{
    Condition, Delivery, INPUT_PREFIX, MatchMode, Recurrence, Status, Trigger, TriggerAdmin,
    TriggerSpec,
};
use ring::digest::{SHA256, digest};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::watch,
    task::{JoinHandle, JoinSet},
};

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(1, |d| d.as_millis() as u64)
}
fn failed(s: impl ToString) -> ToolExecutionError {
    ToolExecutionError::Failed(s.to_string())
}
fn plugin_error(e: impl ToString) -> PluginError {
    PluginError::State(e.to_string())
}
fn invalid(s: &str) -> ToolValidationError {
    ToolValidationError { message: s.into() }
}
fn reject_pending_delivery(admin: &dyn TriggerAdmin, id: &str) -> eve_trigger_api::Result<()> {
    match admin.delivery(id, Delivery::Pending, Delivery::Failed) {
        // 后台和通道可能同时读到 Pending；保留另一方已记录的结局，不覆盖未知发送。
        Ok(()) | Err(eve_trigger_api::TriggerError::Transition) => Ok(()),
        Err(e) => Err(e),
    }
}
fn owner(c: &ToolExecutionContext) -> Result<SessionKey, ToolExecutionError> {
    if c.is_cancel_requested() {
        return Err(ToolExecutionError::Cancelled("本轮已取消".into()));
    }
    let s = c.scope().ok_or_else(|| failed("触发器需要可信会话"))?;
    SessionKey::new(&s.session_id, &s.user_id).map_err(failed)
}

pub(crate) struct Observations {
    pub(crate) sessions: Arc<dyn SessionService>,
    pub(crate) tasks: Option<Arc<dyn TaskAdmin>>,
    workspace: Option<PathBuf>,
    operators: BTreeSet<String>,
    authorization: Option<Arc<dyn eve_developer_api::DeveloperAuthorization>>,
    processes: bool,
    admission: Option<Arc<crate::qq_subject_admission::Admission>>,
}
impl Observations {
    pub(crate) fn new(
        sessions: Arc<dyn SessionService>,
        tasks: Option<Arc<dyn TaskAdmin>>,
    ) -> Result<Self, AppError> {
        let config = match std::env::var_os("EVE_DEVELOPER_CONFIG") {
            Some(path) => {
                let bytes = std::fs::read(path)?;
                if bytes.len() > 32768 {
                    return Err("开发配置过大".into());
                }
                let c: eve_developer_api::DeveloperConfig = serde_json::from_slice(&bytes)?;
                c.validate()?;
                Some(c)
            }
            None => None,
        };
        Ok(Self {
            sessions,
            tasks,
            workspace: config.as_ref().map(|c| c.workspace.clone()),
            operators: config
                .as_ref()
                .map_or_else(BTreeSet::new, |c| c.operators.clone()),
            authorization: None,
            admission: None,
            processes: config.is_some_and(|c| c.commands),
        })
    }
    pub(crate) fn with_admission(
        mut self,
        admission: Arc<crate::qq_subject_admission::Admission>,
    ) -> Self {
        self.admission = Some(admission);
        self
    }
    fn begin_admission(&self, t: &Trigger) -> Result<Option<String>, AppError> {
        match &self.admission {
            None => Ok(Some(String::new())),
            Some(admission) => Ok(admission.begin_work(
                &t.owner,
                &format!("trigger:{}:{}", t.id, t.runs),
                eve_subject_api::EventPriority::Recovery,
                eve_subject_api::EventSource::RecoverySupervisor,
            )?),
        }
    }
    fn finish_admission(&self, ticket: &str, result: QqAdmissionResult) -> Result<(), AppError> {
        if let Some(admission) = &self.admission {
            admission.finish(ticket, result)?;
        }
        Ok(())
    }
    pub(crate) fn with_authorization(
        mut self,
        authorization: Arc<dyn eve_developer_api::DeveloperAuthorization>,
    ) -> Self {
        self.authorization = Some(authorization);
        self
    }
    fn authorized(&self, owner: &SessionKey) -> bool {
        self.operators.contains(&owner.user_id)
            || self.authorization.as_ref().is_some_and(|a| {
                a.is_operator(&ContextScope {
                    session_id: owner.session_id.clone(),
                    user_id: owner.user_id.clone(),
                })
            })
    }
    fn file(&self, owner: &SessionKey, path: &str) -> Result<String, ToolExecutionError> {
        if !self.authorized(owner) {
            return Err(failed("文件触发仅限已授权开发操作者"));
        }
        let root = self
            .workspace
            .as_ref()
            .ok_or_else(|| failed("未配置文件观察目录"))?
            .canonicalize()
            .map_err(|_| failed("文件观察目录不可用"))?;
        let relative = Path::new(path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(failed("文件路径必须是开发目录内的相对路径，不允许 .."));
        }
        let mut current = root.clone();
        for part in relative.components() {
            current.push(part);
            match current.canonicalize() {
                Ok(real) if real.starts_with(&root) => current = real,
                Ok(_) => return Err(failed("文件路径越过授权目录")),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok("missing".into()),
                Err(_) => return Err(failed("文件无法观察")),
            }
        }
        let mut f = std::fs::File::open(current).map_err(|_| failed("文件无法读取"))?;
        let metadata = f.metadata().map_err(|_| failed("文件元数据不可用"))?;
        if !metadata.is_file() || metadata.len() > 1024 * 1024 {
            return Err(failed("只观察 1 MiB 以内的普通文件"));
        }
        let mut bytes = vec![];
        f.by_ref()
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| failed("文件读取失败"))?;
        if bytes.len() > 1024 * 1024 {
            return Err(failed("文件超过观察上限"));
        }
        Ok(digest(&SHA256, &bytes)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    }
    fn process(&self, owner: &SessionKey, pid: u32) -> Result<String, ToolExecutionError> {
        if !self.processes || !self.authorized(owner) {
            return Err(failed("进程观察仅限已启用命令的开发操作者"));
        }
        if !cfg!(target_os = "linux") {
            return Err(failed("当前进程观察仅支持 Linux"));
        }
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok("missing".into()),
            Err(_) => return Err(failed("进程状态不可读")),
        };
        let fields: Vec<_> = stat
            .rsplit_once(')')
            .ok_or_else(|| failed("进程状态无效"))?
            .1
            .split_whitespace()
            .collect();
        if fields.first().is_some_and(|s| matches!(*s, "Z" | "X")) {
            return Ok("missing".into());
        }
        let start = fields.get(19).ok_or_else(|| failed("进程标识无效"))?;
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map_err(|_| failed("系统标识不可用"))?;
        Ok(format!("{}:{start}", boot.trim()))
    }
    fn latest_turn(&self, owner: &SessionKey) -> Result<u64, ToolExecutionError> {
        Ok(self
            .sessions
            .snapshot(owner)
            .map_err(failed)?
            .map_or(0, |s| s.turns.last().map_or(0, |t| t.id)))
    }
    fn task_finished(&self, owner: &SessionKey, id: &str) -> Result<bool, ToolExecutionError> {
        let snapshot = self
            .tasks
            .as_ref()
            .ok_or_else(|| failed("未启用后台任务"))?
            .snapshot()
            .map_err(failed)?;
        let t = snapshot
            .tasks
            .iter()
            .find(|t| {
                t.id == id
                    && t.owner.session_id == owner.session_id
                    && t.owner.user_id == owner.user_id
            })
            .ok_or_else(|| failed("没有本会话的这个后台任务"))?;
        Ok(t.status.finished())
    }
    fn evaluate(&self, t: &Trigger) -> Result<Option<Vec<String>>, ToolExecutionError> {
        let mut evidence = vec![];
        for (i, c) in t.spec.conditions.iter().enumerate() {
            let matched = match c {
                Condition::Time { at_ms } => now_ms() >= *at_ms,
                Condition::FileChanged { path, baseline } => {
                    self.file(&t.owner, path)? != *baseline
                }
                Condition::TaskFinished { task_id } => self.task_finished(&t.owner, task_id)?,
                Condition::MessageContains { text, after_turn } => self
                    .sessions
                    .snapshot(&t.owner)
                    .map_err(failed)?
                    .is_some_and(|s| {
                        s.turns.iter().any(|turn| {
                            turn.id > *after_turn
                                && matches!(turn.status, SessionTurnStatus::Completed { .. })
                                && !turn.input.starts_with(INPUT_PREFIX)
                                && turn.input.contains(text)
                        })
                    }),
                Condition::ProcessState {
                    pid,
                    running,
                    identity,
                } => {
                    let actual = self.process(&t.owner, *pid)?;
                    let same =
                        actual != "missing" && (identity == "missing" || &actual == identity);
                    same == *running
                }
            };
            if matched {
                evidence.push(format!("condition_{i}:matched"));
            }
        }
        let matched = match t.spec.mode {
            MatchMode::All => evidence.len() == t.spec.conditions.len(),
            MatchMode::Any => !evidence.is_empty(),
        };
        Ok(matched.then_some(evidence))
    }
}

pub(crate) struct Tools {
    pub(crate) admin: Arc<dyn TriggerAdmin>,
    pub(crate) observations: Arc<std::sync::OnceLock<Arc<Observations>>>,
    pub(crate) conversations: QqConversations,
    pub(crate) operation: &'static str,
}
impl Tool for Tools {
    fn definition(&self) -> ToolDefinition {
        let (description, schema) = match self.operation {
            "set_wakeup_trigger" => (
                "创建条件触发器，成功后持续保存，跨对话和进程重启有效，不需要每轮重新开启；可在已有授权目标内自行设置。条件满足时在当前会话自动唤醒你，读取原会话历史并执行 instructions，结果主动发回原会话。conditions 有 1–8 项，mode=all/any。type=time 使用 after_seconds 或 at_ms；file_changed 使用开发目录内相对 path；task_finished 使用本会话 task_id；message_contains 使用 text（只匹配创建后完成的用户轮次）；process_state 使用 pid 和 running（只观察该进程实例）。文件和进程仅限授权操作者。expires_in_seconds 默认7天、最多90天。可用 interval_seconds 设置有限周期（仅单一时间条件，max_runs 默认48、最多128）；每30分钟目标检查使用 interval_seconds=1800。resume_task_id 接续本会话已有 blocked 后台任务，保留原任务、步数与期限，不从头执行；与周期互斥。检查不调用模型；执行与投递未知时不重放。",
                json!({"type":"object","properties":{
                "title":{"type":"string"},"instructions":{"type":"string"},"mode":{"type":"string","enum":["all","any"]},
                "expires_in_seconds":{"type":"integer","minimum":1,"maximum":7776000},
                "interval_seconds":{"type":"integer","minimum":60,"maximum":7776000},
                "max_runs":{"type":"integer","minimum":2,"maximum":128},
                "resume_task_id":{"type":"string"},
                "conditions":{"type":"array","minItems":1,"maxItems":8,"items":{"type":"object","properties":{
                    "type":{"type":"string","enum":["time","file_changed","task_finished","message_contains","process_state"]},
                    "after_seconds":{"type":"integer","minimum":1},"at_ms":{"type":"integer","minimum":1},"path":{"type":"string"},"task_id":{"type":"string"},"text":{"type":"string"},"pid":{"type":"integer","minimum":1},"running":{"type":"boolean"}},"required":["type"],"additionalProperties":false}}
            },"required":["title","instructions","conditions"],"additionalProperties":false}),
            ),
            "cancel_wakeup_trigger" => (
                "取消当前会话的一条触发器，执行中时请求停止；不能撤销已经发生的工具副作用。",
                json!({"type":"object","properties":{"trigger_id":{"type":"string"}},"required":["trigger_id"],"additionalProperties":false}),
            ),
            _ => (
                "查看当前会话的触发器、条件、执行结果和实际投递状态；不能查看其他会话。",
                json!({"type":"object","properties":{},"additionalProperties":false}),
            ),
        };
        ToolDefinition {
            name: self.operation.into(),
            description: description.into(),
            argument_schema: schema,
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial {
                scope: "eve.triggers".into(),
            }),
        }
    }
    fn validate_arguments(&self, v: &Value) -> Result<(), ToolValidationError> {
        let o = v.as_object().ok_or_else(|| invalid("参数必须是对象"))?;
        let allowed: &[&str] = match self.operation {
            "set_wakeup_trigger" => &[
                "title",
                "instructions",
                "conditions",
                "mode",
                "expires_in_seconds",
                "interval_seconds",
                "max_runs",
                "resume_task_id",
            ],
            "cancel_wakeup_trigger" | "list_wakeup_triggers" => &["trigger_id"],
            _ => &[],
        };
        if o.keys().any(|k| !allowed.contains(&k.as_str())) {
            return Err(invalid("未知参数"));
        }
        if self.operation == "set_wakeup_trigger" {
            for (key, max) in [("title", 160), ("instructions", 4096)] {
                if !o
                    .get(key)
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.trim().is_empty() && s.trim() == s && s.len() <= max)
                {
                    return Err(invalid("标题或指令无效"));
                }
            }
            if !o
                .get("conditions")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty() && a.len() <= 8)
                || o.get("mode")
                    .is_some_and(|s| !matches!(s.as_str(), Some("all" | "any")))
                || o.get("expires_in_seconds")
                    .is_some_and(|s| !s.as_u64().is_some_and(|n| (1..=7776000).contains(&n)))
            {
                return Err(invalid("触发条件或有效期无效"));
            }
            if o.get("interval_seconds")
                .is_some_and(|v| !v.as_u64().is_some_and(|n| (60..=7776000).contains(&n)))
                || o.get("max_runs")
                    .is_some_and(|v| !v.as_u64().is_some_and(|n| (2..=128).contains(&n)))
                || (o.contains_key("max_runs") && !o.contains_key("interval_seconds"))
                || o.get("resume_task_id").is_some_and(|v| {
                    !v.as_str()
                        .is_some_and(|id| !id.is_empty() && id.len() <= 256)
                })
            {
                return Err(invalid("周期或接续任务参数无效"));
            }
        } else if self.operation == "cancel_wakeup_trigger"
            && !o
                .get("trigger_id")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty() && s.len() <= 256)
        {
            return Err(invalid("trigger_id 无效"));
        }
        Ok(())
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            let owner = owner(&context)?;
            if self
                .conversations
                .identity(&owner)
                .map_err(failed)?
                .is_none()
            {
                return Err(failed("没有已验证的 QQ 会话路由"));
            }
            let observations = self
                .observations
                .get()
                .ok_or_else(|| failed("条件观察服务未就绪"))?;
            match self.operation {
                "list_wakeup_triggers" => {
                    let selected = call.arguments.get("trigger_id").and_then(Value::as_str);
                    let mut records: Vec<_> = self
                        .admin
                        .snapshot()
                        .map_err(failed)?
                        .into_iter()
                        .filter(|t| t.owner == owner && selected.is_none_or(|id| id == t.id))
                        .collect();
                    let total = records.len();
                    records.reverse();
                    let views: Vec<_> = records.into_iter().take(20).map(|t| json!({
                        "id":t.id,"title":t.spec.title,"status":t.status,"delivery":t.delivery,
                        "mode":t.spec.mode,"conditions":t.spec.conditions,"expires_at_ms":t.spec.expires_at_ms,
                        "recurrence":t.spec.recurrence,"runs":t.runs,"resume_task_id":t.spec.resume_task_id,
                        "instructions":eve_task_api::bounded(&t.spec.instructions,1024),
                        "result":t.result.as_deref().map(|s|eve_task_api::bounded(s,if selected.is_some(){4000}else{500}))
                    })).collect();
                    Ok(json!({"triggers":views,"total":total}))
                }
                "cancel_wakeup_trigger" => {
                    self.admin
                        .cancel(
                            &owner,
                            call.arguments["trigger_id"].as_str().unwrap_or_default(),
                        )
                        .map_err(failed)?;
                    Ok(json!({"cancelled":true}))
                }
                _ => {
                    if !observations.authorized(&owner) {
                        return Err(failed("创建条件触发器需要主人或管理员授权"));
                    }
                    let now = now_ms();
                    let a = &call.arguments;
                    let mut conditions = vec![];
                    for c in a["conditions"]
                        .as_array()
                        .ok_or_else(|| failed("conditions 无效"))?
                    {
                        let string = |k: &str| {
                            c[k].as_str()
                                .filter(|s| !s.is_empty())
                                .ok_or_else(|| failed("条件字段无效"))
                        };
                        let allowed: &[&str] = match string("type")? {
                            "time" => &["type", "after_seconds", "at_ms"],
                            "file_changed" => &["type", "path"],
                            "task_finished" => &["type", "task_id"],
                            "message_contains" => &["type", "text"],
                            "process_state" => &["type", "pid", "running"],
                            _ => return Err(failed("未知条件类型")),
                        };
                        if c.as_object()
                            .is_none_or(|o| o.keys().any(|k| !allowed.contains(&k.as_str())))
                        {
                            return Err(failed("条件包含未知字段"));
                        }
                        conditions.push(match string("type")? {
                            "time" => {
                                let at = match (c.get("after_seconds"), c.get("at_ms")) {
                                    (Some(seconds), None) => seconds
                                        .as_u64()
                                        .filter(|n| (1..=7776000).contains(n))
                                        .and_then(|n| now.checked_add(n * 1000)),
                                    (None, Some(at)) => at.as_u64().filter(|n| *n > now),
                                    _ => None,
                                }
                                .ok_or_else(|| {
                                    failed("时间条件需要未来 at_ms 或 after_seconds，二选一")
                                })?;
                                Condition::Time { at_ms: at }
                            }
                            "file_changed" => {
                                let path = string("path")?;
                                Condition::FileChanged {
                                    path: path.into(),
                                    baseline: observations.file(&owner, path)?,
                                }
                            }
                            "task_finished" => {
                                let id = string("task_id")?;
                                observations.task_finished(&owner, id)?;
                                Condition::TaskFinished { task_id: id.into() }
                            }
                            "message_contains" => Condition::MessageContains {
                                text: string("text")?.into(),
                                after_turn: observations.latest_turn(&owner)?,
                            },
                            "process_state" => {
                                let pid = c["pid"]
                                    .as_u64()
                                    .and_then(|n| u32::try_from(n).ok())
                                    .filter(|n| *n > 0)
                                    .ok_or_else(|| failed("pid 无效"))?;
                                Condition::ProcessState {
                                    pid,
                                    running: c["running"]
                                        .as_bool()
                                        .ok_or_else(|| failed("running 无效"))?,
                                    identity: observations.process(&owner, pid)?,
                                }
                            }
                            _ => unreachable!(),
                        });
                    }
                    if let Some(id) = a["resume_task_id"].as_str() {
                        let snapshot = observations
                            .tasks
                            .as_ref()
                            .ok_or_else(|| failed("后台任务未启用"))?
                            .snapshot()
                            .map_err(failed)?;
                        if !snapshot.tasks.iter().any(|t| {
                            t.id == id
                                && t.owner.session_id == owner.session_id
                                && t.owner.user_id == owner.user_id
                                && t.status == eve_task_api::TaskStatus::Blocked
                        }) {
                            return Err(failed("接续目标必须是本会话的 blocked 后台任务"));
                        }
                    }
                    let source = format!("{}:{}", context.turn_id().unwrap_or(0), call.id);
                    let source: String = digest(&SHA256, source.as_bytes())
                        .as_ref()
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    let spec = TriggerSpec {
                        title: a["title"].as_str().unwrap_or_default().into(),
                        instructions: a["instructions"].as_str().unwrap_or_default().into(),
                        mode: if a["mode"] == "any" {
                            MatchMode::Any
                        } else {
                            MatchMode::All
                        },
                        conditions,
                        expires_at_ms: now
                            + a["expires_in_seconds"].as_u64().unwrap_or(7 * 86400) * 1000,
                        recurrence: a["interval_seconds"].as_u64().map(|n| Recurrence {
                            interval_ms: n * 1000,
                            max_runs: a["max_runs"].as_u64().unwrap_or(48) as u32,
                        }),
                        resume_task_id: a["resume_task_id"].as_str().map(str::to_owned),
                    };
                    if spec.conditions.iter().any(
                        |c| matches!(c, Condition::Time { at_ms } if *at_ms >= spec.expires_at_ms),
                    ) {
                        return Err(failed(
                            "时间条件必须早于触发器有效期，请延长 expires_in_seconds",
                        ));
                    }
                    let t = self
                        .admin
                        .create(owner, source, spec, now)
                        .map_err(failed)?;
                    Ok(json!({"created":true,"trigger":t,"current_time_ms":now}))
                }
            }
        })
    }
}

pub(crate) struct Background {
    active: watch::Sender<bool>,
    stop: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
    task: JoinHandle<Result<(), AppError>>,
}
impl Background {
    pub(crate) fn start(
        admin: Arc<dyn TriggerAdmin>,
        observations: Arc<Observations>,
        control: Arc<dyn ControlService>,
        conversations: QqConversations,
    ) -> Self {
        let (active, activated) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let (done, finished) = watch::channel(false);
        let task = tokio::spawn(async move {
            let result = run(
                admin,
                observations,
                control,
                conversations,
                activated,
                stopped,
            )
            .await;
            let _ = done.send(true);
            result
        });
        Self {
            active,
            stop,
            finished,
            task,
        }
    }
    pub(crate) fn activate(&self) {
        let _ = self.active.send(true);
    }
    pub(crate) fn request_stop(&self) {
        let _ = self.stop.send(true);
    }
    pub(crate) fn finished(&self) -> watch::Receiver<bool> {
        self.finished.clone()
    }
    pub(crate) async fn stop(self) -> Result<(), AppError> {
        self.request_stop();
        self.task.await.map_err(|_| "条件唤醒后台异常")?
    }
}
fn transition(
    admin: &dyn TriggerAdmin,
    id: &str,
    from: Status,
    to: Status,
    result: Option<String>,
) -> eve_trigger_api::Result<bool> {
    match admin.transition(id, from, to, result) {
        Ok(()) => Ok(true),
        // CAS 的输家没有状态所有权，不能覆盖取消或令整个后台退出。
        Err(eve_trigger_api::TriggerError::Transition) => Ok(false),
        Err(error) => Err(error),
    }
}
fn failure_receipt(t: &Trigger, stage: &str, execution: &str, reason: &str) -> String {
    json!({"kind":"trigger_failure","trigger_id":t.id,"task_id":t.spec.resume_task_id,"run":t.runs,
        "stage":stage,"execution":execution,"replay_safe":execution=="not_started" && t.spec.resume_task_id.is_none(),
        "reason":reason,"evidence":t.evidence,
        "reproduction":{"mode":"simulation_only","trigger_id":t.id,
            "instruction":"先查看原任务检查点与回执，只在隔离模拟环境复现；不得直接重放工具"}})
    .to_string()
}
struct ActiveWakeup {
    control: Arc<dyn ControlService>,
    key: Option<eve_control_api::GenerationKey>,
}
impl Drop for ActiveWakeup {
    fn drop(&mut self) {
        if let Some(key) = &self.key {
            let _ = self.control.cancel(key);
        }
    }
}
async fn finish_wakeup(
    admin: Arc<dyn TriggerAdmin>,
    control: Arc<dyn ControlService>,
    t: Trigger,
    key: eve_control_api::GenerationKey,
    mut stop: watch::Receiver<bool>,
) -> Result<(), AppError> {
    // 后台错误或停机超时中止等待 Future 时，也撤销仍在运行的原会话代。
    let mut active = ActiveWakeup {
        control: control.clone(),
        key: Some(key.clone()),
    };
    let wait = control.wait(&key);
    tokio::pin!(wait);
    let report = loop {
        tokio::select! {
            r=&mut wait=>break r,
            _=stop.changed()=>{ let _=control.cancel(&key); },
            _=tokio::time::sleep(Duration::from_millis(100))=>{
                if *stop.borrow() || admin.snapshot()?.iter().any(|v|v.id==t.id&&v.status==Status::Cancelled) {
                    let _=control.cancel(&key);
                }
            }
        }
    };
    active.key = None;
    let (state, result) = match report {
        Ok(r) if !r.cancel_requested && r.run.failure.is_none() => {
            match r.run.text.filter(|s| !s.trim().is_empty()) {
                Some(text) => (Status::Completed, Some(eve_task_api::bounded(&text, 32768))),
                None => (
                    Status::Failed,
                    Some(failure_receipt(
                        &t,
                        "result",
                        "finished",
                        "模型没有给出结果",
                    )),
                ),
            }
        }
        _ => (
            Status::Failed,
            Some(failure_receipt(
                &t,
                "execution",
                "unknown",
                "唤醒失败或取消；已有副作用不重放",
            )),
        ),
    };
    transition(admin.as_ref(), &t.id, Status::Running, state, result)?;
    Ok(())
}

/// 同步受理与主体回执一起结算；等待模型结果由独立等待者处理。
fn submit_wakeup(
    control: &dyn ControlService,
    observations: &Observations,
    ticket: &str,
    input: ControlInput,
) -> Result<eve_control_api::ControlResult<eve_control_api::GenerationKey>, AppError> {
    let result = control.submit(input, Arc::new(DiscardControlEvents));
    observations.finish_admission(
        ticket,
        match &result {
            Ok(_) => QqAdmissionResult::Started,
            Err(eve_control_api::ControlError::Busy) => QqAdmissionResult::Retry,
            Err(_) => QqAdmissionResult::Rejected,
        },
    )?;
    Ok(result)
}

async fn run(
    admin: Arc<dyn TriggerAdmin>,
    observations: Arc<Observations>,
    control: Arc<dyn ControlService>,
    conversations: QqConversations,
    mut activated: watch::Receiver<bool>,
    mut stop: watch::Receiver<bool>,
) -> Result<(), AppError> {
    while !*activated.borrow() {
        tokio::select! {_ = activated.changed()=>{},_ = stop.changed()=>return Ok(())}
    }
    let mut joins: JoinSet<Result<(), AppError>> = JoinSet::new();
    loop {
        while let Some(result) = joins.try_join_next() {
            result.map_err(|_| "条件执行后台异常")??;
        }
        if *stop.borrow() {
            break;
        }
        for mut t in admin.snapshot()? {
            if *stop.borrow() {
                break;
            }
            if t.delivery == Some(Delivery::Pending) && !observations.authorized(&t.owner) {
                reject_pending_delivery(admin.as_ref(), &t.id)?;
                continue;
            }
            if t.status == Status::Completed && t.spec.recurrence.is_some() {
                match admin.rearm(&t.id, now_ms()) {
                    Ok(()) => continue,
                    Err(eve_trigger_api::TriggerError::Transition) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            if !matches!(t.status, Status::Armed | Status::Ready) {
                if let Some(admission) = &observations.admission {
                    admission.abandon(&t.owner, &format!("trigger:{}:{}", t.id, t.runs))?;
                }
                continue;
            }
            if !observations.authorized(&t.owner) {
                match admin.cancel(&t.owner, &t.id) {
                    Ok(()) | Err(eve_trigger_api::TriggerError::Transition) => {}
                    Err(e) => return Err(e.into()),
                }
                continue;
            }
            if now_ms() >= t.spec.expires_at_ms {
                transition(
                    admin.as_ref(),
                    &t.id,
                    t.status,
                    Status::Expired,
                    Some(failure_receipt(
                        &t,
                        "expiry",
                        "not_started",
                        "有效期结束，未执行",
                    )),
                )?;
                continue;
            }
            if t.status == Status::Armed {
                match observations.evaluate(&t) {
                    Ok(Some(evidence)) => match admin.ready(&t.id, evidence.clone()) {
                        Ok(()) => {
                            t.status = Status::Ready;
                            t.evidence = evidence;
                        }
                        Err(eve_trigger_api::TriggerError::Transition) => continue,
                        Err(e) => return Err(e.into()),
                    },
                    Ok(None) => continue,
                    Err(_) => {
                        transition(
                            admin.as_ref(),
                            &t.id,
                            Status::Armed,
                            Status::Failed,
                            Some(failure_receipt(
                                &t,
                                "observation",
                                "not_started",
                                "条件观察失败",
                            )),
                        )?;
                        continue;
                    }
                }
            }
            if let Some(id) = &t.spec.resume_task_id {
                let Some(ticket) = observations.begin_admission(&t)? else {
                    continue;
                };
                if !transition(admin.as_ref(), &t.id, Status::Ready, Status::Running, None)? {
                    observations.finish_admission(&ticket, QqAdmissionResult::Rejected)?;
                    continue;
                }
                t.runs = t.runs.saturating_add(1);
                let resumed = observations
                    .tasks
                    .as_ref()
                    .ok_or("后台任务未启用")?
                    .resume_blocked(
                        &eve_task_api::TaskOwner {
                            session_id: t.owner.session_id.clone(),
                            user_id: t.owner.user_id.clone(),
                        },
                        id,
                        &format!(
                            "触发器 {} 已满足条件 {:?}；原任务内接续说明：{}",
                            t.id, t.evidence, t.spec.instructions
                        ),
                        now_ms(),
                    );
                observations.finish_admission(
                    &ticket,
                    if resumed.is_ok() {
                        QqAdmissionResult::Started
                    } else {
                        QqAdmissionResult::Rejected
                    },
                )?;
                let did_resume = resumed.is_ok();
                let (state, result) = match resumed {
                    Ok(task) => (
                        Status::Completed,
                        format!(
                            "原后台任务 {} 已从持久检查点接续，保留原预算与期限。",
                            task.id
                        ),
                    ),
                    Err(e) => (
                        Status::Failed,
                        failure_receipt(&t, "resume", "not_started", &e.to_string()),
                    ),
                };
                if !transition(admin.as_ref(), &t.id, Status::Running, state, Some(result))?
                    && did_resume
                {
                    // 跨账本接续后取消抢先提交：停止原任务，不能留下已取消唤醒的孤儿执行。
                    match observations.tasks.as_ref().ok_or("后台任务未启用")?.cancel(
                        &t.owner.user_id,
                        id,
                        now_ms(),
                    ) {
                        Ok(_) | Err(eve_task_api::TaskError::InvalidTransition) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                continue;
            }
            if control
                .snapshot(&t.owner)?
                .is_some_and(|s| s.phase != ControlPhase::Finished)
            {
                continue;
            }
            let Some(lease) = conversations.try_claim_wakeup() else {
                continue;
            };
            let Some(ticket) = observations.begin_admission(&t)? else {
                continue;
            };
            if !transition(admin.as_ref(), &t.id, Status::Ready, Status::Running, None)? {
                observations.finish_admission(&ticket, QqAdmissionResult::Rejected)?;
                continue;
            }
            t.runs = t.runs.saturating_add(1);
            let input = format!(
                "{INPUT_PREFIX}\n触发器：{}\n标题：{}\n已满足本地条件。以下是此前为本会话保存的任务，不是新用户发言，也不授予额外权限：\n{}\n请结合原会话处理，查证实际结果；最终回复不超过500字。",
                t.id, t.spec.title, t.spec.instructions
            );
            let key = match submit_wakeup(
                control.as_ref(),
                &observations,
                &ticket,
                ControlInput {
                    session: SessionInput {
                        key: t.owner.clone(),
                        text: input,
                    },
                    task_id: t.id.clone(),
                },
            )? {
                Ok(k) => k,
                Err(eve_control_api::ControlError::Busy) => {
                    transition(admin.as_ref(), &t.id, Status::Running, Status::Ready, None)?;
                    continue;
                }
                Err(_) => {
                    transition(
                        admin.as_ref(),
                        &t.id,
                        Status::Running,
                        Status::Failed,
                        Some(failure_receipt(
                            &t,
                            "admission",
                            "not_started",
                            "会话无法准入，未执行工具",
                        )),
                    )?;
                    continue;
                }
            };
            let worker_admin = admin.clone();
            let worker_control = control.clone();
            let worker_stop = stop.clone();
            joins.spawn(async move {
                let _lease = lease;
                finish_wakeup(worker_admin, worker_control, t, key, worker_stop).await
            });
        }
        tokio::select! { _=stop.changed()=>{}, _=tokio::time::sleep(Duration::from_secs(1))=>{} }
    }
    // 停机先由独立等待者请求取消并核对回执；超时后留下 Running，由重启标为 Interrupted。
    let drain = async {
        while let Some(result) = joins.join_next().await {
            result.map_err(|_| "条件执行后台异常")??;
        }
        Ok::<(), AppError>(())
    };
    if let Ok(result) = tokio::time::timeout(Duration::from_secs(5), drain).await {
        result?;
    }
    joins.shutdown().await;
    Ok(())
}

pub(crate) struct ChannelAdapter(
    pub(crate) Arc<dyn TriggerAdmin>,
    pub(crate) Arc<std::sync::OnceLock<Arc<Observations>>>,
);
impl QqOutreach for ChannelAdapter {
    fn due(&self, _: &SessionKey) -> PluginResult<bool> {
        Ok(false)
    }
    fn judge(&self, _: QqOutreachMoment) -> QqOutreachFuture {
        Box::pin(async { Ok(false) })
    }
    fn attach(&self, _: &SessionKey, _: &str) -> PluginResult<Option<QqOutreachMessage>> {
        Ok(None)
    }
    fn next_push(&self) -> PluginResult<Option<QqOutreachPush>> {
        let observations = self
            .1
            .get()
            .ok_or_else(|| plugin_error("条件观察服务未就绪"))?;
        let mut selected = None;
        for t in self
            .0
            .snapshot()
            .map_err(plugin_error)?
            .into_iter()
            .filter(|t| t.delivery == Some(Delivery::Pending))
        {
            if !observations.authorized(&t.owner) {
                reject_pending_delivery(self.0.as_ref(), &t.id).map_err(plugin_error)?;
                continue;
            }
            selected = Some(t);
            break;
        }
        let Some(t) = selected else {
            return Ok(None);
        };
        self.0
            .delivery_for_run(&t.id, t.runs, Delivery::Pending, Delivery::Sending)
            .map_err(plugin_error)?;
        let text = eve_task_api::bounded(
            t.result.as_deref().unwrap_or_default(),
            eve_qqbot_plugin::QQ_OUTREACH_MAX_BYTES,
        );
        Ok(Some(QqOutreachPush {
            user_id: t.owner.user_id.clone(),
            session: Some(t.owner),
            message: QqOutreachMessage {
                id: format!("{}:run-{}", t.id, t.runs),
                text,
            },
        }))
    }
    fn delivered(&self, id: &str, result: QqOutreachResult) -> PluginResult<()> {
        let end = match result {
            QqOutreachResult::Sent { .. } => Delivery::Sent,
            QqOutreachResult::Unknown => Delivery::Unknown,
            _ => Delivery::Failed,
        };
        let (trigger_id, run) = id
            .rsplit_once(":run-")
            .map_or((id, None), |(id, run)| (id, run.parse::<u32>().ok()));
        // 周期回执与状态在账本内原子比较，上一周期不能覆盖新的 Sending。
        let result = match run {
            Some(run) => self
                .0
                .delivery_for_run(trigger_id, run, Delivery::Sending, end),
            None => self.0.delivery(trigger_id, Delivery::Sending, end),
        };
        match result {
            Ok(()) | Err(eve_trigger_api::TriggerError::Transition) => Ok(()),
            Err(e) => Err(plugin_error(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_kernel::{Kernel, KernelServices};
    use eve_llm_api::{ChatMessage, ChatRole};
    use eve_plugin_api::ServiceId;
    use eve_session_api::{SESSION_SERVICE_ID, SessionServiceHandle};

    async fn world() -> (Kernel, Arc<Observations>, tempfile::TempDir) {
        let services = KernelServices::default();
        let registry = services.registry.clone();
        let kernel = Kernel::with_services(services);
        kernel
            .register(Box::new(eve_session_plugin::SessionPlugin::new().unwrap()))
            .unwrap();
        kernel.start_all().await.unwrap();
        let sessions = registry
            .get(&ServiceId::new(SESSION_SERVICE_ID).unwrap())
            .unwrap()
            .unwrap()
            .value
            .downcast::<SessionServiceHandle>()
            .unwrap()
            .0
            .clone();
        let directory = tempfile::tempdir().unwrap();
        let observations = Observations {
            sessions,
            tasks: None,
            workspace: Some(directory.path().into()),
            operators: BTreeSet::from(["u".into()]),
            authorization: None,
            admission: None,
            processes: true,
        };
        (kernel, Arc::new(observations), directory)
    }
    fn trigger(conditions: Vec<Condition>) -> Trigger {
        Trigger {
            id: "test".into(),
            owner: SessionKey::new("s", "u").unwrap(),
            source: "test".into(),
            spec: TriggerSpec {
                title: "test".into(),
                instructions: "test".into(),
                mode: MatchMode::All,
                conditions,
                expires_at_ms: u64::MAX,
                recurrence: None,
                resume_task_id: None,
            },
            created_at_ms: 1,
            status: Status::Armed,
            evidence: vec![],
            result: None,
            delivery: None,
            runs: 0,
        }
    }
    fn complete(sessions: &dyn SessionService, owner: SessionKey, text: &str) {
        let lease = sessions
            .begin(SessionInput {
                key: owner,
                text: text.into(),
            })
            .unwrap()
            .lease;
        sessions
            .complete(
                &lease,
                vec![
                    ChatMessage::text(ChatRole::User, text),
                    ChatMessage::text(ChatRole::Assistant, "OK"),
                ],
            )
            .unwrap();
    }
    #[tokio::test]
    async fn file_changes_time_and_boolean_combinations_use_observed_state() {
        let (k, o, d) = world().await;
        let owner = SessionKey::new("s", "u").unwrap();
        let baseline = o.file(&owner, "output.txt").unwrap();
        assert_eq!(baseline, "missing");
        let mut t = trigger(vec![
            Condition::FileChanged {
                path: "output.txt".into(),
                baseline,
            },
            Condition::Time { at_ms: 1 },
        ]);
        assert!(o.evaluate(&t).unwrap().is_none());
        t.spec.mode = MatchMode::Any;
        assert!(o.evaluate(&t).unwrap().is_some());
        t.spec.mode = MatchMode::All;
        std::fs::write(d.path().join("output.txt"), "done").unwrap();
        assert_eq!(o.evaluate(&t).unwrap().unwrap().len(), 2);
        assert!(o.file(&owner, "../escape").is_err());
        assert!(
            o.file(&SessionKey::new("s", "other").unwrap(), "output.txt")
                .is_err()
        );
        #[cfg(unix)]
        {
            let outside = tempfile::NamedTempFile::new().unwrap();
            std::os::unix::fs::symlink(outside.path(), d.path().join("link")).unwrap();
            assert!(o.file(&owner, "link").is_err());
        }
        k.stop_all().await.unwrap();
    }
    #[tokio::test]
    async fn message_matching_excludes_old_other_scope_and_host_wake_inputs() {
        let (k, o, _) = world().await;
        let owner = SessionKey::new("s", "u").unwrap();
        complete(o.sessions.as_ref(), owner.clone(), "完成");
        let t = trigger(vec![Condition::MessageContains {
            text: "完成".into(),
            after_turn: 1,
        }]);
        assert!(o.evaluate(&t).unwrap().is_none());
        complete(
            o.sessions.as_ref(),
            SessionKey::new("other", "u").unwrap(),
            "完成",
        );
        complete(
            o.sessions.as_ref(),
            owner.clone(),
            &format!("{INPUT_PREFIX} 完成"),
        );
        assert!(o.evaluate(&t).unwrap().is_none());
        complete(o.sessions.as_ref(), owner, "构建完成");
        assert!(o.evaluate(&t).unwrap().is_some());
        k.stop_all().await.unwrap();
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn process_exit_is_observed_without_running_model_supplied_commands() {
        let (k, o, _) = world().await;
        let owner = SessionKey::new("s", "u").unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let identity = o.process(&owner, child.id()).unwrap();
        assert_ne!(identity, "missing");
        let t = trigger(vec![Condition::ProcessState {
            pid: child.id(),
            running: false,
            identity,
        }]);
        assert!(o.evaluate(&t).unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(o.evaluate(&t).unwrap().is_some());
        k.stop_all().await.unwrap();
    }
    #[tokio::test]
    async fn concurrent_cancel_loses_only_that_transition_and_other_triggers_continue() {
        let kernel = Kernel::with_services(KernelServices::default());
        let plugin = eve_trigger_plugin::TriggerPlugin::new().unwrap();
        let admin = plugin.controller();
        kernel.register(Box::new(plugin)).unwrap();
        kernel.start_all().await.unwrap();
        let mut spec = trigger(vec![Condition::Time { at_ms: 2000 }]).spec;
        spec.expires_at_ms = 100_000;
        let owner = SessionKey::new("s", "u").unwrap();
        let first = admin
            .create(owner.clone(), "cancel-race".into(), spec.clone(), 1000)
            .unwrap();
        let second = admin
            .create(owner.clone(), "still-scanning".into(), spec, 1000)
            .unwrap();
        admin.ready(&first.id, vec!["time".into()]).unwrap();
        admin.cancel(&owner, &first.id).unwrap();
        assert!(!transition(&admin, &first.id, Status::Ready, Status::Running, None).unwrap());
        admin.ready(&second.id, vec!["time".into()]).unwrap();
        assert!(transition(&admin, &second.id, Status::Ready, Status::Running, None).unwrap());
        admin.cancel(&owner, &second.id).unwrap();
        assert!(
            !transition(
                &admin,
                &second.id,
                Status::Running,
                Status::Completed,
                Some("迟到结果".into())
            )
            .unwrap()
        );
        assert!(
            admin
                .snapshot()
                .unwrap()
                .iter()
                .all(|t| t.status == Status::Cancelled)
        );
        kernel.stop_all().await.unwrap();
    }

    struct SimulatedControl {
        held_task: String,
        release: Arc<tokio::sync::Notify>,
    }
    impl ControlService for SimulatedControl {
        fn submit(
            &self,
            _: ControlInput,
            _: Arc<dyn eve_control_api::ControlEventSink>,
        ) -> eve_control_api::ControlResult<eve_control_api::GenerationKey> {
            if self.held_task == "busy-submit" {
                return Err(eve_control_api::ControlError::Busy);
            }
            unreachable!()
        }
        fn submit_if_current(
            &self,
            _: &eve_control_api::GenerationKey,
            _: ControlInput,
            _: Arc<dyn eve_control_api::ControlEventSink>,
        ) -> eve_control_api::ControlResult<eve_control_api::GenerationKey> {
            unreachable!()
        }
        fn cancel(
            &self,
            _: &eve_control_api::GenerationKey,
        ) -> eve_control_api::ControlResult<eve_control_api::CancelDisposition> {
            self.release.notify_one();
            Ok(eve_control_api::CancelDisposition::Requested)
        }
        fn wait(
            &self,
            key: &eve_control_api::GenerationKey,
        ) -> eve_control_api::ControlFuture<'static, eve_control_api::ControlReport> {
            let key = key.clone();
            let release = self.release.clone();
            let held = key.task_id == self.held_task;
            Box::pin(async move {
                if held {
                    release.notified().await;
                }
                Ok(eve_control_api::ControlReport {
                    key,
                    cancel_requested: false,
                    run: eve_control_api::RunReport {
                        turn_id: Some(1),
                        commit: eve_control_api::CommitState::Completed,
                        text: Some("隔离模拟已完成".into()),
                        transcript: None,
                        started_tools: Some(0),
                        tool_results: vec![],
                        failure: None,
                    },
                })
            })
        }
        fn snapshot(
            &self,
            _: &SessionKey,
        ) -> eve_control_api::ControlResult<Option<eve_control_api::ControlSnapshot>> {
            Ok(None)
        }
        fn accepts(&self, _: &eve_control_api::ControlEvent) -> bool {
            true
        }
    }
    #[tokio::test]
    async fn one_slow_control_wait_does_not_hold_another_trigger_result() {
        let kernel = Kernel::with_services(KernelServices::default());
        let plugin = eve_trigger_plugin::TriggerPlugin::new().unwrap();
        let admin: Arc<dyn TriggerAdmin> = Arc::new(plugin.controller());
        kernel.register(Box::new(plugin)).unwrap();
        kernel.start_all().await.unwrap();
        let mut spec = trigger(vec![Condition::Time { at_ms: 2000 }]).spec;
        spec.expires_at_ms = 100_000;
        let owner = SessionKey::new("s", "u").unwrap();
        let mut records = vec![];
        for source in ["held", "fast"] {
            let t = admin
                .create(owner.clone(), source.into(), spec.clone(), 1000)
                .unwrap();
            admin.ready(&t.id, vec!["time".into()]).unwrap();
            admin
                .transition(&t.id, Status::Ready, Status::Running, None)
                .unwrap();
            records.push(t);
        }
        let release = Arc::new(tokio::sync::Notify::new());
        let control: Arc<dyn ControlService> = Arc::new(SimulatedControl {
            held_task: records[0].id.clone(),
            release: release.clone(),
        });
        let (_stop_tx, stop) = watch::channel(false);
        let key = |t: &Trigger| eve_control_api::GenerationKey {
            session: t.owner.clone(),
            task_id: t.id.clone(),
            controller_epoch: [0; 16],
            generation: 1,
        };
        let held = tokio::spawn(finish_wakeup(
            admin.clone(),
            control.clone(),
            records[0].clone(),
            key(&records[0]),
            stop.clone(),
        ));
        tokio::time::timeout(
            Duration::from_millis(200),
            finish_wakeup(
                admin.clone(),
                control,
                records[1].clone(),
                key(&records[1]),
                stop,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let state = admin.snapshot().unwrap();
        assert_eq!(
            state.iter().find(|t| t.id == records[1].id).unwrap().status,
            Status::Completed
        );
        assert!(!held.is_finished());
        release.notify_one();
        held.await.unwrap().unwrap();
        kernel.stop_all().await.unwrap();
    }
    #[tokio::test]
    async fn control_busy_rearms_the_same_trigger_admission_without_stopping_other_windows() {
        let (kernel, observations, _directory) = world().await;
        let subject = eve_subject_plugin::SubjectPlugin::new().unwrap();
        let subject_admin = Arc::new(subject.controller());
        let trigger_plugin = eve_trigger_plugin::TriggerPlugin::new().unwrap();
        let admin = trigger_plugin.controller();
        kernel.register(Box::new(subject)).unwrap();
        kernel.register(Box::new(trigger_plugin)).unwrap();
        kernel
            .start(&eve_plugin_api::PluginId::new(eve_subject_api::SUBJECT_PLUGIN_ID).unwrap())
            .await
            .unwrap();
        kernel
            .start(&eve_plugin_api::PluginId::new(eve_trigger_api::PLUGIN_ID).unwrap())
            .await
            .unwrap();
        let observations = Arc::try_unwrap(observations)
            .ok()
            .unwrap()
            .with_admission(Arc::new(crate::qq_subject_admission::Admission(
                subject_admin,
            )));
        let now = now_ms();
        let mut spec = trigger(vec![Condition::Time { at_ms: now }]).spec;
        spec.expires_at_ms = now + 60000;
        let record = admin
            .create(
                SessionKey::new("s", "u").unwrap(),
                "busy-race".into(),
                spec,
                now - 1,
            )
            .unwrap();
        admin.ready(&record.id, vec!["time".into()]).unwrap();
        let record = admin.snapshot().unwrap().remove(0);
        let ticket = observations.begin_admission(&record).unwrap().unwrap();
        admin
            .transition(&record.id, Status::Ready, Status::Running, None)
            .unwrap();
        let control = SimulatedControl {
            held_task: "busy-submit".into(),
            release: Arc::new(tokio::sync::Notify::new()),
        };
        let result = submit_wakeup(
            &control,
            &observations,
            &ticket,
            ControlInput {
                session: SessionInput {
                    key: record.owner.clone(),
                    text: "wake".into(),
                },
                task_id: record.id.clone(),
            },
        )
        .unwrap();
        assert_eq!(result, Err(eve_control_api::ControlError::Busy));
        admin
            .transition(&record.id, Status::Running, Status::Ready, None)
            .unwrap();
        let record = admin.snapshot().unwrap().remove(0);
        assert_eq!(record.runs, 0);
        let unrelated = observations
            .admission
            .as_ref()
            .unwrap()
            .begin(&SessionKey::new("other", "u").unwrap(), "other-window")
            .unwrap()
            .unwrap();
        observations
            .finish_admission(&unrelated, QqAdmissionResult::Started)
            .unwrap();
        let retried = observations
            .begin_admission(&record)
            .expect("Busy不是终局，不应关闭条件后台")
            .unwrap();
        observations
            .finish_admission(&retried, QqAdmissionResult::Started)
            .unwrap();
        assert_eq!(admin.snapshot().unwrap()[0].runs, 0);
    }
}
