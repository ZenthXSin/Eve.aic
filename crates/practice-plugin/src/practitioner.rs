//! 一次实践：草稿 → 结构检查 → 在全新目录中实际运行 → 依据证据修正，至多三次尝试。
//! 每一步先经账本保存，返回后才执行下一项外部动作；停止与超时由宿主调用 `abandon`。
use eve_practice_api::*;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub struct Practitioner {
    admin: Arc<dyn PracticeAdmin>,
    drafter: Arc<dyn PracticeDrafter>,
    runner: Arc<dyn PracticeRunner>,
}

impl Practitioner {
    pub fn new(
        admin: Arc<dyn PracticeAdmin>,
        drafter: Arc<dyn PracticeDrafter>,
        runner: Arc<dyn PracticeRunner>,
    ) -> Self {
        Self {
            admin,
            drafter,
            runner,
        }
    }

    pub fn drafter_version(&self) -> &str {
        self.drafter.version()
    }

    pub fn runner(&self) -> &RunnerProfile {
        self.runner.profile()
    }

    /// 本次实践使用的工作目录；只在宿主给定的根目录之下。
    pub fn workspace(root: &Path, run_id: &str) -> PathBuf {
        root.join(run_id)
    }

    /// 推进已保存为 Running 的实践直到结局。存储失败直接返回错误，
    /// 宿主须停止并重新打开核对；草稿与运行失败都已写入账本，不重试同一步。
    pub async fn practice(
        &self,
        run: &PracticeRun,
        workspace_root: &Path,
        now_ms: impl Fn() -> u64 + Send + Sync,
    ) -> PracticeResult<PracticeRun> {
        let mut run = run.clone();
        let workspace = Self::workspace(workspace_root, &run.id);
        loop {
            if run.status != PracticeStatus::Running {
                remove_workspace(&workspace);
                return Ok(run);
            }
            let current = run.current().ok_or(PracticeError::CorruptState)?.clone();
            if current.outcome.is_some() {
                return Err(PracticeError::CorruptState);
            }
            let at_ms = |at: u64| at.max(current.started_at_ms);
            match current.stage {
                AttemptStage::Drafting => {
                    let previous = run.attempts.iter().rev().nth(1).and_then(|attempt| {
                        attempt.draft.clone().map(|draft| PreviousAttempt {
                            draft,
                            issues: attempt.issues.clone(),
                            evidence: attempt.evidence.clone(),
                        })
                    });
                    let request = DraftRequest {
                        run_id: run.id.clone(),
                        drafter_version: self.drafter.version().into(),
                        attempt: current.number,
                        task: run.task.clone(),
                        runner: run.runner.clone(),
                        previous,
                    };
                    let result = match self.drafter.draft(request).await {
                        Ok(draft) => Ok(draft),
                        Err(PracticeError::Practice(failure)) => Err(failure),
                        Err(error) => return Err(error),
                    };
                    let issues = match &result {
                        Ok(draft) if draft.applicable => sanitize(self.runner.check(draft)),
                        _ => Vec::new(),
                    };
                    run = self
                        .admin
                        .record_draft(&run.id, at_ms(now_ms()), result, issues)?;
                }
                AttemptStage::Running => {
                    let draft = current.draft.as_ref().ok_or(PracticeError::CorruptState)?;
                    let directory = workspace.join(format!("attempt-{}", current.number));
                    let evidence = match prepare(&directory) {
                        Ok(()) => self.runner.run(draft, &directory).await,
                        Err(reason) => start_failed(reason),
                    };
                    remove_workspace(&directory);
                    run = self
                        .admin
                        .record_evidence(&run.id, at_ms(now_ms()), evidence)?;
                }
            }
        }
    }
}

/// 运行器给出的问题按账本上限截断成单行；不改写含义。
fn sanitize(issues: Vec<String>) -> Vec<String> {
    issues
        .into_iter()
        .filter_map(|issue| {
            let line = issue.replace(['\r', '\n'], " ");
            let line = line.trim();
            (!line.is_empty()).then(|| prefix(line, MAX_ISSUE_BYTES).to_string())
        })
        .take(MAX_ISSUES)
        .collect()
}

/// 每次尝试使用全新的空目录；残留目录只可能来自本宿主先前中断的尝试。
fn prepare(directory: &Path) -> Result<(), &'static str> {
    if directory.exists() {
        std::fs::remove_dir_all(directory).map_err(|_| "无法清理残留的工作目录")?;
    }
    std::fs::create_dir_all(directory).map_err(|_| "无法创建工作目录")
}

pub(crate) fn remove_workspace(directory: &Path) {
    let _ = std::fs::remove_dir_all(directory);
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

fn prefix(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
