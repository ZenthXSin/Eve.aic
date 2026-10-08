//! QQ 宿主的实践验证：为等待中的兴趣学习目标做最小产物并在操作者提供的运行环境中实际运行，
//! 每个目标修订至多一次；准入与每一步先持久化，停止、超时与失败都留下记录且不重放。
use crate::AppError;
use eve_cognition_api::{CognitionAdmin, Goal, GoalStatus, SourceKind, Visibility};
use eve_interest_api::{INTEREST_GOAL_CHANNEL, INTEREST_GOAL_VERIFICATION, InterestAdmin};
use eve_interest_plugin::learning_goal_id;
use eve_knowledge_api::{KnowledgeAdmin, KnowledgeStatus, RunStatus as ResearchStatus};
use eve_memory_api::validate_id;
use eve_plugin_api::{PluginError, PluginResult};
use eve_practice_api::{
    AttemptOutcome, MAX_BRIEF_BYTES, MAX_NOTE_BYTES, MAX_NOTES, PracticeAdmin, PracticeAttempt,
    PracticeError, PracticeFailure, PracticeRun, PracticeStatus, PracticeTask, RunExit, TaskNote,
};
use eve_practice_plugin::Practitioner;
use eve_qqbot_plugin::{QqCommandHandler, QqCommandInput};
use std::{
    cmp::Reverse,
    fmt::Write,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::watch, task::JoinHandle};

