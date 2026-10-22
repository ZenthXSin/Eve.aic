//! 说法学习的持久状态：待学原话缓冲与已学说法。原话只在待学期间保存，取出成批即删除。
use super::{error, valid};
use eve_llm_api::ContextScope;
use eve_plugin_api::PluginResult;
use eve_training_api::{
    LearnedPhrase, MAX_PHRASE_DETAIL_CHARS, MAX_PHRASE_DRAFTS, MAX_PHRASE_TEXT_CHARS, MAX_PHRASES,
    PHRASE_BATCH, PhraseBatch, PhraseDraft, PhraseKind,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const KEY: &str = "phrases.v1";
const MAX_BYTES: usize = 2 * 1_048_576;
const MAX_SCOPES: usize = 256;
/// 每个作用域至多缓冲两批；更早的未学原话被新原话替换。
const MAX_PENDING: usize = PHRASE_BATCH * 2;
const MAX_PENDING_CHARS: usize = 512;

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
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Phrases {
    version: u8,
    next: u64,
    pending: Vec<Pending>,
    learned: Vec<Stored>,
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
                version: 1,
                next: 1,
                pending: vec![],
                learned: vec![],
            });
        };
        let corrupt = || error("说法学习状态损坏或版本不兼容；未清空");
        if bytes.len() > MAX_BYTES {
            return Err(corrupt());
        }
        let value: Self = serde_json::from_slice(&bytes).map_err(|_| corrupt())?;
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
        if value.version != 1
            || value.next == 0
            || bad_pending
            || bad_learned
            || scopes.len() > MAX_SCOPES
        {
            return Err(corrupt());
        }
        Ok(value)
    }
    pub(crate) fn bytes(&self) -> PluginResult<Vec<u8>> {
        let bytes = serde_json::to_vec(self).map_err(|_| error("说法学习编码失败"))?;
        if bytes.len() > MAX_BYTES {
            return Err(error("说法学习容量已满；保留状态"));
        }
        Ok(bytes)
    }
    fn scopes(&self) -> BTreeSet<(&str, &str)> {
        self.pending
            .iter()
            .map(|p| key(&p.scope))
            .chain(self.learned.iter().map(|s| key(&s.scope)))
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
    /// 取出最早攒够一批的作用域；`skip` 中的作用域本轮不取。
    pub(crate) fn take(&mut self, skip: &[ContextScope]) -> Option<PhraseBatch> {
        let mut counts: BTreeMap<(&str, &str), usize> = BTreeMap::new();
        let scope = self.pending.iter().find_map(|p| {
            let count = counts.entry(key(&p.scope)).or_default();
            *count += 1;
            (*count >= PHRASE_BATCH && !skip.contains(&p.scope)).then(|| p.scope.clone())
        })?;
        let mut messages = Vec::new();
        self.pending.retain(|p| {
            if p.scope == scope && messages.len() < PHRASE_BATCH {
                messages.push(p.text.clone());
                false
            } else {
                true
            }
        });
        let id = format!("b{}", self.next);
        self.next += 1;
        Some(PhraseBatch {
            id,
            scope,
            messages,
        })
    }
    /// 核对候选并合并；返回新增或更新的条数。
    pub(crate) fn merge(
        &mut self,
        batch: &PhraseBatch,
        drafts: &[PhraseDraft],
        now_ms: u64,
    ) -> PluginResult<usize> {
        let mut seen = BTreeSet::new();
        if drafts.len() > MAX_PHRASE_DRAFTS
            || drafts.iter().any(|d| {
                !valid_draft(d)
                    || !seen.insert((d.kind, d.text.as_str()))
                    || !batch.messages.iter().any(|m| m.contains(&d.example))
                    || (d.kind == PhraseKind::Term
                        && !batch.messages.iter().any(|m| m.contains(&d.text)))
            })
        {
            return Err(error("说法学习结果不合格；本批不保存"));
        }
        let mut changed = 0;
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
                continue;
            }
            let id = format!("p{}", self.next);
            self.next += 1;
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
        Ok(changed)
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
        before != self.learned.len()
    }
    /// 重置作用域：删除待学原话与已学说法。
    pub(crate) fn reset(&mut self, scope: &ContextScope) -> bool {
        let before = (self.pending.len(), self.learned.len());
        self.pending.retain(|p| p.scope != *scope);
        self.learned.retain(|s| s.scope != *scope);
        before != (self.pending.len(), self.learned.len())
    }
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

    #[test]
    fn batches_need_a_full_buffer_and_learned_phrases_need_verbatim_evidence() {
        let mut phrases = Phrases::load(None).unwrap();
        for i in 0..PHRASE_BATCH - 1 {
            phrases
                .buffer(&scope("a"), &format!("第{i}句 yyds"))
                .unwrap();
        }
        assert!(phrases.take(&[]).is_none());
        phrases.buffer(&scope("a"), "这波操作太秀了").unwrap();
        let batch = phrases.take(&[]).unwrap();
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
            2
        );
        assert_eq!(
            phrases
                .merge(&batch, &[draft(PhraseKind::Term, "yyds", "第1句 yyds")], 2)
                .unwrap(),
            1
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
}
