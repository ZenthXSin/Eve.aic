use eve_history_digest::*;
use eve_llm_api::{
    ChatMessage, ChatRole, ContextScope, HistoryDigests, LlmError, LlmFuture, ToolCall, ToolResult,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

struct Fixed(Option<DigestOptions>);
impl DigestPolicy for Fixed {
    fn options(&self) -> Result<Option<DigestOptions>, LlmError> {
        Ok(self.0)
    }
}

type Seen = Arc<Mutex<Vec<DigestRequest>>>;

struct Scripted {
    seen: Seen,
    reply: fn(&DigestRequest) -> Result<Vec<DigestPoint>, LlmError>,
}
impl HistoryDigester for Scripted {
    fn version(&self) -> &str {
        "scripted"
    }
    fn digest(&self, request: DigestRequest) -> LlmFuture<'_, Vec<DigestPoint>> {
        let result = (self.reply)(&request);
        self.seen.lock().unwrap().push(request);
        Box::pin(async move { result })
    }
}

fn cover_all(request: &DigestRequest) -> Result<Vec<DigestPoint>, LlmError> {
    Ok(vec![DigestPoint {
        text: format!("覆盖到第 {} 轮", request.covered_turns()),
        turns: vec![1, request.covered_turns()],
    }])
}

const OPTIONS: DigestOptions = DigestOptions {
    min_uncovered_turns: 2,
    max_input_bytes: 1 << 20,
};

fn scope() -> ContextScope {
    ContextScope {
        session_id: "group:1".into(),
        user_id: "42".into(),
    }
}

fn history(turns: usize) -> Vec<ChatMessage> {
    (1..=turns)
        .flat_map(|n| {
            [
                ChatMessage::text(ChatRole::User, format!("问题 {n}")),
                ChatMessage::text(ChatRole::Assistant, format!("回答 {n}")),
            ]
        })
        .collect()
}

#[tokio::test]
async fn active_compaction_covers_all_batches_persists_reuses_and_never_enqueues_background_work() {
    let directory = tempfile::tempdir().unwrap();
    let service = service(
        directory.path(),
        Some(DigestOptions {
            min_uncovered_turns: 64,
            max_input_bytes: 4096,
        }),
    );
    let seen: Seen = Arc::default();
    service
        .install_compactor(Arc::new(Scripted {
            seen: seen.clone(),
            reply: |r| {
                Ok(vec![DigestPoint {
                    text: "已做过的工作与下一步".into(),
                    turns: vec![r.covered_turns()],
                }])
            },
        }))
        .unwrap();
    let mut original = history(3);
    for message in &mut original {
        message.text = message.text.as_ref().map(|t| t.repeat(5000));
    }
    assert!(service.cached(Some(&scope()), &original).unwrap().is_none());
    let result = service
        .compact(Some(&scope()), &original)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.covered_turns, 3);
    assert_eq!(seen.lock().unwrap().len(), 3);
    assert_eq!(service.pending_count(), 0);
    let restored = HistoryDigestService::new(
        DigestStore::open(directory.path()).unwrap(),
        Box::new(Fixed(Some(OPTIONS))),
    );
    assert_eq!(
        restored.cached(Some(&scope()), &original).unwrap(),
        Some(result)
    );
    let mut changed = original.clone();
    changed[0].text = Some("原文变化".into());
    assert!(restored.cached(Some(&scope()), &changed).unwrap().is_none());
    let calls = seen.lock().unwrap().len();
    service.compact(Some(&scope()), &original).await.unwrap();
    assert_eq!(seen.lock().unwrap().len(), calls);
    service.compact(None, &history(1)).await.unwrap();
    assert_eq!(
        service
            .store()
            .versions(&scope())
            .unwrap()
            .last()
            .unwrap()
            .covered_turns,
        3
    );
}

