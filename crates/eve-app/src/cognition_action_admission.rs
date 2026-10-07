//! 本地文档行动的宿主准入：绑定当前目标、已提交反思与实际文件观察。
//! 本模块只读取公开契约，不写认知状态，不把反思文本解释成路径或执行命令。
use crate::AppError;
use eve_action_api::{
    ACTION_SCHEMA_VERSION, DocumentActionProposal, MAX_ACTION_TIMEOUT_MS, MAX_ARTIFACT_BYTES,
    derive_action_id,
};
use eve_cognition_api::{
    COGNITION_FORMAT_VERSION, CognitionAdmin, CognitionError, CognitiveEvent, CognitiveEventKind,
    CognitiveSnapshot, CognitiveState, ExecutionCommit, FILE_OBSERVATION_CHANNEL, FileObservation,
    FileObservationInput, Goal, GoalStatus, GoalUserFeedback, SourceKind, Visibility, validate_id,
};
use eve_cognition_loop_plugin::{ReflectionArtifact, current_reflection};
use eve_llm_api::ChatRole;
use eve_session_api::{SessionKey, SessionService, SessionTurnStatus};
use ring::digest::{SHA256, digest};
use std::{collections::BTreeSet, fmt};

const SUBJECT: &str = "eve";
const INPUT_CHANNEL: &str = "cognition.cli";

/// 路径由组合层显式绑定；准入请求只携带身份、目标修订与用户确认的输入摘要。
#[derive(Clone)]
pub(crate) struct DocumentActionRequest {
    pub(crate) goal_id: String,
    pub(crate) expected_goal_revision: u64,
    pub(crate) user_id: String,
    pub(crate) input_sha256: String,
}

impl fmt::Debug for DocumentActionRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DocumentActionRequest(<redacted>)")
    }
}

pub(crate) struct PreparedDocumentAction {
    pub(crate) proposal: DocumentActionProposal,
    pub(crate) bytes: Vec<u8>,
}

/// 首次绑定或读取本地文件前调用；没有目标所有权或修订已过期时不需要文件能力。
pub(crate) fn validate_access(
    admin: &dyn CognitionAdmin,
    input: &DocumentActionRequest,
    at_ms: u64,
) -> Result<(), AppError> {
    validate_request(input, at_ms)?;
    let snapshot = checked_snapshot(admin)?;
    eligible_parent(&snapshot, input, at_ms)?;
    Ok(())
}

