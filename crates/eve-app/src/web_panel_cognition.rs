//! 认知状态到面板只读契约的投影；只使用宿主绑定的读取句柄，不写目标、不触发规划。
use crate::web_panel::clip;
use eve_cognition_api::{
    BlockReason, CognitionReader, CognitiveEvent, CognitiveEventKind, CognitiveState,
    ExecutionCommit, Goal, GoalStatus, Source, SourceKind, Visibility,
};
use eve_cognition_loop_plugin::{ReflectionArtifact, current_reflection};
use eve_llm_api::ChatRole;
use eve_session_api::{SessionKey, SessionService, SessionTurnStatus};
use eve_web_panel_api::*;
use std::ops::Bound;

/// 内生反思子目标的来源通道，与 current_reflection 的派生规则一致。
const REFLECTION_CHANNEL: &str = "endogenous";
const LIST_TEXT_BYTES: usize = 512;
const DETAIL_TEXT_BYTES: usize = 8192;
const EVENT_TEXT_BYTES: usize = 2048;
const MAX_EVENTS: usize = 20;

fn is_reflection_source(source: &Source) -> bool {
    source.kind == SourceKind::Inference && source.channel == REFLECTION_CHANNEL
}
fn is_child_of(child: &Goal, parent: &str) -> bool {
    is_reflection_source(&child.source) && child.source.reference == parent && child.id != parent
}
/// 父目标仍存在的反思子目标只在父目标详情中显示；父目标缺失时仍列在顶层，不被隐藏。
fn nested(state: &CognitiveState, goal: &Goal) -> bool {
    is_reflection_source(&goal.source)
        && goal.source.reference != goal.id
        && state.goals.contains_key(&goal.source.reference)
}
fn status(value: &GoalStatus) -> &'static str {
    match value {
        GoalStatus::Ready => "ready",
        GoalStatus::Waiting => "waiting",
        GoalStatus::Executing => "executing",
        GoalStatus::Completed => "completed",
        GoalStatus::Cancelled => "cancelled",
        GoalStatus::Blocked => "blocked",
    }
}
fn source_kind(value: &SourceKind) -> &'static str {
    match value {
        SourceKind::User => "user",
        SourceKind::Environment => "environment",
        SourceKind::Tool => "tool",
        SourceKind::Inference => "inference",
        SourceKind::Internal => "internal",
    }
}
fn summary(state: &CognitiveState, goal: &Goal, max: usize) -> GoalSummary {
    let (description, description_truncated) = clip(&goal.description, max);
    let (visibility, owner) = match &goal.visibility {
        Visibility::Public => ("public", None),
        Visibility::User(user) => ("user", Some(user.clone())),
        Visibility::Internal => ("internal", None),
    };
    GoalSummary {
        id: goal.id.clone(),
        revision: goal.revision,
        status: status(&goal.status),
        priority: goal.priority,
        source_kind: source_kind(&goal.source.kind),
        source_channel: goal.source.channel.clone(),
        visibility,
        owner,
        reflection_of: is_reflection_source(&goal.source).then(|| goal.source.reference.clone()),
        reflections: state
            .goals
            .values()
            .filter(|child| is_child_of(child, &goal.id))
            .count(),
        description,
        description_truncated,
    }
}

pub(crate) fn goals(
    reader: &dyn CognitionReader,
    after: Option<&str>,
    limit: usize,
) -> PanelResult<GoalPage> {
    if !(1..=100).contains(&limit) {
        return Err(PanelError::InvalidInput);
    }
    let view = reader.snapshot().map_err(|_| PanelError::Unavailable)?;
    let start = after.map_or(Bound::Unbounded, Bound::Excluded);
    let items: Vec<_> = view
        .state
        .goals
        .range::<str, _>((start, Bound::Unbounded))
        .map(|(_, goal)| goal)
        .filter(|goal| !nested(&view.state, goal))
        .take(limit)
        .map(|goal| summary(&view.state, goal, LIST_TEXT_BYTES))
        .collect();
    let next_cursor =
        (items.len() == limit).then(|| items.last().expect("nonempty page").id.clone());
    Ok(GoalPage {
        subject_id: view.subject_id,
        revision: view.revision,
        items,
        next_cursor,
    })
}

