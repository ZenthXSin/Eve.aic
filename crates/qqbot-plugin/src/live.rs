//! 同一可信会话的轮内引导和主动交互；队列有界，结束后不接收、不重放。
use eve_llm_api::*;
use eve_plugin_api::{PluginError, PluginResult};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::oneshot;

type Key = (String, String);
fn key(scope: &ContextScope) -> Key {
    (scope.session_id.clone(), scope.user_id.clone())
}
fn unavailable() -> LlmError {
    LlmError::Context("实时交互队列不可用".into())
}

#[derive(Default)]
struct Input {
    accepting: bool,
    messages: Vec<String>,
    bytes: usize,
    accepted: usize,
}
struct Turn {
    id: u64,
    input: Mutex<Input>,
}
pub(crate) struct LiveSend {
    pub scope: ContextScope,
    pub turn_id: u64,
    pub text: String,
    pub needs_answer: bool,
    result: oneshot::Sender<Option<String>>,
}
impl LiveSend {
    pub fn cancelled(&self) -> bool {
        self.result.is_closed()
    }
    pub fn complete(self, message_id: Option<String>) {
        let _ = self.result.send(message_id);
    }
}

#[derive(Default)]
pub struct LiveDialogue {
    turns: Mutex<BTreeMap<Key, Weak<Turn>>>,
    outgoing: Mutex<VecDeque<LiveSend>>,
}
impl LiveDialogue {
    /// 持有入口锁时先保存平台回执，成功后才让运行时看见正文。
    pub(crate) fn steer(
        &self,
        scope: &ContextScope,
        turn_id: u64,
        text: String,
        persist: impl FnOnce() -> PluginResult<()>,
    ) -> PluginResult<bool> {
        let turn = self
            .turns
            .lock()
            .map_err(|_| PluginError::Task("交互队列锁不可用".into()))?
            .get(&key(scope))
            .and_then(Weak::upgrade);
        let Some(turn) = turn.filter(|turn| turn.id == turn_id) else {
            return Ok(false);
        };
        let mut input = turn
            .input
            .lock()
            .map_err(|_| PluginError::Task("引导队列锁不可用".into()))?;
        if !input.accepting || input.accepted >= 64 || input.bytes + text.len() > 256 * 1024 {
            return Ok(false);
        }
        persist()?;
        input.bytes += text.len();
        input.accepted += 1;
        input.messages.push(text);
        Ok(true)
    }
    pub(crate) fn next(&self) -> Option<LiveSend> {
        self.outgoing.lock().ok()?.pop_front()
    }
    pub fn send_tool(self: &Arc<Self>) -> Arc<dyn Tool> {
        Arc::new(SendMessage(self.clone()))
    }
}
impl TurnSteeringSource for LiveDialogue {
    fn open(&self, scope: &ContextScope, turn_id: u64) -> Result<Arc<dyn TurnSteering>, LlmError> {
        let mut turns = self.turns.lock().map_err(|_| unavailable())?;
        turns.retain(|_, turn| turn.strong_count() > 0);
        if turns.get(&key(scope)).and_then(Weak::upgrade).is_some() {
            return Err(unavailable());
        }
        let turn = Arc::new(Turn {
            id: turn_id,
            input: Mutex::new(Input {
                accepting: true,
                ..Input::default()
            }),
        });
        turns.insert(key(scope), Arc::downgrade(&turn));
        Ok(turn)
    }
}
impl TurnSteering for Turn {
    fn take(&self, finish: bool) -> Result<Vec<String>, LlmError> {
        let mut input = self.input.lock().map_err(|_| unavailable())?;
        if finish && input.messages.is_empty() {
            input.accepting = false;
        }
        Ok(std::mem::take(&mut input.messages))
    }
}

