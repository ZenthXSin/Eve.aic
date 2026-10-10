//! QQ 宿主的受控研究：为等待中的兴趣学习目标在操作者配置的来源范围内研究，
//! 每个目标修订至多一次；准入与每个阶段先持久化，停止、超时与失败都留下记录且不重试。
use crate::{AppError, qq_plan};
use eve_cognition_api::{CognitionAdmin, Goal, GoalStatus, SourceKind, Visibility};
use eve_interest_api::{
    INTEREST_GOAL_CHANNEL, INTEREST_GOAL_VERIFICATION, InterestAdmin, InterestStatus,
};
use eve_interest_plugin::learning_goal_id;
use eve_knowledge_api::{
    KnowledgeAdmin, KnowledgeEntry, KnowledgeError, KnowledgeKind, KnowledgeSnapshot,
    KnowledgeStatus, MAX_BRIEF_BYTES, ResearchFailure, ResearchOutcome, ResearchRun, ResearchTopic,
    RunStatus, SourcePolicy,
};
use eve_knowledge_plugin::Researcher;
use eve_memory_api::validate_id;
use eve_plan_api::PlanJournal;
use eve_plugin_api::{PluginError, PluginResult};
use eve_qqbot_plugin::{QqCommandHandler, QqCommandInput};
use std::{
    cmp::Reverse,
    fmt::Write,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::watch, task::JoinHandle};

const SUBJECT: &str = "eve";
/// 一次研究（两次抓取阶段与两次模型请求）的总时长上限。
const RESEARCH_TIMEOUT: Duration = Duration::from_secs(180);
const HELP: &str =
    "用法：/knowledge 兴趣ID 查看 Eve 为这条兴趣研究到的资料与来源；兴趣 ID 可用 /interests 查看。";
