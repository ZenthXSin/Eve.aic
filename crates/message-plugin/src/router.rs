use eve_config_api::{
    CONFIG_PLUGIN_ID, CONFIG_SERVICE_ID, ConfigRequest, ConfigService, ConfigServiceHandle,
};
use eve_control_api::*;
use eve_message_api::*;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginDependency, PluginError, PluginFuture, PluginId,
    PluginManifest, PluginResult, ServiceId, cleanup,
};
use eve_session_api::SessionInput;
use serde_json::json;
use std::{
    collections::HashMap,
    future::{Future, poll_fn},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex},
    task::Poll,
    time::Duration,
};
use tokio::{
    sync::{Mutex as AsyncMutex, watch},
    task::JoinHandle,
};
type MessageKey = (String, String, String);
fn message_key(m: &IncomingMessage) -> MessageKey {
    (
        m.target.session.session_id.clone(),
        m.target.session.user_id.clone(),
        m.message_id.clone(),
    )
}
struct Entry {
    message: IncomingMessage,
    ordinal: u64,
    done: watch::Sender<Option<MessageResult<RouteReport>>>,
    task: Mutex<Option<JoinHandle<()>>>,
}
#[derive(Clone)]
struct Question {
    target: GenerationKey,
    id: String,
    source: String,
    prompt: String,
    ordinal: u64,
}
struct Book {
    active: bool,
    next: u64,
    entries: HashMap<MessageKey, Arc<Entry>>,
    questions: HashMap<String, Question>,
    gates: HashMap<String, Arc<AsyncMutex<()>>>,
}
struct Inner {
    book: Mutex<Book>,
    closed: watch::Sender<bool>,
    control: Arc<dyn ControlService>,
    judge: Arc<dyn RelationJudge>,
    config: Arc<dyn ConfigService>,
}
struct Router(Arc<Inner>);
impl MessageService for Router {
    fn submit(
        &self,
        message: IncomingMessage,
        sink: Arc<dyn ControlEventSink>,
    ) -> MessageResult<MessageTicket> {
        message.validate()?;
        let mut book = self.0.book.lock().map_err(|_| MessageError::Unavailable)?;
        if !book.active {
            return Err(MessageError::Unavailable);
        }
        if let Some(entry) = book.entries.get(&message_key(&message)) {
            if entry.message != message {
                return Err(MessageError::Conflict);
            }
            return Ok(MessageTicket { message });
        }
        let request = self
            .0
            .config
            .begin_request(MESSAGE_NAMESPACE, 1)
            .map_err(|_| MessageError::Configuration)?;
        let settings =
            MessageConfig::try_from(request.initial()).map_err(|_| MessageError::Configuration)?;
        if message.text.len() > settings.max_input_bytes
            || book.entries.len() >= settings.max_tracked_messages
        {
            return Err(MessageError::LimitReached);
        }
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| MessageError::Unavailable)?;
        let next = book.next.checked_add(1).ok_or(MessageError::LimitReached)?;
        let (done, _) = watch::channel(None);
        let entry = Arc::new(Entry {
            message: message.clone(),
            ordinal: book.next,
            done,
            task: Mutex::new(None),
        });
        let gate = book
            .gates
            .entry(message.target.session.session_id.clone())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone();
        let inner = self.0.clone();
        let working = entry.clone();
        let task = runtime.spawn(async move {
            let message = &working.message;
            let result =
                contain(async { inner.route(&working, request, settings, sink, gate).await })
                    .await
                    .unwrap_or(Err(MessageError::Unavailable));
            if let Ok(report) = &result
                && let RouteOutcome::Clarify {
                    question_id,
                    prompt,
                    ..
                } = &report.outcome
                && let Ok(mut book) = inner.book.lock()
                && book.active
                && book
                    .questions
                    .get(&message.target.session.session_id)
                    .is_none_or(|q| q.ordinal <= working.ordinal)
            {
                book.questions.insert(
                    message.target.session.session_id.clone(),
                    Question {
                        target: message.target.clone(),
                        id: question_id.clone(),
                        source: message.text.clone(),
                        prompt: prompt.clone(),
                        ordinal: working.ordinal,
                    },
                );
            }
            working.done.send_replace(Some(result));
        });
        *entry.task.lock().map_err(|_| MessageError::Unavailable)? = Some(task);
        book.next = next;
        book.entries.insert(message_key(&entry.message), entry);
        Ok(MessageTicket { message })
    }
    fn wait(&self, ticket: &MessageTicket) -> MessageFuture<'static, RouteReport> {
        let entry = self
            .0
            .book
            .lock()
            .map_err(|_| MessageError::Unavailable)
            .and_then(|book| {
                if !book.active {
                    return Err(MessageError::Unavailable);
                }
                let entry = book
                    .entries
                    .get(&message_key(&ticket.message))
                    .ok_or(MessageError::InvalidInput)?;
                if entry.message != ticket.message {
                    return Err(MessageError::Conflict);
                }
                Ok(entry.clone())
            });
        Box::pin(async move {
            let entry = entry?;
            let mut done = entry.done.subscribe();
            loop {
                let result = done.borrow_and_update().clone();
                if let Some(result) = result {
                    return result;
                }
                done.changed()
                    .await
                    .map_err(|_| MessageError::Unavailable)?;
            }
        })
    }
}
impl Inner {
    async fn closed(&self) {
        let mut signal = self.closed.subscribe();
        loop {
            if *signal.borrow_and_update() {
                return;
            }
            if signal.changed().await.is_err() {
                return;
            }
        }
    }
    fn current(&self, target: &GenerationKey) -> MessageResult<Option<ControlSnapshot>> {
        match self.control.snapshot(&target.session) {
            Ok(Some(s)) if s.key == *target => Ok(Some(s)),
            Ok(_) => Ok(None),
            Err(ControlError::OwnerMismatch | ControlError::InvalidInput) => {
                Err(MessageError::InvalidInput)
            }
            Err(_) => Err(MessageError::Unavailable),
        }
    }
    async fn route(
        &self,
        entry: &Entry,
        request: ConfigRequest,
        initial: MessageConfig,
        sink: Arc<dyn ControlEventSink>,
        gate: Arc<AsyncMutex<()>>,
    ) -> MessageResult<RouteReport> {
        let m = &entry.message;
        let report = |decision, outcome| {
            Ok(RouteReport {
                message: m.clone(),
                decision,
                outcome,
            })
        };
        let Some(snapshot) = self.current(&m.target)? else {
            return report(None, RouteOutcome::Stale { prior: None });
        };
        if snapshot.phase == ControlPhase::Blocked {
            return report(
                None,
                RouteOutcome::Blocked {
                    prior: snapshot.report.map(Box::new),
                },
            );
        }
        let input = RelationInput {
            message: m.clone(),
            phase: snapshot.phase,
            task_text: snapshot.input_text.clone(),
            cancel_requested: snapshot.cancel_requested,
            started_tools: snapshot.report.as_ref().and_then(|r| r.run.started_tools),
            clarification: self
                .book
                .lock()
                .map_err(|_| MessageError::Unavailable)?
                .questions
                .get(&m.target.session.session_id)
                .filter(|q| q.target == m.target)
                .map(|q| ClarificationContext {
                    question_id: q.id.clone(),
                    source_text: q.source.clone(),
                    prompt: q.prompt.clone(),
                }),
        };
        let judged = tokio::select! { biased;
            _=self.closed()=>return report(None,RouteOutcome::Stopped { prior:None }),
            _=tokio::time::sleep(Duration::from_millis(initial.judge_timeout_ms))=>Err(RelationError::Timeout),
            result=contain(async { self.judge.judge(input.clone()).await })=>result.unwrap_or(Err(RelationError::Panicked)),
        };
        let judged = judged.and_then(|d| d.validate(&input).map(|()| d));
        let _gate = tokio::select! { biased;
            _=self.closed()=>return report(judged.ok(),RouteOutcome::Stopped { prior:None }),
            guard=gate.lock()=>guard,
        };
        let Some(now) = self.current(&m.target)? else {
            return report(judged.ok(), RouteOutcome::Stale { prior: None });
        };
        if now.phase == ControlPhase::Blocked {
            return report(
                judged.ok(),
                RouteOutcome::Blocked {
                    prior: now.report.map(Box::new),
                },
            );
        }
        let decision = match judged {
            Ok(d) => d,
            Err(error) => return report(None, clarify(m, ClarifyReason::Judge(error), None)),
        };
        let settings = MessageConfig::try_from(
            &self
                .config
                .read_request(&request)
                .map_err(|_| MessageError::Configuration)?,
        )
        .map_err(|_| MessageError::Configuration)?;
        let action = map_action(&decision, settings.confidence_threshold);
        if let Action::Clarify(reason) = action {
            return report(Some(decision), clarify(m, reason, None));
        }
        if matches!(action, Action::Keep) {
            return report(Some(decision), RouteOutcome::Unchanged);
        }
        let answer = decision
            .parts
            .iter()
            .any(|p| p.intent == MessageIntent::Answer);
        let question = if answer {
            let book = self.book.lock().map_err(|_| MessageError::Unavailable)?;
            let question = book
                .questions
                .get(&m.target.session.session_id)
                .filter(|q| q.target == m.target && Some(&q.id) == m.reply_to.as_ref())
                .cloned();
            if question.is_none() {
                return report(
                    Some(decision),
                    clarify(m, ClarifyReason::MissingQuestion, None),
                );
            }
            question
        } else {
            None
        };
        let new_input = match action {
            Action::New => Some(ControlInput {
                session: SessionInput {
                    key: m.target.session.clone(),
                    text: decision
                        .parts
                        .iter()
                        .find(|p| p.intent == MessageIntent::NewTask)
                        .and_then(|p| p.span)
                        .map(|s| m.text[s.start..s.end].to_string())
                        .expect("validated new intent"),
                },
                task_id: m.message_id.clone(),
            }),
            Action::Revise => {
                let mut parts = decision
                    .parts
                    .iter()
                    .filter(|p| p.span.is_some())
                    .collect::<Vec<_>>();
                parts.sort_by_key(|p| p.span.expect("filtered").start);
                let changes = parts
                    .into_iter()
                    .map(|p| {
                        let s = p.span.expect("filtered");
                        json!({"kind":p.intent,"text":&m.text[s.start..s.end]})
                    })
                    .collect::<Vec<_>>();
                let text=json!({"request_kind":"revision","base_request":now.input_text,"changes":changes,
                    "clarification_source":question.map(|q|q.source),"policy":"correction_overrides_base; supplement_and_answer_add_constraints"}).to_string();
                Some(ControlInput {
                    session: SessionInput {
                        key: m.target.session.clone(),
                        text,
                    },
                    task_id: m.target.task_id.clone(),
                })
            }
            _ => None,
        };
        if new_input
            .as_ref()
            .is_some_and(|i| i.session.text.len() > settings.max_input_bytes)
        {
            return report(Some(decision), clarify(m, ClarifyReason::TooLarge, None));
        }
        // 捕获原代结果后才取消；外部切换不能让等待句柄误指向新代。
        let waiting = self.control.wait(&m.target);
        match self.control.cancel(&m.target) {
            Ok(_) => {}
            Err(ControlError::StaleGeneration) => {
                return report(Some(decision), RouteOutcome::Stale { prior: None });
            }
            Err(_) => return Err(MessageError::Unavailable),
        }
        let prior = waiting.await.map_err(|_| MessageError::Unavailable)?;
        if matches!(
            prior.run.commit,
            CommitState::Pending | CommitState::Unknown
        ) {
            return report(
                Some(decision),
                RouteOutcome::Blocked {
                    prior: Some(Box::new(prior)),
                },
            );
        }
        match self.control.snapshot(&m.target.session) {
            Ok(Some(snapshot)) if snapshot.key == m.target => {}
            Ok(_) => {
                return report(
                    Some(decision),
                    RouteOutcome::Stale {
                        prior: Some(Box::new(prior)),
                    },
                );
            }
            Err(error) => {
                return report(
                    Some(decision),
                    RouteOutcome::ControlFailed {
                        error,
                        prior: Box::new(prior),
                    },
                );
            }
        }
        let mut book = self.book.lock().map_err(|_| MessageError::Unavailable)?;
        if !book.active {
            return report(
                Some(decision),
                RouteOutcome::Stopped {
                    prior: Some(Box::new(prior)),
                },
            );
        }
        if matches!(action, Action::Cancel) {
            return report(
                Some(decision),
                RouteOutcome::Cancelled {
                    prior: Box::new(prior),
                },
            );
        }
        if matches!(action, Action::Revise) && prior.run.started_tools != Some(0) {
            return report(
                Some(decision),
                clarify(m, ClarifyReason::SideEffects, Some(Box::new(prior))),
            );
        }
        let outcome = match self.control.submit_if_current(
            &m.target,
            new_input.expect("replacement action"),
            sink,
        ) {
            Ok(generation) => {
                book.questions.remove(&m.target.session.session_id);
                RouteOutcome::Replaced {
                    generation,
                    prior: Box::new(prior),
                }
            }
            Err(ControlError::StaleGeneration) => RouteOutcome::Stale {
                prior: Some(Box::new(prior)),
            },
            Err(ControlError::Blocked) => RouteOutcome::Blocked {
                prior: Some(Box::new(prior)),
            },
            Err(error) => RouteOutcome::ControlFailed {
                error,
                prior: Box::new(prior),
            },
        };
        report(Some(decision), outcome)
    }
    async fn close(&self) -> PluginResult<()> {
        let entries = {
            let mut book = self
                .book
                .lock()
                .map_err(|_| PluginError::State("消息记录锁不可用".into()))?;
            book.active = false;
            self.closed.send_replace(true);
            book.entries.values().cloned().collect::<Vec<_>>()
        };
        for entry in entries {
            let task = entry
                .task
                .lock()
                .map_err(|_| PluginError::Task("消息任务锁不可用".into()))?
                .take();
            if let Some(task) = task {
                task.await
                    .map_err(|_| PluginError::Task("消息路由收尾异常".into()))?;
            }
        }
        Ok(())
    }
}
#[derive(Clone, Copy)]
enum Action {
    Keep,
    Cancel,
    New,
    Revise,
    Clarify(ClarifyReason),
}
fn map_action(d: &RelationDecision, threshold: u8) -> Action {
    use MessageIntent::*;
    if d.parts.iter().any(|p| p.confidence < threshold) {
        return Action::Clarify(ClarifyReason::LowConfidence);
    }
    let has = |intent| d.parts.iter().any(|p| p.intent == intent);
    if has(Pause) || has(Resume) {
        return Action::Clarify(ClarifyReason::Unsupported);
    }
    if has(Ambiguous) {
        return Action::Clarify(ClarifyReason::Ambiguous);
    }
    if has(NewTask) {
        return if d.parts.iter().filter(|p| p.intent == NewTask).count() == 1
            && d.parts.iter().all(|p| matches!(p.intent, NewTask | Cancel))
        {
            Action::New
        } else {
            Action::Clarify(ClarifyReason::Conflict)
        };
    }
    let revise = has(Supplement) || has(Correction) || has(Answer);
    if revise {
        return if has(Unrelated) {
            Action::Clarify(ClarifyReason::Conflict)
        } else {
            Action::Revise
        };
    }
    if has(Cancel) {
        return if d.parts.len() == 1 {
            Action::Cancel
        } else {
            Action::Clarify(ClarifyReason::Conflict)
        };
    }
    Action::Keep
}
fn clarify(
    m: &IncomingMessage,
    reason: ClarifyReason,
    prior: Option<Box<ControlReport>>,
) -> RouteOutcome {
    let prompt = match reason {
        ClarifyReason::SideEffects => "已有工具执行或副作用未知；请确认接下来要执行的新任务。",
        ClarifyReason::MissingQuestion => "请引用当前任务最近一次澄清问题，或明确说明新要求。",
        ClarifyReason::Unsupported => "尚未支持任务暂停恢复；请明确继续、取消或开始新任务。",
        ClarifyReason::TooLarge => "组合后的请求过长；请提供精简的新要求。",
        _ => "请明确这条消息是补充、纠正、澄清答复还是新任务。",
    };
    RouteOutcome::Clarify {
        reason,
        question_id: m.message_id.clone(),
        prompt: prompt.into(),
        prior,
    }
}
async fn contain<F: Future>(future: F) -> Result<F::Output, ()> {
    let mut future = Box::pin(future);
    poll_fn(
        |cx| match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(cx))) {
            Ok(Poll::Ready(v)) => Poll::Ready(Ok(v)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(_) => Poll::Ready(Err(())),
        },
    )
    .await
}

