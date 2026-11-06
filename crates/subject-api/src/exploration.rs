//! 无预设目标的探索经历。片段来自宿主实际回执，感想单独保存，不构成授权。
use crate::{SubjectError, SubjectResult, SubjectScope, validate_id, validate_scope};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MAX_EXPLORATION_EXCERPT_BYTES: usize = 2048;
pub const MAX_EXPLORATION_TOOL_EXPERIENCES: usize = 64;
pub const MAX_EXPLORATION_EXPERIENCE_PAGE: usize = 16;
/// 包括 JSON 控制字符转义后的最坏展开，独立于旧回顾的大小限制。
pub const MAX_EXPLORATION_EXPERIENCE_BYTES: usize = 3 * 1024 * 1024;

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplorationExcerpt {
    pub text: String,
    /// 截断前原文的 UTF-8 字节数与 SHA-256，由持有真实回执的宿主生成。
    pub original_bytes: u64,
    pub sha256: String,
    pub truncated: bool,
}

impl std::fmt::Debug for ExplorationExcerpt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExplorationExcerpt")
            .field("original_bytes", &self.original_bytes)
            .field("truncated", &self.truncated)
            .finish_non_exhaustive()
    }
}

impl ExplorationExcerpt {
    pub fn validate(&self) -> SubjectResult<()> {
        let bytes = self.text.len() as u64;
        if self.text.len() > MAX_EXPLORATION_EXCERPT_BYTES
            || self.original_bytes < bytes
            || self.truncated != (self.original_bytes > bytes)
            || self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(SubjectError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExplorationToolOutcome {
    Succeeded,
    Failed,
    Denied,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplorationToolExperience {
    pub call_id: String,
    pub tool: String,
    pub arguments: ExplorationExcerpt,
    pub output: Option<ExplorationExcerpt>,
    pub outcome: ExplorationToolOutcome,
    pub error: Option<ExplorationExcerpt>,
}

impl ExplorationToolExperience {
    pub fn validate(&self) -> SubjectResult<()> {
        validate_id(&self.call_id)?;
        validate_id(&self.tool)?;
        self.arguments.validate()?;
        if let Some(output) = &self.output {
            output.validate()?;
        }
        if let Some(error) = &self.error {
            error.validate()?;
        }
        if match self.outcome {
            ExplorationToolOutcome::Succeeded => self.output.is_none() || self.error.is_some(),
            ExplorationToolOutcome::Failed | ExplorationToolOutcome::Denied => {
                self.output.is_none() && self.error.is_none()
            }
            ExplorationToolOutcome::Unknown => self.output.is_some(),
        } {
            return Err(SubjectError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExplorationEndReason {
    Natural,
    BudgetExhausted,
    Cancelled,
    Failed,
    Unknown,
    Interrupted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplorationExperience {
    pub session_id: String,
    pub scope: SubjectScope,
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    pub end_reason: ExplorationEndReason,
    pub tools: Vec<ExplorationToolExperience>,
    /// 本执行器检查点可观察的总调用数，包含尚无回执的调用，不含子代理内部轨迹。
    /// tools 是其中连续的有界片段；此数不是包含子代理的资源总用量。
    pub total_tool_calls: u64,
    pub tool_offset: u64,
    pub tools_truncated: bool,
    pub subjective_notes: Option<ExplorationExcerpt>,
}

impl ExplorationExperience {
    pub fn validate(&self) -> SubjectResult<()> {
        validate_id(&self.session_id)?;
        validate_scope(&self.scope)?;
        if self.started_at_ms == 0
            || self.ended_at_ms < self.started_at_ms
            || self.tools.len() > MAX_EXPLORATION_TOOL_EXPERIENCES
            || self
                .tool_offset
                .checked_add(self.tools.len() as u64)
                .is_none_or(|end| end > self.total_tool_calls)
            || self.tools_truncated != (self.tools.len() as u64 != self.total_tool_calls)
        {
            return Err(SubjectError::InvalidInput);
        }
        let mut calls = BTreeSet::new();
        for tool in &self.tools {
            tool.validate()?;
            if !calls.insert(&tool.call_id)
                || (tool.outcome == ExplorationToolOutcome::Unknown
                    && self.end_reason != ExplorationEndReason::Unknown)
            {
                return Err(SubjectError::InvalidInput);
            }
        }
        if let Some(notes) = &self.subjective_notes {
            notes.validate()?;
        }
        if serde_json::to_vec(self)
            .map_err(|_| SubjectError::InvalidInput)?
            .len()
            > MAX_EXPLORATION_EXPERIENCE_BYTES
        {
            return Err(SubjectError::InvalidInput);
        }
        Ok(())
    }

    /// 已确认的失败和权限拒绝也是真实经历；仅有感想或未知调用不冒充外界反馈。
    pub fn has_external_evidence(&self) -> bool {
        self.tools.iter().any(|tool| {
            tool.outcome != ExplorationToolOutcome::Unknown
                && (tool.output.is_some() || tool.error.is_some())
        })
    }
}

/// 以持久写入序号倒序分页；游标不依赖时钟，同毫秒或迟到的记录不会相互覆盖。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplorationExperiencePage {
    pub total: u64,
    pub items: Vec<ExplorationExperience>,
    pub next_before: Option<u64>,
}
