//! 逐条真实交流的本地观察与待写记录；不调用模型，不执行或补发消息。
use crate::{
    Message, QqInteractionSource,
    state::{PartState, Progress},
};
use eve_control_api::{ControlPhase, ControlSnapshot};
use eve_plugin_api::{PluginContext, PluginError, PluginResult};
use eve_session_api::SessionKey;
use eve_training_api::{ConversationActivity, ConversationMetadata, ConversationRole};
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

const KEY: &str = "conversation-observation-outbox.v1";
const MAX_PENDING: usize = 256;
const MAX_BYTES: usize = 16 * 1024 * 1024;

/// 仅由已校验的 QQ 接收或真实 Sent 回执构造，正文不能选择身份或投递状态。
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConversationRecord {
    app_id: String,
    scope: String,
    target_id: String,
    user_id: String,
    session: SessionKey,
    conversation: String,
    group: bool,
    id: String,
    platform_message_id: String,
    role: ConversationRole,
    text: String,
    metadata: ConversationMetadata,
}

/// 对外只有只读借用；反序列化入口仅属于私有待写记录，外部不能伪造 Sent。
pub struct QqConversationMessage<'a> {
    inner: &'a ConversationRecord,
}
impl QqConversationMessage<'_> {
    pub fn session(&self) -> &SessionKey {
        &self.inner.session
    }
    pub fn conversation(&self) -> &str {
        &self.inner.conversation
    }
    pub fn source(&self) -> QqInteractionSource {
        if self.inner.group {
            QqInteractionSource::GroupMessage
        } else {
            QqInteractionSource::DirectMessage
        }
    }
    pub fn id(&self) -> &str {
        &self.inner.id
    }
    pub fn platform_message_id(&self) -> &str {
        &self.inner.platform_message_id
    }
    pub fn role(&self) -> ConversationRole {
        self.inner.role
    }
    pub fn text(&self) -> &str {
        &self.inner.text
    }
    pub fn metadata(&self) -> &ConversationMetadata {
        &self.inner.metadata
    }
}

impl ConversationRecord {
    pub(crate) fn metadata(&self) -> &ConversationMetadata {
        &self.metadata
    }

    pub(crate) fn with_activity(mut self, activity: Option<ConversationActivity>) -> Self {
        if activity.is_some() {
            self.metadata.activity = activity;
        }
        self
    }

    pub(crate) fn received(
        app: &str,
        message: &Message,
        original: Option<&str>,
        source_at_ms: Option<u64>,
        snapshot: Option<&ControlSnapshot>,
    ) -> PluginResult<Self> {
        let private = message.text.split_whitespace().next() == Some("/bind");
        let text = if private {
            "/bind [已隐藏]"
        } else {
            original.unwrap_or(&message.text)
        };
        let mut metadata = activity_metadata(snapshot);
        metadata.observed_at_ms = observed_at_ms();
        metadata.source_at_ms = source_at_ms;
        metadata.source_kind = Some(
            if private {
                "qq_redacted_input"
            } else if original.is_some() {
                "qq_original_input"
            } else {
                "qq_visible_input"
            }
            .into(),
        );
        metadata.context = (!private).then(|| message.learning_context()).flatten();
        if metadata.activity.is_none() {
            metadata.activity = Some(ConversationActivity {
                phase: "received".into(),
                elapsed_ms: None,
                tool_name: None,
            });
        }
        Self::new(
            app,
            message,
            &message.id,
            ConversationRole::Human,
            text,
            metadata,
        )
    }

    pub(crate) fn sent(
        app: &str,
        message: &Message,
        platform_id: &str,
        text: &str,
        metadata: ConversationMetadata,
    ) -> PluginResult<Self> {
        Self::new(
            app,
            message,
            platform_id,
            ConversationRole::Eve,
            text,
            metadata,
        )
    }

    fn new(
        app: &str,
        message: &Message,
        platform_id: &str,
        role: ConversationRole,
        text: &str,
        metadata: ConversationMetadata,
    ) -> PluginResult<Self> {
        if !crate::state::valid_id(app) || !message.valid() || !crate::valid_message_id(platform_id)
        {
            return Err(invalid());
        }
        Ok(Self {
            app_id: app.into(),
            scope: message.scope.clone(),
            target_id: message.target_id.clone(),
            user_id: message.user_id.clone(),
            session: message.session_key(app)?,
            conversation: message.conversation_key(app)?,
            group: message.scope == "group",
            id: message_id(role, platform_id),
            platform_message_id: platform_id.into(),
            role,
            text: text.into(),
            metadata,
        })
    }

