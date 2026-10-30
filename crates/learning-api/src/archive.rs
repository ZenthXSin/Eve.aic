use crate::LearningResult;

/// 不可变、内容寻址的学习记录存储。键为正文 SHA256 的小写十六进制。
/// 已存在内容必须逐字相同；错误不能伪装成不存在。宿主单独提供该能力。
pub trait LearningArchiveStore: Send + Sync {
    fn put(&self, key: &str, bytes: &[u8]) -> LearningResult<()>;
    fn get(&self, key: &str) -> LearningResult<Vec<u8>>;
}

/// 单条批次（含输入、候选和补做记录）或决策的冷文件上限。
pub const MAX_ARCHIVE_RECORD_BYTES: usize = 128 * 1024;
/// 兼容全历史读取期间，每本账本最多恢复的序列化正文；避免恶意索引无界分配。
/// 后续分页接口可解除单次全历史恢复的约束，不能把此值当成进程 RSS 上限。
pub const MAX_ARCHIVE_RESTORE_BYTES: usize = 128 * 1024 * 1024;
