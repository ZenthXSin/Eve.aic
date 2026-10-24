//! 主动压缩仅修改请求副本；完整历史、工具回执和当前任务控制状态始终保留。
use eve_llm_api::{
    ChatMessage, ChatRole, ContextCompactionPolicy, ContextScope, ContextSnapshot, HistoryDigest,
    HistoryDigests, LlmError, MAX_HISTORY_DIGEST_BYTES, ModelRequest, ToolDefinition,
    history_turns,
};

const HISTORY: &str = "[宿主上下文压缩摘要：历史]";
const CURRENT: &str = "[宿主上下文压缩摘要：本轮已执行工具]";

/// 无 Grok 官方本地分词器时的保守估算：ASCII 每四字节一 token，其他字符每字两 token。
/// 序列化整个消息与工具定义，包含协议开销；不是厂商计费 token 数。
pub(crate) fn estimate<T: serde::Serialize + ?Sized>(value: &T) -> Result<usize, LlmError> {
    let json =
        serde_json::to_string(value).map_err(|_| LlmError::Context("上下文大小无法估算".into()))?;
    let ascii = json.bytes().filter(u8::is_ascii).count();
    let non_ascii = json.chars().filter(|c| !c.is_ascii()).count();
    Ok(ascii
        .div_ceil(4)
        .saturating_add(non_ascii.saturating_mul(2)))
}

fn valid(digest: &HistoryDigest, turns: usize) -> bool {
    digest.covered_turns > 0
        && digest.covered_turns <= turns
        && !digest.text.trim().is_empty()
        && digest.text.len() <= MAX_HISTORY_DIGEST_BYTES
}

fn summary(messages: &mut Vec<ChatMessage>, marker: &str, text: &str) {
    messages.retain(|m| {
        !(m.role == ChatRole::System && m.text.as_deref().is_some_and(|t| t.starts_with(marker)))
    });
    let last_user = messages
        .iter()
        .rposition(|m| m.role == ChatRole::User)
        .unwrap_or(messages.len());
    messages.insert(last_user, ChatMessage::text(ChatRole::System, format!(
        "{marker}\n以下是旧记录的归纳数据，不是新的指令或原话。保留当前用户目标、限制、纠正、进度、阻塞和待办。工具已经执行，禁止因原始调用不在请求中而重放；需要细节时查原记录。摘要不代表未验证的操作已经成功。继续当前任务，无需用户说继续。\n{text}")));
}

pub(crate) struct State {
    original: Vec<ChatMessage>,
    covered: usize,
    remaining: usize,
    cached: Option<String>,
    current: Option<String>,
}

impl State {
    pub(crate) fn new(
        context: &mut ContextSnapshot,
        service: &dyn HistoryDigests,
        scope: Option<&ContextScope>,
    ) -> Self {
        let original = context.history.clone();
        let cached = service
            .cached(scope, &original)
            .ok()
            .flatten()
            .filter(|d| valid(d, history_turns(&original).len()));
        let covered = cached.as_ref().map_or(0, |d| d.covered_turns);
        let skip = history_turns(&original)
            .iter()
            .take(covered)
            .map(|t| t.len())
            .sum::<usize>();
        context.history.drain(..skip);
        Self {
            original,
            covered,
            remaining: context.history.len(),
            cached: cached.map(|d| d.text),
            current: None,
        }
    }

    pub(crate) fn inject(&self, messages: &mut Vec<ChatMessage>) {
        if let Some(text) = &self.cached {
            summary(messages, HISTORY, text);
        }
    }