    fn valid(&self) -> bool {
        let route = Message {
            id: self.platform_message_id.clone(),
            scope: self.scope.clone(),
            target_id: self.target_id.clone(),
            user_id: self.user_id.clone(),
            text: "route".into(),
            attachments: vec![],
            quoted: None,
        };
        crate::state::valid_id(&self.app_id)
            && route.valid()
            && route.session_key(&self.app_id).as_ref().ok() == Some(&self.session)
            && route.conversation_key(&self.app_id).as_ref().ok() == Some(&self.conversation)
            && self.group == (self.scope == "group")
            && crate::valid_message_id(&self.platform_message_id)
            && self.id == message_id(self.role, &self.platform_message_id)
            && self.text.len() <= eve_training_api::MAX_CONVERSATION_BODY_BYTES
            && (!self.text.trim().is_empty()
                || self
                    .metadata
                    .context
                    .as_ref()
                    .is_some_and(|s| !s.trim().is_empty()))
            && self
                .metadata
                .context
                .as_ref()
                .is_none_or(|s| s.len() <= 8192)
            && self.metadata.sequence.is_none()
            && self.metadata.fragment.is_none()
            && eve_training_api::conversation_message_valid(
                &eve_training_api::ConversationMessage {
                    id: self.id.clone(),
                    speaker: self.session.user_id.clone(),
                    role: self.role,
                    text: self.text.clone(),
                    metadata: Some(self.metadata.clone()),
                },
            )
    }
}

pub(crate) fn message_id(role: ConversationRole, platform_id: &str) -> String {
    let hash: String = digest(&SHA256, platform_id.as_bytes())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!(
        "qq-{}:{hash}",
        if role == ConversationRole::Human {
            "human"
        } else {
            "eve"
        }
    )
}

pub(crate) fn observed_at_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|t| t.as_millis().try_into().ok())
}

pub(crate) fn activity_metadata(snapshot: Option<&ControlSnapshot>) -> ConversationMetadata {
    let current = snapshot.filter(|s| s.report.is_none());
    ConversationMetadata {
        turn_id: current.and_then(|s| s.turn_id),
        activity: current.map(|s| ConversationActivity {
            phase: match s.phase {
                ControlPhase::Starting => "starting",
                ControlPhase::Generating => "generating",
                ControlPhase::Tools => "tools",
                ControlPhase::Committing => "committing",
                ControlPhase::Cancelling => "cancelling",
                ControlPhase::Finished => "finished",
                ControlPhase::Blocked => "blocked",
            }
            .into(),
            elapsed_ms: None,
            tool_name: None,
        }),
        ..Default::default()
    }
}

/// 同步、有界本地学习观察；实现不得请求模型、网络或通道发送。
/// 相同真实消息 ID 必须幂等，首次元数据保留，正文冲突须显式拒绝。
pub trait QqConversationObserver: Send + Sync {
    fn observe(&self, message: &QqConversationMessage<'_>) -> PluginResult<()>;
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    version: u8,
    messages: Vec<ConversationRecord>,
}

pub(crate) struct ConversationObservations {
    observer: Option<Arc<dyn QqConversationObserver>>,
    pending: Pending,
    drain_stopped: bool,
    write_stopped: bool,
}

fn invalid() -> PluginError {
    PluginError::State("QQ 交流观察记录无效；保留原记录".into())
}

impl ConversationObservations {
    pub(crate) fn load(
        ctx: &PluginContext,
        observer: Option<Arc<dyn QqConversationObserver>>,
    ) -> PluginResult<Self> {
        let pending = match ctx.state_get(KEY)? {
            None => Pending {
                version: 1,
                messages: vec![],
            },
            Some(bytes) if bytes.len() <= MAX_BYTES => {
                serde_json::from_slice::<Pending>(&bytes).map_err(|_| invalid())?
            }
            _ => return Err(invalid()),
        };
        if pending.version != 1
            || pending.messages.len() > MAX_PENDING
            || pending.messages.iter().any(|m| !m.valid())
        {
            return Err(invalid());
        }
        Ok(Self {
            observer,
            pending,
            drain_stopped: false,
            write_stopped: false,
        })
    }

