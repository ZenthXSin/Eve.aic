//! 单次、显式授权的文档行动。文件操作和日志保存由同一个 owned worker 顺序完成。
//! 取消不终止操作系统中的文件调用：等待它返回，保留可能存在的产物，再保存失败状态。

use eve_action_api::{
    ActionError, ActionExecutionReport, ActionFailure, ActionJournal, ActionOutcome,
    ActionPrecondition, ActionRecord, ActionResult, ActionStatus, ArtifactReceipt, ArtifactTarget,
    DocumentActionProposal,
};
use ring::digest::{SHA256, digest};
use std::{
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Notify;

#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

/// 克隆后指向同一个取消状态。取消调用后仍须等待执行 future 返回，才能确认 worker 收尾。
#[derive(Clone, Default)]
pub struct ActionCancellation {
    state: Arc<CancellationState>,
}

impl fmt::Debug for ActionCancellation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActionCancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl ActionCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if !self.state.cancelled.swap(true, Ordering::AcqRel) {
            self.state.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    /// 等待取消请求，不代表文件 worker 已经结束。
    pub async fn cancelled(&self) {
        loop {
            let notification = self.state.notify.notified();
            tokio::pin!(notification);
            notification.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notification.await;
        }
    }
}

struct CancelOnDrop {
    cancellation: ActionCancellation,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            // 调用方丢弃 future 时只能请求取消。worker 仍保有全部资源并负责最终封存。
            self.cancellation.cancel();
        }
    }
}

/// 零模型调用、一次新建、一次尝试；先保存 Executing，再检查前提并写入。
///
/// 同一单调时钟预算覆盖排队、前提检查、写入和回读。同步文件系统调用无法强制中断，
/// 超时或取消时仍等待 worker 返回，不清理或重做可能存在的目标文件。
/// 最终日志提交开始后返回实际持久化结果；不能逆转已提交的 Completed。
pub async fn execute_document_action(
    journal: Arc<dyn ActionJournal>,
    target: Arc<dyn ArtifactTarget>,
    proposal: DocumentActionProposal,
    bytes: Vec<u8>,
    precondition: Arc<dyn ActionPrecondition>,
    cancellation: ActionCancellation,
) -> ActionResult<ActionExecutionReport> {
    let started = Instant::now();
    let mut guard = CancelOnDrop {
        cancellation: cancellation.clone(),
        armed: true,
    };
    let result = tokio::task::spawn_blocking(move || {
        let worker = Worker {
            journal,
            target,
            proposal,
            bytes,
            precondition,
            cancellation,
            started,
        };
        worker.execute()
    })
    .await;
    guard.armed = false;
    result.map_err(|_| ActionError::Unavailable)?
}

struct Worker {
    journal: Arc<dyn ActionJournal>,
    target: Arc<dyn ArtifactTarget>,
    proposal: DocumentActionProposal,
    bytes: Vec<u8>,
    precondition: Arc<dyn ActionPrecondition>,
    cancellation: ActionCancellation,
    started: Instant,
}

impl Worker {
    fn execute(&self) -> ActionResult<ActionExecutionReport> {
        let mut active = None;
        let mut committing = false;
        match catch_unwind(AssertUnwindSafe(|| self.run(&mut active, &mut committing))) {
            Ok(result) => result,
            Err(_) => {
                // begin/finish 内部异常可能已经提交，不能猜测或覆盖其结果。
                // 只有已取得 Executing 回执、尚未开始终态提交时，才负责封存未知执行结果。
                if let Some(record) = active.filter(|_| !committing) {
                    catch_unwind(AssertUnwindSafe(|| {
                        self.finish(&record, self.blocked(ActionFailure::Interrupted))
                    }))
                    .unwrap_or(Err(ActionError::Unavailable))
                } else {
                    Err(ActionError::Unavailable)
                }
            }
        }
    }

    fn run(
        &self,
        active: &mut Option<ActionRecord>,
        committing: &mut bool,
    ) -> ActionResult<ActionExecutionReport> {
        self.validate_input()?;

        // 历史请求可以在目标前提变化后查询结果；仍由 begin 核对完整请求一致性。
        // 这里不生成新记录，也不把旧行动重新交给目标能力。
        let snapshot = self.journal.snapshot()?;
        snapshot.validate().map_err(|_| ActionError::CorruptState)?;
        if snapshot.subject_id != self.proposal.subject_id {
            return Err(ActionError::SubjectMismatch);
        }
        if snapshot
            .records
            .iter()
            .any(|record| record.proposal.action_id == self.proposal.action_id)
        {
            let begin = self.journal.begin(self.proposal.clone())?;
            self.validate_record(&begin.record)?;
            if !begin.duplicate {
                return Err(ActionError::CorruptState);
            }
            return Ok(ActionExecutionReport {
                record: begin.record,
                duplicate: true,
            });
        }

        self.check_before_begin()?;
        let precondition = self.precondition.check(&self.proposal);
        self.check_before_begin()?;
        precondition?;

        let begin = self.journal.begin(self.proposal.clone())?;
        self.validate_record(&begin.record)?;
        if begin.duplicate {
            return Ok(ActionExecutionReport {
                record: begin.record,
                duplicate: true,
            });
        }
        if begin.record.status != ActionStatus::Executing {
            return Err(ActionError::CorruptState);
        }
        *active = Some(begin.record.clone());

        let mut outcome = match self.perform_io() {
            Ok(receipt) => ActionOutcome::Completed(receipt),
            Err(failure) => self.blocked(failure),
        };
        // 这是终态提交前最后的取消点，包含回读校验与序列化准备消耗的预算。
        if let Some(failure) = self.stop_reason() {
            outcome = self.blocked(failure);
        }
        *committing = true;
        self.finish(&begin.record, outcome)
    }

