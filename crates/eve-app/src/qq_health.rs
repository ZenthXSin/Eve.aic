//! 健康巡检组合层：读取宿主统计，不请求模型；按可信主人路由投递一次。
use crate::{AppError, qq_admin::Admin};
use eve_health_watch::{Delivery, Finding, HealthSource, HealthWatch};
use eve_llm_api::{
    ContextScope, Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext,
    ToolExecutionError, ToolFuture, ToolValidationError,
};
use eve_plugin_api::{PluginError, PluginResult};
use eve_qqbot_plugin::{
    QqBotStatusHandle, QqConversations, QqInteractionSource, QqOutreach, QqOutreachFuture,
    QqOutreachMessage, QqOutreachMoment, QqOutreachPush, QqOutreachResult,
};
use eve_session_api::SessionKey;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
};

fn error(s: impl ToString) -> PluginError {
    PluginError::State(s.to_string())
}
pub(crate) struct Source {
    pub(crate) health: Arc<eve_model_health::HealthBook>,
    pub(crate) channel: OnceLock<Arc<QqBotStatusHandle>>,
    pub(crate) budget: crate::qq_budget::Gate,
    directory: PathBuf,
    min_free_bytes: u64,
}
impl HealthSource for Source {
    fn inspect(&self) -> Result<Vec<Finding>, String> {
        let mut findings = vec![];
        for (role, state) in self.health.snapshot() {
            if state.last_succeeded == Some(false) && state.consecutive_failures >= 3 {
                findings.push(Finding {
                    code: format!("model.{role}"),
                    summary: format!(
                        "模型用途 {role} 连续失败 {} 次，最近类别 {}",
                        state.consecutive_failures,
                        state
                            .last_failure
                            .map_or("未知".to_owned(), |(failure, _)| format!("{failure:?}"))
                    ),
                });
            }
        }
        match free_bytes(&self.directory) {
            Ok(Some((free, total)))
                if free < self.min_free_bytes
                    || (total > 0 && (free as u128) * 100 < (total as u128) * 5) =>
            {
                findings.push(Finding {
                    code: "disk.low".into(),
                    summary: format!(
                        "状态目录所在磁盘可用 {} MiB，低于巡检阈值",
                        free / 1024 / 1024
                    ),
                });
            }
            Err(()) => findings.push(Finding {
                code: "disk.unreadable".into(),
                summary: "无法读取状态目录所在磁盘的余量".into(),
            }),
            _ => {}
        }
        if let Some(budget) = self.budget.budget() {
            match budget.snapshot(crate::qq_tasks::now_ms()) {
                Ok(snapshot) if snapshot.remaining() == 0 => findings.push(Finding {
                    code: "autonomy.exhausted".into(),
                    summary: "本窗口自主运行预算已用尽，等待原定窗口重置；没有自动增加额度".into(),
                }),
                Err(_) => findings.push(Finding {
                    code: "autonomy.storage".into(),
                    summary: "自主预算记录不可读，需核对持久状态".into(),
                }),
                _ => {}
            }
        }
        if let Some(handle) = self.channel.get() {
            let status = *handle.status.borrow();
            if status.interactions_not_saved > 0 {
                findings.push(Finding {
                    code: "memory.unsaved".into(),
                    summary: format!(
                        "本次进程有 {} 次交流未保存为记忆，需检查容量与记录",
                        status.interactions_not_saved
                    ),
                });
            }
            if status.capacity_rejected > 0 {
                findings.push(Finding {
                    code: "receipts.full".into(),
                    summary: format!(
                        "本次进程有 {} 次请求因回执容量不足被拒绝",
                        status.capacity_rejected
                    ),
                });
            }
            if status.terminal_error {
                findings.push(Finding {
                    code: "qq.terminal".into(),
                    summary: "QQ 通道遇到终止错误".into(),
                });
            }
        }
        Ok(findings)
    }
}
#[cfg(unix)]
fn free_bytes(path: &std::path::Path) -> Result<Option<(u64, u64)>, ()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| ())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // CString 和结果指针在调用期间有效；仅成功时读取已初始化结构。
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(());
    }
    let stat = unsafe { stat.assume_init() };
    let bytes = |blocks: u128| {
        blocks
            .saturating_mul(stat.f_frsize as u128)
            .min(u64::MAX as u128) as u64
    };
    Ok(Some((
        bytes(stat.f_bavail as u128),
        bytes(stat.f_blocks as u128),
    )))
}
#[cfg(not(unix))]
fn free_bytes(_path: &std::path::Path) -> Result<Option<(u64, u64)>, ()> {
    Ok(None)
}