    fn save(&self, ctx: &PluginContext) -> PluginResult<()> {
        let bytes = serde_json::to_vec(&self.pending).map_err(|_| invalid())?;
        if bytes.len() > MAX_BYTES {
            return Err(PluginError::Task("QQ 交流观察待写记录容量已满".into()));
        }
        ctx.state_set(KEY, bytes)
    }

    pub(crate) fn push(
        &mut self,
        ctx: &PluginContext,
        message: ConversationRecord,
    ) -> PluginResult<()> {
        if self.observer.is_none() {
            return Ok(());
        }
        if self.write_stopped {
            return Err(PluginError::Task(
                "QQ 交流观察待写记录保存未知；原回执保留".into(),
            ));
        }
        if !message.valid() {
            return Err(invalid());
        }
        if let Some(existing) = self.pending.messages.iter().find(|m| {
            m.session == message.session
                && m.conversation == message.conversation
                && m.id == message.id
        }) {
            if existing.role != message.role
                || existing.text != message.text
                || existing.user_id != message.user_id
                || existing.app_id != message.app_id
            {
                return Err(invalid());
            }
        } else {
            if self.pending.messages.len() >= MAX_PENDING {
                return Err(PluginError::Task("QQ 交流观察待写记录容量已满".into()));
            }
            self.pending.messages.push(message);
            if let Err(error) = self.save(ctx) {
                self.write_stopped = true;
                return Err(error);
            }
        }
        self.flush(ctx)
    }

    pub(crate) fn disabled() -> Self {
        Self {
            observer: None,
            pending: Pending {
                version: 1,
                messages: vec![],
            },
            drain_stopped: true,
            write_stopped: true,
        }
    }

    pub(crate) fn flush(&mut self, ctx: &PluginContext) -> PluginResult<()> {
        let Some(observer) = &self.observer else {
            return Ok(());
        };
        if self.drain_stopped || self.write_stopped {
            return Ok(());
        }
        while let Some(message) = self.pending.messages.first() {
            let result = catch_unwind(AssertUnwindSafe(|| {
                observer.observe(&QqConversationMessage { inner: message })
            }))
            .map_err(|_| invalid())
            .and_then(|r| r);
            if let Err(error) = result {
                self.drain_stopped = true;
                return Err(error);
            }
            let saved = self.pending.messages.remove(0);
            if let Err(error) = self.save(ctx) {
                self.pending.messages.insert(0, saved);
                self.write_stopped = true;
                return Err(error);
            }
        }
        Ok(())
    }

