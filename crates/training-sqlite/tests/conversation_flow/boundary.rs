use super::*;
use eve_llm_api::{ContextAssembler, TurnInput};
use eve_training_plugin::TrainingContext;

pub(super) fn pair(batch: &ConversationBatch, eve: usize, human: usize) -> bool {
    let prompt: serde_json::Value =
        serde_json::from_str(&conversation_prompt_json(batch).unwrap()).unwrap();
    prompt["feedback_pairs"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!({
            "eve_message_id": format!("m-{eve}"),
            "later_human_message_id": format!("m-{human}")
        }))
}

pub(super) fn locate(batch: &ConversationBatch, index: usize) -> usize {
    batch
        .messages
        .iter()
        .position(|m| m.id == format!("m-{index}"))
        .unwrap()
}

async fn full_boundary(revisit: bool) {
    let dir = tempfile::tempdir().unwrap();
    let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, training) = open(state.clone(), sql.clone()).await;
    let limit = if revisit {
        MAX_CONVERSATION_REVISIT_MESSAGES
    } else {
        MAX_CONVERSATION_MESSAGES
    };
    for index in 0..2 * limit {
        let mut message = event(index);
        if index == limit - 1 {
            message.text = "记录全部查完了，文件已经核对完毕。".into();
        } else if index == limit {
            message.text = "刚才我还跟你聊着呢，你就一直不说话。需要查很久时先接一下我的话，有新发现也可以边聊边做。".into();
        }
        training.observe_conversation(&scope(), message).unwrap();
    }
    if revisit {
        training
            .request_conversation_sweep(
                "boundary-full",
                &scope(),
                &(1..=2 * limit as u64).collect::<Vec<_>>(),
            )
            .unwrap();
    }
    let take = |training: &dyn TrainingService, now| {
        if revisit {
            training.take_conversation_maintenance_batch(now, 4096)
        } else {
            training.take_conversation_batch(now)
        }
        .unwrap()
        .unwrap()
    };
    let first = take(training.as_ref(), 100_000);
    assert_eq!(first.messages.len(), limit);
    let original_eve = first.messages.last().unwrap().clone();
    assert_eq!(original_eve.role, ConversationRole::Eve);
    training
        .finish_conversation_batch(&first, vec![], 100_001)
        .unwrap();
    // 已确认的前批在重启后仍能接续，未完成的 claim 则沿用 Unknown 合同。
    kernel.stop_all().await.unwrap();
    let (kernel, training) = open(state, sql.clone()).await;
    let second = take(training.as_ref(), 300_000);
    assert!(
        pair(&second, limit - 1, limit),
        "满批边界应保留真实 Eve 与本次 Human 的反馈对"
    );
    assert_eq!(second.messages[locate(&second, limit - 1)], original_eve);
    assert_eq!(second.messages.len(), limit);
    assert!(conversation_prompt_json(&second).unwrap().len() <= MAX_CONVERSATION_PROMPT_BYTES);
    // 读取公开状态会重新校验已持久化的 claim，不能只验证 take 的内存返回值。
    assert!(training.conversation_lessons(&scope()).unwrap().is_empty());
    let update = draft(
        &second,
        [locate(&second, limit - 1), locate(&second, limit)],
        None,
        ConversationAssessment::Correction,
        "查询前先承接用户实际说的话，是否分条取决于内容",
    );
    assert_eq!(
        training
            .finish_conversation_batch(&second, vec![update], 300_001)
            .unwrap(),
        1
    );
    let lessons = training.conversation_lessons(&scope()).unwrap();
    assert_eq!(lessons.len(), 1);
    assert!(lessons[0].active);
    let context = TrainingContext(training.clone())
        .assemble_scoped(
            TurnInput {
                text: "帮我查一下记录".into(),
            },
            Some(scope()),
        )
        .await
        .unwrap();
    assert!(
        context
            .memories
            .iter()
            .any(|m| m.contains(&lessons[0].response))
    );
    assert!(
        !context
            .memories
            .iter()
            .any(|m| m.contains(&original_eve.text)),
        "只投影经验，不携带原交流正文"
    );
    let foreign = TrainingContext(training.clone())
        .assemble_scoped(
            TurnInput {
                text: "帮我查一下记录".into(),
            },
            Some(ContextScope {
                session_id: "other-chat".into(),
                user_id: "human".into(),
            }),
        )
        .await
        .unwrap();
    assert!(
        !foreign
            .memories
            .iter()
            .any(|m| m.contains(&lessons[0].response))
    );
    if revisit {
        let status = training.conversation_sweep_status("boundary-full").unwrap();
        assert_eq!(status.processed, (2 * limit - 1) as u64);
        assert_eq!(status.remaining, 1);
        assert_eq!(sweep_counts(&status).unknown, 0);
    } else {
        assert_eq!(checkpoint(sql.as_ref())["cursor"]["sequence"], 2 * limit);
    }
    let last = take(training.as_ref(), 600_000);
    assert!(
        last.messages
            .iter()
            .any(|m| m.id == format!("m-{}", 2 * limit - 1))
    );
    training
        .finish_conversation_batch(&last, vec![], 600_001)
        .unwrap();
    assert_eq!(training.conversation_lessons(&scope()).unwrap(), lessons);
    assert_eq!(
        checkpoint(sql.as_ref())["cursor"]["sequence"],
        2 * limit + 1
    );
    if revisit {
        let status = training.conversation_sweep_status("boundary-full").unwrap();
        assert_eq!(status.processed, (2 * limit) as u64);
        assert!(status.completed);
        assert_eq!(status.model_requests, 3);
        assert_eq!(sweep_counts(&status).failed, 0);
    }
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn normal_full_batch_keeps_the_previous_eve_for_current_correction() {
    full_boundary(false).await;
}