/// 导出真实 Session 中当前已提交反思的三个字段；父目标仍然保持 Waiting。
pub(crate) fn prepare_proposal(
    admin: &dyn CognitionAdmin,
    sessions: &dyn SessionService,
    input: &DocumentActionRequest,
    observed: &FileObservationInput,
    artifact_source_id: &str,
    at_ms: u64,
) -> Result<PreparedDocumentAction, AppError> {
    validate_request(input, at_ms)?;
    let snapshot = checked_snapshot(admin)?;
    let parent = eligible_parent(&snapshot, input, at_ms)?;
    let (event, observation) = latest_observation(&snapshot.state, parent, at_ms)?;
    match_actual_observation(input, observed, &observation, at_ms)?;
    let reflection = completed_current_reflection(&snapshot.state, parent)?;
    let execution = reflection
        .execution
        .as_ref()
        .ok_or(CognitionError::CorruptState)?;
    // 不使用通用内部用户兜底：文件行动只导出同一显式用户拥有的反思。
    let Visibility::User(reflection_user) = &reflection.visibility else {
        return Err(CognitionError::AccessDenied.into());
    };
    let key = SessionKey::new(&execution.session_id, reflection_user)?;
    let session = sessions
        .snapshot(&key)?
        .ok_or("已验证反思的会话记录缺失，未生成文档行动。")?;
    session.validate()?;
    if session.key != key {
        return Err(CognitionError::AccessDenied.into());
    }
    let turn_id = execution.turn_id.ok_or(CognitionError::CorruptState)?;
    let turn = session
        .turns
        .iter()
        .find(|turn| turn.id == turn_id)
        .ok_or("已验证反思的执行轮次缺失，未生成文档行动。")?;
    if turn.input != reflection.description {
        return Err(CognitionError::CorruptState.into());
    }
    let SessionTurnStatus::Completed { messages } = &turn.status else {
        return Err("反思会话轮次尚未提交完成，未生成文档行动。".into());
    };
    if messages.iter().any(|message| {
        message.role == ChatRole::Tool
            || !message.tool_calls.is_empty()
            || !message.tool_results.is_empty()
    }) {
        return Err("反思会话包含工具活动，不能作为文档草稿导出。".into());
    }
    let text = messages
        .last()
        .filter(|message| message.role == ChatRole::Assistant)
        .and_then(|message| message.text.as_deref())
        .ok_or(CognitionError::CorruptState)?;
    let artifact = ReflectionArtifact::parse(text)
        .map_err(|_| "已验证反思的产物结构不一致，未生成文档行动。")?;
    // 序列化已验证结构，不复制模型外围正文；不能截断成另一份未经核对的产物。
    let mut bytes = serde_json::to_vec_pretty(&artifact)?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
        return Err("反思草稿导出超过 16 KiB 上限；未截断或写入文件。".into());
    }
    let proposal = DocumentActionProposal {
        schema_version: ACTION_SCHEMA_VERSION,
        action_id: derive_action_id(
            SUBJECT,
            &input.user_id,
            &parent.id,
            parent.revision,
            &reflection.id,
        )?,
        subject_id: SUBJECT.into(),
        user_id: input.user_id.clone(),
        goal_id: parent.id.clone(),
        goal_revision: parent.revision,
        reflection_goal_id: reflection.id.clone(),
        reflection_goal_revision: reflection.revision,
        observation_event_id: event.id.clone(),
        observation_source_id: observation.observation_source_id,
        input_sha256: observation.sha256,
        input_byte_count: observation.byte_count,
        artifact_source_id: artifact_source_id.into(),
        artifact_sha256: sha256_hex(&bytes),
        artifact_byte_count: bytes.len() as u64,
        created_at_ms: at_ms,
        timeout_ms: MAX_ACTION_TIMEOUT_MS,
    };
    proposal.validate()?;
    Ok(PreparedDocumentAction { proposal, bytes })
}

/// Executing 已保存后、写入前重查；不重新读取或重跑模型会话。
/// 提案绑定的已完成反思修订若发生变化，或者文件/父目标变化，整次行动拒绝写入。
pub(crate) fn revalidate_proposal(
    admin: &dyn CognitionAdmin,
    proposal: &DocumentActionProposal,
    actual_observed: &FileObservationInput,
    now_ms: u64,
) -> Result<(), AppError> {
    proposal.validate()?;
    if proposal.subject_id != SUBJECT {
        return Err(CognitionError::SubjectMismatch.into());
    }
    if proposal.timeout_ms != MAX_ACTION_TIMEOUT_MS
        || now_ms < proposal.created_at_ms
        || proposal
            .created_at_ms
            .checked_add(proposal.timeout_ms)
            .is_none_or(|deadline| now_ms >= deadline)
    {
        return Err("文档行动的有效执行窗口已失效。".into());
    }
    let input = DocumentActionRequest {
        goal_id: proposal.goal_id.clone(),
        expected_goal_revision: proposal.goal_revision,
        user_id: proposal.user_id.clone(),
        input_sha256: proposal.input_sha256.clone(),
    };
    validate_request(&input, now_ms)?;
    let snapshot = checked_snapshot(admin)?;
    let parent = eligible_parent(&snapshot, &input, now_ms)?;
    let (event, observation) = latest_observation(&snapshot.state, parent, now_ms)?;
    match_actual_observation(&input, actual_observed, &observation, now_ms)?;
    if event.id != proposal.observation_event_id
        || observation.observation_source_id != proposal.observation_source_id
        || observation.sha256 != proposal.input_sha256
        || observation.byte_count != proposal.input_byte_count
    {
        return Err(CognitionError::StaleRevision.into());
    }
    let reflection = completed_current_reflection(&snapshot.state, parent)?;
    if reflection.id != proposal.reflection_goal_id
        || reflection.revision != proposal.reflection_goal_revision
    {
        return Err(CognitionError::StaleRevision.into());
    }
    Ok(())
}