pub(crate) fn goal(
    reader: &dyn CognitionReader,
    sessions: &dyn SessionService,
    id: &str,
) -> PanelResult<GoalDetail> {
    let view = reader.snapshot().map_err(|_| PanelError::Unavailable)?;
    let state = &view.state;
    let goal = state.goals.get(id).ok_or(PanelError::NotFound)?;
    // 当前草稿只按派生规则精确判定；记录残缺或矛盾时不退回任何旧草稿。
    let (reflection_check, current) = match current_reflection(state, &view.subject_id, goal) {
        Ok(child) => ("ok", child.map(|child| child.id.clone())),
        Err(_) => ("inconsistent", None),
    };
    let mut reflections: Vec<_> = state
        .goals
        .values()
        .filter(|child| is_child_of(child, &goal.id))
        .map(|child| {
            let (draft_state, draft) = draft(sessions, child);
            ReflectionView {
                goal_id: child.id.clone(),
                parent_revision: parent_revision(state, goal, child),
                status: status(&child.status),
                current: current.as_deref() == Some(child.id.as_str()),
                draft_state,
                draft,
            }
        })
        .collect();
    reflections.sort_by(|a, b| {
        b.current
            .cmp(&a.current)
            .then(b.parent_revision.cmp(&a.parent_revision))
    });
    let related: Vec<_> = state
        .events
        .iter()
        .filter(|event| event.goal_id.as_deref() == Some(id))
        .collect();
    Ok(GoalDetail {
        subject_id: view.subject_id.clone(),
        revision: view.revision,
        goal: summary(state, goal, DETAIL_TEXT_BYTES),
        verification: goal.verification.clone(),
        stop_condition: goal.stop_condition.clone(),
        wait_reason: goal.wait_reason.clone(),
        block_reason: goal.block_reason.as_ref().map(|reason| match reason {
            BlockReason::Interrupted => "interrupted",
            BlockReason::UnknownCommit => "unknown_commit",
            BlockReason::FeedbackSaveFailed => "feedback_save_failed",
            BlockReason::Invalidated => "invalidated",
        }),
        expires_at_ms: goal.expires_at_ms,
        budget: GoalBudget {
            max_model_requests: goal.budget.max_model_requests,
            max_tool_calls: goal.budget.max_tool_calls,
            max_attempts: goal.budget.max_attempts,
            timeout_ms: goal.budget.timeout_ms,
        },
        execution: goal.execution.as_ref().map(|execution| GoalExecution {
            session_id: execution.session_id.clone(),
            task_id: execution.task_id.clone(),
            turn_id: execution.turn_id,
            started_at_ms: execution.started_at_ms,
        }),
        feedback: goal.feedback.as_ref().map(|feedback| {
            let (summary, summary_truncated) = clip(&feedback.summary, DETAIL_TEXT_BYTES);
            GoalFeedback {
                commit: match feedback.commit {
                    ExecutionCommit::NotStarted => "not_started",
                    ExecutionCommit::Completed => "completed",
                    ExecutionCommit::Failed => "failed",
                    ExecutionCommit::Pending => "pending",
                    ExecutionCommit::Unknown => "unknown",
                },
                verification_met: feedback.verification_met,
                started_tools: feedback.started_tools,
                summary,
                summary_truncated,
                at_ms: feedback.at_ms,
            }
        }),
        events: related
            .iter()
            .rev()
            .take(MAX_EVENTS)
            .map(|e| event(e))
            .collect(),
        events_omitted: related.len().saturating_sub(MAX_EVENTS),
        reflections,
        reflection_check,
    })
}

