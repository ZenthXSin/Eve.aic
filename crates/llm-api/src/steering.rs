//! 宿主在同一轮内提供的新用户消息；不改变工具、身份、步数或取消信号。
use crate::{ContextScope, LlmError};
use std::sync::Arc;

/// 同一次消费的消息与审核锚点必须原子取出；后到的消息不能提前成为本批授权。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SteeringBatch {
    pub messages: Vec<String>,
    pub review_anchor: Option<String>,
}

pub trait TurnSteering: Send + Sync {
    /// finish=true 时，若没有新消息则原子关闭入口，避免最终答复与新输入竞态。
    fn take(&self, finish: bool) -> Result<Vec<String>, LlmError>;
    fn take_batch(&self, finish: bool) -> Result<SteeringBatch, LlmError> {
        self.take(finish).map(|messages| SteeringBatch {
            messages,
            review_anchor: None,
        })
    }
}

pub trait TurnSteeringSource: Send + Sync {
    /// 每个会话轮次独立入口；句柄析构时必须关闭，禁止流入下一轮。
    fn open(&self, scope: &ContextScope, turn_id: u64) -> Result<Arc<dyn TurnSteering>, LlmError>;
}
