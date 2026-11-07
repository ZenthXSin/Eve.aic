use super::*;
use eve_kernel::Kernel;
use eve_subject_plugin::SubjectPlugin;
use std::sync::Mutex;

fn scope() -> SubjectScope {
    SubjectScope {
        channel: "web".into(),
        conversation_id: "web:chat:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        principal_id: crate::web_chat::USER.into(),
    }
}
fn record(id: &str, scope: SubjectScope) -> ExplorationExperience {
    ExplorationExperience {
        session_id: id.into(),
        scope,
        started_at_ms: 10,
        ended_at_ms: 20,
        end_reason: ExplorationEndReason::Unknown,
        tools: vec![
            ExplorationToolExperience {
                call_id: "actual-read".into(),
                tool: "read_file".into(),
                arguments: crate::qq_exploration::excerpt(r#"{"path":"materials.txt"}"#),
                output: Some(crate::qq_exploration::excerpt(
                    &"文中仍有未解问题".repeat(400),
                )),
                outcome: ExplorationToolOutcome::Succeeded,
                error: None,
            },
            ExplorationToolExperience {
                call_id: "unknown-call".into(),
                tool: "echo".into(),
                arguments: crate::qq_exploration::excerpt("{}"),
                output: None,
                outcome: ExplorationToolOutcome::Unknown,
                error: None,
            },
        ],
        total_tool_calls: 2,
        tool_offset: 0,
        tools_truncated: false,
        subjective_notes: Some(crate::qq_exploration::excerpt("暂时有一些疑问，不知道答案")),
    }
}
async fn install(memory: Arc<dyn IdleMemoryStore>) -> (Kernel, ReadExploration) {
    let kernel = Kernel::new();
    let plugin = SubjectPlugin::new().unwrap().with_idle_memory(memory);
    let subject = Arc::new(plugin.controller());
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    (
        kernel,
        ReadExploration {
            subject,
            binding: NoteBinding::Web,
        },
    )
}
fn context() -> ToolExecutionContext {
    ToolExecutionContext::new()
        .with_scope(Some(ContextScope {
            session_id: scope().conversation_id,
            user_id: scope().principal_id,
        }))
        .with_delegated(true)
}
async fn execute(tool: &ReadExploration, arguments: Value) -> Result<Value, ToolExecutionError> {
    tool.execute(
        ToolCall {
            id: "history-read".into(),
            name: "read_exploration".into(),
            arguments,
        },
        context(),
    )
    .await
}

#[tokio::test]
async fn persisted_history_pages_read_old_ids_and_preserve_utf8_unknown_and_truncation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("explorations.sqlite3");
    {
        let memory = eve_subject_sqlite::SqliteIdleMemory::open(&path).unwrap();
        for i in 0..5 {
            memory
                .record_exploration(&record(&format!("ours-{i}"), scope()))
                .unwrap();
        }
        for (i, foreign) in [
            SubjectScope {
                channel: "qq".into(),
                ..scope()
            },
            SubjectScope {
                conversation_id: "web:chat:other".into(),
                ..scope()
            },
            SubjectScope {
                principal_id: "other-user".into(),
                ..scope()
            },
        ]
        .into_iter()
        .enumerate()
        {
            memory
                .record_exploration(&record(&format!("private-{i}"), foreign))
                .unwrap();
        }
    }
    // 新句柄读取磁盘旧记录，工具不持有任务管理器，也不依赖活动任务保留。
    let memory = Arc::new(eve_subject_sqlite::SqliteIdleMemory::open(&path).unwrap());
    let (kernel, tool) = install(memory.clone()).await;
    let first = execute(&tool, json!({"action":"list","limit":2}))
        .await
        .unwrap();
    assert_eq!(first["total"], 5);
    assert_eq!(first["items"][0]["session_id"], "ours-4");
    assert_eq!(first["items"][1]["session_id"], "ours-3");
    assert!(!first.to_string().contains("private-"));
    let mut late = record("late-record", scope());
    late.ended_at_ms = 11;
    memory.record_exploration(&late).unwrap();
    let second = execute(
        &tool,
        json!({"action":"list","limit":2,"before":first["next_before"]}),
    )
    .await
    .unwrap();
    assert_eq!(second["total"], 6);
    assert_eq!(second["items"][0]["session_id"], "ours-2");
    assert_eq!(second["items"][1]["session_id"], "ours-1");
    let third = execute(
        &tool,
        json!({"action":"list","limit":2,"before":second["next_before"]}),
    )
    .await
    .unwrap();
    assert_eq!(third["items"].as_array().unwrap().len(), 1);
    assert_eq!(third["items"][0]["session_id"], "ours-0");
    assert!(third["next_before"].is_null());

    let expected = memory.exploration(&scope(), "ours-0").unwrap().unwrap();
    let encoded = serde_json::to_string(&expected).unwrap();
    let hash = crate::qq_exploration::excerpt(&encoded).sha256;
    let mut full = String::new();
    loop {
        let page = execute(
            &tool,
            json!({"action":"read","id":"ours-0","offset":full.len(),"limit":127}),
        )
        .await
        .unwrap();
        assert_eq!(page["scope"], json!(scope()));
        assert_eq!(page["id"], "ours-0");
        assert_eq!(page["sha256"], hash);
        assert_eq!(page["total_bytes"], encoded.len());
        assert_eq!(page["offset"], full.len());
        let text = page["text"].as_str().unwrap();
        assert!(text.len() <= 127);
        full.push_str(text);
        if page["next_offset"].is_null() {
            break;
        }
        assert!(!text.is_empty());
        assert_eq!(page["next_offset"], full.len());
    }
    assert_eq!(full, encoded);
    let actual: ExplorationExperience = serde_json::from_str(&full).unwrap();
    assert_eq!(actual, expected);
    assert!(actual.tools[0].output.as_ref().unwrap().truncated);
    assert_eq!(actual.tools[1].outcome, ExplorationToolOutcome::Unknown);
    assert!(actual.tools[1].output.is_none());
    assert_eq!(
        execute(
            &tool,
            json!({"action":"read","id":"ours-0","offset":encoded.len()})
        )
        .await
        .unwrap()["text"],
        ""
    );
    for offset in [encoded.find('文').unwrap() + 1, encoded.len() + 1] {
        assert!(
            execute(
                &tool,
                json!({"action":"read","id":"ours-0","offset":offset})
            )
            .await
            .is_err()
        );
    }
    for id in ["private-0", "private-1", "private-2", "missing"] {
        let error = execute(&tool, json!({"action":"read","id":id}))
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("文中仍有未解问题"));
    }
    for arguments in [
        json!({"action":"list","scope":scope()}),
        json!({"action":"list","before":0}),
        json!({"action":"list","limit":17}),
        json!({"action":"read","id":"ours-0","limit":3}),
        json!({"action":"read","id":"ours-0","limit":8193}),
        json!({"action":"read","id":"ours-0","user_id":"other-user"}),
    ] {
        assert!(tool.validate_arguments(&arguments).is_err());
        assert!(execute(&tool, arguments).await.is_err());
    }
    for context in [
        ToolExecutionContext::new(),
        ToolExecutionContext::new().with_scope(Some(ContextScope {
            session_id: scope().conversation_id,
            user_id: "other-user".into(),
        })),
    ] {
        assert!(
            tool.execute(
                ToolCall {
                    id: "untrusted".into(),
                    name: "read_exploration".into(),
                    arguments: json!({"action":"list"})
                },
                context
            )
            .await
            .is_err()
        );
    }
    assert_eq!(
        memory.exploration(&scope(), "ours-0").unwrap(),
        Some(expected)
    );
    kernel.stop_all().await.unwrap();
}