    pub(crate) fn restore_sent(
        &mut self,
        ctx: &PluginContext,
        app: &str,
        message: &Message,
        progress: &Progress,
    ) -> PluginResult<()> {
        if progress.state != PartState::Sent
            || progress.decision_id.is_none()
            || progress.image_path.is_some()
        {
            return Ok(());
        }
        let Some(platform_id) = progress
            .platform_message_id
            .as_deref()
            .filter(|id| crate::valid_message_id(id))
        else {
            return Ok(());
        };
        let metadata = progress
            .conversation
            .clone()
            .unwrap_or_else(|| ConversationMetadata {
                source_kind: Some("qq_sent_receipt_recovery".into()),
                reply_to: Some(message_id(
                    ConversationRole::Human,
                    progress.reply_anchor.as_deref().unwrap_or(&message.id),
                )),
                ..Default::default()
            });
        self.push(
            ctx,
            ConversationRecord::sent(app, message, platform_id, &progress.text, metadata)?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
    use eve_plugin_api::{Cleanup, Plugin, PluginFuture, PluginId, PluginManifest, StateStore};
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    struct Capture {
        manifest: PluginManifest,
        context: Arc<Mutex<Option<PluginContext>>>,
    }
    impl Plugin for Capture {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }
        fn start(&mut self, context: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
            *self.context.lock().unwrap() = Some(context);
            Box::pin(async { Ok(None) })
        }
    }
    #[derive(Default)]
    struct Store {
        inner: MemoryStateStore,
        writes: AtomicUsize,
        fail: AtomicBool,
    }
    impl StateStore for Store {
        fn get(&self, owner: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
            self.inner.get(owner, key)
        }
        fn set(&self, owner: &PluginId, key: String, bytes: Vec<u8>) -> PluginResult<()> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(invalid());
            }
            self.inner.set(owner, key, bytes)
        }
    }
    struct Observer {
        fail: AtomicBool,
        seen: Mutex<Vec<String>>,
    }
    impl QqConversationObserver for Observer {
        fn observe(&self, message: &QqConversationMessage<'_>) -> PluginResult<()> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(invalid());
            }
            self.seen.lock().unwrap().push(message.id().into());
            Ok(())
        }
    }
    fn record(id: &str) -> ConversationRecord {
        ConversationRecord::received(
            "app",
            &Message {
                id: id.into(),
                scope: "c2c".into(),
                target_id: "person".into(),
                user_id: "person".into(),
                text: "完整交流正文".into(),
                attachments: vec![],
                quoted: None,
            },
            None,
            None,
            None,
        )
        .unwrap()
    }
    async fn context(store: Arc<Store>) -> (Kernel, PluginContext) {
        let context = Arc::new(Mutex::new(None));
        let kernel = Kernel::with_services(KernelServices {
            state: store,
            ..KernelServices::default()
        });
        kernel
            .register(Box::new(Capture {
                manifest: PluginManifest::new(crate::QQBOT_PLUGIN_ID, "0.1.0").unwrap(),
                context: context.clone(),
            }))
            .unwrap();
        kernel
            .start(&PluginId::new(crate::QQBOT_PLUGIN_ID).unwrap())
            .await
            .unwrap();
        let ctx = context.lock().unwrap().take().unwrap();
        (kernel, ctx)
    }

    #[tokio::test]
    async fn observer_failure_keeps_accepting_durable_events_and_recovers_in_order() {
        let store = Arc::new(Store::default());
        let (kernel, ctx) = context(store).await;
        let observer = Arc::new(Observer {
            fail: AtomicBool::new(true),
            seen: Mutex::new(vec![]),
        });
        let mut observations =
            ConversationObservations::load(&ctx, Some(observer.clone())).unwrap();
        assert!(observations.push(&ctx, record("first")).is_err());
        observations.push(&ctx, record("second")).unwrap();
        let saved: Pending = serde_json::from_slice(&ctx.state_get(KEY).unwrap().unwrap()).unwrap();
        assert_eq!(saved.messages.len(), 2);
        let mut conflicting = record("first");
        conflicting.text = "不可替换已有正文".into();
        assert!(observations.push(&ctx, conflicting).is_err());
        observer.fail.store(false, Ordering::SeqCst);
        let mut restored = ConversationObservations::load(&ctx, Some(observer.clone())).unwrap();
        restored.flush(&ctx).unwrap();
        assert_eq!(
            *observer.seen.lock().unwrap(),
            vec![
                message_id(ConversationRole::Human, "first"),
                message_id(ConversationRole::Human, "second")
            ]
        );
        assert!(restored.pending.messages.is_empty());
        kernel.stop_all().await.unwrap();
    }

    #[tokio::test]
    async fn uncertain_outbox_write_prevents_all_later_writes_until_reload() {
        let store = Arc::new(Store::default());
        let (kernel, ctx) = context(store.clone()).await;
        let observer = Arc::new(Observer {
            fail: AtomicBool::new(true),
            seen: Mutex::new(vec![]),
        });
        let mut observations =
            ConversationObservations::load(&ctx, Some(observer.clone())).unwrap();
        assert!(observations.push(&ctx, record("first")).is_err());
        let original = ctx.state_get(KEY).unwrap().unwrap();
        store.fail.store(true, Ordering::SeqCst);
        assert!(observations.push(&ctx, record("uncertain")).is_err());
        let writes = store.writes.load(Ordering::SeqCst);
        store.fail.store(false, Ordering::SeqCst);
        assert!(observations.push(&ctx, record("later")).is_err());
        observations.flush(&ctx).unwrap();
        assert_eq!(store.writes.load(Ordering::SeqCst), writes);
        assert_eq!(ctx.state_get(KEY).unwrap().unwrap(), original);
        observer.fail.store(false, Ordering::SeqCst);
        ConversationObservations::load(&ctx, Some(observer.clone()))
            .unwrap()
            .flush(&ctx)
            .unwrap();
        assert_eq!(
            *observer.seen.lock().unwrap(),
            vec![message_id(ConversationRole::Human, "first")]
        );
        kernel.stop_all().await.unwrap();
    }

    #[test]
    fn invalid_route_and_storage_owned_fields_never_form_observations() {
        let mut message = record("one");
        assert!(message.valid());
        message.session.user_id = "other".into();
        assert!(!message.valid());
        let mut message = record("one");
        message.metadata.sequence = Some(1);
        assert!(!message.valid());
        let mut message = record("one");
        message.text.push('\0');
        assert!(!message.valid());
    }
}
