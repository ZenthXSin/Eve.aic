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
    pub started_tools: Option<u64>,
    pub tool_receipts: usize,
}