const DISABLED: &str = "受控研究未启用。";
const SHOWN_ENTRIES: usize = 8;
const QUOTE_PREVIEW_BYTES: usize = 160;

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
        knowledge: Arc<dyn KnowledgeAdmin>,
        cognition: Arc<dyn CognitionAdmin>,
        researcher: Arc<Researcher>,
        policy: SourcePolicy,
        plans: Option<Arc<dyn PlanJournal>>,
    ) -> Self {
        let (active, activated) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let (finished_sender, finished) = watch::channel(false);
        let task = tokio::spawn(async move {
            // Sender 在 panic 时也释放；宿主随后关闭通道，不假装后台仍正常。
            let result = run(
                knowledge, cognition, researcher, policy, plans, activated, stopped,
            )
            .await;
            let _ = finished_sender.send(true);
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
            .map_err(|_| "受控研究后台异常；已停止准入")?
    }
}

async fn stop_requested(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

/// 只研究兴趣派生、仍在等待中的学习目标；用户待办、反思子目标和已取消目标都不研究。
fn eligible(goal: &Goal) -> Option<&str> {
    let Visibility::User(owner) = &goal.visibility else {
        return None;
    };
    (goal.source.kind == SourceKind::Inference
        && goal.source.channel == INTEREST_GOAL_CHANNEL
        && goal.verification == INTEREST_GOAL_VERIFICATION
        && goal.status == GoalStatus::Waiting)
        .then_some(owner.as_str())
}

fn topic(goal: &Goal, owner: &str) -> ResearchTopic {
    let brief = prefix(&goal.description, MAX_BRIEF_BYTES);
    ResearchTopic {
        goal_id: goal.id.clone(),
        goal_revision: goal.revision,
        owner: owner.into(),
        brief: brief.into(),
        brief_truncated: brief.len() != goal.description.len(),
    }
}

async fn run(
    knowledge: Arc<dyn KnowledgeAdmin>,
    cognition: Arc<dyn CognitionAdmin>,
    researcher: Arc<Researcher>,
    policy: SourcePolicy,
    plans: Option<Arc<dyn PlanJournal>>,
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
    let version = researcher.version();
    let mut researching = true;
    loop {
        if *stopped.borrow() {
            return Ok(());
        }
        if researching {
            let snapshot = cognition
                .snapshot()
                .map_err(|_| "受控研究无法读取学习目标")?;
            // 兴趣派生的学习目标，以及计划步骤正在请求研究的用户待办。
            let requested = match &plans {
                Some(plans) => qq_plan::requested(&plans.snapshot()?, qq_plan::RESEARCH),
                None => Vec::new(),
            };
            let mut goals: Vec<(&Goal, &str)> = snapshot
                .state
                .goals
                .values()
                .filter_map(|goal| {
                    eligible(goal)
                        .or_else(|| qq_plan::requested_goal(goal, &requested))
                        .map(|owner| (goal, owner))
                })
                .collect();
            goals.sort_by_key(|(goal, _)| (Reverse(goal.priority), goal.id.clone()));
            for (goal, owner) in goals {
                if *stopped.borrow() {
                    return Ok(());
                }
                let run = match knowledge.begin(topic(goal, owner), &policy, &version, now_ms()?) {
                    Ok(Some(run)) => run,
                    Ok(None) => continue,
                    // 容量满保留原记录：停止新研究，已有知识仍可查看。
                    Err(KnowledgeError::LimitReached) => {
                        researching = false;
                        break;
                    }
                    Err(error) => return Err(error.into()),
                };
                execute(&*knowledge, &researcher, &run, &mut stopped).await?;
                // 一次只研究一个目标；下一轮重新读取认知状态。
                break;
            }
        }
        tokio::select! {
            biased;
            _ = stop_requested(&mut stopped) => return Ok(()),
            _ = tokio::time::sleep(Duration::from_millis(250)) => {},
        }
    }
}

async fn execute(
    knowledge: &dyn KnowledgeAdmin,
    researcher: &Researcher,
    run: &ResearchRun,
    stopped: &mut watch::Receiver<bool>,
) -> Result<(), AppError> {
    let began = run.started_at_ms;
    let outcome = {
        let research = researcher.research(run, || now_ms().unwrap_or(began));
        tokio::pin!(research);
        tokio::select! {
            biased;
            _ = stop_requested(stopped) => ResearchOutcome::Failed(ResearchFailure::Cancelled),
            result = tokio::time::timeout(RESEARCH_TIMEOUT, &mut research) => match result {
                Ok(Ok(outcome)) => outcome,
                // 存储失败时句柄已关闭；研究留在 Running，重启后记为中断。
                Ok(Err(error)) => return Err(error.into()),
                Err(_) => ResearchOutcome::Failed(ResearchFailure::Timeout),
            },
        }
    };
    // 远端请求可能已经发生；取消、超时或失败都记录结局，不重试同一修订。
    knowledge.finish(&run.id, now_ms()?.max(began), outcome)?;
    Ok(())
}

/// /knowledge 兴趣ID：只读当前会话自己的兴趣对应的研究与知识。
pub(crate) struct Commands {
    enabled: Option<Enabled>,
}
struct Enabled {
    interests: Arc<dyn InterestAdmin>,
    knowledge: Arc<dyn KnowledgeAdmin>,
}
impl Commands {
    pub(crate) fn disabled() -> Arc<dyn QqCommandHandler> {
        Arc::new(Self { enabled: None })
    }
    pub(crate) fn enabled(
        interests: Arc<dyn InterestAdmin>,
        knowledge: Arc<dyn KnowledgeAdmin>,
    ) -> Arc<dyn QqCommandHandler> {
        Arc::new(Self {
            enabled: Some(Enabled {
                interests,
                knowledge,
            }),
        })
    }
}

enum Command<'a> {
    Show(&'a str),
    Help,
}

fn parse(text: &str) -> Option<Command<'_>> {
    let text = text.trim();
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    let (name, tail) = text.split_at(end);
    let tail = tail.trim();
    (name == "/knowledge").then(|| {
        if validate_id(tail).is_ok() && !tail.chars().any(char::is_whitespace) {
            Command::Show(tail)
        } else {
            Command::Help
        }
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
        let Command::Show(interest_id) = command else {
            return Ok(Some(HELP.into()));
        };
        let scope = crate::qq_memory::scope(input.session);
        let snapshot = enabled
            .interests
            .snapshot(&scope)
            .map_err(|_| failure_text())?;
        let Some(interest) = snapshot
            .interests
            .iter()
            .find(|interest| interest.id == interest_id)
        else {
            return Ok(Some(
                "当前会话没有这条兴趣。发送 /interests 查看兴趣 ID。".into(),
            ));
        };
        let knowledge = enabled.knowledge.snapshot().map_err(|_| failure_text())?;
        let goal_id = learning_goal_id(SUBJECT, &interest.id);
        let entries: Vec<&KnowledgeEntry> = knowledge.entries_for(&goal_id).collect();
        if entries.is_empty() {
            let latest = knowledge
                .runs_for(&goal_id)
                .max_by_key(|run| run.started_at_ms);
            return Ok(Some(match latest.map(|run| run.status) {
                None if interest.status == InterestStatus::Withdrawn => {
                    "这条兴趣已撤回，不会再为它研究。".into()
                }
                None => "还没有为这条兴趣开展研究。".into(),
                Some(RunStatus::Running) => "正在研究这条兴趣，稍后再查看。".into(),
                Some(RunStatus::Completed) => "最近一次研究没有找到可引用的相关资料。".into(),
                Some(RunStatus::Failed(failure)) => format!(
                    "最近一次研究未完成（{}），不会自动重试；这条兴趣有新的原话时会重新研究。",
                    failure_label(failure)
                ),
                Some(RunStatus::Interrupted) => {
                    "最近一次研究因进程退出而中断，不会自动重放；这条兴趣有新的原话时会重新研究。"
                        .into()
                }
            }));
        }
        Ok(Some(render(&interest.topic, &entries, &knowledge)))
    }
}

fn render(topic: &str, entries: &[&KnowledgeEntry], knowledge: &KnowledgeSnapshot) -> String {
    let mut reply = format!(
        "「{topic}」的学习资料（共 {} 条；“来源原文”只表示网页原文这样写，尚未经过实践验证）：",
        entries.len()
    );
    for entry in entries.iter().take(SHOWN_ENTRIES) {
        match (entry.status, &entry.source) {
            (KnowledgeStatus::SourceQuoted, Some(source)) => {
                let document = knowledge.document(&source.document_id);
                let _ = write!(
                    reply,
                    "\n- [来源原文·{}] {}｜版本：{}｜{}｜抓取于 {}｜原文：“{}”",
                    kind_label(entry.kind),
                    entry.statement,
                    entry.version.as_deref().unwrap_or("未注明"),
                    document.map_or("来源缺失", |document| document.url.as_str()),
                    document
                        .map_or_else(|| "未知时间".into(), |document| utc(document.fetched_at_ms)),
                    preview(&source.quote),
                );
            }
            _ => {
                let _ = write!(reply, "\n- [未验证推测] {}", entry.statement);
            }
        }
    }
    if entries.len() > SHOWN_ENTRIES {
        let _ = write!(
            reply,
            "\n（另有 {} 条未显示）",
            entries.len() - SHOWN_ENTRIES
        );
    }
    reply
}

fn kind_label(kind: KnowledgeKind) -> &'static str {
    match kind {
        KnowledgeKind::Fact => "事实",
        KnowledgeKind::Procedure => "做法",
        KnowledgeKind::Version => "版本",
        KnowledgeKind::Hypothesis => "推测",
    }
}

fn failure_label(failure: ResearchFailure) -> &'static str {
    match failure {
        ResearchFailure::Fetch => "资料来源无法访问",
        ResearchFailure::Provider => "模型请求失败",
        ResearchFailure::InvalidOutput => "模型输出未通过来源核对",
        ResearchFailure::Timeout => "研究超时",
        ResearchFailure::Cancelled => "服务停止时取消",
        ResearchFailure::LimitReached => "资料容量已满",
    }
}

