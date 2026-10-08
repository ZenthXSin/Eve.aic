//! 技能调用：后续任务的第一次尝试前，先保存一次选择，再请求选择器；选定且参数合规时，
//! 宿主实例化模板交给实践流程作为第一次草稿，不再请求草稿器。其余情况照常草稿。
//! 调用结果不另行运行，而是事后依据实践账本中那次尝试的证据写入。
use eve_practice_api::{
    AttemptOutcome, DraftRequest, PracticeDraft, PracticeDrafter, PracticeError, PracticeFuture,
    PracticeSnapshot, PracticeStatus, validate_artifact,
};
use eve_skill_api::*;
use std::sync::Arc;

pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// 包装任一草稿器；实践账本记录的草稿器版本为 `skill-aware:v1+内层版本`。
pub struct SkillAwareDrafter {
    admin: Arc<dyn SkillAdmin>,
    selector: Arc<dyn SkillSelector>,
    inner: Arc<dyn PracticeDrafter>,
    clock: Clock,
    version: String,
}

impl SkillAwareDrafter {
    pub fn new(
        admin: Arc<dyn SkillAdmin>,
        selector: Arc<dyn SkillSelector>,
        inner: Arc<dyn PracticeDrafter>,
        clock: Clock,
    ) -> Self {
        let version = format!("skill-aware:v1+{}", inner.version());
        Self {
            admin,
            selector,
            inner,
            clock,
            version,
        }
    }

    async fn skill_draft(&self, request: &DraftRequest) -> SkillResult<Option<PracticeDraft>> {
        let task = &request.task;
        let snapshot = self.admin.snapshot()?;
        let mut candidates = snapshot.enabled_for(&task.owner, &request.runner.runner_id);
        candidates.sort();
        candidates.truncate(MAX_CANDIDATES);
        if candidates.is_empty() {
            return Ok(None);
        }
        // 选择记录已满时保留原记录，任务照常草稿，不中断实践。
        let selection = match self.admin.begin_selection(
            &request.run_id,
            &task.owner,
            &task.goal_id,
            candidates.clone(),
            self.selector.version(),
            (self.clock)(),
        ) {
            Ok(Some(selection)) => selection,
            Ok(None) | Err(SkillError::LimitReached) => return Ok(None),
            Err(error) => return Err(error),
        };
        let summaries = candidates
            .iter()
            .filter_map(|candidate| snapshot.summary(candidate))
            .collect();
        let result = self
            .selector
            .select(SelectRequest {
                selection_id: selection.id.clone(),
                selector_version: self.selector.version().into(),
                task: task.clone(),
                runner: request.runner.clone(),
                candidates: summaries,
            })
            .await;
        let at = (self.clock)().max(selection.started_at_ms);
        let choice = match result {
            Ok(Some(choice)) => choice,
            Ok(None) => {
                self.admin
                    .record_selection(&selection.id, at, Ok(None), vec![])?;
                return Ok(None);
            }
            Err(SkillError::Skill(failure)) => {
                self.admin
                    .record_selection(&selection.id, at, Err(failure), vec![])?;
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let (issues, draft) = match snapshot
            .source(&choice.skill)
            .and_then(Distillation::template)
            .filter(|_| candidates.contains(&choice.skill))
        {
            None => (vec!["选定的技能版本不是候选".to_string()], None),
            Some(proposal) => match instantiate(&proposal.template, &choice.arguments) {
                Err(issues) => (issues, None),
                Ok(mut draft) => {
                    draft.rationale = invocation_rationale(&proposal.template.title, &choice.skill);
                    if validate_artifact(&request.runner, &draft).is_ok() {
                        (vec![], Some(draft))
                    } else {
                        (vec!["实例不符合产物的通用限制".to_string()], None)
                    }
                }
            },
        };
        let issues = crate::consolidator::sanitize(issues);
        self.admin
            .record_selection(&selection.id, at, Ok(Some(choice)), issues)?;
        Ok(draft)
    }
}

impl PracticeDrafter for SkillAwareDrafter {
    fn version(&self) -> &str {
        &self.version
    }

    fn draft(&self, request: DraftRequest) -> PracticeFuture<'_, PracticeDraft> {
        Box::pin(async move {
            if request.drafter_version != self.version {
                return Err(PracticeError::InvalidInput);
            }
            // 只有第一次尝试可以使用技能；修正仍由草稿器依据实际证据完成。
            if request.attempt == 1 && request.previous.is_none() {
                match self.skill_draft(&request).await {
                    Ok(Some(draft)) => return Ok(draft),
                    Ok(None) => {}
                    Err(error) => return Err(practice_error(error)),
                }
            }
            let mut request = request;
            request.drafter_version = self.inner.version().into();
            self.inner.draft(request).await
        })
    }
}

fn practice_error(error: SkillError) -> PracticeError {
    match error {
        SkillError::Storage => PracticeError::Storage,
        SkillError::LimitReached => PracticeError::LimitReached,
        SkillError::CorruptState => PracticeError::CorruptState,
        _ => PracticeError::Unavailable,
    }
}

/// 依据实践账本写入尚未结算的技能调用结果；进行中的实践跳过，下次再核对。
/// 启动时与每次实践结束后调用，不重放任何请求或运行。
pub fn settle(
    admin: &dyn SkillAdmin,
    practice: &PracticeSnapshot,
    now_ms: u64,
) -> SkillResult<usize> {
    let snapshot = admin.snapshot()?;
    let mut settled = 0;
    for selection in snapshot.selections.iter().filter(|selection| {
        selection.status == SelectionStatus::Chosen && selection.outcome.is_none()
    }) {
        let Some(run) = practice.runs.iter().find(|run| run.id == selection.id) else {
            continue;
        };
        let outcome = match run
            .attempts
            .first()
            .and_then(|attempt| attempt.outcome.as_ref())
        {
            Some(AttemptOutcome::Verified) => InvocationOutcome::Verified,
            Some(AttemptOutcome::Failed) => InvocationOutcome::Failed,
            Some(AttemptOutcome::Rejected) => InvocationOutcome::Rejected,
            Some(AttemptOutcome::Abandoned { .. }) => InvocationOutcome::Abandoned,
            // 实例一定适用且不经草稿器；出现其他结局说明没有得到运行结果。
            Some(AttemptOutcome::NotApplicable | AttemptOutcome::DraftFailed { .. }) => {
                InvocationOutcome::Interrupted
            }
            None if run.status == PracticeStatus::Running => continue,
            None => InvocationOutcome::Interrupted,
        };
        let at = selection
            .finished_at_ms
            .map_or(now_ms, |finished| now_ms.max(finished));
        admin.settle(&selection.id, at, outcome)?;
        settled += 1;
    }
    Ok(settled)
}
