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
    result: oneshot::Sender<bool>,
}
impl LiveSend {
    pub fn cancelled(&self) -> bool {
        self.result.is_closed()
    }
    pub fn complete(self, sent: bool) {
        let _ = self.result.send(sent);
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
            description: "在当前会话立即说一句话，不结束当前轮，也不停止正在推进的任务。用于值得立即告知的真实进展、关键发现、短回应，或需要用户补充的具体问题；用户中途回话会直接加入本轮引导，下一次模型请求可见。不要播报调用次数、工具名或每个琐碎步骤，不反复说正在处理，不把未完成的事说成完成，不重复最终答复。一次至多 250 字，受 QQ 剩余消息配额限制；返回 sent=true 才确认送达。失败或结果未知不得自动重发。只发给当前会话，不接受目标地址。需要答案时可以先推进其他独立工作，不能假定用户已回答。".into(),
            argument_schema: json!({"type":"object","properties":{"text":{"type":"string","minLength":1,"maxLength":250}},"required":["text"],"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial { scope: "eve.qq.live-send".into() }),
        }
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        if value.as_object().is_none_or(|v| v.len() != 1)
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
                    result: sender,
                });
            }
            let sent = receiver
                .await
                .map_err(|_| ToolExecutionError::Failed("消息投递结果未知，不要重发".into()))?;
            Ok(
                json!({"sent":sent,"note":if sent {"已送达，继续当前任务；不要在最终答复中重复这句话"} else {"未确认送达或剩余配额不足，不要重发；可在最终答复中说明必要内容"}}),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
