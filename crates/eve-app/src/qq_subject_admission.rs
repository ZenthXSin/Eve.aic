//! 普通轮次与后台执行共用主体优先队列；租约只覆盖一次同步受理。
use eve_plugin_api::{PluginError, PluginResult};
use eve_qqbot_plugin::{QqAdmission, QqAdmissionResult};
use eve_session_api::SessionKey;
use eve_subject_api::*;
use std::sync::Arc;

pub(crate) struct Admission(pub Arc<dyn SubjectAdmin>);
fn scope(key: &SessionKey) -> SubjectScope {
    SubjectScope {
        channel: "qq".into(),
        conversation_id: key.session_id.clone(),
        principal_id: key.user_id.clone(),
    }
}
fn error(e: impl ToString) -> PluginError {
    PluginError::State(e.to_string())
}
// 平台消息ID有独立的UTF-8容量契约；主体来源键不能用加前缀后的原文越过256字节。
// 短来源保留既有持久键，长来源使用完整字节摘要，拒绝截断引起的碰撞。
fn source_key(source: &str) -> String {
    if source.len() <= 200 {
        return source.into();
    }
    let hash = ring::digest::digest(&ring::digest::SHA256, source.as_bytes());
    format!(
        "source-sha256:{}",
        hash.as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}
impl Admission {
    pub(crate) fn begin_work(
        &self,
        key: &SessionKey,
        source: &str,
        priority: EventPriority,
        origin: EventSource,
    ) -> PluginResult<Option<String>> {
        let now = crate::qq_tasks::now_ms();
        let mut event = self
            .0
            .enqueue(
                SubjectEventInput {
                    scope: scope(key),
                    source: origin,
                    dedupe_key: source_key(source),
                    priority,
                    kind: "work_admission".into(),
                    payload: String::new(),
                },
                now,
            )
            .map_err(error)?;
        if event.status == EventStatus::Failed && event.outcome.as_deref() == Some("admission_busy")
        {
            event = self
                .0
                .rearm_busy_admission(&scope(key), &event.id, now)
                .map_err(error)?;
        }
        if event.status.terminal() {
            return Err(error("主体准入已了结或未知，禁止重新提交原工作"));
        }
        let Some(claimed) = self
            .0
            .claim_filtered(
                "qq-work-admission",
                &SubjectClaim::Event(event.id),
                now,
                5000,
            )
            .map_err(error)?
        else {
            return Ok(None);
        };
        let token = claimed.lease.ok_or_else(|| error("准入缺少租约"))?.token;
        if !claimed.checkpoint.resume_data.is_empty() {
            let outcome = match claimed.checkpoint.resume_data.as_str() {
                "admission_started" => {
                    SubjectOutcome::Completed("此前已确认受理；仅结算账本，不重放".into())
                }
                "admission_rejected" => SubjectOutcome::Failed("此前已明确拒绝；仅结算账本".into()),
                _ => return Err(error("准入检查点格式无效，未提交")),
            };
            self.0.settle(&token, outcome, now).map_err(error)?;
            return Err(error("已持久化受理结果，未重复提交工作"));
        }
        self.0
            .checkpoint(
                &token,
                SubjectCheckpoint {
                    boundary: CheckpointBoundary::EffectPending,
                    resume_data: String::new(),
                },
                now,
            )
            .map_err(error)?;
        serde_json::to_string(&token).map(Some).map_err(error)
    }
}
impl QqAdmission for Admission {
    fn begin(&self, key: &SessionKey, source: &str) -> PluginResult<Option<String>> {
        self.begin_work(key, source, EventPriority::User, EventSource::UserIngress)
    }
    fn begin_control(
        &self,
        key: &SessionKey,
        source: &str,
        stop: bool,
    ) -> PluginResult<Option<String>> {
        self.begin_work(
            key,
            source,
            if stop {
                EventPriority::Stop
            } else {
                EventPriority::User
            },
            EventSource::UserIngress,
        )
    }
    fn finish(&self, ticket: &str, result: QqAdmissionResult) -> PluginResult<()> {
        let token: LeaseToken = serde_json::from_str(ticket).map_err(error)?;
        let now = crate::qq_tasks::now_ms();
        match result {
            QqAdmissionResult::Retry => {
                self.0
                    .checkpoint(&token, SubjectCheckpoint::default(), now)
                    .map_err(error)?;
                self.0
                    .settle(&token, SubjectOutcome::Failed("admission_busy".into()), now)
                    .map_err(error)?;
            }
            QqAdmissionResult::Unknown => {
                self.0
                    .mark_unknown(&token, "同步受理结果未知；不得自动提交", now)
                    .map_err(error)?;
            }
            QqAdmissionResult::Started | QqAdmissionResult::Rejected => {
                self.0
                    .checkpoint(
                        &token,
                        SubjectCheckpoint {
                            boundary: CheckpointBoundary::Safe,
                            resume_data: match result {
                                QqAdmissionResult::Started => "admission_started",
                                _ => "admission_rejected",
                            }
                            .into(),
                        },
                        now,
                    )
                    .map_err(error)?;
                let outcome = match result {
                    QqAdmissionResult::Started => {
                        SubjectOutcome::Completed("已受理独立执行；不是业务成功或投递确认".into())
                    }
                    _ => SubjectOutcome::Failed("实际入口明确拒绝；未自动重试".into()),
                };
                self.0.settle(&token, outcome, now).map_err(error)?;
            }
        }
        Ok(())
    }
    fn abandon(&self, key: &SessionKey, source: &str) -> PluginResult<()> {
        let scope = scope(key);
        for event in self.0.snapshot_for(&scope).map_err(error)?.events {
            if event.input.kind == "work_admission" && event.input.dedupe_key == source_key(source)
            {
                self.0
                    .cancel_queued_event(&scope, &event.id, crate::qq_tasks::now_ms())
                    .map_err(error)?;
            }
        }
        Ok(())
    }
    fn completed(&self, key: &SessionKey, source: &str, known: bool) -> PluginResult<()> {
        self.0
            .observe_completed(
                SubjectEventInput {
                    scope: scope(key),
                    source: EventSource::TaskExecutor,
                    dedupe_key: source_key(&format!("outcome:{source}")),
                    priority: EventPriority::Recovery,
                    kind: "dialogue_lifecycle".into(),
                    payload: if known {
                        "控制报告已取得；投递仍按QQ回执判断"
                    } else {
                        "控制结果未知；不得自动重放"
                    }
                    .into(),
                },
                crate::qq_tasks::now_ms(),
            )
            .map_err(error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
    use eve_subject_plugin::SubjectPlugin;
    fn key(s: &str) -> SessionKey {
        SessionKey::new(s, "owner").unwrap()
    }
    async fn open(state: Arc<dyn eve_plugin_api::StateStore>) -> (Kernel, Admission) {
        let kernel = Kernel::with_services(KernelServices {
            state,
            ..Default::default()
        });
        let plugin = SubjectPlugin::new().unwrap();
        let admin = Arc::new(plugin.controller());
        kernel.register(Box::new(plugin)).unwrap();
        kernel.start_all().await.unwrap();
        (kernel, Admission(admin))
    }
    #[tokio::test]
    async fn actual_start_is_gated_by_global_priority_and_long_work_releases_lease() {
        let (_kernel, admission) = open(Arc::new(MemoryStateStore::default())).await;
        use eve_task_api::TaskAdmin;
        let plugin = eve_task_plugin::TaskPlugin::new().unwrap();
        let tasks = plugin.controller();
        _kernel.register(Box::new(plugin)).unwrap();
        _kernel
            .start(&eve_plugin_api::PluginId::new(eve_task_api::TASK_PLUGIN_ID).unwrap())
            .await
            .unwrap();
        let original = tasks
            .create(
                &eve_task_api::TaskOwner {
                    session_id: "task".into(),
                    user_id: "owner".into(),
                },
                eve_task_api::TaskSpec {
                    title: "真实后台准入".into(),
                    instructions: "隔离验证".into(),
                    max_steps: 5,
                    context: None,
                },
                "admission-test",
                crate::qq_tasks::now_ms(),
            )
            .unwrap();
        let chat = admission.begin(&key("chat"), "chat-1").unwrap().unwrap();
        // 此时全局lease尚未释放，后台不得start。
        assert!(
            admission
                .begin_work(
                    &key("task"),
                    "task-1",
                    EventPriority::Autonomy,
                    EventSource::AutonomyScheduler
                )
                .unwrap()
                .is_none()
        );
        assert!(
            admission
                .begin_control(&key("stop"), "cancel-1", true)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            tasks.snapshot().unwrap().get(&original.id).unwrap().status,
            eve_task_api::TaskStatus::Queued
        );
        admission.finish(&chat, QqAdmissionResult::Started).unwrap();
        // 模拟聊天仍在执行；租约已经释放，取消先于后台，不等待聊天完成。
        assert!(
            admission
                .begin_work(
                    &key("task"),
                    "task-1",
                    EventPriority::Autonomy,
                    EventSource::AutonomyScheduler
                )
                .unwrap()
                .is_none()
        );
        let stop = admission
            .begin_control(&key("stop"), "cancel-1", true)
            .unwrap()
            .unwrap();
        admission.finish(&stop, QqAdmissionResult::Started).unwrap();
        let task = admission
            .begin_work(
                &key("task"),
                "task-1",
                EventPriority::Autonomy,
                EventSource::AutonomyScheduler,
            )
            .unwrap()
            .unwrap();
        tasks
            .start(&original.id, crate::qq_tasks::now_ms())
            .unwrap();
        admission.finish(&task, QqAdmissionResult::Started).unwrap();
        assert_eq!(
            tasks.snapshot().unwrap().get(&original.id).unwrap().status,
            eve_task_api::TaskStatus::Running
        );
        let snapshot = admission.0.snapshot_for(&scope(&key("chat"))).unwrap();
        assert_eq!(snapshot.events.len(), 1);
        assert!(snapshot.events[0].input.payload.is_empty());
        assert!(admission.begin(&key("chat"), "chat-1").is_err());
    }
    #[tokio::test]
    async fn restart_unknown_never_replays_and_other_scope_continues() {
        let state = Arc::new(MemoryStateStore::default());
        let (kernel, admission) = open(state.clone()).await;
        let _unknown = admission.begin(&key("a"), "unknown").unwrap().unwrap();
        assert!(admission.begin(&key("b"), "abandoned").unwrap().is_none());
        kernel.stop_all().await.unwrap();
        drop(admission);
        drop(kernel);
        let (_kernel, admission) = open(state).await;
        assert_eq!(
            admission
                .0
                .abandon_pending_kind("work_admission", crate::qq_tasks::now_ms())
                .unwrap(),
            1
        );
        assert!(admission.begin(&key("a"), "unknown").is_err());
        let next = admission.begin(&key("c"), "fresh").unwrap().unwrap();
        admission.finish(&next, QqAdmissionResult::Started).unwrap();
        assert_eq!(
            admission.0.snapshot_for(&scope(&key("a"))).unwrap().events[0].status,
            EventStatus::Unknown
        );
    }
    #[tokio::test]
    async fn known_busy_can_retry_but_cancelled_queue_cannot_block_other_windows() {
        let (_kernel, admission) = open(Arc::new(MemoryStateStore::default())).await;
        let ticket = admission.begin(&key("a"), "retry").unwrap().unwrap();
        admission.finish(&ticket, QqAdmissionResult::Retry).unwrap();
        let unrelated = admission
            .begin(&key("independent"), "independent")
            .unwrap()
            .unwrap();
        admission
            .finish(&unrelated, QqAdmissionResult::Started)
            .unwrap();
        let ticket = admission.begin(&key("a"), "retry").unwrap().unwrap();
        assert!(admission.begin(&key("b"), "cancelled").unwrap().is_none());
        admission.abandon(&key("b"), "cancelled").unwrap();
        admission
            .finish(&ticket, QqAdmissionResult::Started)
            .unwrap();
        let next = admission.begin(&key("c"), "fresh").unwrap().unwrap();
        admission.finish(&next, QqAdmissionResult::Started).unwrap();
    }
    #[tokio::test]
    async fn accepted_receipt_survives_crash_before_settle_without_resubmission() {
        let state = Arc::new(MemoryStateStore::default());
        let (kernel, admission) = open(state.clone()).await;
        let ticket = admission.begin(&key("a"), "accepted").unwrap().unwrap();
        let token: LeaseToken = serde_json::from_str(&ticket).unwrap();
        admission
            .0
            .checkpoint(
                &token,
                SubjectCheckpoint {
                    boundary: CheckpointBoundary::Safe,
                    resume_data: "admission_started".into(),
                },
                crate::qq_tasks::now_ms(),
            )
            .unwrap();
        kernel.stop_all().await.unwrap();
        drop(admission);
        drop(kernel);
        let (_kernel, admission) = open(state).await;
        assert!(admission.begin(&key("a"), "accepted").is_err());
        assert_eq!(
            admission.0.snapshot_for(&scope(&key("a"))).unwrap().events[0].status,
            EventStatus::Completed
        );
    }
    #[tokio::test]
    async fn long_utf8_sources_keep_distinct_identity_and_cancel_lookup() {
        let (_kernel, admission) = open(Arc::new(MemoryStateStore::default())).await;
        let a = format!("qq:{}a", "群".repeat(170));
        let b = format!("qq:{}b", "群".repeat(170));
        assert_ne!(source_key(&a), source_key(&b));
        assert_eq!(source_key("qq:short"), "qq:short");
        let ticket = admission.begin(&key("a"), &a).unwrap().unwrap();
        assert!(admission.begin(&key("b"), &b).unwrap().is_none());
        admission.abandon(&key("b"), &b).unwrap();
        admission
            .finish(&ticket, QqAdmissionResult::Started)
            .unwrap();
        admission.completed(&key("a"), &a, true).unwrap();
        let events = admission.0.snapshot_for(&scope(&key("a"))).unwrap().events;
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.input.dedupe_key.len() <= 256));
        assert_eq!(
            admission.0.snapshot_for(&scope(&key("b"))).unwrap().events[0].status,
            EventStatus::Cancelled
        );
    }
}
