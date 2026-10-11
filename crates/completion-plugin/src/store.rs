//! 完成确认提问账本：认领与结果都先持久化；重启时把发送中的提问记为结果未知，不再问。
use crate::strict_json;
use eve_completion_api::*;
use eve_plugin_api::{PluginContext, PluginError, PluginResult};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    sync::{Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    format_version: u32,
    questions: Vec<CompletionQuestion>,
}
struct Inner {
    ledger: Ledger,
    context: Option<PluginContext>,
}
pub(super) struct StoredQuestions {
    inner: Mutex<Inner>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(1)
        .max(1)
}

impl StoredQuestions {
    pub(super) fn open(context: PluginContext) -> CompletionResult<Self> {
        let mut ledger = match context
            .state_get(COMPLETION_STATE_KEY)
            .map_err(|_| CompletionError::Storage)?
        {
            None => Ledger {
                format_version: FORMAT_VERSION,
                questions: vec![],
            },
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(CompletionError::CorruptState);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| CompletionError::CorruptState)?;
                if value.get("format_version").and_then(|value| value.as_u64())
                    != Some(u64::from(FORMAT_VERSION))
                {
                    return Err(if value.get("format_version").is_some() {
                        CompletionError::UnsupportedVersion
                    } else {
                        CompletionError::CorruptState
                    });
                }
                let ledger: Ledger =
                    serde_json::from_value(value).map_err(|_| CompletionError::CorruptState)?;
                validate_ledger(&ledger)?;
                ledger
            }
        };
        let mut changed = false;
        let now = now_ms();
        for question in &mut ledger.questions {
            // 发送前后进程退出：是否送达不确定，不再问，也不当作已送达。
            if question.status == QuestionStatus::Delivering {
                if let Some(attempt) = question.attempts.last_mut() {
                    attempt.result = Some(AttemptResult::Unknown);
                    attempt.finished_at_ms = Some(now.max(attempt.started_at_ms));
                }
                question.status = QuestionStatus::Unknown;
                question.closed_at_ms = Some(now.max(question.created_at_ms));
                changed = true;
            }
        }
        if changed {
            context
                .state_set(COMPLETION_STATE_KEY, encode(&ledger)?)
                .map_err(|_| CompletionError::Storage)?;
        }
        Ok(Self {
            inner: Mutex::new(Inner {
                ledger,
                context: Some(context),
            }),
        })
    }

    fn lock(&self) -> CompletionResult<MutexGuard<'_, Inner>> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| CompletionError::Unavailable)?;
        if inner.context.is_none() {
            return Err(CompletionError::Unavailable);
        }
        Ok(inner)
    }

    pub(super) fn snapshot(&self) -> CompletionResult<CompletionSnapshot> {
        Ok(CompletionSnapshot {
            questions: self.lock()?.ledger.questions.clone(),
        })
    }

    pub(super) fn claim(
        &self,
        target: &QuestionTarget,
        message_id: &str,
        now_ms: u64,
    ) -> CompletionResult<CompletionQuestion> {
        target.validate()?;
        validate_id(message_id)?;
        if now_ms == 0 {
            return Err(CompletionError::InvalidInput);
        }
        let id = question_id(&target.owner, &target.plan_id);
        let attempt = QuestionAttempt {
            message_id: message_id.into(),
            started_at_ms: now_ms,
            finished_at_ms: None,
            result: None,
        };
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let index = match next.questions.iter().position(|question| question.id == id) {
            Some(index) => {
                let question = &mut next.questions[index];
                if question.status != QuestionStatus::Retry {
                    return Err(CompletionError::AlreadyAsked);
                }
                if question.target != *target
                    || question
                        .attempts
                        .iter()
                        .any(|old| old.message_id == message_id)
                {
                    return Err(CompletionError::InvalidInput);
                }
                question.status = QuestionStatus::Delivering;
                question.attempts.push(attempt);
                index
            }
            None => {
                // 满了时腾出最早结束的一条；发送中与待重试的记录从不删除。
                if next.questions.len() >= MAX_QUESTIONS {
                    let oldest = next
                        .questions
                        .iter()
                        .enumerate()
                        .filter_map(|(index, question)| question.closed_at_ms.map(|at| (at, index)))
                        .min()
                        .ok_or(CompletionError::LimitReached)?;
                    next.questions.remove(oldest.1);
                }
                next.questions.push(CompletionQuestion {
                    id,
                    target: target.clone(),
                    status: QuestionStatus::Delivering,
                    attempts: vec![attempt],
                    created_at_ms: now_ms,
                    closed_at_ms: None,
                });
                next.questions.len() - 1
            }
        };
        let question = next.questions[index].clone();
        question.validate()?;
        persist(&mut inner, next)?;
        Ok(question)
    }

    pub(super) fn record(
        &self,
        id: &str,
        result: AttemptResult,
        now_ms: u64,
    ) -> CompletionResult<CompletionQuestion> {
        validate_id(id)?;
        if let AttemptResult::Sent {
            platform_message_id: Some(platform),
        } = &result
        {
            validate_id(platform)?;
        }
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let question = next
            .questions
            .iter_mut()
            .find(|question| question.id == id)
            .ok_or(CompletionError::NotFound)?;
        if question.status != QuestionStatus::Delivering {
            return Err(CompletionError::InvalidTransition);
        }
        let attempts = question.attempts.len();
        let last = question
            .attempts
            .last_mut()
            .ok_or(CompletionError::InvalidTransition)?;
        let finished = now_ms.max(last.started_at_ms);
        last.finished_at_ms = Some(finished);
        question.status = match &result {
            AttemptResult::Sent { .. } => QuestionStatus::Delivered,
            AttemptResult::Unknown => QuestionStatus::Unknown,
            AttemptResult::Failed { .. } | AttemptResult::NotSent if attempts < MAX_ATTEMPTS => {
                QuestionStatus::Retry
            }
            AttemptResult::Failed { .. } | AttemptResult::NotSent => QuestionStatus::Exhausted,
        };
        last.result = Some(result);
        if question.status.closed() {
            question.closed_at_ms = Some(finished);
        }
        question.validate()?;
        let question = question.clone();
        persist(&mut inner, next)?;
        Ok(question)
    }

    pub(super) fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("完成确认提问状态锁不可用".into()))?
            .context = None;
        Ok(())
    }
}

