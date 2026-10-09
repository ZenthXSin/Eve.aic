//! 主动交流的通道契约：何时邀请、写什么由宿主决定；通道只在两个投递点取出邀请，
//! 并如实回报平台回执。
//!
//! - 被动窗口：普通私聊回复写好后，先由宿主判断此刻是否合适（看用户这条消息与回复），
//!   合适才把邀请作为最后一段随同一条消息的被动回复发出；群聊、命令确认与控制消息不附带。
//! - 主动私聊：宿主开启时，通道空闲后定期询问；路由只取自本通道已确认的私聊回执，
//!   宿主拿不到也不能指定 openid。
use eve_plugin_api::PluginResult;
use eve_session_api::SessionKey;
use std::{future::Future, ops::Range, pin::Pin, time::Duration};

/// 邀请正文上限。
pub const QQ_OUTREACH_MAX_BYTES: usize = 2048;
/// 附带邀请时回复本身至多 3 段：平台较新的单聊被动回复上限为每条消息 4 次。
pub const QQ_OUTREACH_MAX_REPLY_PARTS: usize = 3;
/// 邀请段前的停顿。
pub const QQ_OUTREACH_PAUSE_MS: u64 = 1200;
/// 通道等待时机判断的上限；超时、失败都当作此刻不合适，原回复照常投递。
pub const QQ_OUTREACH_JUDGE_TIMEOUT: Duration = Duration::from_secs(30);
/// 原回复没有分段时，回执中记录的规划器标识。
pub(crate) const OUTREACH_PLANNER: &str = "outreach-v1";

/// 宿主交给通道发送的一条邀请；`id` 由宿主分配，用于回报回执。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QqOutreachMessage {
    pub id: String,
    pub text: String,
}

/// 普通私聊回复写好、即将投递的时刻；正文只交给宿主的时机判断。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QqOutreachMoment {
    pub session: SessionKey,
    pub message_id: String,
    pub user_text: String,
    pub reply_text: String,
}

pub type QqOutreachFuture = Pin<Box<dyn Future<Output = PluginResult<bool>> + Send + 'static>>;

/// 主动私聊：会话的用户标识（与 `SessionKey::user_id` 相同）及邀请。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QqOutreachPush {
    pub user_id: String,
    pub message: QqOutreachMessage,
}

/// 平台对一次邀请投递的真实结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QqOutreachResult {
    Sent {
        platform_message_id: Option<String>,
    },
    /// 平台拒绝或发送失败，内容没有送达。
    Failed {
        http_status: Option<u16>,
        biz_code: Option<i64>,
    },
    /// 没有写出：前面的回复片段失败、回复已过期、正文不合规或没有可用的私聊路由。
    NotSent,
}

/// 由宿主显式接线。除 `judge` 外都是同步、有限的本地操作，不得发起模型、网络或嵌套通道任务。
///
/// `attach` 与 `next_push` 返回邀请即表示宿主已持久化这次投递尝试；之后通道一定以
/// `delivered` 回报结果，进程在写出前后退出时由宿主自行核对，通道不会补发。
/// 返回 Err 时通道记录警告并照常投递原回复。
pub trait QqOutreach: Send + Sync {
    /// 普通私聊回复即将投递时调用：此刻是否有一条可以为这个会话判断时机的邀请。
    fn due(&self, session: &SessionKey) -> PluginResult<bool>;
    /// 判断这次回复后是否适合附带邀请；宿主先持久化判断再发起至多一次模型请求。
    /// 通道至多等待 `QQ_OUTREACH_JUDGE_TIMEOUT`，期间不投递其他回复。
    fn judge(&self, moment: QqOutreachMoment) -> QqOutreachFuture;
    /// 判断为合适后调用：返回这次附带的邀请。
    fn attach(
        &self,
        session: &SessionKey,
        message_id: &str,
    ) -> PluginResult<Option<QqOutreachMessage>>;
    fn next_push(&self) -> PluginResult<Option<QqOutreachPush>>;
    fn delivered(&self, id: &str, result: QqOutreachResult) -> PluginResult<()>;
}