#[tokio::test]
async fn revisit_full_batch_keeps_the_previous_eve_for_current_correction() {
    full_boundary(true).await;
}

#[tokio::test]
async fn oversized_boundary_human_still_advances_and_preserves_every_byte() {
    for revisit in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let sql = Arc::new(SqliteExpressions::open(&dir.path().join("training.sqlite3")).unwrap());
        let (kernel, training) = open(Arc::new(MemoryStateStore::default()), sql.clone()).await;
        observe(training.as_ref(), 0, 16);
        if revisit {
            training
                .request_conversation_sweep("large-prefix", &scope(), &(1..=16).collect::<Vec<_>>())
                .unwrap();
        }
        let first = if revisit {
            training.take_conversation_maintenance_batch(100_000, 4096)
        } else {
            training.take_conversation_batch(100_000)
        }
        .unwrap()
        .unwrap();
        training
            .finish_conversation_batch(&first, vec![], 100_001)
            .unwrap();
        let mut huge = event(16);
        huge.text = format!("中{}", "\"".repeat(MAX_CONVERSATION_BODY_BYTES - 3));
        huge.metadata.as_mut().unwrap().context = Some(format!(
            "文{}",
            "\n".repeat(MAX_CONVERSATION_CONTEXT_BYTES - 3)
        ));
        training
            .observe_conversation(&scope(), huge.clone())
            .unwrap();
        if revisit {
            // 新明确请求有自己的冻结来源，前缀先成功消费，不能借旧 sweep 背景。
            training
                .request_conversation_sweep("large-full", &scope(), &(1..=17).collect::<Vec<_>>())
                .unwrap();
        }
        let mut text = String::new();
        let mut context = String::new();
        let mut saw_fragment = false;
        for round in 0..8 {
            let now = 300_000 + round * 100_000;
            let batch = if revisit {
                training.take_conversation_maintenance_batch(now, 4096)
            } else {
                training.take_conversation_batch(now)
            }
            .unwrap();
            let Some(batch) = batch else {
                break;
            };
            assert!(
                conversation_prompt_json(&batch).unwrap().len() <= MAX_CONVERSATION_PROMPT_BYTES
            );
            for message in batch.messages.iter().filter(|m| m.id == huge.id) {
                saw_fragment |= message.metadata.as_ref().unwrap().fragment.is_some();
                text.push_str(&message.text);
                context.push_str(
                    message
                        .metadata
                        .as_ref()
                        .unwrap()
                        .context
                        .as_deref()
                        .unwrap_or(""),
                );
            }
            training.conversation_lessons(&scope()).unwrap();
            training
                .finish_conversation_batch(&batch, vec![], now + 1)
                .unwrap();
        }
        assert!(saw_fragment);
        assert_eq!(text, huge.text);
        assert_eq!(context, huge.metadata.unwrap().context.unwrap());
        assert_eq!(checkpoint(sql.as_ref())["cursor"]["sequence"], 18);
        kernel.stop_all().await.unwrap();
    }
}
