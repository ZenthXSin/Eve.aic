//! 冷回执数据库公开契约；内容必须完整保存，不能以摘要代替去重证据。
use eve_plugin_api::PluginResult;
use eve_session_api::SessionKey;
use serde::{Deserialize, Serialize};

/// 平台已认证的单条正文。与模型输入、引用及附件说明分离；None 是证据缺口，不能补猜。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QqOriginalMessage {
    pub app_id: String,
    pub message_id: String,
    pub scope: String,
    pub target_id: String,
    pub user_id: String,
    pub text: Option<String>,
}
impl QqOriginalMessage {
    pub fn valid(&self) -> bool {
        crate::state::valid_id(&self.app_id)
            && crate::state::valid_message_id(&self.message_id)
            && crate::state::valid_id(&self.target_id)
            && crate::state::valid_id(&self.user_id)
            && matches!(self.scope.as_str(), "c2c" | "group")
            && (self.scope != "c2c" || self.target_id == self.user_id)
            && self.text.as_ref().is_none_or(|text| text.len() <= 32768)
    }
    fn route(&self) -> PluginResult<crate::Message> {
        if !self.valid() {
            return Err(eve_plugin_api::PluginError::State(
                "QQ 原文身份或正文无效".into(),
            ));
        }
        Ok(crate::Message {
            id: self.message_id.clone(),
            scope: self.scope.clone(),
            target_id: self.target_id.clone(),
            user_id: self.user_id.clone(),
            text: String::new(),
            attachments: vec![],
            quoted: None,
        })
    }
    pub fn session_key(&self) -> PluginResult<SessionKey> {
        self.route()?.session_key(&self.app_id)
    }
    pub fn conversation_key(&self) -> PluginResult<String> {
        self.route()?.conversation_key(&self.app_id)
    }
}

pub struct QqArchivedReceipt {
    pub app_id: String,
    pub message_id: String,
    pub session_id: String,
    pub failed_reply: bool,
    pub payload: Vec<u8>,
}

/// 宿主持久化绑定的最小进度；归档事务可以已提交更多行，但身份或计数绝不能回退。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QqOriginalBinding {
    pub identity: String,
    pub records: u64,
}
impl QqOriginalBinding {
    pub fn valid(&self) -> bool {
        self.identity.len() == 32 && self.identity.bytes().all(|b| b.is_ascii_hexdigit())
    }
    pub fn matches(&self, identity: &str, records: u64) -> bool {
        self.valid() && self.identity == identity && records >= self.records
    }
}

fn check_original_binding(
    actual: Option<(String, u64)>,
    expected: &QqOriginalBinding,
) -> PluginResult<()> {
    if actual.is_some_and(|(identity, records)| expected.matches(&identity, records)) {
        Ok(())
    } else {
        Err(eve_plugin_api::PluginError::State(
            "QQ 原文数据库身份或进度与宿主绑定不符".into(),
        ))
    }
}

pub trait QqReceiptArchive: Send + Sync {
    /// 数据库身份与已提交行数；活跃账本绑定它，缺失/旧数据库不得静默重建。
    fn identity(&self) -> PluginResult<(String, u64)>;
    /// 同一 AppID/message id 只允许完全相同的字节；整批事务成功后才返回总行数。
    fn store(&self, records: &[QqArchivedReceipt]) -> PluginResult<u64>;
    fn get(&self, app: &str, message_id: &str) -> PluginResult<Option<Vec<u8>>>;
    fn latest_failed(&self, app: &str, session_id: &str) -> PluginResult<Option<Vec<u8>>>;
    /// 可替换实现必须显式支持原文库；旧后端不能用加工回执冒充原文。
    fn original_identity(&self) -> PluginResult<Option<(String, u64)>> {
        Ok(None)
    }
    /// 先持久化整条原文再提交模型；同一平台 ID 只接受相同原始字节。
    fn store_original(&self, _record: &QqOriginalMessage) -> PluginResult<u64> {
        Err(eve_plugin_api::PluginError::State(
            "QQ 原文归档未装配".into(),
        ))
    }
    /// 完整身份及真实锚点均须存在，按接收顺序返回截至锚点最近六条，旧到新。
    fn read_originals(
        &self,
        _app: &str,
        _session: &SessionKey,
        _through_message_id: &str,
    ) -> PluginResult<Vec<QqOriginalMessage>> {
        Err(eve_plugin_api::PluginError::State(
            "QQ 原文归档未装配".into(),
        ))
    }
    /// 核对持久绑定后写入。并发或可替换后端须覆盖此方法，将核对和操作置于同一事务；
    /// 默认实现只适用于两次调用期间不会改变数据库身份或回退数据的后端。
    fn store_original_bound(
        &self,
        record: &QqOriginalMessage,
        expected: &QqOriginalBinding,
    ) -> PluginResult<u64> {
        check_original_binding(self.original_identity()?, expected)?;
        self.store_original(record)
    }
    /// 同一快照内验证身份、最低进度及消息锚点；并发后端必须提供原子实现。
    fn read_originals_bound(
        &self,
        app: &str,
        session: &SessionKey,
        through_message_id: &str,
        expected: &QqOriginalBinding,
    ) -> PluginResult<Vec<QqOriginalMessage>> {
        check_original_binding(self.original_identity()?, expected)?;
        self.read_originals(app, session, through_message_id)
    }
    /// 在同一数据库快照内核对绑定、完整会话身份与该会话最后接收的消息 ID。
    /// 只判断元数据，正文缺口或后到但尚未消费的消息同样会使旧锚点失效。
    /// 不支持此原子操作的后端必须拒绝，不能用可变轮次锚点或截止锚点历史代替。
    fn original_is_current_bound(
        &self,
        _app: &str,
        _session: &SessionKey,
        _message_id: &str,
        _expected: &QqOriginalBinding,
    ) -> PluginResult<bool> {
        Err(eve_plugin_api::PluginError::State(
            "QQ 原文归档不支持原子新鲜度校验".into(),
        ))
    }
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
