//! 不可变正文保留两份时的收尾上界；不是另一个执行状态，也不产生任何回执。
use crate::{cold::directory, store};
use eve_task_api::*;
use serde::Serialize;

/// 流式统计实际 serde JSON 字节，避免为历史大正文构造第二份副本。
fn encoded_len(value: &impl Serialize) -> TaskResult<usize> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::other("task capacity arithmetic overflow"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).map_err(|_| TaskError::LimitReached)?;
    Ok(counter.0)
}

/// 同一上下文、检查点、授权正文下，直到收尾/通知/经历确认所需的最大完整正文。
/// 所构造的投影只供测量，可以同时取互斥字段的最大值，绝不能保存或作为执行证据。
pub(super) fn closeout_maximum(task: &Task, raw_bytes: usize) -> TaskResult<usize> {
    if task.settled_at().is_some() {
        return Ok(raw_bytes);
    }
    let canonical = encoded_len(task)?;
    let mut current = directory(task);
    current.result = task.result.clone();
    current.failure = task.failure.clone();
    let current_size = encoded_len(&current)?;

    let mut maximum = current;
    maximum.status = TaskStatus::SessionEnded; // 最长状态名。
    maximum.cancel_requested = false; // JSON false 比 true 多一个字节。
    maximum.started_at_ms = Some(u64::MAX);
    maximum.finished_at_ms = Some(u64::MAX);
    maximum.usage = Some(TaskUsage {
        tool_calls: u32::MAX,
        model_requests: u32::MAX,
    });
    // 普通结果允许控制字符；一个 UTF-8 字节最坏编码为六字节的 \u0000。
    maximum.result = Some("\0".repeat(MAX_RESULT_BYTES));
    maximum.failure = Some("\0".repeat(MAX_FAILURE_BYTES));
    let attempt = DeliveryAttempt {
        // validate_id 禁止控制字符，但允许反斜线，因此每个 ID 最坏扩为两倍。
        channel: DeliveryChannel::Passive {
            message_id: "\\".repeat(256),
        },
        started_at_ms: u64::MAX,
        finished_at_ms: Some(u64::MAX),
        result: Some(DeliveryResult::StoredLocally {
            notification_id: "\\".repeat(256),
        }),
    };
    maximum.notice = (!task.is_exploration()).then(|| Notice {
        status: NoticeStatus::Delivering,
        attempts: vec![attempt.clone(); MAX_ATTEMPTS],
        closed_at_ms: Some(u64::MAX),
    });
    let execution = maximum.execution.get_or_insert_with(TaskExecution::default);
    execution.last_finished_at_ms = Some(u64::MAX);
    execution.recoveries = u32::MAX;
    execution.resume_at_ms = Some(u64::MAX);
    execution.deadline_at_ms = Some(u64::MAX);
    execution.authorization_deadline_at_ms = Some(u64::MAX);
    execution.session_end_reason = Some(TaskSessionEndReason::RecoveryLimitReached);
    execution.exploration_acknowledged_at_ms = Some(u64::MAX);
    execution.progress_notices = if task.is_exploration() {
        vec![]
    } else {
        vec![
            TaskProgressNotice {
                sequence: u64::MAX,
                status: TaskStatus::SessionEnded,
                observed_finished_at_ms: Some(u64::MAX),
                attempt,
            };
            MAX_TASK_PROGRESS_NOTICES
        ]
    };
    let metadata = encoded_len(&maximum)?;
    let mut maximum = canonical
        .checked_sub(current_size)
        .and_then(|base| base.checked_add(metadata))
        .ok_or(TaskError::LimitReached)?;

    if let Some(execution) = &task.execution
        && let Some(contract) = &execution.completion_contract
    {
        // 重启可能把已保存的 result 转成固定验收修复消息。按真实消息构造器计量：
        // 三条消息数组去掉 []、新增三个逗号，再含 closing:true->false，合计不超过数组长度；
        // result/remaining 从最短非空字符串变成 null 还各可能多一个字节。
        // 每次修复 recoveries 增一，因此新增消息会消耗已预留的一份 G，不会额外抢占空间。
        let growth = encoded_len(&store::completion_repair_messages(
            contract,
            &"\0".repeat(MAX_RESULT_BYTES),
            &"\0".repeat(MAX_FAILURE_BYTES),
            u32::MAX,
            usize::MAX,
        ))?
        .checked_add(2)
        .ok_or(TaskError::LimitReached)?;
        maximum = maximum
            .checked_add(
                growth
                    .checked_mul(MAX_AUTO_RECOVERIES.saturating_sub(execution.recoveries) as usize)
                    .ok_or(TaskError::LimitReached)?,
            )
            .ok_or(TaskError::LimitReached)?;
    }
    // 非规范空白的旧 JSON 原样保留；新编码不会比同内容的最大投影更大。
    Ok(maximum.max(raw_bytes))
}
