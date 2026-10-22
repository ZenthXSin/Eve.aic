//! 表情包的 QQ 组合：从本轮收到的图片里收集候选，后台请视觉模型审看，
//! 对话中用 `send_sticker` 按心情挑一张，通道在回复正文发完后作为图片发出。
//!
//! 只收消息里直接附带、下载成功且不超过 512 KiB 的图片；引用消息里的图片不收。审看前不会发出，
//! 不是表情包的图片字节随即删除。挑选结果只在内存里等本条回复带走，进程退出即失效。
use crate::AppError;
use eve_config_api::{ConfigField, ConfigKind, ConfigSchema, ConfigService};
use eve_llm_api::{
    ImageInput, Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext,
    ToolExecutionError, ToolFuture, ToolValidationError,
};
use eve_qqbot_plugin::QqStickers;
use eve_session_api::SessionKey;
use eve_sticker_api::{MOODS, StickerLibrary, StickerReviewer, valid_mood};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::watch, task::JoinHandle};

pub(crate) const NAMESPACE: &str = "runtime.stickers";
pub(crate) const MODEL_USE: &str = "sticker";
pub(crate) const SEND_TOOL: &str = "send_sticker";
const REVIEW_TIMEOUT: Duration = Duration::from_secs(60);
/// 审看请求失败后暂停的时间；待审看的图片留在库里，之后再审。
const PROVIDER_COOLDOWN: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(500);

pub(crate) fn schema() -> ConfigSchema {
    let boolean = |environment: &str| {
        let mut field = ConfigField::new(ConfigKind::Boolean, Some(json!(true)));
        field.environment = Some(environment.into());
        field
    };
    ConfigSchema {
        namespace: NAMESPACE.into(),
        version: 1,
        fields: BTreeMap::from([
            ("collect".into(), boolean("EVE_STICKER_COLLECT")),
            ("send".into(), boolean("EVE_STICKER_SEND")),
        ]),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|v| u64::try_from(v.as_millis()).ok())
        .unwrap_or(1)
}

/// 表情包库、热配置与本轮挑选结果。配置服务在核心安装后才可用；之前一律按关闭处理。
pub(crate) struct Stickers {
    pub(crate) library: Arc<dyn StickerLibrary>,
    pub(crate) settings: OnceLock<Arc<dyn ConfigService>>,
    choices: Mutex<HashMap<(String, String), PathBuf>>,
}

impl Stickers {
    pub(crate) fn new(library: Arc<dyn StickerLibrary>) -> Self {
        Self {
            library,
            settings: OnceLock::new(),
            choices: Mutex::new(HashMap::new()),
        }
    }

    fn enabled(&self, field: &str) -> bool {
        self.settings
            .get()
            .and_then(|s| s.snapshot(NAMESPACE, 1).ok())
            .and_then(|s| s.get::<bool>(field).ok())
            .unwrap_or(false)
    }

    /// 本轮成功下载的一张图片；开启收集时交给库，重复或超限的不收。失败只记诊断，不影响对话。
    pub(crate) fn offer(&self, image: &ImageInput) {
        if !self.enabled("collect") {
            return;
        }
        match self.library.offer(image, now_ms()) {
            Ok(Some(id)) => eprintln!("EVE_STICKERS {}", json!({"outcome": "collected", "id": id})),
            Ok(None) => {}
            Err(error) => eprintln!(
                "EVE_STICKERS {}",
                json!({"outcome": "collect_failed", "error": error.to_string()})
            ),
        }
    }
}

impl QqStickers for Stickers {
    fn take(&self, session: &SessionKey) -> Option<PathBuf> {
        self.choices
            .lock()
            .ok()?
            .remove(&(session.session_id.clone(), session.user_id.clone()))
    }
}

pub(crate) struct SendStickerTool(pub(crate) Arc<Stickers>);