fn validate_request(input: &DocumentActionRequest, at_ms: u64) -> Result<(), AppError> {
    validate_id(&input.goal_id)?;
    validate_id(&input.user_id)?;
    if input.expected_goal_revision == 0
        || at_ms == 0
        || input.input_sha256.len() != 64
        || !input
            .input_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(CognitionError::InvalidInput.into());
    }
    Ok(())
}

fn checked_snapshot(admin: &dyn CognitionAdmin) -> Result<CognitiveSnapshot, AppError> {
    let snapshot = admin.snapshot()?;
    if snapshot.subject_id != SUBJECT {
        return Err(CognitionError::SubjectMismatch.into());
    }
    if snapshot.format_version != COGNITION_FORMAT_VERSION {
        return Err(CognitionError::UnsupportedVersion.into());
    }
    snapshot
        .state
        .validate()
        .map_err(|_| CognitionError::CorruptState)?;
    Ok(snapshot)
}

/// 属于该用户、仍在等待且未过期的本地用户目标；不比较修订。
fn owned_waiting_parent<'a>(
    snapshot: &'a CognitiveSnapshot,
    goal_id: &str,
    user_id: &str,
    at_ms: u64,
) -> Result<&'a Goal, AppError> {
    let parent = snapshot
        .state
        .goals
        .get(goal_id)
        .filter(|goal| {
            goal.source.kind == SourceKind::User
                && goal.source.channel == INPUT_CHANNEL
                && goal.source.reference == goal.id
                && goal.verification == "user-goal:v1"
                && goal.stop_condition == "user-confirmation"
                && goal.visibility == Visibility::User(user_id.into())
        })
        .ok_or(CognitionError::AccessDenied)?;
    if parent.status != GoalStatus::Waiting
        || parent.expires_at_ms.is_some_and(|expiry| at_ms >= expiry)
    {
        return Err(CognitionError::InvalidTransition.into());
    }
    Ok(parent)
}

fn eligible_parent<'a>(
    snapshot: &'a CognitiveSnapshot,
    input: &DocumentActionRequest,
    at_ms: u64,
) -> Result<&'a Goal, AppError> {
    let parent = owned_waiting_parent(snapshot, &input.goal_id, &input.user_id, at_ms)?;
    if parent.revision != input.expected_goal_revision {
        return Err(CognitionError::StaleRevision.into());
    }
    Ok(parent)
}

/// 多步计划的当前绑定：目标当前修订与最近一次文件观察摘要，规则与文档行动准入相同。
/// 没有任何文件观察记录时摘要为空；观察记录残缺或矛盾时报错，不退回旧证据。
pub(crate) fn current_plan_binding(
    admin: &dyn CognitionAdmin,
    goal_id: &str,
    user_id: &str,
    at_ms: u64,
) -> Result<(u64, Option<String>), AppError> {
    validate_id(goal_id)?;
    validate_id(user_id)?;
    let snapshot = checked_snapshot(admin)?;
    let parent = owned_waiting_parent(&snapshot, goal_id, user_id, at_ms)?;
    let observed = snapshot.state.events.iter().any(|event| {
        event.goal_id.as_deref() == Some(goal_id)
            && event.source.channel == FILE_OBSERVATION_CHANNEL
    });
    let input = if observed {
        Some(latest_observation(&snapshot.state, parent, at_ms)?.1.sha256)
    } else {
        None
    };
    Ok((parent.revision, input))
}

