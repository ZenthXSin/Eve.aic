//! 宿主显式授权的文档产物行动契约。提案不携带原始路径，也不授予模型执行权限。
//!
//! 每次行动的预算固定为零次模型请求、一次文件新建和一次尝试。持久化 `Executing`
//! 必须先于副作用；恢复时将未结束记录封存为 `Interrupted`，不能自动重试。
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt, sync::Arc};

pub const ACTION_PLUGIN_ID: &str = "eve.action";
pub const ACTION_SCHEMA_VERSION: u32 = 1;
pub const MAX_ARTIFACT_BYTES: u64 = 16_384;
pub const MAX_INPUT_BYTES: u64 = 65_536;
pub const MAX_ACTION_TIMEOUT_MS: u64 = 30_000;
pub const MAX_ACTION_RECORDS: usize = 128;
pub const MAX_ACTION_JSON_BYTES: usize = 16_384;
pub const MAX_ACTION_STATE_BYTES: usize = 2_097_152;
pub const ACTION_MODEL_CALLS: u32 = 0;
pub const ACTION_FILE_WRITES: u32 = 1;
pub const ACTION_ATTEMPTS: u32 = 1;

pub type ActionResult<T> = Result<T, ActionError>;
pub type ArtifactResult<T> = Result<T, ArtifactError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionError {
    InvalidInput,
    AccessDenied,
    Conflict,
    StaleRevision,
    InvalidTransition,
    LimitReached,
    CorruptState,
    Storage,
    Unavailable,
    SubjectMismatch,
    Cancelled,
    DeadlineExceeded,
}
impl fmt::Display for ActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "invalid action input",
            Self::AccessDenied => "action access denied",
            Self::Conflict => "action request conflicts with a recorded request",
            Self::StaleRevision => "stale action revision",
            Self::InvalidTransition => "invalid action transition",
            Self::LimitReached => "action capacity limit reached",
            Self::CorruptState => "corrupt action state",
            Self::Storage => "action storage operation failed",
            Self::Unavailable => "action service unavailable",
            Self::SubjectMismatch => "action subject mismatch",
            Self::Cancelled => "action cancelled before execution",
            Self::DeadlineExceeded => "action deadline exceeded before execution",
        })
    }
}
impl std::error::Error for ActionError {}

/// 文件适配器只返回固定错误类别，不把本地路径或操作系统错误原文交给上层。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactError {
    InvalidInput,
    AccessDenied,
    AlreadyExists,
    NotFound,
    Changed,
    LimitReached,
    Storage,
    Unavailable,
}
impl fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "invalid artifact input",
            Self::AccessDenied => "artifact access denied",
            Self::AlreadyExists => "artifact already exists",
            Self::NotFound => "artifact not found",
            Self::Changed => "artifact target changed",
            Self::LimitReached => "artifact size limit reached",
            Self::Storage => "artifact storage operation failed",
            Self::Unavailable => "artifact service unavailable",
        })
    }
}
impl std::error::Error for ArtifactError {}

