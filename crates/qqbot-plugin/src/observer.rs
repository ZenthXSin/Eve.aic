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
/// 仅普通消息成功投递后调用，不包括命令、训练发起轮、控制替代轮、工具内部消息或失败轮。
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
            || messages.first().and_then(|first| first.text.as_deref()) != Some(&message.text)
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

#[cfg(test)]
mod tests {
    use super::*;
    use eve_control_api::{RunFailure, RunReport};
    use eve_llm_api::{ChatMessage, ChatRole, LlmError};
    use eve_session_api::{
        SessionError, SessionFailure, SessionInput, SessionResult, SessionSnapshot, SessionTurn,
        StartedTurn, TurnLease,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Snapshot(Option<SessionSnapshot>);
    impl SessionService for Snapshot {
        fn snapshot(&self, _: &SessionKey) -> SessionResult<Option<SessionSnapshot>> {
            Ok(self.0.clone())
        }
        fn begin(&self, _: SessionInput) -> SessionResult<StartedTurn> {
            Err(SessionError::Unavailable)
        }
        fn complete(&self, _: &TurnLease, _: Vec<ChatMessage>) -> SessionResult<()> {
            Err(SessionError::Unavailable)
        }
        fn fail(&self, _: &TurnLease, _: SessionFailure) -> SessionResult<()> {
            Err(SessionError::Unavailable)
        }
    }

    fn fixture() -> (Message, ControlReport, SessionSnapshot) {
        let message = Message {
            id: "opaque+/平台消息=".into(),
            scope: "c2c".into(),
            target_id: "alice".into(),
            user_id: "alice".into(),
            text: "用户原文".into(),
        };
        let key = message.session_key("app").unwrap();
        let messages = vec![
            ChatMessage::text(ChatRole::User, &message.text),
            ChatMessage::text(ChatRole::Assistant, "模型回复"),
        ];
        let report = ControlReport {
            key: GenerationKey {
                session: key.clone(),
                task_id: format!("qq:{}", message.id),
                controller_epoch: [1; 16],
                generation: 1,
            },
            cancel_requested: false,
            run: RunReport {
                turn_id: Some(1),
                commit: CommitState::Completed,
                text: Some("模型回复".into()),
                transcript: Some(messages.clone()),
                started_tools: Some(0),
                tool_results: vec![],
                failure: None,
            },
        };
        let snapshot = SessionSnapshot {
            key,
            revision: 2,
            turns: vec![SessionTurn {
                id: 1,
                input: message.text.clone(),
                status: SessionTurnStatus::Completed { messages },
            }],
        };
        (message, report, snapshot)
    }

    #[test]
    fn exact_committed_turn_is_required() {
        let (message, report, snapshot) = fixture();
        let capture = |r: &ControlReport, s: Option<SessionSnapshot>| {
            CompletedInteraction::capture("app", &message, &report.key, r, &Snapshot(s))
        };
        assert!(capture(&report, Some(snapshot.clone())).unwrap().is_some());
        assert!(capture(&report, None).is_err());
        for alter in [
            |r: &mut ControlReport| r.key.controller_epoch = [2; 16],
            |r: &mut ControlReport| r.key.generation += 1,
            |r: &mut ControlReport| r.key.task_id = "qq:another".into(),
            |r: &mut ControlReport| r.key.session.user_id = "another".into(),
            |r: &mut ControlReport| r.run.turn_id = None,
            |r: &mut ControlReport| r.run.turn_id = Some(2),
            |r: &mut ControlReport| r.run.commit = CommitState::Pending,
            |r: &mut ControlReport| r.run.text = Some("未提交的回复".into()),
            |r: &mut ControlReport| {
                r.run.failure = Some(RunFailure::Execution(LlmError::Cancelled));
            },
        ] {
            let mut changed = report.clone();
            alter(&mut changed);
            assert!(capture(&changed, Some(snapshot.clone())).is_err());
        }
        for alter in [
            |s: &mut SessionSnapshot| s.key.user_id = "another".into(),
            |s: &mut SessionSnapshot| s.revision += 1,
            |s: &mut SessionSnapshot| s.turns[0].id = 2,
            |s: &mut SessionSnapshot| s.turns[0].status = SessionTurnStatus::Interrupted,
            |s: &mut SessionSnapshot| {
                s.turns[0].input = "不同的用户输入".into();
                if let SessionTurnStatus::Completed { messages } = &mut s.turns[0].status {
                    messages[0].text = Some("不同的用户输入".into());
                }
            },
            |s: &mut SessionSnapshot| {
                if let SessionTurnStatus::Completed { messages } = &mut s.turns[0].status {
                    messages[1].text = Some("不同的模型回复".into());
                }
            },
            |s: &mut SessionSnapshot| {
                if let SessionTurnStatus::Completed { messages } = &mut s.turns[0].status {
                    messages[1].role = ChatRole::User;
                }
            },
        ] {
            let mut changed = snapshot.clone();
            alter(&mut changed);
            assert!(capture(&report, Some(changed)).is_err());
        }
    }

    #[test]
    fn source_cannot_cross_app_group_or_sender() {
        let (message, report, snapshot) = fixture();
        for (app, scope, target, user) in [
            ("other_app", "c2c", "alice", "alice"),
            ("app", "group", "alice", "alice"),
            ("app", "group", "group_b", "alice"),
            ("app", "c2c", "bob", "bob"),
        ] {
            let mut changed = message.clone();
            changed.scope = scope.into();
            changed.target_id = target.into();
            changed.user_id = user.into();
            assert!(
                CompletedInteraction::capture(
                    app,
                    &changed,
                    &report.key,
                    &report,
                    &Snapshot(Some(snapshot.clone())),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn commands_and_cancelled_generations_are_not_observed() {
        let (mut message, mut report, _) = fixture();
        report.cancel_requested = true;
        assert!(
            CompletedInteraction::capture("app", &message, &report.key, &report, &Snapshot(None))
                .unwrap()
                .is_none()
        );
        report.cancel_requested = false;
        for text in ["/new 替代任务", "/train start", "说明\n  /cancel"] {
            message.text = text.into();
            assert!(
                CompletedInteraction::capture(
                    "app",
                    &message,
                    &report.key,
                    &report,
                    &Snapshot(None)
                )
                .unwrap()
                .is_none()
            );
        }
    }

    struct BrokenObserver {
        calls: AtomicUsize,
        panic: bool,
    }
    impl QqInteractionObserver for BrokenObserver {
        fn observe(&self, _: &QqInteraction<'_>) -> PluginResult<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panic, "opaque panic detail");
            Err(PluginError::State("opaque backend detail".into()))
        }
    }
    #[test]
    fn observer_error_and_panic_are_contained_without_retry() {
        let (message, report, snapshot) = fixture();
        let captured = CompletedInteraction::capture(
            "app",
            &message,
            &report.key,
            &report,
            &Snapshot(Some(snapshot)),
        )
        .unwrap()
        .unwrap();
        for panic in [false, true] {
            let observer = BrokenObserver {
                calls: AtomicUsize::new(0),
                panic,
            };
            let error = captured
                .observe(&observer, "app", &message, "模型回复")
                .unwrap_err();
            assert!(!error.to_string().contains("opaque"));
            assert_eq!(observer.calls.load(Ordering::SeqCst), 1);
        }
    }
}
