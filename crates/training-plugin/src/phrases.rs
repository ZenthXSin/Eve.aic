//! 原话先移入持久批次，明确暂态失败有限补做，未知结局不重放。
use super::{error, valid};
use eve_llm_api::ContextScope;
use eve_plugin_api::PluginResult;
use eve_training_api::{
    LearnedPhrase, MAX_PHRASE_ATTEMPTS, MAX_PHRASE_DETAIL_CHARS, MAX_PHRASE_DRAFTS,
    MAX_PHRASE_TEXT_CHARS, MAX_PHRASES, PHRASE_BATCH, PHRASE_LEARNER_VERSION, PhraseBatch,
    PhraseDraft, PhraseFailure, PhraseFailureOutcome, PhraseFinish, PhraseKind,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const KEY: &str = "phrases.v1";
const MAX_BYTES: usize = 2 * 1_048_576;
const MAX_SCOPES: usize = 256;
// 旧实现取出批次后暂时释放范围名额，迟到结果可能形成两倍范围；保留可验证旧记录，
// 新增仍受 MAX_SCOPES 和总字节上限限制，不清空既有学习。
const MAX_RESTORED_SCOPES: usize = MAX_SCOPES * 2;
/// 每个作用域至多缓冲两批；更早的未学原话被新原话替换。
const MAX_PENDING: usize = PHRASE_BATCH * 2;
const MAX_PENDING_CHARS: usize = 512;
const RETRY_MS: u64 = 60_000;
/// 各阶段占固定容量额度，避免新 pending 挤掉 Running 转 RetryWait 的空间。
const PHASE_BYTES: usize = 128;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    scope: ContextScope,
    text: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    scope: ContextScope,
    phrase: LearnedPhrase,
}
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum Phase {
    Running {},
    RetryWait {
        failure: PhraseFailure,
        ready_at_ms: u64,
    },
    Unknown {},
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Inflight {
    batch: PhraseBatch,
    phase: Phase,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyPhrases {
    version: u8,
    next: u64,
    pending: Vec<Pending>,
    learned: Vec<Stored>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Phrases {
    version: u8,
    next: u64,
    pending: Vec<Pending>,
    learned: Vec<Stored>,
    inflight: Vec<Inflight>,
}

fn key(scope: &ContextScope) -> (&str, &str) {
    (&scope.session_id, &scope.user_id)
}

fn line(value: &str, max: usize) -> bool {
    let count = value.chars().count();
    (1..=max).contains(&count) && value.trim() == value && !value.chars().any(char::is_control)
}

impl Phrases {
    pub(crate) fn load(bytes: Option<Vec<u8>>) -> PluginResult<Self> {
        let Some(bytes) = bytes else {
            return Ok(Self {
                version: 2,
                next: 1,
                pending: vec![],
                learned: vec![],
                inflight: vec![],
            });
        };
        let corrupt = || error("说法学习状态损坏或版本不兼容；未清空");
        if bytes.len() > MAX_BYTES {
            return Err(corrupt());
        }
        #[derive(Deserialize)]
        struct Version {
            version: u8,
        }
        let version: Version = serde_json::from_slice(&bytes).map_err(|_| corrupt())?;
        let mut value: Self = match version.version {
            1 => {
                let legacy: LegacyPhrases =
                    serde_json::from_slice(&bytes).map_err(|_| corrupt())?;
                if legacy.version != 1 {
                    return Err(corrupt());
                }
                Self {
                    version: 2,
                    next: legacy.next,
                    pending: legacy.pending,
                    learned: legacy.learned,
                    inflight: vec![],
                }
            }
            2 => serde_json::from_slice(&bytes).map_err(|_| corrupt())?,
            _ => return Err(corrupt()),
        };
        let mut scopes = BTreeSet::new();
        let mut pending: BTreeMap<_, usize> = BTreeMap::new();
        let mut learned: BTreeMap<_, usize> = BTreeMap::new();
        let mut ids = BTreeSet::new();
        let mut keys = BTreeSet::new();
        let bad_pending = value.pending.iter().any(|p| {
            scopes.insert(key(&p.scope));
            let count = pending.entry(key(&p.scope)).or_default();
            *count += 1;
            !valid(&p.scope)
                || *count > MAX_PENDING
                || p.text.trim().is_empty()
                || p.text.chars().count() > MAX_PENDING_CHARS
        });
        let bad_learned = value.learned.iter().any(|s| {
            scopes.insert(key(&s.scope));
            let count = learned.entry(key(&s.scope)).or_default();
            *count += 1;
            let p = &s.phrase;
            !valid(&s.scope)
                || *count > MAX_PHRASES
                || !valid_draft(&PhraseDraft {
                    kind: p.kind,
                    text: p.text.clone(),
                    meaning: p.meaning.clone(),
                    example: p.example.clone(),
                })
                || p.uses == 0
                || p.id
                    .strip_prefix('p')
                    .and_then(|n| n.parse::<u64>().ok())
                    .is_none_or(|n| n == 0 || n >= value.next || p.id != format!("p{n}"))
                || !ids.insert(&p.id)
                || !keys.insert((key(&s.scope), p.kind, &p.text))
        });
        let mut active_scopes = BTreeSet::new();
        let bad_inflight = value.inflight.iter().any(|entry| {
            let batch = &entry.batch;
            scopes.insert(key(&batch.scope));
            !valid(&batch.scope)
                || !active_scopes.insert(key(&batch.scope))
                || batch.messages.len() != PHRASE_BATCH
                || batch
                    .messages
                    .iter()
                    .any(|m| m.trim().is_empty() || m.chars().count() > MAX_PENDING_CHARS)
                || !(1..=MAX_PHRASE_ATTEMPTS).contains(&batch.attempt)
                || batch.learner_version != PHRASE_LEARNER_VERSION
                || batch
                    .id
                    .strip_prefix('b')
                    .and_then(|n| n.parse::<u64>().ok())
                    .is_none_or(|n| n == 0 || n >= value.next || batch.id != format!("b{n}"))
                || !ids.insert(&batch.id)
                || matches!(entry.phase, Phase::RetryWait { failure, .. }
                    if !failure.retryable() || batch.attempt >= MAX_PHRASE_ATTEMPTS)
        });
        if value.version != 2
            || value.next == 0
            || bad_pending
            || bad_learned
            || bad_inflight
            || scopes.len() > MAX_RESTORED_SCOPES
        {
            return Err(corrupt());
        }
        // v2 写入已为每个阶段预留容量，恢复时必须校验同一不变量。
        // 满容量 v1 增加 inflight 字段后可能暂时放不下；仍允许只读恢复旧数据。
        if version.version == 2 {
            value.bytes().map_err(|_| corrupt())?;
        }
        // 重开后的 Running 可能已收到结果或提交失败，不猜成 HTTP 暂态。
        for entry in &mut value.inflight {
            if matches!(entry.phase, Phase::Running {}) {
                entry.phase = Phase::Unknown {};
            }
        }
        Ok(value)
    }
    pub(crate) fn bytes(&self) -> PluginResult<Vec<u8>> {
        let bytes = serde_json::to_vec(self).map_err(|_| error("说法学习编码失败"))?;
        let reserve = self.inflight.iter().try_fold(0usize, |reserved, entry| {
            let used = serde_json::to_vec(&entry.phase)
                .map_err(|_| error("说法学习编码失败"))?
                .len();
            Ok::<_, eve_plugin_api::PluginError>(
                reserved.saturating_add(PHASE_BYTES.saturating_sub(used)),
            )
        })?;
        if bytes.len().saturating_add(reserve) > MAX_BYTES {
            return Err(error("说法学习容量已满；保留状态"));
        }
        Ok(bytes)
    }
    fn scopes(&self) -> BTreeSet<(&str, &str)> {
        self.pending
            .iter()
            .map(|p| key(&p.scope))
            .chain(self.learned.iter().map(|s| key(&s.scope)))
            .chain(self.inflight.iter().map(|entry| key(&entry.batch.scope)))
            .collect()
    }
    /// 缓冲一条已通过表达统计筛选的用户原话。
    pub(crate) fn buffer(&mut self, scope: &ContextScope, text: &str) -> PluginResult<()> {
        if text.chars().count() > MAX_PENDING_CHARS || text.trim().is_empty() {
            return Ok(());
        }
        let scopes = self.scopes();
        if !scopes.contains(&key(scope)) && scopes.len() >= MAX_SCOPES {
            return Err(error("说法学习容量已满；保留状态"));
        }
        let mine: Vec<usize> = (0..self.pending.len())
            .filter(|i| self.pending[*i].scope == *scope)
            .collect();
        if mine.len() >= MAX_PENDING {
            self.pending.remove(mine[0]);
        }
        self.pending.push(Pending {
            scope: scope.clone(),
            text: text.into(),
        });
        Ok(())
    }
    pub(crate) fn ready(&self, now: u64, enabled: impl Fn(&ContextScope) -> bool) -> bool {
        self.inflight.iter().any(|entry| {
            enabled(&entry.batch.scope)
                && matches!(entry.phase, Phase::RetryWait { ready_at_ms, .. } if now >= ready_at_ms)
        }) || self.pending_scope(enabled).is_some()
    }
    fn pending_scope(&self, enabled: impl Fn(&ContextScope) -> bool) -> Option<ContextScope> {
        let mut counts: BTreeMap<(&str, &str), usize> = BTreeMap::new();
        self.pending.iter().find_map(|p| {
            let count = counts.entry(key(&p.scope)).or_default();
            *count += 1;
            (*count >= PHRASE_BATCH
                && enabled(&p.scope)
                && !self
                    .inflight
                    .iter()
                    .any(|entry| entry.batch.scope == p.scope))
            .then(|| p.scope.clone())
        })
    }
    /// 只领取新批或已经确认暂态失败的同批下一次尝试。
    pub(crate) fn take(
        &mut self,
        now: u64,
        enabled: impl Fn(&ContextScope) -> bool,
    ) -> PluginResult<Option<PhraseBatch>> {
        if let Some(entry) = self.inflight.iter_mut().find(|entry| {
            enabled(&entry.batch.scope)
                && matches!(entry.phase, Phase::RetryWait { ready_at_ms, .. } if now >= ready_at_ms)
        }) {
            entry.batch.attempt += 1;
            entry.phase = Phase::Running {};
            return Ok(Some(entry.batch.clone()));
        }
        let Some(scope) = self.pending_scope(enabled) else {
            return Ok(None);
        };
        let id = format!("b{}", self.next);
        let next = self
            .next
            .checked_add(1)
            .ok_or_else(|| error("说法学习序号耗尽"))?;
        let mut messages = Vec::new();
        self.pending.retain(|p| {
            if p.scope == scope && messages.len() < PHRASE_BATCH {
                messages.push(p.text.clone());
                false
            } else {
                true
            }
        });
        self.next = next;
        let batch = PhraseBatch {
            id,
            scope,
            messages,
            attempt: 1,
            learner_version: PHRASE_LEARNER_VERSION,
        };
        self.inflight.push(Inflight {
            batch: batch.clone(),
            phase: Phase::Running {},
        });
        Ok(Some(batch))
    }
    fn active(&self, batch: &PhraseBatch) -> Option<usize> {
        self.inflight
            .iter()
            .position(|entry| entry.batch == *batch && matches!(entry.phase, Phase::Running {}))
    }
    pub(crate) fn fail(
        &mut self,
        batch: &PhraseBatch,
        failure: PhraseFailure,
        now: u64,
    ) -> PhraseFailureOutcome {
        let Some(index) = self.active(batch) else {
            return PhraseFailureOutcome::Stale;
        };
        if failure.retryable() && batch.attempt < MAX_PHRASE_ATTEMPTS {
            let ready_at_ms = now.saturating_add(RETRY_MS.saturating_mul(u64::from(batch.attempt)));
            self.inflight[index].phase = Phase::RetryWait {
                failure,
                ready_at_ms,
            };
            PhraseFailureOutcome::Retry { ready_at_ms }
        } else {
            self.inflight.remove(index);
            if failure.retryable() {
                PhraseFailureOutcome::Exhausted
            } else {
                PhraseFailureOutcome::Finished
            }
        }
    }
    pub(crate) fn finish(
        &mut self,
        batch: &PhraseBatch,
        drafts: &[PhraseDraft],
        now_ms: u64,
    ) -> PluginResult<PhraseFinish> {
        let Some(index) = self.active(batch) else {
            return Ok(PhraseFinish::Stale);
        };
        if drafts_valid(batch, drafts) {
            // 合并和容量检查都未触及存储。明确拒绝时不留下部分 uses、序号或候选，
            // 只结束这批；实际提交终态的后端错误仍由调用方封住旧快照。
            let mut accepted = self.clone();
            if let Ok((saved, skipped)) = accepted.merge(batch, drafts, now_ms) {
                accepted.inflight.remove(index);
                if accepted.bytes().is_ok() {
                    *self = accepted;
                    return Ok(if skipped == 0 {
                        PhraseFinish::Saved(saved)
                    } else {
                        PhraseFinish::CapacityLimited { saved, skipped }
                    });
                }
                // 总字节不足时仍完整回退本批候选，不把未提交的更新计为保存。
                self.inflight.remove(index);
                return Ok(PhraseFinish::CapacityLimited {
                    saved: 0,
                    skipped: drafts.len(),
                });
            }
        }
        self.inflight.remove(index);
        Ok(PhraseFinish::Rejected)
    }
    pub(crate) fn cancel_scope(&mut self, scope: &ContextScope) -> bool {
        let before = self.inflight.len();
        self.inflight.retain(|entry| entry.batch.scope != *scope);
        self.inflight.len() != before
    }
    /// 核对候选并合并；返回新增/更新条数与因本范围容量跳过的新说法数。
    fn merge(
        &mut self,
        batch: &PhraseBatch,
        drafts: &[PhraseDraft],
        now_ms: u64,
    ) -> PluginResult<(usize, usize)> {
        if !drafts_valid(batch, drafts) {
            return Err(error("说法学习结果不合格；本批不保存"));
        }
        let scopes = self.scopes();
        if !drafts.is_empty() && !scopes.contains(&key(&batch.scope)) && scopes.len() >= MAX_SCOPES
        {
            return Err(error("说法学习范围容量已满；保留旧状态，不接纳迟到新范围"));
        }
        let mut changed = 0;
        let mut skipped = 0;
        for draft in drafts {
            if let Some(stored) = self.learned.iter_mut().find(|s| {
                s.scope == batch.scope && s.phrase.kind == draft.kind && s.phrase.text == draft.text
            }) {
                let phrase = &mut stored.phrase;
                phrase.uses = phrase.uses.saturating_add(1);
                phrase.meaning.clone_from(&draft.meaning);
                phrase.example.clone_from(&draft.example);
                phrase.updated_at_ms = now_ms;
                changed += 1;
                continue;
            }
            if self
                .learned
                .iter()
                .filter(|s| s.scope == batch.scope)
                .count()
                >= MAX_PHRASES
            {
                skipped += 1;
                continue;
            }
            let id = format!("p{}", self.next);
            self.next = self
                .next
                .checked_add(1)
                .ok_or_else(|| error("说法学习序号耗尽"))?;
            self.learned.push(Stored {
                scope: batch.scope.clone(),
                phrase: LearnedPhrase {
                    id,
                    kind: draft.kind,
                    text: draft.text.clone(),
                    meaning: draft.meaning.clone(),
                    example: draft.example.clone(),
                    uses: 1,
                    updated_at_ms: now_ms,
                },
            });
            changed += 1;
        }
        Ok((changed, skipped))
    }
    pub(crate) fn list(&self, scope: &ContextScope) -> Vec<LearnedPhrase> {
        let mut list: Vec<_> = self
            .learned
            .iter()
            .filter(|s| s.scope == *scope)
            .map(|s| s.phrase.clone())
            .collect();
        list.sort_by(|a, b| {
            b.uses
                .cmp(&a.uses)
                .then(b.updated_at_ms.cmp(&a.updated_at_ms))
        });
        list
    }
    pub(crate) fn forget(&mut self, scope: &ContextScope, id: &str) -> bool {
        let before = self.learned.len();
        self.learned
            .retain(|s| !(s.scope == *scope && s.phrase.id == id));
        let changed = before != self.learned.len();
        if changed {
            self.cancel_scope(scope);
        }
        changed
    }
    /// 重置作用域：删除待学原话与已学说法。
    pub(crate) fn reset(&mut self, scope: &ContextScope) -> bool {
        let before = (self.pending.len(), self.learned.len(), self.inflight.len());
        self.pending.retain(|p| p.scope != *scope);
        self.learned.retain(|s| s.scope != *scope);
        self.cancel_scope(scope);
        before != (self.pending.len(), self.learned.len(), self.inflight.len())
    }
}

fn drafts_valid(batch: &PhraseBatch, drafts: &[PhraseDraft]) -> bool {
    let mut seen = BTreeSet::new();
    drafts.len() <= MAX_PHRASE_DRAFTS
        && drafts.iter().all(|draft| {
            valid_draft(draft)
                && seen.insert((draft.kind, draft.text.as_str()))
                && batch.messages.iter().any(|m| m.contains(&draft.example))
                && (draft.kind != PhraseKind::Term
                    || batch.messages.iter().any(|m| m.contains(&draft.text)))
        })
}

pub(crate) fn valid_draft(draft: &PhraseDraft) -> bool {
    line(&draft.text, MAX_PHRASE_TEXT_CHARS)
        && line(&draft.meaning, MAX_PHRASE_DETAIL_CHARS)
        && line(&draft.example, MAX_PHRASE_DETAIL_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(user: &str) -> ContextScope {
        ContextScope {
            session_id: "s".into(),
            user_id: user.into(),
        }
    }
    fn draft(kind: PhraseKind, text: &str, example: &str) -> PhraseDraft {
        PhraseDraft {
            kind,
            text: text.into(),
            meaning: "意思".into(),
            example: example.into(),
        }
    }
    fn capacity_text(bytes: usize) -> String {
        let mut text = "原".repeat(bytes / 3);
        match bytes % 3 {
            1 => text.push('x'),
            2 => text.push('é'),
            _ => {}
        }
        assert_eq!(text.len(), bytes);
        assert!(text.chars().count() <= MAX_PENDING_CHARS);
        text
    }
    fn largest_buffer_that_fits(phrases: &Phrases, scope: &ContextScope) -> Option<Phrases> {
        let (mut low, mut high) = (1, MAX_PENDING_CHARS * 3);
        let mut best = None;
        while low <= high {
            let middle = low + (high - low) / 2;
            let mut next = phrases.clone();
            next.buffer(scope, &capacity_text(middle)).unwrap();
            if next.bytes().is_ok() {
                best = Some(next);
                low = middle + 1;
            } else {
                high = middle - 1;
            }
        }
        best
    }
    /// 经真实缓冲入口填满合法快照，不按阶段预留公式拼出测试数据。
    fn fill_pending_capacity(phrases: &mut Phrases) {
        let padding = scope("capacity-padding");
        for _ in 0..MAX_PENDING {
            phrases.buffer(&padding, "x").unwrap();
        }
        let message = capacity_text(MAX_PENDING_CHARS * 3);
        for index in 0..MAX_SCOPES {
            let filler = scope(&format!("capacity-{index}"));
            let mut block = phrases.clone();
            for _ in 0..MAX_PENDING {
                block.buffer(&filler, &message).unwrap();
            }
            if block.bytes().is_ok() {
                *phrases = block;
                continue;
            }
            for _ in 0..MAX_PENDING {
                let mut next = phrases.clone();
                next.buffer(&filler, &message).unwrap();
                if next.bytes().is_ok() {
                    *phrases = next;
                    continue;
                }
                if let Some(next) = largest_buffer_that_fits(phrases, &filler) {
                    *phrases = next;
                }
                // 已满 24 条的 padding 范围允许替换一条，将不足新记录开销的余量用满。
                *phrases = largest_buffer_that_fits(phrases, &padding).unwrap();
                assert!(phrases.scopes().len() <= MAX_SCOPES);
                Phrases::load(Some(phrases.bytes().unwrap())).unwrap();
                return;
            }
            unreachable!("整范围已验证放不下");
        }
        panic!("应先达到字节上限，而非范围上限");
    }

    #[test]
    fn batches_need_a_full_buffer_and_learned_phrases_need_verbatim_evidence() {
        let mut phrases = Phrases::load(None).unwrap();
        for i in 0..PHRASE_BATCH - 1 {
            phrases
                .buffer(&scope("a"), &format!("第{i}句 yyds"))
                .unwrap();
        }
        assert!(phrases.take(0, |_| true).unwrap().is_none());
        phrases.buffer(&scope("a"), "这波操作太秀了").unwrap();
        let batch = phrases.take(0, |_| true).unwrap().unwrap();
        assert_eq!(batch.messages.len(), PHRASE_BATCH);
        assert!(phrases.pending.is_empty());
        // 摘录不在原话中：整批拒绝。
        assert!(
            phrases
                .merge(&batch, &[draft(PhraseKind::Term, "yyds", "永远的神")], 1)
                .is_err()
        );
        // 词语本身不在原话中：拒绝。
        assert!(
            phrases
                .merge(&batch, &[draft(PhraseKind::Term, "绝绝子", "太秀了")], 1)
                .is_err()
        );
        assert_eq!(
            phrases
                .merge(
                    &batch,
                    &[
                        draft(PhraseKind::Term, "yyds", "第0句 yyds"),
                        draft(PhraseKind::Expression, "夸人时", "太秀了"),
                    ],
                    1
                )
                .unwrap(),
            (2, 0)
        );
        assert_eq!(
            phrases
                .merge(&batch, &[draft(PhraseKind::Term, "yyds", "第1句 yyds")], 2)
                .unwrap(),
            (1, 0)
        );
        let list = phrases.list(&scope("a"));
        assert_eq!(list[0].text, "yyds");
        assert_eq!(list[0].uses, 2);
        assert!(phrases.list(&scope("b")).is_empty());
        let bytes = phrases.bytes().unwrap();
        let mut loaded = Phrases::load(Some(bytes)).unwrap();
        assert!(loaded.forget(&scope("a"), &list[0].id));
        assert!(!loaded.forget(&scope("b"), &list[1].id));
        assert!(loaded.reset(&scope("a")));
        assert!(loaded.list(&scope("a")).is_empty());
    }

    #[test]
    fn inflight_keeps_scope_capacity_and_legacy_overflow_is_preserved() {
        let mut phrases = Phrases::load(None).unwrap();
        for _ in 0..PHRASE_BATCH {
            phrases.buffer(&scope("old"), "原话").unwrap();
        }
        let batch = phrases.take(0, |_| true).unwrap().unwrap();
        for i in 0..MAX_SCOPES - 1 {
            phrases.buffer(&scope(&i.to_string()), "原话").unwrap();
        }
        assert!(phrases.buffer(&scope("overflow"), "新增").is_err());
        assert_eq!(
            phrases
                .finish(&batch, &[draft(PhraseKind::Term, "原话", "原话")], 1)
                .unwrap(),
            PhraseFinish::Saved(1)
        );
        // 还原旧实现曾写出的第257个范围；恢复不删任何原话，新增仍拒绝。
        phrases.pending.push(Pending {
            scope: scope("legacy-overflow"),
            text: "保留".into(),
        });
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&phrases.bytes().unwrap()).unwrap();
        legacy["version"] = 1.into();
        legacy.as_object_mut().unwrap().remove("inflight");
        let legacy = serde_json::to_vec(&legacy).unwrap();
        let mut restored = Phrases::load(Some(legacy.clone())).unwrap();
        assert_eq!(restored.scopes(), phrases.scopes());
        assert!(restored.inflight.is_empty());
        assert!(restored.buffer(&scope("new"), "新增").is_err());
        restored
            .buffer(&scope("legacy-overflow"), "原范围继续")
            .unwrap();
        assert_eq!(restored.scopes().len(), MAX_SCOPES + 1);
        let mut corrupt: serde_json::Value = serde_json::from_slice(&legacy).unwrap();
        corrupt["pending"][0]["text"] = "".into();
        assert!(Phrases::load(Some(serde_json::to_vec(&corrupt).unwrap())).is_err());
    }

    #[test]
    fn corrupt_state_is_refused_and_pending_is_bounded() {
        assert!(Phrases::load(Some(b"{}".to_vec())).is_err());
        let mut phrases = Phrases::load(None).unwrap();
        for i in 0..MAX_PENDING + 3 {
            phrases.buffer(&scope("a"), &format!("{i}")).unwrap();
        }
        assert_eq!(phrases.pending.len(), MAX_PENDING);
        assert_eq!(phrases.pending[0].text, "3");
        let mut value: serde_json::Value =
            serde_json::from_slice(&phrases.bytes().unwrap()).unwrap();
        value["next"] = 0.into();
        assert!(Phrases::load(Some(serde_json::to_vec(&value).unwrap())).is_err());
    }

    #[test]
    fn invalid_persisted_batches_are_refused_without_coercion() {
        let mut phrases = Phrases::load(None).unwrap();
        for _ in 0..PHRASE_BATCH {
            phrases.buffer(&scope("one"), "原话").unwrap();
        }
        let batch = phrases.take(0, |_| true).unwrap().unwrap();
        phrases.fail(&batch, PhraseFailure::Timeout, 1);
        let valid: serde_json::Value = serde_json::from_slice(&phrases.bytes().unwrap()).unwrap();
        for case in 0..9 {
            let mut value = valid.clone();
            match case {
                0 => value["inflight"][0]["batch"]["attempt"] = 0.into(),
                1 => value["inflight"][0]["batch"]["attempt"] = 3.into(),
                2 => value["inflight"][0]["batch"]["learner_version"] = 2.into(),
                3 => value["inflight"][0]["batch"]["messages"] = serde_json::json!(["原话"]),
                4 => value["inflight"][0]["batch"]["scope"]["user_id"] = "".into(),
                5 => value["inflight"][0]["phase"]["failure"] = "cancelled".into(),
                6 => value["inflight"][0]["batch"]["extra"] = true.into(),
                7 => {
                    let duplicate = value["inflight"][0].clone();
                    value["inflight"].as_array_mut().unwrap().push(duplicate);
                }
                _ => value["next"] = 1.into(),
            }
            assert!(
                Phrases::load(Some(serde_json::to_vec(&value).unwrap())).is_err(),
                "case {case}"
            );
        }
    }

    #[test]
    fn sequence_exhaustion_preserves_pending() {
        let mut phrases = Phrases::load(None).unwrap();
        for _ in 0..PHRASE_BATCH {
            phrases.buffer(&scope("one"), "原话").unwrap();
        }
        phrases.next = u64::MAX;
        let before = phrases.bytes().unwrap();
        assert!(phrases.take(0, |_| true).is_err());
        assert_eq!(phrases.bytes().unwrap(), before);
    }

    #[test]
    fn fixed_batch_bytes_count_toward_the_shared_capacity() {
        let mut phrases = Phrases::load(None).unwrap();
        let message = capacity_text(MAX_PENDING_CHARS * 3);
        for _ in 0..PHRASE_BATCH {
            phrases.buffer(&scope("one"), &message).unwrap();
        }
        let batch = phrases.take(0, |_| true).unwrap().unwrap();
        fill_pending_capacity(&mut phrases);
        let mut overflow = phrases.clone();
        overflow.buffer(&scope("extra"), &message).unwrap();
        assert!(overflow.bytes().is_err());
        assert!(overflow.cancel_scope(&batch.scope));
        assert!(
            overflow.bytes().is_ok(),
            "仅去掉固定批次才回到原总字节上限内"
        );
        Phrases::load(Some(overflow.bytes().unwrap())).unwrap();
    }

    #[test]
    fn pending_cannot_consume_the_space_needed_to_schedule_retry() {
        let mut phrases = Phrases::load(None).unwrap();
        for _ in 0..PHRASE_BATCH {
            phrases.buffer(&scope("one"), "原话").unwrap();
        }
        let batch = phrases.take(0, |_| true).unwrap().unwrap();
        fill_pending_capacity(&mut phrases);
        let before: serde_json::Value = serde_json::from_slice(&phrases.bytes().unwrap()).unwrap();
        assert!(matches!(
            phrases.fail(&batch, PhraseFailure::Timeout, u64::MAX),
            PhraseFailureOutcome::Retry { .. }
        ));
        assert!(phrases.bytes().is_ok(), "RetryWait 变长已由领取阶段预留");
        let saved = phrases.bytes().unwrap();
        Phrases::load(Some(saved.clone())).unwrap();
        let after: serde_json::Value = serde_json::from_slice(&saved).unwrap();
        assert_eq!(after["pending"], before["pending"]);
        phrases.buffer(&scope("extra"), "x").unwrap();
        assert!(
            phrases.bytes().is_err(),
            "额外原话不能借走重试状态的预留空间"
        );
    }

    #[test]
    fn capacity_rejection_consumes_batch_without_partial_merge() {
        let mut phrases = Phrases::load(None).unwrap();
        let owner = scope("one");
        for _ in 0..PHRASE_BATCH {
            phrases.buffer(&owner, "原话").unwrap();
        }
        let previous = phrases.take(0, |_| true).unwrap().unwrap();
        let old = draft(PhraseKind::Term, "原话", "原话");
        assert_eq!(
            phrases
                .finish(&previous, std::slice::from_ref(&old), 1)
                .unwrap(),
            PhraseFinish::Saved(1)
        );
        for _ in 0..PHRASE_BATCH {
            phrases.buffer(&owner, "原话").unwrap();
        }
        let batch = phrases.take(2, |_| true).unwrap().unwrap();
        fill_pending_capacity(&mut phrases);
        let before: serde_json::Value = serde_json::from_slice(&phrases.bytes().unwrap()).unwrap();
        let mut drafts = vec![old];
        for index in 1..MAX_PHRASE_DRAFTS {
            drafts.push(PhraseDraft {
                kind: PhraseKind::Expression,
                text: format!("{index}{}", "学".repeat(MAX_PHRASE_TEXT_CHARS - 1)),
                meaning: "义".repeat(MAX_PHRASE_DETAIL_CHARS),
                example: "原话".into(),
            });
        }
        assert!(drafts_valid(&batch, &drafts));
        let mut expanded = phrases.clone();
        assert_eq!(
            expanded.merge(&batch, &drafts, 3).unwrap(),
            (MAX_PHRASE_DRAFTS, 0)
        );
        expanded.cancel_scope(&owner);
        assert!(expanded.bytes().is_err(), "合法候选超过释放 batch 后的容量");
        assert_eq!(
            phrases.finish(&batch, &drafts, 3).unwrap(),
            PhraseFinish::CapacityLimited {
                saved: 0,
                skipped: MAX_PHRASE_DRAFTS,
            }
        );
        let after: serde_json::Value = serde_json::from_slice(&phrases.bytes().unwrap()).unwrap();
        for field in ["pending", "learned", "next"] {
            assert_eq!(after[field], before[field], "不留下部分 {field}");
        }
        assert!(phrases.inflight.is_empty());
        let restored = Phrases::load(Some(phrases.bytes().unwrap())).unwrap();
        assert_eq!(restored.list(&owner)[0].uses, 1);
    }
}
