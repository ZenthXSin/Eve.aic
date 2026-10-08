use crate::strict_json;
use eve_plugin_api::{PluginContext, PluginError, PluginResult};
use eve_practice_api::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Mutex, MutexGuard},
};

const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    format_version: u32,
    runs: Vec<PracticeRun>,
}
struct Inner {
    ledger: Ledger,
    context: Option<PluginContext>,
}
pub(super) struct StoredPractice {
    inner: Mutex<Inner>,
}

impl StoredPractice {
    pub(super) fn open(context: PluginContext) -> PracticeResult<Self> {
        let mut ledger = match context
            .state_get(PRACTICE_STATE_KEY)
            .map_err(|_| PracticeError::Storage)?
        {
            None => Ledger {
                format_version: FORMAT_VERSION,
                runs: vec![],
            },
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(PracticeError::CorruptState);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| PracticeError::CorruptState)?;
                if value.get("format_version").and_then(|value| value.as_u64())
                    != Some(u64::from(FORMAT_VERSION))
                {
                    return Err(if value.get("format_version").is_some() {
                        PracticeError::UnsupportedVersion
                    } else {
                        PracticeError::CorruptState
                    });
                }
                let ledger: Ledger =
                    serde_json::from_value(value).map_err(|_| PracticeError::CorruptState)?;
                validate_ledger(&ledger).map_err(|_| PracticeError::CorruptState)?;
                ledger
            }
        };
        let mut interrupted = false;
        for run in &mut ledger.runs {
            if run.status == PracticeStatus::Running {
                run.status = PracticeStatus::Interrupted;
                interrupted = true;
            }
        }
        // 先保存中断结局再公开实例；不重放草稿请求或实际运行，也不伪造完成时间。
        if interrupted {
            let bytes = encode(&ledger)?;
            context
                .state_set(PRACTICE_STATE_KEY, bytes)
                .map_err(|_| PracticeError::Storage)?;
        }
        Ok(Self {
            inner: Mutex::new(Inner {
                ledger,
                context: Some(context),
            }),
        })
    }

    fn lock(&self) -> PracticeResult<MutexGuard<'_, Inner>> {
        let inner = self.inner.lock().map_err(|_| PracticeError::Unavailable)?;
        if inner.context.is_none() {
            return Err(PracticeError::Unavailable);
        }
        Ok(inner)
    }

    pub(super) fn snapshot(&self) -> PracticeResult<PracticeSnapshot> {
        Ok(PracticeSnapshot {
            runs: self.lock()?.ledger.runs.clone(),
        })
    }

    pub(super) fn begin(
        &self,
        task: PracticeTask,
        runner: &RunnerProfile,
        drafter_version: &str,
        now_ms: u64,
    ) -> PracticeResult<Option<PracticeRun>> {
        task.validate()?;
        runner.validate()?;
        validate_id(drafter_version)?;
        if now_ms == 0 {
            return Err(PracticeError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let id = run_id(&task.goal_id, task.goal_revision);
        let runs = &inner.ledger.runs;
        // 同一修订只实践一次；每个目标的总次数持久计数，重启不会重置。
        if runs
            .iter()
            .any(|run| run.id == id || run.status == PracticeStatus::Running)
            || runs
                .iter()
                .filter(|run| run.task.goal_id == task.goal_id)
                .count()
                >= MAX_RUNS_PER_GOAL
        {
            return Ok(None);
        }
        if runs.len() >= MAX_RUNS {
            return Err(PracticeError::LimitReached);
        }
        let run = PracticeRun {
            id,
            task,
            runner: runner.clone(),
            drafter_version: drafter_version.into(),
            started_at_ms: now_ms,
            finished_at_ms: None,
            status: PracticeStatus::Running,
            attempts: vec![attempt(1, now_ms)],
        };
        let mut next = inner.ledger.clone();
        next.runs.push(run.clone());
        persist(&mut inner, next)?;
        Ok(Some(run))
    }

    pub(super) fn record_draft(
        &self,
        run_id: &str,
        at_ms: u64,
        result: Result<PracticeDraft, PracticeFailure>,
        issues: Vec<String>,
    ) -> PracticeResult<PracticeRun> {
        validate_issues(&issues)?;
        self.update(run_id, at_ms, AttemptStage::Drafting, |run| {
            let index = run.attempts.len() - 1;
            match result {
                Err(failure) => {
                    if !issues.is_empty() {
                        return Err(PracticeError::InvalidInput);
                    }
                    let current = &mut run.attempts[index];
                    current.outcome = Some(AttemptOutcome::DraftFailed { failure });
                    current.finished_at_ms = Some(at_ms);
                    finish(run, at_ms, PracticeStatus::Failed(failure));
                }
                Ok(draft) => {
                    validate_draft(&run.task, &run.runner, &draft)?;
                    let applicable = draft.applicable;
                    if !applicable && !issues.is_empty() {
                        return Err(PracticeError::InvalidInput);
                    }
                    let current = &mut run.attempts[index];
                    current.draft = Some(draft);
                    if !applicable {
                        current.outcome = Some(AttemptOutcome::NotApplicable);
                        current.finished_at_ms = Some(at_ms);
                        finish(run, at_ms, PracticeStatus::NotApplicable);
                    } else if !issues.is_empty() {
                        current.issues = issues;
                        current.outcome = Some(AttemptOutcome::Rejected);
                        current.finished_at_ms = Some(at_ms);
                        next_or_finish(run, at_ms);
                    } else {
                        // 通过结构检查的草稿先落盘，返回后才可实际运行。
                        current.stage = AttemptStage::Running;
                    }
                }
            }
            Ok(())
        })
    }

    pub(super) fn record_evidence(
        &self,
        run_id: &str,
        at_ms: u64,
        evidence: RunEvidence,
    ) -> PracticeResult<PracticeRun> {
        self.update(run_id, at_ms, AttemptStage::Running, |run| {
            let index = run.attempts.len() - 1;
            let current = &mut run.attempts[index];
            let draft = current.draft.as_ref().ok_or(PracticeError::CorruptState)?;
            evidence.validate(draft)?;
            let verified = evidence.verified(draft);
            current.evidence = Some(evidence);
            current.finished_at_ms = Some(at_ms);
            if verified {
                current.outcome = Some(AttemptOutcome::Verified);
                finish(run, at_ms, PracticeStatus::Verified);
            } else {
                current.outcome = Some(AttemptOutcome::Failed);
                next_or_finish(run, at_ms);
            }
            Ok(())
        })
    }

    pub(super) fn abandon(
        &self,
        run_id: &str,
        at_ms: u64,
        failure: PracticeFailure,
    ) -> PracticeResult<PracticeRun> {
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let position = position(&next, run_id)?;
        let mut run = next.runs[position].clone();
        let Some(current) = run.attempts.last_mut() else {
            return Err(PracticeError::CorruptState);
        };
        if run.status != PracticeStatus::Running || current.outcome.is_some() {
            return Err(PracticeError::Conflict);
        }
        if at_ms < current.started_at_ms {
            return Err(PracticeError::InvalidInput);
        }
        // 运行中的进程已由宿主终止，结果未知；只记录放弃原因，不补写证据。
        current.outcome = Some(AttemptOutcome::Abandoned { failure });
        current.finished_at_ms = Some(at_ms);
        finish(&mut run, at_ms, PracticeStatus::Failed(failure));
        next.runs[position] = run.clone();
        persist(&mut inner, next)?;
        Ok(run)
    }

    fn update(
        &self,
        run_id: &str,
        at_ms: u64,
        stage: AttemptStage,
        apply: impl FnOnce(&mut PracticeRun) -> PracticeResult<()>,
    ) -> PracticeResult<PracticeRun> {
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let position = position(&next, run_id)?;
        let mut run = next.runs[position].clone();
        let current = run.attempts.last().ok_or(PracticeError::CorruptState)?;
        if run.status != PracticeStatus::Running
            || current.outcome.is_some()
            || current.stage != stage
        {
            return Err(PracticeError::Conflict);
        }
        if at_ms < current.started_at_ms {
            return Err(PracticeError::InvalidInput);
        }
        apply(&mut run)?;
        next.runs[position] = run.clone();
        persist(&mut inner, next)?;
        Ok(run)
    }

    pub(super) fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("实践状态锁不可用".into()))?
            .context = None;
        Ok(())
    }
}