fn preview(text: &str) -> String {
    let part = prefix(text, QUOTE_PREVIEW_BYTES);
    if part.len() == text.len() {
        text.into()
    } else {
        format!("{part}…")
    }
}

fn prefix(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// 毫秒时间戳的 UTC 日期时间，精确到分钟。
fn utc(ms: u64) -> String {
    let seconds = ms / 1000;
    let days = (seconds / 86_400) as i64;
    let (hour, minute) = ((seconds % 86_400) / 3600, (seconds % 3600) / 60);
    // 公历换算（Howard Hinnant 的 civil_from_days）。
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
}

fn failure_text() -> PluginError {
    PluginError::State("兴趣或研究资料状态无法确认；服务已停止，请重新打开后查看持久状态。".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_dates_and_command_parsing() {
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(951_782_400_000), "2000-02-29 00:00 UTC");
        assert_eq!(utc(1_791_374_340_000), "2026-10-07 11:59 UTC");
        assert!(matches!(
            parse("/knowledge interest-1"),
            Some(Command::Show("interest-1"))
        ));
        assert!(matches!(parse(" /knowledge "), Some(Command::Help)));
        assert!(matches!(parse("/knowledge a b"), Some(Command::Help)));
        assert!(parse("/knowledgeable x").is_none());
        assert!(parse("knowledge x").is_none());
    }
}
