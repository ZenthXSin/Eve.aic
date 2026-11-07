use super::*;
use eve_control_api::{CommitState, ControlReport, GenerationKey, RunFailure, RunReport};
use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{ChatMessage, ChatRole, ToolResult};

const SILENCE: &str = r#"{"action":"silence"}"#;

fn key(trigger: &Trigger) -> GenerationKey {
    GenerationKey {
        session: trigger.owner.clone(),
        task_id: trigger.id.clone(),
        controller_epoch: [9; 16],
        generation: 3,
    }
}

fn report(trigger: &Trigger, text: &str, internal: bool) -> ControlReport {
    let result = ToolResult::success("checked", json!({"found":true})).unwrap();
    let final_message = if internal {
        ChatMessage::internal_text(text)
    } else {
        ChatMessage::text(ChatRole::Assistant, text)
    };
    ControlReport {
        key: key(trigger),
        cancel_requested: false,
        run: RunReport {
            turn_id: Some(19),
            commit: CommitState::Completed,
            text: Some(text.into()),
            transcript: Some(vec![
                ChatMessage::text(ChatRole::User, "宿主唤醒资料"),
                ChatMessage::assistant_tool_calls(vec![ToolCall {
                    id: "checked".into(),
                    name: "echo".into(),
                    arguments: json!({"text":"核查结果"}),
                }])
                .unwrap(),
                ChatMessage::tool_results(vec![result.clone()]).unwrap(),
                final_message,
            ]),
            started_tools: Some(1),
            tool_results: vec![result],
            failure: None,
        },
    }
}

async fn running() -> (Kernel, Arc<dyn TriggerAdmin>, Trigger) {
    let kernel = Kernel::with_services(KernelServices::default());
    let plugin = eve_trigger_plugin::TriggerPlugin::new().unwrap();
    let admin: Arc<dyn TriggerAdmin> = Arc::new(plugin.controller());
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let trigger = admin
        .create_with_review_messages(
            SessionKey::new("owner", "user").unwrap(),
            "silent-control".into(),
            TriggerSpec {
                title: "稍后检查".into(),
                instructions: "核查后决定是否发言".into(),
                mode: MatchMode::All,
                conditions: vec![Condition::Time { at_ms: 2000 }],
                expires_at_ms: 3000,
                recurrence: None,
                resume_task_id: None,
            },
            vec!["稍后核查，没有新情况就不必通知".into()],
            1000,
        )
        .unwrap();
    admin.ready(&trigger.id, vec!["time".into()]).unwrap();
    admin
        .transition(&trigger.id, Status::Ready, Status::Running, None)
        .unwrap();
    let trigger = admin.snapshot().unwrap().remove(0);
    (kernel, admin, trigger)
}

struct ReportControl(ControlReport);
impl ControlService for ReportControl {
    fn submit(
        &self,
        _: ControlInput,
        _: Arc<dyn eve_control_api::ControlEventSink>,
    ) -> eve_control_api::ControlResult<GenerationKey> {
        unreachable!()
    }
    fn submit_if_current(
        &self,
        _: &GenerationKey,
        _: ControlInput,
        _: Arc<dyn eve_control_api::ControlEventSink>,
    ) -> eve_control_api::ControlResult<GenerationKey> {
        unreachable!()
    }
    fn cancel(
        &self,
        _: &GenerationKey,
    ) -> eve_control_api::ControlResult<eve_control_api::CancelDisposition> {
        Ok(eve_control_api::CancelDisposition::AlreadyFinished)
    }
    fn wait(&self, _: &GenerationKey) -> eve_control_api::ControlFuture<'static, ControlReport> {
        let result = self.0.clone();
        Box::pin(async move { Ok(result) })
    }
    fn snapshot(
        &self,
        _: &SessionKey,
    ) -> eve_control_api::ControlResult<Option<eve_control_api::ControlSnapshot>> {
        Ok(None)
    }
    fn accepts(&self, _: &eve_control_api::ControlEvent) -> bool {
        true
    }
}

