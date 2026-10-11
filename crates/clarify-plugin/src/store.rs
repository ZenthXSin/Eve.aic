//! 请求澄清账本：每一步先持久化再执行对应外部动作；重启时把进行中的撰写与判断记为中断、
//! 发送中的投递记为结果未知，都不重放。
use crate::strict_json;
use eve_clarify_api::*;
use eve_plugin_api::{PluginContext, PluginError, PluginResult};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    sync::{Mutex, MutexGuard},
};

const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    format_version: u32,
    requests: Vec<Clarification>,
    preferences: Vec<OwnerPreference>,
}
struct Inner {
    ledger: Ledger,
    context: Option<PluginContext>,
}
pub(super) struct StoredClarify {
    inner: Mutex<Inner>,
}

impl StoredClarify {
    pub(super) fn open(context: PluginContext) -> ClarifyResult<Self> {
        let mut ledger = match context
            .state_get(CLARIFY_STATE_KEY)
            .map_err(|_| ClarifyError::Storage)?
        {
            None => Ledger {
                format_version: FORMAT_VERSION,
                requests: vec![],
                preferences: vec![],
            },
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(ClarifyError::CorruptState);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| ClarifyError::CorruptState)?;
                if value.get("format_version").and_then(|value| value.as_u64())
                    != Some(u64::from(FORMAT_VERSION))
                {
                    return Err(if value.get("format_version").is_some() {
                        ClarifyError::UnsupportedVersion
                    } else {
                        ClarifyError::CorruptState
                    });
                }
                let ledger: Ledger =
                    serde_json::from_value(value).map_err(|_| ClarifyError::CorruptState)?;
                validate_ledger(&ledger).map_err(|_| ClarifyError::CorruptState)?;
                ledger
            }
        };
        let mut changed = false;
        for request in &mut ledger.requests {
            // 判断请求不重放；这条消息不再判断，不伪造完成时间。
            if let Some(judgement) = request
                .judgements
                .last_mut()
                .filter(|judgement| judgement.outcome.is_none())
            {
                judgement.outcome = Some(JudgeOutcome::Interrupted);
                changed = true;
            }
            match request.status {
                // 撰写请求不重放；不伪造完成时间。
                ClarifyStatus::Composing => {
                    request.status = ClarifyStatus::Interrupted;
                    changed = true;
                }
                // 发送前后进程退出：是否送达不确定，不重发，也不当作已送达。
                ClarifyStatus::Delivering => {
                    if let Some(attempt) = request.attempts.last_mut() {
                        attempt.result = Some(DeliveryResult::Unknown);
                    }
                    request.status = ClarifyStatus::Unknown;
                    changed = true;
                }
                _ => {}
            }
        }
        if changed {
            let bytes = encode(&ledger)?;
            context
                .state_set(CLARIFY_STATE_KEY, bytes)
                .map_err(|_| ClarifyError::Storage)?;
        }
        Ok(Self {
            inner: Mutex::new(Inner {
                ledger,
                context: Some(context),
            }),
        })
    }

    fn lock(&self) -> ClarifyResult<MutexGuard<'_, Inner>> {
        let inner = self.inner.lock().map_err(|_| ClarifyError::Unavailable)?;
        if inner.context.is_none() {
            return Err(ClarifyError::Unavailable);
        }
        Ok(inner)
    }

    pub(super) fn snapshot(&self) -> ClarifyResult<ClarifySnapshot> {
        let inner = self.lock()?;
        Ok(ClarifySnapshot {
            requests: inner.ledger.requests.clone(),
            preferences: inner.ledger.preferences.clone(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn begin(
        &self,
        owner: &str,
        goal_id: &str,
        goal_revision: u64,
        stall: Stall,
        facts: Vec<Fact>,
        composer_version: &str,
        now_ms: u64,
    ) -> ClarifyResult<Option<Clarification>> {
        for id in [owner, goal_id, &stall.run_id, composer_version] {
            validate_id(id)?;
        }
        validate_facts(&facts)?;
        if goal_revision == 0 || now_ms == 0 {
            return Err(ClarifyError::InvalidInput);
        }
        let mut inner = self.lock()?;
        if inner
            .ledger
            .requests
            .iter()
            .any(|request| request.goal_id == goal_id && request.goal_revision == goal_revision)
        {
            return Ok(None);
        }
        if inner.ledger.requests.len() >= MAX_REQUESTS {
            return Err(ClarifyError::LimitReached);
        }
        let request = Clarification {
            id: clarification_id(goal_id, goal_revision),
            owner: owner.into(),
            goal_id: goal_id.into(),
            goal_revision,
            stall,
            facts,
            composer_version: composer_version.into(),
            created_at_ms: now_ms,
            composed_at_ms: None,
            question: None,
            status: ClarifyStatus::Composing,
            judgements: vec![],
            attempts: vec![],
            delivered_at_ms: None,
            answer: None,
            closed_at_ms: None,
        };
        let mut next = inner.ledger.clone();
        next.requests.push(request.clone());
        persist(&mut inner, next)?;
        Ok(Some(request))
    }

    pub(super) fn record_composition(
        &self,
        id: &str,
        at_ms: u64,
        result: Result<String, ClarifyFailure>,
    ) -> ClarifyResult<Clarification> {
        if let Ok(question) = &result {
            validate_question(question)?;
        }
        self.update(id, at_ms, |_, entry| {
            if entry.status != ClarifyStatus::Composing {
                return Err(ClarifyError::InvalidTransition);
            }
            let at = at_ms.max(last_time(entry));
            match result {
                Ok(question) => {
                    entry.question = Some(question);
                    entry.composed_at_ms = Some(at);
                    entry.status = ClarifyStatus::Pending;
                }
                Err(failure) => {
                    entry.status = ClarifyStatus::Failed(failure);
                    entry.closed_at_ms = Some(at);
                }
            }
            Ok(())
        })
    }

    pub(super) fn begin_judgement(
        &self,
        id: &str,
        message_id: &str,
        at_ms: u64,
    ) -> ClarifyResult<Clarification> {
        validate_id(message_id)?;
        self.update(id, at_ms, |_, entry| {
            if entry.status != ClarifyStatus::Pending
                || entry.judging()
                || entry.judged(message_id)
                || entry.judgements.len() >= MAX_JUDGEMENTS
            {
                return Err(ClarifyError::InvalidTransition);
            }
            entry.judgements.push(Judgement {
                message_id: message_id.into(),
                started_at_ms: at_ms.max(last_time(entry)),
                finished_at_ms: None,
                outcome: None,
            });
            Ok(())
        })
    }

    pub(super) fn record_judgement(
        &self,
        id: &str,
        message_id: &str,
        at_ms: u64,
        outcome: Result<bool, ClarifyFailure>,
    ) -> ClarifyResult<Clarification> {
        self.update(id, at_ms, |_, entry| {
            if entry.status != ClarifyStatus::Pending {
                return Err(ClarifyError::InvalidTransition);
            }
            let started = match entry.judgements.last() {
                Some(judgement)
                    if judgement.message_id == message_id && judgement.outcome.is_none() =>
                {
                    judgement.started_at_ms
                }
                _ => return Err(ClarifyError::InvalidTransition),
            };
            let outcome = match outcome {
                Ok(true) => JudgeOutcome::Ask,
                Ok(false) => JudgeOutcome::NotNow,
                Err(failure) => JudgeOutcome::Failed { failure },
            };
            let at = at_ms.max(started);
            if let Some(judgement) = entry.judgements.last_mut() {
                judgement.finished_at_ms = Some(at);
                judgement.outcome = Some(outcome);
            }
            // 判断次数用尽仍没有合适的时机：不再等待。
            if outcome != JudgeOutcome::Ask && entry.judgements.len() >= MAX_JUDGEMENTS {
                entry.status = ClarifyStatus::Failed(ClarifyFailure::Exhausted);
                entry.closed_at_ms = Some(at);
            }
            Ok(())
        })
    }

    pub(super) fn claim(
        &self,
        id: &str,
        message_id: &str,
        at_ms: u64,
    ) -> ClarifyResult<Clarification> {
        validate_id(message_id)?;
        self.update(id, at_ms, |_, entry| {
            let latest_ask = entry.judgements.last().is_some_and(|judgement| {
                judgement.message_id == message_id && judgement.outcome == Some(JudgeOutcome::Ask)
            });
            if entry.status != ClarifyStatus::Pending
                || !latest_ask
                || entry.carried_by(message_id)
                || entry.attempts.len() >= MAX_ATTEMPTS
            {
                return Err(ClarifyError::InvalidTransition);
            }
            entry.attempts.push(DeliveryAttempt {
                message_id: message_id.into(),
                started_at_ms: at_ms.max(last_time(entry)),
                finished_at_ms: None,
                result: None,
            });
            entry.status = ClarifyStatus::Delivering;
            Ok(())
        })
    }

    pub(super) fn record_delivery(
        &self,
        id: &str,
        at_ms: u64,
        result: DeliveryResult,
    ) -> ClarifyResult<Clarification> {
        validate_result(&result)?;
        if result == DeliveryResult::Unknown {
            // 结果未知只由重启核对写入。
            return Err(ClarifyError::InvalidInput);
        }
        self.update(id, at_ms, |_, entry| {
            if entry.status != ClarifyStatus::Delivering {
                return Err(ClarifyError::InvalidTransition);
            }
            let at = at_ms.max(last_time(entry));
            let attempts = entry.attempts.len();
            let attempt = entry
                .attempts
                .last_mut()
                .filter(|attempt| attempt.result.is_none())
                .ok_or(ClarifyError::InvalidTransition)?;
            attempt.finished_at_ms = Some(at);
            attempt.result = Some(result.clone());
            match result {
                DeliveryResult::Sent { .. } => {
                    entry.status = ClarifyStatus::Delivered;
                    entry.delivered_at_ms = Some(at);
                }
                _ if attempts >= MAX_ATTEMPTS => {
                    entry.status = ClarifyStatus::Failed(ClarifyFailure::Exhausted);
                    entry.closed_at_ms = Some(at);
                }
                _ => entry.status = ClarifyStatus::Pending,
            }
            Ok(())
        })
    }

    pub(super) fn record_answer(&self, id: &str, answer: Answer) -> ClarifyResult<Clarification> {
        validate_id(&answer.message_id)?;
        validate_id(&answer.evidence_id)?;
        validate_answer_text(&answer.text)?;
        let at_ms = answer.at_ms;
        self.update(id, at_ms, |_, entry| {
            let delivered = entry
                .delivered_at_ms
                .filter(|_| entry.status == ClarifyStatus::Delivered)
                .ok_or(ClarifyError::InvalidTransition)?;
            if at_ms < delivered
                || at_ms - delivered > ANSWER_WINDOW_MS
                || entry.carried_by(&answer.message_id)
            {
                return Err(ClarifyError::InvalidInput);
            }
            entry.closed_at_ms = Some(at_ms.max(last_time(entry)));
            entry.answer = Some(answer);
            entry.status = ClarifyStatus::Answered;
            Ok(())
        })
    }

    pub(super) fn expire(&self, id: &str, at_ms: u64) -> ClarifyResult<Clarification> {
        self.update(id, at_ms, |_, entry| {
            let delivered = entry
                .delivered_at_ms
                .filter(|_| entry.status == ClarifyStatus::Delivered)
                .ok_or(ClarifyError::InvalidTransition)?;
            if at_ms.saturating_sub(delivered) <= ANSWER_WINDOW_MS {
                return Err(ClarifyError::InvalidTransition);
            }
            entry.status = ClarifyStatus::Expired;
            entry.closed_at_ms = Some(at_ms.max(last_time(entry)));
            Ok(())
        })
    }

    pub(super) fn cancel(
        &self,
        id: &str,
        at_ms: u64,
        reason: CancelReason,
    ) -> ClarifyResult<Clarification> {
        self.update(id, at_ms, |_, entry| {
            // 正在撰写、判断或发送的请求不能取消；结束后下一轮再取消。
            if entry.status != ClarifyStatus::Pending || entry.judging() {
                return Err(ClarifyError::InvalidTransition);
            }
            entry.status = ClarifyStatus::Cancelled(reason);
            entry.closed_at_ms = Some(at_ms.max(last_time(entry)));
            Ok(())
        })
    }

    pub(super) fn set_quiet(
        &self,
        owner: &str,
        quiet: bool,
        at_ms: u64,
    ) -> ClarifyResult<OwnerPreference> {
        validate_id(owner)?;
        if at_ms == 0 {
            return Err(ClarifyError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let preference = match next
            .preferences
            .iter_mut()
            .find(|preference| preference.owner == owner)
        {
            Some(preference) => {
                preference.quiet = quiet;
                preference.changed_at_ms = at_ms.max(preference.changed_at_ms);
                preference.clone()
            }
            None => {
                if next.preferences.len() >= MAX_PREFERENCES {
                    return Err(ClarifyError::LimitReached);
                }
                let preference = OwnerPreference {
                    owner: owner.into(),
                    quiet,
                    changed_at_ms: at_ms,
                };
                next.preferences.push(preference.clone());
                preference
            }
        };
        persist(&mut inner, next)?;
        Ok(preference)
    }

    fn update(
        &self,
        id: &str,
        at_ms: u64,
        apply: impl FnOnce(&Ledger, &mut Clarification) -> ClarifyResult<()>,
    ) -> ClarifyResult<Clarification> {
        if at_ms == 0 {
            return Err(ClarifyError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let position = next
            .requests
            .iter()
            .position(|entry| entry.id == id)
            .ok_or(ClarifyError::NotFound)?;
        let mut entry = next.requests[position].clone();
        apply(&next, &mut entry)?;
        next.requests[position] = entry.clone();
        persist(&mut inner, next)?;
        Ok(entry)
    }

    pub(super) fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("请求澄清状态锁不可用".into()))?
            .context = None;
        Ok(())
    }
}

/// 请求上最近一次已记录的时间；之后的记录不能更早。
fn last_time(entry: &Clarification) -> u64 {
    entry
        .attempts
        .iter()
        .flat_map(|attempt| [Some(attempt.started_at_ms), attempt.finished_at_ms])
        .chain(
            entry
                .judgements
                .iter()
                .flat_map(|judgement| [Some(judgement.started_at_ms), judgement.finished_at_ms]),
        )
        .flatten()
        .chain(entry.composed_at_ms)
        .chain(entry.delivered_at_ms)
        .chain([entry.created_at_ms])
        .max()
        .unwrap_or(0)
}

fn validate_result(result: &DeliveryResult) -> ClarifyResult<()> {
    match result {
        DeliveryResult::Sent {
            platform_message_id: Some(id),
        } => validate_id(id),
        _ => Ok(()),
    }
}

fn persist(inner: &mut Inner, next: Ledger) -> ClarifyResult<()> {
    let bytes = encode(&next)?;
    let context = inner.context.as_ref().ok_or(ClarifyError::Unavailable)?;
    // 失败也可能已经提交；关闭整个实例，禁止旧缓存继续读取或覆盖后端。
    if context.state_set(CLARIFY_STATE_KEY, bytes).is_err() {
        inner.context = None;
        return Err(ClarifyError::Storage);
    }
    inner.ledger = next;
    Ok(())
}

fn encode(ledger: &Ledger) -> ClarifyResult<Vec<u8>> {
    if ledger.requests.len() > MAX_REQUESTS || ledger.preferences.len() > MAX_PREFERENCES {
        return Err(ClarifyError::LimitReached);
    }
    let bytes = serde_json::to_vec(ledger).map_err(|_| ClarifyError::InvalidInput)?;
    // 重启时写入的中断与未知结局比原状态略长；预留空间保证能保存。
    let open = ledger
        .requests
        .iter()
        .filter(|entry| {
            matches!(
                entry.status,
                ClarifyStatus::Composing | ClarifyStatus::Delivering
            ) || entry.judging()
        })
        .count();
    if bytes.len().saturating_add(open * 32) > MAX_STATE_BYTES {
        return Err(ClarifyError::LimitReached);
    }
    Ok(bytes)
}

/// 启动时完整核对；任何不一致都拒绝打开并保留原字节。
fn validate_ledger(ledger: &Ledger) -> ClarifyResult<()> {
    if ledger.requests.len() > MAX_REQUESTS || ledger.preferences.len() > MAX_PREFERENCES {
        return Err(ClarifyError::CorruptState);
    }
    let mut ids = BTreeSet::new();
    let mut goals = BTreeSet::new();
    for entry in &ledger.requests {
        validate_request(entry)?;
        if !ids.insert(entry.id.as_str())
            || !goals.insert((entry.goal_id.as_str(), entry.goal_revision))
        {
            return Err(ClarifyError::CorruptState);
        }
    }
    let mut owners = BTreeSet::new();
    for preference in &ledger.preferences {
        validate_id(&preference.owner)?;
        if preference.changed_at_ms == 0 || !owners.insert(preference.owner.as_str()) {
            return Err(ClarifyError::CorruptState);
        }
    }
    Ok(())
}

fn validate_request(entry: &Clarification) -> ClarifyResult<()> {
    for id in [
        &entry.id,
        &entry.owner,
        &entry.goal_id,
        &entry.stall.run_id,
        &entry.composer_version,
    ] {
        validate_id(id)?;
    }
    validate_facts(&entry.facts)?;
    if entry.id != clarification_id(&entry.goal_id, entry.goal_revision)
        || entry.goal_revision == 0
        || entry.created_at_ms == 0
        || entry.judgements.len() > MAX_JUDGEMENTS
        || entry.attempts.len() > MAX_ATTEMPTS
        || entry.composed_at_ms.is_some() != entry.question.is_some()
        || entry
            .composed_at_ms
            .is_some_and(|at| at < entry.created_at_ms)
    {
        return Err(ClarifyError::CorruptState);
    }
    if let Some(question) = &entry.question {
        validate_question(question)?;
    }
    let mut messages = BTreeSet::new();
    for (index, judgement) in entry.judgements.iter().enumerate() {
        validate_id(&judgement.message_id)?;
        let last = index + 1 == entry.judgements.len();
        if !messages.insert(judgement.message_id.as_str())
            || (judgement.outcome.is_some() != judgement.finished_at_ms.is_some()
                && judgement.outcome != Some(JudgeOutcome::Interrupted))
            || (judgement.outcome.is_none() && !last)
            || judgement
                .finished_at_ms
                .is_some_and(|at| at < judgement.started_at_ms)
        {
            return Err(ClarifyError::CorruptState);
        }
    }
    for (index, attempt) in entry.attempts.iter().enumerate() {
        validate_id(&attempt.message_id)?;
        if let Some(result) = &attempt.result {
            validate_result(result)?;
        }
        let last = index + 1 == entry.attempts.len();
        let judged_ask = entry.judgements.iter().any(|judgement| {
            judgement.message_id == attempt.message_id
                && judgement.outcome == Some(JudgeOutcome::Ask)
        });
        if !judged_ask
            || (attempt.result.is_none() && !last)
            || (!last && matches!(attempt.result, Some(DeliveryResult::Sent { .. })))
            || attempt
                .finished_at_ms
                .is_some_and(|at| at < attempt.started_at_ms)
        {
            return Err(ClarifyError::CorruptState);
        }
    }
    if let Some(answer) = &entry.answer {
        validate_id(&answer.message_id)?;
        validate_id(&answer.evidence_id)?;
        validate_answer_text(&answer.text)?;
    }
    let last_result = entry.attempts.last().map(|attempt| &attempt.result);
    let sent = matches!(last_result, Some(Some(DeliveryResult::Sent { .. })));
    let consistent = match entry.status {
        ClarifyStatus::Composing | ClarifyStatus::Interrupted => {
            entry.question.is_none()
                && entry.judgements.is_empty()
                && entry.attempts.is_empty()
                && entry.closed_at_ms.is_none()
        }
        ClarifyStatus::Pending => {
            entry.question.is_some()
                && !sent
                && !matches!(
                    last_result,
                    Some(None) | Some(Some(DeliveryResult::Unknown))
                )
                && entry.closed_at_ms.is_none()
        }
        ClarifyStatus::Delivering => {
            matches!(last_result, Some(None)) && !entry.judging() && entry.closed_at_ms.is_none()
        }
        ClarifyStatus::Delivered => {
            sent && entry.delivered_at_ms.is_some() && entry.closed_at_ms.is_none()
        }
        ClarifyStatus::Answered => {
            sent && entry.answer.as_ref().is_some_and(|answer| {
                entry.delivered_at_ms.is_some_and(|delivered| {
                    answer.at_ms >= delivered && answer.at_ms - delivered <= ANSWER_WINDOW_MS
                }) && !entry.carried_by(&answer.message_id)
            }) && entry.closed_at_ms.is_some()
        }
        ClarifyStatus::Expired => {
            sent && entry.delivered_at_ms.is_some() && entry.closed_at_ms.is_some()
        }
        ClarifyStatus::Unknown => {
            matches!(last_result, Some(Some(DeliveryResult::Unknown)))
                && entry.closed_at_ms.is_none()
        }
        ClarifyStatus::Cancelled(_) => {
            entry.question.is_some() && !sent && entry.closed_at_ms.is_some()
        }
        ClarifyStatus::Failed(_) => !sent && entry.closed_at_ms.is_some(),
    };
    let delivered_matches = entry.delivered_at_ms.is_some()
        == matches!(
            entry.status,
            ClarifyStatus::Delivered | ClarifyStatus::Answered | ClarifyStatus::Expired
        );
    let answer_matches = entry.answer.is_some() == (entry.status == ClarifyStatus::Answered);
    let judging_allowed = !entry.judging() || entry.status == ClarifyStatus::Pending;
    if !consistent || !delivered_matches || !answer_matches || !judging_allowed {
        return Err(ClarifyError::CorruptState);
    }
    Ok(())
}