fn completed_current_reflection<'a>(
    state: &'a CognitiveState,
    parent: &Goal,
) -> Result<&'a Goal, AppError> {
    let reflection = current_reflection(state, SUBJECT, parent)?
        .ok_or("当前目标修订还没有反思草稿，未生成文档行动。")?;
    if reflection.status != GoalStatus::Completed
        || reflection.visibility != parent.visibility
        || reflection.revision == 0
        || !reflection.feedback.as_ref().is_some_and(|feedback| {
            feedback.commit == ExecutionCommit::Completed
                && feedback.verification_met
                && feedback.started_tools == Some(0)
        })
    {
        return Err("当前反思尚无已提交且零工具的有效产物，未生成文档行动。".into());
    }
    Ok(reflection)
}

fn latest_observation<'a>(
    state: &'a CognitiveState,
    parent: &Goal,
    at_ms: u64,
) -> Result<(&'a CognitiveEvent, FileObservation), AppError> {
    let mut latest: Option<(&CognitiveEvent, FileObservation)> = None;
    let mut observation_revisions = BTreeSet::new();
    for (index, event) in state.events.iter().enumerate().filter(|(_, event)| {
        event.goal_id.as_deref() == Some(parent.id.as_str())
            && event.source.channel == FILE_OBSERVATION_CHANNEL
    }) {
        let observation =
            FileObservation::parse(&event.summary).map_err(|_| CognitionError::CorruptState)?;
        let expected_cause = state.events[..index]
            .iter()
            .rev()
            .find(|cause| {
                cause.goal_id.as_deref() == Some(parent.id.as_str())
                    && parent.visibility.restricts(&cause.visibility)
            })
            .map(|cause| cause.id.as_str());
        if event.kind != CognitiveEventKind::ExternalInput
            || event.source.kind != SourceKind::Environment
            || event.source.reference != observation.observation_source_id
            || event.visibility != parent.visibility
            || event.caused_by.as_deref() != expected_cause
            || event.at_ms != observation.observed_at_ms
            || observation.observed_at_ms > at_ms
            || observation.goal_id != parent.id
            || observation.goal_revision > parent.revision
            || event.id != observation.event_id(SUBJECT)?
            || (observation.goal_revision == parent.revision
                && parent.wait_reason.as_deref() != Some(event.summary.as_str()))
            || !observation_revisions.insert(observation.goal_revision)
            || latest
                .as_ref()
                .is_some_and(|(_, previous)| previous.goal_revision >= observation.goal_revision)
        {
            return Err(CognitionError::CorruptState.into());
        }
        latest = Some((event, observation));
    }
    if state.events.iter().any(|event| {
        event.goal_id.as_deref() == Some(parent.id.as_str())
            && event.kind == CognitiveEventKind::ExternalInput
            && event.source.kind == SourceKind::User
            && GoalUserFeedback::parse(&event.summary).is_ok_and(|feedback| {
                feedback.goal_id == parent.id
                    && observation_revisions.contains(&feedback.goal_revision)
            })
    }) {
        return Err(CognitionError::CorruptState.into());
    }
    latest.ok_or_else(|| "当前目标没有完整的文件观察证据，未生成文档行动。".into())
}

