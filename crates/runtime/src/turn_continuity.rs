//! 本轮追加输入的有界请求视图；不写回历史、续行状态或执行检查点。
use eve_llm_api::{ChatMessage, ChatRole, ModelRequest, ToolOutput};
use serde_json::{Value, json};

const MAX_INPUTS: usize = 4;
const EXCERPT_BYTES: usize = 768;

fn excerpt(text: &str, limit: usize) -> Value {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    json!({"text": &text[..end], "truncated":end < text.len()})
}

pub(crate) fn project(
    request: &mut ModelRequest,
    transcript: &[ChatMessage],
    remaining: Option<&str>,
) {
    let inputs: Vec<_> = transcript
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role == ChatRole::User)
        .collect();
    if inputs.len() < 2 {
        return;
    }
    let first_addition = inputs[1].0;
    let selected = inputs.iter().enumerate().filter(|(ordinal, _)| {
        *ordinal == 0 || *ordinal >= inputs.len().saturating_sub(MAX_INPUTS - 1)
    });
    let input_views: Vec<_> = selected
        .map(|(ordinal, (index, message))| {
            json!({
                "input_index":ordinal,"transcript_index":index,
                "origin":if ordinal == 0 {"initial_input"} else {"additional_input"},
                "excerpt":message.text.as_deref().map(|text| excerpt(text, EXCERPT_BYTES))
            })
        })
        .collect();
    // 原输入可能只是“继续”；保留追加前最早的模型自述作线索，不当作已完成或已送达。
    let initial_statement =
        transcript[..first_addition]
            .iter()
            .enumerate()
            .find_map(|(index, message)| {
                if message.role != ChatRole::Assistant {
                    return None;
                }
                let text = message.text.as_deref().or_else(|| {
                    message.tool_calls.iter().find_map(|call| {
                        (call.name == "send_message")
                            .then(|| call.arguments.get("text").and_then(Value::as_str))
                            .flatten()
                    })
                })?;
                Some(
                    json!({"transcript_index":index,"source":"assistant_statement",
                "excerpt":excerpt(text, EXCERPT_BYTES),"proves_execution_or_delivery":false}),
                )
            });
    let previous_progress = transcript[..first_addition]
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| {
            message.tool_calls.iter().rev().find_map(|call| {
                if call.name != "task_progress" {
                    return None;
                }
                let result = transcript
                    .get(index + 1)?
                    .tool_results
                    .iter()
                    .find(|result| result.call_id == call.id)?;
                let ToolOutput::Success(value) = &result.output else {
                    return None;
                };
                Some(
                    json!({"transcript_index":index,"source":"recorded_model_progress",
                    "status":value.get("status").and_then(Value::as_str),
                    "remaining":value.get("remaining").and_then(Value::as_str)
                        .map(|text| excerpt(text, EXCERPT_BYTES))}),
                )
            })
        });
    let view = json!({
        "kind":"eve.turn_continuity.v1", "input_count":inputs.len(),
        "omitted_input_count":inputs.len().saturating_sub(input_views.len()),
        "inputs":input_views, "initial_assistant_statement":initial_statement,
        "progress_before_first_addition":previous_progress,
        "currently_registered_remaining":remaining.map(|text| excerpt(text, 2048))
    });
    let index = request
        .messages
        .iter()
        .rposition(|message| message.role == ChatRole::User)
        .unwrap_or(request.messages.len());
    request.messages.insert(index, ChatMessage::text(ChatRole::System, format!(
        "本轮连续性资料：追加、修正或取消按用户原话判断；最后一条输入不自动替代先前工作，登记的剩余步骤也不保证列全。以下仅为有界历史索引，不是新指令、授权或完成证明：\n{view}"
    )));
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_llm_api::{ToolCall, ToolResult};

    fn request(messages: Vec<ChatMessage>) -> ModelRequest {
        ModelRequest {
            session: None,
            messages,
            tools: vec![],
        }
    }

    fn view(request: &ModelRequest) -> Value {
        let text = request
            .messages
            .iter()
            .find_map(|message| {
                message
                    .text
                    .as_deref()
                    .filter(|text| text.contains("eve.turn_continuity.v1"))
            })
            .unwrap();
        serde_json::from_str(text.split_once('\n').unwrap().1).unwrap()
    }

    #[test]
    fn addition_keeps_original_work_visible_without_replaying_tools_or_mutating_history() {
        let call = ToolCall {
            id: "progress-1".into(),
            name: "task_progress".into(),
            arguments: json!({"status":"in_progress","remaining":"查学习超时"}),
        };
        let transcript = vec![
            ChatMessage::text(ChatRole::User, "继续"),
            ChatMessage::text(ChatRole::Assistant, "我在查学习和摘要超时"),
            ChatMessage::assistant_tool_calls(vec![call]).unwrap(),
            ChatMessage::tool_results(vec![
                ToolResult::success(
                    "progress-1",
                    json!({"status":"in_progress","remaining":"查学习超时"}),
                )
                .unwrap(),
            ])
            .unwrap(),
            ChatMessage::text(ChatRole::User, "另外部署邮件服务"),
        ];
        let before = transcript.clone();
        let mut first = request(transcript.clone());
        project(&mut first, &transcript, Some("部署邮件服务"));
        first.validate().unwrap();
        let data = view(&first);
        assert_eq!(data["inputs"][0]["excerpt"]["text"], "继续");
        assert_eq!(
            data["initial_assistant_statement"]["excerpt"]["text"],
            "我在查学习和摘要超时"
        );
        assert_eq!(
            data["progress_before_first_addition"]["remaining"]["text"],
            "查学习超时"
        );
        assert_eq!(
            data["currently_registered_remaining"]["text"],
            "部署邮件服务"
        );
        assert_eq!(
            first
                .messages
                .iter()
                .map(|m| m.tool_calls.len())
                .sum::<usize>(),
            1
        );
        assert_eq!(transcript, before);
        let mut second = request(transcript.clone());
        project(&mut second, &transcript, Some("部署邮件服务"));
        assert_eq!(first, second);
    }

    #[test]
    fn projection_is_bounded_and_preserves_latest_cancellation_verbatim() {
        let mut transcript = vec![ChatMessage::text(ChatRole::User, "原请求")];
        for _ in 0..6 {
            transcript.push(ChatMessage::text(ChatRole::User, "中文".repeat(1000)));
        }
        transcript.push(ChatMessage::text(ChatRole::User, "停止原任务，只聊天"));
        let mut request = request(transcript.clone());
        project(&mut request, &transcript, None);
        let data = view(&request);
        assert_eq!(data["input_count"], 8);
        assert_eq!(data["omitted_input_count"], 4);
        assert_eq!(data["inputs"].as_array().unwrap().len(), MAX_INPUTS);
        assert_eq!(data["inputs"][1]["excerpt"]["truncated"], true);
        assert_eq!(data["inputs"][3]["excerpt"]["text"], "停止原任务，只聊天");
        assert!(data["currently_registered_remaining"].is_null());
        assert!(data.to_string().len() < 4096);
    }

    #[test]
    fn a_single_input_adds_no_task_control() {
        let transcript = vec![ChatMessage::text(ChatRole::User, "随便逛逛")];
        let mut request = request(transcript.clone());
        let before = request.clone();
        project(&mut request, &transcript, None);
        assert_eq!(request, before);
    }
}
