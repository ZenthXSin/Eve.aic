//! 本机 Web 收件箱。保存成功只表示本机持久收录；已读由用户显式打开确认。
use crate::PanelResult;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    pub id: u64,
    pub session_id: String,
    pub source_id: String,
    /// task、trigger 或 progress。
    pub kind: String,
    pub status: String,
    pub title: String,
    pub body: String,
    pub truncated: bool,
    /// 收件箱实际收录的时间；不是浏览器收到或阅读的时间。
    pub created_at_ms: u64,
    pub read_at_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct NotificationPage {
    pub items: Vec<Notification>,
    pub next_before: Option<u64>,
    pub unread: usize,
}

/// 实现必须恢复已保存正文与已读状态；损坏、读取失败不能当作空收件箱。
/// 每页 1..=100 条，新的在前；已读不是任务已完成、授权或平台送达的凭证。
pub trait WebInbox: Send + Sync {
    fn list(&self, before: Option<u64>, limit: usize) -> PanelResult<NotificationPage>;
    fn mark_read(&self, id: u64, now_ms: u64) -> PanelResult<bool>;
}
