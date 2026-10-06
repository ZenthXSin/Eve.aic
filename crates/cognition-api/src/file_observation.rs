//! 受信宿主实际读取本地文件后提交的有界观察；不授予文件访问或执行权限。
use crate::{CognitionError, CognitionResult, validate_id};
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use std::{fmt, sync::Arc};

pub const FILE_OBSERVATION_SCHEMA_VERSION: u32 = 1;
pub const FILE_OBSERVATION_CHANNEL: &str = "cognition.file-observation";
pub const MAX_FILE_OBSERVATION_BYTES: u64 = 65_536;
pub const MAX_FILE_OBSERVATION_EXCERPT_BYTES: usize = 4096;
pub const MAX_FILE_OBSERVATION_JSON_BYTES: usize = 8192;

/// 主体、用户和原目标通道由宿主绑定；这里只接收已读取的数据，不接收文件路径。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileObservationInput {
    pub goal_id: String,
    pub expected_goal_revision: u64,
    /// 宿主由显式绑定的文件路径派生的稳定摘要；不得填写原文件路径。
    pub observation_source_id: String,
    /// 完整文件字节的 SHA-256，小写十六进制编码。
    pub sha256: String,
    pub byte_count: u64,
    pub text_excerpt: String,
    pub text_truncated: bool,
    pub observed_at_ms: u64,
}
impl FileObservationInput {
    pub fn validate(&self) -> CognitionResult<()> {
        validate_fields(
            &self.goal_id,
            &self.observation_source_id,
            &self.sha256,
            self.byte_count,
            &self.text_excerpt,
            self.text_truncated,
            self.observed_at_ms,
        )?;
        if self.expected_goal_revision == 0 {
            return Err(CognitionError::InvalidInput);
        }
        validate_encoded(self)
    }
    pub fn parse(text: &str) -> CognitionResult<Self> {
        validate_json_length(text)?;
        let parsed: Self = serde_json::from_str(text).map_err(|_| CognitionError::InvalidInput)?;
        parsed.validate()?;
        Ok(parsed)
    }
    pub fn to_json(&self) -> CognitionResult<String> {
        self.validate()?;
        serde_json::to_string(self).map_err(|_| CognitionError::InvalidInput)
    }
}

/// Environment 来源事件的 summary；同时更新 Waiting 父目标的 wait_reason。
/// 观察内容仍是外部数据，不能解释为用户反馈、工具回执、已达成目标或新的执行权限。
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileObservation {
    pub schema_version: u32,
    pub goal_id: String,
    pub observation_source_id: String,
    pub sha256: String,
    pub byte_count: u64,
    pub text_excerpt: String,
    pub text_truncated: bool,
    pub observed_at_ms: u64,
    pub previous_goal_revision: u64,
    pub goal_revision: u64,
}
impl FileObservation {
    pub fn validate(&self) -> CognitionResult<()> {
        validate_fields(
            &self.goal_id,
            &self.observation_source_id,
            &self.sha256,
            self.byte_count,
            &self.text_excerpt,
            self.text_truncated,
            self.observed_at_ms,
        )?;
        if self.schema_version != FILE_OBSERVATION_SCHEMA_VERSION
            || self.previous_goal_revision == 0
            || self.previous_goal_revision.checked_add(1) != Some(self.goal_revision)
        {
            return Err(CognitionError::InvalidInput);
        }
        validate_encoded(self)
    }
    pub fn parse(text: &str) -> CognitionResult<Self> {
        validate_json_length(text)?;
        let parsed: Self = serde_json::from_str(text).map_err(|_| CognitionError::InvalidInput)?;
        parsed.validate()?;
        Ok(parsed)
    }
    pub fn to_json(&self) -> CognitionResult<String> {
        self.validate()?;
        serde_json::to_string(self).map_err(|_| CognitionError::InvalidInput)
    }

