//! 交流经验的持久状态机；固定批次、真实摘录、有限重试与反例停用。
use super::{error, expression::hash, valid};
use eve_llm_api::ContextScope;
use eve_plugin_api::PluginResult;
use eve_training_api::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
mod persistent;
pub(crate) use persistent::PersistentConversations;

pub(crate) const KEY: &str = "conversation.v1";
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_SCOPES: usize = 128;
const MAX_SEEN: usize = 256;
const RETRY_MS: u64 = 60_000;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Seen {
    id: String,
    body: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scope {
    scope: ContextScope,
    messages: Vec<ConversationMessage>,
    seen: Vec<Seen>,
    pending: usize,
    generation: u64,
    ready_at: u64,
    inflight: Option<ConversationBatch>,
    lessons: Vec<ConversationLesson>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    history: Vec<ConversationLessonRevision>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Conversations {
    version: u8,
    next: u64,
    scopes: Vec<Scope>,
}

fn identifier(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 256
        && text.trim() == text
        && !text.chars().any(char::is_control)
}
fn text(text: &str, limit: usize) -> bool {
    !text.trim().is_empty()
        && text.len() <= limit
        && !text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
}
fn digest_valid(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn message_valid(m: &ConversationMessage) -> bool {
    conversation_message_valid(m)
}
fn lesson_valid(l: &ConversationLesson) -> bool {
    identifier(&l.id)
        && text(&l.situation, 240)
        && text(&l.response, 480)
        && l.avoid.len() <= 480
        && l.evidence.len() >= 2
        && l.evidence.len() <= 4
        && l.evidence
            .iter()
            .all(|e| identifier(&e.message_id) && text(&e.quote, 240))
        && l.evaluated_message_ids.len() <= 64
        && l.evaluated_message_ids.iter().all(|id| identifier(id))
        && (!l.active || l.counterexamples == 0 && (l.observations >= 2 || l.corrections > 0))
}
fn batch_valid(b: &ConversationBatch) -> bool {
    identifier(&b.id)
        && valid(&b.scope)
        && (1..=3).contains(&b.attempt)
        && (2..=MAX_CONVERSATION_MESSAGES).contains(&b.messages.len())
        && b.messages.iter().all(message_valid)
        && b.messages
            .iter()
            .map(|m| &m.id)
            .collect::<BTreeSet<_>>()
            .len()
            == b.messages.len()
        && b.lessons.len() <= MAX_CONVERSATION_LESSONS
        && b.lessons.iter().all(lesson_valid)
}
impl Conversations {
    pub(crate) fn load(bytes: Option<Vec<u8>>) -> PluginResult<Self> {
        let Some(bytes) = bytes else {
            return Ok(Self {
                version: 1,
                next: 1,
                scopes: vec![],
            });
        };
        if bytes.len() > MAX_BYTES {
            return Err(error("交流经验容量异常；保留原记录"));
        }
        let value: Self =
            serde_json::from_slice(&bytes).map_err(|_| error("交流经验损坏；保留原记录"))?;
        let mut scopes = BTreeSet::new();
        if value.version != 1
            || value.next == 0
            || value.scopes.len() > MAX_SCOPES
            || value.scopes.iter().any(|s| {
                !valid(&s.scope)
                    || !scopes.insert((&s.scope.session_id, &s.scope.user_id))
                    || s.generation == 0
                    || s.messages.len() > MAX_CONVERSATION_MESSAGES
                    || s.seen.len() > MAX_SEEN
                    || s.pending > MAX_CONVERSATION_MESSAGES
                    || !s.messages.iter().all(message_valid)
                    || s.messages
                        .iter()
                        .map(|m| &m.id)
                        .collect::<BTreeSet<_>>()
                        .len()
                        != s.messages.len()
                    || s.seen
                        .iter()
                        .any(|r| !digest_valid(&r.id) || !digest_valid(&r.body))
                    || s.seen.iter().map(|r| &r.id).collect::<BTreeSet<_>>().len() != s.seen.len()
                    || s.lessons.len() > MAX_CONVERSATION_LESSONS
                    || !s.lessons.iter().all(lesson_valid)
                    || s.history.iter().any(|r| !lesson_valid(&r.lesson))
                    || s.lessons
                        .iter()
                        .map(|l| &l.id)
                        .collect::<BTreeSet<_>>()
                        .len()
                        != s.lessons.len()
                    || s.inflight
                        .as_ref()
                        .is_some_and(|b| !batch_valid(b) || b.scope != s.scope)
            })
        {
            return Err(error("交流经验版本或内容无效；未清空"));
        }
        Ok(value)
    }
    pub(crate) fn bytes(&self) -> PluginResult<Vec<u8>> {
        let bytes = serde_json::to_vec(self).map_err(|_| error("交流经验编码失败"))?;
        if bytes.len() > MAX_BYTES {
            return Err(error("交流经验容量已满；保留原记录"));
        }
        Ok(bytes)
    }
    pub(crate) fn observe(
        &mut self,
        scope: &ContextScope,
        message: ConversationMessage,
        enabled: bool,
    ) -> PluginResult<bool> {
        if !valid(scope) || !message_valid(&message) {
            return Err(error("交流样本身份或正文无效"));
        }
        let mut core = message.clone();
        core.metadata = None;
        let body = hash(&serde_json::to_vec(&core).map_err(|_| error("交流样本编码失败"))?);
        let id = hash(message.id.as_bytes());
        let index = match self.scopes.iter().position(|s| s.scope == *scope) {
            Some(index) => index,
            None => {
                if self.scopes.len() >= MAX_SCOPES {
                    return Err(error("交流学习范围容量已满；保留原记录"));
                }
                self.scopes.push(Scope {
                    scope: scope.clone(),
                    messages: vec![],
                    seen: vec![],
                    pending: 0,
                    generation: 1,
                    ready_at: 0,
                    inflight: None,
                    lessons: vec![],
                    history: vec![],
                });
                self.scopes.len() - 1
            }
        };
        let s = &mut self.scopes[index];
        if let Some(previous) = s.seen.iter().find(|r| r.id == id) {
            return if previous.body == body {
                Ok(false)
            } else {
                Err(error("交流样本编号冲突；原记录保留"))
            };
        }
        s.seen.push(Seen { id, body });
        if s.seen.len() > MAX_SEEN {
            s.seen.remove(0);
        }
        if enabled {
            s.messages.push(message);
            if s.messages.len() > MAX_CONVERSATION_MESSAGES {
                s.messages.remove(0);
            }
            s.pending = (s.pending + 1).min(MAX_CONVERSATION_MESSAGES);
        }
        Ok(true)
    }
    pub(crate) fn take(
        &mut self,
        now: u64,
        enabled: impl Fn(&ContextScope) -> bool,
    ) -> PluginResult<Option<ConversationBatch>> {
        let Some(s) = self.scopes.iter_mut().find(|s| {
            enabled(&s.scope)
                && now >= s.ready_at
                && (s.inflight.is_some()
                    || s.pending >= CONVERSATION_BATCH_MESSAGES
                        && s.messages
                            .iter()
                            .filter(|m| m.role == ConversationRole::Human)
                            .count()
                            >= 2)
        }) else {
            return Ok(None);
        };
        if let Some(batch) = &mut s.inflight {
            if batch.attempt >= 3 {
                s.inflight = None;
                s.ready_at = now.saturating_add(RETRY_MS);
                return Ok(None);
            }
            batch.attempt += 1;
            s.ready_at = now.saturating_add(RETRY_MS);
            return Ok(Some(batch.clone()));
        }
        let id = format!("conversation-{}-{}", self.next, s.generation);
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| error("交流学习序号耗尽"))?;
        let batch = ConversationBatch {
            id,
            scope: s.scope.clone(),
            messages: s.messages.clone(),
            lessons: s.lessons.clone(),
            lesson_history: vec![],
            attempt: 1,
        };
        s.pending = 0;
        s.ready_at = now.saturating_add(RETRY_MS);
        s.inflight = Some(batch.clone());
        Ok(Some(batch))
    }
    pub(crate) fn ready(&self, now: u64, enabled: impl Fn(&ContextScope) -> bool) -> bool {
        self.scopes.iter().any(|s| {
            now >= s.ready_at
                && enabled(&s.scope)
                && (s.inflight.is_some()
                    || s.pending >= CONVERSATION_BATCH_MESSAGES
                        && s.messages
                            .iter()
                            .filter(|m| m.role == ConversationRole::Human)
                            .count()
                            >= 2)
        })
    }
    pub(crate) fn fail(&mut self, batch: &ConversationBatch, now: u64) -> PluginResult<()> {
        let Some(s) = self
            .scopes
            .iter_mut()
            .find(|s| s.scope == batch.scope && s.inflight.as_ref() == Some(batch))
        else {
            return Ok(());
        };
        if batch.attempt >= 3 {
            s.inflight = None;
        }
        s.ready_at = now.saturating_add(RETRY_MS.saturating_mul(u64::from(batch.attempt)));
        Ok(())
    }
    pub(crate) fn finish(
        &mut self,
        batch: &ConversationBatch,
        drafts: &[ConversationLessonDraft],
        now: u64,
    ) -> PluginResult<usize> {
        let s = self
            .scopes
            .iter_mut()
            .find(|s| s.scope == batch.scope && s.inflight.as_ref() == Some(batch))
            .ok_or_else(|| error("交流学习批次已失效"))?;
        if drafts.len() > MAX_CONVERSATION_DRAFTS {
            return Err(error("交流学习候选超限"));
        }
        let mut updated = BTreeSet::new();
        let mut keys = BTreeSet::new();
        let mut changed = 0;
        for draft in drafts {
            if !keys.insert((
                &draft.lesson_id,
                &draft.situation,
                &draft.response,
                &draft.avoid,
            )) {
                return Err(error("同批交流经验重复"));
            }
            if !text(&draft.situation, 240)
                || !text(&draft.response, 480)
                || draft.avoid.len() > 480
                || !(2..=4).contains(&draft.evidence.len())
            {
                return Err(error("交流经验格式无效"));
            }
            let mut indices = BTreeSet::new();
            for evidence in &draft.evidence {
                if !text(&evidence.quote, 240) {
                    return Err(error("交流依据为空或超限"));
                }
                let Some(index) = batch
                    .messages
                    .iter()
                    .position(|m| m.id == evidence.message_id && m.text.contains(&evidence.quote))
                else {
                    return Err(error("交流依据不是本批真实原话"));
                };
                if !indices.insert(index) {
                    return Err(error("交流依据重复"));
                }
            }
            let humans: Vec<_> = indices
                .iter()
                .copied()
                .filter(|i| batch.messages[*i].role == ConversationRole::Human)
                .collect();
            let feedback = humans.iter().any(|i| {
                batch.messages[..*i]
                    .iter()
                    .rposition(|m| m.role == ConversationRole::Eve)
                    .is_some_and(|j| indices.contains(&j))
            });
            if humans.is_empty()
                || (draft.assessment == ConversationAssessment::Observation && humans.len() < 2)
                || (draft.assessment != ConversationAssessment::Observation && !feedback)
            {
                return Err(error("交流评价缺少真人反应证据"));
            }
            let existing = match &draft.lesson_id {
                Some(id) => {
                    if !batch.lessons.iter().any(|l| &l.id == id) || !updated.insert(id.clone()) {
                        return Err(error("交流经验更新越过固定批次"));
                    }
                    Some(
                        s.lessons
                            .iter()
                            .position(|l| &l.id == id)
                            .ok_or_else(|| error("交流经验已失效"))?,
                    )
                }
                None => {
                    if draft.assessment == ConversationAssessment::Counterexample {
                        return Err(error("反例必须指向已有经验"));
                    }
                    s.lessons.iter().position(|l| {
                        l.situation == draft.situation
                            && l.response == draft.response
                            && l.avoid == draft.avoid
                    })
                }
            };
            let index = if let Some(index) = existing {
                index
            } else {
                if s.lessons.len() == MAX_CONVERSATION_LESSONS {
                    let index = s
                        .lessons
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, l)| (l.active, l.updated_at_ms))
                        .map(|(i, _)| i)
                        .expect("full");
                    s.history.push(ConversationLessonRevision {
                        sequence: s.history.len() as u64 + 1,
                        lesson: s.lessons.remove(index),
                        reason: "capacity".into(),
                        batch_id: Some(batch.id.clone()),
                        recorded_at_ms: Some(now),
                    });
                }
                let id = format!("lesson-{}", self.next);
                self.next = self
                    .next
                    .checked_add(1)
                    .ok_or_else(|| error("交流经验序号耗尽"))?;
                s.lessons.push(ConversationLesson {
                    id,
                    situation: draft.situation.clone(),
                    response: draft.response.clone(),
                    avoid: draft.avoid.clone(),
                    observations: 0,
                    corrections: 0,
                    counterexamples: 0,
                    active: false,
                    updated_at_ms: now,
                    evidence: vec![],
                    evaluated_message_ids: vec![],
                });
                s.lessons.len() - 1
            };
            let lesson = &mut s.lessons[index];
            // 相同上下文反复出现在滚动窗口，不得把一次观察刷成多次支持。
            if humans.iter().all(|i| {
                lesson
                    .evaluated_message_ids
                    .contains(&batch.messages[*i].id)
            }) {
                continue;
            }
            if !lesson.evidence.is_empty() {
                s.history.push(ConversationLessonRevision {
                    sequence: s.history.len() as u64 + 1,
                    lesson: lesson.clone(),
                    reason: "before_update".into(),
                    batch_id: Some(batch.id.clone()),
                    recorded_at_ms: Some(now),
                });
            }
            match draft.assessment {
                ConversationAssessment::Observation => {
                    lesson.observations = lesson.observations.saturating_add(1)
                }
                ConversationAssessment::Correction => {
                    lesson.corrections = lesson.corrections.saturating_add(1);
                    lesson.counterexamples = 0;
                }
                ConversationAssessment::Counterexample => {
                    lesson.counterexamples = lesson.counterexamples.saturating_add(1)
                }
            }
            if draft.assessment != ConversationAssessment::Counterexample {
                lesson.situation = draft.situation.clone();
                lesson.response = draft.response.clone();
                lesson.avoid = draft.avoid.clone();
            }
            lesson.active =
                lesson.counterexamples == 0 && (lesson.observations >= 2 || lesson.corrections > 0);
            lesson.updated_at_ms = now;
            lesson.evidence = draft.evidence.clone();
            for index in humans {
                if !lesson
                    .evaluated_message_ids
                    .contains(&batch.messages[index].id)
                {
                    lesson
                        .evaluated_message_ids
                        .push(batch.messages[index].id.clone());
                }
            }
            if lesson.evaluated_message_ids.len() > 64 {
                lesson
                    .evaluated_message_ids
                    .drain(..lesson.evaluated_message_ids.len() - 64);
            }
            s.history.push(ConversationLessonRevision {
                sequence: s.history.len() as u64 + 1,
                lesson: lesson.clone(),
                reason: "updated".into(),
                batch_id: Some(batch.id.clone()),
                recorded_at_ms: Some(now),
            });
            changed += 1;
        }
        s.inflight = None;
        s.ready_at = now;
        Ok(changed)
    }
    pub(crate) fn lessons(&self, scope: &ContextScope) -> Vec<ConversationLesson> {
        self.scopes
            .iter()
            .find(|s| s.scope == *scope)
            .map(|s| s.lessons.clone())
            .unwrap_or_default()
    }
    pub(crate) fn reset(&mut self, scope: &ContextScope) -> PluginResult<bool> {
        let Some(s) = self.scopes.iter_mut().find(|s| s.scope == *scope) else {
            return Ok(false);
        };
        s.messages.clear();
        for lesson in s.lessons.drain(..) {
            s.history.push(ConversationLessonRevision {
                sequence: s.history.len() as u64 + 1,
                lesson,
                reason: "reset".into(),
                batch_id: None,
                recorded_at_ms: None,
            });
        }
        s.inflight = None;
        s.pending = 0;
        s.generation = s
            .generation
            .checked_add(1)
            .ok_or_else(|| error("交流学习代次耗尽"))?;
        Ok(true)
    }
    pub(crate) fn cancel(&mut self, scope: &ContextScope) {
        if let Some(s) = self.scopes.iter_mut().find(|s| s.scope == *scope) {
            s.inflight = None;
            s.pending = 0;
            s.messages.clear();
        }
    }
}
