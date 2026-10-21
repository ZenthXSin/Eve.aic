//! 本轮用户消息附带的图像输入。
//!
//! 图像只随本轮模型请求在内存中传递：`ChatMessage::images` 不参与序列化，
//! 会话历史、检查点、压缩与子代理上下文都不会写入像素。
use crate::{ContextScope, LlmError, LlmFuture, TurnInput};
use std::{fmt, sync::Arc};

/// 单轮至多附带的图像数。
pub const MAX_TURN_IMAGES: usize = 4;
/// 单张图像的字节上限。
pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

/// 已核对格式的图像字节；媒体类型由文件头判定，不信任来源声明。
#[derive(Clone, PartialEq, Eq)]
pub struct ImageInput {
    media_type: &'static str,
    data: Arc<[u8]>,
}

impl ImageInput {
    /// 只接受 PNG、JPEG、GIF 与 WebP，且不超过 `MAX_IMAGE_BYTES`。
    pub fn new(data: impl Into<Arc<[u8]>>) -> Result<Self, LlmError> {
        let data = data.into();
        if data.len() > MAX_IMAGE_BYTES {
            return Err(LlmError::Protocol("图像超过大小上限".into()));
        }
        let media_type =
            sniff_image(&data).ok_or_else(|| LlmError::Protocol("不是受支持的图像格式".into()))?;
        Ok(Self { media_type, data })
    }

    pub fn media_type(&self) -> &'static str {
        self.media_type
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

impl fmt::Debug for ImageInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageInput")
            .field("media_type", &self.media_type)
            .field("bytes", &self.data.len())
            .finish()
    }
}

/// 按文件头识别受支持的图像格式。
pub fn sniff_image(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// 图像来源为本轮准备的输入：附在用户消息上的图像，以及给模型的宿主说明
/// （哪些图像已附上、哪些因何未附上）。说明是宿主数据，不是用户原话。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnImages {
    pub images: Vec<ImageInput>,
    pub notes: Vec<String>,
}

/// 为本轮准备图像输入；运行时每轮至多调用一次，结果只用于本轮请求。
///
/// 返回错误时运行时只告诉模型图像未能附上，本轮照常进行。
pub trait TurnImageSource: Send + Sync {
    fn images<'a>(
        &'a self,
        scope: Option<&'a ContextScope>,
        input: &'a TurnInput,
    ) -> LlmFuture<'a, TurnImages>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_supported_formats_and_rejects_others_and_oversize() {
        let png = ImageInput::new(b"\x89PNG\r\n\x1a\nrest".to_vec()).unwrap();
        assert_eq!(png.media_type(), "image/png");
        assert_eq!(sniff_image(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(sniff_image(b"GIF89a.."), Some("image/gif"));
        assert_eq!(sniff_image(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert!(ImageInput::new(b"<svg/>".to_vec()).is_err());
        let mut big = b"\x89PNG\r\n\x1a\n".to_vec();
        big.resize(MAX_IMAGE_BYTES + 1, 0);
        assert!(ImageInput::new(big).is_err());
        assert!(format!("{png:?}").contains("bytes: 12"));
    }
}