#[tokio::test]
async fn active_timeout_is_identified_as_compaction_and_oversized_turn_is_bounded() {
    let directory = tempfile::tempdir().unwrap();
    let service = service(
        directory.path(),
        Some(DigestOptions {
            min_uncovered_turns: 1,
            max_input_bytes: 4096,
        }),
    );
    let seen: Seen = Arc::default();
    service
        .install_compactor(Arc::new(Scripted {
            seen: seen.clone(),
            reply: |_| Err(LlmError::ProviderTimeout),
        }))
        .unwrap();
    let original = vec![
        ChatMessage::text(ChatRole::User, "很长的用户要求".repeat(10000)),
        ChatMessage::text(ChatRole::Assistant, "很长的旧回答".repeat(10000)),
    ];
    assert!(
        matches!(service.compact(Some(&scope()),&original).await,Err(LlmError::Context(reason)) if reason.starts_with("上下文压缩"))
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(serde_json::to_vec(&seen[0].turns[0]).unwrap().len() <= 4096);
    assert!(seen[0].turns[0].truncated);
}

#[tokio::test]
async fn active_compaction_preserves_corrupt_files_and_rejects_invalid_output() {
    let directory = tempfile::tempdir().unwrap();
    let service = service(directory.path(), Some(OPTIONS));
    service
        .install_compactor(Arc::new(Scripted {
            seen: Arc::default(),
            reply: |_| Ok(vec![]),
        }))
        .unwrap();
    assert!(service.compact(Some(&scope()), &history(2)).await.is_err());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    service
        .store()
        .put(
            &scope(),
            DigestVersion {
                covered_turns: 1,
                fingerprint: fingerprint(&history(2), 1).unwrap(),
                points: vec![DigestPoint {
                    text: "已保存".into(),
                    turns: vec![1],
                }],
                generator: "test".into(),
                created_at_ms: 1,
            },
        )
        .unwrap();
    let path = std::fs::read_dir(directory.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(&path, b"corrupt").unwrap();
    assert!(service.compact(Some(&scope()), &history(2)).await.is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"corrupt");
}

fn service(directory: &std::path::Path, options: Option<DigestOptions>) -> HistoryDigestService {
    HistoryDigestService::new(
        DigestStore::open(directory).unwrap(),
        Box::new(Fixed(options)),
    )
}

#[tokio::test]
async fn incremental_digest_covers_prefix_and_is_reused_only_while_original_matches() {
    let directory = tempfile::tempdir().unwrap();
    let service = service(directory.path(), Some(OPTIONS));
    let seen: Seen = Arc::default();
    let digester = Scripted {
        seen: seen.clone(),
        reply: cover_all,
    };
    // 少于阈值不登记，也没有摘要。
    assert_eq!(service.digest(Some(&scope()), &history(1)).unwrap(), None);
    assert_eq!(service.pending_count(), 0);
    assert_eq!(service.digest(None, &history(3)).unwrap(), None);
    assert_eq!(service.pending_count(), 0);

    assert_eq!(service.digest(Some(&scope()), &history(3)).unwrap(), None);
    let pending = service.take_pending().unwrap();
    let saved = generate(&service, &digester, &pending, OPTIONS, 10)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.covered_turns, 3);
    assert_eq!(seen.lock().unwrap()[0].turns.len(), 3);
    assert_eq!(seen.lock().unwrap()[0].turns[0].user, "问题 1");

    // 下一轮省略更多：返回已有摘要；新增未覆盖轮次未达阈值时不登记。
    let digest = service
        .digest(Some(&scope()), &history(4))
        .unwrap()
        .unwrap();
    assert_eq!(digest.covered_turns, 3);
    assert_eq!(digest.text, "- 覆盖到第 3 轮（第 1、3 轮）");
    assert_eq!(service.pending_count(), 0);

    // 达到阈值后增量请求只带上次摘要和新轮次。
    service.digest(Some(&scope()), &history(5)).unwrap();
    let pending = service.take_pending().unwrap();
    generate(&service, &digester, &pending, OPTIONS, 20)
        .await
        .unwrap();
    let request = seen.lock().unwrap()[1].clone();
    assert_eq!(request.previous_turns, 3);
    assert_eq!(request.previous[0].text, "覆盖到第 3 轮");
    assert_eq!(
        request.turns.iter().map(|t| t.number).collect::<Vec<_>>(),
        [4, 5]
    );
    let versions = service.store().versions(&scope()).unwrap();
    assert_eq!(
        versions.iter().map(|v| v.covered_turns).collect::<Vec<_>>(),
        [3, 5]
    );
    // 窗口变大、省略变少时退回覆盖较少的旧版本。
    assert_eq!(
        service
            .digest(Some(&scope()), &history(4))
            .unwrap()
            .unwrap()
            .covered_turns,
        3
    );

    // 原文不一致（别的历史）时不使用。
    let mut other = history(5);
    other[0] = ChatMessage::text(ChatRole::User, "不同的问题");
    assert_eq!(service.digest(Some(&scope()), &other).unwrap(), None);

    // 关闭后不提供也不登记，已保存的版本保留。
    let disabled = self::service(directory.path(), None);
    assert_eq!(disabled.digest(Some(&scope()), &history(9)).unwrap(), None);
    assert_eq!(disabled.pending_count(), 0);
    assert_eq!(disabled.store().versions(&scope()).unwrap().len(), 2);
}

#[tokio::test]
async fn invalid_or_failed_output_is_not_saved_and_input_respects_budget() {
    let directory = tempfile::tempdir().unwrap();
    let service = service(directory.path(), Some(OPTIONS));
    let seen: Seen = Arc::default();
    for reply in [
        (|_: &DigestRequest| {
            Ok(vec![DigestPoint {
                text: "引用了未覆盖轮次".into(),
                turns: vec![9],
            }])
        }) as fn(&DigestRequest) -> _,
        |_| Ok(vec![]),
        |_| Err(LlmError::ProviderTimeout),
    ] {
        service.digest(Some(&scope()), &history(3)).unwrap();
        let pending = service.take_pending().unwrap();
        let digester = Scripted {
            seen: seen.clone(),
            reply,
        };
        assert!(
            generate(&service, &digester, &pending, OPTIONS, 1)
                .await
                .is_err()
        );
        assert!(service.store().versions(&scope()).unwrap().is_empty());
    }

    // 预算只够一轮时每次至少一轮，超长原文截断标明但不改原文。
    let mut long = history(3);
    long[0] = ChatMessage::text(ChatRole::User, "长".repeat(5000));
    let tight = DigestOptions {
        min_uncovered_turns: 1,
        max_input_bytes: 10,
    };
    let pending = PendingDigest {
        scope: scope(),
        omitted: long.clone(),
        base: None,
    };
    let request = plan(&pending, tight).unwrap();
    assert_eq!(request.turns.len(), 1);
    assert!(request.turns[0].truncated);
    assert_eq!(request.turns[0].user.chars().count(), MAX_TURN_TEXT_CHARS);
    assert_eq!(long[0].text.as_deref().unwrap().chars().count(), 5000);
}

#[test]
fn tool_calls_are_summarized_by_name_and_clipped_result() {
    let call = ToolCall {
        id: "c1".into(),
        name: "read_file".into(),
        arguments: json!({}),
    };
    let turn = digest_turn(
        7,
        &[
            ChatMessage::text(ChatRole::User, "读文件"),
            ChatMessage::assistant_tool_calls(vec![call]).unwrap(),
            ChatMessage::tool_results(vec![
                ToolResult::success("c1", json!({"text": "x".repeat(1000)})).unwrap(),
            ])
            .unwrap(),
            ChatMessage::text(ChatRole::Assistant, "读完了"),
        ],
    );
    assert_eq!(turn.number, 7);
    assert_eq!(turn.assistant, ["读完了"]);
    assert_eq!(turn.tools[0].name, "read_file");
    assert_eq!(
        turn.tools[0].result.as_ref().unwrap().chars().count(),
        MAX_TOOL_RESULT_CHARS
    );
    assert!(turn.truncated);
}

#[tokio::test]
async fn corrupt_file_is_kept_reported_and_never_overwritten() {
    let directory = tempfile::tempdir().unwrap();
    let service = service(directory.path(), Some(OPTIONS));
    let digester = Scripted {
        seen: Arc::default(),
        reply: cover_all,
    };
    service.digest(Some(&scope()), &history(3)).unwrap();
    let pending = service.take_pending().unwrap();
    generate(&service, &digester, &pending, OPTIONS, 1)
        .await
        .unwrap();
    let file = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "json"))
        .unwrap();
    std::fs::write(&file, b"{broken").unwrap();
    assert!(matches!(
        service.digest(Some(&scope()), &history(5)),
        Err(LlmError::Context(_))
    ));
    assert_eq!(
        service.store().versions(&scope()),
        Err(DigestError::Corrupt)
    );
    assert!(
        generate(&service, &digester, &pending, OPTIONS, 2)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"{broken");
}
