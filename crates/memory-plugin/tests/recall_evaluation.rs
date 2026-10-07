//! 固定、手工编写的公开小样本，只约束词法召回的可解释行为和失败边界。
//! 这里不测语义相似度，不把这些样例的通过率当作泛化召回、学习或 AGI 指标。

use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{ChatMessage, ChatRole};
use eve_memory_api::*;
use eve_memory_plugin::{LexicalMemoryRecall, MemoryController, MemoryPlugin};
use eve_plugin_api::ServiceId;
use eve_session_api::{
    SESSION_SERVICE_ID, SessionInput, SessionKey, SessionService, SessionServiceHandle,
};
use eve_session_plugin::SessionPlugin;
use std::{collections::BTreeSet, sync::Arc};

struct Fixture {
    kernel: Kernel,
    admin: MemoryController,
    sessions: Arc<dyn SessionService>,
    scope: MemoryScope,
}

impl Fixture {
    async fn open() -> Self {
        let backends = KernelServices::default();
        let registry = backends.registry.clone();
        let kernel = Kernel::with_services(backends);
        let plugin = MemoryPlugin::new().unwrap();
        let admin = plugin.controller();
        kernel.register(Box::new(plugin)).unwrap();
        kernel
            .register(Box::new(SessionPlugin::new().unwrap()))
            .unwrap();
        kernel.start_all().await.unwrap();
        let sessions = registry
            .get(&ServiceId::new(SESSION_SERVICE_ID).unwrap())
            .unwrap()
            .unwrap()
            .value
            .downcast::<SessionServiceHandle>()
            .unwrap()
            .0
            .clone();
        Self {
            kernel,
            admin,
            sessions,
            scope: MemoryScope {
                channel: "evaluation".into(),
                session_id: "public-example-session".into(),
                user_id: "public-example-user".into(),
            },
        }
    }

    fn snapshot(&self) -> MemorySnapshot {
        self.admin
            .reader(self.scope.clone())
            .unwrap()
            .snapshot()
            .unwrap()
    }

    fn change(&self, action: PreferenceAction, statement: &str) {
        let revision = self.snapshot().revision;
        self.admin
            .update_preference(
                &self.scope,
                revision,
                PreferenceChange {
                    operation_id: format!("operation-{revision}"),
                    at_ms: 100 + revision,
                    evidence: PreferenceEvidence::Statement(UserStatement {
                        evidence_id: format!("statement-{revision}"),
                        message_id: format!("message-statement-{revision}"),
                        text: statement.into(),
                        at_ms: 100 + revision,
                    }),
                    action,
                },
            )
            .unwrap();
    }

    fn confirm(&self, id: &str, text: &str) {
        self.change(
            PreferenceAction::Confirm {
                id: id.into(),
                text: text.into(),
            },
            text,
        );
    }

    fn completed(&self, id: &str, user_text: &str, assistant_text: &str) {
        let key = SessionKey::new(&self.scope.session_id, &self.scope.user_id).unwrap();
        let turn = self
            .sessions
            .begin(SessionInput {
                key: key.clone(),
                text: user_text.into(),
            })
            .unwrap();
        self.sessions
            .complete(
                &turn.lease,
                vec![
                    ChatMessage::text(ChatRole::User, user_text),
                    ChatMessage::text(ChatRole::Assistant, assistant_text),
                ],
            )
            .unwrap();
        let revision = self.snapshot().revision;
        self.admin
            .import_completed(
                &self.scope,
                revision,
                CompletedInteraction {
                    evidence_id: id.into(),
                    message_id: format!("message-{id}"),
                    at_ms: 100 + revision,
                    snapshot: self.sessions.snapshot(&key).unwrap().unwrap(),
                    turn_id: turn.lease.turn_id,
                },
            )
            .unwrap();
    }

    fn recall(&self, query: &str) -> MemoryRecallResponse {
        self.recall_in(self.scope.clone(), query).unwrap()
    }

    fn recall_in(&self, scope: MemoryScope, query: &str) -> MemoryResult<MemoryRecallResponse> {
        let request = MemoryRecallRequest {
            query: query.into(),
            limit: MAX_RECALL_RESULTS,
        };
        let response = LexicalMemoryRecall::new(Arc::new(self.admin.clone()))
            .reader(scope.clone())?
            .recall(&request)?;
        response.validate_for(&scope, &request)?;
        Ok(response)
    }

