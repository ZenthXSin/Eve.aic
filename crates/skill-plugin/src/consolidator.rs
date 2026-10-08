//! 技能固化：已验证实践 → 一次提炼请求 → 宿主检查（形状、逐字还原、验证参数、运行器结构）
//! → 在全新目录中用宿主选取的参数实际运行 → 验证通过才写入技能版本并启用。
//! 每一步先经账本保存，返回后才执行下一项外部动作；停止与超时由宿主调用 `abandon_distillation`。
use eve_practice_api::{
    MAX_ISSUE_BYTES, MAX_ISSUES, PracticeDraft, PracticeRunner, PracticeSnapshot, PracticeStatus,
    RunEvidence, RunExit, RunnerProfile,
};
use eve_skill_api::*;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// 一次可以提炼的已验证实践。
pub struct Candidate {
    pub owner: String,
    pub origin: SkillOrigin,
    pub source: PracticeDraft,
    pub evidence: RunEvidence,
}

pub struct Consolidator {
    admin: Arc<dyn SkillAdmin>,
    distiller: Arc<dyn SkillDistiller>,
    runner: Arc<dyn PracticeRunner>,
}

impl Consolidator {
    pub fn new(
        admin: Arc<dyn SkillAdmin>,
        distiller: Arc<dyn SkillDistiller>,
        runner: Arc<dyn PracticeRunner>,
    ) -> Self {
        Self {
            admin,
            distiller,
            runner,
        }
    }

    pub fn distiller_version(&self) -> &str {
        self.distiller.version()
    }

    pub fn runner(&self) -> &RunnerProfile {
        self.runner.profile()
    }

    /// 本次提炼使用的工作目录；只在宿主给定的根目录之下。
    pub fn workspace(root: &Path, distillation_id: &str) -> PathBuf {
        root.join(distillation_id)
    }

    /// 找出最早一次尚未提炼的已验证实践。技能实例验证通过的实践不再提炼，避免重复造技能。
    pub fn candidate(
        &self,
        skills: &SkillSnapshot,
        practice: &PracticeSnapshot,
    ) -> Option<Candidate> {
        let runner_id = &self.runner.profile().runner_id;
        let mut runs: Vec<_> = practice
            .runs
            .iter()
            .filter(|run| {
                run.status == PracticeStatus::Verified && &run.runner.runner_id == runner_id
            })
            .collect();
        runs.sort_by_key(|run| (run.finished_at_ms, run.id.clone()));
        runs.into_iter().find_map(|run| {
            if skills.distillation(&distillation_id(&run.id)).is_some() {
                return None;
            }
            let attempt = run.verified_attempt()?;
            let from_skill = attempt.number == 1
                && skills
                    .selection(&run.id)
                    .is_some_and(|selection| selection.status == SelectionStatus::Chosen);
            if from_skill {
                return None;
            }
            Some(Candidate {
                owner: run.task.owner.clone(),
                origin: SkillOrigin {
                    practice_run_id: run.id.clone(),
                    attempt: attempt.number,
                    goal_id: run.task.goal_id.clone(),
                },
                source: attempt.draft.clone()?,
                evidence: attempt.evidence.clone()?,
            })
        })
    }

