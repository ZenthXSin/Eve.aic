//! 由受信宿主替换的成功交互观察契约；不包含记忆实现或模型调用。
use crate::state::Message;
use eve_control_api::{CommitState, ControlReport, GenerationKey};
use eve_plugin_api::{PluginError, PluginResult};
use eve_session_api::{SessionKey, SessionService, SessionTurnStatus};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
};

/// QQ 已验证的消息范围；用户正文或模型输出不能选择此来源。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QqInteractionSource {
    DirectMessage,
    GroupMessage,
}

/// Session 已提交、QQ 已确认投递且 Sent 已保存的普通交互。
///
/// 只能由通道构造；没有公开构造函数、可写字段或反序列化入口。
/// 范围沿用通道的 AppID/私聊或群/目标/发送者隔离，不是跨会话授权。
/// 不提供平台消息时间；观察者自己的时钟只能标记观察时间。
pub struct QqInteraction<'a> {
    app_id: &'a str,
    source: QqInteractionSource,
    session: &'a SessionKey,
    message_id: &'a str,
    turn_id: u64,
    user_text: &'a str,
    assistant_text: &'a str,
}
impl QqInteraction<'_> {
    pub fn app_id(&self) -> &str {
        self.app_id
    }
    pub fn source(&self) -> QqInteractionSource {
        self.source
    }
    pub fn session(&self) -> &SessionKey {
        self.session
    }
    pub fn message_id(&self) -> &str {
        self.message_id
    }
    pub fn turn_id(&self) -> u64 {
        self.turn_id
    }
    pub fn user_text(&self) -> &str {
        self.user_text
    }
    pub fn assistant_text(&self) -> &str {
        self.assistant_text
    }
}

/// 由宿主显式接线的同步、有限本地操作；不得发起模型、网络或嵌套通道任务。
///
/// 仅普通消息成功投递后调用，不包括命令、控制替代轮、工具内部消息或失败轮。
/// Err/panic 后通道保留 Sent，记录警告并关闭，绝不自动再调。
/// 首版不在重启时补采；观察持久化与 QQ 回执不构成跨服务事务。
pub trait QqInteractionObserver: Send + Sync {
    fn observe(&self, interaction: &QqInteraction<'_>) -> PluginResult<()>;
}

pub(crate) struct Observation {
    pub observer: Arc<dyn QqInteractionObserver>,
    pub sessions: Arc<dyn SessionService>,
}

/// 仅保存发送前核对的会话凭据，正文继续使用同一个 Reply，避免回调文本漂移。
pub(crate) struct CompletedInteraction {
    session: SessionKey,
    turn_id: u64,
}
impl CompletedInteraction {
    pub fn capture(
        app_id: &str,
        message: &Message,
        expected: &GenerationKey,
        report: &ControlReport,
        sessions: &dyn SessionService,
    ) -> PluginResult<Option<Self>> {
        // 控制消息可以产生模型轮次，但原输入可能是路由组合文本，本版不导入。
        if message
            .text
            .lines()
            .any(|line| line.trim_start().starts_with('/'))
            || report.cancel_requested
        {
            return Ok(None);
        }
        let invalid = || PluginError::State("QQBot 成功交互提交凭据不匹配".into());
        let session = message.session_key(app_id)?;
        if report.key != *expected
            || report.key.session != session
            || report.key.task_id != format!("qq:{}", message.id)
            || report.run.commit != CommitState::Completed
            || report.run.failure.is_some()
        {
            return Err(invalid());
        }
        let turn_id = report.run.turn_id.ok_or_else(invalid)?;
        let text = report.run.text.as_deref().ok_or_else(invalid)?;
        let snapshot = sessions
            .snapshot(&session)
            .map_err(|_| invalid())?
            .ok_or_else(invalid)?;
        if snapshot.key != session || snapshot.validate().is_err() {
            return Err(invalid());
        }
        let turn = snapshot
            .turns
            .iter()
            .find(|turn| turn.id == turn_id)
            .ok_or_else(invalid)?;
        let SessionTurnStatus::Completed { messages } = &turn.status else {
            return Err(invalid());
        };
        if turn.input != message.text
            || messages.last().and_then(|last| last.text.as_deref()) != Some(text)
        {
            return Err(invalid());
        }
        Ok(Some(Self { session, turn_id }))
    }

    /// 仅在匹配 delivery.ok=true 且 Sent 保存成功后调用。
    pub fn observe(
        &self,
        observer: &dyn QqInteractionObserver,
        app_id: &str,
        message: &Message,
        text: &str,
    ) -> PluginResult<()> {
        let interaction = QqInteraction {
            app_id,
            source: if message.scope == "c2c" {
                QqInteractionSource::DirectMessage
            } else {
                QqInteractionSource::GroupMessage
            },
            session: &self.session,
            message_id: &message.id,
            turn_id: self.turn_id,
            user_text: &message.text,
            assistant_text: text,
        };
        catch_unwind(AssertUnwindSafe(|| observer.observe(&interaction)))
            .map_err(|_| PluginError::State("QQBot 交互观察者异常".into()))?
            .map_err(|_| PluginError::State("QQBot 交互观察失败；Sent 已保留且未重试".into()))
    }
}
