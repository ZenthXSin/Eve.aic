//! 表情包的公开契约：从聊天里收到的图片中收集表情包，经审看后按心情挑选发送。
//! 契约不绑定聊天平台；图片字节由实现保存在自己的目录里，调用方只拿到只读路径。
use eve_llm_api::{ImageInput, LlmFuture};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 收集的单张图片上限；更大的多半是照片或截图，不收。
pub const MAX_STICKER_BYTES: usize = 512 * 1024;
/// 已收下的表情包上限；满了不再收集新图。
pub const MAX_STICKERS: usize = 500;
/// 待审看的图片上限；满了先不收，等审看腾出位置。
pub const MAX_PENDING: usize = 20;

/// 挑选表情包时用的心情；审看器只能从这里选。
pub const MOODS: [&str; 10] = [
    "开心", "赞同", "无语", "惊讶", "难过", "生气", "害羞", "疑惑", "加油", "晚安",
];

pub fn valid_mood(mood: &str) -> bool {
    MOODS.contains(&mood)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StickerState {
    /// 已保存字节，等待审看。
    Pending,
    Approved,
    /// 审看认为不是表情包或不适合发送；字节已删除，只留摘要避免重复审看。
    Rejected,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sticker {
    pub id: String,
    /// 图片字节的 SHA-256；同一张图只收一次。
    pub sha256: String,
    pub media_type: String,
    pub bytes: usize,
    pub state: StickerState,
    /// 审看器对图片的简短描述；审看前为空。
    pub description: String,
    pub moods: Vec<String>,
    pub uses: u64,
    pub added_at_ms: u64,
}

/// 审看结论：是不是适合在聊天里发的表情包，以及描述和心情。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StickerReview {
    pub sticker: bool,
    pub description: String,
    pub moods: Vec<String>,
}

/// 可替换的审看器：只看这一张图，不调用工具，不读写其他状态。
pub trait StickerReviewer: Send + Sync {
    fn review(&self, image: ImageInput) -> LlmFuture<'_, StickerReview>;
}

/// 一张挑好的表情包：本地只读路径，交给渠道上传。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StickerChoice {
    pub id: String,
    pub path: PathBuf,
    pub description: String,
}

pub type StickerResult<T> = Result<T, StickerError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StickerError {
    /// 状态损坏或版本不兼容；不清空原文件。
    Corrupt(String),
    Storage(String),
    Invalid(String),
}
impl std::fmt::Display for StickerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Corrupt(m) | Self::Storage(m) | Self::Invalid(m) => f.write_str(m),
        }
    }
}
impl std::error::Error for StickerError {}

/// 表情包库。所有变更先写盘再生效。
pub trait StickerLibrary: Send + Sync {
    /// 收下一张候选图片；重复、超限、库满或待审看已满时不收，返回 None。
    fn offer(&self, image: &ImageInput, now_ms: u64) -> StickerResult<Option<String>>;
    /// 取一张待审看的图片；没有时为 None。
    fn next_pending(&self) -> StickerResult<Option<(String, ImageInput)>>;
    /// 写入审看结论；不是表情包时删除字节。
    fn finish_review(&self, id: &str, review: &StickerReview) -> StickerResult<()>;
    /// 按心情挑一张已收下的表情包（用得少的优先），并记一次使用。
    fn pick(&self, mood: &str) -> StickerResult<Option<StickerChoice>>;
    /// 当前有表情包可选的心情。
    fn moods(&self) -> StickerResult<Vec<String>>;
    fn list(&self) -> StickerResult<Vec<Sticker>>;
    /// 删除一张表情包及其字节；不存在时返回 false。
    fn forget(&self, id: &str) -> StickerResult<bool>;
}