    /// 推进已保存为 Running 的提炼直到结局。存储失败直接返回错误，宿主须停止并重新打开核对；
    /// 提炼与验证失败都已写入账本，不重试同一步。
    pub async fn consolidate(
        &self,
        distillation: &Distillation,
        source_evidence: &RunEvidence,
        workspace_root: &Path,
        now_ms: impl Fn() -> u64 + Send + Sync,
    ) -> SkillResult<Distillation> {
        let mut entry = distillation.clone();
        let workspace = Self::workspace(workspace_root, &entry.id);
        loop {
            if entry.status != DistillStatus::Running {
                let _ = std::fs::remove_dir_all(&workspace);
                return Ok(entry);
            }
            let at_ms = |at: u64| at.max(entry.started_at_ms);
            match entry.stage {
                DistillStage::Proposing => {
                    let snapshot = self.admin.snapshot()?;
                    let mut existing: Vec<_> = snapshot
                        .skills_for(&entry.owner)
                        .filter(|skill| skill.runner_id == entry.runner.runner_id)
                        .filter_map(|skill| {
                            snapshot.summary(&SkillRef {
                                skill_id: skill.id.clone(),
                                version: skill.latest()?.version,
                            })
                        })
                        .collect();
                    existing.truncate(MAX_CANDIDATES);
                    let request = DistillRequest {
                        distillation_id: entry.id.clone(),
                        distiller_version: self.distiller.version().into(),
                        runner: entry.runner.clone(),
                        source: entry.source.clone(),
                        evidence: source_evidence.clone(),
                        existing,
                    };
                    let result = match self.distiller.distill(request).await {
                        Ok(proposal) => Ok(proposal),
                        Err(SkillError::Skill(failure)) => Err(failure),
                        Err(error) => return Err(error),
                    };
                    let (issues, holdout) = match &result {
                        Ok(Proposal::Skill(proposal)) => self.check(&snapshot, &entry, proposal),
                        _ => (vec![], None),
                    };
                    entry = self.admin.record_proposal(
                        &entry.id,
                        at_ms(now_ms()),
                        result,
                        issues,
                        holdout,
                    )?;
                }
                DistillStage::Verifying => {
                    let draft = entry.holdout_draft().ok_or(SkillError::CorruptState)?;
                    let directory = workspace.join("holdout");
                    let evidence = match prepare(&directory) {
                        Ok(()) => self.runner.run(&draft, &directory).await,
                        Err(reason) => start_failed(reason),
                    };
                    let _ = std::fs::remove_dir_all(&directory);
                    entry = self
                        .admin
                        .record_verification(&entry.id, at_ms(now_ms()), evidence)?;
                }
            }
        }
    }

    /// 宿主对提案的全部检查；问题为空时给出验证参数。
    fn check(
        &self,
        snapshot: &SkillSnapshot,
        entry: &Distillation,
        proposal: &SkillProposal,
    ) -> (Vec<String>, Option<Arguments>) {
        let (mut issues, holdout) = proposal_issues(proposal, &entry.source, &entry.runner);
        issues.extend(snapshot.lineage_issues(&entry.owner, &entry.runner.runner_id, proposal));
        if issues.is_empty()
            && let Some(draft) = holdout
                .as_ref()
                .and_then(|holdout| instantiate(&proposal.template, holdout).ok())
        {
            issues.extend(
                self.runner
                    .check(&draft)
                    .into_iter()
                    .map(|issue| format!("验证实例：{issue}")),
            );
        }
        let issues = sanitize(issues);
        if issues.is_empty() {
            (issues, holdout)
        } else {
            (issues, None)
        }
    }
}

/// 问题按账本上限截断成单行；不改写含义。
pub(crate) fn sanitize(issues: Vec<String>) -> Vec<String> {
    issues
        .into_iter()
        .filter_map(|issue| {
            let line = issue.replace(['\r', '\n'], " ");
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let mut end = line.len().min(MAX_ISSUE_BYTES);
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            Some(line[..end].to_string())
        })
        .take(MAX_ISSUES)
        .collect()
}

/// 每次验证使用全新的空目录；残留目录只可能来自本宿主先前中断的验证。
fn prepare(directory: &Path) -> Result<(), &'static str> {
    if directory.exists() {
        std::fs::remove_dir_all(directory).map_err(|_| "无法清理残留的工作目录")?;
    }
    std::fs::create_dir_all(directory).map_err(|_| "无法创建工作目录")
}

fn start_failed(reason: &str) -> RunEvidence {
    RunEvidence {
        runtime_version: String::new(),
        exit: RunExit::StartFailed,
        loaded: false,
        warnings: vec![],
        probes: vec![],
        log_excerpt: reason.into(),
        log_sha256: "0".repeat(64),
        log_bytes: 0,
        duration_ms: 0,
    }
}
