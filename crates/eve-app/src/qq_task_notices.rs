//! 已授权后台任务的定时进度提醒；从真实账本取状态，不把持久 Running 当作活进程证据。
//! 主动发送沿用原 QQ 会话路由，认领、预算与回执由任务账本持久化。
use eve_developer_api::DeveloperAuthorization;
use eve_llm_api::ContextScope;
use eve_outreach_api::OutreachAdmin;
use eve_plugin_api::{PluginError, PluginResult};
use eve_qqbot_plugin::{
    QQ_OUTREACH_MAX_BYTES, QqOutreach, QqOutreachFuture, QqOutreachMessage, QqOutreachMoment,
    QqOutreachPush, QqOutreachResult,
};
use eve_session_api::SessionKey;
use eve_task_api::{
    DeliveryChannel, DeliveryResult, Task, TaskAdmin, TaskError, TaskOwner, TaskStatus, bounded,
};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const ID_PREFIX: &str = "taskprogress-";

fn now_ms() -> PluginResult<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|time| u64::try_from(time.as_millis()).ok())
        .filter(|time| *time != 0)
        .ok_or_else(|| PluginError::State("进度提醒时钟不可用".into()))
}
fn error(error: TaskError) -> PluginError {
    PluginError::State(error.to_string())
}
fn message_id(task: &Task, sequence: u64) -> String {
    format!("{ID_PREFIX}{}-{sequence}", task.id)
}

/// 只展示已保存结果和真实停止/排队状态；没有本地运行句柄时不宣称正在持续执行。
pub(crate) fn progress_text(task: &Task, now_ms: u64) -> String {
    let execution = task.execution.as_ref();
    let checkpoint = execution.and_then(|e| e.checkpoint.as_ref());
    let state = match task.status {
        TaskStatus::Queued => "排队等待执行，尚未开始".to_owned(),
        TaskStatus::Running if task.cancel_requested => "已收到停止请求，等待执行器确认停止".into(),
        TaskStatus::Running
            if execution.is_some_and(|e| {
                e.authorization_deadline_at_ms
                    .is_some_and(|at| at <= now_ms)
                    || e.deadline_at_ms.is_some_and(|at| at <= now_ms)
            }) =>
        {
            "原授权或执行期限已到，等待停止状态核对；没有新的执行确认".into()
        }
        TaskStatus::Running if execution.is_some_and(|e| e.resume_at_ms.is_some()) => {
            let at = execution.and_then(|e| e.resume_at_ms).unwrap_or(now_ms);
            if at > now_ms {
                format!(
                    "等待自动恢复，约 {} 秒后到达恢复时点",
                    (at - now_ms).div_ceil(1000)
                )
            } else {
                "已到自动恢复时点，等待调度接续；尚无新的执行确认".into()
            }
        }
        TaskStatus::Running => {
            "执行器已接收任务，以下是最近保存的进度；当前进程是否仍在执行尚无独立确认".into()
        }
        TaskStatus::Blocked => "本次执行已停止，等待补充信息或解除阻塞".into(),
        TaskStatus::Cancelled => "已停止并取消".into(),
        TaskStatus::Completed => "任务已结束并保存结果".into(),
        TaskStatus::Failed => "执行已停止，任务未完成".into(),
        TaskStatus::Interrupted => "执行已中断，任务未完成".into(),
    };
    let mut text = format!(
        "后台任务「{}」进度提醒\n{}。\n已派发 {} 分钟。",
        task.spec.title,
        state,
        now_ms.saturating_sub(task.created_at_ms) / 60_000
    );
    if let Some(checkpoint) = checkpoint {
        let steps = checkpoint
            .used_tool_calls
            .saturating_add(checkpoint.delegated_tool_calls);
        text.push_str(&format!(
            "已记账工具步骤 {steps}/{}，模型请求 {} 次。",
            task.spec.max_steps,
            checkpoint
                .provider_requests
                .saturating_add(checkpoint.delegated_model_requests)
        ));
        if !checkpoint.pending_calls.is_empty() {
            text.push_str("最近工具调用尚无确认结果，不能认定成功或自动重放。");
        }
        if let Some(remaining) = &checkpoint.remaining {
            text.push_str(&format!(
                "\n已保存的剩余工作：{}",
                crate::qq_task_diagnostics::safe_detail(remaining, 500)
            ));
        }
        let window = crate::qq_task_diagnostics::task_step_tail(task, 2);
        for step in window.steps {
            let tool = step
                .get("tool")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("工具");
            let status = step
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("未确认");
            let detail = step
                .get("detail")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            text.push_str(&format!("\n最近步骤 {tool}：{status}，{detail}"));
        }
    } else {
        text.push_str("尚无已保存的执行步骤。");
    }
    if matches!(
        task.status,
        TaskStatus::Blocked | TaskStatus::Failed | TaskStatus::Interrupted
    ) && let Some(reason) = task.failure.as_ref().or(task.result.as_ref())
    {
        text.push_str(&format!(
            "\n当前原因：{}",
            crate::qq_task_diagnostics::safe_detail(reason, 500)
        ));
    }
    text.push_str(&format!("\n任务编号 {}。", task.id));
    bounded(&text, QQ_OUTREACH_MAX_BYTES)
}