fn persist(inner: &mut Inner, next: Ledger) -> CompletionResult<()> {
    let bytes = encode(&next)?;
    let context = inner.context.as_ref().ok_or(CompletionError::Unavailable)?;
    // 失败也可能已经提交；关闭整个实例，禁止旧缓存继续读取或覆盖后端。
    if context.state_set(COMPLETION_STATE_KEY, bytes).is_err() {
        inner.context = None;
        return Err(CompletionError::Storage);
    }
    inner.ledger = next;
    Ok(())
}

fn encode(ledger: &Ledger) -> CompletionResult<Vec<u8>> {
    if ledger.questions.len() > MAX_QUESTIONS {
        return Err(CompletionError::LimitReached);
    }
    let bytes = serde_json::to_vec(ledger).map_err(|_| CompletionError::InvalidInput)?;
    if bytes.len() > MAX_STATE_BYTES {
        return Err(CompletionError::LimitReached);
    }
    Ok(bytes)
}

fn validate_ledger(ledger: &Ledger) -> CompletionResult<()> {
    if ledger.questions.len() > MAX_QUESTIONS {
        return Err(CompletionError::CorruptState);
    }
    let mut ids = BTreeSet::new();
    for question in &ledger.questions {
        question
            .validate()
            .map_err(|_| CompletionError::CorruptState)?;
        if !ids.insert(question.id.as_str()) {
            return Err(CompletionError::CorruptState);
        }
    }
    Ok(())
}