/// 宿主在核对目标、当前反思、观察和显式绑定目标文件后组装的完整提案。
/// `artifact_*` 只描述宿主已渲染的有限文档字节；不能将模型文本解释为文件路径。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentActionProposal {
    pub schema_version: u32,
    pub action_id: String,
    pub subject_id: String,
    pub user_id: String,
    pub goal_id: String,
    pub goal_revision: u64,
    pub reflection_goal_id: String,
    pub reflection_goal_revision: u64,
    pub observation_event_id: String,
    pub observation_source_id: String,
    pub input_sha256: String,
    pub input_byte_count: u64,
    pub artifact_source_id: String,
    pub artifact_sha256: String,
    pub artifact_byte_count: u64,
    pub created_at_ms: u64,
    pub timeout_ms: u64,
}
impl DocumentActionProposal {
    pub fn validate(&self) -> ActionResult<()> {
        self.validate_fields()?;
        validate_encoded(self, MAX_ACTION_JSON_BYTES)
    }
    fn validate_fields(&self) -> ActionResult<()> {
        for id in [
            &self.action_id,
            &self.subject_id,
            &self.user_id,
            &self.goal_id,
            &self.reflection_goal_id,
            &self.observation_event_id,
        ] {
            validate_id(id)?;
        }
        if self.schema_version != ACTION_SCHEMA_VERSION
            || self.goal_revision == 0
            || self.reflection_goal_revision == 0
            || !is_digest_id(&self.observation_event_id, "file-observation:")
            || !is_digest_id(&self.observation_source_id, "file-source:")
            || !is_digest_id(&self.artifact_source_id, "artifact-file:")
            || !is_sha256(&self.input_sha256)
            || !is_sha256(&self.artifact_sha256)
            || self.input_byte_count > MAX_INPUT_BYTES
            || self.artifact_byte_count > MAX_ARTIFACT_BYTES
            || self.created_at_ms == 0
            || !(1..=MAX_ACTION_TIMEOUT_MS).contains(&self.timeout_ms)
            || self.action_id
                != derive_action_id(
                    &self.subject_id,
                    &self.user_id,
                    &self.goal_id,
                    self.goal_revision,
                    &self.reflection_goal_id,
                )?
        {
            return Err(ActionError::InvalidInput);
        }
        Ok(())
    }
    pub fn parse(text: &str) -> ActionResult<Self> {
        let parsed: Self = parse_json(text, MAX_ACTION_JSON_BYTES)?;
        parsed.validate()?;
        Ok(parsed)
    }
    pub fn to_json(&self) -> ActionResult<String> {
        self.validate()?;
        encode_json(self)
    }
    /// 重送只允许生成时刻不同；来源、产物、目标修订和超时预算仍须完全一致。
    pub fn same_request(&self, other: &Self) -> bool {
        let mut normalized = other.clone();
        normalized.created_at_ms = self.created_at_ms;
        self == &normalized
    }
}

/// 行动身份不含输出目标；为同一父目标修订和反思换路径不能获得另一次行动预算。
pub fn derive_action_id(
    subject_id: &str,
    user_id: &str,
    goal_id: &str,
    goal_revision: u64,
    reflection_goal_id: &str,
) -> ActionResult<String> {
    for id in [subject_id, user_id, goal_id, reflection_goal_id] {
        validate_id(id)?;
    }
    if goal_revision == 0 {
        return Err(ActionError::InvalidInput);
    }
    let mut digest = Context::new(&SHA256);
    for part in [
        "eve.document-action:v1",
        subject_id,
        user_id,
        goal_id,
        &goal_revision.to_string(),
        reflection_goal_id,
    ] {
        digest.update(&(part.len() as u64).to_be_bytes());
        digest.update(part.as_bytes());
    }
    let mut id = String::from("document-action:");
    for byte in digest.finish().as_ref() {
        use std::fmt::Write;
        let _ = write!(&mut id, "{byte:02x}");
    }
    Ok(id)
}