fn match_actual_observation(
    input: &DocumentActionRequest,
    actual: &FileObservationInput,
    saved: &FileObservation,
    at_ms: u64,
) -> Result<(), AppError> {
    actual.validate()?;
    if actual.goal_id != input.goal_id
        || actual.expected_goal_revision != input.expected_goal_revision
        || actual.observed_at_ms > at_ms
        || actual.observation_source_id != saved.observation_source_id
        || actual.sha256 != input.input_sha256
        || actual.sha256 != saved.sha256
        || actual.byte_count != saved.byte_count
        || actual.text_excerpt != saved.text_excerpt
        || actual.text_truncated != saved.text_truncated
    {
        return Err("实际输入文件与确认的摘要或当前观察证据不一致，未执行文档行动。".into());
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_cognition_api::{
        CognitionReader, CognitionResult, ExecutionAttempt, ExecutionBudget,
        FILE_OBSERVATION_SCHEMA_VERSION, Feedback, ReadAccess, Source,
    };
    use eve_cognition_loop_api::{AllowedSource, EndogenousOptions, ExecutionScope};
    use eve_cognition_loop_plugin::EndogenousPlanner;
    use eve_llm_api::ChatMessage;
    use eve_session_api::{
        SessionError, SessionFailure, SessionInput, SessionResult, SessionSnapshot, SessionTurn,
        StartedTurn, TurnLease,
    };
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    const ARTIFACT: &str =
        r#"{"summary":"已整理文件内容。","next_step":"请核对原始目标。","needs_user_input":true}"#;

    struct MemoryAdmin(Mutex<CognitiveSnapshot>);
    impl CognitionAdmin for MemoryAdmin {
        fn snapshot(&self) -> CognitionResult<CognitiveSnapshot> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn reader(&self, _: ReadAccess) -> CognitionResult<Arc<dyn CognitionReader>> {
            Err(CognitionError::Unavailable)
        }
        fn replace(
            &self,
            expected_revision: u64,
            mut state: CognitiveState,
        ) -> CognitionResult<CognitiveSnapshot> {
            let mut snapshot = self.0.lock().unwrap();
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

    struct SavedSessions {
        value: Mutex<SessionSnapshot>,
        reads: AtomicUsize,
    }
    impl SessionService for SavedSessions {
        fn snapshot(&self, _: &SessionKey) -> SessionResult<Option<SessionSnapshot>> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(Some(self.value.lock().unwrap().clone()))
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

    struct Fixture {
        admin: Arc<MemoryAdmin>,
        sessions: SavedSessions,
        request: DocumentActionRequest,
        observed: FileObservationInput,
        artifact_source: String,
        reflection_id: String,
    }
    impl Fixture {
        fn prepare(&self) -> Result<PreparedDocumentAction, AppError> {
            prepare_proposal(
                self.admin.as_ref(),
                &self.sessions,
                &self.request,
                &self.observed,
                &self.artifact_source,
                50,
            )
        }
    }

    fn fixture() -> Fixture {
        let observation = FileObservation {
            schema_version: FILE_OBSERVATION_SCHEMA_VERSION,
            goal_id: "parent".into(),
            observation_source_id: format!("file-source:{}", "a".repeat(64)),
            sha256: sha256_hex(b"source evidence"),
            byte_count: 15,
            text_excerpt: "source evidence".into(),
            text_truncated: false,
            observed_at_ms: 20,
            previous_goal_revision: 1,
            goal_revision: 2,
        };
        let source = Source {
            kind: SourceKind::User,
            channel: INPUT_CHANNEL.into(),
            reference: "parent".into(),
        };
        let visibility = Visibility::User("owner".into());
        let parent = Goal {
            id: "parent".into(),
            revision: 2,
            source: source.clone(),
            visibility: visibility.clone(),
            description: "基于给定文件起草下一步。".into(),
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
            wait_reason: Some(observation.to_json().unwrap()),
            block_reason: None,
            execution: None,
            feedback: None,
        };
        let mut state = CognitiveState::default();
        state.goals.insert(parent.id.clone(), parent);
        state.events = vec![
            CognitiveEvent {
                id: "input".into(),
                kind: CognitiveEventKind::ExternalInput,
                source,
                visibility: visibility.clone(),
                goal_id: Some("parent".into()),
                caused_by: None,
                at_ms: 10,
                summary: "用户输入".into(),
            },
            CognitiveEvent {
                id: observation.event_id(SUBJECT).unwrap(),
                kind: CognitiveEventKind::ExternalInput,
                source: Source {
                    kind: SourceKind::Environment,
                    channel: FILE_OBSERVATION_CHANNEL.into(),
                    reference: observation.observation_source_id.clone(),
                },
                visibility,
                goal_id: Some("parent".into()),
                caused_by: Some("input".into()),
                at_ms: 20,
                summary: observation.to_json().unwrap(),
            },
        ];
        let admin = Arc::new(MemoryAdmin(Mutex::new(CognitiveSnapshot {
            format_version: COGNITION_FORMAT_VERSION,
            subject_id: SUBJECT.into(),
            revision: 2,
            state,
        })));
        let planner = EndogenousPlanner::new(
            admin.clone(),
            EndogenousOptions {
                scope: ExecutionScope {
                    subject_id: SUBJECT.into(),
                    access: ReadAccess::Internal,
                    sources: vec![AllowedSource {
                        kind: SourceKind::User,
                        channel: INPUT_CHANNEL.into(),
                    }],
                },
                max_derivations: 1,
                timeout_ms: 30_000,
            },
        )
        .unwrap();
        let reflection_id = planner.reconcile(30).unwrap().created_goal_ids.remove(0);
        let mut snapshot = admin.0.lock().unwrap();
        let child = snapshot.state.goals.get_mut(&reflection_id).unwrap();
        child.status = GoalStatus::Completed;
        child.revision = 3;
        child.execution = Some(ExecutionAttempt {
            attempt_id: "attempt".into(),
            session_id: "reflection-session".into(),
            task_id: "reflection-task".into(),
            turn_id: Some(1),
            started_at_ms: 35,
        });
        child.feedback = Some(Feedback {
            commit: ExecutionCommit::Completed,
            verification_met: true,
            started_tools: Some(0),
            summary: "结构已验证".into(),
            at_ms: 40,
        });
        let description = child.description.clone();
        snapshot.revision += 2;
        drop(snapshot);
        let session = SessionSnapshot {
            key: SessionKey::new("reflection-session", "owner").unwrap(),
            revision: 2,
            turns: vec![SessionTurn {
                id: 1,
                input: description.clone(),
                status: SessionTurnStatus::Completed {
                    messages: vec![
                        ChatMessage::text(ChatRole::User, description),
                        ChatMessage::text(ChatRole::Assistant, ARTIFACT),
                    ],
                },
            }],
        };
        Fixture {
            admin,
            sessions: SavedSessions {
                value: Mutex::new(session),
                reads: AtomicUsize::new(0),
            },
            request: DocumentActionRequest {
                goal_id: "parent".into(),
                expected_goal_revision: 2,
                user_id: "owner".into(),
                input_sha256: observation.sha256.clone(),
            },
            observed: FileObservationInput {
                goal_id: "parent".into(),
                expected_goal_revision: 2,
                observation_source_id: observation.observation_source_id,
                sha256: observation.sha256,
                byte_count: observation.byte_count,
                text_excerpt: observation.text_excerpt,
                text_truncated: observation.text_truncated,
                observed_at_ms: 45,
            },
            artifact_source: format!("artifact-file:{}", "b".repeat(64)),
            reflection_id,
        }
    }

    #[test]
    fn admission_exports_exact_saved_artifact_without_modifying_parent() {
        let f = fixture();
        let before = f.admin.snapshot().unwrap();
        validate_access(f.admin.as_ref(), &f.request, 45).unwrap();
        let prepared = f.prepare().unwrap();
        let artifact = ReflectionArtifact::parse(ARTIFACT).unwrap();
        let mut expected = serde_json::to_vec_pretty(&artifact).unwrap();
        expected.push(b'\n');
        assert_eq!(prepared.bytes, expected);
        assert_eq!(prepared.proposal.artifact_sha256, sha256_hex(&expected));
        assert_eq!(prepared.proposal.goal_revision, 2);
        assert_eq!(prepared.proposal.reflection_goal_revision, 3);
        assert_eq!(prepared.proposal.timeout_ms, 30_000);
        assert_eq!(f.admin.snapshot().unwrap(), before);
        revalidate_proposal(f.admin.as_ref(), &prepared.proposal, &f.observed, 60).unwrap();
        assert_eq!(f.sessions.reads.load(Ordering::Relaxed), 1);
        assert_eq!(f.admin.snapshot().unwrap(), before);
    }

    #[test]
    fn foreign_owner_revision_and_unconfirmed_input_stop_before_session_read() {
        for change in 0..4 {
            let mut f = fixture();
            match change {
                0 => f.request.user_id = "another-user".into(),
                1 => f.request.expected_goal_revision = 1,
                2 => f.request.input_sha256 = "c".repeat(64),
                _ => f.observed.observation_source_id = format!("file-source:{}", "c".repeat(64)),
            }
            assert!(f.prepare().is_err());
            assert_eq!(f.sessions.reads.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn corrupted_observation_provenance_never_falls_back_to_older_evidence() {
        for change in 0..7 {
            let f = fixture();
            let mut snapshot = f.admin.0.lock().unwrap();
            let event = &mut snapshot.state.events[1];
            match change {
                0 => event.source.kind = SourceKind::User,
                1 => event.visibility = Visibility::Internal,
                2 => event.caused_by = None,
                3 => event.source.reference = format!("file-source:{}", "c".repeat(64)),
                4 => event.at_ms = 21,
                5 => event.id = format!("file-observation:{}", "c".repeat(64)),
                _ => {
                    snapshot.state.goals.get_mut("parent").unwrap().wait_reason =
                        Some("没有相同修订的观察内容".into());
                }
            }
            drop(snapshot);
            assert!(f.prepare().is_err());
            assert_eq!(f.sessions.reads.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn committed_reflection_does_not_substitute_for_actual_completed_session() {
        for change in 0..4 {
            let f = fixture();
            let mut session = f.sessions.value.lock().unwrap();
            match change {
                0 => session.key.user_id = "another-user".into(),
                1 => {
                    session.turns[0].status = SessionTurnStatus::Interrupted;
                }
                2 => {
                    session.turns[0].input = "别的输入".into();
                    let SessionTurnStatus::Completed { messages } = &mut session.turns[0].status
                    else {
                        unreachable!()
                    };
                    messages[0].text = Some("别的输入".into());
                }
                _ => {
                    let SessionTurnStatus::Completed { messages } = &mut session.turns[0].status
                    else {
                        unreachable!()
                    };
                    messages[1].text = Some(
                        r#"{"summary":"x","next_step":"y","needs_user_input":false,"completed":true}"#
                            .into(),
                    );
                }
            }
            drop(session);
            assert!(f.prepare().is_err());
        }
    }

    #[test]
    fn changed_parent_reflection_input_and_expired_window_block_revalidation() {
        for change in 0..5 {
            let mut f = fixture();
            let prepared = f.prepare().unwrap();
            let mut now = 60;
            match change {
                0 => {
                    f.admin
                        .0
                        .lock()
                        .unwrap()
                        .state
                        .goals
                        .get_mut("parent")
                        .unwrap()
                        .revision += 1
                }
                1 => {
                    f.admin
                        .0
                        .lock()
                        .unwrap()
                        .state
                        .goals
                        .get_mut(&f.reflection_id)
                        .unwrap()
                        .revision += 1;
                }
                2 => f.observed.sha256 = "c".repeat(64),
                3 => now = 30_050,
                _ => now = 49,
            }
            assert!(
                revalidate_proposal(f.admin.as_ref(), &prepared.proposal, &f.observed, now)
                    .is_err()
            );
            assert_eq!(f.sessions.reads.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn escaped_output_over_limit_is_rejected_without_truncating_verified_text() {
        let f = fixture();
        let oversized = serde_json::json!({
            "summary": "\u{0001}".repeat(4000),
            "next_step": "请核对。",
            "needs_user_input": true
        })
        .to_string();
        assert!(ReflectionArtifact::parse(&oversized).is_ok());
        let mut session = f.sessions.value.lock().unwrap();
        let SessionTurnStatus::Completed { messages } = &mut session.turns[0].status else {
            unreachable!()
        };
        messages[1].text = Some(oversized);
        drop(session);
        assert!(f.prepare().is_err());
        assert_eq!(
            f.admin.snapshot().unwrap().state.goals["parent"].status,
            GoalStatus::Waiting
        );
    }
}
