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
    pub fn new(text: String, origin: SystemPromptOrigin, revision: String) -> Result<Self, LlmError> {
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

/// 只在组合层装配时调用；不得在模型请求或工具循环中进行同步 I/O。
pub trait SystemPromptSource: Send + Sync {
    fn load(&self) -> Result<SystemPromptSnapshot, LlmError>;
}
