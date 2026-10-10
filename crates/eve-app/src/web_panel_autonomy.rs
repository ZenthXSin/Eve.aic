//! 兴趣、领域知识、实践、技能与主动邀请到面板只读契约的投影；只调用各账本的读取方法。
use eve_interest_api::{InterestAdmin, InterestStatus, StatementKind};
use eve_knowledge_api::{KnowledgeAdmin, KnowledgeStatus};
use eve_memory_api::MemoryScope;
use eve_outreach_api::{InvitationStatus, OutreachAdmin, ResponseKind};
use eve_practice_api::{PracticeAdmin, PracticeStatus};
use eve_skill_api::{InvocationOutcome, SkillAdmin};
use eve_web_panel_api::*;
use std::sync::Arc;

const QUOTES: usize = 3;
const KNOWLEDGE: usize = 24;
const PRACTICE: usize = 12;
const INVITATIONS: usize = 12;

/// 宿主持有的各账本管理能力只经这里的读取方法进入面板。
pub(crate) struct AutonomyRead {
    pub(crate) interests: Arc<dyn InterestAdmin>,
    pub(crate) knowledge: Option<Arc<dyn KnowledgeAdmin>>,
    pub(crate) practice: Option<Arc<dyn PracticeAdmin>>,
    pub(crate) skills: Option<Arc<dyn SkillAdmin>>,
    pub(crate) outreach: Option<Arc<dyn OutreachAdmin>>,
}

pub(crate) fn autonomy(read: &AutonomyRead, scope: &MemoryScope) -> PanelResult<AutonomyView> {
    let owner = scope.user_id.as_str();
    let interests = read
        .interests
        .snapshot(scope)
        .map_err(|_| PanelError::Unavailable)?;
    let interests = interests
        .interests
        .iter()
        .map(|interest| AutonomyInterest {
            id: interest.id.clone(),
            topic: interest.topic.clone(),
            status: match interest.status {
                InterestStatus::Active => "active",
                InterestStatus::Withdrawn => "withdrawn",
            },
            quotes: interest
                .statements
                .iter()
                .filter(|statement| statement.kind != StatementKind::Withdrawal)
                .take(QUOTES)
                .map(|statement| statement.quote.clone())
                .collect(),
            updated_at_ms: interest.updated_at_ms,
        })
        .collect();
    let mut knowledge = Vec::new();
    if let Some(admin) = &read.knowledge {
        let snapshot = admin.snapshot().map_err(|_| PanelError::Unavailable)?;
        let mut entries: Vec<_> = snapshot
            .entries
            .iter()
            .filter(|entry| entry.owner == owner)
            .collect();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.created_at_ms));
        knowledge = entries
            .into_iter()
            .take(KNOWLEDGE)
            .map(|entry| AutonomyKnowledge {
                statement: entry.statement.clone(),
                source_quoted: entry.status == KnowledgeStatus::SourceQuoted,
                url: entry
                    .source
                    .as_ref()
                    .and_then(|source| snapshot.document(&source.document_id))
                    .map(|document| document.url.clone()),
                version: entry.version.clone(),
            })
            .collect();
    }
    let mut practice = Vec::new();
    if let Some(admin) = &read.practice {
        let snapshot = admin.snapshot().map_err(|_| PanelError::Unavailable)?;
        let mut runs: Vec<_> = snapshot
            .runs
            .iter()
            .filter(|run| run.task.owner == owner)
            .collect();
        runs.sort_by_key(|run| std::cmp::Reverse(run.started_at_ms));
        practice = runs
            .into_iter()
            .take(PRACTICE)
            .map(|run| {
                let evidence = run
                    .attempts
                    .iter()
                    .rev()
                    .find_map(|attempt| attempt.evidence.as_ref());
                AutonomyPractice {
                    id: run.id.clone(),
                    follow_up: run.task.goal_id.starts_with("eve.outreach.request."),
                    status: match run.status {
                        PracticeStatus::Running => "running",
                        PracticeStatus::Verified => "verified",
                        PracticeStatus::Unverified => "unverified",
                        PracticeStatus::NotApplicable => "not_applicable",
                        PracticeStatus::Failed(_) => "failed",
                        PracticeStatus::Interrupted => "interrupted",
                    },
                    attempts: run.attempts.len(),
                    runtime_version: evidence
                        .map(|evidence| evidence.runtime_version.clone())
                        .filter(|version| !version.is_empty()),
                    probes_passed: evidence.map_or(0, |evidence| {
                        evidence
                            .probes
                            .iter()
                            .filter(|result| result.passed)
                            .count()
                    }),
                    probes_total: evidence.map_or(0, |evidence| evidence.probes.len()),
                    started_at_ms: run.started_at_ms,
                }
            })
            .collect();
    }
    let mut skills = Vec::new();
    if let Some(admin) = &read.skills {
        let snapshot = admin.snapshot().map_err(|_| PanelError::Unavailable)?;
        skills = snapshot
            .skills_for(owner)
            .map(|skill| {
                let invocations: Vec<_> = snapshot.invocations(&skill.id).collect();
                let calls: Vec<_> = snapshot
                    .tool_calls
                    .iter()
                    .filter(|call| call.skill.skill_id == skill.id)
                    .collect();
                AutonomySkill {
                    id: skill.id.clone(),
                    name: skill.name.clone(),
                    enabled: skill.enabled,
                    versions: skill.versions.len(),
                    task_invocations: invocations.len(),
                    task_verified: invocations
                        .iter()
                        .filter(|selection| selection.outcome == Some(InvocationOutcome::Verified))
                        .count(),
                    tool_calls: calls.len(),
                    tool_verified: calls
                        .iter()
                        .filter(|call| call.outcome == Some(InvocationOutcome::Verified))
                        .count(),
                }
            })
            .collect();
    }
    let mut invitations = Vec::new();
    if let Some(admin) = &read.outreach {
        let snapshot = admin.snapshot().map_err(|_| PanelError::Unavailable)?;
        let mut owned: Vec<_> = snapshot.for_owner(owner).collect();
        owned.sort_by_key(|invitation| std::cmp::Reverse(invitation.created_at_ms));
        invitations = owned
            .into_iter()
            .take(INVITATIONS)
            .map(|invitation| {
                let feedback = invitation.feedback();
                AutonomyInvitation {
                    id: invitation.id.clone(),
                    status: match invitation.status {
                        InvitationStatus::Composing => "composing",
                        InvitationStatus::Pending => "pending",
                        InvitationStatus::Delivering => "delivering",
                        InvitationStatus::Delivered => "delivered",
                        InvitationStatus::Unknown => "unknown",
                        InvitationStatus::Cancelled(_) => "cancelled",
                        InvitationStatus::Failed(_) => "failed",
                        InvitationStatus::Interrupted => "interrupted",
                    },
                    text: invitation.text.clone(),
                    delivered_at_ms: invitation.delivered_at_ms,
                    feedback: feedback.map(|verdict| match verdict.kind {
                        ResponseKind::Request => "request",
                        ResponseKind::Interested => "interested",
                        ResponseKind::Declined => "declined",
                        ResponseKind::BadTiming => "bad_timing",
                        ResponseKind::Unrelated => "unrelated",
                    }),
                    quote: feedback.and_then(|verdict| verdict.quote.clone()),
                }
            })
            .collect();
    }
    Ok(AutonomyView {
        scope: scope.clone(),
        interests,
        knowledge,
        practice,
        skills,
        invitations,
    })
}
