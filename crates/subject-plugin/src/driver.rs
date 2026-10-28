use eve_subject_api::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// 单次有限调度的结果。Started 表示后台任务已被持久化受理，而不是等待长任务执行完。
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "dispatch", rename_all = "snake_case", deny_unknown_fields)]
pub enum SubjectDispatch {
    Completed {
        summary: String,
    },
    Failed {
        reason: String,
    },
    Cancelled,
    Started {
        task_id: String,
        title: String,
        goal: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    subject_driver_receipt_v1: SubjectDispatch,
}

/// 全局租约只覆盖一次有限调度，不能持有它等待模型长任务。
/// 执行闭包使用 event.id 作为外部任务的幂等 source；返回 Started 后任务独立执行。
/// 若外部结果未知，闭包返回错误，立即标记 Unknown 并释放租约，不重放。
pub struct SubjectDriver {
    admin: Arc<dyn SubjectAdmin>,
    worker: String,
    lease_ms: u64,
    filter: SubjectClaim,
}
impl SubjectDriver {
    pub fn new(admin: Arc<dyn SubjectAdmin>, worker: impl Into<String>) -> SubjectResult<Self> {
        let worker = worker.into();
        validate_id(&worker)?;
        Ok(Self {
            admin,
            worker,
            lease_ms: MAX_LEASE_MS,
            filter: SubjectClaim::Any,
        })
    }
    pub fn with_filter(mut self, filter: SubjectClaim) -> Self {
        self.filter = filter;
        self
    }
    pub fn with_lease_ms(mut self, lease_ms: u64) -> SubjectResult<Self> {
        if !(1..=MAX_LEASE_MS).contains(&lease_ms) {
            return Err(SubjectError::InvalidInput);
        }
        self.lease_ms = lease_ms;
        Ok(self)
    }
    /// 每次只处理一个事件；调用方循环运行并持续扫描后台完成事件。
    /// 闭包应立即返回调度回执；不要在闭包里执行/等待长时间的模型调用。
    pub fn run_one(
        &self,
        now_ms: u64,
        dispatch: impl FnOnce(&SubjectEvent) -> SubjectResult<SubjectDispatch>,
    ) -> SubjectResult<Option<SubjectEvent>> {
        let Some(event) =
            self.admin
                .claim_filtered(&self.worker, &self.filter, now_ms, self.lease_ms)?
        else {
            return Ok(None);
        };
        let token = &event.lease.as_ref().ok_or(SubjectError::LeaseLost)?.token;
        // 结果回执已持久化而了结尚未提交时，只重放本地账本操作。
        let receipt = if event.checkpoint.resume_data.is_empty() {
            self.admin.checkpoint(
                token,
                SubjectCheckpoint {
                    boundary: CheckpointBoundary::EffectPending,
                    resume_data: String::new(),
                },
                now_ms,
            )?;
            let result = match dispatch(&event) {
                Ok(result) => result,
                Err(error) => {
                    self.admin.mark_unknown(
                        token,
                        &format!("外部派发结果未知：{error}；不自动重放"),
                        now_ms,
                    )?;
                    return Err(error);
                }
            };
            let receipt = Receipt {
                subject_driver_receipt_v1: result,
            };
            let data = serde_json::to_string(&receipt).map_err(|_| SubjectError::InvalidInput)?;
            self.admin.checkpoint(
                token,
                SubjectCheckpoint {
                    boundary: CheckpointBoundary::Safe,
                    resume_data: data,
                },
                now_ms,
            )?;
            receipt
        } else {
            serde_json::from_str::<Receipt>(&event.checkpoint.resume_data)
                .map_err(|_| SubjectError::InvalidInput)?
        };
        let outcome = match receipt.subject_driver_receipt_v1 {
            SubjectDispatch::Completed { summary } => SubjectOutcome::Completed(summary),
            SubjectDispatch::Failed { reason } => SubjectOutcome::Failed(reason),
            SubjectDispatch::Cancelled => SubjectOutcome::Cancelled,
            SubjectDispatch::Started {
                task_id,
                title,
                goal,
            } => {
                validate_id(&task_id)?;
                self.admin.upsert_focus(
                    SubjectFocus {
                        id: event.id.clone(),
                        scope: event.input.scope.clone(),
                        title,
                        goal,
                        task_id: Some(task_id.clone()),
                        status: FocusStatus::Active,
                        next_check_at_ms: now_ms
                            .checked_add(AUTONOMY_INTERVAL_MS)
                            .ok_or(SubjectError::InvalidInput)?,
                        updated_at_ms: now_ms,
                    },
                    now_ms,
                )?;
                SubjectOutcome::Completed(format!("已派发后台任务 {task_id}"))
            }
        };
        self.admin.settle(token, outcome, now_ms).map(Some)
    }
}