fn attempt(number: usize, at_ms: u64) -> PracticeAttempt {
    PracticeAttempt {
        number,
        stage: AttemptStage::Drafting,
        started_at_ms: at_ms,
        finished_at_ms: None,
        draft: None,
        issues: vec![],
        evidence: None,
        outcome: None,
    }
}

/// 仍有次数时在同一次提交中开启下一次尝试（先落盘再请求修正）；否则记为未验证。
fn next_or_finish(run: &mut PracticeRun, at_ms: u64) {
    if run.attempts.len() < MAX_ATTEMPTS {
        let number = run.attempts.len() + 1;
        run.attempts.push(attempt(number, at_ms));
    } else {
        finish(run, at_ms, PracticeStatus::Unverified);
    }
}

fn finish(run: &mut PracticeRun, at_ms: u64, status: PracticeStatus) {
    run.status = status;
    run.finished_at_ms = Some(at_ms);
}

fn position(ledger: &Ledger, run_id: &str) -> PracticeResult<usize> {
    ledger
        .runs
        .iter()
        .position(|run| run.id == run_id)
        .ok_or(PracticeError::NotFound)
}

fn persist(inner: &mut Inner, next: Ledger) -> PracticeResult<()> {
    let bytes = encode(&next)?;
    let context = inner.context.as_ref().ok_or(PracticeError::Unavailable)?;
    // 失败也可能已经提交；关闭整个实例，禁止旧缓存继续读取或覆盖后端。
    if context.state_set(PRACTICE_STATE_KEY, bytes).is_err() {
        inner.context = None;
        return Err(PracticeError::Storage);
    }
    inner.ledger = next;
    Ok(())
}