    fn validate_input(&self) -> ActionResult<()> {
        self.proposal.validate()?;
        if self.bytes.len() as u64 != self.proposal.artifact_byte_count
            || sha256(&self.bytes) != self.proposal.artifact_sha256
            || self.target.source_id() != self.proposal.artifact_source_id
            || std::str::from_utf8(&self.bytes)
                .map(|text| text.contains('\0'))
                .unwrap_or(true)
        {
            return Err(ActionError::InvalidInput);
        }
        Ok(())
    }

    fn validate_record(&self, record: &ActionRecord) -> ActionResult<()> {
        record.validate().map_err(|_| ActionError::CorruptState)?;
        if !record.proposal.same_request(&self.proposal) {
            return Err(ActionError::CorruptState);
        }
        Ok(())
    }

    fn check_before_begin(&self) -> ActionResult<()> {
        match self.stop_reason() {
            Some(ActionFailure::Cancelled) => Err(ActionError::Cancelled),
            Some(ActionFailure::DeadlineExceeded) => Err(ActionError::DeadlineExceeded),
            Some(_) => Err(ActionError::Unavailable),
            None => Ok(()),
        }
    }

    fn check_running(&self) -> Result<(), ActionFailure> {
        self.stop_reason().map_or(Ok(()), Err)
    }

    fn stop_reason(&self) -> Option<ActionFailure> {
        if self.cancellation.is_cancelled() {
            Some(ActionFailure::Cancelled)
        } else if self.started.elapsed() >= Duration::from_millis(self.proposal.timeout_ms) {
            Some(ActionFailure::DeadlineExceeded)
        } else {
            None
        }
    }

    fn perform_io(&self) -> Result<ArtifactReceipt, ActionFailure> {
        self.check_running()?;
        let precondition = self.precondition.check(&self.proposal);
        self.check_running()?;
        precondition.map_err(|_| ActionFailure::PreconditionChanged)?;
        if self.target.source_id() != self.proposal.artifact_source_id {
            return Err(ActionFailure::PreconditionChanged);
        }
        self.check_running()?;
        let written = self.target.write_new(&self.bytes);
        self.check_running()?;
        written.map_err(|_| ActionFailure::WriteFailed)?;

        self.check_running()?;
        let verified_at_ms = now_ms().map_err(|_| ActionFailure::Interrupted)?;
        let read_back = self.target.read_back(verified_at_ms);
        self.check_running()?;
        let receipt = read_back.map_err(|_| ActionFailure::VerificationFailed)?;
        if receipt.validate().is_err()
            || !receipt.matches_proposal(&self.proposal)
            || receipt.verified_at_ms != verified_at_ms
        {
            return Err(ActionFailure::VerificationFailed);
        }
        Ok(receipt)
    }

    fn blocked(&self, failure: ActionFailure) -> ActionOutcome {
        ActionOutcome::Blocked {
            failure,
            // 时钟故障不能阻止封存；提案的有效时间仅作日志保底，未冒充回读证据。
            finished_at_ms: now_ms().unwrap_or(self.proposal.created_at_ms),
        }
    }

    fn finish(
        &self,
        record: &ActionRecord,
        outcome: ActionOutcome,
    ) -> ActionResult<ActionExecutionReport> {
        let expected = outcome.clone();
        let finished = self
            .journal
            .finish(&self.proposal.action_id, record.revision, outcome)?;
        self.validate_record(&finished)?;
        if finished.status == ActionStatus::Executing || finished.revision <= record.revision {
            return Err(ActionError::CorruptState);
        }
        match expected {
            ActionOutcome::Completed(receipt)
                if finished.status == ActionStatus::Completed
                    && finished.receipt.as_ref() == Some(&receipt) => {}
            ActionOutcome::Blocked {
                failure,
                finished_at_ms,
            } if finished.status == ActionStatus::Blocked
                && finished.failure == Some(failure)
                && finished.finished_at_ms == Some(finished_at_ms) => {}
            _ => return Err(ActionError::CorruptState),
        }
        Ok(ActionExecutionReport {
            record: finished,
            duplicate: false,
        })
    }
}

fn now_ms() -> ActionResult<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .filter(|at_ms| *at_ms > 0)
        .ok_or(ActionError::Unavailable)
}

fn sha256(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
