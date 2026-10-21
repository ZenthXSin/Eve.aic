//! 文件投递的通道契约：发什么、从哪来由宿主决定；通道只负责取出一条请求，
//! 用平台 SDK 上传并如实回报结果。
//!
//! 路径在交给通道前已由宿主解析并校验（绝对路径、普通文件、大小上限），通道不拼接、
//! 不展开、不接受模型给出的路径。失败不重试。
use eve_plugin_api::PluginResult;
use eve_session_api::SessionKey;

/// 文件名上限（UTF-8 字节）。
pub const QQ_FILE_NAME_MAX_BYTES: usize = 255;
/// 说明文字上限，与一次进展消息同量级。
pub const QQ_FILE_CAPTION_MAX_BYTES: usize = 1024;
/// 传给 Node 的路径长度上限；实际长度远小于此。
pub const QQ_FILE_PATH_MAX_BYTES: usize = 4096;

/// 宿主交给通道的一条文件投递；`id` 由宿主分配，用于回报结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QqFileMessage {
    pub id: String,
    /// 已经确认过的会话；通道据此从自己的回执里取平台路由，不接受外部目标。
    pub session: SessionKey,
    /// 宿主已解析并校验过的绝对路径。
    pub path: String,
    pub name: Option<String>,
    pub caption: Option<String>,
}

/// 平台对一次文件投递的真实结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QqFileResult {
    Sent {
        platform_message_id: Option<String>,
    },
    /// 平台拒绝或上传失败，文件没有送达。
    Failed {
        http_status: Option<u16>,
        biz_code: Option<i64>,
    },
    /// 没有写出：找不到这个会话的已确认路由、参数不合规或通道已关闭。
    NotSent,
}

/// 由宿主显式接线；只做同步、有限的本地操作，不得发起模型或网络请求。
///
/// `next_file` 返回请求即表示宿主已持久化这次尝试；之后通道一定以 `delivered`
/// 回报结果。进程在写出前后退出时由宿主自行核对，通道不会补发。
pub trait QqFileDelivery: Send + Sync {
    fn next_file(&self) -> PluginResult<Option<QqFileMessage>>;
    fn delivered(&self, id: &str, result: QqFileResult) -> PluginResult<()>;
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= QQ_FILE_NAME_MAX_BYTES
        && name.trim() == name
        && !name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
}

pub fn valid_caption(caption: &str) -> bool {
    !caption.trim().is_empty()
        && caption.len() <= QQ_FILE_CAPTION_MAX_BYTES
        && !caption.chars().any(char::is_control)
}

pub fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= QQ_FILE_PATH_MAX_BYTES
        && path.starts_with('/')
        && !path
            .chars()
            .any(|c| c.is_control() || c == '\n' || c == '\r')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_never_carry_a_directory() {
        assert!(valid_name("报告.txt"));
        assert!(!valid_name(""));
        assert!(!valid_name("../etc/passwd"));
        assert!(!valid_name("a/b.txt"));
        assert!(!valid_name("a\\b.txt"));
        assert!(!valid_name("x\ny"));
        assert!(!valid_name(" dot "));
        assert!(!valid_name(&"x".repeat(QQ_FILE_NAME_MAX_BYTES + 1)));
    }

    #[test]
    fn captions_are_short_non_empty_text() {
        assert!(valid_caption("这是结果"));
        assert!(!valid_caption(""));
        assert!(!valid_caption("  "));
        assert!(!valid_caption("a\u{0}b"));
        assert!(!valid_caption(&"长".repeat(QQ_FILE_CAPTION_MAX_BYTES)));
    }

    #[test]
    fn paths_must_be_absolute_without_control_characters() {
        assert!(valid_path("/srv/outbox/a.txt"));
        assert!(!valid_path("a.txt"));
        assert!(!valid_path("./a.txt"));
        assert!(!valid_path(""));
        assert!(!valid_path("/srv/a\nb"));
        assert!(!valid_path(&format!(
            "/{}",
            "x".repeat(QQ_FILE_PATH_MAX_BYTES)
        )));
    }
}