const SUBJECT: &str = "eve";
/// 一次实践（至多三次草稿请求与三次实际运行）的总时长上限。
const PRACTICE_TIMEOUT: Duration = Duration::from_secs(900);
const HELP: &str = "用法：/practice 兴趣ID 查看 Eve 为这条兴趣做过的实践与真实运行证据；兴趣 ID 可用 /interests 查看。";
const DISABLED: &str = "实践验证未启用。";
const SHOWN_RUNS: usize = 2;

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
        practice: Arc<dyn PracticeAdmin>,
        cognition: Arc<dyn CognitionAdmin>,
        knowledge: Option<Arc<dyn KnowledgeAdmin>>,
        practitioner: Arc<Practitioner>,
        workspace_root: PathBuf,
    ) -> Self {
        let (active, activated) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let (finished_sender, finished) = watch::channel(false);
        let task = tokio::spawn(async move {
            // Sender 在 panic 时也释放；宿主随后关闭通道，不假装后台仍正常。
            let result = run(
                practice,
                cognition,
                knowledge,
                practitioner,
                workspace_root,
                activated,
                stopped,
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
            .map_err(|_| "实践验证后台异常；已停止准入")?
    }
}

async fn stop_requested(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

/// 只实践兴趣派生、仍在等待中的学习目标；用户待办、反思子目标和已取消目标都不实践。
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

/// 开启研究时等该修订的研究结束（或该目标研究次数用尽），再把有来源的知识交给草稿器；
/// 来源原文在前，未验证推测在后。
fn task(
    goal: &Goal,
    owner: &str,
    knowledge: Option<&Arc<dyn KnowledgeAdmin>>,
) -> Result<Option<PracticeTask>, AppError> {
    let mut notes = Vec::new();
    if let Some(knowledge) = knowledge {
        let snapshot = knowledge.snapshot()?;
        let runs: Vec<_> = snapshot.runs_for(&goal.id).collect();
        let settled = runs.iter().any(|run| {
            run.topic.goal_revision == goal.revision && run.status != ResearchStatus::Running
        }) || (runs.len() >= eve_knowledge_api::MAX_RUNS_PER_GOAL
            && runs.iter().all(|run| run.status != ResearchStatus::Running));
        if !settled {
            return Ok(None);
        }
        let mut entries: Vec<_> = snapshot.entries_for(&goal.id).collect();
        entries.sort_by_key(|entry| entry.status != KnowledgeStatus::SourceQuoted);
        for entry in entries.into_iter().take(MAX_NOTES) {
            let mut text = entry.statement.clone();
            if let Some(source) = &entry.source {
                let _ = write!(text, "（原文：“{}”）", source.quote);
            }
            notes.push(TaskNote {
                id: entry.id.clone(),
                text: prefix(&text, MAX_NOTE_BYTES).into(),
                source: entry
                    .source
                    .as_ref()
                    .and_then(|source| snapshot.document(&source.document_id))
                    .map(|document| prefix(&document.url, MAX_NOTE_BYTES).to_string()),
                version: entry.version.clone(),
                source_quoted: entry.status == KnowledgeStatus::SourceQuoted,
            });
        }
    }
    let brief = prefix(&goal.description, MAX_BRIEF_BYTES);
    Ok(Some(PracticeTask {
        goal_id: goal.id.clone(),
        goal_revision: goal.revision,
        owner: owner.into(),
        brief: brief.into(),
        brief_truncated: brief.len() != goal.description.len(),
        notes,
    }))
}

async fn run(
    practice: Arc<dyn PracticeAdmin>,
    cognition: Arc<dyn CognitionAdmin>,
    knowledge: Option<Arc<dyn KnowledgeAdmin>>,
    practitioner: Arc<Practitioner>,
    workspace_root: PathBuf,
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
    // 工作目录只存放进行中尝试的临时文件；上次进程中断留下的目录不再有用，证据已在账本中。
    if workspace_root.exists() {
        std::fs::remove_dir_all(&workspace_root).map_err(|_| "无法清理实践工作目录")?;
    }
    std::fs::create_dir_all(&workspace_root).map_err(|_| "无法创建实践工作目录")?;
    let mut practicing = true;
    loop {
        if *stopped.borrow() {
            return Ok(());
        }
        if practicing {
            let snapshot = cognition
                .snapshot()
                .map_err(|_| "实践验证无法读取学习目标")?;
            let mut goals: Vec<(&Goal, &str)> = snapshot
                .state
                .goals
                .values()
                .filter_map(|goal| eligible(goal).map(|owner| (goal, owner)))
                .collect();
            goals.sort_by_key(|(goal, _)| (Reverse(goal.priority), goal.id.clone()));
            for (goal, owner) in goals {
                if *stopped.borrow() {
                    return Ok(());
                }
                let Some(task) = task(goal, owner, knowledge.as_ref())? else {
                    continue;
                };
                let run = match practice.begin(
                    task,
                    practitioner.runner(),
                    practitioner.drafter_version(),
                    now_ms()?,
                ) {
                    Ok(Some(run)) => run,
                    Ok(None) => continue,
                    // 容量满保留原记录：停止新实践，已有记录仍可查看。
                    Err(PracticeError::LimitReached) => {
                        practicing = false;
                        break;
                    }
                    Err(error) => return Err(error.into()),
                };
                execute(
                    &*practice,
                    &practitioner,
                    &run,
                    &workspace_root,
                    &mut stopped,
                )
                .await?;
                // 一次只实践一个目标；下一轮重新读取认知状态。
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
    practice: &dyn PracticeAdmin,
    practitioner: &Practitioner,
    run: &PracticeRun,
    workspace_root: &std::path::Path,
    stopped: &mut watch::Receiver<bool>,
) -> Result<(), AppError> {
    let began = run.started_at_ms;
    let abandoned = {
        let work = practitioner.practice(run, workspace_root, || now_ms().unwrap_or(began));
        tokio::pin!(work);
        tokio::select! {
            biased;
            _ = stop_requested(stopped) => Some(PracticeFailure::Cancelled),
            result = tokio::time::timeout(PRACTICE_TIMEOUT, &mut work) => match result {
                Ok(Ok(_)) => None,
                // 存储失败时句柄已关闭；实践留在 Running，重启后记为中断。
                Ok(Err(error)) => return Err(error.into()),
                Err(_) => Some(PracticeFailure::Timeout),
            },
        }
    };
    // 放弃时运行中的进程已随 future 终止；只记录原因，结果未知，不重放。
    if let Some(failure) = abandoned {
        let _ = std::fs::remove_dir_all(Practitioner::workspace(workspace_root, &run.id));
        practice.abandon(&run.id, now_ms()?.max(began), failure)?;
    }
    Ok(())
}

/// /practice 兴趣ID：只读当前会话自己的兴趣对应的实践记录。
pub(crate) struct Commands {
    enabled: Option<Enabled>,
}
struct Enabled {
    interests: Arc<dyn InterestAdmin>,
    practice: Arc<dyn PracticeAdmin>,
}
impl Commands {
    pub(crate) fn disabled() -> Arc<dyn QqCommandHandler> {
        Arc::new(Self { enabled: None })
    }
    pub(crate) fn enabled(
        interests: Arc<dyn InterestAdmin>,
        practice: Arc<dyn PracticeAdmin>,
    ) -> Arc<dyn QqCommandHandler> {
        Arc::new(Self {
            enabled: Some(Enabled {
                interests,
                practice,
            }),
        })
    }
}

fn parse(text: &str) -> Option<Option<&str>> {
    let text = text.trim();
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    let (name, tail) = text.split_at(end);
    let tail = tail.trim();
    (name == "/practice").then(|| {
        (validate_id(tail).is_ok() && !tail.chars().any(char::is_whitespace)).then_some(tail)
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
        let Some(interest_id) = command else {
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
        let practice = enabled.practice.snapshot().map_err(|_| failure_text())?;
        let goal_id = learning_goal_id(SUBJECT, &interest.id);
        let mut runs: Vec<&PracticeRun> = practice.runs_for(&goal_id).collect();
        if runs.is_empty() {
            return Ok(Some("还没有为这条兴趣做过实践。".into()));
        }
        runs.sort_by_key(|run| Reverse(run.started_at_ms));
        let mut reply = format!(
            "「{}」的实践记录（共 {} 次；只有在运行环境中实际加载且全部探测通过才算已验证）：",
            interest.topic,
            runs.len()
        );
        for run in runs.iter().take(SHOWN_RUNS) {
            render(&mut reply, run);
        }
        Ok(Some(reply))
    }
}

fn render(reply: &mut String, run: &PracticeRun) {
    let _ = write!(
        reply,
        "\n- 目标修订 {}：{}｜运行环境：{}",
        run.task.goal_revision,
        status_label(run.status),
        run.runner.runtime
    );
    for attempt in &run.attempts {
        render_attempt(reply, attempt);
    }
}

fn render_attempt(reply: &mut String, attempt: &PracticeAttempt) {
    let _ = write!(reply, "\n  尝试 {}：", attempt.number);
    match &attempt.outcome {
        None => reply.push_str("进行中或因进程退出中断（结果未知，不重放）"),
        Some(AttemptOutcome::NotApplicable) => {
            reply.push_str("运行环境与这条兴趣无关，没有实践");
        }
        Some(AttemptOutcome::Rejected) => {
            let _ = write!(
                reply,
                "结构检查未通过，没有运行：{}",
                attempt.issues.join("；")
            );
        }
        Some(AttemptOutcome::DraftFailed { failure })
        | Some(AttemptOutcome::Abandoned { failure }) => {
            reply.push_str(failure_label(*failure));
        }
        Some(AttemptOutcome::Verified) | Some(AttemptOutcome::Failed) => {
            let verified = attempt.outcome == Some(AttemptOutcome::Verified);
            reply.push_str(if verified { "已验证" } else { "未通过" });
            if let Some(evidence) = &attempt.evidence {
                if !evidence.runtime_version.is_empty() {
                    let _ = write!(reply, "｜{}", evidence.runtime_version);
                }
                let _ = write!(
                    reply,
                    "｜{}｜{}",
                    exit_label(evidence.exit),
                    if evidence.loaded {
                        "已加载"
                    } else {
                        "未加载"
                    }
                );
                for warning in evidence.warnings.iter().take(3) {
                    let _ = write!(reply, "\n    警告：{warning}");
                }
                for result in &evidence.probes {
                    let _ = write!(
                        reply,
                        "\n    探测 {}.{}：期望 {}，实际 {}{}",
                        result.probe.subject,
                        result.probe.property,
                        result.probe.expected,
                        result.actual.as_deref().unwrap_or("无"),
                        if result.passed { " ✓" } else { " ✗" }
                    );
                }
            }
        }
    }
    if let Some(draft) = &attempt.draft
        && draft.applicable
    {
        let files: Vec<&str> = draft.files.iter().map(|file| file.path.as_str()).collect();
        let _ = write!(reply, "\n    产物：{}", files.join("、"));
        if !draft.notes_used.is_empty() {
            let _ = write!(reply, "｜依据资料 {} 条", draft.notes_used.len());
        }
    }
}

fn status_label(status: PracticeStatus) -> &'static str {
    match status {
        PracticeStatus::Running => "进行中",
        PracticeStatus::Verified => "已验证",
        PracticeStatus::NotApplicable => "不适用",
        PracticeStatus::Unverified => "尝试用尽仍未验证",
        PracticeStatus::Failed(_) => "未完成",
        PracticeStatus::Interrupted => "因进程退出而中断，不重放",
    }
}

fn failure_label(failure: PracticeFailure) -> &'static str {
    match failure {
        PracticeFailure::Provider => "草稿请求失败",
        PracticeFailure::InvalidOutput => "草稿未通过格式核对",
        PracticeFailure::Timeout => "超时",
        PracticeFailure::Cancelled => "服务停止时取消，结果未知",
        PracticeFailure::LimitReached => "实践容量已满",
    }
}

fn exit_label(exit: RunExit) -> &'static str {
    match exit {
        RunExit::Completed => "运行完成",
        RunExit::Timeout => "运行超时",
        RunExit::StartFailed => "无法启动",
        RunExit::Crashed => "运行中途退出",
    }
}

fn prefix(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn failure_text() -> PluginError {
    PluginError::State("兴趣或实践状态无法确认；服务已停止，请重新打开后查看持久状态。".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_parsing() {
        assert_eq!(parse("/practice interest-1"), Some(Some("interest-1")));
        assert_eq!(parse(" /practice "), Some(None));
        assert_eq!(parse("/practice a b"), Some(None));
        assert_eq!(parse("/practices x"), None);
    }
}