impl Tool for SendStickerTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: SEND_TOOL.into(),
            description: format!(
                "在这条回复的文字后面跟一张表情包，像聊天时顺手发个表情。mood 只能是：{}。\
                 只在气氛合适的闲聊里偶尔用，正式回答、认真讨论或用户不喜欢时不要用；一轮至多一张。\
                 宿主会自动查看用户直接附带、至多 512 KiB 的图片，由当前主模型审看，合适的表情包自动保存，不需要额外收藏工具；引用图片不收。\
                 action=status 可查询实际收集/发送开关与库存，不发送图片，此时无需 mood。默认 action=send，需要 mood；没有对应心情时返回 chosen=false。\
                 正文里不要描述或预告这张表情包。",
                MOODS.join("、")
            ),
            argument_schema: json!({
                "type": "object",
                "properties": {"mood": {"type": "string", "enum": MOODS}, "action":{"type":"string","enum":["send","status"]}},
                "additionalProperties": false
            }),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial {
                scope: "eve.qqbot.stickers".into(),
            }),
        }
    }

    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        let invalid = |message: &str| ToolValidationError {
            message: message.into(),
        };
        let object = value.as_object().ok_or_else(|| invalid("参数必须是对象"))?;
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "mood" | "action"))
        {
            return Err(invalid("参数只能包含 mood 和 action"));
        }
        if let Some(action) = object.get("action") {
            match action.as_str() {
                Some("status") if !object.contains_key("mood") => return Ok(()),
                Some("send") => {}
                _ => return Err(invalid("action 只能是 send 或 status；status 不携带 mood")),
            }
        }
        match object.get("mood").and_then(Value::as_str) {
            Some(mood) if valid_mood(mood) => Ok(()),
            _ => Err(invalid(&format!("mood 只能是：{}", MOODS.join("、")))),
        }
    }

    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            let scope = context.scope().ok_or_else(|| {
                ToolExecutionError::Failed("只有带用户身份的对话里才能发表情包".into())
            })?;
            if call.arguments.get("action").and_then(Value::as_str) == Some("status") {
                let rows = self
                    .0
                    .library
                    .list()
                    .map_err(|_| ToolExecutionError::Failed("表情包库不可用，未修改".into()))?;
                return Ok(
                    json!({"chosen":false,"collect_enabled":self.0.enabled("collect"),"send_enabled":self.0.enabled("send"),
                    "approved":rows.iter().filter(|s| s.state == eve_sticker_api::StickerState::Approved).count(),
                    "pending":rows.iter().filter(|s| s.state == eve_sticker_api::StickerState::Pending).count(),
                    "rejected":rows.iter().filter(|s| s.state == eve_sticker_api::StickerState::Rejected).count()}),
                );
            }
            if !self.0.enabled("send") {
                return Ok(json!({"chosen": false, "reason": "表情包发送已关闭"}));
            }
            let mood = call
                .arguments
                .get("mood")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let failed = |error: eve_sticker_api::StickerError| {
                ToolExecutionError::Failed(format!("表情包库不可用：{error}"))
            };
            let Some(choice) = self.0.library.pick(&mood).map_err(failed)? else {
                return Ok(
                    json!({"chosen": false, "available_moods": self.0.library.moods().map_err(failed)?}),
                );
            };
            self.0
                .choices
                .lock()
                .map_err(|_| ToolExecutionError::Failed("表情包选择不可用".into()))?
                .insert(
                    (scope.session_id.clone(), scope.user_id.clone()),
                    choice.path,
                );
            Ok(json!({
                "chosen": true,
                "description": choice.description,
                "note": "这张表情包会在本条回复的文字发完后跟着发出；正文里不要再描述它。发送失败不会重试。"
            }))
        })
    }
}

pub(crate) struct Background {
    active: watch::Sender<bool>,
    stop: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
    task: JoinHandle<()>,
}
impl Background {
    pub(crate) fn start(stickers: Arc<Stickers>, reviewer: Arc<dyn StickerReviewer>) -> Self {
        let (active, activated) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let (finished_sender, finished) = watch::channel(false);
        let task = tokio::spawn(async move {
            run(stickers, reviewer, activated, stopped).await;
            let _ = finished_sender.send(true);
        });
        Self {
            active,
            stop,
            finished,
            task,
        }
    }
    pub(crate) fn activate(&self) {
        let _ = self.active.send(true);
    }
    pub(crate) fn request_stop(&self) {
        let _ = self.stop.send(true);
    }
    pub(crate) fn finished(&self) -> watch::Receiver<bool> {
        self.finished.clone()
    }
    pub(crate) async fn stop(self) -> Result<(), AppError> {
        self.request_stop();
        self.task
            .await
            .map_err(|_| "表情包审看后台异常；已收下的表情包不受影响".into())
    }
}

async fn stop_requested(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

fn report(outcome: &str, id: &str, detail: Value) {
    eprintln!(
        "EVE_STICKERS {}",
        json!({"outcome": outcome, "id": id, "at_ms": now_ms(), "detail": detail})
    );
}

async fn run(
    stickers: Arc<Stickers>,
    reviewer: Arc<dyn StickerReviewer>,
    mut active: watch::Receiver<bool>,
    mut stopped: watch::Receiver<bool>,
) {
    loop {
        if *active.borrow() {
            break;
        }
        tokio::select! {
            biased;
            _ = stop_requested(&mut stopped) => return,
            changed = active.changed() => if changed.is_err() { return; },
        }
    }
    loop {
        let mut pause = POLL;
        if stickers.enabled("collect") {
            match stickers.library.next_pending() {
                Ok(Some((id, image))) => {
                    let request = reviewer.review(image);
                    tokio::pin!(request);
                    let result = tokio::select! {
                        biased;
                        _ = stop_requested(&mut stopped) => return,
                        result = tokio::time::timeout(REVIEW_TIMEOUT, &mut request) => result,
                    };
                    match result {
                        Ok(Ok(review)) => match stickers.library.finish_review(&id, &review) {
                            Ok(()) => report(
                                if review.sticker {
                                    "approved"
                                } else {
                                    "rejected"
                                },
                                &id,
                                json!({"moods": review.moods}),
                            ),
                            Err(error) => {
                                report("unavailable", &id, json!({"error": error.to_string()}));
                                pause = PROVIDER_COOLDOWN;
                            }
                        },
                        // 输出不合格按“不是表情包”处理：删除字节，以后也不再收这张图。
                        Ok(Err(eve_llm_api::LlmError::Protocol(_))) => {
                            let rejected = eve_sticker_api::StickerReview {
                                sticker: false,
                                description: String::new(),
                                moods: vec![],
                            };
                            let _ = stickers.library.finish_review(&id, &rejected);
                            report("rejected_invalid_output", &id, json!({}));
                        }
                        // 模型暂时不可用：图片留在待审看里，冷却后再审。
                        failed => {
                            let error = match failed {
                                Ok(Err(error)) => error.to_string(),
                                _ => "timeout".into(),
                            };
                            report("review_failed", &id, json!({"error": error}));
                            pause = PROVIDER_COOLDOWN;
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    report("unavailable", "", json!({"error": error.to_string()}));
                    pause = PROVIDER_COOLDOWN;
                }
            }
        }
        tokio::select! {
            biased;
            _ = stop_requested(&mut stopped) => return,
            _ = tokio::time::sleep(pause) => {},
        }
    }
}
