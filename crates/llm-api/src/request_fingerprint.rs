//! 判断一个待恢复的模型请求是否仍是同一正文；不用于派生会话、身份或缓存键。
use crate::LlmError;
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};

/// Provider 对实际请求目的地、正文与模式的版本化摘要。
/// 原文只进入 SHA-256；检查点只保存摘要。不能把它当作用户身份或会话 ID。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ModelRequestFingerprint(String);

impl ModelRequestFingerprint {
    /// `domain` 标识 Provider 的线协议版本；`value` 必须包含所有影响实际请求的固定配置。
    /// 可包含请求正文，但不得包含凭据；该方法不会保存或输出输入数据。
    pub fn from_json<T: Serialize>(domain: &str, value: &T) -> Result<Self, LlmError> {
        struct Writer(Context);
        impl std::io::Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.update(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut writer = Writer(Context::new(&SHA256));
        serde_json::to_writer(&mut writer, &("eve.request-fingerprint.v1", domain, value))
            .map_err(|_| LlmError::Protocol("模型请求指纹无法编码".into()))?;
        Ok(Self(
            writer
                .0
                .finish()
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        ))
    }
}

impl From<ModelRequestFingerprint> for String {
    fn from(value: ModelRequestFingerprint) -> Self {
        value.0
    }
}

impl TryFrom<String> for ModelRequestFingerprint {
    type Error = LlmError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(LlmError::Context("模型请求指纹无效；保留原记录".into()));
        }
        Ok(Self(value))
    }
}
