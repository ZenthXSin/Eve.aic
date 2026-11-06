//! 从真实运行事件生成系统进度；不调用模型，不把内部正文或工具输出变成发言。
use super::activities;
use eve_control_api::{ControlEvent, ControlPhase, ControlSnapshot, GenerationKey};
use eve_llm_api::TurnEventKind;
use eve_plugin_api::{LogEntry, LogLevel, PluginContext};
use std::{collections::BTreeMap, sync::Mutex, time::Duration};
use tokio::time::Instant;

type Scope = (String, String);
fn scope(key: &GenerationKey) -> Scope {
    (key.session.session_id.clone(), key.session.user_id.clone())
}

enum Phase {
    Model,
    Tools(BTreeMap<String, String>),
    Compaction,
}
struct Activity {
    key: GenerationKey,
    turn_id: u64,
    version: u64,
    since: Instant,
    phase: Phase,
}
#[derive(Default)]
struct State {
    version: u64,
    active: BTreeMap<Scope, Activity>,
}

/// 回调同步更新，结束事件不能因展示队列拥塞而丢失。
#[derive(Default)]
pub(super) struct HostProgress(Mutex<State>);

/// 只减少宿主提示，不改变模型任务和发言权；宁可省略提示，也不打断明确要求的安静。
pub(super) fn quiet_request(text: &str) -> bool {
    [
        "不要进度",
        "不用进度",
        "别发进度",
        "不发进度",
        "不要汇报",
        "不用汇报",
        "无需汇报",
        "先不用回答",
        "先不回答",
        "先别回复",
        "不要回复",
        "不用回复",
        "不要打扰",
        "保持安静",
        "静默运行",
        "安静执行",
    ]
    .iter()
    .any(|phrase| text.contains(phrase))
        || [
            "no progress",
            "don't send progress",
            "do not reply",
            "stay quiet",
            "silently",
        ]
        .iter()
        .any(|phrase| text.to_ascii_lowercase().contains(phrase))
}

/// 活动平台编号进入现有宿主日志；主回执继续使用 v9 的状态与去重语义。
pub(super) fn log_delivery(
    ctx: &PluginContext,
    app: &str,
    input: &str,
    index: usize,
    state: crate::state::PartState,
    platform_message_id: Option<&str>,
) {
    if let Some(entry) = delivery_entry(app, input, index, state, platform_message_id)
        && ctx.log(entry).is_err()
    {
        // 不改变已保存的投递状态，不重发消息，也不递归请求失败的日志后端。
        eprintln!("EVE_QQ_HOST_PROGRESS_LOG_UNAVAILABLE");
    }
}

fn delivery_entry(
    app: &str,
    input: &str,
    index: usize,
    state: crate::state::PartState,
    platform_message_id: Option<&str>,
) -> Option<LogEntry> {
    use crate::state::{PartState, valid_id, valid_message_id};
    if !valid_id(app) || !valid_message_id(input) || index >= 64 {
        return None;
    }
    let (level, status) = match state {
        PartState::Sent => (LogLevel::Info, "sent"),
        PartState::Failed => (LogLevel::Warn, "failed"),
        PartState::Unknown => (LogLevel::Warn, "unknown"),
        _ => return None,
    };
    let mut entry = LogEntry::new(level, "eve.qqbot", "host_progress_delivery")
        .ok()?
        .with_field("app_id", app)
        .with_field("input_message_id", input)
        .with_field("progress_index", index.to_string())
        .with_field("delivery_status", status);
    if state == PartState::Sent {
        entry = entry.with_field(
            "platform_message_id",
            platform_message_id.filter(|id| valid_message_id(id))?,
        );
    }
    Some(entry)
}

pub(super) struct Notice {
    pub(super) key: GenerationKey,
    pub(super) turn_id: u64,
    pub(super) text: String,
    version: u64,
    phase: ControlPhase,
}
impl Notice {
    pub(super) fn accepts(&self, snapshot: &ControlSnapshot) -> bool {
        snapshot.key == self.key
            && snapshot.turn_id == Some(self.turn_id)
            && snapshot.phase == self.phase
            && !snapshot.cancel_requested
            && !snapshot.events_retired
    }
}

impl HostProgress {
    pub(super) fn observe(&self, event: &ControlEvent) {
        self.observe_at(event, Instant::now());
    }

