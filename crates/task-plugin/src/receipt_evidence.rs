//! 查询宿主持有的真实历史回执，不执行工具、不复验当前产物、不改变旧完成合同。
use eve_llm_api::{ChatMessage, ModelRequest, ToolCall, ToolFailureCode, ToolOutput};
use eve_task_api::{
    TaskEvaluationContract, TaskReceiptEvidenceCheck, TaskReceiptEvidenceReference,
    TaskReceiptEvidenceReport, TaskReceiptEvidenceStatus,
};
use ring::digest::{Context, SHA256};
use serde::Serialize;
use std::collections::BTreeSet;
use std::io::{self, Write};

/// contract 必须由可信宿主在本次执行前冻结；messages 与 pending_calls 必须来自
/// 同一完整受信轨迹。身份、执行 ID、checkpoint 预算/版本与存储校验由组合层负责。
///
/// 每项只选最近一个包含精确 tool+arguments 调用的批次；该批重复匹配为 Unknown，
/// 不用旧成功掩盖新失败。后续非匹配工具批次不遮掉原历史来源，但不会由此证明
/// 检查之后的产物或环境没有变化。旧 require_final_batch=true 仍须通过原 validate。
pub fn select_task_receipt_evidence(
    contract: &TaskEvaluationContract,
    messages: &[ChatMessage],
    pending_calls: &[ToolCall],
) -> TaskReceiptEvidenceReport {
    use TaskReceiptEvidenceStatus::*;
    let mut report = TaskReceiptEvidenceReport {
        status: Missing,
        reason: "no_matching_call".into(),
        contract_sha256: digest(contract),
        checks: contract
            .checks
            .iter()
            .enumerate()
            .map(|(check_index, check)| TaskReceiptEvidenceCheck {
                check_index,
                check_sha256: digest(check),
                status: Missing,
                reason: "no_matching_call".into(),
                source: None,
            })
            .collect(),
    };
    if contract.validate().is_err() {
        mark_unknown(&mut report, "invalid_frozen_contract");
        return report;
    }
    if ModelRequest::validate_parts(messages, &[]).is_err() {
        mark_unknown(&mut report, "invalid_tool_trace");
        return report;
    }
    let mut ids = BTreeSet::new();
    for call in messages
        .iter()
        .flat_map(|message| &message.tool_calls)
        .chain(pending_calls)
    {
        if call.validate().is_err() {
            mark_unknown(&mut report, "invalid_tool_call");
            return report;
        }
        if !ids.insert(&call.id) {
            mark_unknown(&mut report, "ambiguous_call_ids");
            return report;
        }
    }

    let batches: Vec<_> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| !message.tool_calls.is_empty())
        .enumerate()
        .map(|(ordinal, (index, message))| (ordinal + 1, index, message))
        .collect();
    for (check, observation) in contract.checks.iter().zip(&mut report.checks) {
        let Some((ordinal, index, batch)) = batches.iter().rev().find(|(_, _, batch)| {
            batch
                .tool_calls
                .iter()
                .any(|call| call.name == check.tool && call.arguments == check.arguments)
        }) else {
            continue;
        };
        let calls: Vec<_> = batch
            .tool_calls
            .iter()
            .filter(|call| call.name == check.tool && call.arguments == check.arguments)
            .collect();
        if calls.len() != 1 {
            observation.status = Unknown;
            observation.reason = "ambiguous_matching_calls_in_batch".into();
            continue;
        }
        let call = calls[0];
        // 配对协议已验证；只读调用所在批次的真实回执，不搜索别的执行或模型正文。
        let Some(receipt) = messages.get(index + 1).and_then(|message| {
            message
                .tool_results
                .iter()
                .find(|receipt| receipt.call_id == call.id)
        }) else {
            observation.status = Unknown;
            observation.reason = "missing_paired_receipt".into();
            continue;
        };
        observation.source = Some(TaskReceiptEvidenceReference {
            call_id: call.id.clone(),
            batch_ordinal: *ordinal,
            tool: call.name.clone(),
            arguments_sha256: digest(&call.arguments),
            receipt_sha256: digest(receipt),
        });
        let (status, reason) = match &receipt.output {
            ToolOutput::Success(value) => match value.pointer(&check.output_pointer) {
                Some(actual) if actual == &check.expected => {
                    (HistoricalMatch, "historical_observation_match")
                }
                Some(_) => (HistoricalMismatch, "historical_observation_mismatch"),
                None => (Unknown, "missing_observation"),
            },
            ToolOutput::Failure { code, .. } if unknown_effect(*code) => {
                (Unknown, "unknown_tool_effect")
            }
            // 工具失败没有给出合同要求的实际观察，不等于产物本身不符。
            ToolOutput::Failure { .. } => (Unknown, "tool_failed"),
        };
        observation.status = status;
        observation.reason = reason.into();
    }

    // 即便未知效果与某项检查无关，也不能将本执行当作确定的历史匹配来源。
    // 保留已配对的来源方便人工核对；不自动重放 pending 或失败动作。
    if !pending_calls.is_empty() {
        mark_unknown(&mut report, "pending_tool_effect");
        return report;
    }
    if messages
        .iter()
        .flat_map(|message| &message.tool_results)
        .any(|receipt| {
            matches!(&receipt.output, ToolOutput::Failure { code, .. } if unknown_effect(*code))
        })
    {
        mark_unknown(&mut report, "unknown_tool_effect");
        return report;
    }
    let (status, reason) = if report.checks.iter().any(|check| check.status == Unknown) {
        (Unknown, "historical_checks_unknown")
    } else if report
        .checks
        .iter()
        .any(|check| check.status == HistoricalMismatch)
    {
        (HistoricalMismatch, "historical_checks_mismatch")
    } else if report.checks.iter().any(|check| check.status == Missing) {
        (Missing, "historical_checks_missing")
    } else {
        (HistoricalMatch, "historical_checks_match")
    };
    report.status = status;
    report.reason = reason.into();
    report
}

fn unknown_effect(code: ToolFailureCode) -> bool {
    matches!(
        code,
        ToolFailureCode::ExecutionFailed | ToolFailureCode::TimedOut | ToolFailureCode::Cancelled
    )
}

fn mark_unknown(report: &mut TaskReceiptEvidenceReport, reason: &str) {
    report.status = TaskReceiptEvidenceStatus::Unknown;
    report.reason = reason.into();
    for check in &mut report.checks {
        check.status = TaskReceiptEvidenceStatus::Unknown;
        check.reason = reason.into();
    }
}

/// 流式摘要完整结构，避免额外复制大回执正文。类型都是宿主持有的可序列化数据。
fn digest(value: &impl Serialize) -> String {
    struct DigestWriter(Context);
    impl Write for DigestWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = DigestWriter(Context::new(&SHA256));
    serde_json::to_writer(&mut writer, value).expect("serializable receipt evidence");
    writer
        .0
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