    fn seed_public_examples(&self) {
        for (id, text) in [
            ("english-cat", "Use cat care examples."),
            ("english-category", "Use category charts."),
            ("english-scatter", "Use scatter plots."),
            ("chinese-memory", "记忆检索"),
            ("chinese-overlap", "记账预算"),
            ("chinese-unrelated", "天气预报"),
        ] {
            self.confirm(id, text);
        }
        for (id, user_text, assistant_text) in [
            ("cat-turn", "How do I feed a cat?", "Offer balanced meals."),
            (
                "category-turn",
                "Explain category labels.",
                "Group labels by theme.",
            ),
            (
                "assistant-only-turn",
                "What did you suggest?",
                "Use Rust for this parser.",
            ),
            ("chinese-turn", "记忆持久化", "完成保存"),
        ] {
            self.completed(id, user_text, assistant_text);
        }
    }
}

fn source_label(hit: &MemoryRecallHit) -> String {
    match &hit.source {
        MemoryRecallSource::ConfirmedPreference { preference_id, .. } => {
            format!("preference:{preference_id}")
        }
        MemoryRecallSource::CompletedInteraction {
            evidence_id, field, ..
        } => {
            let field = match field {
                RecallField::User => "user",
                RecallField::Assistant => "assistant",
            };
            format!("{field}:{evidence_id}")
        }
    }
}

fn labels(response: &MemoryRecallResponse) -> BTreeSet<String> {
    response.hits.iter().map(source_label).collect()
}

fn expected(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).into()).collect()
}