/// 独立读取已写目标得到的内容证据。回执只证明这些文件字节，不宣称父目标已达成。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactReceipt {
    pub artifact_source_id: String,
    pub sha256: String,
    pub byte_count: u64,
    pub verified_at_ms: u64,
}
impl ArtifactReceipt {
    pub fn validate(&self) -> ActionResult<()> {
        if !is_digest_id(&self.artifact_source_id, "artifact-file:")
            || !is_sha256(&self.sha256)
            || self.byte_count > MAX_ARTIFACT_BYTES
            || self.verified_at_ms == 0
        {
            return Err(ActionError::InvalidInput);
        }
        validate_encoded(self, MAX_ACTION_JSON_BYTES)
    }
    pub fn parse(text: &str) -> ActionResult<Self> {
        let parsed: Self = parse_json(text, MAX_ACTION_JSON_BYTES)?;
        parsed.validate()?;
        Ok(parsed)
    }
    pub fn to_json(&self) -> ActionResult<String> {
        self.validate()?;
        encode_json(self)
    }
    pub fn matches_proposal(&self, proposal: &DocumentActionProposal) -> bool {
        self.artifact_source_id == proposal.artifact_source_id
            && self.sha256 == proposal.artifact_sha256
            && self.byte_count == proposal.artifact_byte_count
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    Executing,
    Completed,
    Blocked,
}

/// 失败原因不含模型文本、路径或不受限的外部错误字符串。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionFailure {
    Interrupted,
    Cancelled,
    DeadlineExceeded,
    PreconditionChanged,
    WriteFailed,
    VerificationFailed,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionRecord {
    pub revision: u64,
    pub proposal: DocumentActionProposal,
    pub status: ActionStatus,
    pub finished_at_ms: Option<u64>,
    pub receipt: Option<ArtifactReceipt>,
    pub failure: Option<ActionFailure>,
}
impl ActionRecord {
    pub fn validate(&self) -> ActionResult<()> {
        self.proposal.validate()?;
        if self.revision == 0 {
            return Err(ActionError::InvalidInput);
        }
        match self.status {
            ActionStatus::Executing => {
                if self.finished_at_ms.is_some() || self.receipt.is_some() || self.failure.is_some()
                {
                    return Err(ActionError::InvalidInput);
                }
            }
            ActionStatus::Completed => {
                let receipt = self.receipt.as_ref().ok_or(ActionError::InvalidInput)?;
                receipt.validate()?;
                if self.failure.is_some()
                    || self.finished_at_ms != Some(receipt.verified_at_ms)
                    || !receipt.matches_proposal(&self.proposal)
                {
                    return Err(ActionError::InvalidInput);
                }
            }
            ActionStatus::Blocked => {
                if self.receipt.is_some()
                    || self.failure.is_none()
                    || self.finished_at_ms.is_none_or(|at_ms| at_ms == 0)
                {
                    return Err(ActionError::InvalidInput);
                }
            }
        }
        validate_encoded(self, MAX_ACTION_JSON_BYTES)
    }
    pub fn parse(text: &str) -> ActionResult<Self> {
        let parsed: Self = parse_json(text, MAX_ACTION_JSON_BYTES)?;
        parsed.validate()?;
        Ok(parsed)
    }
    pub fn to_json(&self) -> ActionResult<String> {
        self.validate()?;
        encode_json(self)
    }
}

/// 每个主体的有界行动日志；容量耗尽必须显式失败，不可删除旧记录以重新执行。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionSnapshot {
    pub schema_version: u32,
    pub subject_id: String,
    pub revision: u64,
    pub records: Vec<ActionRecord>,
}
impl ActionSnapshot {
    pub fn validate(&self) -> ActionResult<()> {
        validate_id(&self.subject_id)?;
        if self.schema_version != ACTION_SCHEMA_VERSION
            || self.records.len() > MAX_ACTION_RECORDS
            || (self.revision == 0 && !self.records.is_empty())
        {
            return Err(ActionError::InvalidInput);
        }
        let mut action_ids = BTreeSet::new();
        for record in &self.records {
            record.validate()?;
            if record.proposal.subject_id != self.subject_id
                || record.revision > self.revision
                || !action_ids.insert(&record.proposal.action_id)
            {
                return Err(ActionError::InvalidInput);
            }
        }
        validate_encoded(self, MAX_ACTION_STATE_BYTES)
    }
    pub fn parse(text: &str) -> ActionResult<Self> {
        let parsed: Self = parse_json(text, MAX_ACTION_STATE_BYTES)?;
        parsed.validate()?;
        Ok(parsed)
    }
    pub fn to_json(&self) -> ActionResult<String> {
        self.validate()?;
        encode_json(self)
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionBegin {
    pub record: ActionRecord,
    pub duplicate: bool,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionExecutionReport {
    pub record: ActionRecord,
    pub duplicate: bool,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionOutcome {
    Completed(ArtifactReceipt),
    Blocked {
        failure: ActionFailure,
        finished_at_ms: u64,
    },
}

pub trait ActionJournal: Send + Sync {
    fn snapshot(&self) -> ActionResult<ActionSnapshot>;
    /// 先持久化 Executing，再允许宿主写入。相同 action_id 使用 same_request 判断幂等，
    /// 已保存的任何状态均不可重新执行；不同目标文件等内容返回 Conflict。
    fn begin(&self, proposal: DocumentActionProposal) -> ActionResult<ActionBegin>;
    /// 仅 Executing 能结束，必须 CAS 核对 record 修订。写失败保持原记录不变。
    fn finish(
        &self,
        action_id: &str,
        expected_record_revision: u64,
        outcome: ActionOutcome,
    ) -> ActionResult<ActionRecord>;
}
#[derive(Clone)]
pub struct ActionJournalHandle(pub Arc<dyn ActionJournal>);

/// 宿主在 Executing 保存前后、写入前两次检查当前目标、反思和实际输入字节。
pub trait ActionPrecondition: Send + Sync {
    fn check(&self, proposal: &DocumentActionProposal) -> ActionResult<()>;
}

/// 能力对象由宿主事先绑定一个目标文件；调用方不能传入路径、覆盖开关或额外命令。
pub trait ArtifactTarget: Send + Sync {
    fn source_id(&self) -> &str;
    /// 限制为 MAX_ARTIFACT_BYTES 字节，新建且不覆盖现有对象。失败后不隐式重试。
    fn write_new(&self, bytes: &[u8]) -> ArtifactResult<()>;
    /// 必须独立读取落盘字节，不能用传入写入内容生成伪回执。
    fn read_back(&self, verified_at_ms: u64) -> ArtifactResult<ArtifactReceipt>;
}

fn validate_id(value: &str) -> ActionResult<()> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(ActionError::InvalidInput);
    }
    Ok(())
}
fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn is_digest_id(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(is_sha256)
}
fn parse_json<T: serde::de::DeserializeOwned>(text: &str, max_bytes: usize) -> ActionResult<T> {
    if text.is_empty() || text.len() > max_bytes {
        return Err(ActionError::InvalidInput);
    }
    serde_json::from_str(text).map_err(|_| ActionError::InvalidInput)
}
fn encode_json(value: &impl Serialize) -> ActionResult<String> {
    serde_json::to_string(value).map_err(|_| ActionError::InvalidInput)
}
fn validate_encoded(value: &impl Serialize, max_bytes: usize) -> ActionResult<()> {
    if encode_json(value)?.len() > max_bytes {
        return Err(ActionError::InvalidInput);
    }
    Ok(())
}
macro_rules! redacted {
    ($($ty:ty),+) => { $(impl fmt::Debug for $ty {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(concat!(stringify!($ty), "(<redacted>)"))
        }
    })+ };
}
redacted!(
    DocumentActionProposal,
    ArtifactReceipt,
    ActionRecord,
    ActionSnapshot,
    ActionBegin,
    ActionExecutionReport,
    ActionOutcome,
    ActionJournalHandle
);

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal() -> DocumentActionProposal {
        let subject_id = "subject".to_owned();
        let user_id = "owner".to_owned();
        let goal_id = "goal".to_owned();
        let reflection_goal_id = "reflection".to_owned();
        DocumentActionProposal {
            schema_version: ACTION_SCHEMA_VERSION,
            action_id: derive_action_id(&subject_id, &user_id, &goal_id, 4, &reflection_goal_id)
                .unwrap(),
            subject_id,
            user_id,
            goal_id,
            goal_revision: 4,
            reflection_goal_id,
            reflection_goal_revision: 3,
            observation_event_id: format!("file-observation:{}", "1".repeat(64)),
            observation_source_id: format!("file-source:{}", "2".repeat(64)),
            input_sha256: "3".repeat(64),
            input_byte_count: 6000,
            artifact_source_id: format!("artifact-file:{}", "4".repeat(64)),
            artifact_sha256: "5".repeat(64),
            artifact_byte_count: 1000,
            created_at_ms: 100,
            timeout_ms: MAX_ACTION_TIMEOUT_MS,
        }
    }

