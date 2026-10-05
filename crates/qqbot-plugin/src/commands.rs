//! 宿主提供的同步命令扩展，不包含具体业务或模型调用。
use eve_plugin_api::{PluginError, PluginResult};
use eve_session_api::SessionKey;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// 已通过 QQ 帧校验的消息；会话身份由宿主绑定，不从正文解析。
/// session 同时隔离 AppID、C2C/群、目标和发送者，处理器不得缩减身份范围。
pub struct QqCommandInput<'a> {
    pub message_id: &'a str,
    pub session: &'a SessionKey,
    pub text: &'a str,
}

/// 可替换的宿主命令处理器。只做有界的同步状态查询或提交，不等待模型、工具或网络。
///
/// 通道先保存 Processing 回执，再调用一次；Some 是独立回复，None 继续原控制路径。
/// 返回 None 前不得产生业务副作用。已识别但关闭/参数无效的命令应返回明确提示，
/// 不能返回 None 后交给其他控制器。Err/panic 不回退或重放；State 错误关闭通道。
/// 业务保存与 QQ 回执之间没有跨插件事务，恢复后 Processing/ReplyPending 不自动重试。
pub trait QqCommandHandler: Send + Sync {
    fn handle(&self, input: QqCommandInput<'_>) -> PluginResult<Option<String>>;
}

pub(crate) fn dispatch(
    handler: &dyn QqCommandHandler,
    input: QqCommandInput<'_>,
) -> PluginResult<Option<String>> {
    if !crate::state::valid_message_id(input.message_id)
        || input.session.validate().is_err()
        || input.text.trim().is_empty()
        || input.text.len() > 32768
    {
        return Err(PluginError::Task("QQBot 命令输入无效".into()));
    }
    let reply = catch_unwind(AssertUnwindSafe(|| handler.handle(input)))
        // panic 可能发生在业务提交后；无法确认处理器状态时关闭通道。
        .map_err(|_| PluginError::State("QQBot 命令处理状态无法确认".into()))??;
    if reply
        .as_ref()
        .is_some_and(|text| text.trim().is_empty() || text.len() > 32768)
    {
        return Err(PluginError::Task("QQBot 命令回复无效或超过字节上限".into()));
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Message;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Default)]
    struct Capture {
        calls: AtomicUsize,
        inputs: Mutex<Vec<(String, SessionKey, String)>>,
    }
    impl QqCommandHandler for Capture {
        fn handle(&self, input: QqCommandInput<'_>) -> PluginResult<Option<String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inputs.lock().unwrap().push((
                input.message_id.into(),
                input.session.clone(),
                input.text.into(),
            ));
            Ok(None)
        }
    }
    struct Respond(Option<String>);
    impl QqCommandHandler for Respond {
        fn handle(&self, _: QqCommandInput<'_>) -> PluginResult<Option<String>> {
            Ok(self.0.clone())
        }
    }
    struct Failing {
        panic: bool,
        calls: AtomicUsize,
    }
    impl QqCommandHandler for Failing {
        fn handle(&self, _: QqCommandInput<'_>) -> PluginResult<Option<String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.panic {
                panic!("handler interrupted");
            }
            Err(PluginError::State("state unavailable".into()))
        }
    }
    fn input(session: &SessionKey) -> QqCommandInput<'_> {
        QqCommandInput {
            message_id: "ROBOT1.0_.opaque!",
            session,
            text: "/goals",
        }
    }

    #[test]
    fn forwards_full_validated_route_and_opaque_message_once_without_rewriting_text() {
        let handler = Capture::default();
        let message = Message {
            id: "ROBOT1.0_.opaque!".into(),
            scope: "group".into(),
            target_id: "group-1".into(),
            user_id: "user-1".into(),
            text: " /goal 第一行\n/cancel 是待办内容 ".into(),
        };
        let routes = [
            ("app-one", message.clone()),
            ("app-two", message.clone()),
            (
                "app-one",
                Message {
                    target_id: "group-2".into(),
                    ..message.clone()
                },
            ),
            (
                "app-one",
                Message {
                    user_id: "user-2".into(),
                    ..message.clone()
                },
            ),
            (
                "app-one",
                Message {
                    scope: "c2c".into(),
                    target_id: "user-1".into(),
                    ..message.clone()
                },
            ),
        ];
        let mut sessions = std::collections::BTreeSet::new();
        for (app, message) in routes {
            let session = message.session_key(app).unwrap();
            assert!(sessions.insert(session.session_id.clone()));
            assert!(
                dispatch(
                    &handler,
                    QqCommandInput {
                        message_id: &message.id,
                        session: &session,
                        text: &message.text,
                    }
                )
                .unwrap()
                .is_none()
            );
            let values = handler.inputs.lock().unwrap();
            let (id, bound, text) = values.last().unwrap();
            assert_eq!(id, &message.id);
            assert_eq!(bound, &session);
            assert_eq!(text, &message.text);
        }
        assert_eq!(handler.calls.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn invalid_input_never_invokes_the_host_handler() {
        let handler = Capture::default();
        let session = SessionKey::new("qq:bound", "qq:bound").unwrap();
        for id in ["", "bad\nid", " padded "] {
            assert!(
                dispatch(
                    &handler,
                    QqCommandInput {
                        message_id: id,
                        ..input(&session)
                    }
                )
                .is_err()
            );
        }
        for text in ["".into(), " \n ".into(), "x".repeat(32769)] {
            assert!(
                dispatch(
                    &handler,
                    QqCommandInput {
                        text: &text,
                        ..input(&session)
                    }
                )
                .is_err()
            );
        }
        let invalid_session = SessionKey {
            session_id: "".into(),
            user_id: "bound".into(),
        };
        assert!(dispatch(&handler, input(&invalid_session)).is_err());
        assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn reply_limit_counts_utf8_bytes_and_none_preserves_fallback() {
        let session = SessionKey::new("qq:bound", "qq:bound").unwrap();
        assert!(dispatch(&Respond(None), input(&session)).unwrap().is_none());
        let boundary = format!("{}aa", "好".repeat(10922));
        assert_eq!(boundary.len(), 32768);
        assert_eq!(
            dispatch(&Respond(Some(boundary.clone())), input(&session)).unwrap(),
            Some(boundary)
        );
        for reply in ["".into(), " \n ".into(), "好".repeat(10923)] {
            assert!(dispatch(&Respond(Some(reply)), input(&session)).is_err());
        }
    }

    #[test]
    fn error_and_panic_do_not_fall_back_or_repeat_the_handler() {
        let session = SessionKey::new("qq:bound", "qq:bound").unwrap();
        for panic in [false, true] {
            let handler = Failing {
                panic,
                calls: AtomicUsize::new(0),
            };
            assert!(matches!(
                dispatch(&handler, input(&session)),
                Err(PluginError::State(_))
            ));
            assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
        }
    }
}
