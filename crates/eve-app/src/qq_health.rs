//! 健康巡检组合层：读取宿主统计，不请求模型；按可信主人路由投递一次。
use crate::AppError;
use eve_health_watch::{
    Delivery, Finding, HealthSource, HealthWatch, NoticeCandidate, Observation,
};
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
    sync::{Arc, Mutex, OnceLock},
};

fn error(s: impl ToString) -> PluginError {
    PluginError::State(s.to_string())
}
/// 主人判断；组合层用同一份平台绑定实现，巡检只读，不改名单。
pub(crate) trait OwnerCheck: Send + Sync {
    fn is_owner(&self, scope: &ContextScope) -> bool;
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
        self.observe(&[], 0).map(|observation| observation.findings)
    }

    fn observe(&self, previous: &[Finding], checked_at_ms: u64) -> Result<Observation, String> {
        let mut findings = vec![];
        let mut unknown = vec![];
        let mut recovered = vec![];
        let roles = self.health.snapshot();
        for (role, state) in &roles {
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
        for finding in previous {
            if let Some(role) = finding.code.strip_prefix("model.") {
                let state = roles.iter().find(|(name, _)| name == role).map(|(_, s)| s);
                if state.is_some_and(|s| s.last_success_at_ms.is_some_and(|at| at > checked_at_ms))
                {
                    recovered.push(finding.code.clone());
                } else if state
                    .is_none_or(|s| s.last_succeeded != Some(true) && s.consecutive_failures < 3)
                {
                    unknown.push(finding.code.clone());
                }
            }
        }
        let disk = free_bytes(&self.directory);
        match disk {
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
        for finding in previous {
            if (finding.code == "disk.low" && !matches!(disk, Ok(Some(_))))
                || (finding.code == "disk.unreadable" && matches!(disk, Ok(None)))
            {
                unknown.push(finding.code.clone());
            }
        }
        if let Some(budget) = self.budget.budget() {
            match budget.snapshot(crate::qq_tasks::now_ms()) {
                Ok(snapshot) if snapshot.remaining() == 0 => findings.push(Finding {
                    code: "autonomy.exhausted".into(),
                    summary: "本窗口自主运行预算已用尽，等待原定窗口重置；没有自动增加额度".into(),
                }),
                Err(_) => {
                    findings.push(Finding {
                        code: "autonomy.storage".into(),
                        summary: "自主预算记录不可读，需核对持久状态".into(),
                    });
                    if previous.iter().any(|f| f.code == "autonomy.exhausted") {
                        unknown.push("autonomy.exhausted".into());
                    }
                }
                _ => {}
            }
        } else {
            // 未装配预算探针不证明旧预算账本已恢复。
            unknown.extend(
                previous
                    .iter()
                    .filter(|finding| {
                        matches!(
                            finding.code.as_str(),
                            "autonomy.exhausted" | "autonomy.storage"
                        )
                    })
                    .map(|finding| finding.code.clone()),
            );
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
        for finding in previous {
            let known = finding.code.starts_with("model.")
                || matches!(
                    finding.code.as_str(),
                    "disk.low" | "disk.unreadable" | "autonomy.exhausted" | "autonomy.storage"
                );
            // 通道计数仅在本进程内有效，零值或尚未挂接不证明重启前的问题已恢复。
            if !known && !findings.iter().any(|f| f.code == finding.code) {
                unknown.push(finding.code.clone());
            }
        }
        Ok(Observation {
            findings,
            unknown,
            recovered,
        })
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
    /// 主人判断；由组合层注入，与群白名单共用同一份绑定。
    admin: Arc<dyn OwnerCheck>,
    interval_ms: u64,
    first_check: Mutex<bool>,
}
impl Monitor {
    pub(crate) fn open(
        directory: PathBuf,
        health: Arc<eve_model_health::HealthBook>,
        budget: crate::qq_budget::Gate,
        conversations: QqConversations,
        admin: Arc<dyn OwnerCheck>,
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
            first_check: Mutex::new(true),
        }))
    }
    fn check(&self) -> Result<(), String> {
        self.check_inner(false)
    }
    fn check_inner(&self, before_delivery: bool) -> Result<(), String> {
        let mut first = self.first_check.lock().map_err(|_| "健康首检不可用")?;
        self.store.check(
            &self.source,
            crate::qq_tasks::now_ms(),
            if *first || before_delivery {
                0
            } else {
                self.interval_ms
            },
        )?;
        *first = false;
        Ok(())
    }

    fn notice_candidate(
        candidate: &eve_qqbot_plugin::QqOutreachCandidate,
    ) -> PluginResult<NoticeCandidate> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Facts {
            kind: String,
            at_ms: u64,
            findings: Vec<Finding>,
            unknown: Vec<Finding>,
            resolutions: Vec<eve_health_watch::Resolution>,
        }
        let facts: Facts = serde_json::from_value(candidate.facts.clone())
            .map_err(|_| error("健康候选事实无效"))?;
        if facts.kind != "health_findings" {
            return Err(error("健康候选类别无效"));
        }
        Ok(NoticeCandidate {
            id: candidate.id.clone(),
            at_ms: facts.at_ms,
            findings: facts.findings,
            unknown: facts.unknown,
            resolutions: facts.resolutions,
        })
    }
}
impl QqOutreach for Monitor {
    fn candidates(&self) -> PluginResult<Vec<eve_qqbot_plugin::QqOutreachCandidate>> {
        self.check().map_err(error)?;
        let owners: Vec<_> = self
            .conversations
            .recent_identities()?
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
            return Ok(vec![]);
        };
        self.store
            .snapshot()
            .map_err(error)?
            .candidates()
            .into_iter()
            .map(|notice| {
                crate::qq_speech::candidate(
                    notice.id,
                    Some(identity.session.clone()),
                    identity.session.user_id.clone(),
                    json!({"kind":"health_findings","at_ms":notice.at_ms,
                        "findings":notice.findings,"unknown":notice.unknown,
                        "resolutions":notice.resolutions}),
                )
            })
            .collect()
    }
    fn claim_candidate(
        &self,
        candidate: &eve_qqbot_plugin::QqOutreachCandidate,
        text: &str,
        _anchor: &str,
    ) -> PluginResult<Option<QqOutreachPush>> {
        self.check_inner(true).map_err(error)?;
        if !self.candidates()?.contains(candidate) {
            return Ok(None);
        }
        let Some(session) = &candidate.session else {
            return Ok(None);
        };
        let selected = Self::notice_candidate(candidate)?;
        let Some(notice) = self
            .store
            .claim_candidate(
                &selected,
                ContextScope {
                    session_id: session.session_id.clone(),
                    user_id: session.user_id.clone(),
                },
            )
            .map_err(error)?
        else {
            return Ok(None);
        };
        Ok(Some(QqOutreachPush {
            session: Some(session.clone()),
            user_id: session.user_id.clone(),
            message: QqOutreachMessage {
                id: notice.id,
                text: text.into(),
            },
        }))
    }
    fn silence_candidate(
        &self,
        candidate: &eve_qqbot_plugin::QqOutreachCandidate,
    ) -> PluginResult<()> {
        self.check_inner(true).map_err(error)?;
        if !self.candidates()?.contains(candidate) {
            return Ok(());
        }
        let Some(session) = &candidate.session else {
            return Ok(());
        };
        self.store
            .suppress_candidate(
                &Self::notice_candidate(candidate)?,
                ContextScope {
                    session_id: session.session_id.clone(),
                    user_id: session.user_id.clone(),
                },
            )
            .map_err(error)
    }

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
        let Some(candidate) = self
            .store
            .snapshot()
            .map_err(error)?
            .candidates()
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        let Some(notice) = self
            .store
            .claim_candidate(
                &candidate,
                ContextScope {
                    session_id: session.session_id.clone(),
                    user_id: session.user_id.clone(),
                },
            )
            .map_err(error)?
        else {
            return Ok(None);
        };
        let details = candidate
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
        ToolDefinition{name:"inspect_runtime_health".into(),description:"读取宿主低成本巡检结果：模型连续失败、磁盘余量、记忆和回执容量、自主预算及通知状态；不请求模型、不执行修复，不含日志正文或凭据。active 是本次确认的异常，unknown 是尚无恢复观测的旧问题，通知保留逐项恢复或后继替代原因。结合日志、代码和现有工具自行核对具体原因。".into(),argument_schema:json!({"type":"object","properties":{},"additionalProperties":false}),required_permissions:vec![],concurrency:Some(ToolConcurrency::ParallelSafe)}
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
                json!({"checked_at_ms":snapshot.checked_at_ms,"active":snapshot.active,"unknown":snapshot.unknown,"notices":snapshot.notices.iter().rev().take(20).collect::<Vec<_>>(),"automatic_repair":false}),
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

    fn source(directory: &std::path::Path, health: Arc<eve_model_health::HealthBook>) -> Source {
        Source {
            health,
            channel: OnceLock::new(),
            budget: crate::qq_budget::Gate::default(),
            directory: directory.to_path_buf(),
            min_free_bytes: 1,
        }
    }

    #[test]
    fn restarted_model_requires_observation_before_retiring_or_notifying_the_old_incident() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        let health = Arc::new(eve_model_health::HealthBook::new());
        let before_restart = source(dir.path(), health.clone());
        for at in 1..=3 {
            health.record("dialogue", ModelOutcome::TimedOut, at);
        }
        let watch = HealthWatch::open(path.clone()).unwrap();
        watch.check(&before_restart, 100, 1).unwrap();
        let old = watch.snapshot().unwrap().candidates().remove(0);
        let health = Arc::new(eve_model_health::HealthBook::new());
        let after_restart = source(dir.path(), health.clone());
        let watch = HealthWatch::open(path).unwrap();
        watch.check(&after_restart, 101, 1).unwrap();
        let snapshot = watch.snapshot().unwrap();
        assert!(snapshot.active.is_empty());
        assert_eq!(snapshot.unknown, old.findings);
        assert!(snapshot.candidates().is_empty());
        assert_eq!(snapshot.notices[0].delivery, Delivery::Pending);
        assert!(snapshot.notices[0].resolutions.is_empty());
        health.register("dialogue");
        watch.check(&after_restart, 102, 1).unwrap();
        for at in 103..=104 {
            health.record("dialogue", ModelOutcome::TimedOut, at);
            watch.check(&after_restart, at, 1).unwrap();
            assert!(watch.snapshot().unwrap().candidates().is_empty());
            assert!(watch.snapshot().unwrap().notices[0].resolutions.is_empty());
        }
        health.record("dialogue", ModelOutcome::Succeeded, 105);
        watch.check(&after_restart, 106, 1).unwrap();
        let snapshot = watch.snapshot().unwrap();
        assert!(snapshot.unknown.is_empty());
        assert_eq!(snapshot.notices[0].delivery, Delivery::Retired);
        assert!(snapshot.notices[0].owner.is_none());
        assert_eq!(
            snapshot.notices[0].resolutions[0].reason,
            eve_health_watch::ResolutionReason::Recovered {}
        );
        for at in 107..=109 {
            health.record("dialogue", ModelOutcome::TimedOut, at);
        }
        watch.check(&after_restart, 110, 1).unwrap();
        let candidate = watch.snapshot().unwrap().candidates().remove(0);
        assert_ne!(candidate.id, old.id);
        assert_eq!(candidate.findings[0].code, "model.dialogue");
    }

    #[test]
    fn success_then_failure_between_checks_is_a_distinct_incident() {
        let dir = tempfile::tempdir().unwrap();
        let health = Arc::new(eve_model_health::HealthBook::new());
        let source = source(dir.path(), health.clone());
        let watch = HealthWatch::open(dir.path().join("health.json")).unwrap();
        for at in 1..=3 {
            health.record("dialogue", ModelOutcome::TimedOut, at);
        }
        watch.check(&source, 100, 1).unwrap();
        health.record("dialogue", ModelOutcome::Succeeded, 101);
        for at in 102..=104 {
            health.record("dialogue", ModelOutcome::TimedOut, at);
        }
        watch.check(&source, 105, 1).unwrap();
        let snapshot = watch.snapshot().unwrap();
        assert_eq!(snapshot.notices.len(), 2);
        assert_eq!(snapshot.notices[0].delivery, Delivery::Retired);
        assert_eq!(snapshot.candidates()[0].id, snapshot.notices[1].id);
        watch.check(&source, 106, 1).unwrap();
        assert_eq!(
            watch.snapshot().unwrap().candidates(),
            snapshot.candidates()
        );
        assert_eq!(watch.snapshot().unwrap().notices.len(), 2);
    }

    #[test]
    fn absent_process_counters_or_removed_probes_are_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let source = source(dir.path(), Arc::new(eve_model_health::HealthBook::new()));
        let previous = [
            "model.retired_role",
            "autonomy.exhausted",
            "autonomy.storage",
            "memory.unsaved",
            "receipts.full",
            "qq.terminal",
            "custom.probe",
        ]
        .map(|code| Finding {
            code: code.into(),
            summary: "旧异常".into(),
        });
        let observation = source.observe(&previous, 100).unwrap();
        assert_eq!(
            observation.unknown,
            previous.iter().map(|f| f.code.clone()).collect::<Vec<_>>()
        );
        assert!(observation.recovered.is_empty());
        assert!(observation.findings.is_empty());
    }

    #[test]
    fn first_monitor_check_ignores_the_previous_process_interval() {
        struct Owner;
        impl OwnerCheck for Owner {
            fn is_owner(&self, _: &ContextScope) -> bool {
                true
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health-watch.json");
        let health = Arc::new(eve_model_health::HealthBook::new());
        let before_restart = source(dir.path(), health.clone());
        for at in 1..=3 {
            health.record("dialogue", ModelOutcome::TimedOut, at);
        }
        let watch = HealthWatch::open(path.clone()).unwrap();
        watch
            .check(&before_restart, crate::qq_tasks::now_ms(), 1)
            .unwrap();
        assert_eq!(watch.snapshot().unwrap().candidates().len(), 1);
        let monitor = Monitor {
            store: HealthWatch::open(path).unwrap(),
            source: source(dir.path(), Arc::new(eve_model_health::HealthBook::new())),
            conversations: QqConversations::default(),
            admin: Arc::new(Owner),
            interval_ms: 3_600_000,
            first_check: Mutex::new(true),
        };
        assert!(monitor.candidates().unwrap().is_empty());
        let snapshot = monitor.store.snapshot().unwrap();
        assert!(snapshot.active.is_empty());
        assert_eq!(snapshot.unknown[0].code, "model.dialogue");
        assert_eq!(snapshot.notices[0].delivery, Delivery::Pending);
        assert!(!*monitor.first_check.lock().unwrap());
    }
}
