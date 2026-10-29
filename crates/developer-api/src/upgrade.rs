//! 实时升级的文件交接协议：模型只选择已验证候选摘要，监督器掌握停机与启动。
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const LIVE_UPGRADE_VERSION: u8 = 1;
pub const LIVE_UPGRADE_MAX_JSON: usize = 1024 * 1024;
pub const LIVE_UPGRADE_LEASE_MS: u64 = 24 * 3600 * 1000;

/// 宿主准备阶段签发的内容绑定；模型只能选摘要，不能补写或放宽校验字段。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpgradeLease {
    pub format_version: u8,
    pub candidate_sha256: String,
    pub manifest_sha256: String,
    pub plan_sha256: String,
    pub validation_sha256: String,
    pub state_schema_version: u8,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
}

pub fn valid_candidate_sha(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyUpgradeArguments {
    pub candidate_sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpgradeRequest {
    pub format_version: u8,
    pub candidate_sha256: String,
    pub candidate_directory: PathBuf,
    pub session_id: String,
    pub requested_by: String,
    pub turn_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<UpgradeLease>,
}

/// Python 监督器定期原子保存；附加状态只作为数据，不能授予权限。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpgradeSupervisor {
    pub format_version: u8,
    pub pid: u32,
    pub heartbeat_ms: u64,
    pub phase: String,
    pub active_sha256: String,
    pub candidate_sha256: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
}
