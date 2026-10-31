//! 主动压缩仅修改请求副本；完整历史、工具回执和当前任务控制状态始终保留。
use eve_llm_api::{
    ChatMessage, ChatRole, ContextCompactionPolicy, ContextScope, ContextSnapshot, HistoryDigest,
    HistoryDigests, LlmError, MAX_HISTORY_DIGEST_BYTES, ToolDefinition, history_turns,
};

const HISTORY: &str = "[宿主上下文压缩摘要：历史]";
const CURRENT: &str = "[宿主上下文压缩摘要：本轮已执行工具]";

/// 无 Grok 官方本地分词器时的保守估算：ASCII 每四字节一 token，其他字符每字两 token。
/// 序列化整个消息与工具定义，包含协议开销；不是厂商计费 token 数。
pub(crate) fn estimate<T: serde::Serialize + ?Sized>(value: &T) -> Result<usize, LlmError> {
    let mut count = JsonCharacters::default();
    serde_json::to_writer(&mut count, value)
        .map_err(|_| LlmError::Context("上下文大小无法估算".into()))?;
    Ok(count
        .ascii
        .div_ceil(4)
        .saturating_add(count.non_ascii.saturating_mul(2)))
}

/// JSON 序列化器输出合法 UTF-8；每个非 ASCII 字符恰有一个首字节，不受 write 分片边界影响。
#[derive(Default)]
struct JsonCharacters {
    ascii: usize,
    non_ascii: usize,
}
impl std::io::Write for JsonCharacters {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let overflow = || std::io::Error::other("上下文字符计数溢出");
        let ascii = self
            .ascii
            .checked_add(bytes.iter().filter(|byte| byte.is_ascii()).count())
            .ok_or_else(overflow)?;
        let non_ascii = self
            .non_ascii
            .checked_add(bytes.iter().filter(|byte| **byte >= 0xc0).count())
            .ok_or_else(overflow)?;
        self.ascii = ascii;
        self.non_ascii = non_ascii;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(serde::Serialize)]
