//! 已完成模型结果与可选统计尾流。尾流不进入模型重试范围，不执行交付回调。
use crate::{chat_stream::UsageTail, stream::Sse};
use eve_llm_api::{ModelResponse, ModelUsage};
use std::time::Duration;
use tokio::time::{Instant, timeout_at};

const TAIL_GRACE: Duration = Duration::from_secs(1);
const MAX_TAIL_BYTES: usize = 64 * 1024;

pub(crate) struct CompletedRequest {
    response: ModelResponse,
    usage: Option<ModelUsage>,
    tail: Option<ChatTail>,
}
impl CompletedRequest {
    pub(crate) fn new(response: ModelResponse, usage: Option<ModelUsage>) -> Self {
        Self {
            response,
            usage,
            tail: None,
        }
    }
    pub(crate) fn chat(response: ModelResponse, tail: ChatTail) -> Self {
        Self {
            response,
            usage: None,
            tail: Some(tail),
        }
    }
    pub(crate) async fn finish(self, deadline: Instant) -> (ModelResponse, Option<ModelUsage>) {
        let usage = match self.tail {
            Some(tail) => tail.drain(deadline).await,
            None => self.usage,
        };
        (self.response, usage)
    }
}

pub(crate) struct ChatTail {
    response: reqwest::Response,
    parser: Sse,
    state: UsageTail,
    remainder: Vec<u8>,
    remainder_overflow: bool,
    budget: usize,
    deadline: Instant,
}
impl ChatTail {
    pub(crate) fn new(
        response: reqwest::Response,
        parser: Sse,
        state: UsageTail,
        remainder: &[u8],
        remaining_budget: usize,
    ) -> Self {
        let budget = remaining_budget.min(MAX_TAIL_BYTES);
        Self {
            response,
            parser,
            state,
            remainder: remainder[..remainder.len().min(budget)].to_vec(),
            remainder_overflow: remainder.len() > budget,
            budget,
            deadline: Instant::now() + TAIL_GRACE,
        }
    }

    async fn drain(mut self, total_deadline: Instant) -> Option<ModelUsage> {
        self.deadline = self.deadline.min(total_deadline);
        let outcome = self.read().await;
        if outcome != "done" {
            // 仅固定类别；统计缺失保持 None，不能伪造缓存未命中或触发模型重试。
            eprintln!("EVE_CHAT_USAGE_TAIL {{\"outcome\":\"{outcome}\"}}");
        }
        self.state.usage
    }

    async fn read(&mut self) -> &'static str {
        let remainder = std::mem::take(&mut self.remainder);
        if let Some(outcome) = self.consume(&remainder) {
            return outcome;
        }
        if self.remainder_overflow {
            return "size_limit";
        }
        loop {
            if Instant::now() >= self.deadline {
                return "deadline";
            }
            if self.budget == 0 {
                return "size_limit";
            }
            let chunk = match timeout_at(self.deadline, self.response.chunk()).await {
                Err(_) => return "deadline",
                Ok(Err(_)) => return "transport",
                Ok(Ok(None)) => return "eof",
                Ok(Ok(Some(chunk))) => chunk,
            };
            if let Some(outcome) = self.consume(&chunk) {
                return outcome;
            }
        }
    }

    fn consume(&mut self, bytes: &[u8]) -> Option<&'static str> {
        for &byte in bytes {
            if Instant::now() >= self.deadline {
                return Some("deadline");
            }
            if self.budget == 0 {
                return Some("size_limit");
            }
            self.budget -= 1;
            match self.parser.byte(byte) {
                Err(_) => return Some("protocol"),
                Ok(None) => {}
                Ok(Some((name, data))) => match self.state.event(&name, &data) {
                    Ok(true) => return Some("done"),
                    Ok(false) => {}
                    Err(_) => return Some("protocol"),
                },
            }
        }
        None
    }
}