    fn observe_at(&self, event: &ControlEvent, now: Instant) {
        let Some(turn_id) = event.event.turn_id else {
            return;
        };
        let Ok(mut state) = self.0.lock() else { return };
        let id = scope(&event.key);
        // 迟到的旧代开始事件也不能覆盖新活动；同一控制代的 turn_id 不应变化。
        if state.active.get(&id).is_some_and(|activity| {
            (activity.key.controller_epoch == event.key.controller_epoch
                && activity.key.generation > event.key.generation)
                || (activity.key == event.key && activity.turn_id != turn_id)
        }) {
            return;
        }
        let next = match &event.event.kind {
            TurnEventKind::ProviderStarted { .. } => Some(Phase::Model),
            TurnEventKind::ContextCompactionStarted => Some(Phase::Compaction),
            TurnEventKind::ToolBatchStarted { calls } => {
                let remaining: BTreeMap<_, _> = calls
                    .iter()
                    .filter(|call| {
                        !matches!(
                            call.name.as_str(),
                            "send_message" | "send_sticker" | "wait_for_input" | "task_progress"
                        )
                    })
                    .map(|call| (call.id.clone(), call.name.clone()))
                    .collect();
                (!remaining.is_empty()).then_some(Phase::Tools(remaining))
            }
            TurnEventKind::ToolResult { result, .. } => {
                if let Some(activity) = state
                    .active
                    .get_mut(&id)
                    .filter(|activity| activity.key == event.key && activity.turn_id == turn_id)
                    && let Phase::Tools(remaining) = &mut activity.phase
                    && remaining.remove(&result.call_id).is_some()
                {
                    // 保留整批开始时刻，但逐项回执立即使旧活动类别失效。
                    if remaining.is_empty() {
                        state.active.remove(&id);
                    } else {
                        state.version = state.version.saturating_add(1);
                        let version = state.version;
                        if let Some(activity) = state.active.get_mut(&id) {
                            activity.version = version;
                        }
                    }
                }
                return;
            }
            TurnEventKind::ResponseCompleted { .. }
            | TurnEventKind::ToolProgress { .. }
            | TurnEventKind::TurnCompleted { .. }
            | TurnEventKind::SessionSaved
            | TurnEventKind::Failed { .. } => None,
            _ => return,
        };
        if let Some(phase) = next {
            if state.active.len() >= 128 && !state.active.contains_key(&id) {
                // 取消可能没有后续结束事件；旧槽不能永久阻止新会话显示真实活动。
                let oldest = state
                    .active
                    .iter()
                    .min_by_key(|(_, activity)| activity.since)
                    .map(|(scope, _)| scope.clone());
                if let Some(oldest) = oldest {
                    state.active.remove(&oldest);
                }
            }
            state.version = state.version.saturating_add(1);
            let version = state.version;
            state.active.insert(
                id,
                Activity {
                    key: event.key.clone(),
                    turn_id,
                    version,
                    since: now,
                    phase,
                },
            );
        } else if state
            .active
            .get(&id)
            .is_some_and(|activity| activity.key == event.key && activity.turn_id == turn_id)
        {
            state.active.remove(&id);
        }
    }

    pub(super) fn due(&self, key: &GenerationKey, after: Duration) -> Option<Notice> {
        self.due_at(key, after, Instant::now())
    }
    fn due_at(&self, key: &GenerationKey, after: Duration, now: Instant) -> Option<Notice> {
        let state = self.0.lock().ok()?;
        let activity = state.active.get(&scope(key))?;
        if activity.key != *key || now.saturating_duration_since(activity.since) < after {
            return None;
        }
        let (phase, text) = match &activity.phase {
            Phase::Model => (
                ControlPhase::Generating,
                "系统进度：正在等待回复生成，本轮处理尚未结束。".into(),
            ),
            Phase::Compaction => (
                ControlPhase::Generating,
                "系统进度：正在整理本轮上下文，处理尚未结束。".into(),
            ),
            Phase::Tools(remaining) => (
                ControlPhase::Tools,
                format!(
                    "系统进度：已进入{}阶段，尚未收到全部工具结果。",
                    activities(remaining.values().map(String::as_str))
                ),
            ),
        };
        Some(Notice {
            key: key.clone(),
            turn_id: activity.turn_id,
            text,
            version: activity.version,
            phase,
        })
    }

