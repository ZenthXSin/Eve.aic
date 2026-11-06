//! 回顾和规划只获知执行器的当前能力，不在本阶段开放工具或冻结未来权限。
use eve_llm_api::{ContextScope, LlmError};
use eve_runtime::LlmHost;
use serde_json::{Value, json};

const MAX_NAMES: usize = 128;
const MAX_NAMES_BYTES: usize = 8192;

pub(super) fn rules(host: &LlmHost, scope: Option<&ContextScope>, phase_rules: &str) -> String {
    // 后台实际运行也使用 delegated；不能把前台发言等已过滤能力写进未来行动清单。
    let snapshot = snapshot(host.clone().delegated().available_tool_names(scope));
    format!(
        "{phase_rules}\n后续行动执行器的当前能力快照（仅工具名称数据）：\n{snapshot}\n\
        本轮 tools 为空仅表示当前回顾或规划阶段不执行，不代表 Eve 整体没有工具。\
        回顾选择 explore 后直接进入独立工具会话，不需要目标、收益或成功合同；选择 act 才按原任务流程规划和验收。\
        status=available 时，清单来自当前执行宿主已注册并通过装配检查的工具，可帮助判断后续行动是否可行；\
        omitted 大于零表示只展示部分工具，未展示不等于不存在。\
        status=unavailable 表示本次能力快照未能核对，不等于零工具，也不能猜测可用工具或保证行动成功。\
        后续执行会重新装配工具并检查身份、权限和具体动作；清单不是调用许可、未来能力保证或已执行证据。\
        需要更多事实时，可以基于当前材料提出用可用查询能力先核验的行动，无须把等待新消息当作取得证据的唯一办法；\
        旧任务存在未知副作用时，核验已有记录与重放旧操作是两回事。不能用本阶段 tools 为空断言后续无法查源码或核验记录。\
        有无工具不决定必须行动：回顾阶段可以选择 wait、reflect、explore 或 act；规划阶段继续使用本任务的规划输出协议。保留未知与未完成的事实。"
    )
}

fn snapshot(names: Result<Vec<String>, LlmError>) -> Value {
    let Ok(names) = names else {
        return json!({"kind":"eve.task.action-capabilities","status":"unavailable",
            "total":null,"omitted":null,"tools":[]});
    };
    let total = names.len();
    let mut shown = Vec::new();
    let mut bytes = 2;
    for name in names.into_iter().take(MAX_NAMES) {
        let next = json!(&name).to_string().len() + usize::from(!shown.is_empty());
        if bytes + next > MAX_NAMES_BYTES {
            break;
        }
        bytes += next;
        shown.push(name);
    }
    json!({"kind":"eve.task.action-capabilities","status":"available",
        "total":total,"omitted":total-shown.len(),"tools":shown})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_directory_does_not_claim_zero_available_tools() {
        let absent = snapshot(Err(LlmError::Configuration("fixture failure".into())));
        assert_eq!(absent["status"], "unavailable");
        assert!(absent["total"].is_null());
        assert!(absent["omitted"].is_null());
        assert_eq!(absent["tools"], json!([]));
        let empty = snapshot(Ok(vec![]));
        assert_eq!(empty["status"], "available");
        assert_eq!(empty["total"], 0);
        assert_eq!(empty["omitted"], 0);
    }

    #[test]
    fn large_directory_preserves_names_and_reports_both_limits() {
        for names in [
            (0..160).map(|n| format!("tool_{n:03}")).collect(),
            (0..100)
                .map(|n| format!("tool_{n:03}_{}", "x".repeat(115)))
                .collect(),
        ] {
            let value = snapshot(Ok(names));
            let shown = value["tools"].as_array().unwrap();
            assert!(shown.len() <= MAX_NAMES);
            assert!(value["tools"].to_string().len() <= MAX_NAMES_BYTES);
            assert!(value["omitted"].as_u64().unwrap() > 0);
            assert_eq!(
                value["total"].as_u64().unwrap(),
                shown.len() as u64 + value["omitted"].as_u64().unwrap()
            );
        }
    }
}