    /// 稳定事件标识；长度前缀及域标记避免字段边界歧义，不包含文件原路径。
    pub fn event_id(&self, subject_id: &str) -> CognitionResult<String> {
        self.validate()?;
        validate_id(subject_id)?;
        let mut digest = Context::new(&SHA256);
        for part in [
            "cognition.file-observation:v1",
            subject_id,
            &self.goal_id,
            &self.observation_source_id,
            &self.previous_goal_revision.to_string(),
            &self.sha256,
        ] {
            digest.update(&(part.len() as u64).to_be_bytes());
            digest.update(part.as_bytes());
        }
        let mut id = String::from("file-observation:");
        for byte in digest.finish().as_ref() {
            use std::fmt::Write;
            // Writing into String cannot fail.
            let _ = write!(&mut id, "{byte:02x}");
        }
        Ok(id)
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileObservationReport {
    pub goal_id: String,
    /// 幂等返回原观察提交后的目标修订；revision 是读取到的当前全局修订。
    pub goal_revision: u64,
    pub revision: u64,
    pub duplicate: bool,
}
pub trait FileObservationService: Send + Sync {
    /// 一次 CAS 保存实际观察和父目标的新修订；不自动重试、不增加执行预算。
    /// 该目标最近文件观察的来源和内容相同时返回 duplicate，不重写后续用户反馈。
    /// observed_at_ms 和 expected_goal_revision 不参与已保存内容的幂等比较。
    fn submit(&self, input: FileObservationInput) -> CognitionResult<FileObservationReport>;
}
#[derive(Clone)]
pub struct FileObservationServiceHandle(pub Arc<dyn FileObservationService>);

fn validate_fields(
    goal_id: &str,
    observation_source_id: &str,
    sha256: &str,
    byte_count: u64,
    text_excerpt: &str,
    text_truncated: bool,
    observed_at_ms: u64,
) -> CognitionResult<()> {
    validate_id(goal_id)?;
    validate_id(observation_source_id)?;
    if !observation_source_id
        .strip_prefix("file-source:")
        .is_some_and(is_sha256)
        || !is_sha256(sha256)
        || byte_count > MAX_FILE_OBSERVATION_BYTES
        || text_excerpt.len() > MAX_FILE_OBSERVATION_EXCERPT_BYTES
        || text_excerpt.contains('\0')
        || text_excerpt.len() as u64 > byte_count
        || text_truncated != ((text_excerpt.len() as u64) < byte_count)
        || observed_at_ms == 0
    {
        return Err(CognitionError::InvalidInput);
    }
    Ok(())
}
fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn validate_json_length(text: &str) -> CognitionResult<()> {
    if text.is_empty() || text.len() > MAX_FILE_OBSERVATION_JSON_BYTES {
        return Err(CognitionError::InvalidInput);
    }
    Ok(())
}
fn validate_encoded(value: &impl Serialize) -> CognitionResult<()> {
    let text = serde_json::to_string(value).map_err(|_| CognitionError::InvalidInput)?;
    validate_json_length(&text)
}
macro_rules! redacted {
    ($($ty:ty),+) => { $(impl fmt::Debug for $ty {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(concat!(stringify!($ty), "(<redacted>)"))
        }
    })+ };
}
redacted!(FileObservationInput, FileObservation, FileObservationReport);

#[cfg(test)]
mod tests {
    use super::*;

    fn observation() -> FileObservation {
        FileObservation {
            schema_version: 1,
            goal_id: "goal".into(),
            observation_source_id: format!("file-source:{}", "a".repeat(64)),
            sha256: "b".repeat(64),
            byte_count: "文件 📚".len() as u64,
            text_excerpt: "文件 📚".into(),
            text_truncated: false,
            observed_at_ms: 100,
            previous_goal_revision: 1,
            goal_revision: 2,
        }
    }

