//! QQ 宿主的自主运行预算：后台学习组件的模型请求按用途记账，开始一项工作前先核对余额，/budget 查看用量。
//!
//! 计数发生在每次模型请求之前（`BudgetedResolver`）；准入只是避免开始一项余额明显不够的工作。
//! 准入通过后实际请求仍可能因余额不足被拒，此时该次请求按模型失败记入对应账本，不重试。
use crate::AppError;
use eve_budget_api::{AutonomyBudget, BudgetError, BudgetSnapshot};
use eve_budget_plugin::BudgetedResolver;
use eve_llm_api::LlmModelResolver;
use eve_plugin_api::{PluginError, PluginResult};
use eve_qqbot_plugin::{QqCommandHandler, QqCommandInput};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const INTEREST: &str = "interest";
pub(crate) const RESEARCH: &str = "research";
pub(crate) const PRACTICE: &str = "practice";
pub(crate) const SKILL: &str = "skill";
pub(crate) const FORGE: &str = "forge";
pub(crate) const OUTREACH: &str = "outreach";
pub(crate) const CLARIFY: &str = "clarify";

/// 一项工作开始前要求的最少余额：观察 1 次；研究选来源与提取各 1 次；
/// 实践至多 3 次草稿另加 1 次技能选择；技能提炼、工具锻造、撰写邀请或问题、判断时机与回应各 1 次。
pub(crate) const INTEREST_UNITS: u32 = 1;
pub(crate) const RESEARCH_UNITS: u32 = 2;
pub(crate) const PRACTICE_UNITS: u32 = 4;
pub(crate) const SINGLE_UNIT: u32 = 1;

const DISABLED: &str =
    "自主运行预算未启用：后台学习组件的模型请求不受总量限制（各组件仍有自己的次数限制）。";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(1)
}

/// 后台开始一项工作前的余额核对；未启用预算时总是放行。
#[derive(Clone, Default)]
pub(crate) struct Gate {
    budget: Option<Arc<dyn AutonomyBudget>>,
}
impl Gate {
    pub(crate) fn new(budget: Option<Arc<dyn AutonomyBudget>>) -> Self {
        Self { budget }
    }
    /// 余额不足返回 false，不写入任何状态；预算不可用（写入失败后实例已关闭）时返回错误，后台随之停止。
    pub(crate) fn admits(&self, units: u32) -> Result<bool, AppError> {
        match &self.budget {
            None => Ok(true),
            Some(budget) => Ok(budget
                .snapshot(now_ms())
                .map_err(|error| error.to_string())?
                .remaining()
                >= units),
        }
    }
    /// 按用途包装后台模型解析器；未启用预算时原样返回。
    pub(crate) fn wrap(
        &self,
        resolver: Arc<dyn LlmModelResolver>,
        category: &str,
    ) -> Result<Arc<dyn LlmModelResolver>, AppError> {
        Ok(match &self.budget {
            None => resolver,
            Some(budget) => Arc::new(
                BudgetedResolver::new(resolver, budget.clone(), category)
                    .map_err(|error| error.to_string())?,
            ),
        })
    }
    pub(crate) fn commands(&self) -> Arc<dyn QqCommandHandler> {
        Arc::new(Commands {
            budget: self.budget.clone(),
        })
    }
}

fn label(category: &str) -> &str {
    match category {
        INTEREST => "兴趣观察",
        RESEARCH => "受控研究",
        PRACTICE => "实践草稿",
        SKILL => "技能提炼与选择",
        FORGE => "工具锻造",
        OUTREACH => "主动交流",
        CLARIFY => "请求澄清",
        other => other,
    }
}

fn hours(ms: u64) -> String {
    if ms.is_multiple_of(3_600_000) {
        format!("{} 小时", ms / 3_600_000)
    } else {
        format!("{} 分钟", ms / 60_000)
    }
}

fn describe(snapshot: &BudgetSnapshot) -> String {
    let current = &snapshot.current;
    let mut reply = format!(
        "自主运行预算：每 {}至多 {} 次后台模型请求。本窗口已用 {} 次、剩余 {} 次，{} 重置。",
        hours(snapshot.policy.window_ms),
        snapshot.policy.limit,
        current.used,
        snapshot.remaining(),
        crate::qq_research::utc(snapshot.resets_at_ms)
    );
    if !current.by_category.is_empty() {
        let parts: Vec<String> = current
            .by_category
            .iter()
            .map(|(category, count)| format!("{} {count}", label(category)))
            .collect();
        reply.push_str(&format!(" 按用途：{}。", parts.join("，")));
    }
    if snapshot.remaining() == 0 {
        reply.push_str(
            " 余额用尽：后台不再开始新的观察、研究、实践或邀请，已保存的记录不变，重置后继续。",
        );
    }
    if let Some(last) = snapshot.history.first() {
        reply.push_str(&format!(
            " 上一个有用量的窗口（{} 开始）共 {} 次。",
            crate::qq_research::utc(last.start_ms),
            last.used
        ));
    }
    reply.push_str(" 计数在请求发出前保存，中断或失败的请求不退还；回复用户消息不计入。");
    reply
}

/// /budget：全局用量，不含任何用户正文。
struct Commands {
    budget: Option<Arc<dyn AutonomyBudget>>,
}
impl QqCommandHandler for Commands {
    fn handle(&self, input: QqCommandInput<'_>) -> PluginResult<Option<String>> {
        if input.text.trim() != "/budget" {
            return Ok(None);
        }
        let Some(budget) = &self.budget else {
            return Ok(Some(DISABLED.into()));
        };
        match budget.snapshot(now_ms()) {
            Ok(snapshot) => Ok(Some(describe(&snapshot))),
            Err(BudgetError::Unavailable) => {
                Err(PluginError::State("自主运行预算暂时不可用".into()))
            }
            Err(error) => Err(PluginError::State(error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_budget_api::{BudgetPolicy, BudgetWindow};
    use std::collections::BTreeMap;

    #[test]
    fn description_names_usage_by_category_and_the_reset_time() {
        let snapshot = BudgetSnapshot {
            policy: BudgetPolicy {
                window_ms: 86_400_000,
                limit: 5,
            },
            current: BudgetWindow {
                start_ms: 1_791_331_200_000,
                used: 5,
                by_category: BTreeMap::from([(RESEARCH.into(), 2), (INTEREST.into(), 3)]),
            },
            resets_at_ms: 1_791_417_600_000,
            history: vec![BudgetWindow {
                start_ms: 1_791_244_800_000,
                used: 4,
                by_category: BTreeMap::from([(PRACTICE.into(), 4)]),
            }],
        };
        let text = describe(&snapshot);
        for part in [
            "每 24 小时至多 5 次",
            "已用 5 次、剩余 0 次",
            "2026-10-08 00:00 UTC 重置",
            "兴趣观察 3，受控研究 2",
            "余额用尽",
            "共 4 次",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }
    }
}
