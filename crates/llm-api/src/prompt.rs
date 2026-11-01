//! 固定提示词来源契约；只描述已经读取的内容，不访问文件系统。
use crate::LlmError;
use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SystemPromptOrigin {
    Inline,
    File { path: String },
    Custom { name: String },
}

/// 修订由来源实现提供；它是内容版本，不是权限或授信凭据。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemPromptMetadata {
    pub origin: SystemPromptOrigin,
    pub revision: String,
    pub bytes: usize,
}

/// 字段私有，构造后不可变；Debug 只展示来源信息。
#[derive(Clone, Eq, PartialEq)]
pub struct SystemPromptSnapshot {
    text: String,
    metadata: SystemPromptMetadata,
}

impl SystemPromptSnapshot {
    pub fn new(
        text: String,
        origin: SystemPromptOrigin,
        revision: String,
    ) -> Result<Self, LlmError> {
        if text.trim().is_empty() || text.contains('\0') || revision.trim().is_empty() {
            return Err(LlmError::Configuration("提示词内容或版本无效".into()));
        }
        let valid_origin = match &origin {
            SystemPromptOrigin::Inline => true,
            SystemPromptOrigin::File { path } => !path.trim().is_empty(),
            SystemPromptOrigin::Custom { name } => !name.trim().is_empty(),
        };
        if !valid_origin {
            return Err(LlmError::Configuration("提示词来源标识不能为空".into()));
        }
        Ok(Self {
            metadata: SystemPromptMetadata {
                origin,
                revision,
                bytes: text.len(),
            },
            text,
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn metadata(&self) -> &SystemPromptMetadata {
        &self.metadata
    }
}

impl fmt::Debug for SystemPromptSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemPromptSnapshot")
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

/// 静态来源只在组合层装配时调用。动态来源应只返回后台已验证缓存，
/// 不能在模型请求或工具循环中进行同步文件 I/O。
pub trait SystemPromptSource: Send + Sync {
    fn load(&self) -> Result<SystemPromptSnapshot, LlmError>;
}

/// 动态来源的绑定身份；相等表示同一缓存来源，不代表其当前文本版本相同。
#[derive(Clone)]
pub struct SystemPromptSourceHandle(std::sync::Arc<dyn SystemPromptSource>);
impl SystemPromptSourceHandle {
    pub fn new(source: std::sync::Arc<dyn SystemPromptSource>) -> Self {
        Self(source)
    }
}
impl PartialEq for SystemPromptSourceHandle {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for SystemPromptSourceHandle {}
impl SystemPromptSource for SystemPromptSourceHandle {
    fn load(&self) -> Result<SystemPromptSnapshot, LlmError> {
        self.0.load()
    }
}
impl fmt::Debug for SystemPromptSourceHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SystemPromptSourceHandle(..)")
    }
}