fn event(event: &CognitiveEvent) -> GoalEvent {
    let (summary, summary_truncated) = clip(&event.summary, EVENT_TEXT_BYTES);
    GoalEvent {
        id: event.id.clone(),
        kind: match event.kind {
            CognitiveEventKind::ExternalInput => "external_input",
            CognitiveEventKind::StateChanged => "state_changed",
            CognitiveEventKind::DriveEvaluated => "drive_evaluated",
            CognitiveEventKind::AgendaSelected => "agenda_selected",
            CognitiveEventKind::Feedback => "feedback",
        },
        source_kind: source_kind(&event.source.kind),
        source_channel: event.source.channel.clone(),
        at_ms: event.at_ms,
        summary,
        summary_truncated,
    }
}

/// 子目标创建事件指向派生输入，输入摘要记录父目标修订；任何一环缺失都返回空，不猜测。
fn parent_revision(state: &CognitiveState, parent: &Goal, child: &Goal) -> Option<u64> {
    let created = state.events.iter().find(|event| {
        event.kind == CognitiveEventKind::DriveEvaluated
            && event.goal_id.as_deref() == Some(child.id.as_str())
            && is_reflection_source(&event.source)
    })?;
    let cause = created.caused_by.as_deref()?;
    let input = state.events.iter().find(|event| {
        event.id == cause
            && event.kind == CognitiveEventKind::StateChanged
            && event.goal_id.as_deref() == Some(parent.id.as_str())
    })?;
    let evidence: serde_json::Value = serde_json::from_str(&input.summary).ok()?;
    if evidence["kind"] != "waiting_input" || evidence["parent_id"] != parent.id.as_str() {
        return None;
    }
    evidence["parent_revision"].as_u64()
}