    fn receipt(proposal: &DocumentActionProposal) -> ArtifactReceipt {
        ArtifactReceipt {
            artifact_source_id: proposal.artifact_source_id.clone(),
            sha256: proposal.artifact_sha256.clone(),
            byte_count: proposal.artifact_byte_count,
            verified_at_ms: 50,
        }
    }

    fn executing(proposal: DocumentActionProposal) -> ActionRecord {
        ActionRecord {
            revision: 1,
            proposal,
            status: ActionStatus::Executing,
            finished_at_ms: None,
            receipt: None,
            failure: None,
        }
    }

    fn snapshot() -> ActionSnapshot {
        ActionSnapshot {
            schema_version: ACTION_SCHEMA_VERSION,
            subject_id: "subject".into(),
            revision: 1,
            records: vec![executing(proposal())],
        }
    }

    #[test]
    fn action_identity_scopes_requests_and_does_not_offer_a_budget_for_another_path() {
        let value = proposal();
        let mut new_target = value.clone();
        new_target.artifact_source_id = format!("artifact-file:{}", "6".repeat(64));
        new_target.validate().unwrap();
        assert_eq!(new_target.action_id, value.action_id);
        assert!(!new_target.same_request(&value));

        let ids = [
            derive_action_id("subject", "owner", "goal", 4, "reflection").unwrap(),
            derive_action_id("different", "owner", "goal", 4, "reflection").unwrap(),
            derive_action_id("subject", "different", "goal", 4, "reflection").unwrap(),
            derive_action_id("subject", "owner", "different", 4, "reflection").unwrap(),
            derive_action_id("subject", "owner", "goal", 5, "reflection").unwrap(),
            derive_action_id("subject", "owner", "goal", 4, "different").unwrap(),
        ];
        assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), ids.len());
        assert_ne!(
            derive_action_id("ab", "c", "goal", 1, "reflection").unwrap(),
            derive_action_id("a", "bc", "goal", 1, "reflection").unwrap(),
        );
        assert!(derive_action_id(" subject", "owner", "goal", 1, "reflection").is_err());
        assert!(derive_action_id("subject", "owner", "goal", 0, "reflection").is_err());
    }

    #[test]
    fn replay_only_ignores_creation_time() {
        let value = proposal();
        let mut replay = value.clone();
        replay.created_at_ms += 1;
        assert!(replay.same_request(&value));
        for changed in [
            DocumentActionProposal {
                timeout_ms: 1,
                ..replay.clone()
            },
            DocumentActionProposal {
                input_sha256: "6".repeat(64),
                ..replay.clone()
            },
            DocumentActionProposal {
                artifact_sha256: "6".repeat(64),
                ..replay.clone()
            },
            DocumentActionProposal {
                reflection_goal_revision: 4,
                ..replay.clone()
            },
        ] {
            changed.validate().unwrap();
            assert!(!value.same_request(&changed));
        }
    }

    #[test]
    fn proposal_rejects_forged_identity_unbound_sources_and_out_of_range_budgets() {
        let value = proposal();
        let invalid = [
            DocumentActionProposal {
                schema_version: 2,
                ..value.clone()
            },
            DocumentActionProposal {
                action_id: format!("document-action:{}", "0".repeat(64)),
                ..value.clone()
            },
            DocumentActionProposal {
                goal_revision: 5,
                ..value.clone()
            },
            DocumentActionProposal {
                reflection_goal_revision: 0,
                ..value.clone()
            },
            DocumentActionProposal {
                observation_event_id: "forged-event".into(),
                ..value.clone()
            },
            DocumentActionProposal {
                observation_source_id: "/private/file".into(),
                ..value.clone()
            },
            DocumentActionProposal {
                artifact_source_id: "/private/output".into(),
                ..value.clone()
            },
            DocumentActionProposal {
                input_sha256: "A".repeat(64),
                ..value.clone()
            },
            DocumentActionProposal {
                artifact_sha256: "6".repeat(63),
                ..value.clone()
            },
            DocumentActionProposal {
                input_byte_count: MAX_INPUT_BYTES + 1,
                ..value.clone()
            },
            DocumentActionProposal {
                artifact_byte_count: MAX_ARTIFACT_BYTES + 1,
                ..value.clone()
            },
            DocumentActionProposal {
                created_at_ms: 0,
                ..value.clone()
            },
            DocumentActionProposal {
                timeout_ms: 0,
                ..value.clone()
            },
            DocumentActionProposal {
                timeout_ms: MAX_ACTION_TIMEOUT_MS + 1,
                ..value.clone()
            },
        ];
        for invalid in invalid {
            assert_eq!(invalid.validate(), Err(ActionError::InvalidInput));
        }
        DocumentActionProposal {
            input_byte_count: MAX_INPUT_BYTES,
            artifact_byte_count: MAX_ARTIFACT_BYTES,
            ..value
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn json_contract_rejects_duplicate_unknown_trailing_and_oversized_data() {
        let value = proposal();
        let json = value.to_json().unwrap();
        assert_eq!(DocumentActionProposal::parse(&json).unwrap(), value);
        for invalid in [
            json.replacen(
                "\"schema_version\":1",
                "\"schema_version\":1,\"schema_version\":1",
                1,
            ),
            json.replacen(
                "\"schema_version\":1",
                "\"schema_version\":1,\"path\":\"/private\"",
                1,
            ),
            format!("{json} {{}}"),
            " ".repeat(MAX_ACTION_JSON_BYTES + 1),
        ] {
            assert!(DocumentActionProposal::parse(&invalid).is_err());
        }
        let receipt = receipt(&value);
        assert_eq!(
            ArtifactReceipt::parse(&receipt.to_json().unwrap()).unwrap(),
            receipt
        );
        let state = snapshot();
        assert_eq!(
            ActionSnapshot::parse(&state.to_json().unwrap()).unwrap(),
            state
        );
        let record = &state.records[0];
        assert_eq!(
            ActionRecord::parse(&record.to_json().unwrap()).unwrap(),
            *record
        );
    }

    #[test]
    fn completed_requires_independent_matching_evidence_and_all_states_are_disjoint() {
        let running = executing(proposal());
        running.validate().unwrap();
        let receipt = receipt(&running.proposal);
        let completed = ActionRecord {
            revision: 2,
            status: ActionStatus::Completed,
            finished_at_ms: Some(receipt.verified_at_ms),
            receipt: Some(receipt.clone()),
            ..running.clone()
        };
        // 墙钟可能倒退；正确回执时间无需晚于 proposal.created_at_ms。
        completed.validate().unwrap();
        ActionRecord {
            revision: 2,
            status: ActionStatus::Blocked,
            finished_at_ms: Some(50),
            failure: Some(ActionFailure::Interrupted),
            ..running.clone()
        }
        .validate()
        .unwrap();
        for invalid in [
            ActionRecord {
                receipt: None,
                ..completed.clone()
            },
            ActionRecord {
                finished_at_ms: Some(0),
                ..completed.clone()
            },
            ActionRecord {
                finished_at_ms: Some(51),
                ..completed.clone()
            },
            ActionRecord {
                failure: Some(ActionFailure::WriteFailed),
                ..completed.clone()
            },
            ActionRecord {
                receipt: Some(ArtifactReceipt {
                    sha256: "6".repeat(64),
                    ..receipt.clone()
                }),
                ..completed.clone()
            },
            ActionRecord {
                receipt: Some(ArtifactReceipt {
                    byte_count: receipt.byte_count + 1,
                    ..receipt.clone()
                }),
                ..completed.clone()
            },
            ActionRecord {
                receipt: Some(ArtifactReceipt {
                    artifact_source_id: format!("artifact-file:{}", "6".repeat(64)),
                    ..receipt
                }),
                ..completed.clone()
            },
            ActionRecord {
                status: ActionStatus::Executing,
                ..completed
            },
            ActionRecord {
                status: ActionStatus::Blocked,
                ..running.clone()
            },
            ActionRecord {
                revision: 0,
                ..running
            },
        ] {
            assert_eq!(invalid.validate(), Err(ActionError::InvalidInput));
        }
    }

    #[test]
    fn snapshots_reject_duplicate_cross_subject_and_uncommitted_records() {
        let value = snapshot();
        ActionSnapshot {
            records: vec![],
            revision: 0,
            ..value.clone()
        }
        .validate()
        .unwrap();
        for invalid in [
            ActionSnapshot {
                revision: 0,
                ..value.clone()
            },
            ActionSnapshot {
                schema_version: 2,
                ..value.clone()
            },
            ActionSnapshot {
                subject_id: "other".into(),
                ..value.clone()
            },
            ActionSnapshot {
                records: vec![value.records[0].clone(); 2],
                ..value.clone()
            },
            ActionSnapshot {
                records: vec![ActionRecord {
                    revision: 2,
                    ..value.records[0].clone()
                }],
                ..value.clone()
            },
        ] {
            assert_eq!(invalid.validate(), Err(ActionError::InvalidInput));
        }
        let mut many = value;
        many.records.clear();
        for index in 0..=MAX_ACTION_RECORDS {
            let mut proposal = proposal();
            proposal.goal_id = format!("goal-{index}");
            proposal.action_id = derive_action_id(
                &proposal.subject_id,
                &proposal.user_id,
                &proposal.goal_id,
                proposal.goal_revision,
                &proposal.reflection_goal_id,
            )
            .unwrap();
            many.records.push(executing(proposal));
        }
        assert_eq!(many.validate(), Err(ActionError::InvalidInput));
        many.records.pop();
        many.validate().unwrap();
    }

    #[test]
    fn debug_output_redacts_all_caller_identifiers() {
        let state = snapshot();
        let record = state.records[0].clone();
        let receipt = receipt(&record.proposal);
        for output in [
            format!("{state:?}"),
            format!("{record:?}"),
            format!("{:?}", record.proposal),
            format!("{receipt:?}"),
            format!(
                "{:?}",
                ActionBegin {
                    record: record.clone(),
                    duplicate: false
                }
            ),
            format!(
                "{:?}",
                ActionExecutionReport {
                    record,
                    duplicate: false
                }
            ),
            format!("{:?}", ActionOutcome::Completed(receipt)),
        ] {
            assert!(output.contains("<redacted>"));
            assert!(!output.contains("subject"));
            assert!(!output.contains("owner"));
            assert!(!output.contains("file-source:"));
            assert!(!output.contains("artifact-file:"));
        }
    }
}
