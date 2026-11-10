//! 已消费观察批次的不可变原件与按范围读取；归档不会重试观察或把失败改成成功。
use crate::{InterestError, InterestResult, InterestSnapshot, ObservationJob};
use serde::{Deserialize, Serialize};
use std::fmt;

/// 热任务和归档引用合计上限；达到后保留历史，不自动清空。
pub const MAX_JOB_REFERENCES: usize = 4096;
pub const MAX_JOB_ARCHIVE_RECORD_BYTES: usize = 128 * 1024;
pub const MAX_JOB_PAGE: usize = 20;

/// 由可信宿主显式装配。hash 为原始 bytes 的 64 位小写 SHA256。
/// put 成功意味着完整字节已持久；同摘要已存在时必须核对原件一致，不能覆盖异文。
/// get 缺块、超限或摘要不匹配必须失败；不以空页或空字节掩盖损坏。
/// 无删除接口，提交未知时遗留的未引用原件也保留。错误不可携带原文或凭据。
pub trait InterestJobArchiveStore: Send + Sync {
    fn put(&self, hash: &str, bytes: &[u8]) -> InterestResult<()>;
    fn get(&self, hash: &str) -> InterestResult<Vec<u8>>;
    /// 仅供缺失根状态时初始化新库：必须证明没有任何历史或提交未知的原件。
    /// 已知基础设施文件可排除；未引用正文、临时文件和未知内容都不能当作空库。
    /// 无法作出可靠判断的替换实现明确拒绝，不能靠未实现接口自动创建空根。
    fn is_empty(&self) -> InterestResult<bool> {
        Err(InterestError::Unavailable)
    }
}

/// 当前范围的热层与归档任务合并页，按宿主持久序号递增；after_sequence=0 从头读。
/// next_after 为最后返回任务的序号；None 表示本次索引已无后续记录。
/// 归档移动不改变序号。追加的新任务可在后续页出现，已返回序号不回退或重用。
/// 页本身只含同范围历史，不增加模型投影；调用方仍负责可信身份和原授权范围。
#[derive(Clone, Eq, PartialEq)]
pub struct InterestJobPage {
    pub jobs: Vec<ObservationJob>,
    pub next_after: Option<u64>,
}
impl fmt::Debug for InterestJobPage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InterestJobPage(<redacted>)")
    }
}

/// 全局容量计数，只供可信宿主诊断，不含范围、消息、批次正文或磁盘路径。
/// 未启用归档的替换实现默认返回 enabled=false；其他零值不是已核实全局统计。
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterestArchiveStatus {
    pub enabled: bool,
    pub hot_jobs: usize,
    pub archived_jobs: usize,
    pub archived_bytes: u64,
    pub index_bytes: usize,
}

pub fn validate_job_page_limit(limit: usize) -> InterestResult<()> {
    if limit == 0 || limit > MAX_JOB_PAGE {
        return Err(InterestError::InvalidInput);
    }
    Ok(())
}

/// 老替换实现仅有完整有界快照时采用本地序号；有冷历史时必须实现真正分页。
pub(crate) fn snapshot_page(
    snapshot: InterestSnapshot,
    after_sequence: u64,
    limit: usize,
) -> InterestResult<InterestJobPage> {
    validate_job_page_limit(limit)?;
    if snapshot.archived_jobs != 0 {
        return Err(InterestError::Unavailable);
    }
    if snapshot.jobs.len() > crate::MAX_JOBS
        || snapshot
            .jobs
            .iter()
            .any(|job| job.batch.scope != snapshot.scope)
    {
        return Err(InterestError::CorruptState);
    }
    let start = usize::try_from(after_sequence)
        .unwrap_or(usize::MAX)
        .min(snapshot.jobs.len());
    let end = start.saturating_add(limit).min(snapshot.jobs.len());
    Ok(InterestJobPage {
        next_after: (end < snapshot.jobs.len()).then_some(end as u64),
        jobs: snapshot.jobs.into_iter().skip(start).take(limit).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_limits_and_archived_default_fail_explicitly() {
        assert!(validate_job_page_limit(0).is_err());
        assert!(validate_job_page_limit(MAX_JOB_PAGE + 1).is_err());
        assert!(validate_job_page_limit(MAX_JOB_PAGE).is_ok());
        let snapshot = InterestSnapshot {
            scope: eve_memory_api::MemoryScope {
                channel: "qq".into(),
                session_id: "s".into(),
                user_id: "u".into(),
            },
            interests: vec![],
            jobs: vec![],
            archived_jobs: 1,
        };
        assert_eq!(
            snapshot_page(snapshot, 0, 1).err(),
            Some(InterestError::Unavailable)
        );
        assert!(!InterestArchiveStatus::default().enabled);
    }
}
