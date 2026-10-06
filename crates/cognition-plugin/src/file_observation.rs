use eve_cognition_api::*;
use std::sync::Arc;

/// 宿主专用观察接收端；只更新其绑定用户的 Waiting 原始目标。
/// 不读取文件、不接收路径、不发布 CognitionAdmin，也不调用模型或工具。
pub struct UserGoalFileObservation {
    admin: Arc<dyn CognitionAdmin>,
    subject_id: String,
    user_id: String,
    parent_channel: String,
}
impl UserGoalFileObservation {
    pub fn new(
        admin: Arc<dyn CognitionAdmin>,
        subject_id: String,
        user_id: String,
        parent_channel: String,
    ) -> CognitionResult<Self> {
        for id in [&subject_id, &user_id, &parent_channel] {
            validate_id(id)?;
        }
        if parent_channel == FILE_OBSERVATION_CHANNEL {
            return Err(CognitionError::InvalidInput);
        }
        Ok(Self {
            admin,
            subject_id,
            user_id,
            parent_channel,
        })
    }

    fn owns(&self, goal: &Goal) -> bool {
        goal.source.kind == SourceKind::User
            && goal.source.channel == self.parent_channel
            && goal.visibility == Visibility::User(self.user_id.clone())
            && goal.verification == "user-goal:v1"
    }
}
impl FileObservationService for UserGoalFileObservation {
    fn submit(&self, input: FileObservationInput) -> CognitionResult<FileObservationReport> {
        input.validate()?;
        let snapshot = self.admin.snapshot()?;
        if snapshot.subject_id != self.subject_id {
            return Err(CognitionError::SubjectMismatch);
        }
        let parent = snapshot
            .state
            .goals
            .get(&input.goal_id)
            .filter(|goal| self.owns(goal))
            .ok_or(CognitionError::AccessDenied)?;
        let source = Source {
            kind: SourceKind::Environment,
            channel: FILE_OBSERVATION_CHANNEL.into(),
            reference: input.observation_source_id.clone(),
        };
        let visibility = Visibility::User(self.user_id.clone());
        // 检查目标最近一次文件观察，而非 wait_reason；后续用户事实不会被相同文件覆盖。
        // 切换来源（包括切回曾观察过的来源）必须提交新上下文，不能命中更早的历史记录。
        if let Some(event) = snapshot.state.events.iter().rev().find(|event| {
            event.goal_id.as_ref() == Some(&input.goal_id)
                && event.source.channel == FILE_OBSERVATION_CHANNEL
        }) {
            let observation =
                FileObservation::parse(&event.summary).map_err(|_| CognitionError::CorruptState)?;
            if event.kind != CognitiveEventKind::ExternalInput
                || event.source.kind != SourceKind::Environment
                || event.source.reference != observation.observation_source_id
                || event.visibility != visibility
                || observation.goal_id != input.goal_id
                || event.at_ms != observation.observed_at_ms
                || observation.goal_revision > parent.revision
                || event.id != observation.event_id(&self.subject_id)?
            {
                return Err(CognitionError::CorruptState);
            }
            if observation.observation_source_id == input.observation_source_id
                && observation.sha256 == input.sha256
                && observation.byte_count == input.byte_count
                && observation.text_excerpt == input.text_excerpt
                && observation.text_truncated == input.text_truncated
            {
                return Ok(FileObservationReport {
                    goal_id: input.goal_id,
                    goal_revision: observation.goal_revision,
                    revision: snapshot.revision,
                    duplicate: true,
                });
            }
        }
        if parent.status != GoalStatus::Waiting
            || parent
                .expires_at_ms
                .is_some_and(|expiry| input.observed_at_ms >= expiry)
        {
            return Err(CognitionError::InvalidTransition);
        }
        if parent.revision != input.expected_goal_revision {
            return Err(CognitionError::StaleRevision);
        }
        if snapshot.state.events.len() >= MAX_RECORDS {
            return Err(CognitionError::LimitReached);
        }
        let observation = FileObservation {
            schema_version: FILE_OBSERVATION_SCHEMA_VERSION,
            goal_id: input.goal_id.clone(),
            observation_source_id: input.observation_source_id,
            sha256: input.sha256,
            byte_count: input.byte_count,
            text_excerpt: input.text_excerpt,
            text_truncated: input.text_truncated,
            observed_at_ms: input.observed_at_ms,
            previous_goal_revision: parent.revision,
            goal_revision: parent
                .revision
                .checked_add(1)
                .ok_or(CognitionError::LimitReached)?,
        };
        let encoded = observation.to_json()?;
        let event_id = observation.event_id(&self.subject_id)?;
        if snapshot
            .state
            .events
            .iter()
            .any(|event| event.id == event_id)
        {
            return Err(CognitionError::CorruptState);
        }
        let cause = snapshot
            .state
            .events
            .iter()
            .rev()
            .find(|event| {
                event.goal_id.as_ref() == Some(&input.goal_id)
                    && visibility.restricts(&event.visibility)
            })
            .map(|event| event.id.clone());
        let mut state = snapshot.state;
        state
            .goals
            .get_mut(&input.goal_id)
            .ok_or(CognitionError::InvalidInput)?
            .wait_reason = Some(encoded.clone());
        state.events.push(CognitiveEvent {
            id: event_id,
            kind: CognitiveEventKind::ExternalInput,
            source,
            visibility,
            goal_id: Some(input.goal_id.clone()),
            caused_by: cause,
            at_ms: input.observed_at_ms,
            summary: encoded,
        });
        let saved = self.admin.replace(snapshot.revision, state)?;
        Ok(FileObservationReport {
            goal_id: input.goal_id,
            goal_revision: observation.goal_revision,
            revision: saved.revision,
            duplicate: false,
        })
    }
}