struct SendMessage(Arc<LiveDialogue>);
impl Tool for SendMessage {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "send_message".into(),
            description: "在当前会话立即说一句话，不结束当前轮，也不停止正在推进的任务。多步任务在私聊开始、关键节点、长等待时用自己的话告知真实进展，结尾用最终答复；群聊只发开始、一个中间节点与结尾。必要授权或问题独立使用 purpose=question 或 authorization，不计作普通进度；用户中途回话会直接加入本轮引导，下一次模型请求可见。不要播报调用次数、工具名或每个琐碎步骤，不反复说正在处理，不把未完成的事说成完成，不重复最终答复。一次至多 250 字，受 QQ 剩余消息配额限制；返回 sent=true 且有 message_id、session_id 才确认送达。失败或结果未知不得自动重发。只发给当前会话，不接受目标地址。需要答案时继续推进其他独立工作，不能假定用户已回答；调用本工具不会终止主执行循环。".into(),
            argument_schema: json!({"type":"object","properties":{"text":{"type":"string","minLength":1,"maxLength":250},"purpose":{"type":"string","enum":["progress","question","authorization"]}},"required":["text"],"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial { scope: "eve.qq.live-send".into() }),
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        if value
            .as_object()
            .is_none_or(|v| v.keys().any(|k| k != "text" && k != "purpose"))
            || value.get("purpose").is_some_and(|p| {
                !matches!(p.as_str(), Some("progress" | "question" | "authorization"))
            })
            || value
                .get("text")
                .and_then(Value::as_str)
                .is_none_or(|text| {
                    text.trim().is_empty()
                        || text.chars().count() > 250
                        || text
                            .chars()
                            .any(|c| c.is_control() && c != '\n' && c != '\t')
                })
        {
            return Err(ToolValidationError {
                message: "只接受非空 text，最多 250 字".into(),
            });
        }
        Ok(())
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            let rejected = || ToolExecutionError::Rejected("当前没有可投递的对话轮次".into());
            let scope = context.scope().cloned().ok_or_else(rejected)?;
            let turn_id = context.turn_id().ok_or_else(rejected)?;
            let turn = self
                .0
                .turns
                .lock()
                .map_err(|_| rejected())?
                .get(&key(&scope))
                .and_then(Weak::upgrade)
                .filter(|t| t.id == turn_id)
                .ok_or_else(rejected)?;
            if !turn.input.lock().map_err(|_| rejected())?.accepting {
                return Err(rejected());
            }
            let session_id = scope.session_id.clone();
            let (sender, receiver) = oneshot::channel();
            {
                let mut queue = self.0.outgoing.lock().map_err(|_| rejected())?;
                if queue.len() >= 64 {
                    return Err(rejected());
                }
                queue.push_back(LiveSend {
                    scope,
                    turn_id,
                    text: call.arguments["text"]
                        .as_str()
                        .ok_or_else(rejected)?
                        .trim()
                        .into(),
                    needs_answer: matches!(
                        call.arguments.get("purpose").and_then(Value::as_str),
                        Some("question" | "authorization")
                    ),
                    result: sender,
                });
            }
            let message_id = receiver
                .await
                .map_err(|_| ToolExecutionError::Failed("消息投递结果未知，不要重发".into()))?;
            let sent = message_id.is_some();
            Ok(
                json!({"sent":sent,"message_id":message_id,"session_id":session_id,"delivery_status":if sent {"sent"} else {"unknown"},"note":if sent {"已送达，继续当前任务；不要在最终答复中重复这句话"} else {"未确认送达或剩余配额不足，不要重发；可在最终答复中说明必要内容"}}),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn question_send_stays_in_live_turn_and_success_returns_both_bound_ids() {
        let live = Arc::new(LiveDialogue::default());
        let scope = ContextScope {
            session_id: "trusted-session".into(),
            user_id: "owner".into(),
        };
        let turn = live.open(&scope, 7).unwrap();
        let tool = live.send_tool();
        let call = ToolCall {
            id: "send-1".into(),
            name: "send_message".into(),
            arguments: json!({"text":"这个目录需要保留吗？", "purpose":"question"}),
        };
        tool.validate_arguments(&call.arguments).unwrap();
        let context = ToolExecutionContext::new()
            .with_scope(Some(scope.clone()))
            .with_turn_id(Some(7));
        let sending = tokio::spawn(async move { tool.execute(call, context).await });
        let outgoing = loop {
            if let Some(outgoing) = live.next() {
                break outgoing;
            }
            tokio::task::yield_now().await;
        };
        assert!(outgoing.needs_answer);
        assert_eq!(outgoing.scope, scope);
        outgoing.complete(Some("platform-message-7".into()));
        let result = sending.await.unwrap().unwrap();
        assert_eq!(result["sent"], true);
        assert_eq!(result["message_id"], "platform-message-7");
        assert_eq!(result["session_id"], "trusted-session");
        assert!(live.steer(&scope, 7, "保留".into(), || Ok(())).unwrap());
        assert_eq!(turn.take(false).unwrap(), ["保留"]);
        let foreign = ToolExecutionContext::new()
            .with_scope(Some(ContextScope {
                user_id: "stranger".into(),
                ..scope
            }))
            .with_turn_id(Some(7));
        let tool = live.send_tool();
        let call = ToolCall {
            id: "send-2".into(),
            name: "send_message".into(),
            arguments: json!({"text":"越权"}),
        };
        assert!(tool.execute(call, foreign).await.is_err());
    }

    #[test]
    fn steering_is_isolated_durable_before_visible_and_atomically_sealed() {
        let live = LiveDialogue::default();
        let owner = ContextScope {
            session_id: "s".into(),
            user_id: "owner".into(),
        };
        let other = ContextScope {
            user_id: "other".into(),
            ..owner.clone()
        };
        let turn = live.open(&owner, 1).unwrap();
        assert!(
            !live
                .steer(&other, 1, "attack".into(), || panic!("foreign persist"))
                .unwrap()
        );
        assert!(
            !live
                .steer(&owner, 2, "stale".into(), || panic!("stale persist"))
                .unwrap()
        );
        assert!(
            live.steer(&owner, 1, "not saved".into(), || Err(PluginError::State(
                "disk failure".into()
            )))
            .is_err()
        );
        assert!(turn.take(false).unwrap().is_empty());
        assert!(live.steer(&owner, 1, "先备份".into(), || Ok(())).unwrap());
        assert_eq!(turn.take(true).unwrap(), ["先备份"]);
        assert!(live.steer(&owner, 1, "继续处理".into(), || Ok(())).unwrap());
        assert_eq!(turn.take(false).unwrap(), ["继续处理"]);
        assert!(turn.take(true).unwrap().is_empty());
        assert!(
            !live
                .steer(&owner, 1, "too late".into(), || panic!("closed persist"))
                .unwrap()
        );
        drop(turn);
        let next = live.open(&owner, 2).unwrap();
        assert!(next.take(false).unwrap().is_empty());
    }
}
