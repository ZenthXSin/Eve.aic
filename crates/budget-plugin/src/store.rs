//! 用量账本：当前窗口与最近窗口的计数，按用途细分。每次计数先持久化再允许请求。
use crate::strict_json;
use eve_budget_api::*;
use eve_plugin_api::{PluginContext, PluginError, PluginResult};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Mutex, MutexGuard},
};

const FORMAT_VERSION: u32 = 1;
const MAX_STATE_BYTES: usize = 64 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    format_version: u32,
    policy: BudgetPolicy,
    current: BudgetWindow,
    history: Vec<BudgetWindow>,
}
impl Ledger {
    /// 按配置的策略与当前时间滚动窗口。窗口长度改变或进入新窗口时，旧窗口进入历史；
    /// 时钟回拨到已记录窗口之前时沿用已记录的窗口，不重新计数。
    fn rolled(&self, policy: BudgetPolicy, now_ms: u64) -> Self {
        let mut next = self.clone();
        let start = policy.window_start(now_ms);
        let changed_length = next.policy.window_ms != policy.window_ms;
        next.policy = policy;
        if changed_length || start > next.current.start_ms {
            let finished = std::mem::replace(
                &mut next.current,
                BudgetWindow {
                    start_ms: start,
                    used: 0,
                    by_category: Default::default(),
                },
            );
            if finished.used > 0 {
                next.history.insert(0, finished);
                next.history.truncate(MAX_HISTORY);
            }
        }
        next
    }
    fn snapshot(&self) -> BudgetSnapshot {
        BudgetSnapshot {
            policy: self.policy,
            current: self.current.clone(),
            category_limits: BTreeMap::new(),
            resets_at_ms: self.current.start_ms.saturating_add(self.policy.window_ms),
            history: self.history.clone(),
        }
    }
}

struct Inner {
    ledger: Ledger,
    context: Option<PluginContext>,
}
pub(crate) struct StoredBudget {
    policy: BudgetPolicy,
    category_limits: BTreeMap<String, u32>,
    inner: Mutex<Inner>,
}

impl StoredBudget {
    pub(crate) fn open(
        context: PluginContext,
        policy: BudgetPolicy,
        category_limits: BTreeMap<String, u32>,
    ) -> BudgetResult<Self> {
        policy.validate()?;
        let ledger = match context
            .state_get(BUDGET_STATE_KEY)
            .map_err(|_| BudgetError::Storage)?
        {
            None => Ledger {
                format_version: FORMAT_VERSION,
                policy,
                current: BudgetWindow::default(),
                history: vec![],
            },
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(BudgetError::CorruptState);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| BudgetError::CorruptState)?;
                if value.get("format_version").and_then(|value| value.as_u64())
                    != Some(u64::from(FORMAT_VERSION))
                {
                    return Err(if value.get("format_version").is_some() {
                        BudgetError::UnsupportedVersion
                    } else {
                        BudgetError::CorruptState
                    });
                }
                let ledger: Ledger =
                    serde_json::from_value(value).map_err(|_| BudgetError::CorruptState)?;
                validate(&ledger)?;
                ledger
            }
        };
        Ok(Self {
            policy,
            category_limits,
            inner: Mutex::new(Inner {
                ledger,
                context: Some(context),
            }),
        })
    }

    fn lock(&self) -> BudgetResult<MutexGuard<'_, Inner>> {
        let inner = self.inner.lock().map_err(|_| BudgetError::Unavailable)?;
        if inner.context.is_none() {
            return Err(BudgetError::Unavailable);
        }
        Ok(inner)
    }

    pub(crate) fn snapshot(&self, now_ms: u64) -> BudgetResult<BudgetSnapshot> {
        let mut snapshot = self.lock()?.ledger.rolled(self.policy, now_ms).snapshot();
        snapshot.category_limits = self.category_limits.clone();
        Ok(snapshot)
    }

    pub(crate) fn reserve(&self, category: &str, count: u32, now_ms: u64) -> BudgetResult<u32> {
        validate_category(category)?;
        if count == 0 {
            return Err(BudgetError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let mut next = inner.ledger.rolled(self.policy, now_ms);
        let used = next.current.used.checked_add(count);
        if used.is_none_or(|used| used > next.policy.limit) {
            return Err(BudgetError::Exhausted {
                resets_at_ms: next.snapshot().resets_at_ms,
            });
        }
        if self.category_limits.get(category).is_some_and(|limit| {
            next.current
                .by_category
                .get(category)
                .copied()
                .unwrap_or(0)
                .checked_add(count)
                .is_none_or(|used| used > *limit)
        }) {
            return Err(BudgetError::CategoryExhausted {
                resets_at_ms: next.snapshot().resets_at_ms,
            });
        }
        if !next.current.by_category.contains_key(category)
            && next.current.by_category.len() >= MAX_CATEGORIES
        {
            return Err(BudgetError::InvalidInput);
        }
        next.current.used += count;
        *next
            .current
            .by_category
            .entry(category.to_string())
            .or_default() += count;
        let remaining = next.policy.limit - next.current.used;
        persist(&mut inner, next)?;
        Ok(remaining)
    }

    pub(crate) fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("自主运行预算锁不可用".into()))?
            .context = None;
        Ok(())
    }
}

fn validate_window(window: &BudgetWindow) -> BudgetResult<()> {
    let mut sum = 0u64;
    if window.by_category.len() > MAX_CATEGORIES {
        return Err(BudgetError::CorruptState);
    }
    for (category, count) in &window.by_category {
        validate_category(category).map_err(|_| BudgetError::CorruptState)?;
        if *count == 0 {
            return Err(BudgetError::CorruptState);
        }
        sum += u64::from(*count);
    }
    if sum != u64::from(window.used) {
        return Err(BudgetError::CorruptState);
    }
    Ok(())
}

fn validate(ledger: &Ledger) -> BudgetResult<()> {
    ledger
        .policy
        .validate()
        .map_err(|_| BudgetError::CorruptState)?;
    validate_window(&ledger.current)?;
    if ledger.history.len() > MAX_HISTORY
        || ledger
            .history
            .iter()
            .any(|window| window.start_ms >= ledger.current.start_ms || window.used == 0)
        || ledger
            .history
            .windows(2)
            .any(|pair| pair[1].start_ms >= pair[0].start_ms)
    {
        return Err(BudgetError::CorruptState);
    }
    for window in &ledger.history {
        validate_window(window)?;
    }
    Ok(())
}

fn persist(inner: &mut Inner, next: Ledger) -> BudgetResult<()> {
    let bytes = serde_json::to_vec(&next).map_err(|_| BudgetError::InvalidInput)?;
    let context = inner.context.as_ref().ok_or(BudgetError::Unavailable)?;
    // 失败也可能已经提交；关闭整个实例，之后的请求一律拒绝，须重新打开核对。
    if context.state_set(BUDGET_STATE_KEY, bytes).is_err() {
        inner.context = None;
        return Err(BudgetError::Storage);
    }
    inner.ledger = next;
    Ok(())
}