pub(crate) fn valid_text(text: &str) -> bool {
    !text.trim().is_empty() && text.trim() == text && text.len() <= QQ_OUTREACH_MAX_BYTES
}

/// 原回复的片段：字节范围与段前停顿。
pub(crate) type BaseParts = Vec<(Range<usize>, u64)>;

/// 附带邀请后的完整回复与片段。
pub(crate) struct Appended {
    pub text: String,
    pub planner: String,
    pub ranges: Vec<Range<usize>>,
    pub pauses: Vec<u64>,
    pub part: usize,
}

/// 把邀请作为最后一段附在回复后；原回复的片段与停顿不变。`base` 为原回复的规划器与片段，
/// 没有时整条回复（去掉首尾空白）为一段。结果不满足回执的片段规则时返回 None。
pub(crate) fn append(
    reply: &str,
    base: Option<(&str, BaseParts)>,
    invitation: &str,
) -> Option<Appended> {
    if !valid_text(invitation) {
        return None;
    }
    let (planner, mut parts) = match base {
        Some((planner, parts)) => (planner.to_string(), parts),
        None => {
            let start = reply.len() - reply.trim_start().len();
            let end = reply.trim_end().len();
            if start >= end {
                return None;
            }
            (OUTREACH_PLANNER.to_string(), vec![(start..end, 0)])
        }
    };
    if parts.is_empty() || parts.len() > QQ_OUTREACH_MAX_REPLY_PARTS {
        return None;
    }
    let mut text = String::with_capacity(reply.len() + 2 + invitation.len());
    text.push_str(reply);
    text.push_str("\n\n");
    let start = text.len();
    text.push_str(invitation);
    if text.len() > 32768 {
        return None;
    }
    parts.push((start..text.len(), QQ_OUTREACH_PAUSE_MS));
    eve_segment_api::validate_ranges(&text, parts.iter().map(|(range, _)| range.clone()), 32768)
        .ok()?;
    let part = parts.len() - 1;
    let (ranges, pauses) = parts.into_iter().unzip();
    Some(Appended {
        text,
        planner,
        ranges,
        pauses,
        part,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invitation_becomes_the_last_part_without_changing_the_reply() {
        let appended = append("  好的，我记下了。 ", None, "要不要一起做个新方块？").unwrap();
        assert_eq!(appended.planner, OUTREACH_PLANNER);
        assert_eq!(appended.part, 1);
        assert_eq!(
            &appended.text[appended.ranges[0].clone()],
            "好的，我记下了。"
        );
        assert_eq!(
            &appended.text[appended.ranges[1].clone()],
            "要不要一起做个新方块？"
        );
        assert_eq!(appended.pauses, vec![0, QQ_OUTREACH_PAUSE_MS]);

        let reply = "第一段。\n\n第二段。";
        let appended = append(
            reply,
            Some(("paragraph-v1", vec![(0..12, 0), (14..26, 800)])),
            "一起来？",
        )
        .unwrap();
        assert_eq!(appended.planner, "paragraph-v1");
        assert_eq!(appended.pauses, vec![0, 800, QQ_OUTREACH_PAUSE_MS]);
        assert_eq!(&appended.text[appended.ranges[2].clone()], "一起来？");
    }

    #[test]
    fn invalid_invitations_or_full_replies_are_not_appended() {
        assert!(append("好的", None, " 前后有空白 ").is_none());
        assert!(append("好的", None, &"长".repeat(QQ_OUTREACH_MAX_BYTES)).is_none());
        let full = (0..4).map(|i| (i * 3..i * 3 + 2, 0)).collect();
        assert!(append("甲\n乙\n丙\n丁", Some(("p", full)), "一起来？").is_none());
        // 未闭合的代码块会把邀请包进同一段，回执规则不允许；保持原回复。
        assert!(append("```\ncode", None, "一起来？").is_none());
        assert!(append(&"字".repeat(10_922), None, "一起来？").is_none());
    }
}