struct Replaced(Mutex<ExplorationExperiencePage>);
impl IdleMemoryStore for Replaced {
    fn exploration(
        &self,
        _: &SubjectScope,
        _: &str,
    ) -> SubjectResult<Option<ExplorationExperience>> {
        Ok(self.0.lock().unwrap().items.first().cloned())
    }
    fn explorations(
        &self,
        _: &SubjectScope,
        _: Option<u64>,
        _: usize,
    ) -> SubjectResult<ExplorationExperiencePage> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn snapshot(&self, _: &SubjectScope) -> SubjectResult<IdleMind> {
        Err(SubjectError::Unavailable)
    }
    fn prepare_observation(&self, _: &mut IdleObservation) -> SubjectResult<()> {
        Err(SubjectError::Unavailable)
    }
    fn commit(
        &self,
        _: &IdleObservation,
        _: &IdleReflection,
        _: &str,
        _: u64,
    ) -> SubjectResult<IdleMind> {
        Err(SubjectError::Unavailable)
    }
    fn interests(
        &self,
        _: &SubjectScope,
        _: Option<&str>,
        _: usize,
    ) -> SubjectResult<SelfInterestPage> {
        Err(SubjectError::Unavailable)
    }
    fn history(
        &self,
        _: &SubjectScope,
        _: &str,
        _: u64,
        _: usize,
    ) -> SubjectResult<SelfInterestHistoryPage> {
        Err(SubjectError::Unavailable)
    }
    fn import_legacy(&self, _: &SubjectScope, _: &IdleMind) -> SubjectResult<()> {
        Err(SubjectError::Unavailable)
    }
}