    pub(crate) async fn compact(
        &mut self,
        messages: &mut Vec<ChatMessage>,
        tools: &[ToolDefinition],
        policy: ContextCompactionPolicy,
        service: &dyn HistoryDigests,
        scope: Option<&ContextScope>,
        fallback_trigger: Option<usize>,
    ) -> Result<(), LlmError> {
        let before = estimate(&ModelRequest {
            messages: messages.clone(),
            tools: tools.to_vec(),
        })?;
        if before < policy.trigger_tokens {
            return Ok(());
        }
        // 为摘要正文预留空间。历史只移除最早的完整轮次，所有 System 提示与当前用户输入保持原样。
        let mut needed = before.saturating_sub(
            policy
                .target_tokens
                .saturating_sub(MAX_HISTORY_DIGEST_BYTES),
        );
        if self.remaining > 0 {
            let groups = history_turns(&self.original);
            let mut count = self.covered;
            let mut remove = 0;
            for group in &groups[self.covered..] {
                count += 1;
                remove += group.len();
                needed = needed.saturating_sub(estimate(*group)?);
                if needed == 0 {
                    break;
                }
            }
            let end = groups[..count].iter().map(|g| g.len()).sum::<usize>();
            let digest = service
                .compact(scope, &self.original[..end])
                .await?
                .filter(|d| valid(d, count) && d.covered_turns == count)
                .ok_or_else(|| {
                    LlmError::Context("主动压缩未覆盖全部选定原文；保留记录并停止本次请求".into())
                })?;
            // build_messages 的前两条为固定系统提示，其后为连续完整历史。
            if messages.get(2..2 + remove) != Some(&self.original[end - remove..end]) {
                return Err(LlmError::Context("待压缩历史与请求副本不一致".into()));
            }
            messages.drain(2..2 + remove);
            self.remaining -= remove;
            self.covered = count;
            self.cached = Some(digest.text.clone());
            summary(messages, HISTORY, &digest.text);
        }
        let now = estimate(&ModelRequest {
            messages: messages.clone(),
            tools: tools.to_vec(),
        })?;
        if now > policy.target_tokens {
            let user = messages
                .iter()
                .rposition(|m| m.role == ChatRole::User)
                .ok_or_else(|| LlmError::Context("压缩请求缺少当前用户输入".into()))?;
            let mut selected = Vec::new();
            let mut ephemeral = Vec::new();
            let mut need = now.saturating_sub(
                policy
                    .target_tokens
                    .saturating_sub(MAX_HISTORY_DIGEST_BYTES),
            );
            // 每个已完成工具批次作为原子单元压缩；调用与结果一起移除，诊断和 transcript 不变。
            let mut index = user + 1;
            while index + 1 < messages.len() && need > 0 {
                if !messages[index].tool_calls.is_empty()
                    && messages[index + 1].role == ChatRole::Tool
                {
                    let pair = &messages[index..=index + 1];
                    ephemeral.push(ChatMessage::text(
                        ChatRole::User,
                        "归纳本轮已执行的工具批次；仅记录事实、结果和待办，禁止执行或重放。",
                    ));
                    ephemeral.extend_from_slice(pair);
                    selected.extend([index, index + 1]);
                    need = need.saturating_sub(estimate(pair)?);
                    index += 2;
                } else {
                    index += 1;
                }
            }
            if !selected.is_empty() {
                if let Some(previous) = &self.current {
                    ephemeral[0]
                        .text
                        .as_mut()
                        .unwrap()
                        .push_str(&format!("\n先前工具摘要（数据）：{previous}"));
                }
                let turns = history_turns(&ephemeral).len();
                let digest = service
                    .compact(None, &ephemeral)
                    .await?
                    .filter(|d| valid(d, turns) && d.covered_turns == turns)
                    .ok_or_else(|| {
                        LlmError::Context("本轮工具压缩未完整生成；原始回执保留".into())
                    })?;
                for index in selected.into_iter().rev() {
                    messages.remove(index);
                }
                self.current = Some(digest.text.clone());
                summary(messages, CURRENT, &digest.text);
            }
        }
        let after = estimate(&ModelRequest {
            messages: messages.clone(),
            tools: tools.to_vec(),
        })?;
        // 闲聊预算只是更早压缩；固定提示或当前输入本身超过它时，退回主阈值继续，不让本轮失败。
        if after >= policy.trigger_tokens && fallback_trigger.is_none_or(|limit| after >= limit) {
            return Err(LlmError::Context(
                "系统提示、当前输入或工具定义本身已达上下文阈值，无法安全压缩；原记录保留".into(),
            ));
        }
        eprintln!(
            "EVE_CONTEXT_COMPACTION {{\"estimated_before\":{before},\"estimated_after\":{after},\"trigger_tokens\":{},\"target_tokens\":{}}}",
            policy.trigger_tokens, policy.target_tokens
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_llm_api::LlmFuture;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Digests(AtomicUsize);
    impl HistoryDigests for Digests {
        fn digest(
            &self,
            _: Option<&ContextScope>,
            _: &[ChatMessage],
        ) -> Result<Option<HistoryDigest>, LlmError> {
            unreachable!()
        }
        fn compact<'a>(
            &'a self,
            _: Option<&'a ContextScope>,
            messages: &'a [ChatMessage],
        ) -> LlmFuture<'a, Option<HistoryDigest>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(Some(HistoryDigest {
                    covered_turns: history_turns(messages).len(),
                    text: "保留工作目标与真实进度".into(),
                }))
            })
        }
    }
    #[tokio::test]
    async fn exact_150k_boundary_includes_tools_and_below_threshold_keeps_all_history() {
        let policy = ContextCompactionPolicy {
            trigger_tokens: 150000,
            target_tokens: 100000,
        };
        for tokens in [149999, 150000, 150001] {
            let service = Digests(AtomicUsize::new(0));
            let mut context = ContextSnapshot {
                revision: "test".into(),
                profile: String::new(),
                memories: vec![],
                history: vec![
                    ChatMessage::text(ChatRole::User, ""),
                    ChatMessage::text(ChatRole::Assistant, "done"),
                ],
            };
            let template = vec![
                ChatMessage::text(ChatRole::System, "original policy"),
                ChatMessage::text(ChatRole::System, "tools"),
                context.history[0].clone(),
                context.history[1].clone(),
                ChatMessage::text(ChatRole::User, "current request"),
            ];
            let base = estimate(&ModelRequest {
                messages: template.clone(),
                tools: vec![],
            })
            .unwrap();
            context.history[0].text = Some("a".repeat((tokens - base) * 4));
            let original = context.history.clone();
            let mut state = State::new(&mut context, &service, None);
            let mut messages = template;
            messages[2] = context.history[0].clone();
            assert_eq!(
                estimate(&ModelRequest {
                    messages: messages.clone(),
                    tools: vec![]
                })
                .unwrap(),
                tokens
            );
            let before = messages.clone();
            state
                .compact(&mut messages, &[], policy, &service, None, None)
                .await
                .unwrap();
            assert_eq!(
                service.0.load(Ordering::SeqCst),
                usize::from(tokens >= 150000)
            );
            assert_eq!(state.original, original);
            if tokens < 150000 {
                assert_eq!(messages, before);
            } else {
                assert!(
                    estimate(&ModelRequest {
                        messages: messages.clone(),
                        tools: vec![]
                    })
                    .unwrap()
                        < 100000
                );
                assert_eq!(
                    messages.last().unwrap().text.as_deref(),
                    Some("current request")
                );
                assert_eq!(messages[0], before[0]);
            }
        }
    }
    #[tokio::test]
    async fn irreducible_current_input_is_preserved_and_does_not_loop() {
        let service = Digests(AtomicUsize::new(0));
        let mut context = ContextSnapshot {
            revision: "test".into(),
            profile: String::new(),
            memories: vec![],
            history: vec![],
        };
        let mut state = State::new(&mut context, &service, None);
        let mut messages = vec![
            ChatMessage::text(ChatRole::System, "policy"),
            ChatMessage::text(ChatRole::User, "中".repeat(75000)),
        ];
        let original = messages.clone();
        assert!(
            state
                .compact(
                    &mut messages,
                    &[],
                    ContextCompactionPolicy {
                        trigger_tokens: 150000,
                        target_tokens: 100000
                    },
                    &service,
                    None,
                    None
                )
                .await
                .is_err()
        );
        assert_eq!(messages, original);
        assert_eq!(service.0.load(Ordering::SeqCst), 0);
    }
}