pub(crate) struct Monitor {
    store: HealthWatch,
    pub(crate) source: Source,
    conversations: QqConversations,
    admin: Arc<Admin>,
    interval_ms: u64,
}
impl Monitor {
    pub(crate) fn open(
        directory: PathBuf,
        health: Arc<eve_model_health::HealthBook>,
        budget: crate::qq_budget::Gate,
        conversations: QqConversations,
        admin: Arc<Admin>,
    ) -> Result<Arc<Self>, AppError> {
        let number = |name: &str, default: u64, min: u64, max: u64| -> Result<u64, AppError> {
            match std::env::var(name) {
                Ok(value) => value
                    .parse::<u64>()
                    .ok()
                    .filter(|v| (*v >= min) && (*v <= max))
                    .ok_or_else(|| format!("{name} 无效").into()),
                Err(std::env::VarError::NotPresent) => Ok(default),
                Err(_) => Err(format!("{name} 无效").into()),
            }
        };
        Ok(Arc::new(Self {
            store: HealthWatch::open(directory.join("health-watch.json"))?,
            source: Source {
                health,
                channel: OnceLock::new(),
                budget,
                directory,
                min_free_bytes: number(
                    "EVE_HEALTH_MIN_FREE_BYTES",
                    2 * 1024 * 1024 * 1024,
                    1,
                    u64::MAX,
                )?,
            },
            conversations,
            admin,
            interval_ms: number("EVE_HEALTH_CHECK_MS", 30_000, 1000, 3_600_000)?,
        }))
    }
    fn check(&self) -> Result<(), String> {
        self.store
            .check(&self.source, crate::qq_tasks::now_ms(), self.interval_ms)
    }
}
impl QqOutreach for Monitor {
    fn due(&self, _: &SessionKey) -> PluginResult<bool> {
        Ok(false)
    }
    fn judge(&self, _: QqOutreachMoment) -> QqOutreachFuture {
        Box::pin(async { Ok(false) })
    }
    fn attach(&self, _: &SessionKey, _: &str) -> PluginResult<Option<QqOutreachMessage>> {
        Ok(None)
    }
    fn next_push(&self) -> PluginResult<Option<QqOutreachPush>> {
        self.check().map_err(error)?;
        let identities = self.conversations.recent_identities()?;
        let owners: Vec<_> = identities
            .into_iter()
            .filter(|i| {
                self.admin.is_owner(&ContextScope {
                    session_id: i.session.session_id.clone(),
                    user_id: i.session.user_id.clone(),
                })
            })
            .collect();
        let Some(identity) = owners
            .iter()
            .find(|i| i.source == QqInteractionSource::DirectMessage)
            .or_else(|| owners.first())
        else {
            return Ok(None);
        };
        let session = identity.session.clone();
        let Some(notice) = self
            .store
            .claim(ContextScope {
                session_id: session.session_id.clone(),
                user_id: session.user_id.clone(),
            })
            .map_err(error)?
        else {
            return Ok(None);
        };
        let details = notice
            .findings
            .iter()
            .take(3)
            .map(|f| f.summary.as_str())
            .collect::<Vec<_>>()
            .join("；");
        let text = eve_task_api::bounded(
            &format!(
                "本地巡检发现：{details}。记录 {}，可查看运行健康核对详情。",
                notice.id
            ),
            eve_qqbot_plugin::QQ_OUTREACH_MAX_BYTES,
        );
        Ok(Some(QqOutreachPush {
            user_id: session.user_id.clone(),
            session: Some(session),
            message: QqOutreachMessage {
                id: notice.id,
                text,
            },
        }))
    }
    fn delivered(&self, id: &str, result: QqOutreachResult) -> PluginResult<()> {
        let outcome = match result {
            QqOutreachResult::Sent { .. } => Delivery::Sent,
            QqOutreachResult::Unknown => Delivery::Unknown,
            _ => Delivery::Failed,
        };
        self.store.delivered(id, outcome).map_err(error)
    }
}
pub(crate) struct ReadTool(pub(crate) Arc<Monitor>);
impl Tool for ReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition{name:"inspect_runtime_health".into(),description:"读取宿主低成本巡检结果：模型连续失败、磁盘余量、记忆和回执容量、自主预算及通知状态；不请求模型、不执行修复，不含日志正文或凭据。自动检查只向绑定主人通知新出现的问题一次。原始原因仍应结合日志和代码核对，未知问题先问一次。".into(),argument_schema:json!({"type":"object","properties":{},"additionalProperties":false}),required_permissions:vec![],concurrency:Some(ToolConcurrency::ParallelSafe)}
    }
    fn validate_arguments(&self, a: &Value) -> Result<(), ToolValidationError> {
        if a.as_object().is_none_or(|o| !o.is_empty()) {
            return Err(ToolValidationError {
                message: "运行健康查询不接受参数".into(),
            });
        }
        Ok(())
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            self.validate_arguments(&call.arguments)
                .map_err(|e| ToolExecutionError::Rejected(e.message))?;
            if context.is_cancel_requested() {
                return Err(ToolExecutionError::Cancelled("查询已取消".into()));
            }
            self.0.check().map_err(ToolExecutionError::Rejected)?;
            let snapshot = self
                .0
                .store
                .snapshot()
                .map_err(ToolExecutionError::Rejected)?;
            Ok(
                json!({"checked_at_ms":snapshot.checked_at_ms,"active":snapshot.active,"notices":snapshot.notices.iter().rev().take(20).collect::<Vec<_>>(),"automatic_repair":false}),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_llm_api::{ModelHealthObserver, ModelOutcome};

    #[test]
    fn only_consecutive_model_failure_is_active_and_success_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        let health = Arc::new(eve_model_health::HealthBook::new());
        let source = Source {
            health: health.clone(),
            channel: OnceLock::new(),
            budget: crate::qq_budget::Gate::default(),
            directory: dir.path().to_path_buf(),
            min_free_bytes: 1,
        };
        for at in 1..=2 {
            health.record("dialogue", ModelOutcome::TimedOut, at);
        }
        assert!(
            !source
                .inspect()
                .unwrap()
                .iter()
                .any(|f| f.code == "model.dialogue")
        );
        health.record("dialogue", ModelOutcome::TimedOut, 3);
        let findings = source.inspect().unwrap();
        let failure = findings
            .iter()
            .find(|f| f.code == "model.dialogue")
            .unwrap();
        assert!(failure.summary.contains("3 次"));
        health.record("dialogue", ModelOutcome::Succeeded, 4);
        assert!(
            !source
                .inspect()
                .unwrap()
                .iter()
                .any(|f| f.code == "model.dialogue")
        );
    }
}
