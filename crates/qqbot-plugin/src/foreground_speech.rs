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
    progress_observed: bool,
    progress_revision: u64,
    observed_revision: u64,
    delegated: BTreeMap<String, eve_llm_api::DelegatedToolProgress>,
}

struct ProgressBudget {
    key: GenerationKey,
    count: usize,
    last: Instant,
}

#[derive(Default)]
struct State {
    version: u64,
    batches: BTreeMap<ScopeKey, Batch>,
    progress: BTreeMap<ScopeKey, ProgressBudget>,
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
                progress_observed: false,
                progress_revision: 0,
                observed_revision: 0,
                delegated: BTreeMap::new(),
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
        } else if let TurnEventKind::DelegatedToolProgress {
            parent_call_id,
            progress,
        } = &event.event.kind
        {
            if let Some(batch) = state.batches.get_mut(&key).filter(|batch| {
                batch.key == event.key
                    && event.event.turn_id == Some(batch.turn_id)
                    && batch.remaining.contains_key(parent_call_id)
            }) && batch.delegated.get(parent_call_id) != Some(progress)
            {
                batch.progress_revision = batch.progress_revision.saturating_add(1);
                batch
                    .delegated
                    .insert(parent_call_id.clone(), progress.clone());
            }
        } else if let TurnEventKind::ToolResult { result, .. } = &event.event.kind {
            // 替换 ControlRunner 可逐个回报；只有最后一个工具结果回来才结束整批活动。
            if let Some(batch) = state.batches.get_mut(&key).filter(|batch| {
                batch.key == event.key && event.event.turn_id == Some(batch.turn_id)
            }) {
                batch.remaining.remove(&result.call_id);
                batch.delegated.remove(&result.call_id);
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

    /// 慢工具及之后实际变化的子代理阶段各提供一次交流机会；同代最多三次、至少间隔一分钟。
    /// 这里限制的是观察与模型判断，不自动发消息，不重建已消失的批次。
    pub(super) fn progress_candidate(&self, key: &GenerationKey, turn_id: u64) -> Option<Batch> {
        let mut state = self.0.lock().ok()?;
        let scope = scope_key(key);
        let batch = state.batches.get(&scope)?;
        if batch.key != *key
            || batch.turn_id != turn_id
            || (batch.progress_observed && batch.observed_revision == batch.progress_revision)
            || batch.since.elapsed() < std::time::Duration::from_secs(8)
        {
            return None;
        }
        if state.progress.get(&scope).is_some_and(|budget| {
            budget.key == *key
                && (budget.count >= 3 || budget.last.elapsed() < std::time::Duration::from_secs(60))
        }) {
            return None;
        }
        let batch = state.batches.get_mut(&scope)?;
        batch.progress_observed = true;
        batch.observed_revision = batch.progress_revision;
        let candidate = batch.clone();
        let previous = state
            .progress
            .remove(&scope)
            .filter(|budget| budget.key == *key);
        // 运行范围有界；旧会话预算不因活动结束提前释放，避免同代重新开始绕过节流。
        if state.progress.len() >= 128 {
            let oldest = state
                .progress
                .iter()
                .min_by_key(|(_, budget)| budget.last)
                .map(|(scope, _)| scope.clone());
            if let Some(oldest) = oldest {
                state.progress.remove(&oldest);
            }
        }
        state.progress.insert(
            scope,
            ProgressBudget {
                key: key.clone(),
                count: previous.map_or(1, |budget| budget.count + 1),
                last: Instant::now(),
            },
        );
        Some(candidate)
    }

    fn observed_latest(&self, key: &GenerationKey, turn_id: u64, version: u64) -> Option<Batch> {
        let mut state = self.0.lock().ok()?;
        let batch = state.batches.get_mut(&scope_key(key))?;
        if batch.key != *key || batch.turn_id != turn_id || batch.version != version {
            return None;
        }
        batch.observed_revision = batch.progress_revision;
        let observed = batch.clone();
        if let Some(budget) = state
            .progress
            .get_mut(&scope_key(key))
            .filter(|budget| budget.key == *key)
        {
            // 按实际开始审视计时，开窗或其他会话排队耗时不能缩短下一次模型判断间隔。
            budget.last = Instant::now();
        }
        Some(observed)
    }

    /// 运行时已交付工具批次、仍等待回执；耗时自交付计，不冒充实际执行或成功。
    pub(super) fn learning_activity(
        &self,
        snapshot: &eve_control_api::ControlSnapshot,
    ) -> Option<eve_training_api::ConversationActivity> {
        let batch = self.current(&snapshot.key, snapshot.turn_id?)?;
        let mut names = batch.remaining.values().filter(|name| {
            !matches!(
                name.as_str(),
                "send_message" | "send_sticker" | "wait_for_input" | "task_progress"
            )
        });
        let first = names.next()?.clone();
        Some(eve_training_api::ConversationActivity {
            phase: "awaiting_tool_results".into(),
            elapsed_ms: batch.since.elapsed().as_millis().try_into().ok(),
            tool_name: if names.next().is_none() {
                Some(first)
            } else {
                None
            },
        })
    }
}

pub(super) struct ForegroundInput {
    pub(super) event: QqSpeechEvent,
    pub(super) key: GenerationKey,
    pub(super) turn_id: u64,
    pub(super) resolved: bool,
    pub(super) next: Instant,
    batch: u64,
    pub(super) progress: bool,
    proactive_at: Option<Instant>,
    decision_started: bool,
    progress_revision: u64,
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
            progress: false,
            proactive_at: None,
            decision_started: false,
            progress_revision: batch.progress_revision,
            key: batch.key,
            turn_id: batch.turn_id,
            batch: batch.version,
            resolved: false,
            // 短工具通常会先回到主轮；这里只延后交流机会，不发送定时文案。
            next: Instant::now() + std::time::Duration::from_secs(2),
        }
    }

    pub(super) fn progress(batch: Batch, anchor: String, started: Instant) -> Self {
        let mut value = Self::new(batch, anchor);
        value.progress = true;
        // 只在原轮已确定超过被动回复期后使用独立主动通道，不延长平台窗口。
        value.proactive_at = Some(started + std::time::Duration::from_secs(240));
        value.event.id = value
            .event
            .id
            .replacen("foreground-input:", "foreground-progress:", 1);
        value
            .event
            .id
            .push_str(&format!("-{}", value.progress_revision));
        value.event.facts["kind"] = json!("foreground_tool_progress");
        value.event.facts["input_pending"] = json!(false);
        value.event.facts["tools_elapsed_ms"] =
            value.event.facts["tools_elapsed_ms_at_input"].take();
        value
            .event
            .facts
            .as_object_mut()
            .expect("固定事实对象")
            .remove("tools_elapsed_ms_at_input");
        value.next = Instant::now();
        value
    }

    /// 追加原话只有自己的真实被动窗口，不能借父输入的年龄升级为主动投递。
    pub(super) fn use_merged_window(&mut self) {
        self.proactive_at = None;
    }

    pub(super) fn proactive(&self, now: Instant) -> bool {
        self.progress && self.proactive_at.is_some_and(|at| now >= at)
    }

    /// 只在首次决策前冻结最新只读进展；之后同 event 的事实不变，缓存决定不再次调用模型。
    pub(super) fn prepare_decision(&mut self, activity: &ForegroundActivity) {
        if !self.progress || self.decision_started {
            return;
        }
        if let Some(batch) = activity.observed_latest(&self.key, self.turn_id, self.batch) {
            self.event.facts["activity"] = json!(batch.activity);
            self.event.facts["tools_elapsed_ms"] = json!(batch.since.elapsed().as_millis() as u64);
            self.event.facts["observed_at_ms"] = json!(now_ms());
            self.event.facts["delegated_tools"] =
                json!(batch.delegated.values().collect::<Vec<_>>());
            self.event.facts["delegated_results_meaning"] = json!(
                "submitted仅表示交付执行器，仍可能校验或排队；returned是收到工具回执且包括失败，failed/unknown是失败及副作用未确认的计数，不能据此推断编译或业务完成"
            );
        }
        self.decision_started = true;
    }

    pub(super) fn current(
        &self,
        live: &LiveDialogue,
        activity: &ForegroundActivity,
        control: &dyn ControlService,
    ) -> bool {
        let scope = ContextScope {
            session_id: self.key.session.session_id.clone(),
            user_id: self.key.session.user_id.clone(),
        };
        self.activity_is_current(activity, control)
            && if self.progress {
                live.progress_is_current(&scope, self.turn_id)
            } else {
                live.pending_anchor(&scope, self.turn_id).as_deref() == Some(&self.event.anchor)
            }
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

/// 已持久领取后，业务自然结束不会撤销即时交流；真实取消、退役和换代仍撤销。
pub(super) fn claimed_reply_is_current(
    control: &dyn ControlService,
    key: &GenerationKey,
    turn_id: u64,
) -> bool {
    control.snapshot(&key.session).is_ok_and(|snapshot| {
        snapshot.is_some_and(|state| {
            state.key == *key
                && state.turn_id == Some(turn_id)
                && !state.cancel_requested
                && !state.events_retired
                && !matches!(
                    state.phase,
                    ControlPhase::Cancelling | ControlPhase::Blocked
                )
        })
    })
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
    fn natural_progress_uses_a_separate_proactive_channel_after_the_original_window() {
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
                calls: vec![ToolCall {
                    id: "coding".into(),
                    name: "run_subagent".into(),
                    arguments: json!({}),
                }],
            },
        ));
        let batch = activity.current(&key, 3).unwrap();
        let started = Instant::now();
        let natural = ForegroundInput::progress(batch.clone(), "original".into(), started);
        assert!(!natural.proactive(started + std::time::Duration::from_secs(239)));
        assert!(natural.proactive(started + std::time::Duration::from_secs(240)));
        let after_compaction = ForegroundInput::progress(
            batch.clone(),
            "original".into(),
            started - std::time::Duration::from_secs(250),
        );
        assert!(
            after_compaction.proactive(started),
            "压缩消耗旧窗口不能取消后续真实工具进度"
        );
        let mut merged = ForegroundInput::progress(batch.clone(), "merged".into(), started);
        merged.use_merged_window();
        assert!(
            !merged.proactive(started + std::time::Duration::from_secs(1000)),
            "追加消息不能借父轮年龄升级为主动投递"
        );
        let input = ForegroundInput::new(batch, "new-input".into());
        assert!(
            !input.proactive(started + std::time::Duration::from_secs(1000)),
            "旧输入即时回复仍不能自动升级主动消息"
        );
    }

    #[test]
    fn long_batch_opportunity_needs_no_new_input_and_is_bounded_by_generation() {
        let activity = ForegroundActivity::default();
        let mut key = GenerationKey {
            session: SessionKey::new("session", "owner").unwrap(),
            task_id: "task".into(),
            controller_epoch: [1; 16],
            generation: 1,
        };
        let start = |key: &GenerationKey| {
            event(
                key,
                TurnEventKind::ToolBatchStarted {
                    calls: vec![ToolCall {
                        id: "coding".into(),
                        name: "run_subagent".into(),
                        arguments: json!({"private":"secret"}),
                    }],
                },
            )
        };
        activity.observe(&start(&key));
        assert!(
            activity.progress_candidate(&key, 3).is_none(),
            "短工具不触发"
        );
        let age = |activity: &ForegroundActivity, key: &GenerationKey| {
            activity
                .0
                .lock()
                .unwrap()
                .batches
                .get_mut(&scope_key(key))
                .unwrap()
                .since -= std::time::Duration::from_secs(9);
        };
        age(&activity, &key);
        let first = activity.progress_candidate(&key, 3).unwrap();
        let input = ForegroundInput::progress(first, "original-message".into(), Instant::now());
        assert!(input.event.id.starts_with("foreground-progress:"));
        assert_eq!(input.event.facts["kind"], "foreground_tool_progress");
        assert_eq!(input.event.facts["input_pending"], false);
        assert_eq!(input.event.facts["tool_state"], "awaiting_results");
        assert!(!input.event.facts.to_string().contains("secret"));
        assert!(
            activity.progress_candidate(&key, 3).is_none(),
            "同批只观察一次"
        );
        for count in 1..=3 {
            activity.observe(&start(&key));
            age(&activity, &key);
            assert!(
                activity.progress_candidate(&key, 3).is_none(),
                "同代新批仍有间隔"
            );
            activity
                .0
                .lock()
                .unwrap()
                .progress
                .get_mut(&scope_key(&key))
                .unwrap()
                .last -= std::time::Duration::from_secs(61);
            assert_eq!(
                activity.progress_candidate(&key, 3).is_some(),
                count < 3,
                "每代最多三个机会"
            );
        }
        key.generation += 1;
        activity.observe(&start(&key));
        age(&activity, &key);
        assert!(
            activity.progress_candidate(&key, 3).is_some(),
            "新代有独立预算"
        );
        activity.observe(&event(&key, TurnEventKind::ProviderStarted { request: 2 }));
        assert!(
            activity.progress_candidate(&key, 3).is_none(),
            "工具结束不恢复旧批次"
        );
    }

    #[test]
    fn delegated_stage_updates_keep_parent_identity_and_need_new_facts_for_another_decision() {
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
                calls: vec![ToolCall {
                    id: "coding".into(),
                    name: "run_subagent".into(),
                    arguments: json!({"task":"private task"}),
                }],
            },
        ));
        activity
            .0
            .lock()
            .unwrap()
            .batches
            .get_mut(&scope_key(&key))
            .unwrap()
            .since -= std::time::Duration::from_secs(9);
        let batch = activity.progress_candidate(&key, 3).unwrap();
        let version = batch.version;
        let mut first = ForegroundInput::progress(batch, "original".into(), Instant::now());
        let snapshot = eve_llm_api::DelegatedToolProgress {
            submitted_calls: 1,
            pending_tool_names: vec!["run_command".into()],
            ..Default::default()
        };
        let update = |parent: &str, progress| {
            event(
                &key,
                TurnEventKind::DelegatedToolProgress {
                    parent_call_id: parent.into(),
                    progress,
                },
            )
        };
        activity.observe(&update("foreign", snapshot.clone()));
        assert!(activity.current(&key, 3).unwrap().delegated.is_empty());
        activity.observe(&update("coding", snapshot.clone()));
        first.prepare_decision(&activity);
        assert_eq!(
            first.event.facts["delegated_tools"][0]["submitted_calls"],
            1
        );
        assert_eq!(first.event.facts["delegated_tools"][0]["returned_calls"], 0);
        assert!(!first.event.facts.to_string().contains("private task"));
        assert_eq!(activity.current(&key, 3).unwrap().version, version);
        let frozen = first.event.clone();
        activity
            .0
            .lock()
            .unwrap()
            .progress
            .get_mut(&scope_key(&key))
            .unwrap()
            .last -= std::time::Duration::from_secs(61);
        activity.observe(&update("coding", snapshot.clone()));
        assert!(
            activity.progress_candidate(&key, 3).is_none(),
            "不变快照不可重复判断"
        );
        let returned = eve_llm_api::DelegatedToolProgress {
            returned_calls: 1,
            pending_tool_names: vec![],
            last_returned_tool_name: Some("run_command".into()),
            ..snapshot
        };
        activity.observe(&update("coding", returned));
        first.prepare_decision(&activity);
        assert_eq!(first.event, frozen, "持久事件事实冻结后不能复用版本改写");
        let mut second = ForegroundInput::progress(
            activity.progress_candidate(&key, 3).unwrap(),
            "original".into(),
            Instant::now(),
        );
        second.prepare_decision(&activity);
        assert_ne!(first.event.id, second.event.id);
        assert_eq!(
            second.event.facts["delegated_tools"][0]["returned_calls"],
            1
        );
        assert_eq!(second.event.facts["tool_state"], "awaiting_results");
        let mut foreign_generation = update("coding", Default::default());
        foreign_generation.key.generation += 1;
        activity.observe(&foreign_generation);
        assert_eq!(
            activity.current(&key, 3).unwrap().delegated["coding"].returned_calls,
            1
        );
        activity.observe(&event(
            &key,
            TurnEventKind::ToolResult {
                ordinal: 0,
                result: eve_llm_api::ToolResult::success("coding", json!({"done":true})).unwrap(),
            },
        ));
        activity.observe(&update("coding", Default::default()));
        assert!(
            activity.current(&key, 3).is_none(),
            "父调用结束后迟到阶段不可复活"
        );
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
