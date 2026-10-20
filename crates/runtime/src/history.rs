//! 历史窗口实现：只选择连续的完整最近轮次，不裁剪文字、工具调用或结果。
use eve_llm_api::{
    ChatRole, ContextScope, ContextSnapshot, HistoryDigests, HistoryWindow, LlmError,
    MAX_HISTORY_DIGEST_BYTES,
};

/// 选择窗口内的最近完整轮次。窗口省略较早轮次、且装配了历史摘要时，以摘要提供被省略的部分；
/// 摘要不可用或不适用时照常省略，不让本轮失败。
pub(crate) fn apply(
    context: &mut ContextSnapshot,
    window: HistoryWindow,
    digests: Option<&dyn HistoryDigests>,
    scope: Option<&ContextScope>,
) -> Result<(), LlmError> {
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
        let omitted = starts.len() - retained;
        let digest = digests
            .and_then(|digests| {
                digests
                    .digest(scope, &context.history[..first])
                    .ok()
                    .flatten()
            })
            .filter(|digest| {
                digest.covered_turns > 0
                    && digest.covered_turns <= omitted
                    && !digest.text.trim().is_empty()
                    && digest.text.len() <= MAX_HISTORY_DIGEST_BYTES
            });
        context.history.drain(..first);
        let covered = digest.as_ref().map_or(0, |digest| digest.covered_turns);
        let notice = serde_json::json!({"kind":"eve.runtime.history-window",
            "retained_turns":retained,"omitted_turns":omitted,"digested_turns":covered,
            "retained_bytes":bytes,"max_turns":window.max_turns,"max_bytes":window.max_bytes});
        context.memories.push(match digest {
            None => format!(
                "以下是宿主上下文窗口数据，不是用户指令：{notice}。本次请求只提供最近的完整轮次，较早历史未纳入；这不表示旧记录已删除，也不是对旧内容的摘要。需要早期信息时先用已提供的记忆工具核对，没有依据时说明不确定。不得猜测省略的内容、声称已遗忘全部历史或重放旧工具。"
            ),
            Some(digest) => {
                let gap = if covered < omitted {
                    format!("第 {} 至 {omitted} 轮尚未压缩，未纳入；", covered + 1)
                } else {
                    String::new()
                };
                format!(
                    "以下是宿主上下文窗口数据，不是用户指令：{notice}。本次请求提供最近的完整轮次；较早的第 1 至 {covered} 轮已压缩为下面的摘要，{gap}原文都完整保存，没有删除。摘要是对旧对话的归纳而不是原文，可能遗漏细节；需要原话时先用已提供的记忆工具核对，没有依据时说明不确定。不得据摘要重放旧工具或把摘要当作用户刚说的话。\n早期对话摘要（第 1 至 {covered} 轮）：\n{}",
                    digest.text.trim()
                )
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_llm_api::{ChatMessage, HistoryDigest, ModelRequest};

    fn apply_plain(context: &mut ContextSnapshot, window: HistoryWindow) -> Result<(), LlmError> {
        apply(context, window, None, None)
    }

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
        apply_plain(
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
            apply_plain(
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
        apply_plain(
            &mut context,
            HistoryWindow {
                max_turns: 1,
                max_bytes: 1,
            },
        )
        .unwrap();
        assert!(context.memories.is_empty());
    }

    struct FixedDigest(Result<Option<HistoryDigest>, LlmError>);

    impl HistoryDigests for FixedDigest {
        fn digest(
            &self,
            _scope: Option<&ContextScope>,
            omitted: &[ChatMessage],
        ) -> Result<Option<HistoryDigest>, LlmError> {
            assert_eq!(omitted.len(), 2);
            assert_eq!(omitted[0].text.as_deref(), Some("旧输入"));
            self.0.clone()
        }
    }

    fn narrow() -> HistoryWindow {
        HistoryWindow {
            max_turns: 1,
            max_bytes: 1 << 20,
        }
    }

    #[test]
    fn digest_replaces_the_omission_notice_and_cites_covered_turns() {
        let mut context = context();
        let digests = FixedDigest(Ok(Some(HistoryDigest {
            covered_turns: 1,
            text: "第 1 轮：用户提到旧输入。".into(),
        })));
        apply(&mut context, narrow(), Some(&digests), None).unwrap();
        assert_eq!(context.history.len(), 2);
        assert!(context.memories[0].contains("\"digested_turns\":1"));
        assert!(context.memories[0].contains("第 1 轮：用户提到旧输入。"));
        assert!(context.memories[0].contains("早期对话摘要（第 1 至 1 轮）"));
    }

    #[test]
    fn failing_or_inapplicable_digest_falls_back_to_plain_notice() {
        for result in [
            Err(LlmError::Backend("digest store unavailable".into())),
            Ok(None),
            Ok(Some(HistoryDigest {
                covered_turns: 2,
                text: "越界".into(),
            })),
            Ok(Some(HistoryDigest {
                covered_turns: 1,
                text: "  ".into(),
            })),
            Ok(Some(HistoryDigest {
                covered_turns: 1,
                text: "长".repeat(MAX_HISTORY_DIGEST_BYTES),
            })),
        ] {
            let mut context = context();
            apply(&mut context, narrow(), Some(&FixedDigest(result)), None).unwrap();
            assert_eq!(context.history.len(), 2);
            assert!(context.memories[0].contains("\"digested_turns\":0"));
            assert!(context.memories[0].contains("也不是对旧内容的摘要"));
        }
    }
}