    #[test]
    fn strict_json_rejects_duplicates_unknown_fields_invalid_ranges_and_escaped_overflow() {
        let value = observation();
        let json = value.to_json().unwrap();
        assert_eq!(FileObservation::parse(&json).unwrap(), value);
        assert!(
            FileObservation::parse(&json.replacen(
                "\"schema_version\":1",
                "\"schema_version\":1,\"schema_version\":1",
                1
            ))
            .is_err()
        );
        assert!(
            FileObservation::parse(&json.replacen(
                "\"schema_version\":1",
                "\"schema_version\":1,\"path\":\"/private\"",
                1
            ))
            .is_err()
        );
        for invalid in [
            FileObservation {
                schema_version: 2,
                ..value.clone()
            },
            FileObservation {
                previous_goal_revision: u64::MAX,
                goal_revision: 0,
                ..value.clone()
            },
            FileObservation {
                goal_revision: 3,
                ..value.clone()
            },
            FileObservation {
                observed_at_ms: 0,
                ..value.clone()
            },
            FileObservation {
                observation_source_id: "/private/path".into(),
                ..value.clone()
            },
            FileObservation {
                sha256: "A".repeat(64),
                ..value.clone()
            },
            FileObservation {
                sha256: "b".repeat(63),
                ..value.clone()
            },
            FileObservation {
                byte_count: MAX_FILE_OBSERVATION_BYTES + 1,
                text_truncated: true,
                ..value.clone()
            },
            FileObservation {
                byte_count: 0,
                ..value.clone()
            },
            FileObservation {
                text_truncated: true,
                ..value.clone()
            },
            FileObservation {
                byte_count: 4097,
                text_excerpt: "x".repeat(4097),
                ..value.clone()
            },
            FileObservation {
                byte_count: 1,
                text_excerpt: "\0".into(),
                ..value.clone()
            },
            FileObservation {
                byte_count: 4096,
                text_excerpt: "\u{0001}".repeat(4096),
                ..value.clone()
            },
        ] {
            assert_eq!(invalid.validate(), Err(CognitionError::InvalidInput));
            assert!(invalid.to_json().is_err());
        }
        let mut padded = json.clone();
        padded.push_str(&" ".repeat(MAX_FILE_OBSERVATION_JSON_BYTES));
        assert!(FileObservation::parse(&padded).is_err());
    }

    #[test]
    fn empty_whitespace_and_utf8_prefix_are_valid_without_fake_completion() {
        let base = observation();
        for (text, byte_count, truncated) in [
            ("", 0, false),
            (" \n", 2, false),
            ("文件", MAX_FILE_OBSERVATION_BYTES, true),
        ] {
            let value = FileObservation {
                text_excerpt: text.into(),
                byte_count,
                text_truncated: truncated,
                ..base.clone()
            };
            assert_eq!(
                FileObservation::parse(&value.to_json().unwrap()).unwrap(),
                value
            );
        }
        let input = FileObservationInput {
            goal_id: base.goal_id,
            expected_goal_revision: 1,
            observation_source_id: base.observation_source_id,
            sha256: base.sha256,
            byte_count: 0,
            text_excerpt: String::new(),
            text_truncated: false,
            observed_at_ms: 100,
        };
        assert_eq!(
            FileObservationInput::parse(&input.to_json().unwrap()).unwrap(),
            input
        );
        assert!(
            FileObservationInput {
                expected_goal_revision: 0,
                ..input
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn stable_event_id_binds_subject_source_parent_revision_and_digest() {
        let value = observation();
        let id = value.event_id("eve").unwrap();
        assert_eq!(id.len(), "file-observation:".len() + 64);
        assert_eq!(value.event_id("eve").unwrap(), id);
        assert_ne!(value.event_id("another-eve").unwrap(), id);
        assert_ne!(
            FileObservation {
                goal_id: "another-goal".into(),
                ..value.clone()
            }
            .event_id("eve")
            .unwrap(),
            id
        );
        assert_ne!(
            FileObservation {
                observation_source_id: format!("file-source:{}", "c".repeat(64)),
                ..value.clone()
            }
            .event_id("eve")
            .unwrap(),
            id
        );
        assert_ne!(
            FileObservation {
                previous_goal_revision: 2,
                goal_revision: 3,
                ..value.clone()
            }
            .event_id("eve")
            .unwrap(),
            id
        );
        assert_ne!(
            FileObservation {
                sha256: "c".repeat(64),
                ..value.clone()
            }
            .event_id("eve")
            .unwrap(),
            id
        );
        assert!(value.event_id("").is_err());
    }
}