#[tokio::test]
async fn substituted_history_backend_cannot_return_foreign_ids_scopes_or_invalid_pages() {
    let original = ExplorationExperiencePage {
        total: 2,
        items: vec![record("ours", scope())],
        next_before: Some(1),
    };
    let memory = Arc::new(Replaced(Mutex::new(original.clone())));
    let (kernel, tool) = install(memory.clone()).await;
    assert!(
        execute(&tool, json!({"action":"read","id":"ours"}))
            .await
            .is_ok()
    );
    assert!(
        execute(&tool, json!({"action":"read","id":"another-id"}))
            .await
            .is_err()
    );
    for foreign in [
        SubjectScope {
            channel: "qq".into(),
            ..scope()
        },
        SubjectScope {
            conversation_id: "web:chat:other".into(),
            ..scope()
        },
        SubjectScope {
            principal_id: "other-user".into(),
            ..scope()
        },
    ] {
        memory.0.lock().unwrap().items[0].scope = foreign;
        assert!(
            execute(&tool, json!({"action":"read","id":"ours"}))
                .await
                .is_err()
        );
        assert!(
            execute(&tool, json!({"action":"list","limit":1}))
                .await
                .is_err()
        );
    }
    let mut bad_pages = vec![];
    for cursor in [0, 2, 3] {
        bad_pages.push(ExplorationExperiencePage {
            next_before: Some(cursor),
            ..original.clone()
        });
    }
    bad_pages.push(ExplorationExperiencePage {
        total: 0,
        ..original.clone()
    });
    bad_pages.push(ExplorationExperiencePage {
        items: vec![record("ours", scope()), record("ours", scope())],
        ..original.clone()
    });
    bad_pages.push(ExplorationExperiencePage {
        items: vec![],
        ..original.clone()
    });
    for page in bad_pages {
        *memory.0.lock().unwrap() = page;
        assert!(
            execute(&tool, json!({"action":"list","before":2,"limit":1}))
                .await
                .is_err()
        );
    }
    *memory.0.lock().unwrap() = ExplorationExperiencePage {
        total: 2,
        items: vec![record("ours", scope()), record("ours", scope())],
        next_before: None,
    };
    assert!(
        execute(&tool, json!({"action":"list","limit":2}))
            .await
            .is_err()
    );
    kernel.stop_all().await.unwrap();
}
