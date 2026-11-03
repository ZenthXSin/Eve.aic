use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_llm_api::{ContextAssembler, ContextScope, TurnInput};
use eve_plugin_api::{PluginId, ServiceId, StateStore};
use eve_training_api::*;
use eve_training_plugin::{TrainingContext, TrainingPlugin};
use std::sync::Arc;

async fn open(state: Arc<MemoryStateStore>) -> (Kernel, Arc<dyn TrainingService>) {
    let backends = KernelServices {
        state,
        ..Default::default()
    };
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    kernel
        .register(Box::new(TrainingPlugin::new(true).unwrap()))
        .unwrap();
    kernel
        .start(&PluginId::new(TRAINING_PLUGIN_ID).unwrap())
        .await
        .unwrap();
    let training = registry
        .get(&ServiceId::new(TRAINING_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<TrainingServiceHandle>()
        .unwrap()
        .0
        .clone();
    (kernel, training)
}
fn scope(group: &str) -> ContextScope {
    ContextScope {
        session_id: group.into(),
        user_id: "person".into(),
    }
}
fn event(id: &str, role: ConversationRole, body: &str) -> ConversationMessage {
    ConversationMessage {
        id: id.into(),
        speaker: if role == ConversationRole::Human {
            "person"
        } else {
            "eve"
        }
        .into(),
        role,
        text: body.into(),
    }
}
fn seed(t: &dyn TrainingService, s: &ContextScope, prefix: &str) {
    for (i, role, text) in [
        (1, ConversationRole::Human, "今天跑了一整天，有点累"),
        (
            2,
            ConversationRole::Eve,
            "建议你制定一份休息计划，要我帮你安排吗？",
        ),
        (
            3,
            ConversationRole::Human,
            "不用帮我安排，就陪我吐槽两句，别每次都提建议",
        ),
        (4, ConversationRole::Eve, "这一天够折腾的，先歇会儿"),
    ] {
        t.observe_conversation(s, event(&format!("{prefix}{i}"), role, text))
            .unwrap();
    }
}
fn draft(
    b: &ConversationBatch,
    assessment: ConversationAssessment,
    id: Option<String>,
) -> ConversationLessonDraft {
    let rows = if assessment == ConversationAssessment::Observation {
        [0, 2]
    } else {
        [1, 2]
    };
    ConversationLessonDraft {
        lesson_id: id,
        situation: "对方只是分享疲惫而未求助时".into(),
        response: "先回应眼下经历，陪对方聊两句".into(),
        avoid: "对方明确求助时仍提供具体帮助".into(),
        assessment,
        evidence: rows
            .into_iter()
            .map(|i| ConversationEvidence {
                message_id: b.messages[i].id.clone(),
                quote: b.messages[i].text.clone(),
            })
            .collect(),
    }
}

#[tokio::test]
async fn correction_changes_later_context_survives_restart_and_stays_in_scope() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, t) = open(state.clone()).await;
    let s = scope("one");
    seed(t.as_ref(), &s, "a");
    let b = t.take_conversation_batch(1).unwrap().unwrap();
    assert_eq!(
        t.finish_conversation_batch(
            &b,
            vec![draft(&b, ConversationAssessment::Correction, None)],
            2
        )
        .unwrap(),
        1
    );
    let context = TrainingContext(t.clone())
        .assemble_scoped(
            TurnInput {
                text: "又忙了一天".into(),
            },
            Some(s.clone()),
        )
        .await
        .unwrap();
    assert!(
        context
            .memories
            .iter()
            .any(|m| m.contains("先回应眼下经历"))
    );
    assert!(
        !context
            .memories
            .iter()
            .any(|m| m.contains("建议你制定一份")),
        "上下文不复述采样私人对话"
    );
    assert!(t.conversation_lessons(&scope("two")).unwrap().is_empty());
    kernel.stop_all().await.unwrap();
    let (kernel, t) = open(state).await;
    assert!(t.conversation_lessons(&s).unwrap()[0].active);
    assert!(
        t.take_conversation_batch(3).unwrap().is_none(),
        "已完成分析不重跑"
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn passive_observation_requires_independent_evidence_and_counterexample_disables() {
    let (kernel, t) = open(Arc::new(MemoryStateStore::default())).await;
    let s = scope("group");
    seed(t.as_ref(), &s, "a");
    let b = t.take_conversation_batch(1).unwrap().unwrap();
    t.finish_conversation_batch(
        &b,
        vec![draft(&b, ConversationAssessment::Observation, None)],
        2,
    )
    .unwrap();
    assert!(!t.conversation_lessons(&s).unwrap()[0].active);
    seed(t.as_ref(), &s, "b");
    let b2 = t.take_conversation_batch(3).unwrap().unwrap();
    let old = t.conversation_lessons(&s).unwrap()[0].id.clone();
    t.finish_conversation_batch(
        &b2,
        vec![draft(
            &b2,
            ConversationAssessment::Observation,
            Some(old.clone()),
        )],
        4,
    )
    .unwrap();
    assert_eq!(
        t.conversation_lessons(&s).unwrap()[0].observations,
        1,
        "重复滚动原话不能刷支持数"
    );
    seed(t.as_ref(), &s, "c");
    let b3 = t.take_conversation_batch(5).unwrap().unwrap();
    let mut d = draft(&b3, ConversationAssessment::Observation, Some(old.clone()));
    d.evidence = [8, 10]
        .map(|i| ConversationEvidence {
            message_id: b3.messages[i].id.clone(),
            quote: b3.messages[i].text.clone(),
        })
        .to_vec();
    t.finish_conversation_batch(&b3, vec![d], 6).unwrap();
    assert!(t.conversation_lessons(&s).unwrap()[0].active);
    seed(t.as_ref(), &s, "d");
    let b4 = t.take_conversation_batch(7).unwrap().unwrap();
    let mut d = draft(&b4, ConversationAssessment::Counterexample, Some(old));
    d.evidence = [13, 14]
        .map(|i| ConversationEvidence {
            message_id: b4.messages[i].id.clone(),
            quote: b4.messages[i].text.clone(),
        })
        .to_vec();
    t.finish_conversation_batch(&b4, vec![d], 8).unwrap();
    let lesson = &t.conversation_lessons(&s).unwrap()[0];
    assert!(!lesson.active);
    assert_eq!(lesson.counterexamples, 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn fabricated_evidence_rejects_whole_batch_and_pause_invalidates_pending_result() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, t) = open(state.clone()).await;
    let s = scope("one");
    seed(t.as_ref(), &s, "a");
    let b = t.take_conversation_batch(1).unwrap().unwrap();
    let mut invalid = draft(&b, ConversationAssessment::Correction, None);
    invalid.evidence[1].quote = "用户没说过".into();
    let before = state
        .get(
            &PluginId::new(TRAINING_PLUGIN_ID).unwrap(),
            "conversation.v1",
        )
        .unwrap();
    assert!(
        t.finish_conversation_batch(
            &b,
            vec![draft(&b, ConversationAssessment::Correction, None), invalid],
            2
        )
        .is_err()
    );
    assert_eq!(
        state
            .get(
                &PluginId::new(TRAINING_PLUGIN_ID).unwrap(),
                "conversation.v1"
            )
            .unwrap(),
        before
    );
    t.set_enabled(&s, false).unwrap();
    assert!(
        t.finish_conversation_batch(
            &b,
            vec![draft(&b, ConversationAssessment::Correction, None)],
            3
        )
        .is_err()
    );
    t.set_enabled(&s, true).unwrap();
    assert!(t.take_conversation_batch(100_000).unwrap().is_none());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn retries_and_reset_keep_budget_and_do_not_replay_old_samples_after_restart() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, t) = open(state.clone()).await;
    let s = scope("one");
    seed(t.as_ref(), &s, "a");
    let b = t.take_conversation_batch(1).unwrap().unwrap();
    assert_eq!(b.attempt, 1);
    t.fail_conversation_batch(&b, 2).unwrap();
    assert!(t.take_conversation_batch(3).unwrap().is_none());
    kernel.stop_all().await.unwrap();
    let (kernel, t) = open(state).await;
    let b2 = t.take_conversation_batch(60_002).unwrap().unwrap();
    assert_eq!(b.id, b2.id);
    assert_eq!(b2.attempt, 2);
    let b3 = t.take_conversation_batch(120_002).unwrap().unwrap();
    assert_eq!(b3.attempt, 3);
    assert!(t.take_conversation_batch(180_002).unwrap().is_none());
    seed(t.as_ref(), &s, "b");
    let b4 = t.take_conversation_batch(240_002).unwrap().unwrap();
    t.reset_expression(&s).unwrap();
    assert!(t.finish_conversation_batch(&b4, vec![], 240_003).is_err());
    seed(t.as_ref(), &s, "b");
    assert!(t.take_conversation_batch(300_000).unwrap().is_none());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn corrupt_conversation_state_is_preserved() {
    let state = Arc::new(MemoryStateStore::default());
    state
        .set(
            &PluginId::new(TRAINING_PLUGIN_ID).unwrap(),
            "conversation.v1".into(),
            b"{broken}".to_vec(),
        )
        .unwrap();
    let kernel = Kernel::with_services(KernelServices {
        state: state.clone(),
        ..Default::default()
    });
    kernel
        .register(Box::new(TrainingPlugin::new(true).unwrap()))
        .unwrap();
    assert!(
        kernel
            .start(&PluginId::new(TRAINING_PLUGIN_ID).unwrap())
            .await
            .is_err()
    );
    assert_eq!(
        state
            .get(
                &PluginId::new(TRAINING_PLUGIN_ID).unwrap(),
                "conversation.v1"
            )
            .unwrap()
            .unwrap(),
        b"{broken}"
    );
}
