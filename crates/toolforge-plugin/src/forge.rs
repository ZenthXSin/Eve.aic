//! 锻造流程：从实践账本推导反复出现的问题，取真实草稿作样例；已启用的工具回放即覆盖时只记录复用，
//! 否则先保存准入再请求锻造器，并用同一批草稿回放验证后保存结果。
use eve_practice_api::{
    AttemptOutcome, DraftCheck, GapKind, PracticeDraft, PracticeError, PracticeResult, PracticeRun,
    PracticeSnapshot, RunnerProfile, capability_gaps, gap_key,
};
use eve_toolforge_api::*;
use std::{collections::BTreeSet, sync::Arc};

/// 一个待锻造的缺口，以及回放验证所用的真实草稿。
#[derive(Clone, Debug)]
pub struct Candidate {
    pub owner: String,
    pub gap: GapRef,
    pub occurrences: usize,
    pub failing: Vec<ExampleFiles>,
    pub passing: Vec<ExampleFiles>,
    /// 已启用、且回放即覆盖这个缺口的工具版本；有则只记录复用。
    pub reuse: Option<(ToolRef, Verification)>,
    /// 同一缺口工具的最新规格，供锻造器改进。
    pub current: Option<CheckSpec>,
}

pub struct Forge {
    admin: Arc<dyn ToolAdmin>,
    forger: Arc<dyn ToolForger>,
    runner: RunnerProfile,
}

impl Forge {
    pub fn new(
        admin: Arc<dyn ToolAdmin>,
        forger: Arc<dyn ToolForger>,
        runner: RunnerProfile,
    ) -> Self {
        Self {
            admin,
            forger,
            runner,
        }
    }

    pub fn forger_version(&self) -> &str {
        self.forger.version()
    }

    /// 最早一个可以处理的缺口：该运行器下反复出现、不是运行前检查自身给出的问题，
    /// 两侧都有真实草稿，用户没有停用对应工具，且同一出现次数还没有锻造或复用过。
    pub fn candidate(
        &self,
        tools: &ToolSnapshot,
        practice: &PracticeSnapshot,
    ) -> Option<Candidate> {
        let runner = self.runner.runner_id.as_str();
        let owners: BTreeSet<&str> = practice
            .runs
            .iter()
            .filter(|run| run.runner.runner_id == runner)
            .map(|run| run.task.owner.as_str())
            .collect();
        for owner in owners {
            for gap in capability_gaps(practice, owner) {
                if gap.kind != GapKind::RepeatedIssue || gap.runner_id != runner {
                    continue;
                }
                let id = forge_id(owner, runner, &gap.key, gap.occurrences);
                let attempts = tools
                    .forges_for(owner)
                    .filter(|forge| forge.gap.runner_id == runner && forge.gap.key == gap.key)
                    .count();
                if attempts >= MAX_FORGES_PER_GAP || tools.forges.iter().any(|forge| forge.id == id)
                {
                    continue;
                }
                let own = tools.tool(&tool_id(owner, runner, &gap.key));
                if own.is_some_and(ForgedTool::disabled_by_owner) {
                    continue;
                }
                let (failing, passing) = examples(practice, owner, runner, &gap.key);
                if failing.is_empty() || passing.is_empty() {
                    continue;
                }
                // 先看同一缺口的工具，再看同一用户在该运行器下的其他已启用工具。
                let mut enabled: Vec<_> = tools.enabled_for(owner, runner).collect();
                enabled.sort_by_key(|(tool, _)| tool.gap_key != gap.key);
                let reuse = enabled.into_iter().find_map(|(tool, version)| {
                    let verification = verify(&version.spec, &failing, &passing);
                    verification.passed().then(|| {
                        (
                            ToolRef {
                                tool_id: tool.id.clone(),
                                version: version.version,
                            },
                            verification,
                        )
                    })
                });
                return Some(Candidate {
                    owner: owner.into(),
                    gap: GapRef {
                        runner_id: runner.into(),
                        key: gap.key.clone(),
                        summary: gap.summary.clone(),
                    },
                    occurrences: gap.occurrences,
                    failing,
                    passing,
                    reuse,
                    current: own
                        .and_then(ForgedTool::latest)
                        .map(|latest| latest.spec.clone()),
                });
            }
        }
        None
    }

    /// 复用时直接保存已结束的复用记录；否则保存 Running 准入。已处理过返回 None。
    pub fn begin(&self, candidate: &Candidate, now_ms: u64) -> ToolResult<Option<ForgeAttempt>> {
        match &candidate.reuse {
            Some((reference, verification)) => self.admin.record_reuse(
                &candidate.owner,
                candidate.gap.clone(),
                candidate.occurrences,
                reference.clone(),
                verification.clone(),
                now_ms,
            ),
            None => self.admin.begin_forge(
                &candidate.owner,
                candidate.gap.clone(),
                candidate.occurrences,
                self.forger.version(),
                now_ms,
            ),
        }
    }