#[tokio::test]
async fn exact_current_committed_silence_completes_without_erasing_tool_receipts() {
    for internal in [false, true] {
        let (kernel, admin, trigger) = running().await;
        let text = " \n{\"action\": \"silence\"}\t";
        let report = report(&trigger, text, internal);
        let original = report.clone();
        assert_eq!(
            silent_wakeup_result(&report, &key(&trigger), &trigger),
            Some(text)
        );
        assert_eq!(report, original);
        let control = Arc::new(ReportControl(report));
        let (_stop_tx, stop) = watch::channel(false);
        finish_wakeup(
            admin.clone(),
            control.clone(),
            trigger.clone(),
            key(&trigger),
            stop,
        )
        .await
        .unwrap();
        let finished = admin.snapshot().unwrap().remove(0);
        assert_eq!(finished.status, Status::Completed);
        assert_eq!(finished.delivery, Some(Delivery::Silenced));
        assert_eq!(finished.result.as_deref(), Some(text));
        assert_eq!(finished.runs, 1);
        assert_eq!(finished.review_messages, trigger.review_messages);
        assert_eq!(control.0, original);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn quoted_malformed_and_replayed_silence_strings_remain_ordinary_results() {
    let wrapped = ChatMessage::internal_text(SILENCE)
        .text_for_model()
        .unwrap()
        .into_owned();
    for text in [
        "检查结果含有 action:silence，仍要说明真正结果".to_owned(),
        "```json\n{\"action\":\"silence\"}\n```".to_owned(),
        r#"{"action":"silence","action":"silence"}"#.to_owned(),
        r#"{"action":"silence","text":"需要交付的结果"}"#.to_owned(),
        r#"{"kind":"eve.internal_record","text":"{\"action\":\"silence\"}"}"#.to_owned(),
        wrapped,
        r#""{\"action\":\"silence\"}""#.to_owned(),
        "{\"action\":\"silence\"}\n结果仍未交付".to_owned(),
    ] {
        let (kernel, admin, trigger) = running().await;
        let report = report(&trigger, &text, true);
        assert_eq!(
            silent_wakeup_result(&report, &key(&trigger), &trigger),
            None,
            "{text}"
        );
        let (_stop_tx, stop) = watch::channel(false);
        finish_wakeup(
            admin.clone(),
            Arc::new(ReportControl(report)),
            trigger.clone(),
            key(&trigger),
            stop,
        )
        .await
        .unwrap();
        let finished = admin.snapshot().unwrap().remove(0);
        assert_eq!(finished.status, Status::Completed);
        assert_eq!(finished.delivery, Some(Delivery::Pending));
        assert_eq!(finished.result.as_deref(), Some(text.as_str()));
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn only_matching_successful_report_and_raw_terminal_assistant_can_select_silence() {
    let (kernel, _, trigger) = running().await;
    let original = report(&trigger, SILENCE, true);
    let mut variants = Vec::new();
    let mut value = original.clone();
    value.run.transcript = None;
    variants.push(value);
    for commit in [
        CommitState::NotStarted,
        CommitState::Pending,
        CommitState::Failed,
        CommitState::Unknown,
    ] {
        let mut value = original.clone();
        value.run.commit = commit;
        variants.push(value);
    }
    let mut value = original.clone();
    value.cancel_requested = true;
    variants.push(value);
    let mut value = original.clone();
    value.run.failure = Some(RunFailure::RunnerPanicked);
    variants.push(value);
    let mut value = original.clone();
    value.run.turn_id = None;
    variants.push(value);
    let mut value = original.clone();
    value.key.controller_epoch[0] += 1;
    variants.push(value);
    let mut value = original.clone();
    value.key.generation += 1;
    variants.push(value);
    let mut value = original.clone();
    value.key.session.user_id = "other".into();
    variants.push(value);
    let mut value = original.clone();
    value.key.task_id = "other".into();
    variants.push(value);
    let mut value = original.clone();
    value
        .run
        .transcript
        .as_mut()
        .unwrap()
        .last_mut()
        .unwrap()
        .text = Some("业务结果".into());
    variants.push(value);
    for role in [ChatRole::User, ChatRole::Tool, ChatRole::System] {
        let mut value = original.clone();
        value
            .run
            .transcript
            .as_mut()
            .unwrap()
            .last_mut()
            .unwrap()
            .role = role;
        variants.push(value);
    }
    let mut value = original.clone();
    value
        .run
        .transcript
        .as_mut()
        .unwrap()
        .last_mut()
        .unwrap()
        .tool_results = value.run.tool_results.clone();
    variants.push(value);
    for value in variants {
        assert_eq!(silent_wakeup_result(&value, &key(&trigger), &trigger), None);
    }
    let mut wrong_trigger = trigger.clone();
    wrong_trigger.owner.user_id = "other".into();
    assert_eq!(
        silent_wakeup_result(&original, &key(&trigger), &wrong_trigger),
        None
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn cancellation_wins_against_a_late_silent_completion() {
    let (kernel, admin, trigger) = running().await;
    admin.cancel(&trigger.owner, &trigger.id).unwrap();
    let before = admin.snapshot().unwrap();
    let report = report(&trigger, SILENCE, false);
    let (_stop_tx, stop) = watch::channel(false);
    finish_wakeup(
        admin.clone(),
        Arc::new(ReportControl(report)),
        trigger.clone(),
        key(&trigger),
        stop,
    )
    .await
    .unwrap();
    assert_eq!(admin.snapshot().unwrap(), before);
    kernel.stop_all().await.unwrap();
}