pub(crate) struct ChannelAdapter {
    pub(crate) tasks: Arc<dyn TaskAdmin>,
    pub(crate) authorization: Arc<dyn DeveloperAuthorization>,
    pub(crate) outreach: Option<Arc<dyn OutreachAdmin>>,
}
impl ChannelAdapter {
    fn allowed(&self, owner: &TaskOwner) -> PluginResult<bool> {
        // Web 收件箱有独立持久结算；QQ 不能认领其他通道而消耗其提醒机会。
        if !owner.session_id.starts_with("qq:") {
            return Ok(false);
        }
        let scope = ContextScope {
            session_id: owner.session_id.clone(),
            user_id: owner.user_id.clone(),
        };
        if !self.authorization.permits_business(&scope) || !self.authorization.is_operator(&scope) {
            return Ok(false);
        }
        match &self.outreach {
            Some(outreach) => outreach
                .snapshot()
                .map(|snapshot| !snapshot.quiet(&owner.user_id))
                .map_err(|error| PluginError::State(error.to_string())),
            None => Ok(true),
        }
    }
    fn next_at(&self, now_ms: u64) -> PluginResult<Option<QqOutreachPush>> {
        let snapshot = self.tasks.progress_snapshot().map_err(error)?;
        snapshot.validate().map_err(error)?;
        for entry in snapshot.due(now_ms) {
            if !self.allowed(&entry.owner)? {
                continue;
            }
            let task = match self.tasks.claim_progress(
                &entry.owner,
                &entry.id,
                DeliveryChannel::Proactive,
                now_ms,
            ) {
                Ok(task) => task,
                Err(TaskError::InvalidTransition | TaskError::NotFound) => continue,
                Err(failure) => return Err(error(failure)),
            };
            if !entry.matches(&task) {
                return Err(error(TaskError::CorruptState));
            }
            let notice = task
                .execution
                .as_ref()
                .and_then(|execution| execution.progress_notices.last())
                .filter(|notice| {
                    notice.attempt.result.is_none()
                        && notice.attempt.channel == DeliveryChannel::Proactive
                })
                .ok_or_else(|| error(TaskError::CorruptState))?;
            // 认领后再次检查撤销/quiet；没有写出可如实结算，绝不发送到另一范围。
            if !self.allowed(&task.owner)? {
                self.tasks
                    .record_progress_delivery(
                        &task.owner,
                        &task.id,
                        notice.sequence,
                        DeliveryResult::NotSent,
                        now_ms,
                    )
                    .map_err(error)?;
                continue;
            }
            return Ok(Some(QqOutreachPush {
                session: Some(SessionKey {
                    session_id: task.owner.session_id.clone(),
                    user_id: task.owner.user_id.clone(),
                }),
                user_id: task.owner.user_id.clone(),
                message: QqOutreachMessage {
                    id: message_id(&task, notice.sequence),
                    text: progress_text(&task, now_ms),
                },
            }));
        }
        Ok(None)
    }
    fn record_at(&self, id: &str, result: QqOutreachResult, now_ms: u64) -> PluginResult<()> {
        let snapshot = self.tasks.progress_snapshot().map_err(error)?;
        snapshot.validate().map_err(error)?;
        let (task, sequence) = snapshot
            .tasks
            .iter()
            .find_map(|entry| {
                let notice = entry.notices.last()?;
                (id == format!("{ID_PREFIX}{}-{}", entry.id, notice.sequence)
                    && notice.attempt.channel == DeliveryChannel::Proactive
                    && notice.attempt.result.is_none())
                .then_some((entry, notice.sequence))
            })
            .ok_or_else(|| error(TaskError::NotFound))?;
        let result = match result {
            QqOutreachResult::Sent {
                platform_message_id,
            } => match platform_message_id.filter(|id| eve_task_api::validate_id(id).is_ok()) {
                Some(id) => DeliveryResult::Sent {
                    platform_message_id: Some(id),
                },
                None => DeliveryResult::Unknown,
            },
            QqOutreachResult::Failed {
                http_status,
                biz_code,
            } => DeliveryResult::Failed {
                http_status,
                biz_code,
            },
            QqOutreachResult::NotSent => DeliveryResult::NotSent,
            QqOutreachResult::Unknown => DeliveryResult::Unknown,
        };
        self.tasks
            .record_progress_delivery(&task.owner, &task.id, sequence, result, now_ms)
            .map_err(error)?;
        Ok(())
    }
}
impl QqOutreach for ChannelAdapter {
    fn due(&self, _session: &SessionKey) -> PluginResult<bool> {
        Ok(false)
    }
    fn judge(&self, _moment: QqOutreachMoment) -> QqOutreachFuture {
        Box::pin(async { Ok(false) })
    }
    fn attach(
        &self,
        _session: &SessionKey,
        _message_id: &str,
    ) -> PluginResult<Option<QqOutreachMessage>> {
        Ok(None)
    }
    fn next_push(&self) -> PluginResult<Option<QqOutreachPush>> {
        self.next_at(now_ms()?)
    }
    fn delivered(&self, id: &str, result: QqOutreachResult) -> PluginResult<()> {
        self.record_at(id, result, now_ms()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
    use eve_outreach_plugin::OutreachPlugin;
    use eve_task_api::{TASK_PROGRESS_INTERVAL_MS, TaskEnd, TaskSpec, TaskUsage};
    use eve_task_plugin::TaskPlugin;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Authorization(AtomicBool);
    impl DeveloperAuthorization for Authorization {
        fn is_operator(&self, scope: &ContextScope) -> bool {
            scope.user_id == "alice" && self.0.load(Ordering::SeqCst)
        }
    }
    fn owner(session: &str, user: &str) -> TaskOwner {
        TaskOwner {
            session_id: session.into(),
            user_id: user.into(),
        }
    }
    fn spec() -> TaskSpec {
        TaskSpec {
            title: "核对交付物".into(),
            instructions: "逐项验证并保存实际结果".into(),
            max_steps: 100,
            context: None,
        }
    }

    #[tokio::test]
    async fn proactive_progress_uses_original_scope_quiet_permissions_and_actual_receipts() {
        let directory = tempfile::tempdir().unwrap();
        let kernel = Kernel::with_services(KernelServices {
            state: Arc::new(FileStateStore::open(directory.path()).unwrap()),
            ..KernelServices::default()
        });
        let plugin = TaskPlugin::new().unwrap();
        let tasks = plugin.controller();
        let outreach_plugin = OutreachPlugin::new().unwrap();
        let outreach = outreach_plugin.controller();
        kernel.register(Box::new(plugin)).unwrap();
        kernel.register(Box::new(outreach_plugin)).unwrap();
        kernel.start_all().await.unwrap();
        let scope = owner("qq:original-group", "alice");
        let task = tasks.create(&scope, spec(), "progress", 10).unwrap();
        let web = tasks
            .create(&owner("web:owner", "alice"), spec(), "web-progress", 1)
            .unwrap();
        let unauthorized = tasks
            .create(&owner("qq:foreign", "bob"), spec(), "foreign-progress", 1)
            .unwrap();
        let auth = Arc::new(Authorization(AtomicBool::new(true)));
        let channel = ChannelAdapter {
            tasks: Arc::new(tasks.clone()),
            authorization: auth.clone(),
            outreach: Some(Arc::new(outreach.clone())),
        };
        let at = task.created_at_ms + TASK_PROGRESS_INTERVAL_MS;
        assert!(channel.next_at(at - 1).unwrap().is_none());
        outreach.set_quiet("alice", true, at).unwrap();
        assert!(channel.next_at(at).unwrap().is_none());
        outreach.set_quiet("alice", false, at + 1).unwrap();
        auth.0.store(false, Ordering::SeqCst);
        assert!(channel.next_at(at + 1).unwrap().is_none());
        auth.0.store(true, Ordering::SeqCst);
        let push = channel.next_at(at + 1).unwrap().unwrap();
        assert_eq!(
            push.session,
            Some(SessionKey {
                session_id: scope.session_id.clone(),
                user_id: scope.user_id.clone()
            })
        );
        assert!(push.message.id.starts_with(ID_PREFIX));
        assert!(push.message.text.contains("排队等待执行，尚未开始"));
        assert!(!channel.due(&push.session.clone().unwrap()).unwrap());
        let persisted = tasks.get_owned(&scope, &task.id).unwrap().unwrap();
        assert!(
            persisted.execution.unwrap().progress_notices[0]
                .attempt
                .result
                .is_none()
        );
        for task in [web, unauthorized] {
            assert!(
                tasks
                    .get_owned(&task.owner, &task.id)
                    .unwrap()
                    .unwrap()
                    .execution
                    .unwrap()
                    .progress_notices
                    .is_empty()
            );
        }
        channel
            .record_at(
                &push.message.id,
                QqOutreachResult::Sent {
                    platform_message_id: Some("actual-platform-message-1".into()),
                },
                at + 2,
            )
            .unwrap();
        let stored = tasks.get_owned(&scope, &task.id).unwrap().unwrap();
        assert_eq!(stored.status, TaskStatus::Queued);
        assert_eq!(
            stored.execution.unwrap().progress_notices[0].attempt.result,
            Some(DeliveryResult::Sent {
                platform_message_id: Some("actual-platform-message-1".into())
            })
        );
        assert!(
            channel
                .next_at(at + TASK_PROGRESS_INTERVAL_MS)
                .unwrap()
                .is_none()
        );
        let second = channel
            .next_at(at + 1 + TASK_PROGRESS_INTERVAL_MS)
            .unwrap()
            .unwrap();
        channel
            .record_at(
                &second.message.id,
                QqOutreachResult::Unknown,
                at + 2 + TASK_PROGRESS_INTERVAL_MS,
            )
            .unwrap();
        assert!(
            channel
                .next_at(at + 4 * TASK_PROGRESS_INTERVAL_MS)
                .unwrap()
                .is_none()
        );
        kernel.stop_all().await.unwrap();
    }

    #[tokio::test]
    async fn status_text_distinguishes_saved_running_recovery_blocked_and_stopped() {
        let kernel = Kernel::new();
        let plugin = TaskPlugin::new().unwrap();
        let tasks = plugin.controller();
        kernel.register(Box::new(plugin)).unwrap();
        kernel.start_all().await.unwrap();
        let scope = owner("qq:original", "alice");
        let task = tasks.create(&scope, spec(), "states", 1).unwrap();
        let mut running = tasks.start(&task.id, 2).unwrap();
        assert!(progress_text(&running, 5).contains("当前进程是否仍在执行尚无独立确认"));
        running.execution.as_mut().unwrap().resume_at_ms = Some(10_000);
        assert!(progress_text(&running, 9_000).contains("等待自动恢复，约 1 秒"));
        assert!(progress_text(&running, 10_001).contains("等待调度接续"));
        running.cancel_requested = true;
        assert!(progress_text(&running, 9_000).contains("等待执行器确认停止"));
        let blocked = tasks
            .finish(
                &task.id,
                TaskEnd::Blocked {
                    result: "缺少本地验收文件".into(),
                },
                TaskUsage::default(),
                3,
            )
            .unwrap();
        let text = progress_text(&blocked, 5);
        assert!(text.contains("本次执行已停止，等待补充信息或解除阻塞"));
        assert!(text.contains("缺少本地验收文件"));
        assert!(!text.contains("执行器已接收"));
        let cancelled = tasks.cancel("alice", &task.id, 4).unwrap();
        assert!(progress_text(&cancelled, 5).contains("已停止并取消"));
        kernel.stop_all().await.unwrap();
    }
}