/// 只读取已通过结构校验的子目标会话结果；正文缺失或格式不一致时不显示，也不报告为已保存。
fn draft(sessions: &dyn SessionService, child: &Goal) -> (&'static str, Option<ReflectionDraft>) {
    let saved = child.status == GoalStatus::Completed
        && child
            .feedback
            .as_ref()
            .is_some_and(|feedback| feedback.verification_met);
    if !saved {
        return ("not_saved", None);
    }
    let text = (|| {
        let execution = child.execution.as_ref()?;
        let user = match &child.visibility {
            Visibility::User(user) => user.as_str(),
            _ => crate::qq_cognition::INTERNAL_USER,
        };
        let key = SessionKey::new(&execution.session_id, user).ok()?;
        let turn = sessions
            .snapshot(&key)
            .ok()??
            .turns
            .into_iter()
            .find(|turn| Some(turn.id) == execution.turn_id)?;
        match turn.status {
            SessionTurnStatus::Completed { messages } => messages
                .into_iter()
                .rev()
                .find(|message| message.role == ChatRole::Assistant)
                .and_then(|message| message.text),
            _ => None,
        }
    })();
    match text.and_then(|text| ReflectionArtifact::parse(&text).ok()) {
        Some(artifact) => (
            "saved",
            Some(ReflectionDraft {
                summary: artifact.summary,
                next_step: artifact.next_step,
                needs_user_input: artifact.needs_user_input,
            }),
        ),
        None => ("unavailable", None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_cognition_api::{
        COGNITION_FORMAT_VERSION, CognitionAdmin, CognitionError, CognitionResult,
        CognitiveSnapshot, CognitiveView, ExecutionAttempt, ExecutionBudget, Feedback, ReadAccess,
    };
    use eve_cognition_loop_api::{AllowedSource, EndogenousOptions, ExecutionScope};
    use eve_cognition_loop_plugin::EndogenousPlanner;
    use eve_llm_api::ChatMessage;
    use eve_session_api::{
        SessionError, SessionFailure, SessionInput, SessionResult, SessionSnapshot, SessionTurn,
        StartedTurn, TurnLease,
    };
    use std::sync::{Arc, Mutex};

    const SUBJECT: &str = "eve";
    const CHANNEL: &str = "qq.goal";
    const OLD_DRAFT: &str =
        r#"{"summary":"旧版本草稿。","next_step":"等待用户确认。","needs_user_input":true}"#;

    struct Admin {
        snapshot: Mutex<CognitiveSnapshot>,
        broken: Mutex<bool>,
    }
    impl CognitionAdmin for Admin {
        fn snapshot(&self) -> CognitionResult<CognitiveSnapshot> {
            Ok(self.snapshot.lock().unwrap().clone())
        }
        fn reader(&self, _: ReadAccess) -> CognitionResult<Arc<dyn CognitionReader>> {
            Err(CognitionError::Unavailable)
        }
        fn replace(
            &self,
            expected_revision: u64,
            mut state: CognitiveState,
        ) -> CognitionResult<CognitiveSnapshot> {
            let mut snapshot = self.snapshot.lock().unwrap();
            if snapshot.revision != expected_revision {
                return Err(CognitionError::StaleRevision);
            }
            for (id, goal) in &mut state.goals {
                match snapshot.state.goals.get(id) {
                    Some(old) if old == goal => {}
                    Some(old) => goal.revision = old.revision + 1,
                    None => goal.revision = 1,
                }
            }
            state.validate()?;
            snapshot.state = state;
            snapshot.revision += 1;
            Ok(snapshot.clone())
        }
    }
    impl CognitionReader for Admin {
        fn snapshot(&self) -> CognitionResult<CognitiveView> {
            if *self.broken.lock().unwrap() {
                return Err(CognitionError::Unavailable);
            }
            let snapshot = self.snapshot.lock().unwrap().clone();
            Ok(CognitiveView {
                subject_id: snapshot.subject_id,
                revision: snapshot.revision,
                state: snapshot.state,
            })
        }
    }
    impl Admin {
        fn edit(&self, change: impl FnOnce(&mut CognitiveState)) {
            let mut snapshot = self.snapshot.lock().unwrap();
            change(&mut snapshot.state);
            snapshot.revision += 1;
        }
    }

    #[derive(Default)]
    struct Sessions(Mutex<Option<SessionSnapshot>>);
    impl SessionService for Sessions {
        fn snapshot(&self, key: &SessionKey) -> SessionResult<Option<SessionSnapshot>> {
            Ok(self.0.lock().unwrap().clone().filter(|s| &s.key == key))
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

    fn user_goal(id: &str, description: &str) -> Goal {
        Goal {
            id: id.into(),
            revision: 1,
            source: Source {
                kind: SourceKind::User,
                channel: CHANNEL.into(),
                reference: format!("message-{id}"),
            },
            visibility: Visibility::User("owner".into()),
            description: description.into(),
            verification: "user-goal:v1".into(),
            priority: 50,
            budget: ExecutionBudget {
                max_model_requests: 1,
                max_tool_calls: 0,
                max_attempts: 1,
                timeout_ms: 30_000,
            },
            stop_condition: "user-confirmation".into(),
            expires_at_ms: None,
            status: GoalStatus::Waiting,
            wait_reason: Some("等待反思草稿与用户处理。".into()),
            block_reason: None,
            execution: None,
            feedback: None,
        }
    }

    fn reconcile(admin: &Arc<Admin>, at_ms: u64) -> String {
        EndogenousPlanner::new(
            admin.clone(),
            EndogenousOptions {
                scope: ExecutionScope {
                    subject_id: SUBJECT.into(),
                    access: ReadAccess::Internal,
                    sources: vec![AllowedSource {
                        kind: SourceKind::User,
                        channel: CHANNEL.into(),
                    }],
                },
                max_derivations: 1,
                timeout_ms: 30_000,
            },
        )
        .unwrap()
        .reconcile(at_ms)
        .unwrap()
        .created_goal_ids
        .remove(0)
    }

    /// 父目标版本 1 的草稿已保存；用户修订后版本 2 的反思尚未执行。
    fn fixture() -> (Arc<Admin>, Sessions, String, String) {
        let mut state = CognitiveState::default();
        state
            .goals
            .insert("parent".into(), user_goal("parent", "整理房间的计划"));
        let admin = Arc::new(Admin {
            snapshot: Mutex::new(CognitiveSnapshot {
                format_version: COGNITION_FORMAT_VERSION,
                subject_id: SUBJECT.into(),
                revision: 1,
                state,
            }),
            broken: Mutex::new(false),
        });
        let old = reconcile(&admin, 10);
        admin.edit(|state| {
            let child = state.goals.get_mut(&old).unwrap();
            child.status = GoalStatus::Completed;
            child.revision += 1;
            child.execution = Some(ExecutionAttempt {
                attempt_id: "attempt".into(),
                session_id: "reflection-session".into(),
                task_id: "reflection-task".into(),
                turn_id: Some(1),
                started_at_ms: 11,
            });
            child.feedback = Some(Feedback {
                commit: ExecutionCommit::Completed,
                verification_met: true,
                started_tools: Some(0),
                summary: "结构已验证".into(),
                at_ms: 12,
            });
        });
        let snapshot = CognitionAdmin::snapshot(admin.as_ref()).unwrap();
        let mut state = snapshot.state;
        state.goals.get_mut("parent").unwrap().wait_reason = Some("用户补充后等待新草稿。".into());
        CognitionAdmin::replace(admin.as_ref(), snapshot.revision, state).unwrap();
        let new = reconcile(&admin, 20);
        let sessions = Sessions(Mutex::new(Some(SessionSnapshot {
            key: SessionKey::new("reflection-session", "owner").unwrap(),
            revision: 2,
            turns: vec![SessionTurn {
                id: 1,
                input: "反思输入".into(),
                status: SessionTurnStatus::Completed {
                    messages: vec![
                        ChatMessage::text(ChatRole::User, "反思输入"),
                        ChatMessage::text(ChatRole::Assistant, OLD_DRAFT),
                    ],
                },
            }],
        })));
        (admin, sessions, old, new)
    }

    #[test]
    fn detail_marks_only_the_current_revision_draft_current_and_keeps_history() {
        let (admin, sessions, old, new) = fixture();
        let detail = goal(admin.as_ref(), &sessions, "parent").unwrap();
        assert_eq!(detail.goal.revision, 2);
        assert_eq!(detail.goal.status, "waiting");
        assert_eq!(detail.goal.owner.as_deref(), Some("owner"));
        assert_eq!(detail.goal.reflections, 2);
        assert_eq!(detail.reflection_check, "ok");
        let rows: Vec<_> = detail
            .reflections
            .iter()
            .map(|r| {
                (
                    r.goal_id.as_str(),
                    r.parent_revision,
                    r.current,
                    r.draft_state,
                    r.status,
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                (new.as_str(), Some(2), true, "not_saved", "ready"),
                (old.as_str(), Some(1), false, "saved", "completed"),
            ]
        );
        let draft = detail.reflections[1].draft.as_ref().unwrap();
        assert_eq!(draft.summary, "旧版本草稿。");
        assert!(draft.needs_user_input);
        assert!(detail.reflections[0].draft.is_none());
        assert!(!detail.events.is_empty());
        assert_eq!(detail.events_omitted, 0);
        assert!(detail.events.windows(2).all(|w| w[0].at_ms >= w[1].at_ms));
    }

    #[test]
    fn missing_session_result_is_unavailable_and_never_reported_as_saved() {
        let (admin, _, old, _) = fixture();
        let detail = goal(admin.as_ref(), &Sessions::default(), "parent").unwrap();
        let row = detail
            .reflections
            .iter()
            .find(|r| r.goal_id == old)
            .unwrap();
        assert_eq!(row.draft_state, "unavailable");
        assert!(row.draft.is_none());
        let wrong = Sessions(Mutex::new(Some(SessionSnapshot {
            key: SessionKey::new("reflection-session", "owner").unwrap(),
            revision: 1,
            turns: vec![SessionTurn {
                id: 1,
                input: "反思输入".into(),
                status: SessionTurnStatus::Completed {
                    messages: vec![ChatMessage::text(ChatRole::Assistant, "不是草稿 JSON")],
                },
            }],
        })));
        let detail = goal(admin.as_ref(), &wrong, "parent").unwrap();
        let row = detail
            .reflections
            .iter()
            .find(|r| r.goal_id == old)
            .unwrap();
        assert_eq!(row.draft_state, "unavailable");
    }

    #[test]
    fn broken_derivation_evidence_marks_no_draft_current() {
        let (admin, sessions, _, new) = fixture();
        admin.edit(|state| {
            state.events.retain(|event| {
                !(event.kind == CognitiveEventKind::DriveEvaluated
                    && event.goal_id.as_deref() == Some(new.as_str()))
            })
        });
        let detail = goal(admin.as_ref(), &sessions, "parent").unwrap();
        assert_eq!(detail.reflection_check, "inconsistent");
        assert!(detail.reflections.iter().all(|r| !r.current));
        let row = detail
            .reflections
            .iter()
            .find(|r| r.goal_id == new)
            .unwrap();
        assert_eq!(row.parent_revision, None);
    }

    #[test]
    fn list_nests_children_keeps_orphans_and_pages_by_id() {
        let (admin, _, old, new) = fixture();
        let page = goals(admin.as_ref(), None, 10).unwrap();
        assert_eq!(page.subject_id, SUBJECT);
        let ids: Vec<_> = page.items.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids, ["parent"]);
        assert_eq!(page.items[0].reflections, 2);
        assert_eq!(page.next_cursor, None);

        admin.edit(|state| {
            let long = "界".repeat(400);
            for id in ["a-goal", "z-goal"] {
                state.goals.insert(id.into(), user_goal(id, &long));
            }
        });
        let first = goals(admin.as_ref(), None, 1).unwrap();
        assert_eq!(first.items[0].id, "a-goal");
        assert!(first.items[0].description_truncated);
        assert!(first.items[0].description.len() <= LIST_TEXT_BYTES);
        assert_eq!(first.next_cursor.as_deref(), Some("a-goal"));
        let rest = goals(admin.as_ref(), Some("a-goal"), 100).unwrap();
        let ids: Vec<_> = rest.items.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids, ["parent", "z-goal"]);
        let detail = goal(admin.as_ref(), &Sessions::default(), "z-goal").unwrap();
        assert!(!detail.goal.description_truncated);

        // 父目标缺失时反思子目标仍显示在顶层，并保留其声明的父目标。
        admin.edit(|state| {
            state.goals.remove("parent");
        });
        let orphaned = goals(admin.as_ref(), None, 100).unwrap();
        let rows: Vec<_> = orphaned
            .items
            .iter()
            .map(|g| (g.id.as_str(), g.reflection_of.as_deref()))
            .collect();
        assert!(rows.contains(&(old.as_str(), Some("parent"))));
        assert!(rows.contains(&(new.as_str(), Some("parent"))));
    }

    #[test]
    fn unknown_goals_invalid_pages_and_unreadable_state_are_explicit_errors() {
        let (admin, sessions, _, _) = fixture();
        assert!(matches!(
            goal(admin.as_ref(), &sessions, "missing"),
            Err(PanelError::NotFound)
        ));
        for limit in [0, 101] {
            assert!(matches!(
                goals(admin.as_ref(), None, limit),
                Err(PanelError::InvalidInput)
            ));
        }
        *admin.broken.lock().unwrap() = true;
        assert!(matches!(
            goals(admin.as_ref(), None, 10),
            Err(PanelError::Unavailable)
        ));
        assert!(matches!(
            goal(admin.as_ref(), &sessions, "parent"),
            Err(PanelError::Unavailable)
        ));
    }
}