fn encode(ledger: &Ledger) -> PracticeResult<Vec<u8>> {
    if ledger.runs.len() > MAX_RUNS {
        return Err(PracticeError::LimitReached);
    }
    let bytes = serde_json::to_vec(ledger).map_err(|_| PracticeError::InvalidInput)?;
    // Interrupted 比 Running 多四个字节；写入时预留，保证下次启动能保存中断结局。
    let recovery_bytes = ledger
        .runs
        .iter()
        .filter(|run| run.status == PracticeStatus::Running)
        .count()
        * ("Interrupted".len() - "Running".len());
    if bytes.len().saturating_add(recovery_bytes) > MAX_STATE_BYTES {
        return Err(PracticeError::LimitReached);
    }
    Ok(bytes)
}

/// 启动时完整核对；任何不一致都拒绝打开并保留原字节。
fn validate_ledger(ledger: &Ledger) -> PracticeResult<()> {
    let invalid = || PracticeError::InvalidInput;
    if ledger.format_version != FORMAT_VERSION || ledger.runs.len() > MAX_RUNS {
        return Err(invalid());
    }
    let mut ids = BTreeMap::new();
    let mut per_goal: BTreeMap<&str, usize> = BTreeMap::new();
    let mut running = 0;
    for run in &ledger.runs {
        validate_run(run)?;
        if ids.insert(run.id.as_str(), ()).is_some() {
            return Err(invalid());
        }
        let count = per_goal.entry(run.task.goal_id.as_str()).or_default();
        *count += 1;
        if *count > MAX_RUNS_PER_GOAL {
            return Err(invalid());
        }
        if run.status == PracticeStatus::Running {
            running += 1;
        }
    }
    if running > 1 {
        return Err(invalid());
    }
    Ok(())
}

