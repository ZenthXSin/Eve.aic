//! 冷回执数据库公开契约；内容必须完整保存，不能以摘要代替去重证据。
use eve_plugin_api::PluginResult;

pub struct QqArchivedReceipt {
    pub app_id: String,
    pub message_id: String,
    pub session_id: String,
    pub failed_reply: bool,
    pub payload: Vec<u8>,
}

pub trait QqReceiptArchive: Send + Sync {
    /// 数据库身份与已提交行数；活跃账本绑定它，缺失/旧数据库不得静默重建。
    fn identity(&self) -> PluginResult<(String, u64)>;
    /// 同一 AppID/message id 只允许完全相同的字节；整批事务成功后才返回总行数。
    fn store(&self, records: &[QqArchivedReceipt]) -> PluginResult<u64>;
    fn get(&self, app: &str, message_id: &str) -> PluginResult<Option<Vec<u8>>>;
    fn latest_failed(&self, app: &str, session_id: &str) -> PluginResult<Option<Vec<u8>>>;
}

/// 只读拉取本会话的一条冷回执；调用者不能指定其他会话，不把旧原文当成新指令。
pub fn read_archived_receipt(
    archive: &dyn QqReceiptArchive,
    app: &str,
    session: &eve_session_api::SessionKey,
    message_id: &str,
) -> PluginResult<Option<serde_json::Value>> {
    if !crate::state::valid_message_id(message_id) {
        return Err(eve_plugin_api::PluginError::State("回执编号无效".into()));
    }
    let entry = archive
        .get(app, message_id)?
        .map(|bytes| crate::state::decode_archived(&bytes))
        .transpose()?;
    Ok(entry.filter(|e| e.app_id == app && e.message.id == message_id && e.message.session_key(app).ok().as_ref() == Some(session))
        .map(|e| serde_json::json!({"message_id":e.message.id,"input":e.message.text,"state":e.state,"reply":e.reply,
            "attachments":e.message.attachments,"quoted":e.message.quoted,"sticker":e.sticker,"merged_into":e.merged_into,"segments":e.segments,"progress":e.progress})))
}
