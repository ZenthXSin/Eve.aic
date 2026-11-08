use eve_llm_api::{ContextAssembler, ContextScope, ContextSnapshot, TurnInput};
use eve_plugin_api::PluginResult;
use eve_training_api::{
    ConversationLesson, ExpressionSnapshot, LearnedPhrase, MAX_CONVERSATION_LESSONS, PhraseKind,
    TrainingService,
};
use eve_training_plugin::TrainingContext;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

struct ContextSource {
    scope: ContextScope,
    enabled: AtomicBool,
    lessons: Mutex<Vec<ConversationLesson>>,
}
impl ContextSource {
    fn new() -> Arc<Self> {
        let lessons = (0..MAX_CONVERSATION_LESSONS)
            .map(|index| {
                serde_json::from_value(serde_json::json!({
                    "id":format!("lesson-{index}"),
                    "situation":if index + 1 == MAX_CONVERSATION_LESSONS {
                        "已授权排查进行中，对方仍在补充".to_owned()
                    } else { format!("历史交流场景 {index}") },
                    "response":if index + 1 == MAX_CONVERSATION_LESSONS {
                        "可先自然接话再查证，有新发现时继续分条说".to_owned()
                    } else { format!("适用于历史场景 {index} 的回应") },
                    "avoid":"当前明确要求安静或需要完整交付时重新判断",
                    "observations":2,"corrections":if index < 6 { 50 } else { 0 },
                    "counterexamples":0,"active":true,"updated_at_ms":index + 1,
                    "evidence":[
                        {"message_id":"source-private-id","quote":"PRIVATE_QUOTE_STAYS_IN_ARCHIVE"},
                        {"message_id":"source-private-other","quote":"SECOND_PRIVATE_QUOTE_STAYS_IN_ARCHIVE"}
                    ],
                    "evaluated_message_ids":["source-private-id","source-private-other"]
                }))
                .unwrap()
            })
            .collect();
        Arc::new(Self {
            scope: ContextScope {
                session_id: "conversation-one".into(),
                user_id: "person-one".into(),
            },
            enabled: AtomicBool::new(true),
            lessons: Mutex::new(lessons),
        })
    }
}
impl TrainingService for ContextSource {
    fn enabled(&self, scope: &ContextScope) -> PluginResult<bool> {
        Ok(scope == &self.scope && self.enabled.load(Ordering::SeqCst))
    }
    fn set_enabled(&self, scope: &ContextScope, enabled: bool) -> PluginResult<()> {
        assert_eq!(scope, &self.scope);
        self.enabled.store(enabled, Ordering::SeqCst);
        Ok(())
    }
    fn conversation_lessons(&self, scope: &ContextScope) -> PluginResult<Vec<ConversationLesson>> {
        Ok(if scope == &self.scope {
            self.lessons.lock().unwrap().clone()
        } else {
            vec![]
        })
    }
    fn phrases(&self, scope: &ContextScope) -> PluginResult<Vec<LearnedPhrase>> {
        Ok(if scope == &self.scope {
            vec![LearnedPhrase {
                id: "phrase-one".into(),
                kind: PhraseKind::Expression,
                text: "赞同时".into(),
                meaning: "直接认同眼下的说法".into(),
                example: "确实".into(),
                uses: 4,
                updated_at_ms: 1,
            }]
        } else {
            vec![]
        })
    }
    fn expression_snapshot(
        &self,
        scope: &ContextScope,
    ) -> PluginResult<Option<ExpressionSnapshot>> {
        Ok((scope == &self.scope).then(|| ExpressionSnapshot {
            revision: "observed-32".into(),
            samples: 32,
            median_chars: 5,
            p75_chars: 20,
            short_percent: 90,
            single_paragraph_percent: 90,
            question_percent: 10,
            formal_percent: 0,
            casual_percent: 50,
        }))
    }
}

