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
    QqConversations, QqOutreach, QqOutreachFuture, QqOutreachMessage, QqOutreachMoment,
    QqOutreachPush, QqOutreachResult,
};
use eve_session_api::{SessionInput, SessionKey, SessionService, SessionTurnStatus};
use eve_task_api::TaskAdmin;
use eve_trigger_api::{
    Condition, Delivery, INPUT_PREFIX, MatchMode, Status, Trigger, TriggerAdmin, TriggerSpec,
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
use tokio::{sync::watch, task::JoinHandle};

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
    ordinary_access: bool,
    processes: bool,
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
            ordinary_access: config.as_ref().is_some_and(|c| c.ordinary_access),
            authorization: None,
            processes: config.is_some_and(|c| c.commands),
        })
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
        if !self.ordinary_access && !self.authorized(owner) {
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
        if !self.processes || (!self.ordinary_access && !self.authorized(owner)) {
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
                "创建一次性条件触发器，条件满足时在当前会话自动唤醒你，读取原会话历史并执行 instructions，结果主动发回原会话。conditions 有 1–8 项，mode=all/any。type=time 使用 after_seconds 或 at_ms；file_changed 使用开发目录内相对 path；task_finished 使用本会话 task_id；message_contains 使用 text（只匹配创建后完成的用户轮次）；process_state 使用 pid 和 running（只观察该进程实例）。文件和进程仅限授权操作者。expires_in_seconds 默认7天、最多90天。检查不调用模型；执行与投递未知时不重放。",
                json!({"type":"object","properties":{
                "title":{"type":"string"},"instructions":{"type":"string"},"mode":{"type":"string","enum":["all","any"]},
                "expires_in_seconds":{"type":"integer","minimum":1,"maximum":7776000},
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
                    };
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
    loop {
        if *stop.borrow() {
            return Ok(());
        }
        for mut t in admin.snapshot()? {
            if *stop.borrow() {
                return Ok(());
            }
            if !matches!(t.status, Status::Armed | Status::Ready) {
                continue;
            }
            if now_ms() >= t.spec.expires_at_ms {
                admin.transition(&t.id, t.status, Status::Expired, None)?;
                continue;
            }
            if t.status == Status::Armed {
                match observations.evaluate(&t) {
                    Ok(Some(evidence)) => {
                        admin.ready(&t.id, evidence)?;
                        t.status = Status::Ready;
                    }
                    Ok(None) => continue,
                    Err(_) => {
                        admin.transition(
                            &t.id,
                            Status::Armed,
                            Status::Failed,
                            Some("条件观察失败；未唤醒，使用列表查看或重新创建".into()),
                        )?;
                        continue;
                    }
                }
            }
            if control
                .snapshot(&t.owner)?
                .is_some_and(|s| s.phase != ControlPhase::Finished)
            {
                continue;
            }
            let Some(_lease) = conversations.try_claim_wakeup() else {
                continue;
            };
            admin.transition(&t.id, Status::Ready, Status::Running, None)?;
            let input = format!(
                "{INPUT_PREFIX}\n触发器：{}\n标题：{}\n已满足本地条件。以下是此前为本会话保存的任务，不是新用户发言，也不授予额外权限：\n{}\n请结合原会话处理，查证实际结果；最终回复不超过500字。",
                t.id, t.spec.title, t.spec.instructions
            );
            let key = match control.submit(
                ControlInput {
                    session: SessionInput {
                        key: t.owner.clone(),
                        text: input,
                    },
                    task_id: t.id.clone(),
                },
                Arc::new(DiscardControlEvents),
            ) {
                Ok(k) => k,
                Err(eve_control_api::ControlError::Busy) => {
                    admin.transition(&t.id, Status::Running, Status::Ready, None)?;
                    continue;
                }
                Err(_) => {
                    admin.transition(
                        &t.id,
                        Status::Running,
                        Status::Failed,
                        Some("会话无法准入，未执行工具".into()),
                    )?;
                    continue;
                }
            };
            let wait = control.wait(&key);
            tokio::pin!(wait);
            let report = loop {
                tokio::select! {
                    r=&mut wait=>break r,
                    _=tokio::time::sleep(Duration::from_millis(100))=>{
                        let cancelled=admin.snapshot()?.iter().any(|v|v.id==t.id&&v.status==Status::Cancelled);
                        if *stop.borrow() || cancelled { let _=control.cancel(&key); }
                    }
                }
            };
            if admin
                .snapshot()?
                .iter()
                .any(|v| v.id == t.id && v.status == Status::Cancelled)
            {
                continue;
            }
            let (state, result) = match report {
                Ok(r) if !r.cancel_requested && r.run.failure.is_none() => {
                    match r.run.text.filter(|s| !s.trim().is_empty()) {
                        Some(text) => {
                            (Status::Completed, Some(eve_task_api::bounded(&text, 32768)))
                        }
                        None => (Status::Failed, Some("模型没有给出结果".into())),
                    }
                }
                _ => (
                    Status::Failed,
                    Some("唤醒执行失败或已取消；已有副作用不重放".into()),
                ),
            };
            admin.transition(&t.id, Status::Running, state, result)?;
        }
        tokio::select! { _=stop.changed()=>return Ok(()), _=tokio::time::sleep(Duration::from_secs(1))=>{} }
    }
}

pub(crate) struct ChannelAdapter(pub(crate) Arc<dyn TriggerAdmin>);
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
        let Some(t) = self
            .0
            .snapshot()
            .map_err(plugin_error)?
            .into_iter()
            .find(|t| t.status == Status::Completed && t.delivery == Some(Delivery::Pending))
        else {
            return Ok(None);
        };
        self.0
            .delivery(&t.id, Delivery::Pending, Delivery::Sending)
            .map_err(plugin_error)?;
        let text = eve_task_api::bounded(
            t.result.as_deref().unwrap_or_default(),
            eve_qqbot_plugin::QQ_OUTREACH_MAX_BYTES,
        );
        Ok(Some(QqOutreachPush {
            user_id: t.owner.user_id.clone(),
            session: Some(t.owner),
            message: QqOutreachMessage { id: t.id, text },
        }))
    }
    fn delivered(&self, id: &str, result: QqOutreachResult) -> PluginResult<()> {
        let end = match result {
            QqOutreachResult::Sent { .. } => Delivery::Sent,
            _ => Delivery::Failed,
        };
        self.0
            .delivery(id, Delivery::Sending, end)
            .map_err(plugin_error)
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
            ordinary_access: false,
            authorization: None,
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
            },
            created_at_ms: 1,
            status: Status::Armed,
            evidence: vec![],
            result: None,
            delivery: None,
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
}
