use crate::LlmError;

/// 单次请求的历史窗口；字节数按各完整轮次的 JSON UTF-8 长度累计，不是模型 token 数。
/// 不限制当前用户输入、系统资料、工具定义或本轮新产生的工具结果，不修改持久历史。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryWindow {
    pub max_turns: usize,
    pub max_bytes: usize,
}

impl HistoryWindow {
    pub fn validate(self) -> Result<(), LlmError> {
        if self.max_turns == 0 || self.max_bytes == 0 {
            return Err(LlmError::Configuration(
                "历史窗口轮数和字节数必须大于零".into(),
            ));
        }
        Ok(())
    }
}

/// 每轮开始前读取一次；同一轮的多次模型调用使用同一快照。
pub trait HistoryWindowResolver: Send + Sync {
    fn resolve(&self) -> Result<HistoryWindow, LlmError>;
}