pub struct MessageRouterPlugin {
    manifest: PluginManifest,
    judge_service: ServiceId,
}
impl MessageRouterPlugin {
    pub fn new(judge_dependency: PluginDependency, judge_service: ServiceId) -> PluginResult<Self> {
        let mut manifest = PluginManifest::new(ROUTER_PLUGIN_ID, env!("CARGO_PKG_VERSION"))?;
        manifest.dependencies = [
            PluginId::new(CONTROL_PLUGIN_ID)?,
            PluginId::new(CONFIG_PLUGIN_ID)?,
        ]
        .into_iter()
        .map(|id| PluginDependency {
            id,
            requirement: Some("^0.1".into()),
        })
        .collect();
        manifest.dependencies.push(judge_dependency);
        Ok(Self {
            manifest,
            judge_service,
        })
    }
    pub fn builtin() -> PluginResult<Self> {
        Self::new(
            PluginDependency {
                id: PluginId::new(RELATION_PLUGIN_ID)?,
                requirement: Some("^0.1".into()),
            },
            ServiceId::new(RELATION_SERVICE_ID)?,
        )
    }
}
impl Plugin for MessageRouterPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        let judge_id = self.judge_service.clone();
        Box::pin(async move {
            let control = ctx
                .service::<ControlServiceHandle>(&ServiceId::new(CONTROL_SERVICE_ID)?)?
                .ok_or_else(|| PluginError::State("缺少控制服务".into()))?
                .0
                .clone();
            let judge = ctx
                .service::<RelationServiceHandle>(&judge_id)?
                .ok_or_else(|| PluginError::State("缺少消息判断服务".into()))?
                .0
                .clone();
            let config = ctx
                .service::<ConfigServiceHandle>(&ServiceId::new(CONFIG_SERVICE_ID)?)?
                .ok_or_else(|| PluginError::State("缺少消息配置服务".into()))?
                .0
                .clone();
            MessageConfig::try_from(
                &config
                    .snapshot(MESSAGE_NAMESPACE, 1)
                    .map_err(|_| PluginError::State("消息配置不可用".into()))?,
            )
            .map_err(|_| PluginError::State("消息配置无效".into()))?;
            let (closed, _) = watch::channel(false);
            let inner = Arc::new(Inner {
                control,
                judge,
                config,
                closed,
                book: Mutex::new(Book {
                    active: true,
                    next: 1,
                    entries: HashMap::new(),
                    questions: HashMap::new(),
                    gates: HashMap::new(),
                }),
            });
            let closing = inner.clone();
            ctx.cleanup(cleanup(move || async move { closing.close().await }))?;
            ctx.provide_service(
                ServiceId::new(ROUTER_SERVICE_ID)?,
                MessageServiceHandle(Arc::new(Router(inner))),
            )?;
            Ok(None)
        })
    }
}
