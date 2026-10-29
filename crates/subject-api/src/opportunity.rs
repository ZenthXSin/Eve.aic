//! 模型提出的价值判断只是候选数据；不构成授权或成功证据。
use crate::{SubjectError, SubjectResult, validate_id};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpportunityAssessment {
    /// 同一问题使用稳定键；改标题、换授权轮次不应再次执行。
    pub problem_key: String,
    pub expected_gain: String,
    pub verification: String,
    pub reuse_plan: String,
    pub benefit: u8,
    pub confidence: u8,
    pub cost: u8,
    pub risk: u8,
    /// 当前范围内已有关注或任务的标识。宿主核对存在性，不将引用当作成功证明。
    pub evidence: Vec<String>,
}

impl OpportunityAssessment {
    pub fn validate(&self) -> SubjectResult<()> {
        validate_id(&self.problem_key)?;
        if [&self.expected_gain, &self.verification, &self.reuse_plan]
            .iter()
            .any(|s| s.trim().is_empty() || s.len() > 1024)
            || [self.benefit, self.confidence, self.cost, self.risk]
                .iter()
                .any(|n| *n > 100)
            || self.evidence.len() > 8
        {
            return Err(SubjectError::InvalidInput);
        }
        for reference in &self.evidence {
            validate_id(reference)?;
        }
        Ok(())
    }

    /// 有价值才进入议程；该分数不是风险操作的审核结果。
    pub fn score(&self) -> i32 {
        i32::from(self.benefit) * i32::from(self.confidence)
            - 100 * (i32::from(self.cost) + i32::from(self.risk))
    }
}
