use eve_cognition_api::*;
use std::sync::Arc;

/// 只更新宿主绑定用户的 Waiting 用户待办；不发布管理能力、不调用模型或工具。
pub struct UserGoalFeedback {
    admin: Arc<dyn CognitionAdmin>,
    subject_id: String,
    user_id: String,
    parent_channel: String,
    feedback_channel: String,
}
impl UserGoalFeedback {
    pub fn new(
        admin: Arc<dyn CognitionAdmin>,
        subject_id: String,
        user_id: String,
        parent_channel: String,
        feedback_channel: String,
    ) -> CognitionResult<Self> {
        for id in [&subject_id, &user_id, &parent_channel, &feedback_channel] {
            validate_id(id)?;
        }
        if parent_channel == feedback_channel {
            return Err(CognitionError::InvalidInput);
        }
        Ok(Self {
            admin,
            subject_id,
            user_id,
            parent_channel,
            feedback_channel,
        })
    }

    fn owns(&self, goal: &Goal) -> bool {
        goal.source.kind == SourceKind::User
            && goal.source.channel == self.parent_channel
            && goal.visibility == Visibility::User(self.user_id.clone())
            && goal.verification == "user-goal:v1"
    }
}
impl GoalFeedbackService for UserGoalFeedback {
    fn submit(&self, input: GoalFeedbackInput) -> CognitionResult<GoalFeedbackReport> {
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
            kind: SourceKind::User,
            channel: self.feedback_channel.clone(),
            reference: input.feedback_id.clone(),
        };
        let visibility = Visibility::User(self.user_id.clone());
        // 包括其它来源占用相同标识，均先比对完整来源与原始输入，不能覆盖历史。
        if let Some(event) = snapshot
            .state
            .events
            .iter()
            .find(|event| event.id == input.feedback_id)
        {
            if event.kind != CognitiveEventKind::ExternalInput
                || event.source != source
                || event.visibility != visibility
                || event.goal_id.as_ref() != Some(&input.goal_id)
            {
                return Err(CognitionError::InvalidInput);
            }
            let payload = GoalUserFeedback::parse(&event.summary)
                .map_err(|_| CognitionError::InvalidInput)?;
            if payload.goal_id != input.goal_id
                || payload.feedback_id != input.feedback_id
                || payload.text != input.text
                || payload.goal_revision > parent.revision
            {
                return Err(CognitionError::InvalidInput);
            }
            return Ok(GoalFeedbackReport {
                goal_id: input.goal_id,
                goal_revision: payload.goal_revision,
                revision: snapshot.revision,
                duplicate: true,
            });
        }
        if parent.status != GoalStatus::Waiting
            || parent
                .expires_at_ms
                .is_some_and(|expiry| input.at_ms >= expiry)
        {
            return Err(CognitionError::InvalidTransition);
        }
        if parent.revision != input.expected_goal_revision {
            return Err(CognitionError::StaleRevision);
        }
        if snapshot.state.events.len() >= MAX_RECORDS {
            return Err(CognitionError::LimitReached);
        }
        let payload = GoalUserFeedback {
            schema_version: GOAL_USER_FEEDBACK_SCHEMA_VERSION,
            goal_id: input.goal_id.clone(),
            previous_goal_revision: parent.revision,
            goal_revision: parent
                .revision
                .checked_add(1)
                .ok_or(CognitionError::LimitReached)?,
            feedback_id: input.feedback_id.clone(),
            text: input.text,
        };
        let encoded = payload.to_json()?;
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
            id: input.feedback_id,
            kind: CognitiveEventKind::ExternalInput,
            source,
            visibility,
            goal_id: Some(input.goal_id.clone()),
            caused_by: cause,
            at_ms: input.at_ms,
            summary: encoded,
        });
        let saved = self.admin.replace(snapshot.revision, state)?;
        Ok(GoalFeedbackReport {
            goal_id: input.goal_id,
            goal_revision: payload.goal_revision,
            revision: saved.revision,
            duplicate: false,
        })
    }
}