struct RequestShape<'a, M: ?Sized> {
    messages: &'a M,
    tools: &'a [ToolDefinition],
}
pub(crate) fn estimate_request(
    messages: &[ChatMessage],
    tools: &[ToolDefinition],
) -> Result<usize, LlmError> {
    estimate(&RequestShape { messages, tools })
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
        Self::new_with_cache(context, service, scope, true)
    }

    pub(crate) fn new_with_cache(
        context: &mut ContextSnapshot,
        service: &dyn HistoryDigests,
        scope: Option<&ContextScope>,
        allow_cache: bool,
    ) -> Self {
        let original = context.history.clone();
        let cached = allow_cache
            .then(|| {
                service
                    .cached(scope, &original)
                    .ok()
                    .flatten()
                    .filter(|d| valid(d, history_turns(&original).len()))
            })
            .flatten();
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

    /// 软目标容纳不了固定开销时提前使用既有主预算，避免每轮归纳几十个 token。
    pub(crate) fn select_policy(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        preferred: ContextCompactionPolicy,
        main: Option<ContextCompactionPolicy>,
    ) -> Result<ContextCompactionPolicy, LlmError> {
        let Some(main) = main.filter(|main| *main != preferred) else {
            return Ok(preferred);
        };
        if main.trigger_tokens <= preferred.trigger_tokens {
            return Ok(main);
        }
        let minimum = self.minimum_size(messages, tools)?;
        Ok(if minimum >= preferred.target_tokens {
            main
        } else {
            preferred
        })
    }

    fn minimum_size(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
    ) -> Result<usize, LlmError> {
        let end = 2 + self.remaining;
        let mut fixed = Vec::new();
        let mut index = 0;
        while index < messages.len() {
            if (2..end).contains(&index) {
                index += 1;
            } else if index >= end
                && !messages[index].tool_calls.is_empty()
                && messages
                    .get(index + 1)
                    .is_some_and(|m| m.role == ChatRole::Tool)
            {
                index += 2;
            } else {
                fixed.push(&messages[index]);
                index += 1;
            }
        }
        estimate(&RequestShape {
            messages: &fixed,
            tools,
        })
    }

    pub(crate) fn adaptive_policy(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        limit: usize,
    ) -> Result<ContextCompactionPolicy, LlmError> {
        let floor = self.minimum_size(messages, tools)?;
        let reserve = (limit.saturating_sub(floor) / 4).min(8192);
        let trigger_tokens = limit - reserve;
        let target_tokens = if floor >= trigger_tokens {
            trigger_tokens - 1
        } else {
            floor + (trigger_tokens - floor) / 2
        };
        Ok(ContextCompactionPolicy {
            trigger_tokens,
            target_tokens,
        })
    }

    pub(crate) fn has_work(&self, messages: &[ChatMessage]) -> bool {
        self.remaining > 0
            || messages
                .get(2 + self.remaining..)
                .unwrap_or_default()
                .windows(2)
                .any(|pair| !pair[0].tool_calls.is_empty() && pair[1].role == ChatRole::Tool)
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
        let before = estimate_request(messages, tools)?;
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
        let now = estimate_request(messages, tools)?;
        if now > policy.target_tokens {
            messages
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
            // 轮内引导可能位于已完成工具之后；保留所有 User 原文，扫描整段本轮工具。
            let mut index = 2 + self.remaining;
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
        let after = estimate_request(messages, tools)?;
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
    use eve_llm_api::{LlmFuture, ModelRequest};
    use std::sync::atomic::{AtomicUsize, Ordering};
    fn original_estimate<T: serde::Serialize + ?Sized>(value: &T) -> usize {
        let json = serde_json::to_string(value).unwrap();
        json.bytes()
            .filter(u8::is_ascii)
            .count()
            .div_ceil(4)
            .saturating_add(
                json.chars()
                    .filter(|ch| !ch.is_ascii())
                    .count()
                    .saturating_mul(2),
            )
    }
    #[test]
    fn streamed_estimate_matches_original_json_for_unicode_escaping_and_large_values() {
        for value in [
            serde_json::json!(null),
            serde_json::json!([
                true,
                false,
                0,
                -2,
                1.25,
                "",
                "ascii",
                "猫猫",
                "😀𠀀",
                "引号\"\\\n\t\u{0}"
            ]),
            serde_json::json!({"系统":[{"tool":"echo","arguments":{"text":"猫😀\"\\\n".repeat(100_000)}}],"value":1e100}),
        ] {
            assert_eq!(estimate(&value).unwrap(), original_estimate(&value));
        }
        struct Reject;
        impl serde::Serialize for Reject {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("private-serializer-detail"))
            }
        }
        assert_eq!(
            estimate(&Reject).unwrap_err(),
            LlmError::Context("上下文大小无法估算".into())
        );
    }
    #[test]
    fn borrowed_request_shape_matches_owned_request_wire_and_estimate() {
        let messages = vec![
            ChatMessage::text(ChatRole::System, "规则\n\""),
            ChatMessage::text(ChatRole::User, "猫😀".repeat(100_000)),
        ];
        let tools = vec![ToolDefinition {
            name: "echo".into(),
            description: "说明".into(),
            argument_schema: serde_json::json!({"type":"object","properties":{"text":{"type":"string"}}}),
            required_permissions: vec![],
            concurrency: None,
        }];
        let owned = ModelRequest {
            messages: messages.clone(),
            tools: tools.clone(),
        };
        assert_eq!(
            serde_json::to_vec(&RequestShape {
                messages: &messages,
                tools: &tools
            })
            .unwrap(),
            serde_json::to_vec(&owned).unwrap()
        );
        assert_eq!(
            estimate_request(&messages, &tools).unwrap(),
            original_estimate(&owned)
        );
        let references: Vec<_> = messages.iter().collect();
        assert_eq!(
            estimate(&RequestShape {
                messages: &references,
                tools: &tools
            })
            .unwrap(),
            original_estimate(&owned)
        );
    }
    #[test]
    fn utf8_count_survives_write_boundaries_and_cannot_wrap() {
        use std::io::Write;
        let text = "a猫😀\n字";
        for size in 1..=7 {
            let mut count = JsonCharacters::default();
            for bytes in text.as_bytes().chunks(size) {
                count.write_all(bytes).unwrap();
            }
            assert_eq!(count.ascii, text.bytes().filter(u8::is_ascii).count());
            assert_eq!(
                count.non_ascii,
                text.chars().filter(|ch| !ch.is_ascii()).count()
            );
        }
        let mut count = JsonCharacters {
            ascii: usize::MAX,
            non_ascii: 0,
        };
        assert!(count.write_all(b"x").is_err());
        let mut count = JsonCharacters {
            ascii: 0,
            non_ascii: usize::MAX,
        };
        assert!(count.write_all("猫".as_bytes()).is_err());
    }
    struct Digests(AtomicUsize);

    #[tokio::test]
    async fn completed_tools_before_a_steering_message_can_be_compacted_without_removing_users() {
        use eve_llm_api::{ToolCall, ToolOutput, ToolResult};
        let service = Digests(AtomicUsize::new(0));
        let mut context = ContextSnapshot {
            revision: "test".into(),
            profile: String::new(),
            memories: vec![],
            history: vec![],
            pinned: vec![],
        };
        let mut state = State::new(&mut context, &service, None);
        let mut messages = vec![
            ChatMessage::text(ChatRole::System, "policy"),
            ChatMessage::text(ChatRole::System, "facts"),
            ChatMessage::text(ChatRole::User, "原任务"),
            ChatMessage::assistant_tool_calls(vec![ToolCall {
                id: "once".into(),
                name: "read".into(),
                arguments: serde_json::json!({}),
            }])
            .unwrap(),
            ChatMessage::tool_results(vec![ToolResult {
                call_id: "once".into(),
                output: ToolOutput::Success(serde_json::json!("中".repeat(100000))),
            }])
            .unwrap(),
            ChatMessage::text(ChatRole::User, "补充：保留已做结果"),
        ];
        let original = messages.clone();
        let policy = state.adaptive_policy(&messages, &[], 200000).unwrap();
        assert!(state.has_work(&messages));
        state
            .compact(&mut messages, &[], policy, &service, None, Some(200000))
            .await
            .unwrap();
        assert_eq!(service.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            messages
                .iter()
                .filter(|m| m.role == ChatRole::User)
                .map(|m| m.text.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["原任务", "补充：保留已做结果"]
        );
        assert!(
            messages
                .iter()
                .all(|m| m.tool_calls.is_empty() && m.tool_results.is_empty())
        );
        assert_eq!(original[3].tool_calls[0].id, "once");
        ModelRequest {
            messages,
            tools: vec![],
        }
        .validate()
        .unwrap();
    }
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
                pinned: vec![],
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
            pinned: vec![],
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
