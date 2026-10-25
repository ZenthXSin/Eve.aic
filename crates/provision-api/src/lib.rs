//! 条件自备：Eve 做事缺少运行条件（例如某个服务端、验证工具）时，自己找来源取得，实际验证后启用。
//!
//! 宿主登记有哪些条件、每个条件怎样才算可用（`ConditionVerifier`）；来源（下载地址或本机文件）
//! 由调用方给出，可以是模型自己找到的。取得的文件先放在暂存目录，摘要与实际验证都通过后才启用；
//! 验证失败的文件不保留，只记下失败原因。启用的条件以账本为准，账本损坏时拒绝启动，不自动清空。
use serde::{Deserialize, Serialize};
use std::{fmt, future::Future, path::Path, pin::Pin};

pub const PROVISION_PLUGIN_ID: &str = "eve.provision";
pub const PROVISION_STATE_KEY: &str = "provisions.v1";
pub const MAX_STATE_BYTES: usize = 256 * 1024;
/// 每个条件保留的最近尝试记录条数。
pub const MAX_ATTEMPTS_KEPT: usize = 8;
pub const MAX_SOURCE_BYTES: usize = 2048;
pub const MAX_REASON_BYTES: usize = 600;
pub const MAX_EVIDENCE_BYTES: usize = 2000;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type ProvisionResult<T> = Result<T, ProvisionError>;

/// 宿主登记的一项条件。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConditionSpec {
    /// 小写字母、数字与 `-`。
    pub id: String,
    /// 给模型和操作者看的说明：是什么、做什么用、对版本有什么要求。
    pub description: String,
    /// 启用后的文件名，例如 `server.jar`。
    pub file_name: String,
    /// 单个文件的大小上限。
    pub max_bytes: u64,
}

/// 取得来源：HTTPS 地址，或本机上已经存在的文件（例如 Eve 自己构建出来的产物）。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcquireSource {
    Url(String),
    LocalFile(String),
}
impl AcquireSource {
    pub fn text(&self) -> &str {
        match self {
            Self::Url(text) | Self::LocalFile(text) => text,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcquireRequest {
    pub condition: String,
    pub source: AcquireSource,
    /// 来源公布的 SHA-256（小写十六进制）；给出时必须一致。
    pub expected_sha256: Option<String>,
    /// 为什么需要它，记入账本。
    pub reason: String,
    pub now_ms: u64,
}

/// 已启用的条件。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Acquisition {
    pub condition: String,
    pub source: AcquireSource,
    pub sha256: String,
    pub bytes: u64,
    /// 相对条件目录的路径：`<条件>/<sha256>/<文件名>`。
    pub path: String,
    /// 验证时实际观察到的结果，例如运行环境版本。
    pub evidence: String,
    pub reason: String,
    pub acquired_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptRecord {
    pub source: AcquireSource,
    pub at_ms: u64,
    pub ready: bool,
    /// 成功时为验证证据，失败时为原因。
    pub detail: String,
}

/// 条件的当前情况。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConditionView {
    pub spec: ConditionSpec,
    pub ready: Option<Acquisition>,
    /// 最近的尝试，从新到旧。
    pub attempts: Vec<AttemptRecord>,
}

/// 由宿主实现：在不改动文件的前提下实际使用一次，确认它能满足条件，返回观察到的证据。
pub trait ConditionVerifier: Send + Sync {
    fn verify<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<String, String>>;
}

/// 只交给可信宿主。
pub trait ConditionProvider: Send + Sync {
    fn conditions(&self) -> ProvisionResult<Vec<ConditionView>>;
    /// 已启用条件的文件绝对路径；未启用为 None。
    fn ready_path(&self, condition: &str) -> ProvisionResult<Option<std::path::PathBuf>>;
    /// 取得、核对摘要并验证；全部通过后启用并替换此前启用的版本。同一条件同时只进行一次。
    fn acquire(&self, request: AcquireRequest) -> BoxFuture<'_, ProvisionResult<Acquisition>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProvisionError {
    InvalidInput(String),
    UnknownCondition,
    /// 同一条件正在取得。
    Busy,
    Fetch(String),
    TooLarge,
    DigestMismatch {
        actual: String,
    },
    VerifyFailed(String),
    Storage,
    CorruptState,
    UnsupportedVersion,
    Unavailable,
}
impl fmt::Display for ProvisionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(reason) => write!(f, "参数无效：{reason}"),
            Self::UnknownCondition => f.write_str("没有登记这个条件"),
            Self::Busy => f.write_str("这个条件正在取得"),
            Self::Fetch(reason) => write!(f, "取得失败：{reason}"),
            Self::TooLarge => f.write_str("文件超过这个条件的大小上限"),
            Self::DigestMismatch { actual } => write!(f, "SHA-256 不一致，实际为 {actual}"),
            Self::VerifyFailed(reason) => write!(f, "验证未通过：{reason}"),
            Self::Storage => f.write_str("条件账本写入失败"),
            Self::CorruptState => f.write_str("条件账本已损坏"),
            Self::UnsupportedVersion => f.write_str("条件账本格式版本不受支持"),
            Self::Unavailable => f.write_str("条件账本不可用"),
        }
    }
}
impl std::error::Error for ProvisionError {}

pub fn is_condition_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 文件名：字母、数字、`.`、`_`、`-`，不以 `.` 开头。
pub fn is_file_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.starts_with('.')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// 截到字节上限并去掉控制字符，用于账本里的原因与证据。
pub fn bounded(text: &str, limit: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut end = cleaned.len().min(limit);
    while !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    cleaned[..end].trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_and_bounded_text() {
        assert!(is_condition_id("mindustry-server"));
        assert!(!is_condition_id("Mindustry") && !is_condition_id("-a") && !is_condition_id(""));
        assert!(is_sha256(&"a".repeat(64)) && !is_sha256(&"A".repeat(64)));
        assert!(is_file_name("server.jar") && !is_file_name("../x") && !is_file_name(".hidden"));
        assert_eq!(bounded("一二\n三", 7), "一二");
    }
}
