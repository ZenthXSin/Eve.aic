//! 主轮等待工具结果时的只读交流事实；业务输入仍由原轮的 steering 消费。
use super::{activities, now_ms};
use crate::{LiveDialogue, QqSpeechEvent};
use eve_control_api::{ControlEvent, ControlPhase, ControlService, GenerationKey};
use eve_llm_api::{ContextScope, TurnEventKind};
use serde_json::json;
use std::{collections::BTreeMap, sync::Mutex};
use tokio::time::Instant;

type ScopeKey = (String, String);
fn scope_key(key: &GenerationKey) -> ScopeKey {
    (key.session.session_id.clone(), key.session.user_id.clone())
}

#[derive(Clone)]
pub(super) struct Batch {
    key: GenerationKey,
    turn_id: u64,
    version: u64,
    since: Instant,
    activity: String,
    remaining: BTreeMap<String, String>,
}

#[derive(Default)]
struct State {
    version: u64,
    batches: BTreeMap<ScopeKey, Batch>,
}

/// 真实运行时回调同步更新，不能依赖可能丢弃的进度展示队列。
#[derive(Default)]
pub(super) struct ForegroundActivity(Mutex<State>);
impl ForegroundActivity {
    pub(super) fn observe(&self, event: &ControlEvent) {
        let Ok(mut state) = self.0.lock() else { return };
        let key = scope_key(&event.key);
        if let TurnEventKind::ToolBatchStarted { calls } = &event.event.kind
            && let Some(turn_id) = event.event.turn_id
            && calls.iter().any(|call| {
                !matches!(
                    call.name.as_str(),
                    "send_message" | "send_sticker" | "wait_for_input" | "task_progress"
                )
            })
        {
            if state.batches.len() >= 128 && !state.batches.contains_key(&key) {
                return;
            }
            state.version = state.version.saturating_add(1);
            let batch = Batch {
                key: event.key.clone(),
                turn_id,
                version: state.version,
                since: Instant::now(),
                activity: activities(calls.iter().map(|c| c.name.as_str())),
                remaining: calls
                    .iter()
                    .map(|call| (call.id.clone(), call.name.clone()))
                    .collect(),
            };
            state.batches.insert(key, batch);
        } else if let TurnEventKind::ToolResult { result, .. } = &event.event.kind {
            // 替换 ControlRunner 可逐个回报；只有最后一个工具结果回来才结束整批活动。
            if let Some(batch) = state.batches.get_mut(&key).filter(|batch| {
                batch.key == event.key && event.event.turn_id == Some(batch.turn_id)
            }) {
                batch.remaining.remove(&result.call_id);
                batch.activity = activities(batch.remaining.values().map(String::as_str));
                if batch.remaining.is_empty() {
                    state.batches.remove(&key);
                }
            }
        } else if matches!(
            event.event.kind,
            TurnEventKind::ToolBatchStarted { .. }
                | TurnEventKind::ToolProgress { .. }
                | TurnEventKind::ProviderStarted { .. }
                | TurnEventKind::ResponseCompleted { .. }
                | TurnEventKind::ContextCompactionStarted
                | TurnEventKind::TurnCompleted { .. }
                | TurnEventKind::SessionSaved
                | TurnEventKind::Failed { .. }
        ) && state
            .batches
            .get(&key)
            .is_some_and(|batch| batch.key == event.key)
        {
            state.batches.remove(&key);
        }
    }

    pub(super) fn current(&self, key: &GenerationKey, turn_id: u64) -> Option<Batch> {
        self.0
            .lock()
            .ok()?
            .batches
            .get(&scope_key(key))
            .filter(|batch| batch.key == *key && batch.turn_id == turn_id)
            .cloned()
    }
}

pub(super) struct ForegroundInput {
    pub(super) event: QqSpeechEvent,
    pub(super) key: GenerationKey,
    pub(super) turn_id: u64,
    pub(super) resolved: bool,
    pub(super) next: Instant,
    batch: u64,
}
impl ForegroundInput {
    pub(super) fn new(batch: Batch, anchor: String) -> Self {
        let identity = serde_json::to_vec(&(&batch.key, batch.turn_id, batch.version, &anchor))
            .expect("固定身份结构可序列化");
        let id = ring::digest::digest(&ring::digest::SHA256, &identity)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        Self {
            event: QqSpeechEvent {
                id: format!("foreground-input:{id}"),
                session: batch.key.session.clone(),
                anchor,
                facts: json!({"kind":"foreground_input_pending", "phase":"tools", "tool_state":"awaiting_results",
                    "activity":batch.activity,"turn_id":batch.turn_id,"observed_at_ms":now_ms(),
                    "tools_elapsed_ms_at_input":batch.since.elapsed().as_millis() as u64,
                    "input_pending":true,"business_action":"continues_in_original_turn"}),
            },
            key: batch.key,
            turn_id: batch.turn_id,
            batch: batch.version,
            resolved: false,
            // 短工具通常会先回到主轮；这里只延后交流机会，不发送定时文案。
            next: Instant::now() + std::time::Duration::from_secs(2),
        }
    }