fn validate_run(run: &PracticeRun) -> PracticeResult<()> {
    let invalid = || PracticeError::InvalidInput;
    run.task.validate()?;
    run.runner.validate()?;
    validate_id(&run.drafter_version)?;
    if run.id != run_id(&run.task.goal_id, run.task.goal_revision)
        || run.started_at_ms == 0
        || run.attempts.is_empty()
        || run.attempts.len() > MAX_ATTEMPTS
    {
        return Err(invalid());
    }
    let open = matches!(
        run.status,
        PracticeStatus::Running | PracticeStatus::Interrupted
    );
    match run.finished_at_ms {
        None if open => {}
        Some(at) if !open && at >= run.started_at_ms => {}
        _ => return Err(invalid()),
    }
    let mut previous = run.started_at_ms;
    let last = run.attempts.len() - 1;
    for (index, attempt) in run.attempts.iter().enumerate() {
        if attempt.number != index + 1 || attempt.started_at_ms < previous {
            return Err(invalid());
        }
        validate_issues(&attempt.issues)?;
        if let Some(draft) = &attempt.draft {
            validate_draft(&run.task, &run.runner, draft)?;
        }
        if let Some(evidence) = &attempt.evidence {
            let draft = attempt.draft.as_ref().ok_or_else(invalid)?;
            evidence.validate(draft)?;
        }
        match (&attempt.outcome, attempt.finished_at_ms) {
            (None, None) => {}
            (Some(_), Some(at)) if at >= attempt.started_at_ms => previous = at,
            _ => return Err(invalid()),
        }
        let consistent = match &attempt.outcome {
            None => {
                index == last
                    && open
                    && attempt.evidence.is_none()
                    && attempt.issues.is_empty()
                    && (attempt.stage == AttemptStage::Drafting) == attempt.draft.is_none()
            }
            Some(AttemptOutcome::NotApplicable) => {
                index == last
                    && run.status == PracticeStatus::NotApplicable
                    && attempt.stage == AttemptStage::Drafting
                    && attempt
                        .draft
                        .as_ref()
                        .is_some_and(|draft| !draft.applicable)
                    && attempt.issues.is_empty()
                    && attempt.evidence.is_none()
            }
            Some(AttemptOutcome::Rejected) => {
                attempt.stage == AttemptStage::Drafting
                    && attempt.draft.as_ref().is_some_and(|draft| draft.applicable)
                    && !attempt.issues.is_empty()
                    && attempt.evidence.is_none()
            }
            Some(AttemptOutcome::Verified) => {
                index == last
                    && run.status == PracticeStatus::Verified
                    && attempt.stage == AttemptStage::Running
                    && attempt.issues.is_empty()
                    && attempt.draft.as_ref().is_some_and(|draft| {
                        attempt
                            .evidence
                            .as_ref()
                            .is_some_and(|evidence| evidence.verified(draft))
                    })
            }
            Some(AttemptOutcome::Failed) => {
                attempt.stage == AttemptStage::Running
                    && attempt.issues.is_empty()
                    && attempt.draft.as_ref().is_some_and(|draft| {
                        attempt
                            .evidence
                            .as_ref()
                            .is_some_and(|evidence| !evidence.verified(draft))
                    })
            }
            Some(AttemptOutcome::DraftFailed { failure }) => {
                index == last
                    && run.status == PracticeStatus::Failed(*failure)
                    && attempt.stage == AttemptStage::Drafting
                    && attempt.draft.is_none()
                    && attempt.issues.is_empty()
            }
            Some(AttemptOutcome::Abandoned { failure }) => {
                index == last
                    && run.status == PracticeStatus::Failed(*failure)
                    && attempt.evidence.is_none()
                    && attempt.issues.is_empty()
                    && (attempt.stage == AttemptStage::Drafting) == attempt.draft.is_none()
            }
        };
        // 被拒绝或运行失败的尝试之后，只能是下一次尝试或尝试用尽后的未验证结局。
        let retryable = matches!(
            attempt.outcome,
            Some(AttemptOutcome::Rejected | AttemptOutcome::Failed)
        );
        let placed = if index < last {
            retryable
        } else if retryable {
            run.status == PracticeStatus::Unverified && run.attempts.len() == MAX_ATTEMPTS
        } else {
            true
        };
        if !consistent || !placed {
            return Err(invalid());
        }
    }
    Ok(())
}
