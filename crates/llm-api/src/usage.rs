//! 模型请求的输入用量与前缀复用观察：用于统计提供方的提示缓存命中率，
//! 以及宿主本地估算的可复用前缀。只记数字，不记请求或回复正文。

/// 提供方在响应里报告的用量；兼容服务不报告的字段为 None。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModelUsage {
    pub input_tokens: Option<u64>,
    /// 输入中命中提供方提示缓存的部分。
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

impl ModelUsage {
    pub fn is_empty(&self) -> bool {
        self.input_tokens.is_none()
            && self.cached_input_tokens.is_none()
            && self.output_tokens.is_none()
    }
}

/// 宿主按实际发出的请求估算的前缀复用：与同一用途最近的请求相比，
/// 从开头起完全相同的部分有多大。估算值不是提供方的计费 token。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PrefixReuse {
    pub estimated_tokens: u64,
    pub reused_tokens: u64,
    pub messages: usize,
    /// 第一条与已知前缀不同的消息下标；整条请求都是已知前缀时为 None。
    pub first_changed_message: Option<usize>,
    /// 工具定义与已知请求都不同（工具变化会让整个前缀失效）。
    pub tools_changed: bool,
}

/// 用量观察者；按宿主登记的用途区分。实现不得阻塞或修改请求。
pub trait ModelUsageObserver: Send + Sync {
    fn reported(&self, role: &str, usage: ModelUsage);
    fn prefix(&self, _role: &str, _reuse: PrefixReuse) {}
}