async fn assemble(source: &Arc<ContextSource>, text: &str) -> ContextSnapshot {
    TrainingContext(source.clone())
        .assemble_scoped(TurnInput { text: text.into() }, Some(source.scope.clone()))
        .await
        .unwrap()
}
fn lessons(context: &ContextSnapshot) -> Vec<serde_json::Value> {
    let memory = context
        .memories
        .iter()
        .find(|memory| memory.contains("\"kind\":\"eve.conversation-lessons-v1\""))
        .unwrap();
    let data: serde_json::Value =
        serde_json::from_str(&memory[memory.find('{').unwrap()..]).unwrap();
    data["lessons"].as_array().unwrap().clone()
}

#[tokio::test]
async fn all_active_scenes_reach_context_despite_six_frequent_old_corrections() {
    let source = ContextSource::new();
    let context = assemble(&source, "先核对新日志，查到原因再继续").await;
    let selected = lessons(&context);
    assert_eq!(selected.len(), MAX_CONVERSATION_LESSONS);
    assert_eq!(selected[0]["id"], "lesson-23");
    assert_eq!(selected[0]["corrections"], 0);
    for index in 0..MAX_CONVERSATION_LESSONS {
        assert!(
            selected
                .iter()
                .any(|item| item["id"] == format!("lesson-{index}"))
        );
    }
    let data = context.memories.join("\n");
    assert!(!data.contains("PRIVATE_QUOTE_STAYS_IN_ARCHIVE"));
    assert!(!data.contains("source-private-id"));
    assert!(
        selected
            .iter()
            .all(|item| item.as_object().unwrap().len() == 7)
    );
    assert!(context.revision.starts_with("eve-training-6:"));

    // 换话题不靠关键词从宿主选掉经验；本轮模型按场景及例外判断适用性。
    let other_topic = assemble(&source, "今天只是想随便聊聊，不做任务").await;
    assert_eq!(context.memories, other_topic.memories);
    assert_eq!(context.revision, other_topic.revision);
}

#[tokio::test]
async fn revised_conditions_and_disabled_lessons_refresh_view_without_mutating_prior_snapshot() {
    let source = ContextSource::new();
    let before = assemble(&source, "继续查").await;
    {
        let mut stored = source.lessons.lock().unwrap();
        stored[23].situation = "排查已经结束，用户要求完整证据".into();
        stored[23].response = "把结论和证据一起交付".into();
        stored[23].avoid = "不能再沿用处理中暂时回应的节奏".into();
        stored[23].updated_at_ms = 100;
        stored[0].active = false;
        stored[0].counterexamples = 1;
    }
    let after = assemble(&source, "继续查").await;
    assert_ne!(before.revision, after.revision);
    assert_eq!(lessons(&before).len(), 24);
    assert_eq!(lessons(&after).len(), 23);
    assert_eq!(
        lessons(&before)[0]["response"],
        "可先自然接话再查证，有新发现时继续分条说"
    );
    assert_eq!(lessons(&after)[0]["response"], "把结论和证据一起交付");
    assert!(!lessons(&after).iter().any(|item| item["id"] == "lesson-0"));

    let other_scope = TrainingContext(source.clone())
        .assemble_scoped(
            TurnInput {
                text: "继续查".into(),
            },
            Some(ContextScope {
                session_id: "conversation-two".into(),
                user_id: "person-two".into(),
            }),
        )
        .await
        .unwrap();
    assert!(other_scope.memories.is_empty());
    let no_scope = TrainingContext(source)
        .assemble(TurnInput {
            text: "继续查".into(),
        })
        .await
        .unwrap();
    assert!(no_scope.memories.is_empty());
}

#[tokio::test]
async fn pause_suppresses_statistics_phrases_and_lessons_without_deleting_them() {
    let source = ContextSource::new();
    let before = assemble(&source, "继续").await;
    assert!(!before.profile.is_empty());
    assert_eq!(before.memories.len(), 2);
    source.set_enabled(&source.scope, false).unwrap();
    let paused = assemble(&source, "继续").await;
    assert!(paused.profile.is_empty());
    assert!(paused.memories.is_empty());
    assert!(paused.pinned.join("\n").contains("已暂停"));
    assert_eq!(source.lessons.lock().unwrap().len(), 24);
    source.set_enabled(&source.scope, true).unwrap();
    let resumed = assemble(&source, "继续").await;
    assert_eq!(before.memories, resumed.memories);
    assert_eq!(before.revision, resumed.revision);
}
