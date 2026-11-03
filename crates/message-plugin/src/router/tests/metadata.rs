use super::*;
use eve_llm_api::ContextScope;

struct MetadataControl {
    inner: Arc<Control>,
    received: Mutex<Vec<(ControlInput, Option<ControlRunMetadata>)>>,
}
impl MetadataControl {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Control::new(None),
            received: Mutex::new(vec![]),
        })
    }
}
impl ControlService for MetadataControl {
    fn submit(
        &self,
        input: ControlInput,
        sink: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        self.inner.submit(input, sink)
    }
    fn submit_if_current(
        &self,
        expected: &GenerationKey,
        input: ControlInput,
        sink: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        self.inner.submit_if_current(expected, input, sink)
    }
    fn submit_if_current_with_metadata(
        &self,
        expected: &GenerationKey,
        input: ControlInput,
        metadata: Option<ControlRunMetadata>,
        sink: Arc<dyn ControlEventSink>,
    ) -> ControlResult<GenerationKey> {
        if let Some(metadata) = &metadata {
            metadata.validate_for(&input)?;
        }
        self.received
            .lock()
            .unwrap()
            .push((input.clone(), metadata));
        self.inner.submit_if_current(expected, input, sink)
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

fn metadata(message: &IncomingMessage) -> ControlRunMetadata {
    ControlRunMetadata {
        scope: ContextScope {
            session_id: message.target.session.session_id.clone(),
            user_id: message.target.session.user_id.clone(),
        },
        task_id: message.target.task_id.clone(),
        review: ControlReviewEvidence::ChannelAnchor("trusted-platform-message".into()),
    }
}

#[tokio::test]
async fn replacement_freezes_original_evidence_before_control_admission_and_rebinds_task() {
    for text in ["/new 新的要求", "/add 保留旧成果并增加验证"] {
        for review in [
            ControlReviewEvidence::ChannelAnchor("trusted-platform-message".into()),
            ControlReviewEvidence::FrozenDelegation(vec!["  用户原话\n不得改写  ".into()]),
        ] {
            let control = MetadataControl::new();
            let router = router(control.clone(), Arc::new(RulesJudge));
            let incoming = message(&control.inner, text);
            let mut original = metadata(&incoming);
            original.review = review;
            let ticket = router
                .submit_with_metadata(
                    incoming.clone(),
                    Some(original.clone()),
                    Arc::new(DiscardControlEvents),
                )
                .unwrap();
            let report = bounded(router.wait(&ticket)).await.unwrap();
            let RouteOutcome::Replaced { generation, .. } = report.outcome else {
                panic!("替代要求未准入：{report:?}");
            };
            let received = control.received.lock().unwrap().clone();
            let [(input, Some(actual))] = received.as_slice() else {
                panic!("控制器必须在准入时收到完整证据");
            };
            assert_eq!(actual.scope, original.scope);
            assert_eq!(actual.review, original.review);
            assert_eq!(actual.task_id, input.task_id);
            assert_eq!(actual.task_id, generation.task_id);
            assert_eq!(
                actual.task_id,
                if text.starts_with("/new") {
                    incoming.message_id
                } else {
                    incoming.target.task_id
                }
            );
            router.0.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn duplicate_identity_cannot_replace_or_remove_its_frozen_metadata() {
    let control = MetadataControl::new();
    let judge = SlowJudge::new();
    let router = router(control.clone(), judge.clone());
    let incoming = message(&control.inner, "/new 新的要求");
    let original = metadata(&incoming);
    let ticket = router
        .submit_with_metadata(
            incoming.clone(),
            Some(original.clone()),
            Arc::new(DiscardControlEvents),
        )
        .unwrap();
    bounded(judge.entered.notified()).await;
    assert_eq!(
        router
            .submit_with_metadata(
                incoming.clone(),
                Some(original.clone()),
                Arc::new(DiscardControlEvents),
            )
            .unwrap(),
        ticket
    );
    let mut changed = original.clone();
    changed.review = ControlReviewEvidence::ChannelAnchor("another-message".into());
    let mut changed_task = original.clone();
    changed_task.task_id = "another-task".into();
    for evidence in [Some(changed), Some(changed_task), None] {
        assert_eq!(
            router
                .submit_with_metadata(incoming.clone(), evidence, Arc::new(DiscardControlEvents),),
            Err(MessageError::Conflict)
        );
    }
    judge.release.notify_one();
    assert!(matches!(
        bounded(router.wait(&ticket)).await.unwrap().outcome,
        RouteOutcome::Replaced { .. }
    ));
    let received = control.received.lock().unwrap().clone();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].1.as_ref().unwrap().review, original.review);
    router.0.close().await.unwrap();
}

#[tokio::test]
async fn metadata_requires_exact_target_identity_and_task_before_judging() {
    let control = MetadataControl::new();
    let router = router(control.clone(), Arc::new(RulesJudge));
    let incoming = message(&control.inner, "/new 新的要求");
    let original = metadata(&incoming);
    let mut wrong_session = original.clone();
    wrong_session.scope.session_id = "other-session".into();
    let mut wrong_user = original.clone();
    wrong_user.scope.user_id = "other-user".into();
    let mut wrong_task = original.clone();
    wrong_task.task_id = "other-task".into();
    let mut invalid_anchor = original;
    invalid_anchor.review = ControlReviewEvidence::ChannelAnchor(String::new());
    for evidence in [wrong_session, wrong_user, wrong_task, invalid_anchor] {
        assert_eq!(
            router.submit_with_metadata(
                incoming.clone(),
                Some(evidence),
                Arc::new(DiscardControlEvents),
            ),
            Err(MessageError::InvalidInput)
        );
    }
    assert!(router.0.book.lock().unwrap().entries.is_empty());
    assert_eq!(control.inner.cancellations.load(Ordering::SeqCst), 0);
    assert!(control.received.lock().unwrap().is_empty());
    router.0.close().await.unwrap();
}

#[tokio::test]
async fn unsupported_control_cannot_drop_metadata_and_start_a_replacement() {
    let control = Control::new(None);
    let router = router(control.clone(), Arc::new(RulesJudge));
    let incoming = message(&control, "/new 新的要求");
    let ticket = router
        .submit_with_metadata(
            incoming.clone(),
            Some(metadata(&incoming)),
            Arc::new(DiscardControlEvents),
        )
        .unwrap();
    assert!(matches!(
        bounded(router.wait(&ticket)).await.unwrap().outcome,
        RouteOutcome::ControlFailed {
            error: ControlError::Unavailable,
            ..
        }
    ));
    assert!(control.submissions.lock().unwrap().is_empty());
    router.0.close().await.unwrap();
}

#[tokio::test]
async fn cancellation_and_clarification_do_not_submit_new_authorization() {
    for text in ["/cancel", "这是补充，继续原任务"] {
        let control = MetadataControl::new();
        let router = router(control.clone(), Arc::new(RulesJudge));
        let incoming = message(&control.inner, text);
        let ticket = router
            .submit_with_metadata(
                incoming.clone(),
                Some(metadata(&incoming)),
                Arc::new(DiscardControlEvents),
            )
            .unwrap();
        assert!(matches!(
            bounded(router.wait(&ticket)).await.unwrap().outcome,
            RouteOutcome::Cancelled { .. } | RouteOutcome::Clarify { .. }
        ));
        assert!(control.received.lock().unwrap().is_empty());
        assert!(control.inner.submissions.lock().unwrap().is_empty());
        router.0.close().await.unwrap();
    }
}

#[tokio::test]
async fn natural_classification_reply_keeps_its_own_anchor_when_reusing_question_text() {
    let control = MetadataControl::new();
    let router = router(control.clone(), Arc::new(RulesJudge));
    let question = message(&control.inner, "请也处理这份源码");
    let first = router
        .submit_with_metadata(
            question.clone(),
            Some(metadata(&question)),
            Arc::new(DiscardControlEvents),
        )
        .unwrap();
    assert!(matches!(
        bounded(router.wait(&first)).await.unwrap().outcome,
        RouteOutcome::Clarify { .. }
    ));
    let mut followup = message(&control.inner, "这是新任务");
    followup.message_id = "classification-reply".into();
    let mut evidence = metadata(&followup);
    evidence.review = ControlReviewEvidence::ChannelAnchor(followup.message_id.clone());
    let ticket = router
        .submit_with_metadata(
            followup.clone(),
            Some(evidence.clone()),
            Arc::new(DiscardControlEvents),
        )
        .unwrap();
    assert!(matches!(
        bounded(router.wait(&ticket)).await.unwrap().outcome,
        RouteOutcome::Replaced { .. }
    ));
    let received = control.received.lock().unwrap().clone();
    let [(input, Some(actual))] = received.as_slice() else {
        panic!("澄清后的替代任务必须携带当前消息证据");
    };
    assert_eq!(input.session.text, question.text);
    assert_eq!(actual.review, evidence.review);
    assert_eq!(actual.task_id, followup.message_id);
    router.0.close().await.unwrap();
}
