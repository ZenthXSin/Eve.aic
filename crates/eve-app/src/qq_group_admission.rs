//! 群主动回复在同步提交时参与全局主体准入；等待和发送不持有调度租约。
use crate::qq_subject_admission::Admission;
use eve_control_api::*;
use eve_qqbot_plugin::{QqAdmission, QqAdmissionResult};
use eve_session_api::SessionKey;
use std::sync::Arc;

pub(crate) struct GroupControl {
    pub(crate) inner: Arc<dyn ControlService>,
    pub(crate) admission: Arc<Admission>,
}
impl GroupControl {
    pub(crate) fn wrap(
        inner: Arc<dyn ControlService>,
        admission: Arc<Admission>,
    ) -> Arc<dyn ControlService> {
        Arc::new(Self { inner, admission })
    }
    fn admit(
        &self,
        input: &ControlInput,
        submit: impl FnOnce() -> ControlResult<GenerationKey>,
    ) -> ControlResult<GenerationKey> {
        input.validate()?;
        let source = format!("group-chime:{}", input.task_id);
        let Some(ticket) = self
            .admission
            .begin_work(
                &input.session.key,
                &source,
                eve_subject_api::EventPriority::Autonomy,
                eve_subject_api::EventSource::AutonomyScheduler,
            )
            .map_err(|_| ControlError::Blocked)?
        else {
            // 群观察不排队重播旧消息。撤销本次未被领取的意图，避免遗留队首阻塞别的窗口。
            self.admission
                .abandon(&input.session.key, &source)
                .map_err(|_| ControlError::Blocked)?;
            return Err(ControlError::Busy);
        };
        let result = submit();
        let outcome = match &result {
            Ok(_) => QqAdmissionResult::Started,
            Err(ControlError::Busy) => QqAdmissionResult::Retry,
            Err(ControlError::Blocked | ControlError::Unavailable) => QqAdmissionResult::Unknown,
            Err(_) => QqAdmissionResult::Rejected,
        };
        if self.admission.finish(&ticket, outcome).is_err() {
            if let Ok(key) = &result {
                let _ = self.inner.cancel(key);
            }
            return Err(ControlError::Blocked);
        }
        result
    }
}
impl ControlService for GroupControl {
    fn list_keys(&self, after: Option<&str>, limit: usize) -> ControlResult<Vec<SessionKey>> {
        self.inner.list_keys(after, limit)
    }
    fn submit(
        &self,
        input: ControlInput,
        sink: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        self.admit(&input, || self.inner.submit(input.clone(), sink))
    }
    fn submit_if_current(
        &self,
        expected: &GenerationKey,
        input: ControlInput,
        sink: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        self.admit(&input, || {
            self.inner.submit_if_current(expected, input.clone(), sink)
        })
    }
    fn cancel(&self, key: &GenerationKey) -> ControlResult<CancelDisposition> {
        self.inner.cancel(key)
    }
    fn wait(&self, key: &GenerationKey) -> ControlFuture<'static, ControlReport> {
        self.inner.wait(key)
    }
    fn snapshot(&self, key: &SessionKey) -> ControlResult<Option<ControlSnapshot>> {
        self.inner.snapshot(key)
    }
    fn accepts(&self, event: &ControlEvent) -> bool {
        self.inner.accepts(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_chime_api::*;
    use eve_chime_plugin::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountControl(AtomicUsize);
    impl ControlService for CountControl {
        fn submit(
            &self,
            input: ControlInput,
            _: Arc<dyn ControlEventSink>,
        ) -> ControlResult<GenerationKey> {
            if input.task_id == "blocked" {
                return Err(ControlError::Blocked);
            }
            if input.task_id == "unavailable" {
                return Err(ControlError::Unavailable);
            }
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(GenerationKey {
                session: input.session.key,
                task_id: input.task_id,
                controller_epoch: [0; 16],
                generation: 1,
            })
        }
        fn submit_if_current(
            &self,
            _: &GenerationKey,
            input: ControlInput,
            sink: Arc<dyn ControlEventSink>,
        ) -> ControlResult<GenerationKey> {
            self.submit(input, sink)
        }
        fn cancel(&self, _: &GenerationKey) -> ControlResult<CancelDisposition> {
            Ok(CancelDisposition::AlreadyFinished)
        }
        fn wait(&self, key: &GenerationKey) -> ControlFuture<'static, ControlReport> {
            let key = key.clone();
            Box::pin(async move {
                Ok(ControlReport {
                    key,
                    cancel_requested: false,
                    run: RunReport {
                        turn_id: Some(1),
                        commit: CommitState::Completed,
                        text: Some("已核对".into()),
                        transcript: None,
                        started_tools: Some(0),
                        tool_results: vec![],
                        failure: None,
                    },
                })
            })
        }
        fn snapshot(&self, _: &SessionKey) -> ControlResult<Option<ControlSnapshot>> {
            Ok(None)
        }
        fn accepts(&self, _: &ControlEvent) -> bool {
            true
        }
    }
    struct Settings;
    impl ChimeSettingsSource for Settings {
        fn settings(&self) -> Option<ChimeSettings> {
            Some(ChimeSettings {
                enabled: true,
                cooldown_ms: 0,
                max_replies_per_hour: 100,
                min_confidence: 80,
                window_messages: 10,
                judge_timeout: std::time::Duration::from_secs(1),
                max_conversations: 10,
            })
        }
    }
    struct Outlet;
    impl ChimeOutlet for Outlet {
        fn send<'a>(&'a self, _: &'a ChimeReply) -> eve_llm_api::LlmFuture<'a, ()> {
            Box::pin(async { Ok(()) })
        }
    }
    #[tokio::test]
    async fn actual_group_engine_cannot_bypass_another_windows_subject_lease() {
        let kernel = eve_kernel::Kernel::with_services(Default::default());
        let plugin = eve_subject_plugin::SubjectPlugin::new().unwrap();
        let admission = Arc::new(Admission(Arc::new(plugin.controller())));
        kernel.register(Box::new(plugin)).unwrap();
        kernel.start_all().await.unwrap();
        let other = SessionKey::new("private", "alice").unwrap();
        let ticket = admission.begin(&other, "user-priority").unwrap().unwrap();
        let inner = Arc::new(CountControl(AtomicUsize::new(0)));
        let engine = ChimeEngine::new(
            GroupControl::wrap(inner.clone(), admission.clone()),
            Arc::new(QuietJudge),
            Arc::new(eve_segment_plugin::ParagraphPlanner::default()),
            eve_segment_api::SegmentLimits {
                max_segments: 3,
                max_segment_bytes: 4096,
                max_pause_ms: 2000,
            },
            Arc::new(Settings),
            "Eve",
            crate::qq_group::USER,
        );
        let message = |id: &str| GroupMessage {
            id: id.into(),
            conversation: "group".into(),
            speaker: "bob".into(),
            speaker_name: None,
            text: "Eve 请看看".into(),
            mentions_me: true,
            replies_to_me: false,
            from_me: false,
            at_ms: 1000,
        };
        let blocked = engine.observe(message("blocked"), &Outlet).await.unwrap();
        assert!(
            matches!(blocked.delivery, ChimeDelivery::NotGenerated { .. }),
            "群回复必须通过同一主体准入"
        );
        assert_eq!(inner.0.load(Ordering::SeqCst), 0);
        admission
            .finish(&ticket, QqAdmissionResult::Started)
            .unwrap();
        let delivered = engine.observe(message("fresh"), &Outlet).await.unwrap();
        assert!(matches!(delivered.delivery, ChimeDelivery::Sent { .. }));
        assert_eq!(inner.0.load(Ordering::SeqCst), 1);
        let other_ticket = admission.begin(&other, "after-group").unwrap().unwrap();
        admission
            .finish(&other_ticket, QqAdmissionResult::Started)
            .unwrap();
        let repeated = engine.observe(message("fresh"), &Outlet).await.unwrap();
        assert!(matches!(
            repeated.delivery,
            ChimeDelivery::NotGenerated { .. }
        ));
        assert_eq!(inner.0.load(Ordering::SeqCst), 1, "原群回复不能重复执行");
    }
    #[tokio::test]
    async fn blocked_or_unavailable_control_retains_unknown_in_subject_ledger() {
        let kernel = eve_kernel::Kernel::with_services(Default::default());
        let plugin = eve_subject_plugin::SubjectPlugin::new().unwrap();
        let admission = Arc::new(Admission(Arc::new(plugin.controller())));
        kernel.register(Box::new(plugin)).unwrap();
        kernel.start_all().await.unwrap();
        let inner = Arc::new(CountControl(AtomicUsize::new(0)));
        let control = GroupControl::wrap(inner.clone(), admission.clone());
        for id in ["blocked", "unavailable"] {
            let key = SessionKey::new(id, "owner").unwrap();
            let input = ControlInput {
                session: eve_session_api::SessionInput {
                    key: key.clone(),
                    text: "check".into(),
                },
                task_id: id.into(),
            };
            assert!(
                control
                    .submit(input.clone(), Arc::new(DiscardControlEvents))
                    .is_err()
            );
            let saved = admission
                .0
                .snapshot_for(&eve_subject_api::SubjectScope {
                    channel: "qq".into(),
                    conversation_id: id.into(),
                    principal_id: "owner".into(),
                })
                .unwrap();
            assert_eq!(
                saved.events[0].status,
                eve_subject_api::EventStatus::Unknown
            );
            assert_eq!(
                saved.events[0].checkpoint.boundary,
                eve_subject_api::CheckpointBoundary::EffectPending
            );
            assert!(
                control
                    .submit(input, Arc::new(DiscardControlEvents))
                    .is_err()
            );
        }
        assert_eq!(inner.0.load(Ordering::SeqCst), 0);
    }
}
