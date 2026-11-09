//! 完整事件日志之上的持久消费游标。领取不丢原文，结果与经验版本原子提交。
use super::{Conversations, RETRY_MS, lesson_valid};
use crate::expression::hash;
use eve_llm_api::ContextScope;
use eve_plugin_api::{PluginError, PluginResult};
use eve_training_api::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

fn state(message: &str) -> PluginError {
    PluginError::State(message.into())
}
fn rejected(message: &str) -> PluginError {
    PluginError::Task(message.into())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    sequence: u64,
    text_offset: usize,
    context_offset: usize,
}
impl Cursor {
    fn at(sequence: u64) -> Self {
        Self {
            sequence,
            text_offset: 0,
            context_offset: 0,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    sweep: bool,
    batch: ConversationBatch,
    after: Cursor,
    /// 只有宿主已经明确记录失败，才可重新请求这次无工具学习。
    retryable: bool,
}

#[derive(Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SweepOutcome {
    Succeeded,
    Failed,
    Unknown,
}
impl SweepOutcome {
    fn combine(self, previous: Option<Self>) -> Self {
        match (self, previous) {
            (Self::Unknown, _) | (_, Some(Self::Unknown)) => Self::Unknown,
            (Self::Failed, _) | (_, Some(Self::Failed)) => Self::Failed,
            _ => Self::Succeeded,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SweepAccounting {
    failed: u64,
    unknown: u64,
    unclassified: u64,
    legacy_processed: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    partial: Option<SweepOutcome>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sweep {
    id: String,
    sequences: Vec<u64>,
    cursor: Cursor,
    processed: u64,
    updated: u64,
    failed_batches: u64,
    interrupted_batches: u64,
    #[serde(default)]
    model_requests: u64,
    #[serde(default)]
    covered: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    accounting: Option<SweepAccounting>,
}
impl Sweep {
    fn pending(&self) -> bool {
        self.cursor.sequence <= self.sequences.len() as u64
    }
    fn initialize_accounting(&mut self) {
        if self.accounting.is_none() {
            self.accounting = Some(SweepAccounting {
                unclassified: self.cursor.sequence - 1,
                legacy_processed: self.processed,
                partial: (self.cursor.text_offset != 0 || self.cursor.context_offset != 0)
                    .then_some(SweepOutcome::Unknown),
                ..Default::default()
            });
            self.processed = 0;
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    version: u8,
    generation: u64,
    next_id: u64,
    cursor: Cursor,
    context_from: u64,
    ready_at: u64,
    pending_since_ms: Option<u64>,
    last_claimed_at: u64,
    inflight: Option<Claim>,
    lessons: Vec<ConversationLesson>,
    lesson_cursor: usize,
    history_cursor: u64,
    #[serde(default)]
    history_from: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sweep: Option<Sweep>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    sweep_requests: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    sweep_archive: Vec<Sweep>,
}
impl Default for Checkpoint {
    fn default() -> Self {
        Self {
            version: 2,
            generation: 1,
            next_id: 1,
            cursor: Cursor::at(1),
            context_from: 1,
            ready_at: 0,
            pending_since_ms: None,
            last_claimed_at: 0,
            inflight: None,
            lessons: vec![],
            lesson_cursor: 0,
            history_cursor: 0,
            history_from: 0,
            sweep: None,
            sweep_requests: vec![],
            sweep_archive: vec![],
        }
    }
}
impl Checkpoint {
    fn bytes(&self) -> PluginResult<Vec<u8>> {
        serde_json::to_vec(self).map_err(|_| state("交流消费检查点编码失败"))
    }
    fn load(row: &ConversationStoredScope, store: &dyn ConversationStore) -> PluginResult<Self> {
        let Some(bytes) = &row.checkpoint else {
            let value = Self::default();
            validate_checkpoint(store, row, &value)?;
            return Ok(value);
        };
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(state("交流消费检查点超限；保留原记录"));
        }
        let mut value: Self =
            serde_json::from_slice(bytes).map_err(|_| state("交流消费检查点损坏；保留原记录"))?;
        let mut ids = BTreeSet::new();
        if value.version != 2
            || value.generation == 0
            || value.next_id == 0
            || value.cursor.sequence == 0
            || value.cursor.sequence > row.next_sequence
            || value.context_from == 0
            || value.context_from > value.cursor.sequence
            || value.cursor.text_offset > MAX_CONVERSATION_BODY_BYTES
            || value.cursor.context_offset > MAX_CONVERSATION_CONTEXT_BYTES
            || (value.cursor.sequence == row.next_sequence
                && (value.cursor.text_offset != 0 || value.cursor.context_offset != 0))
            || value.lessons.len() > MAX_CONVERSATION_LESSONS
            || value.history_cursor < value.history_from
            || value
                .lessons
                .iter()
                .any(|l| !lesson_valid(l) || !ids.insert(&l.id))
            || value.inflight.as_ref().is_some_and(|claim| {
                !super::identifier(&claim.batch.id)
                    || claim.batch.scope != row.scope
                    || !(1..=3).contains(&claim.batch.attempt)
                    || claim.retryable && claim.batch.attempt >= 3
                    || claim.batch.messages.is_empty()
                    || claim.batch.messages.len()
                        > if claim.sweep {
                            MAX_CONVERSATION_REVISIT_MESSAGES
                        } else {
                            MAX_CONVERSATION_MESSAGES
                        }
                    || claim
                        .batch
                        .messages
                        .iter()
                        .any(|m| !conversation_message_valid(m))
                    || claim
                        .batch
                        .messages
                        .iter()
                        .map(|m| &m.id)
                        .collect::<BTreeSet<_>>()
                        .len()
                        != claim.batch.messages.len()
                    || !claim.sweep && claim.after.sequence < value.cursor.sequence
                    || !claim.sweep && claim.after.sequence > row.next_sequence
                    || claim.after.text_offset > MAX_CONVERSATION_BODY_BYTES
                    || claim.after.context_offset > MAX_CONVERSATION_CONTEXT_BYTES
                    || (!claim.sweep
                        && claim.after.sequence == row.next_sequence
                        && (claim.after.text_offset != 0 || claim.after.context_offset != 0))
                    || claim.batch.lessons.len() > MAX_CONVERSATION_LESSONS
                    || claim.batch.lessons.iter().any(|l| !lesson_valid(l))
                    || claim
                        .batch
                        .lessons
                        .iter()
                        .map(|l| &l.id)
                        .collect::<BTreeSet<_>>()
                        .len()
                        != claim.batch.lessons.len()
                    || claim.batch.lesson_history.len() > 4
                    || claim.batch.lesson_history.iter().any(|r| {
                        r.sequence == 0
                            || !lesson_valid(&r.lesson)
                            || !super::identifier(&r.reason)
                            || r.batch_id.as_ref().is_some_and(|id| !super::identifier(id))
                    })
                    || claim
                        .batch
                        .lesson_history
                        .iter()
                        .map(|r| r.sequence)
                        .collect::<BTreeSet<_>>()
                        .len()
                        != claim.batch.lesson_history.len()
            })
        {
            return Err(state("交流消费检查点内容无效；未清空"));
        }
        validate_checkpoint(store, row, &value)?;
        // 不扫描批次归档来猜旧分片结果；下次真实提交时一并保存迁移后的统计。
        for sweep in value.sweep.iter_mut().chain(&mut value.sweep_archive) {
            sweep.initialize_accounting();
        }
        Ok(value)
    }
    fn next_name(&mut self, prefix: &str) -> PluginResult<String> {
        let value = format!("{prefix}-v2-{}-{}", self.next_id, self.generation);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| state("交流学习序号耗尽"))?;
        Ok(value)
    }
}

fn original_event(
    store: &dyn ConversationStore,
    scope: &ContextScope,
    sequence: u64,
) -> PluginResult<ConversationEvent> {
    let event = store
        .events(scope, sequence, 1)?
        .into_iter()
        .next()
        .filter(|event| event.sequence == sequence)
        .ok_or_else(|| state("交流检查点引用缺失的原事件；保留原记录"))?;
    if !conversation_message_valid(&event.message)
        || event.message.metadata.as_ref().is_some_and(|m| {
            m.fragment.is_some() || m.sequence.is_some_and(|value| value != sequence)
        })
    {
        return Err(state("交流检查点引用的原事件不一致"));
    }
    Ok(event)
}
fn validate_cursor(
    store: &dyn ConversationStore,
    row: &ConversationStoredScope,
    cursor: &Cursor,
) -> PluginResult<()> {
    if cursor.sequence == 0 || cursor.sequence > row.next_sequence {
        return Err(state("交流消费位置超出已保存原事件"));
    }
    if cursor.sequence == row.next_sequence {
        return if cursor.text_offset == 0 && cursor.context_offset == 0 {
            Ok(())
        } else {
            Err(state("交流末尾消费位置不能含分片偏移"))
        };
    }
    let event = original_event(store, &row.scope, cursor.sequence)?;
    let context = context_text(&event.message);
    if !event.message.text.is_char_boundary(cursor.text_offset)
        || !context.is_char_boundary(cursor.context_offset)
        || cursor.context_offset != 0 && cursor.text_offset != event.message.text.len()
        || cursor.text_offset == event.message.text.len() && cursor.context_offset == context.len()
    {
        return Err(state("交流消费分片位置不在原消息的合法边界"));
    }
    Ok(())
}
fn validate_checkpoint(
    store: &dyn ConversationStore,
    row: &ConversationStoredScope,
    checkpoint: &Checkpoint,
) -> PluginResult<()> {
    validate_cursor(store, row, &checkpoint.cursor)?;
    for lesson in &checkpoint.lessons {
        let revision = store.latest_revision(&row.scope, &lesson.id)?;
        if revision.as_ref().map(|revision| &revision.lesson) != Some(lesson) {
            return Err(state("交流当前经验与已保存版本不一致；保留原记录"));
        }
    }
    for sweep in checkpoint.sweep.iter().chain(&checkpoint.sweep_archive) {
        if !super::identifier(&sweep.id)
            || sweep.sequences.len() > 250_000
            || sweep.cursor.sequence == 0
            || sweep.cursor.sequence > sweep.sequences.len() as u64 + 1
            || sweep
                .sequences
                .iter()
                .any(|s| *s == 0 || *s >= row.next_sequence)
            || sweep.sequences.iter().collect::<BTreeSet<_>>().len() != sweep.sequences.len()
            || sweep.processed > sweep.cursor.sequence - 1
        {
            return Err(state("交流重访检查点无效"));
        }
        let partial = sweep.cursor.text_offset != 0 || sweep.cursor.context_offset != 0;
        if partial {
            let sequence = sweep
                .sequences
                .get(sweep.cursor.sequence as usize - 1)
                .ok_or_else(|| state("交流重访末尾不能含分片偏移"))?;
            let mut cursor = sweep.cursor.clone();
            cursor.sequence = *sequence;
            validate_cursor(store, row, &cursor)?;
        }
        if let Some(accounting) = &sweep.accounting {
            let consumed = [
                sweep.processed,
                accounting.failed,
                accounting.unknown,
                accounting.unclassified,
            ]
            .into_iter()
            .try_fold(0_u64, u64::checked_add);
            if consumed != Some(sweep.cursor.sequence - 1)
                || accounting.legacy_processed > accounting.unclassified
                || accounting.partial.is_some() != partial
            {
                return Err(state("交流重访消息统计与消费位置不一致；保留原记录"));
            }
        }
    }
    let Some(claim) = &checkpoint.inflight else {
        return Ok(());
    };
    if claim.sweep {
        let sweep = checkpoint
            .sweep
            .as_ref()
            .ok_or_else(|| state("交流重访批次缺少请求"))?;
        let mut expected = sweep.cursor.clone();
        for message in &claim.batch.messages {
            let sequence = *sweep
                .sequences
                .get(expected.sequence as usize - 1)
                .ok_or_else(|| state("交流重访批次越过冻结来源"))?;
            let event = original_event(store, &row.scope, sequence)?;
            let mut offset = expected.clone();
            offset.sequence = sequence;
            let total = event.message.text.len() + context_text(&event.message).len();
            let end = message
                .metadata
                .as_ref()
                .and_then(|m| m.fragment.as_ref())
                .map_or(total, |f| f.text_end + f.context_end);
            if project(&event, &offset, end)? != *message {
                return Err(state("交流重访批次原文不一致"));
            }
            expected = if end == total {
                Cursor::at(expected.sequence + 1)
            } else {
                Cursor {
                    sequence: expected.sequence,
                    text_offset: end.min(event.message.text.len()),
                    context_offset: end.saturating_sub(event.message.text.len()),
                }
            };
        }
        if expected != claim.after || !fits(&claim.batch)? {
            return Err(state("交流重访批次进度不一致"));
        }
        return validate_claim_lessons(store, row, checkpoint, claim);
    }
    let mut expected = checkpoint.cursor.clone();
    let mut previous_sequence = 0;
    let mut progressed = false;
    for message in &claim.batch.messages {
        let sequence = message
            .metadata
            .as_ref()
            .and_then(|m| m.sequence)
            .ok_or_else(|| state("交流固定批次缺少原事件顺序"))?;
        if sequence <= previous_sequence {
            return Err(state("交流固定批次事件顺序冲突"));
        }
        previous_sequence = sequence;
        let event = original_event(store, &row.scope, sequence)?;
        if sequence < checkpoint.cursor.sequence {
            if progressed || sequence < checkpoint.context_from || message != &event.message {
                return Err(state("交流固定批次的已消费背景与原事件不符"));
            }
            continue;
        }
        if sequence != expected.sequence {
            return Err(state("交流固定批次越过未消费原事件"));
        }
        let body = event.message.text.len();
        let context = context_text(&event.message).len();
        let end =
            if let Some(fragment) = message.metadata.as_ref().and_then(|m| m.fragment.as_ref()) {
                if fragment.text_start != expected.text_offset
                    || fragment.context_start != expected.context_offset
                    || fragment.text_bytes != body
                    || fragment.context_bytes != context
                    || fragment.context_end != 0 && fragment.text_end != body
                {
                    return Err(state("交流固定分片与原正文区间不符"));
                }
                fragment.text_end + fragment.context_end
            } else {
                body + context
            };
        if project(&event, &expected, end)? != *message {
            return Err(state("交流固定批次原文、来源或分片不一致"));
        }
        let after = if end == body + context {
            Cursor::at(
                sequence
                    .checked_add(1)
                    .ok_or_else(|| state("交流事件序号耗尽"))?,
            )
        } else {
            Cursor {
                sequence,
                text_offset: end.min(body),
                context_offset: end.saturating_sub(body),
            }
        };
        if after == expected {
            return Err(state("交流固定分片没有推进消费位置"));
        }
        expected = after;
        progressed = true;
    }
    if !progressed || expected != claim.after || !fits(&claim.batch)? {
        return Err(state("交流固定批次消费结尾或字节预算不一致"));
    }
    validate_cursor(store, row, &claim.after)?;
    validate_claim_lessons(store, row, checkpoint, claim)
}
fn validate_claim_lessons(
    store: &dyn ConversationStore,
    row: &ConversationStoredScope,
    checkpoint: &Checkpoint,
    claim: &Claim,
) -> PluginResult<()> {
    for lesson in &claim.batch.lessons {
        if !checkpoint.lessons.contains(lesson) {
            return Err(state("交流固定批次的当前经验版本不一致"));
        }
    }
    for revision in &claim.batch.lesson_history {
        if revision.sequence <= checkpoint.history_from {
            return Err(state("交流固定批次越过经验重置边界"));
        }
        let original = store
            .revisions(&row.scope, revision.sequence - 1, 1)?
            .into_iter()
            .next();
        if original.as_ref() != Some(revision) {
            return Err(state("交流固定批次的经验历史来源不一致"));
        }
    }
    Ok(())
}

pub(crate) struct PersistentConversations {
    store: Arc<dyn ConversationStore>,
    writable: AtomicBool,
    gate: Mutex<()>,
}
impl PersistentConversations {
    pub(crate) fn open(
        store: Arc<dyn ConversationStore>,
        legacy_bytes: Option<Vec<u8>>,
    ) -> PluginResult<Self> {
        let legacy = Conversations::load(legacy_bytes.clone())?;
        let digest = hash(legacy_bytes.as_deref().unwrap_or(b"conversation.v1:absent"));
        store.import_legacy(&digest, &legacy_import(&legacy)?)?;
        let value = Self {
            store,
            writable: AtomicBool::new(true),
            gate: Mutex::new(()),
        };
        value.run(|| {
            for row in value.store.scopes()? {
                let mut checkpoint = Checkpoint::load(&row, value.store.as_ref())?;
                if checkpoint.inflight.as_ref().is_some_and(|c| !c.retryable) {
                    let claim = checkpoint.inflight.take().expect("checked claim");
                    // 可能已经请求过模型，不能凭进程重启自动重放。原批次完整保留供核查。
                    advance(&mut checkpoint, &claim, "interrupted", 0)?;
                    checkpoint.pending_since_ms = None;
                    checkpoint.ready_at = 0;
                    value.save(
                        &row,
                        &checkpoint,
                        vec![],
                        Some(ConversationBatchArchive {
                            batch: claim.batch,
                            outcome: "interrupted".into(),
                            recorded_at_ms: None,
                        }),
                    )?;
                }
            }
            Ok(())
        })?;
        Ok(value)
    }
    fn run<T>(&self, operation: impl FnOnce() -> PluginResult<T>) -> PluginResult<T> {
        let _guard = self
            .gate
            .lock()
            .map_err(|_| state("交流持久状态锁不可用"))?;
        if !self.writable.load(Ordering::Acquire) {
            return Err(state(
                "交流保存结果未知；旧实例停止读写，重启后核对持久结果",
            ));
        }
        let result = operation();
        if result
            .as_ref()
            .is_err_and(|e| matches!(e, PluginError::State(_)))
        {
            self.writable.store(false, Ordering::Release);
        }
        result
    }
    fn save(
        &self,
        row: &ConversationStoredScope,
        checkpoint: &Checkpoint,
        revisions: Vec<ConversationLessonRevision>,
        batch: Option<ConversationBatchArchive>,
    ) -> PluginResult<()> {
        if !self.store.commit(
            &row.scope,
            &ConversationCommit {
                expected_revision: row.revision,
                checkpoint: checkpoint.bytes()?,
                revisions,
                batch,
            },
        )? {
            return Err(rejected("交流检查点已被另一写入更新；本次未写入"));
        }
        Ok(())
    }
    pub(crate) fn observe(
        &self,
        scope: &ContextScope,
        message: ConversationMessage,
        enabled: bool,
    ) -> PluginResult<bool> {
        self.run(|| {
            self.store
                .append(scope, &message, enabled)
                .map(|result| result.inserted)
        })
    }
    pub(crate) fn lessons(&self, scope: &ContextScope) -> PluginResult<Vec<ConversationLesson>> {
        self.run(|| {
            self.store
                .scope(scope)?
                .map(|row| Checkpoint::load(&row, self.store.as_ref()).map(|c| c.lessons))
                .transpose()
                .map(Option::unwrap_or_default)
        })
    }
    pub(crate) fn take(
        &self,
        now: u64,
        enabled: impl Fn(&ContextScope) -> bool,
        maintenance: bool,
    ) -> PluginResult<Option<ConversationBatch>> {
        self.run(|| {
            let mut candidates = self
                .store
                .scopes()?
                .into_iter()
                .map(|row| {
                    Checkpoint::load(&row, self.store.as_ref()).map(|checkpoint| (row, checkpoint))
                })
                .collect::<PluginResult<Vec<_>>>()?;
            let mut request_usage = std::collections::BTreeMap::<String, u64>::new();
            for (_, checkpoint) in &candidates {
                for sweep in checkpoint.sweep.iter().chain(&checkpoint.sweep_archive) {
                    let used = request_usage.entry(sweep.id.clone()).or_default();
                    *used = used.saturating_add(sweep.model_requests);
                }
            }
            candidates.sort_by_key(|(_, c)| (c.last_claimed_at, c.ready_at));
            for (row, mut checkpoint) in candidates {
                if checkpoint
                    .inflight
                    .as_ref()
                    .is_some_and(|claim| claim.sweep && claim.retryable)
                    && checkpoint.sweep.as_ref().is_some_and(|sweep| {
                        request_usage
                            .get(&sweep.id)
                            .is_some_and(|used| *used >= 4096)
                    })
                {
                    // 维护入口在额度用尽后可能不再运行；普通领取也可完成这次本地结算。
                    // 这里只处理已明确失败的等待重试批次，不触碰在途未知请求。
                    let claim = checkpoint.inflight.take().expect("checked retryable claim");
                    advance(&mut checkpoint, &claim, "failed", 0)?;
                    checkpoint.pending_since_ms = None;
                    checkpoint.ready_at = 0;
                    self.save(
                        &row,
                        &checkpoint,
                        vec![],
                        Some(ConversationBatchArchive {
                            batch: claim.batch,
                            outcome: "budget_exhausted".into(),
                            recorded_at_ms: Some(now),
                        }),
                    )?;
                    return Ok(None);
                }
                if checkpoint
                    .inflight
                    .as_ref()
                    .is_some_and(|claim| maintenance != claim.sweep)
                    || maintenance && !checkpoint.sweep.as_ref().is_some_and(Sweep::pending)
                {
                    continue;
                }
                if !maintenance && !enabled(&row.scope) {
                    continue;
                }
                if now < checkpoint.ready_at {
                    continue;
                }
                if let Some(claim) = &mut checkpoint.inflight {
                    if !claim.retryable {
                        continue;
                    }
                    if claim.batch.attempt >= 3 {
                        return Err(state("交流失败重试检查点无效"));
                    }
                    claim.batch.attempt += 1;
                    if claim.sweep {
                        checkpoint
                            .sweep
                            .as_mut()
                            .expect("validated sweep")
                            .model_requests += 1;
                    }
                    claim.retryable = false;
                    let batch = claim.batch.clone();
                    checkpoint.last_claimed_at = now;
                    self.save(&row, &checkpoint, vec![], None)?;
                    return Ok(Some(batch));
                }
                if maintenance {
                    let batch = self.take_sweep(&row, &mut checkpoint, now)?;
                    return Ok(Some(batch));
                }
                // 冻结来源由独立维护游标负责；只越过集合内的事件，保留夹在其间的新输入。
                // 旧请求也保留预约，不能在换请求或预算耗尽后复活失败、未知来源。
                let reserved: BTreeSet<_> = checkpoint
                    .sweep
                    .iter()
                    .chain(&checkpoint.sweep_archive)
                    .flat_map(|sweep| sweep.sequences.iter().copied())
                    .collect();
                let before = checkpoint.cursor.clone();
                while reserved.contains(&checkpoint.cursor.sequence) {
                    checkpoint.cursor = Cursor::at(checkpoint.cursor.sequence + 1);
                }
                let cursor_changed = checkpoint.cursor != before;
                if checkpoint.cursor.sequence >= row.next_sequence {
                    if cursor_changed {
                        self.save(&row, &checkpoint, vec![], None)?;
                    }
                    continue;
                }
                let events = self.store.events(
                    &row.scope,
                    checkpoint.cursor.sequence,
                    MAX_CONVERSATION_MESSAGES,
                )?;
                let Some(first) = events.first() else {
                    return Err(state("交流消费游标指向缺失事件"));
                };
                if first.sequence != checkpoint.cursor.sequence {
                    return Err(state("交流事件序号不连续"));
                }
                let count = row.next_sequence - checkpoint.cursor.sequence;
                let partial =
                    checkpoint.cursor.text_offset != 0 || checkpoint.cursor.context_offset != 0;
                let large = first.message.text.len() + context_text(&first.message).len() > 8_192;
                if count < CONVERSATION_BATCH_MESSAGES as u64 && !partial && !large {
                    let first_observed = first
                        .message
                        .metadata
                        .as_ref()
                        .and_then(|m| m.observed_at_ms);
                    let since = checkpoint
                        .pending_since_ms
                        .or(first_observed.filter(|t| *t <= now))
                        .unwrap_or(now);
                    if now.saturating_sub(since) < RETRY_MS {
                        if checkpoint.pending_since_ms.is_none() || cursor_changed {
                            checkpoint.pending_since_ms = Some(since);
                            self.save(&row, &checkpoint, vec![], None)?;
                        }
                        continue;
                    }
                }
                let id = checkpoint.next_name("conversation")?;
                let mut batch = ConversationBatch {
                    id,
                    scope: row.scope.clone(),
                    messages: vec![],
                    lessons: vec![],
                    lesson_history: vec![],
                    attempt: 1,
                };
                self.lesson_page(&row.scope, &mut checkpoint, &mut batch)?;
                let start = checkpoint.cursor.clone();
                let mut after = start.clone();
                for event in &events {
                    if reserved.contains(&event.sequence) {
                        break;
                    }
                    if event.sequence != after.sequence {
                        break;
                    }
                    let full = project(
                        event,
                        &after,
                        event.message.text.len() + context_text(&event.message).len(),
                    )?;
                    batch.messages.push(full);
                    if fits(&batch)? {
                        after = Cursor::at(
                            event
                                .sequence
                                .checked_add(1)
                                .ok_or_else(|| state("交流事件序号耗尽"))?,
                        );
                    } else {
                        batch.messages.pop();
                        if !batch.messages.is_empty() {
                            break;
                        }
                        let (message, cursor) = fitting_fragment(event, &after, &mut batch)?;
                        batch.messages.push(message);
                        after = cursor;
                        break;
                    }
                    if batch.messages.len() >= MAX_CONVERSATION_MESSAGES {
                        break;
                    }
                }
                if batch.messages.is_empty() || after == start {
                    return Err(state("交流批次未能推进消费游标"));
                }
                // 最近的已消费原文只是连续性背景，正文太大时不挤走新事件。
                let from = start
                    .sequence
                    .saturating_sub(12)
                    .max(checkpoint.context_from);
                let previous = self.store.events(&row.scope, from, 12)?;
                for event in previous
                    .iter()
                    .rev()
                    .filter(|e| e.sequence < start.sequence)
                {
                    if batch.messages.len() >= MAX_CONVERSATION_MESSAGES {
                        break;
                    }
                    batch.messages.insert(0, event.message.clone());
                    if !fits(&batch)? {
                        batch.messages.remove(0);
                        break;
                    }
                }
                if !fits(&batch)? {
                    return Err(state("交流批次实际编码超过预算"));
                }
                checkpoint.pending_since_ms = None;
                checkpoint.last_claimed_at = now;
                checkpoint.inflight = Some(Claim {
                    sweep: false,
                    batch: batch.clone(),
                    after,
                    retryable: false,
                });
                self.save(&row, &checkpoint, vec![], None)?;
                return Ok(Some(batch));
            }
            Ok(None)
        })
    }
    pub(crate) fn take_maintenance(
        &self,
        now: u64,
        max_requests: u64,
    ) -> PluginResult<Option<ConversationBatch>> {
        if max_requests == 0 || max_requests > 4096 {
            return Err(rejected("交流维护请求额度无效"));
        }
        let used = self.run(|| {
            let mut requests = std::collections::BTreeMap::<String, (u64, bool)>::new();
            for row in self.store.scopes()? {
                let checkpoint = Checkpoint::load(&row, self.store.as_ref())?;
                for sweep in checkpoint.sweep.into_iter().chain(checkpoint.sweep_archive) {
                    let entry = requests.entry(sweep.id.clone()).or_default();
                    entry.0 += sweep.model_requests;
                    entry.1 |= sweep.pending();
                }
            }
            Ok(requests
                .into_values()
                .filter(|(_, pending)| *pending)
                .map(|(used, _)| used)
                .max()
                .unwrap_or(0))
        })?;
        if used >= max_requests {
            return Ok(None);
        }
        self.take(now, |_| true, true)
    }
    pub(crate) fn import_history(
        &self,
        record: &ConversationHistoryRecord,
    ) -> PluginResult<ConversationAppend> {
        self.run(|| {
            self.store
                .import_history(&record.scope, &record.event.message)
        })
    }
    pub(crate) fn request_sweep(
        &self,
        id: &str,
        scope: &ContextScope,
        sequences: &[u64],
    ) -> PluginResult<()> {
        self.run(|| {
            if !super::identifier(id)
                || sequences.is_empty()
                || sequences.len() > 250_000
                || sequences.iter().collect::<BTreeSet<_>>().len() != sequences.len()
            {
                return Err(rejected("交流重访请求无效"));
            }
            let row = self
                .store
                .scope(scope)?
                .ok_or_else(|| rejected("交流重访范围不存在"))?;
            let mut checkpoint = Checkpoint::load(&row, self.store.as_ref())?;
            let request_digest =
                hash(&serde_json::to_vec(sequences).map_err(|_| state("交流重访来源编码失败"))?);
            if let Some((_, previous)) = checkpoint
                .sweep_requests
                .iter()
                .find(|(request, _)| request == id)
            {
                return if previous == &request_digest {
                    Ok(())
                } else {
                    Err(rejected("交流重访请求ID内容冲突"))
                };
            }
            if checkpoint.sweep_requests.len() >= 256 {
                return Err(rejected("交流重访请求历史已满；旧记录保留"));
            }
            if let Some(previous) = &checkpoint.sweep {
                if previous.id == id {
                    return if previous.sequences == sequences {
                        Ok(())
                    } else {
                        Err(rejected("交流重访请求ID内容冲突"))
                    };
                }
                if previous.pending() {
                    return Err(rejected("已有交流重访未完成"));
                }
            }
            if sequences.iter().any(|s| *s == 0 || *s >= row.next_sequence) {
                return Err(rejected("交流重访来源未完整导入"));
            }
            if let Some(previous) = checkpoint.sweep.take() {
                checkpoint.sweep_archive.push(previous);
            }
            checkpoint.sweep_requests.push((id.into(), request_digest));
            checkpoint.sweep = Some(Sweep {
                id: id.into(),
                sequences: sequences.to_vec(),
                cursor: Cursor::at(1),
                processed: 0,
                updated: 0,
                failed_batches: 0,
                interrupted_batches: 0,
                model_requests: 0,
                covered: vec![],
                accounting: Some(SweepAccounting::default()),
            });
            self.save(&row, &checkpoint, vec![], None)
        })
    }
    pub(crate) fn sweep_status(&self, id: &str) -> PluginResult<ConversationSweepStatus> {
        self.run(|| {
            let mut status = ConversationSweepStatus {
                id: id.into(),
                message_counts: Some(ConversationSweepMessageCounts::default()),
                ..Default::default()
            };
            for row in self.store.scopes()? {
                let checkpoint = Checkpoint::load(&row, self.store.as_ref())?;
                if let Some(sweep) = checkpoint
                    .sweep
                    .into_iter()
                    .chain(checkpoint.sweep_archive)
                    .find(|s| s.id == id)
                {
                    status.scopes += 1;
                    status.total += sweep.sequences.len() as u64;
                    status.processed += sweep.processed;
                    status.remaining +=
                        (sweep.sequences.len() as u64 + 1).saturating_sub(sweep.cursor.sequence);
                    status.updated += sweep.updated;
                    status.failed_batches += sweep.failed_batches;
                    status.interrupted_batches += sweep.interrupted_batches;
                    status.model_requests += sweep.model_requests;
                    let accounting = sweep.accounting.expect("initialized accounting");
                    let counts = status.message_counts.as_mut().expect("initialized counts");
                    counts.failed += accounting.failed;
                    counts.unknown += accounting.unknown;
                    counts.unclassified += accounting.unclassified;
                    counts.legacy_processed += accounting.legacy_processed;
                    counts.partial += u64::from(accounting.partial.is_some());
                    counts.partial_failed +=
                        u64::from(accounting.partial == Some(SweepOutcome::Failed));
                    counts.partial_unknown +=
                        u64::from(accounting.partial == Some(SweepOutcome::Unknown));
                }
            }
            status.completed = status.scopes > 0 && status.remaining == 0;
            Ok(status)
        })
    }
    fn take_sweep(
        &self,
        row: &ConversationStoredScope,
        checkpoint: &mut Checkpoint,
        now: u64,
    ) -> PluginResult<ConversationBatch> {
        let mut batch = ConversationBatch {
            id: checkpoint.next_name("conversation-revisit")?,
            scope: row.scope.clone(),
            messages: vec![],
            lessons: vec![],
            lesson_history: vec![],
            attempt: 1,
        };
        self.lesson_page(&row.scope, checkpoint, &mut batch)?;
        let sweep = checkpoint
            .sweep
            .as_ref()
            .ok_or_else(|| state("交流重访请求缺失"))?;
        let mut after = sweep.cursor.clone();
        while let Some(sequence) = sweep.sequences.get(after.sequence as usize - 1) {
            let event = original_event(self.store.as_ref(), &row.scope, *sequence)?;
            let mut offset = after.clone();
            offset.sequence = *sequence;
            let total = event.message.text.len() + context_text(&event.message).len();
            batch.messages.push(project(&event, &offset, total)?);
            if fits(&batch)? {
                after = Cursor::at(after.sequence + 1);
            } else {
                batch.messages.pop();
                if !batch.messages.is_empty() {
                    break;
                }
                let (message, cursor) = fitting_fragment(&event, &offset, &mut batch)?;
                batch.messages.push(message);
                after = if cursor.sequence > *sequence {
                    Cursor::at(after.sequence + 1)
                } else {
                    Cursor {
                        sequence: after.sequence,
                        text_offset: cursor.text_offset,
                        context_offset: cursor.context_offset,
                    }
                };
                break;
            }
            if batch.messages.len() >= MAX_CONVERSATION_REVISIT_MESSAGES {
                break;
            }
        }
        if batch.messages.is_empty() || after == sweep.cursor {
            return Err(state("交流重访批次没有推进"));
        }
        checkpoint.last_claimed_at = now;
        checkpoint
            .sweep
            .as_mut()
            .expect("validated sweep")
            .model_requests += 1;
        checkpoint.inflight = Some(Claim {
            sweep: true,
            batch: batch.clone(),
            after,
            retryable: false,
        });
        self.save(row, checkpoint, vec![], None)?;
        Ok(batch)
    }
    fn lesson_page(
        &self,
        scope: &ContextScope,
        checkpoint: &mut Checkpoint,
        batch: &mut ConversationBatch,
    ) -> PluginResult<()> {
        let len = checkpoint.lessons.len();
        if len > 0 {
            let start = checkpoint.lesson_cursor % len;
            for offset in 0..len {
                batch
                    .lessons
                    .push(checkpoint.lessons[(start + offset) % len].clone());
                if conversation_prompt_json(batch)?.len() > MAX_CONVERSATION_PROMPT_BYTES - 16_384 {
                    batch.lessons.pop();
                    break;
                }
            }
            if batch.lessons.is_empty() {
                return Err(state("单条交流经验超出批次预算"));
            }
            checkpoint.lesson_cursor = (start + batch.lessons.len()) % len;
        }
        let mut history = self.store.revisions(scope, checkpoint.history_cursor, 4)?;
        if history.is_empty() && checkpoint.history_cursor > checkpoint.history_from {
            checkpoint.history_cursor = checkpoint.history_from;
            history = self.store.revisions(scope, checkpoint.history_from, 4)?;
        }
        for revision in history {
            let sequence = revision.sequence;
            if batch.lessons.contains(&revision.lesson) {
                checkpoint.history_cursor = sequence;
                continue;
            }
            batch.lesson_history.push(revision);
            if conversation_prompt_json(batch)?.len() > MAX_CONVERSATION_PROMPT_BYTES - 8_192 {
                batch.lesson_history.pop();
                break;
            }
            checkpoint.history_cursor = sequence;
        }
        Ok(())
    }
    pub(crate) fn finish(
        &self,
        batch: &ConversationBatch,
        drafts: &[ConversationLessonDraft],
        now: u64,
    ) -> PluginResult<usize> {
        self.run(|| {
            let row = self
                .store
                .scope(&batch.scope)?
                .ok_or_else(|| rejected("交流批次已失效"))?;
            let mut checkpoint = Checkpoint::load(&row, self.store.as_ref())?;
            let claim = checkpoint
                .inflight
                .as_ref()
                .filter(|c| c.batch == *batch && !c.retryable)
                .ok_or_else(|| rejected("交流批次已失效或已记录失败"))?
                .clone();
            let mut revisions = vec![];
            let changed = apply_drafts(
                self.store.as_ref(),
                &mut checkpoint,
                batch,
                drafts,
                now,
                &mut revisions,
            )?;
            advance(&mut checkpoint, &claim, "completed", changed as u64)?;
            checkpoint.inflight = None;
            checkpoint.ready_at = now;
            checkpoint.pending_since_ms = None;
            self.save(
                &row,
                &checkpoint,
                revisions,
                Some(ConversationBatchArchive {
                    batch: batch.clone(),
                    outcome: "completed".into(),
                    recorded_at_ms: Some(now),
                }),
            )?;
            Ok(changed)
        })
    }
    pub(crate) fn fail(&self, batch: &ConversationBatch, now: u64) -> PluginResult<()> {
        self.run(|| {
            let Some(row) = self.store.scope(&batch.scope)? else {
                return Ok(());
            };
            let mut checkpoint = Checkpoint::load(&row, self.store.as_ref())?;
            let Some(claim) = checkpoint
                .inflight
                .as_mut()
                .filter(|c| c.batch == *batch && !c.retryable)
            else {
                return Ok(());
            };
            let final_attempt = batch.attempt >= 3;
            let claim = claim.clone();
            if final_attempt {
                advance(&mut checkpoint, &claim, "failed", 0)?;
                checkpoint.inflight = None;
                checkpoint.pending_since_ms = None;
            } else {
                checkpoint
                    .inflight
                    .as_mut()
                    .expect("checked claim")
                    .retryable = true;
            }
            checkpoint.ready_at =
                now.saturating_add(RETRY_MS.saturating_mul(u64::from(batch.attempt)));
            self.save(
                &row,
                &checkpoint,
                vec![],
                Some(ConversationBatchArchive {
                    batch: batch.clone(),
                    outcome: "failed".into(),
                    recorded_at_ms: Some(now),
                }),
            )
        })
    }
    pub(crate) fn pause(&self, scope: &ContextScope) -> PluginResult<()> {
        self.run(|| {
            let Some(row) = self.store.scope(scope)? else {
                return Ok(());
            };
            let mut checkpoint = Checkpoint::load(&row, self.store.as_ref())?;
            let archive = checkpoint.inflight.take().map(|claim| {
                if !claim.retryable {
                    advance(&mut checkpoint, &claim, "interrupted", 0).expect("validated progress");
                }
                ConversationBatchArchive {
                    batch: claim.batch,
                    outcome: "paused".into(),
                    recorded_at_ms: None,
                }
            });
            checkpoint.generation = checkpoint
                .generation
                .checked_add(1)
                .ok_or_else(|| state("交流代次耗尽"))?;
            checkpoint.ready_at = 0;
            self.save(&row, &checkpoint, vec![], archive)
        })
    }
    pub(crate) fn reset(&self, scope: &ContextScope) -> PluginResult<()> {
        self.run(|| {
            let Some(row) = self.store.scope(scope)? else {
                return Ok(());
            };
            let mut checkpoint = Checkpoint::load(&row, self.store.as_ref())?;
            let archive = checkpoint
                .inflight
                .take()
                .map(|claim| ConversationBatchArchive {
                    batch: claim.batch,
                    outcome: "reset".into(),
                    recorded_at_ms: None,
                });
            let mut last_revision = checkpoint.history_from;
            loop {
                let rows = self.store.revisions(scope, last_revision, 256)?;
                let Some(last) = rows.last() else {
                    break;
                };
                last_revision = last.sequence;
                if rows.len() < 256 {
                    break;
                }
            }
            let revisions: Vec<_> = checkpoint
                .lessons
                .drain(..)
                .map(|lesson| ConversationLessonRevision {
                    sequence: 0,
                    lesson,
                    reason: "reset".into(),
                    batch_id: None,
                    recorded_at_ms: None,
                })
                .collect();
            checkpoint.sweep = None;
            checkpoint.cursor = Cursor::at(row.next_sequence);
            checkpoint.context_from = row.next_sequence;
            checkpoint.generation = checkpoint
                .generation
                .checked_add(1)
                .ok_or_else(|| state("交流代次耗尽"))?;
            checkpoint.pending_since_ms = None;
            checkpoint.ready_at = 0;
            checkpoint.lesson_cursor = 0;
            checkpoint.history_from = last_revision
                .checked_add(revisions.len() as u64)
                .ok_or_else(|| state("交流历史版本序号耗尽"))?;
            checkpoint.history_cursor = checkpoint.history_from;
            self.save(&row, &checkpoint, revisions, archive)
        })
    }
}

fn advance(
    checkpoint: &mut Checkpoint,
    claim: &Claim,
    outcome: &str,
    updated: u64,
) -> PluginResult<()> {
    if claim.sweep {
        let sweep = checkpoint
            .sweep
            .as_mut()
            .ok_or_else(|| state("交流重访完成缺少请求"))?;
        let fragment_outcome = match outcome {
            "completed" => SweepOutcome::Succeeded,
            "failed" => SweepOutcome::Failed,
            "interrupted" => SweepOutcome::Unknown,
            _ => return Err(state("交流重访结局无效")),
        };
        let accounting = sweep.accounting.as_mut().expect("initialized accounting");
        let mut message_outcome = fragment_outcome.combine(accounting.partial.take());
        for index in sweep.cursor.sequence..claim.after.sequence {
            match message_outcome {
                SweepOutcome::Succeeded => sweep.processed += 1,
                SweepOutcome::Failed => accounting.failed += 1,
                SweepOutcome::Unknown => accounting.unknown += 1,
            }
            sweep.covered.push(sweep.sequences[index as usize - 1]);
            message_outcome = fragment_outcome;
        }
        if claim.after.text_offset != 0 || claim.after.context_offset != 0 {
            accounting.partial = Some(message_outcome);
        }
        if outcome == "failed" {
            sweep.failed_batches += 1;
        }
        if outcome == "interrupted" {
            sweep.interrupted_batches += 1;
        }
        sweep.updated += updated;
        sweep.cursor = claim.after.clone();
        if !sweep.pending() {
            let covered: BTreeSet<_> = sweep.covered.iter().copied().collect();
            while covered.contains(&checkpoint.cursor.sequence) {
                checkpoint.cursor = Cursor::at(checkpoint.cursor.sequence + 1);
            }
        }
    } else {
        checkpoint.cursor = claim.after.clone();
    }
    Ok(())
}

fn context_text(message: &ConversationMessage) -> &str {
    message
        .metadata
        .as_ref()
        .and_then(|m| m.context.as_deref())
        .unwrap_or("")
}
fn fits(batch: &ConversationBatch) -> PluginResult<bool> {
    Ok(conversation_prompt_json(batch)?.len() <= MAX_CONVERSATION_PROMPT_BYTES)
}
fn project(
    event: &ConversationEvent,
    cursor: &Cursor,
    end: usize,
) -> PluginResult<ConversationMessage> {
    let source = &event.message;
    let context = context_text(source);
    let body_len = source.text.len();
    let body_end = end.min(body_len);
    let context_end = end.saturating_sub(body_len).min(context.len());
    if cursor.sequence != event.sequence
        || cursor.text_offset > body_end
        || cursor.context_offset > context_end
        || !source.text.is_char_boundary(cursor.text_offset)
        || !source.text.is_char_boundary(body_end)
        || !context.is_char_boundary(cursor.context_offset)
        || !context.is_char_boundary(context_end)
    {
        return Err(state("交流分片消费游标无效"));
    }
    let mut message = source.clone();
    message.text = source.text[cursor.text_offset..body_end].to_owned();
    let metadata = message.metadata.get_or_insert_with(Default::default);
    metadata.sequence = Some(event.sequence);
    metadata.context = source
        .metadata
        .as_ref()
        .and_then(|m| m.context.as_ref())
        .map(|_| context[cursor.context_offset..context_end].to_owned());
    if cursor.text_offset != 0
        || cursor.context_offset != 0
        || body_end != body_len
        || context_end != context.len()
    {
        metadata.fragment = Some(ConversationFragment {
            text_start: cursor.text_offset,
            text_end: body_end,
            text_bytes: body_len,
            context_start: cursor.context_offset,
            context_end,
            context_bytes: context.len(),
        });
    }
    Ok(message)
}
fn fitting_fragment(
    event: &ConversationEvent,
    cursor: &Cursor,
    batch: &mut ConversationBatch,
) -> PluginResult<(ConversationMessage, Cursor)> {
    let text = &event.message.text;
    let context = context_text(&event.message);
    let start = cursor.text_offset + cursor.context_offset;
    let total = text.len() + context.len();
    let boundaries: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain(context.char_indices().map(|(i, _)| text.len() + i))
        .chain(std::iter::once(total))
        .filter(|i| *i > start)
        .collect();
    let mut lower = 0;
    let mut upper = boundaries.len();
    let mut best = None;
    while lower < upper {
        let middle = lower + (upper - lower) / 2;
        let end = boundaries[middle];
        let message = project(event, cursor, end)?;
        batch.messages.push(message.clone());
        let allowed = fits(batch)?;
        batch.messages.pop();
        if allowed {
            best = Some((message, end));
            lower = middle + 1;
        } else {
            upper = middle;
        }
    }
    let (message, end) =
        best.ok_or_else(|| state("交流批次连一个完整字符都无法容纳；原事件未消费"))?;
    let after = if end == total {
        Cursor::at(event.sequence + 1)
    } else {
        Cursor {
            sequence: event.sequence,
            text_offset: end.min(text.len()),
            context_offset: end.saturating_sub(text.len()),
        }
    };
    Ok((message, after))
}

fn archive(
    lesson: ConversationLesson,
    reason: &str,
    batch: &ConversationBatch,
    now: u64,
) -> ConversationLessonRevision {
    ConversationLessonRevision {
        sequence: 0,
        lesson,
        reason: reason.into(),
        batch_id: Some(batch.id.clone()),
        recorded_at_ms: Some(now),
    }
}
fn apply_drafts(
    store: &dyn ConversationStore,
    checkpoint: &mut Checkpoint,
    batch: &ConversationBatch,
    drafts: &[ConversationLessonDraft],
    now: u64,
    revisions: &mut Vec<ConversationLessonRevision>,
) -> PluginResult<usize> {
    if drafts.len() > MAX_CONVERSATION_DRAFTS {
        return Err(rejected("交流学习候选超限"));
    }
    let mut updated = BTreeSet::new();
    let mut keys = BTreeSet::new();
    let mut changed = 0;
    for draft in drafts {
        if !keys.insert((&draft.situation, &draft.response, &draft.avoid))
            || !super::text(&draft.situation, 240)
            || !super::text(&draft.response, 480)
            || draft.avoid.len() > 480
            || !(2..=4).contains(&draft.evidence.len())
        {
            return Err(rejected("交流经验格式无效或同批重复"));
        }
        let mut indices = BTreeSet::new();
        let mut source_ids = BTreeSet::new();
        for evidence in &draft.evidence {
            if !super::text(&evidence.quote, 240) || !source_ids.insert(&evidence.message_id) {
                return Err(rejected("交流依据为空、超限或重复原消息"));
            }
            let Some(index) = batch
                .messages
                .iter()
                .position(|m| m.id == evidence.message_id && m.text.contains(&evidence.quote))
            else {
                return Err(rejected(
                    "交流依据不是本批真实正文；引用附件材料不能冒充发言",
                ));
            };
            indices.insert(index);
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
                .is_some_and(|j| {
                    indices.contains(&j)
                        && !batch.messages[j]
                            .metadata
                            .as_ref()
                            .is_some_and(|m| m.ordering_uncertain)
                        && !batch.messages[*i]
                            .metadata
                            .as_ref()
                            .is_some_and(|m| m.ordering_uncertain)
                        && (!checkpoint.inflight.as_ref().is_some_and(|c| c.sweep)
                            || conversation_feedback_order_known(
                                &batch.messages[j],
                                &batch.messages[*i],
                            ))
                })
        });
        if humans.is_empty()
            || (draft.assessment == ConversationAssessment::Observation && humans.len() < 2)
            || (draft.assessment != ConversationAssessment::Observation && !feedback)
        {
            return Err(rejected("交流评价缺少不同真人原消息或前后反馈证据"));
        }
        let mut restored = None;
        let existing = if let Some(id) = &draft.lesson_id {
            if !batch.lessons.iter().any(|l| &l.id == id)
                && !batch.lesson_history.iter().any(|r| &r.lesson.id == id)
            {
                return Err(rejected("交流经验更新越过固定批次"));
            }
            if !updated.insert(id.clone()) {
                return Err(rejected("同批不能重复更新一条经验"));
            }
            if let Some(index) = checkpoint.lessons.iter().position(|l| &l.id == id) {
                Some(index)
            } else {
                let revision = store
                    .latest_revision(&batch.scope, id)?
                    .ok_or_else(|| rejected("交流经验来源已失效"))?;
                if revision.reason == "reset" {
                    return Err(rejected("重置前的经验编号不复活；新证据只能建立新经验"));
                }
                restored = Some(revision.lesson);
                None
            }
        } else {
            if draft.assessment == ConversationAssessment::Counterexample {
                return Err(rejected("反例必须指向已有经验"));
            }
            checkpoint.lessons.iter().position(|l| {
                l.situation == draft.situation
                    && l.response == draft.response
                    && l.avoid == draft.avoid
            })
        };
        let mut lesson = existing
            .map(|i| checkpoint.lessons[i].clone())
            .or(restored)
            .unwrap_or(ConversationLesson {
                id: String::new(),
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
        let revisiting = checkpoint.inflight.as_ref().is_some_and(|c| c.sweep);
        let mut evaluated: BTreeSet<String> =
            lesson.evaluated_message_ids.iter().cloned().collect();
        if !lesson.id.is_empty() {
            let mut after = checkpoint.history_from;
            loop {
                let history = store.revisions(&batch.scope, after, 256)?;
                for revision in &history {
                    if revision.lesson.id == lesson.id {
                        evaluated.extend(revision.lesson.evaluated_message_ids.iter().cloned());
                    }
                }
                let Some(last) = history.last() else {
                    break;
                };
                after = last.sequence;
            }
        }
        let previously_evaluated = humans
            .iter()
            .all(|i| evaluated.contains(&batch.messages[*i].id));
        if previously_evaluated && !revisiting {
            continue;
        }
        if previously_evaluated
            && (lesson.situation == draft.situation
                && lesson.response == draft.response
                && lesson.avoid == draft.avoid)
        {
            continue;
        }
        if lesson.id.is_empty() {
            lesson.id = checkpoint.next_name("lesson")?;
        }
        if !lesson.evidence.is_empty() {
            revisions.push(archive(lesson.clone(), "before_update", batch, now));
        }
        if !previously_evaluated {
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
        }
        // 新独立观察也能修正原则；只有直接纠正可以解除既存反例。
        if draft.assessment != ConversationAssessment::Counterexample || previously_evaluated {
            lesson.situation = draft.situation.clone();
            lesson.response = draft.response.clone();
            lesson.avoid = draft.avoid.clone();
        }
        lesson.active =
            lesson.counterexamples == 0 && (lesson.observations >= 2 || lesson.corrections > 0);
        lesson.updated_at_ms = now;
        lesson.evidence = draft.evidence.clone();
        for index in humans {
            let id = &batch.messages[index].id;
            if !lesson.evaluated_message_ids.contains(id) {
                lesson.evaluated_message_ids.push(id.clone());
            }
        }
        if lesson.evaluated_message_ids.len() > 64 {
            lesson
                .evaluated_message_ids
                .drain(..lesson.evaluated_message_ids.len() - 64);
        }
        revisions.push(archive(lesson.clone(), "updated", batch, now));
        if let Some(index) = existing {
            checkpoint.lessons[index] = lesson;
        } else {
            if checkpoint.lessons.len() >= MAX_CONVERSATION_LESSONS {
                let index = checkpoint
                    .lessons
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| !updated.contains(&l.id))
                    .min_by_key(|(_, l)| (l.active, l.updated_at_ms))
                    .map(|(i, _)| i)
                    .ok_or_else(|| rejected("当前批次没有可轮换的经验位置"))?;
                revisions.push(archive(
                    checkpoint.lessons.remove(index),
                    "capacity",
                    batch,
                    now,
                ));
            }
            checkpoint.lessons.push(lesson);
        }
        changed += 1;
    }
    Ok(changed)
}

fn legacy_import(legacy: &Conversations) -> PluginResult<Vec<ConversationLegacyScope>> {
    let mut result = vec![];
    for scope in &legacy.scopes {
        let mut events: Vec<ConversationEvent> = vec![];
        let sources = scope
            .inflight
            .iter()
            .flat_map(|b| b.messages.iter())
            .chain(scope.messages.iter());
        for message in sources {
            if let Some(previous) = events.iter().find(|event| event.message.id == message.id) {
                if previous.message.speaker != message.speaker
                    || previous.message.role != message.role
                    || previous.message.text != message.text
                {
                    return Err(state("旧交流窗口与批次消息冲突；保留来源"));
                }
                continue;
            }
            let sequence = events.len() as u64 + 1;
            let mut message = message.clone();
            message
                .metadata
                .get_or_insert_with(Default::default)
                .sequence = Some(sequence);
            events.push(ConversationEvent { sequence, message });
        }
        let next = events.len() as u64 + 1;
        let pending_start = scope.messages.len().saturating_sub(scope.pending);
        let pending_sequence = scope
            .messages
            .get(pending_start)
            .and_then(|m| events.iter().find(|e| e.message.id == m.id))
            .map_or(next, |e| e.sequence);
        let mut checkpoint = Checkpoint {
            generation: scope.generation,
            next_id: legacy.next,
            cursor: Cursor::at(pending_sequence),
            ready_at: scope.ready_at,
            lessons: scope.lessons.clone(),
            ..Default::default()
        };
        let mut batches = vec![];
        if let Some(batch) = &scope.inflight {
            let mut batch = batch.clone();
            for message in &mut batch.messages {
                if let Some(event) = events.iter().find(|event| event.message.id == message.id) {
                    *message = event.message.clone();
                }
            }
            let after = batch
                .messages
                .last()
                .and_then(|m| m.metadata.as_ref())
                .and_then(|m| m.sequence)
                .map_or(next, |n| n + 1);
            checkpoint.cursor = Cursor::at(after);
            checkpoint.ready_at = 0;
            batches.push(ConversationBatchArchive {
                batch,
                outcome: "interrupted".into(),
                recorded_at_ms: None,
            });
        }
        let mut revisions = scope.history.clone();
        revisions.extend(
            scope
                .lessons
                .iter()
                .cloned()
                .map(|lesson| ConversationLessonRevision {
                    sequence: 0,
                    lesson,
                    reason: "legacy".into(),
                    batch_id: None,
                    recorded_at_ms: None,
                }),
        );
        result.push(ConversationLegacyScope {
            scope: scope.scope.clone(),
            events,
            seen: scope
                .seen
                .iter()
                .map(|s| ConversationSeen {
                    id: s.id.clone(),
                    body: s.body.clone(),
                })
                .collect(),
            checkpoint: checkpoint.bytes()?,
            revisions,
            batches,
        });
    }
    Ok(result)
}