    /// 推进已保存为 Running 的锻造：请求锻造器，回放验证后保存结果。存储失败直接返回错误，
    /// 宿主须停止并重新打开核对；锻造失败已写入账本，不重试。
    pub async fn complete(
        &self,
        entry: &ForgeAttempt,
        candidate: &Candidate,
        now_ms: impl Fn() -> u64 + Send + Sync,
    ) -> ToolResult<ForgeAttempt> {
        if entry.status != ForgeStatus::Running {
            return Err(ToolError::Conflict);
        }
        let request = ForgeRequest {
            forge_id: entry.id.clone(),
            forger_version: entry.forger_version.clone(),
            gap: candidate.gap.clone(),
            runner: self.runner.clone(),
            failing: candidate.failing.clone(),
            passing: candidate.passing.clone(),
            current: candidate.current.clone(),
        };
        let (result, verification) = match self.forger.forge(request).await {
            Ok(ForgeOutput::Check(spec)) if spec.validate().is_err() => {
                (Err(ForgeFailure::InvalidOutput), None)
            }
            Ok(ForgeOutput::Check(spec)) => {
                let verification = verify(&spec, &candidate.failing, &candidate.passing);
                (Ok(ForgeOutput::Check(spec)), Some(verification))
            }
            Ok(output) => (Ok(output), None),
            Err(ToolError::Forge(failure)) => (Err(failure), None),
            Err(error) => return Err(error),
        };
        let at = now_ms().max(entry.started_at_ms);
        self.admin.record_forge(&entry.id, at, result, verification)
    }
}

/// 取该用户、该运行器的真实草稿：出现该问题的尝试与实际运行验证通过的尝试，各取最近的至多
/// `MAX_EXAMPLES` 份。被运行前检查拦下的尝试不算样例，它们没有实际运行。
fn examples(
    practice: &PracticeSnapshot,
    owner: &str,
    runner: &str,
    key: &str,
) -> (Vec<ExampleFiles>, Vec<ExampleFiles>) {
    let mut failing = Vec::new();
    let mut passing = Vec::new();
    for run in practice
        .runs
        .iter()
        .filter(|run| run.task.owner == owner && run.runner.runner_id == runner)
    {
        for attempt in &run.attempts {
            let Some(draft) = attempt.draft.as_ref().filter(|draft| draft.applicable) else {
                continue;
            };
            let at = attempt.finished_at_ms.unwrap_or(attempt.started_at_ms);
            let example = ExampleFiles {
                run_id: run.id.clone(),
                attempt: attempt.number,
                files: draft.files.clone(),
            };
            let checked = attempt
                .issues
                .iter()
                .any(|issue| issue.starts_with(eve_practice_api::DRAFT_CHECK_MARK));
            let warnings = attempt
                .evidence
                .iter()
                .flat_map(|evidence| &evidence.warnings);
            if !checked
                && attempt
                    .issues
                    .iter()
                    .chain(warnings)
                    .any(|issue| gap_key(issue) == key)
            {
                failing.push((at, example));
            } else if attempt.outcome == Some(AttemptOutcome::Verified) {
                passing.push((at, example));
            }
        }
    }
    let recent = |mut list: Vec<(u64, ExampleFiles)>| {
        list.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
        list.into_iter()
            .take(MAX_EXAMPLES)
            .map(|(_, example)| example)
            .collect()
    };
    (recent(failing), recent(passing))
}

/// 实践运行前调用当前启用的锻造工具：只读草稿，先保存调用记录再返回结果。
/// 调用记录已满时不再调用（宿主的 /tools 会显示），实践照常交给运行器实际验证。
pub struct ForgedChecks {
    admin: Arc<dyn ToolAdmin>,
}
impl ForgedChecks {
    pub fn new(admin: Arc<dyn ToolAdmin>) -> Self {
        Self { admin }
    }
}
impl DraftCheck for ForgedChecks {
    fn check(
        &self,
        run: &PracticeRun,
        draft: &PracticeDraft,
        at_ms: u64,
    ) -> PracticeResult<Vec<String>> {
        let snapshot = self
            .admin
            .snapshot()
            .map_err(|_| PracticeError::Unavailable)?;
        let attempt = run.current().ok_or(PracticeError::CorruptState)?.number;
        let mut issues = Vec::new();
        for (tool, version) in snapshot.enabled_for(&run.task.owner, &run.runner.runner_id) {
            let findings = evaluate(&version.spec, &draft.files);
            let call = ToolCall {
                id: call_id(&tool.id, &run.id, attempt),
                tool: ToolRef {
                    tool_id: tool.id.clone(),
                    version: version.version,
                },
                owner: run.task.owner.clone(),
                run_id: run.id.clone(),
                attempt,
                at_ms,
                findings: findings.clone(),
            };
            match self.admin.record_call(call) {
                Ok(_) => {}
                Err(ToolError::LimitReached) => break,
                Err(_) => return Err(PracticeError::Storage),
            }
            if !findings.is_empty() {
                issues.push(format!(
                    "{}（工具 {} 第 {} 版：{}）",
                    version.spec.message,
                    version.spec.name,
                    version.version,
                    findings.join("；")
                ));
            }
        }
        Ok(issues)
    }
}