#[tokio::test]
async fn fixed_english_examples_match_whole_words_without_stemming_or_synonyms() {
    let fixture = Fixture::open().await;
    fixture.seed_public_examples();
    let cases: &[(&str, &[&str])] = &[
        ("cat", &["preference:english-cat", "user:cat-turn"]),
        ("CAT", &["preference:english-cat", "user:cat-turn"]),
        (
            "category",
            &["preference:english-category", "user:category-turn"],
        ),
        ("scatter", &["preference:english-scatter"]),
        // cat 不能从 category/scatter 的子串命中；这里也不做词干或同义词推断。
        ("cats", &[]),
        ("caterpillar", &[]),
        ("feline", &[]),
        ("par", &[]),
    ];
    for (query, sources) in cases {
        assert_eq!(
            labels(&fixture.recall(query)),
            expected(sources),
            "fixed lexical case: {query}"
        );
    }
    fixture.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn chinese_character_overlap_is_weaker_and_does_not_establish_semantic_relevance() {
    let fixture = Fixture::open().await;
    fixture.seed_public_examples();
    let response = fixture.recall("记忆");
    assert_eq!(
        labels(&response),
        expected(&[
            "preference:chinese-memory",
            "preference:chinese-overlap",
            "user:chinese-turn",
        ])
    );
    let score = |label: &str| {
        response
            .hits
            .iter()
            .find(|hit| source_label(hit) == label)
            .unwrap()
            .score
    };
    // 记账和记忆仅共有「记」，这个弱命中是词法边界，不能据此断言话题相同。
    let weak = score("preference:chinese-overlap");
    assert!(weak > 0);
    assert!(score("preference:chinese-memory") > weak);
    assert!(score("user:chinese-turn") > weak);
    assert_eq!(
        labels(&fixture.recall("保存")),
        expected(&["assistant:chinese-turn"])
    );
    // 不进行简繁转换；这不是中文语义检索质量指标。
    assert!(fixture.recall("記憶").hits.is_empty());
    fixture.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn assistant_only_match_remains_a_saved_reply_and_never_becomes_a_preference() {
    let fixture = Fixture::open().await;
    fixture.seed_public_examples();
    let before = fixture.snapshot();
    let response = fixture.recall("Rust");
    assert_eq!(
        labels(&response),
        expected(&["assistant:assistant-only-turn"])
    );
    assert_eq!(response.hits[0].excerpt, "Use Rust for this parser.");
    assert!(matches!(
        &response.hits[0].source,
        MemoryRecallSource::CompletedInteraction {
            field: RecallField::Assistant,
            turn_id: 3,
            ..
        }
    ));
    assert_eq!(fixture.snapshot(), before);
    assert!(
        before
            .preferences
            .iter()
            .all(|preference| !preference.text.contains("Rust"))
    );
    fixture.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn public_dataset_queries_do_not_cross_any_scope_component() {
    let fixture = Fixture::open().await;
    fixture.seed_public_examples();
    assert!(!fixture.recall("cat").hits.is_empty());
    for other in [
        MemoryScope {
            channel: "other-channel".into(),
            ..fixture.scope.clone()
        },
        MemoryScope {
            session_id: "other-session".into(),
            ..fixture.scope.clone()
        },
        MemoryScope {
            user_id: "other-user".into(),
            ..fixture.scope.clone()
        },
    ] {
        let response = fixture.recall_in(other.clone(), "cat").unwrap();
        assert_eq!(response.scope, other);
        assert_eq!(response.revision, 0);
        assert!(response.hits.is_empty());
    }
    fixture.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn changed_and_revoked_preferences_do_not_return_through_their_statement_history() {
    let fixture = Fixture::open().await;
    fixture.confirm("style", "Use verbose storytelling.");
    assert_eq!(
        labels(&fixture.recall("verbose")),
        expected(&["preference:style"])
    );
    fixture.change(
        PreferenceAction::Correct {
            id: "style".into(),
            text: "Use concise summaries.".into(),
        },
        "Please replace verbose storytelling with concise summaries.",
    );
    assert!(fixture.recall("verbose").hits.is_empty());
    let corrected = fixture.recall("concise");
    assert_eq!(labels(&corrected), expected(&["preference:style"]));
    assert!(matches!(
        &corrected.hits[0].source,
        MemoryRecallSource::ConfirmedPreference {
            preference_revision: 2,
            ..
        }
    ));
    fixture.change(
        PreferenceAction::Revoke { id: "style".into() },
        "Revoke my concise summaries preference.",
    );
    assert!(fixture.recall("concise").hits.is_empty());
    assert!(fixture.recall("verbose").hits.is_empty());
    let after = fixture.snapshot();
    assert_eq!(after.evidence.len(), 3);
    assert_eq!(after.preferences[0].history.len(), 3);
    assert_eq!(after.preferences[0].status, PreferenceStatus::Revoked);
    fixture.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn repeated_queries_preserve_order_and_repeated_terms_do_not_inflate_scores() {
    let fixture = Fixture::open().await;
    fixture.seed_public_examples();
    fixture.confirm("second-cat-example", "A cat needs attention.");
    let first = fixture.recall("cat");
    for _ in 0..5 {
        assert_eq!(fixture.recall("cat"), first);
    }
    assert!(
        first
            .hits
            .windows(2)
            .all(|pair| pair[0].score >= pair[1].score)
    );
    let repeated = fixture.recall("cat cat cat");
    assert_eq!(labels(&repeated), labels(&first));
    for hit in &repeated.hits {
        let original = first
            .hits
            .iter()
            .find(|original| source_label(original) == source_label(hit))
            .unwrap();
        // 重复完整词不会增加词法覆盖；短语加分可以因短语不同而减少。
        assert!(hit.score <= original.score);
    }
    fixture.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn unicode_and_control_boundaries_are_explicit_without_hidden_normalization() {
    let fixture = Fixture::open().await;
    fixture.confirm("composed", "café");
    fixture.confirm("decomposed", "cafe\u{301}");
    fixture.confirm("office", "office");
    assert_eq!(
        labels(&fixture.recall("café")),
        expected(&["preference:composed"])
    );
    assert_eq!(
        labels(&fixture.recall("cafe\u{301}")),
        expected(&["preference:decomposed"])
    );
    // 零宽分隔符不拼接成完整 office；无词元查询不会变成全量读取。
    for query in ["of\u{200b}fice", "\u{200b}", "\u{200d}", "...?!"] {
        assert!(fixture.recall(query).hits.is_empty(), "query: {query:?}");
    }
    for query in ["office\0", "off\nfice", "office\t", "\u{1b}"] {
        assert_eq!(
            fixture.recall_in(fixture.scope.clone(), query),
            Err(MemoryError::InvalidInput)
        );
    }
    fixture.kernel.stop_all().await.unwrap();
}
