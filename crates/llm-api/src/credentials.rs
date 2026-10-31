//! 模型凭据只由宿主注入；配置中的引用不是读取任意环境变量的许可。
use std::{fmt, sync::Arc};
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CredentialPurpose {
    Primary,
    Jev,
    Semantic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialError {
    UnknownReference,
    Missing,
    Invalid,
    Unavailable,
    VersionExhausted,
}
impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnknownReference => "模型凭据引用未获宿主绑定",
            Self::Missing => "所选模型凭据缺失",
            Self::Invalid => "模型凭据或租约无效",
            Self::Unavailable => "模型凭据代理不可用",
            Self::VersionExhausted => "模型凭据版本已耗尽",
        })
    }
}
impl std::error::Error for CredentialError {}

/// 一次解析的不可变快照。正文不实现 Debug/Serialize；最后一份持有者释放时清理本对象的字节。
/// 提供方自行建立的 HTTP 头及操作系统中的副本不在这项保证内。
#[derive(Clone)]
pub struct CredentialLease {
    purpose: CredentialPurpose,
    reference: String,
    version: u64,
    secret: Arc<Zeroizing<String>>,
}
impl CredentialLease {
    pub fn new(
        purpose: CredentialPurpose,
        reference: String,
        version: u64,
        secret: String,
    ) -> Result<Self, CredentialError> {
        let secret = Zeroizing::new(secret);
        if version == 0
            || reference.is_empty()
            || reference.len() > 256
            || reference
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control())
            || secret.is_empty()
            || secret.len() > 8192
            || !secret.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(CredentialError::Invalid);
        }
        Ok(Self {
            purpose,
            reference,
            version,
            secret: Arc::new(secret),
        })
    }
    pub fn validate_for(
        &self,
        purpose: CredentialPurpose,
        reference: &str,
    ) -> Result<(), CredentialError> {
        if self.purpose != purpose || self.reference != reference {
            return Err(CredentialError::Invalid);
        }
        Ok(())
    }
    pub fn version(&self) -> u64 {
        self.version
    }
    /// 只在可信提供方装配处使用；不得用于回执、日志或普通配置。
    pub fn expose(&self) -> &str {
        self.secret.as_str()
    }
}

/// 每轮重新解析，已开始的调用持有自己的租约/提供方。实现必须在凭据改变后递增版本，
/// 缺失或失败必须报错，不能隐式借用另一个用途或引用的密钥。
pub trait ModelCredentials: Send + Sync {
    fn resolve(
        &self,
        purpose: CredentialPurpose,
        reference: &str,
    ) -> Result<CredentialLease, CredentialError>;
}
