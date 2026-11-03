//! Web 对话只接收会话和请求编号，真实主人身份由宿主绑定。
use crate::{PanelError, PanelResult};
use eve_control_api::GenerationKey;
use serde::{Deserialize, Serialize};

pub const MAX_CHAT_INPUT_BYTES: usize = 32768;
pub fn valid_chat_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChatLookup {
    pub conversation_id: String,
    pub request_id: String,
}
impl ChatLookup {
    pub fn validate(&self) -> PanelResult<()> {
        if valid_chat_id(&self.conversation_id) && valid_chat_id(&self.request_id) {
            Ok(())
        } else {
            Err(PanelError::InvalidInput)
        }
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChatSubmit {
    pub conversation_id: String,
    pub request_id: String,
    pub text: String,
}
impl ChatSubmit {
    pub fn lookup(&self) -> ChatLookup {
        ChatLookup {
            conversation_id: self.conversation_id.clone(),
            request_id: self.request_id.clone(),
        }
    }
    pub fn validate(&self) -> PanelResult<()> {
        self.lookup().validate()?;
        if self.text.trim().is_empty()
            || self.text.len() > MAX_CHAT_INPUT_BYTES
            || self.text.contains('\0')
        {
            Err(PanelError::InvalidInput)
        } else {
            Ok(())
        }
    }
}
#[derive(Serialize)]
pub struct ChatInfo {
    pub role: &'static str,
    pub user_id: &'static str,
    pub session_prefix: &'static str,
    pub max_input_bytes: usize,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatState {
    pub conversation_id: String,
    pub request_id: String,
    /// accepted/generating/tools/committing/cancelling/completed/failed/interrupted/unknown。
    pub status: String,
    pub generation: Option<GenerationKey>,
    pub turn_id: Option<u64>,
    pub text: Option<String>,
    pub failure: Option<String>,
    /// 回复完成状态与记忆采集结果独立；有值表示交流记忆尚未确认保存。
    /// 已保存的 Web 回复不等于用户已读，补采成功后宿主清除此字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_error: Option<String>,
    pub started_tools: Option<u64>,
    pub tool_receipts: usize,
}

#[cfg(test)]
mod tests {
    use super::ChatState;

    #[test]
    fn legacy_receipts_default_memory_error_without_changing_completed_status() {
        let value = serde_json::json!({
            "conversation_id": "11111111111111111111111111111111",
            "request_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "status": "completed",
            "generation": null,
            "turn_id": null,
            "text": "已保存回复",
            "failure": null,
            "started_tools": 0,
            "tool_receipts": 0
        });
        let mut state: ChatState = serde_json::from_value(value.clone()).unwrap();
        assert!(state.memory_error.is_none());
        assert_eq!(serde_json::to_value(&state).unwrap(), value);
        state.memory_error = Some("交互记忆容量已满；保留原记录".into());
        let restored: ChatState =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(restored.status, "completed");
        assert_eq!(restored.text.as_deref(), Some("已保存回复"));
        assert!(restored.failure.is_none());
        assert_eq!(restored.memory_error, state.memory_error);
    }
}