    pub(super) fn current(
        &self,
        live: &LiveDialogue,
        activity: &ForegroundActivity,
        control: &dyn ControlService,
    ) -> bool {
        self.activity_is_current(activity, control)
            && live
                .pending_anchor(
                    &ContextScope {
                        session_id: self.key.session.session_id.clone(),
                        user_id: self.key.session.user_id.clone(),
                    },
                    self.turn_id,
                )
                .as_deref()
                == Some(&self.event.anchor)
    }

    /// 领取输入的临界区已经持有 LiveDialogue 的输入锁，不能再递归读取 pending_anchor。
    pub(super) fn activity_is_current(
        &self,
        activity: &ForegroundActivity,
        control: &dyn ControlService,
    ) -> bool {
        activity
            .current(&self.key, self.turn_id)
            .is_some_and(|batch| batch.version == self.batch)
            && control.snapshot(&self.key.session).is_ok_and(|snapshot| {
                snapshot.is_some_and(|s| {
                    s.key == self.key
                        && s.turn_id == Some(self.turn_id)
                        && s.phase == ControlPhase::Tools
                        && !s.cancel_requested
                        && !s.events_retired
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_llm_api::{ToolCall, TurnEvent};
    use eve_session_api::SessionKey;

    fn event(key: &GenerationKey, kind: TurnEventKind) -> ControlEvent {
        ControlEvent {
            key: key.clone(),
            event: TurnEvent {
                turn_id: Some(3),
                kind,
            },
        }
    }

    #[test]
    fn activity_is_exact_to_generation_and_batch_and_never_includes_command_arguments() {
        let activity = ForegroundActivity::default();
        let key = GenerationKey {
            session: SessionKey::new("session", "owner").unwrap(),
            task_id: "task".into(),
            controller_epoch: [1; 16],
            generation: 1,
        };
        let start = event(
            &key,
            TurnEventKind::ToolBatchStarted {
                calls: vec![ToolCall {
                    id: "call".into(),
                    name: "run_command".into(),
                    arguments: json!({"args":["private-token"]}),
                }],
            },
        );
        activity.observe(&start);
        let first = activity.current(&key, 3).unwrap();
        assert!(activity.current(&key, 4).is_none());
        let mut other = key.clone();
        other.generation += 1;
        assert!(activity.current(&other, 3).is_none());
        activity.observe(&event(
            &other,
            TurnEventKind::ProviderStarted { request: 1 },
        ));
        assert!(activity.current(&key, 3).is_some());
        let input = ForegroundInput::new(first.clone(), "new-input".into());
        assert_eq!(input.event.facts["tool_state"], "awaiting_results");
        assert_eq!(input.event.facts["activity"], "命令执行");
        assert!(!input.event.facts.to_string().contains("private-token"));
        activity.observe(&event(
            &key,
            TurnEventKind::ToolProgress {
                completed_calls: 1,
                failed_calls: 0,
                tool_names: vec!["run_command".into()],
            },
        ));
        assert!(activity.current(&key, 3).is_none());
        activity.observe(&start);
        assert_ne!(activity.current(&key, 3).unwrap().version, first.version);
        activity.observe(&event(
            &key,
            TurnEventKind::ToolBatchStarted {
                calls: vec![ToolCall {
                    id: "wait".into(),
                    name: "wait_for_input".into(),
                    arguments: json!({}),
                }],
            },
        ));
        assert!(activity.current(&key, 3).is_none());
    }

    #[test]
    fn individual_fast_result_preserves_pending_long_tool_and_ignores_foreign_call_ids() {
        let activity = ForegroundActivity::default();
        let key = GenerationKey {
            session: SessionKey::new("session", "owner").unwrap(),
            task_id: "task".into(),
            controller_epoch: [1; 16],
            generation: 1,
        };
        activity.observe(&event(
            &key,
            TurnEventKind::ToolBatchStarted {
                calls: vec![
                    ToolCall {
                        id: "fast".into(),
                        name: "read_file".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "slow".into(),
                        name: "run_command".into(),
                        arguments: json!({}),
                    },
                ],
            },
        ));
        let version = activity.current(&key, 3).unwrap().version;
        let result = |call: &str| TurnEventKind::ToolResult {
            ordinal: 0,
            result: eve_llm_api::ToolResult::success(call, json!({"done":true})).unwrap(),
        };
        activity.observe(&event(&key, result("foreign")));
        assert_eq!(activity.current(&key, 3).unwrap().remaining.len(), 2);
        activity.observe(&event(&key, result("fast")));
        let current = activity.current(&key, 3).expect("长工具仍然在运行");
        assert_eq!(current.version, version);
        assert_eq!(current.remaining.len(), 1);
        assert_eq!(current.activity, "命令执行");
        let input = ForegroundInput::new(current, "while-slow".into());
        assert_eq!(input.event.facts["activity"], "命令执行");
        activity.observe(&event(&key, result("fast")));
        assert!(activity.current(&key, 3).is_some());
        activity.observe(&event(&key, result("slow")));
        assert!(activity.current(&key, 3).is_none());
    }
}
