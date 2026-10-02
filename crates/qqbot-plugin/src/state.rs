use eve_plugin_api::{PluginContext, PluginError, PluginResult};
use eve_session_api::SessionKey;
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};

const STATE_KEY: &str = "receipts.v1";
pub(crate) const MAX_BYTES: usize = 1_048_576;
const MAX_RECORDS: usize = 4096;
pub(crate) fn valid_id(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 128
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
pub(crate) fn valid_message_id(v: &str) -> bool {
    !v.is_empty() && v.len() <= 128 && v.trim() == v && !v.chars().any(char::is_control)
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Message {
    pub id: String,
    pub scope: String,
    pub target_id: String,
    pub user_id: String,
    pub text: String,
}
impl Message {
    pub fn valid(&self) -> bool {
        [&self.target_id, &self.user_id]
            .into_iter()
            .all(|v| valid_id(v))
            && valid_message_id(&self.id)
            && matches!(self.scope.as_str(), "c2c" | "group")
            && (self.scope != "c2c" || self.target_id == self.user_id)
            && !self.text.trim().is_empty()
            && self.text.len() <= 32768
    }
    /// 首版临时会话映射；未来统一认知时只替换此策略并迁移旧历史。
    pub fn session_key(&self, app_id: &str) -> PluginResult<SessionKey> {
        let identity = serde_json::to_vec(&(app_id, &self.scope, &self.target_id, &self.user_id))
            .map_err(|_| corrupt())?;
        let mut key = String::from("qq:");
        for byte in digest(&SHA256, &identity).as_ref() {
            use std::fmt::Write;
            write!(&mut key, "{byte:02x}").expect("writing a String cannot fail");
        }
        SessionKey::new(key.clone(), key).map_err(|_| corrupt())
    }
}
#[derive(Clone, Copy, Serialize, Deserialize)]
pub(crate) enum ReceiptState {
    Processing,
    ReplyPending,
    Sent,
    Failed,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    pub app_id: String,
    pub message: Message,
    pub state: ReceiptState,
    pub reply: Option<String>,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ledger {
    pub version: u8,
    pub entries: Vec<Receipt>,
}
impl Ledger {
    pub fn load(ctx: &PluginContext) -> PluginResult<Self> {
        let Some(bytes) = ctx.state_get(STATE_KEY)? else {
            return Ok(Self {
                version: 1,
                entries: Vec::new(),
            });
        };
        if bytes.len() > MAX_BYTES {
            return Err(corrupt());
        }
        let ledger: Self = serde_json::from_slice(&bytes).map_err(|_| corrupt())?;
        if ledger.version != 1 || ledger.entries.len() > MAX_RECORDS {
            return Err(corrupt());
        }
        let mut identities = std::collections::BTreeSet::new();
        for entry in &ledger.entries {
            if !valid_id(&entry.app_id)
                || !entry.message.valid()
                || !identities.insert((&entry.app_id, &entry.message.id))
                || entry
                    .reply
                    .as_ref()
                    .is_some_and(|r| r.trim().is_empty() || r.len() > 32768)
                || (matches!(entry.state, ReceiptState::ReplyPending | ReceiptState::Sent)
                    && entry.reply.is_none())
            {
                return Err(corrupt());
            }
        }
        Ok(ledger)
    }
    pub fn find(&self, app: &str, id: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|r| r.app_id == app && r.message.id == id)
    }
    pub fn insert(
        &mut self,
        ctx: &PluginContext,
        app: &str,
        message: Message,
    ) -> PluginResult<bool> {
        if self.entries.len() >= MAX_RECORDS {
            return Ok(false);
        }
        self.entries.push(Receipt {
            app_id: app.into(),
            message,
            state: ReceiptState::Processing,
            reply: None,
        });
        let bytes = serde_json::to_vec(self).map_err(|_| corrupt())?;
        if bytes.len() + 6 * 32768 + 64 > MAX_BYTES {
            self.entries.pop();
            return Ok(false);
        }
        ctx.state_set(STATE_KEY, bytes)?;
        Ok(true)
    }
    pub fn save(&self, ctx: &PluginContext) -> PluginResult<()> {
        let bytes = serde_json::to_vec(self).map_err(|_| corrupt())?;
        if bytes.len() > MAX_BYTES {
            return Err(PluginError::State(
                "QQBot 回执容量不足，保留既有状态".into(),
            ));
        }
        ctx.state_set(STATE_KEY, bytes)
    }
}
fn corrupt() -> PluginError {
    PluginError::State("QQBot 回执状态损坏或版本不兼容；未清空".into())
}
