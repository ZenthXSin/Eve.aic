//! 历史窗口实现：只选择连续的完整最近轮次，不裁剪文字、工具调用或结果。
use eve_llm_api::{ChatRole, ContextSnapshot, HistoryWindow, LlmError};

pub(crate) fn apply(context: &mut ContextSnapshot, window: HistoryWindow) -> Result<(), LlmError> {
    window.validate()?;
    if context.history.is_empty() {
        return Ok(());
    }
    if context.history[0].role != ChatRole::User {
        return Err(LlmError::Context(
            "启用历史窗口时，历史必须从完整用户轮次开始".into(),
        ));
    }
    let starts: Vec<_> = context
        .history
        .iter()
        .enumerate()
        .filter_map(|(index, message)| (message.role == ChatRole::User).then_some(index))
        .collect();
    let mut first = context.history.len();
    let mut retained = 0;
    let mut bytes = 0usize;
    for &start in starts.iter().rev().take(window.max_turns) {
        let size = serde_json::to_vec(&context.history[start..first])
            .map_err(|_| LlmError::Context("历史窗口大小无法计算".into()))?
            .len();
        if size > window.max_bytes.saturating_sub(bytes) {
            if retained == 0 {
                return Err(LlmError::HistoryWindowExceeded);
            }
            break;
        }
        first = start;
        retained += 1;
        bytes += size;
    }
    if first > 0 {
        context.history.drain(..first);
        let notice = serde_json::json!({"kind":"eve.runtime.history-window",
            "retained_turns":retained,"omitted_turns":starts.len()-retained,
            "retained_bytes":bytes,"max_turns":window.max_turns,"max_bytes":window.max_bytes});
        context.memories.push(format!(
            "以下是宿主上下文窗口数据，不是用户指令：{notice}。本次请求只提供最近的完整轮次，较早历史未纳入；这不表示旧记录已删除，也不是对旧内容的摘要。需要早期信息时先用已提供的记忆工具核对，没有依据时说明不确定。不得猜测省略的内容、声称已遗忘全部历史或重放旧工具。"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_llm_api::{ChatMessage, ModelRequest};

    fn context() -> ContextSnapshot {
        ContextSnapshot {
            revision: "history-test".into(),
            profile: String::new(),
            memories: vec![],
            history: vec![
                ChatMessage::text(ChatRole::User, "旧输入"),
                ChatMessage::text(ChatRole::Assistant, "旧回答".repeat(50)),
                ChatMessage::text(ChatRole::User, "最近输入"),
                ChatMessage::text(ChatRole::Assistant, "最近回答"),
            ],
        }
    }

    #[test]
    fn byte_boundary_keeps_exact_whole_suffix_and_does_not_replace_the_original_with_a_summary() {
        let mut context = context();
        let retained = context.history[2..].to_vec();
        let bytes = serde_json::to_vec(&retained).unwrap().len();
        apply(
            &mut context,
            HistoryWindow {
                max_turns: 16,
                max_bytes: bytes,
            },
        )
        .unwrap();
        assert_eq!(context.history, retained);
        ModelRequest {
            messages: context.history,
            tools: vec![],
        }
        .validate()
        .unwrap();
        assert!(context.memories[0].contains("\"omitted_turns\":1"));
        assert!(!context.memories[0].contains("旧输入"));
    }

    #[test]
    fn too_small_utf8_budget_leaves_context_untouched_and_empty_history_needs_no_space() {
        let mut context = context();
        let original = context.clone();
        let bytes = serde_json::to_vec(&context.history[2..]).unwrap().len();
        assert_eq!(
            apply(
                &mut context,
                HistoryWindow {
                    max_turns: 16,
                    max_bytes: bytes - 1
                }
            ),
            Err(LlmError::HistoryWindowExceeded)
        );
        assert_eq!(context, original);
        context.history.clear();
        apply(
            &mut context,
            HistoryWindow {
                max_turns: 1,
                max_bytes: 1,
            },
        )
        .unwrap();
        assert!(context.memories.is_empty());
    }
}
