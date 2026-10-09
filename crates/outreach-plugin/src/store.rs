use crate::strict_json;
use eve_outreach_api::*;
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
    invitations: Vec<Invitation>,
    preferences: Vec<OwnerPreference>,
}
struct Inner {
    ledger: Ledger,
    context: Option<PluginContext>,
}
pub(super) struct StoredOutreach {
    inner: Mutex<Inner>,
}

impl StoredOutreach {
    pub(super) fn open(context: PluginContext) -> OutreachResult<Self> {
        let mut ledger = match context
            .state_get(OUTREACH_STATE_KEY)
            .map_err(|_| OutreachError::Storage)?
        {
            None => Ledger {
                format_version: FORMAT_VERSION,
                invitations: vec![],
                preferences: vec![],
            },
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(OutreachError::CorruptState);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| OutreachError::CorruptState)?;
                if value.get("format_version").and_then(|value| value.as_u64())
                    != Some(u64::from(FORMAT_VERSION))
                {
                    return Err(if value.get("format_version").is_some() {
                        OutreachError::UnsupportedVersion
                    } else {
                        OutreachError::CorruptState
                    });
                }
                let ledger: Ledger =
                    serde_json::from_value(value).map_err(|_| OutreachError::CorruptState)?;
                validate_ledger(&ledger).map_err(|_| OutreachError::CorruptState)?;
                ledger
            }
        };
        let mut changed = false;
        for invitation in &mut ledger.invitations {
            // 判断请求不重放；该消息不再判断，不伪造完成时间。
            if let Some(judgement) = invitation
                .judgements
                .last_mut()
                .filter(|judgement| judgement.outcome.is_none())
            {
                judgement.outcome = Some(JudgementOutcome::Interrupted);
                changed = true;
            }
            // 回应识别不重放；这批对话不再识别，不伪造完成时间。
            if let Some(response) = invitation
                .responses
                .last_mut()
                .filter(|response| response.outcome.is_none())
            {
                response.outcome = Some(ResponseOutcome::Interrupted);
                changed = true;
            }
            match invitation.status {
                // 撰写请求不重放；不伪造完成时间。
                InvitationStatus::Composing => {
                    invitation.status = InvitationStatus::Interrupted;
                    changed = true;
                }
                // 发送前后进程退出：是否送达不确定，不重发，也不当作已送达。
                InvitationStatus::Delivering => {
                    if let Some(attempt) = invitation.attempts.last_mut() {
                        attempt.result = Some(AttemptResult::Unknown);
                    }
                    invitation.status = InvitationStatus::Unknown;
                    changed = true;
                }
                _ => {}
            }
        }
        if changed {
            let bytes = encode(&ledger)?;
            context
                .state_set(OUTREACH_STATE_KEY, bytes)
                .map_err(|_| OutreachError::Storage)?;
        }
        Ok(Self {
            inner: Mutex::new(Inner {
                ledger,
                context: Some(context),
            }),
        })
    }

    fn lock(&self) -> OutreachResult<MutexGuard<'_, Inner>> {
        let inner = self.inner.lock().map_err(|_| OutreachError::Unavailable)?;
        if inner.context.is_none() {
            return Err(OutreachError::Unavailable);
        }
        Ok(inner)
    }

    pub(super) fn snapshot(&self) -> OutreachResult<OutreachSnapshot> {
        let inner = self.lock()?;
        Ok(OutreachSnapshot {
            invitations: inner.ledger.invitations.clone(),
            preferences: inner.ledger.preferences.clone(),
        })
    }

    pub(super) fn begin(
        &self,
        owner: &str,
        goal_id: &str,
        milestone: Milestone,
        facts: Vec<Fact>,
        composer_version: &str,
        now_ms: u64,
    ) -> OutreachResult<Option<Invitation>> {
        validate_id(owner)?;
        validate_id(goal_id)?;
        validate_id(composer_version)?;
        validate_milestone(&milestone)?;
        validate_facts(&facts)?;
        if now_ms == 0 {
            return Err(OutreachError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let id = invitation_id(goal_id);
        if inner.ledger.invitations.iter().any(|entry| entry.id == id) {
            return Ok(None);
        }
        if inner.ledger.invitations.len() >= MAX_INVITATIONS {
            return Err(OutreachError::LimitReached);
        }
        let invitation = Invitation {
            id,
            owner: owner.into(),
            goal_id: goal_id.into(),
            milestone,
            facts,
            composer_version: composer_version.into(),
            created_at_ms: now_ms,
            composed_at_ms: None,
            text: None,
            status: InvitationStatus::Composing,
            judgements: vec![],
            attempts: vec![],
            delivered_at_ms: None,
            closed_at_ms: None,
            responses: vec![],
        };
        let mut next = inner.ledger.clone();
        next.invitations.push(invitation.clone());
        persist(&mut inner, next)?;
        Ok(Some(invitation))
    }

    pub(super) fn record_composition(
        &self,
        id: &str,
        at_ms: u64,
        result: Result<String, OutreachFailure>,
    ) -> OutreachResult<Invitation> {
        self.update(id, at_ms, |_, entry| {
            if entry.status != InvitationStatus::Composing || at_ms < entry.created_at_ms {
                return Err(OutreachError::Conflict);
            }
            match result {
                Ok(text) => {
                    validate_text(&text)?;
                    entry.text = Some(text);
                    entry.composed_at_ms = Some(at_ms);
                    entry.status = InvitationStatus::Pending;
                }
                Err(failure) => {
                    entry.status = InvitationStatus::Failed(failure);
                    entry.closed_at_ms = Some(at_ms);
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
    ) -> OutreachResult<Invitation> {
        validate_id(message_id)?;
        self.update(id, at_ms, |ledger, entry| {
            let busy = ledger.invitations.iter().any(|other| {
                other.owner == entry.owner
                    && (other.status == InvitationStatus::Delivering || other.judging())
            });
            if entry.status != InvitationStatus::Pending
                || busy
                || entry.judgements.len() >= MAX_JUDGEMENTS
                || entry
                    .judgements
                    .iter()
                    .any(|judgement| judgement.message_id == message_id)
                || at_ms < last_time(entry)
            {
                return Err(OutreachError::Conflict);
            }
            entry.judgements.push(Judgement {
                message_id: message_id.into(),
                started_at_ms: at_ms,
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
        outcome: Result<Verdict, OutreachFailure>,
    ) -> OutreachResult<Invitation> {
        self.update(id, at_ms, |_, entry| {
            let Some(judgement) = entry.judgements.last_mut() else {
                return Err(OutreachError::Conflict);
            };
            if judgement.outcome.is_some()
                || judgement.message_id != message_id
                || at_ms < judgement.started_at_ms
            {
                return Err(OutreachError::Conflict);
            }
            judgement.outcome = Some(match outcome {
                Ok(verdict) => JudgementOutcome::Verdict(verdict),
                Err(failure) => JudgementOutcome::Failed(failure),
            });
            judgement.finished_at_ms = Some(at_ms);
            Ok(())
        })
    }

    pub(super) fn claim(
        &self,
        id: &str,
        at_ms: u64,
        channel: DeliveryChannel,
    ) -> OutreachResult<Invitation> {
        validate_channel(&channel)?;
        self.update(id, at_ms, |ledger, entry| {
            let busy = ledger.invitations.iter().any(|other| {
                other.owner == entry.owner && other.status == InvitationStatus::Delivering
            });
            if entry.status != InvitationStatus::Pending
                || busy
                || entry.judging()
                || entry.attempts.len() >= MAX_ATTEMPTS
                || at_ms < last_time(entry)
            {
                return Err(OutreachError::Conflict);
            }
            // 被动附带只在这条消息的时机判断为“邀请”之后，且同一条消息只附带一次。
            if let DeliveryChannel::Passive { message_id } = &channel
                && (entry.verdict_for(message_id) != Some(Verdict::Invite)
                    || entry
                        .attempts
                        .iter()
                        .any(|attempt| attempt.channel == channel))
            {
                return Err(OutreachError::Conflict);
            }
            // 主动消息受平台配额限制：每条邀请至多主动尝试一次。
            if channel == DeliveryChannel::Proactive && entry.proactive_attempts() > 0 {
                return Err(OutreachError::Conflict);
            }
            entry.attempts.push(DeliveryAttempt {
                channel,
                started_at_ms: at_ms,
                finished_at_ms: None,
                result: None,
            });
            entry.status = InvitationStatus::Delivering;
            Ok(())
        })
    }

    pub(super) fn record_delivery(
        &self,
        id: &str,
        at_ms: u64,
        result: AttemptResult,
    ) -> OutreachResult<Invitation> {
        validate_result(&result)?;
        if result == AttemptResult::Unknown {
            return Err(OutreachError::InvalidInput);
        }
        self.update(id, at_ms, |_, entry| {
            let attempts = entry.attempts.len();
            let Some(attempt) = entry.attempts.last_mut() else {
                return Err(OutreachError::Conflict);
            };
            if entry.status != InvitationStatus::Delivering || at_ms < attempt.started_at_ms {
                return Err(OutreachError::Conflict);
            }
            let sent = matches!(result, AttemptResult::Sent { .. });
            attempt.result = Some(result);
            attempt.finished_at_ms = Some(at_ms);
            if sent {
                entry.status = InvitationStatus::Delivered;
                entry.delivered_at_ms = Some(at_ms);
                entry.closed_at_ms = Some(at_ms);
            } else if attempts >= MAX_ATTEMPTS {
                entry.status = InvitationStatus::Failed(OutreachFailure::Exhausted);
                entry.closed_at_ms = Some(at_ms);
            } else {
                // 没有送达，内容仍可在下一个时机投递。
                entry.status = InvitationStatus::Pending;
            }
            Ok(())
        })
    }

    pub(super) fn cancel(
        &self,
        id: &str,
        at_ms: u64,
        reason: CancelReason,
    ) -> OutreachResult<Invitation> {
        self.update(id, at_ms, |_, entry| {
            if entry.status != InvitationStatus::Pending || at_ms < last_time(entry) {
                return Err(OutreachError::Conflict);
            }
            entry.status = InvitationStatus::Cancelled(reason);
            entry.closed_at_ms = Some(at_ms);
            Ok(())
        })
    }

    pub(super) fn set_quiet(
        &self,
        owner: &str,
        quiet: bool,
        at_ms: u64,
    ) -> OutreachResult<OwnerPreference> {
        validate_id(owner)?;
        if at_ms == 0 {
            return Err(OutreachError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let preference = OwnerPreference {
            owner: owner.into(),
            quiet,
            changed_at_ms: at_ms,
        };
        let full = next.preferences.len() >= MAX_PREFERENCES;
        match next
            .preferences
            .iter_mut()
            .find(|entry| entry.owner == owner)
        {
            Some(entry) if entry.quiet == quiet => return Ok(entry.clone()),
            Some(entry) => {
                if at_ms < entry.changed_at_ms {
                    return Err(OutreachError::InvalidInput);
                }
                *entry = preference.clone();
            }
            None if full => return Err(OutreachError::LimitReached),
            None => next.preferences.push(preference.clone()),
        }
        persist(&mut inner, next)?;
        Ok(preference)
    }

    pub(super) fn begin_response(
        &self,
        id: &str,
        turns: Vec<ResponseTurn>,
        at_ms: u64,
    ) -> OutreachResult<Invitation> {
        self.update(id, at_ms, |_, entry| {
            if !entry.listening() || at_ms < last_time(entry) {
                return Err(OutreachError::Conflict);
            }
            let delivered = entry.delivered_at_ms.ok_or(OutreachError::Conflict)?;
            validate_turns(entry, &turns, delivered, at_ms)?;
            entry.responses.push(ResponseJudgement {
                turns,
                started_at_ms: at_ms,
                finished_at_ms: None,
                outcome: None,
            });
            Ok(())
        })
    }

    pub(super) fn record_response(
        &self,
        id: &str,
        at_ms: u64,
        outcome: Result<ResponseVerdict, OutreachFailure>,
    ) -> OutreachResult<Invitation> {
        self.update(id, at_ms, |_, entry| {
            let Some(response) = entry.responses.last_mut() else {
                return Err(OutreachError::Conflict);
            };
            if response.outcome.is_some() || at_ms < response.started_at_ms {
                return Err(OutreachError::Conflict);
            }
            response.outcome = Some(match outcome {
                Ok(verdict) => {
                    validate_response_verdict(response, &verdict)?;
                    ResponseOutcome::Verdict(verdict)
                }
                Err(failure) => ResponseOutcome::Failed(failure),
            });
            response.finished_at_ms = Some(at_ms);
            Ok(())
        })
    }

    fn update(
        &self,
        id: &str,
        at_ms: u64,
        apply: impl FnOnce(&Ledger, &mut Invitation) -> OutreachResult<()>,
    ) -> OutreachResult<Invitation> {
        if at_ms == 0 {
            return Err(OutreachError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let position = next
            .invitations
            .iter()
            .position(|entry| entry.id == id)
            .ok_or(OutreachError::NotFound)?;
        let mut entry = next.invitations[position].clone();
        apply(&next, &mut entry)?;
        next.invitations[position] = entry.clone();
        persist(&mut inner, next)?;
        Ok(entry)
    }

    pub(super) fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("主动交流状态锁不可用".into()))?
            .context = None;
        Ok(())
    }
}

/// 邀请上最近一次已记录的时间；之后的记录不能更早。
fn last_time(entry: &Invitation) -> u64 {
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
        .chain(
            entry
                .responses
                .iter()
                .flat_map(|response| [Some(response.started_at_ms), response.finished_at_ms]),
        )
        .flatten()
        .chain(entry.composed_at_ms)
        .chain([entry.created_at_ms])
        .max()
        .unwrap_or(0)
}

/// 一次识别的输入：1 至 3 轮、按时间先后、都在送达之后的回应窗口内且不晚于识别开始；
/// 不含附带邀请的那条消息，也不与之前识别过的对话重复。
fn validate_turns(
    entry: &Invitation,
    turns: &[ResponseTurn],
    delivered: u64,
    started: u64,
) -> OutreachResult<()> {
    let invalid = || OutreachError::InvalidInput;
    if turns.is_empty() || turns.len() > MAX_RESPONSE_TURNS {
        return Err(invalid());
    }
    let mut previous = delivered;
    let mut seen = BTreeSet::new();
    for turn in turns {
        validate_id(&turn.evidence_id)?;
        validate_id(&turn.message_id)?;
        if turn.at_ms < previous
            || turn.at_ms > started
            || turn.at_ms - delivered > RESPONSE_WINDOW_MS
            || entry.heard(&turn.message_id)
            || !seen.insert(turn.message_id.as_str())
        {
            return Err(invalid());
        }
        previous = turn.at_ms;
    }
    Ok(())
}

/// 结论须指向这次识别输入中的一条消息。
fn validate_response_verdict(
    response: &ResponseJudgement,
    verdict: &ResponseVerdict,
) -> OutreachResult<()> {
    validate_verdict_shape(verdict)?;
    if let Some(message_id) = &verdict.message_id
        && !response
            .turns
            .iter()
            .any(|turn| turn.message_id == *message_id)
    {
        return Err(OutreachError::InvalidInput);
    }
    Ok(())
}

fn validate_milestone(milestone: &Milestone) -> OutreachResult<()> {
    validate_id(&milestone.practice_run_id)?;
    if let Some(skill) = &milestone.skill_id {
        validate_id(skill)?;
    }
    Ok(())
}

fn validate_channel(channel: &DeliveryChannel) -> OutreachResult<()> {
    match channel {
        DeliveryChannel::Passive { message_id } => validate_id(message_id),
        DeliveryChannel::Proactive => Ok(()),
    }
}

fn validate_result(result: &AttemptResult) -> OutreachResult<()> {
    match result {
        AttemptResult::Sent {
            platform_message_id: Some(id),
        } => validate_id(id),
        _ => Ok(()),
    }
}

fn persist(inner: &mut Inner, next: Ledger) -> OutreachResult<()> {
    let bytes = encode(&next)?;
    let context = inner.context.as_ref().ok_or(OutreachError::Unavailable)?;
    // 失败也可能已经提交；关闭整个实例，禁止旧缓存继续读取或覆盖后端。
    if context.state_set(OUTREACH_STATE_KEY, bytes).is_err() {
        inner.context = None;
        return Err(OutreachError::Storage);
    }
    inner.ledger = next;
    Ok(())
}

fn encode(ledger: &Ledger) -> OutreachResult<Vec<u8>> {
    if ledger.invitations.len() > MAX_INVITATIONS || ledger.preferences.len() > MAX_PREFERENCES {
        return Err(OutreachError::LimitReached);
    }
    let bytes = serde_json::to_vec(ledger).map_err(|_| OutreachError::InvalidInput)?;
    // 重启时写入的中断与未知结局比原状态略长；预留空间保证能保存。
    let open = ledger
        .invitations
        .iter()
        .filter(|entry| {
            matches!(
                entry.status,
                InvitationStatus::Composing | InvitationStatus::Delivering
            ) || entry.judging()
                || entry.responding()
        })
        .count();
    if bytes.len().saturating_add(open * 32) > MAX_STATE_BYTES {
        return Err(OutreachError::LimitReached);
    }
    Ok(bytes)
}

/// 启动时完整核对；任何不一致都拒绝打开并保留原字节。
fn validate_ledger(ledger: &Ledger) -> OutreachResult<()> {
    let invalid = || OutreachError::InvalidInput;
    if ledger.format_version != FORMAT_VERSION
        || ledger.invitations.len() > MAX_INVITATIONS
        || ledger.preferences.len() > MAX_PREFERENCES
    {
        return Err(invalid());
    }
    let mut ids = BTreeSet::new();
    let mut busy = BTreeSet::new();
    for entry in &ledger.invitations {
        validate_invitation(entry)?;
        let open = entry.status == InvitationStatus::Delivering || entry.judging();
        if !ids.insert(entry.id.as_str()) || (open && !busy.insert(entry.owner.as_str())) {
            return Err(invalid());
        }
    }
    let mut owners = BTreeSet::new();
    for preference in &ledger.preferences {
        validate_id(&preference.owner)?;
        if preference.changed_at_ms == 0 || !owners.insert(preference.owner.as_str()) {
            return Err(invalid());
        }
    }
    Ok(())
}

fn validate_invitation(entry: &Invitation) -> OutreachResult<()> {
    let invalid = || OutreachError::InvalidInput;
    validate_id(&entry.owner)?;
    validate_id(&entry.goal_id)?;
    validate_id(&entry.composer_version)?;
    validate_milestone(&entry.milestone)?;
    validate_facts(&entry.facts)?;
    if entry.id != invitation_id(&entry.goal_id)
        || entry.created_at_ms == 0
        || entry.attempts.len() > MAX_ATTEMPTS
        || entry.proactive_attempts() > 1
    {
        return Err(invalid());
    }
    if let Some(text) = &entry.text {
        validate_text(text)?;
    }
    let composed = match (entry.composed_at_ms, &entry.text) {
        (Some(at), Some(_)) if at >= entry.created_at_ms => true,
        (None, None) => false,
        _ => return Err(invalid()),
    };
    // 判断按时间先后排列、每条消息至多一次；只有最后一次可以未结束，且只在 Pending 时进行。
    if entry.judgements.len() > MAX_JUDGEMENTS || (!composed && !entry.judgements.is_empty()) {
        return Err(invalid());
    }
    let mut judged = BTreeSet::new();
    let mut previous = entry.composed_at_ms.unwrap_or(entry.created_at_ms);
    let last = entry.judgements.len().saturating_sub(1);
    for (index, judgement) in entry.judgements.iter().enumerate() {
        validate_id(&judgement.message_id)?;
        if judgement.started_at_ms < previous || !judged.insert(judgement.message_id.as_str()) {
            return Err(invalid());
        }
        match (judgement.finished_at_ms, judgement.outcome) {
            (Some(at), Some(JudgementOutcome::Verdict(_) | JudgementOutcome::Failed(_)))
                if at >= judgement.started_at_ms =>
            {
                previous = at;
            }
            (None, Some(JudgementOutcome::Interrupted)) => previous = judgement.started_at_ms,
            (None, None) if index == last && entry.status == InvitationStatus::Pending => {}
            _ => return Err(invalid()),
        }
    }
    // 每次被动附带都对应一次“邀请”判断，且在判断之后。
    for attempt in &entry.attempts {
        if let DeliveryChannel::Passive { message_id } = &attempt.channel {
            let invited = entry.judgements.iter().any(|judgement| {
                judgement.message_id == *message_id
                    && judgement.outcome == Some(JudgementOutcome::Verdict(Verdict::Invite))
                    && judgement
                        .finished_at_ms
                        .is_some_and(|at| at <= attempt.started_at_ms)
            });
            let once = entry
                .attempts
                .iter()
                .filter(|other| other.channel == attempt.channel)
                .count()
                == 1;
            if !invited || !once {
                return Err(invalid());
            }
        }
    }
    // 尝试按时间先后排列；只有最后一次可以未结束。
    let mut previous = entry.composed_at_ms.unwrap_or(entry.created_at_ms);
    let last = entry.attempts.len().saturating_sub(1);
    for (index, attempt) in entry.attempts.iter().enumerate() {
        validate_channel(&attempt.channel)?;
        if let Some(result) = &attempt.result {
            validate_result(result)?;
        }
        if attempt.started_at_ms < previous {
            return Err(invalid());
        }
        let retryable = matches!(
            attempt.result,
            Some(AttemptResult::Failed { .. } | AttemptResult::NotSent)
        );
        match (attempt.finished_at_ms, &attempt.result) {
            (Some(at), Some(result))
                if at >= attempt.started_at_ms && *result != AttemptResult::Unknown =>
            {
                previous = at;
            }
            (None, None | Some(AttemptResult::Unknown)) if index == last => {}
            _ => return Err(invalid()),
        }
        if index < last && !retryable {
            return Err(invalid());
        }
    }
    let last_result = entry.attempts.last().map(|attempt| &attempt.result);
    let settled = entry
        .attempts
        .iter()
        .all(|attempt| attempt.finished_at_ms.is_some());
    let consistent = match entry.status {
        InvitationStatus::Composing | InvitationStatus::Interrupted => {
            !composed && entry.attempts.is_empty()
        }
        InvitationStatus::Pending => composed && settled,
        InvitationStatus::Delivering => composed && matches!(last_result, Some(None)),
        InvitationStatus::Delivered => {
            composed
                && matches!(last_result, Some(Some(AttemptResult::Sent { .. })))
                && entry.delivered_at_ms.is_some()
                && entry.delivered_at_ms == entry.closed_at_ms
                && entry
                    .attempts
                    .last()
                    .and_then(|attempt| attempt.finished_at_ms)
                    == entry.delivered_at_ms
        }
        InvitationStatus::Unknown => {
            composed && matches!(last_result, Some(Some(AttemptResult::Unknown)))
        }
        InvitationStatus::Cancelled(_) => composed && settled,
        InvitationStatus::Failed(OutreachFailure::Exhausted) => {
            composed && settled && entry.attempts.len() == MAX_ATTEMPTS
        }
        InvitationStatus::Failed(_) => !composed && entry.attempts.is_empty(),
    };
    let closed = matches!(
        entry.status,
        InvitationStatus::Delivered | InvitationStatus::Cancelled(_) | InvitationStatus::Failed(_)
    );
    let closed_ok = match entry.closed_at_ms {
        None => !closed,
        Some(at) => closed && at >= previous,
    };
    let delivered_ok =
        entry.delivered_at_ms.is_none() || entry.status == InvitationStatus::Delivered;
    if !consistent || !closed_ok || !delivered_ok {
        return Err(invalid());
    }
    validate_responses(entry)
}

/// 回应识别只在送达之后，按时间先后、每轮对话至多一次；只有最后一次可以未结束；
/// 得出回应结论后不再识别。
fn validate_responses(entry: &Invitation) -> OutreachResult<()> {
    let invalid = || OutreachError::InvalidInput;
    if entry.responses.is_empty() {
        return Ok(());
    }
    let Some(delivered) = entry.delivered_at_ms else {
        return Err(invalid());
    };
    if entry.status != InvitationStatus::Delivered || entry.responses.len() > MAX_RESPONSES {
        return Err(invalid());
    }
    let mut earlier = entry.clone();
    earlier.responses.clear();
    let mut previous = delivered;
    let last = entry.responses.len() - 1;
    for (index, response) in entry.responses.iter().enumerate() {
        if earlier.feedback().is_some() || response.started_at_ms < previous {
            return Err(invalid());
        }
        validate_turns(&earlier, &response.turns, delivered, response.started_at_ms)?;
        match (response.finished_at_ms, &response.outcome) {
            (Some(at), Some(outcome)) if at >= response.started_at_ms => {
                match outcome {
                    ResponseOutcome::Verdict(verdict) => {
                        validate_response_verdict(response, verdict)?
                    }
                    ResponseOutcome::Failed(_) => {}
                    ResponseOutcome::Interrupted => return Err(invalid()),
                }
                previous = at;
            }
            (None, Some(ResponseOutcome::Interrupted)) => previous = response.started_at_ms,
            (None, None) if index == last => {}
            _ => return Err(invalid()),
        }
        earlier.responses.push(response.clone());
    }
    Ok(())
}