    pub(super) fn current(&self, notice: &Notice) -> bool {
        self.0.lock().is_ok_and(|state| {
            state
                .active
                .get(&scope(&notice.key))
                .is_some_and(|activity| {
                    activity.key == notice.key
                        && activity.turn_id == notice.turn_id
                        && activity.version == notice.version
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_llm_api::{ToolCall, ToolResult, TurnEvent};
    use eve_session_api::SessionKey;
    use serde_json::json;

    fn key() -> GenerationKey {
        GenerationKey {
            session: SessionKey::new("scope", "owner").unwrap(),
            task_id: "task".into(),
            controller_epoch: [1; 16],
            generation: 1,
        }
    }
    fn event(key: &GenerationKey, kind: TurnEventKind) -> ControlEvent {
        ControlEvent {
            key: key.clone(),
            event: TurnEvent {
                turn_id: Some(3),
                kind,
            },
        }
    }
    fn batch(names: &[(&str, &str)]) -> TurnEventKind {
        TurnEventKind::ToolBatchStarted {
            calls: names
                .iter()
                .map(|(id, name)| ToolCall {
                    id: (*id).into(),
                    name: (*name).into(),
                    arguments: json!({"private":"不得输出"}),
                })
                .collect(),
        }
    }
    fn result(id: &str) -> TurnEventKind {
        TurnEventKind::ToolResult {
            ordinal: 0,
            result: ToolResult::success(id, json!({"untrusted":"声称已经完成"})).unwrap(),
        }
    }

    #[test]
    fn slow_model_is_observed_without_turning_final_or_fast_work_into_speech() {
        let host = HostProgress::default();
        let key = key();
        let now = Instant::now();
        let after = Duration::from_secs(45);
        host.observe_at(
            &event(&key, TurnEventKind::ProviderStarted { request: 1 }),
            now,
        );
        assert!(
            host.due_at(&key, after, now + Duration::from_secs(44))
                .is_none()
        );
        let notice = host.due_at(&key, after, now + after).unwrap();
        assert!(notice.text.starts_with("系统进度："));
        assert!(host.current(&notice));
        host.observe_at(&event(&key, TurnEventKind::SessionSaved), now + after);
        assert!(!host.current(&notice));
        assert!(host.due_at(&key, after, now + after).is_none());
    }

    #[test]
    fn partial_tool_receipts_keep_the_real_remaining_activity_and_invalidate_old_notice() {
        let host = HostProgress::default();
        let key = key();
        let now = Instant::now();
        host.observe_at(
            &event(&key, batch(&[("a", "read_file"), ("b", "run_command")])),
            now,
        );
        let old = host.due_at(&key, Duration::ZERO, now).unwrap();
        assert!(!old.text.contains("不得输出"));
        host.observe_at(&event(&key, result("foreign")), now);
        assert!(host.current(&old));
        host.observe_at(&event(&key, result("a")), now);
        assert!(!host.current(&old));
        let next = host.due_at(&key, Duration::ZERO, now).unwrap();
        assert!(next.text.contains("命令执行"));
        assert!(!next.text.contains("源码"));
        assert!(!next.text.contains("已经完成"));
        host.observe_at(&event(&key, result("b")), now);
        assert!(!host.current(&next));
    }

    #[test]
    fn quiet_listening_and_explicit_send_have_no_automatic_notice() {
        let host = HostProgress::default();
        let key = key();
        let now = Instant::now();
        for name in [
            "wait_for_input",
            "send_message",
            "send_sticker",
            "task_progress",
        ] {
            host.observe_at(&event(&key, batch(&[("call", name)])), now);
            assert!(host.due_at(&key, Duration::ZERO, now).is_none());
        }
        assert!(quiet_request("这个先不用回答，先核对"));
        assert!(quiet_request("Please stay quiet while checking"));
        assert!(!quiet_request("执行期间告诉我现在在做什么"));
    }

    #[test]
    fn retired_generation_cannot_clear_or_authorize_a_new_activity() {
        let host = HostProgress::default();
        let old = key();
        let mut new = old.clone();
        new.generation += 1;
        let now = Instant::now();
        host.observe_at(
            &event(&new, TurnEventKind::ProviderStarted { request: 1 }),
            now,
        );
        host.observe_at(
            &event(&old, TurnEventKind::ProviderStarted { request: 2 }),
            now,
        );
        host.observe_at(&event(&old, TurnEventKind::SessionSaved), now);
        assert!(host.due_at(&old, Duration::ZERO, now).is_none());
        let notice = host.due_at(&new, Duration::ZERO, now).unwrap();
        let mut snapshot = ControlSnapshot {
            key: new,
            input_text: String::new(),
            phase: ControlPhase::Generating,
            cancel_requested: false,
            events_retired: false,
            turn_id: Some(3),
            report: None,
        };
        assert!(notice.accepts(&snapshot));
        snapshot.cancel_requested = true;
        assert!(!notice.accepts(&snapshot));
        snapshot.cancel_requested = false;
        snapshot.phase = ControlPhase::Finished;
        assert!(!notice.accepts(&snapshot));
    }

    #[test]
    fn abandoned_scopes_cannot_exhaust_activity_tracking() {
        let host = HostProgress::default();
        let now = Instant::now();
        let mut latest = key();
        for index in 0..129 {
            latest.session = SessionKey::new(format!("scope-{index}"), "owner").unwrap();
            host.observe_at(
                &event(&latest, TurnEventKind::ProviderStarted { request: 1 }),
                now + Duration::from_millis(index),
            );
        }
        assert!(
            host.due_at(&latest, Duration::ZERO, now + Duration::from_secs(1))
                .is_some()
        );
        assert_eq!(host.0.lock().unwrap().active.len(), 128);
    }

    #[test]
    fn activity_delivery_log_keeps_validated_receipt_ids_and_never_invents_unknown_ids() {
        use crate::state::PartState;
        let sent = delivery_entry("app", "input", 0, PartState::Sent, Some("platform-1")).unwrap();
        assert_eq!(sent.message, "host_progress_delivery");
        assert_eq!(sent.fields["input_message_id"], "input");
        assert_eq!(sent.fields["platform_message_id"], "platform-1");
        assert_eq!(sent.fields["delivery_status"], "sent");
        assert_eq!(sent.fields.len(), 5);
        assert!(delivery_entry("app", "input", 0, PartState::Sent, Some("\n")).is_none());
        let unknown =
            delivery_entry("app", "input", 1, PartState::Unknown, Some("late-id")).unwrap();
        assert_eq!(unknown.fields["delivery_status"], "unknown");
        assert!(!unknown.fields.contains_key("platform_message_id"));
    }
}
