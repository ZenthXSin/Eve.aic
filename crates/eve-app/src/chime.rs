//! 群聊插话的组合层：热配置、判断器选择，以及不绑定平台的 `eve-chime` 宿主。
//!
//! 宿主从标准输入逐行读取渠道已核对过的群消息（JSON），交给插话引擎；需要说话时经
//! 正常对话链路生成并保存回复，按句分条，再把每条消息的决定与回复写成一行 JSON。
//! 真实渠道接入时只需提供自己的 `ChimeOutlet` 并把群消息喂给同一个引擎。
use crate::{AppError, CoreBootstrap, core_bootstrap, finish_core, install_core, storage};
use eve_chime_api::{
    ChimeContext, ChimeJudge, ChimeOutlet, ChimeReply, ChimeService, ChimeVerdict, GroupMessage,
};
use eve_chime_plugin::{
    ChimeEngine, ChimeSettings, ChimeSettingsSource, ModelChimeJudge, QuietJudge,
};
use eve_config_api::{ConfigField, ConfigKind, ConfigSchema, ConfigService};
use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::LlmFuture;
use serde_json::json;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::{BufRead, Write},
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};

pub(crate) const NAMESPACE: &str = "runtime.chime";
pub(crate) const MODEL_USE: &str = "chime";
/// 群回复会话的用户标识；会话标识取群标识。
const SESSION_USER: &str = "eve-chime";
const SEGMENT_LIMITS: eve_segment_api::SegmentLimits = eve_segment_api::SegmentLimits {
    max_segments: 3,
    max_segment_bytes: 4096,
    max_pause_ms: 2500,
};

pub(crate) fn schema() -> ConfigSchema {
    let integer = |default: i64, min: i64, max: i64, environment: &str| {
        let mut field = ConfigField::new(
            ConfigKind::Integer {
                minimum: Some(min),
                maximum: Some(max),
            },
            Some(json!(default)),
        );
        field.environment = Some(environment.into());
        field
    };
    let mut enabled = ConfigField::new(ConfigKind::Boolean, Some(json!(true)));
    enabled.environment = Some("EVE_CHIME_ENABLED".into());
    let mut judge = ConfigField::new(ConfigKind::String, Some(json!("model")));
    judge.environment = Some("EVE_CHIME_JUDGE".into());
    ConfigSchema {
        namespace: NAMESPACE.into(),
        version: 1,
        fields: BTreeMap::from([
            ("enabled".into(), enabled),
            ("judge".into(), judge),
            (
                "cooldown_ms".into(),
                integer(60_000, 0, 86_400_000, "EVE_CHIME_COOLDOWN_MS"),
            ),
            (
                "max_replies_per_hour".into(),
                integer(20, 0, 600, "EVE_CHIME_MAX_REPLIES_PER_HOUR"),
            ),
            (
                "min_confidence".into(),
                integer(70, 0, 100, "EVE_CHIME_MIN_CONFIDENCE"),
            ),
            (
                "window_messages".into(),
                integer(20, 1, 100, "EVE_CHIME_WINDOW_MESSAGES"),
            ),
            (
                "judge_timeout_ms".into(),
                integer(15_000, 1000, 60_000, "EVE_CHIME_JUDGE_TIMEOUT_MS"),
            ),
        ]),
    }
}

/// 配置服务在核心安装后才可用；之前与读取失败时按关闭处理。
#[derive(Default)]
pub(crate) struct Policy {
    pub(crate) settings: OnceLock<Arc<dyn ConfigService>>,
}
impl Policy {
    fn judge(&self) -> Option<String> {
        self.settings
            .get()?
            .snapshot(NAMESPACE, 1)
            .ok()?
            .get::<String>("judge")
            .ok()
    }
}
impl ChimeSettingsSource for Policy {
    fn settings(&self) -> Option<ChimeSettings> {
        let snapshot = self.settings.get()?.snapshot(NAMESPACE, 1).ok()?;
        let number = |name: &str| {
            snapshot
                .get::<i64>(name)
                .ok()
                .and_then(|v| u64::try_from(v).ok())
        };
        Some(ChimeSettings {
            enabled: snapshot.get("enabled").ok()?,
            cooldown_ms: number("cooldown_ms")?,
            max_replies_per_hour: usize::try_from(number("max_replies_per_hour")?).ok()?,
            min_confidence: u8::try_from(number("min_confidence")?).ok()?,
            window_messages: usize::try_from(number("window_messages")?).ok()?,
            judge_timeout: Duration::from_millis(number("judge_timeout_ms")?),
            max_conversations: 256,
        })
    }
}

/// 每次判断时按热配置选择：`model` 请模型判断，`quiet` 只在被叫到时接话；其他值按 `quiet`。
struct SwitchJudge {
    policy: Arc<Policy>,
    model: Arc<dyn ChimeJudge>,
}
impl ChimeJudge for SwitchJudge {
    fn judge(&self, context: ChimeContext) -> LlmFuture<'_, ChimeVerdict> {
        if self.policy.judge().as_deref() == Some("model") {
            self.model.judge(context)
        } else {
            QuietJudge.judge(context)
        }
    }
}

/// 示例宿主的出口：回复随决定一起写到标准输出，不在这里停顿或重发。
struct EchoOutlet;
impl ChimeOutlet for EchoOutlet {
    fn send<'a>(&'a self, _: &'a ChimeReply) -> LlmFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

struct AlwaysSentences;
impl eve_segment_plugin::ChatSplitting for AlwaysSentences {
    fn sentences(&self) -> bool {
        true
    }
}

pub const CHIME_HELP: &str = "用法：eve-chime [--state-dir 目录] [--database-config 文件] [--agent 文件] [--persona 名字]
从标准输入逐行读取群消息 JSON：{\"id\",\"conversation\",\"speaker\",\"speaker_name\",\"text\",\"mentions_me\",\"replies_to_me\",\"from_me\",\"at_ms\"}，
每行输出一条 JSON：本条的决定（是否说话、原因）与回复（按句分条）。被 @、被回复或被叫到名字时一定接话；
其余由 runtime.chime.judge 选择的判断器决定（model 或 quiet），并受冷却与每小时上限约束。回复照常保存在以群为会话的历史里。";

#[derive(Clone, Debug)]
pub struct ChimeOptions {
    pub state_directory: PathBuf,
    pub database_config: Option<PathBuf>,
    pub agent_path: PathBuf,
    pub persona: String,
}
impl Default for ChimeOptions {
    fn default() -> Self {
        Self {
            state_directory: ".eve-chime".into(),
            database_config: None,
            agent_path: "AGENT.md".into(),
            persona: "Eve".into(),
        }
    }
}
impl ChimeOptions {
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Option<Self>, AppError> {
        let mut args = args.into_iter();
        let mut options = Self::default();
        while let Some(arg) = args.next() {
            if arg == "--help" || arg == "-h" {
                return Ok(None);
            }
            let name = arg
                .to_str()
                .ok_or("未知启动参数；使用 --help 查看用法。")?
                .to_owned();
            if !matches!(
                name.as_str(),
                "--state-dir" | "--database-config" | "--agent" | "--persona"
            ) {
                return Err("未知启动参数；使用 --help 查看用法。".into());
            }
            let value = args.next().ok_or("启动参数缺少值。")?;
            if value.is_empty() {
                return Err("启动参数值不能为空。".into());
            }
            match name.as_str() {
                "--state-dir" => options.state_directory = value.into(),
                "--database-config" => options.database_config = Some(value.into()),
                "--agent" => options.agent_path = value.into(),
                _ => {
                    let persona = value.into_string().map_err(|_| "名字必须为 UTF-8。")?;
                    if persona.trim() != persona || persona.chars().count() > 24 {
                        return Err("名字不能有首尾空白，且不超过 24 字。".into());
                    }
                    options.persona = persona;
                }
            }
        }
        Ok(Some(options))
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ChimeSummary {
    pub messages: usize,
    pub replies: usize,
}

/// 逐行处理；一行无效只输出错误行，不影响后续消息。
pub async fn run_chime(
    options: ChimeOptions,
    input: impl BufRead,
    mut output: impl Write,
) -> Result<ChimeSummary, AppError> {
    let bootstrap: CoreBootstrap = core_bootstrap(&options.agent_path)?;
    let api_key = bootstrap.api_key.clone();
    let backends = KernelServices {
        state: storage::open_state_store(
            &options.state_directory,
            options.database_config.as_deref(),
        )?,
        ..KernelServices::default()
    };
    let registry = backends.registry.clone();
    let permissions = backends.permissions.clone();
    let logger = backends.logger.clone();
    let kernel = Kernel::with_services(backends);
    let result = async {
        let control = install_core(
            &kernel,
            registry.clone(),
            permissions,
            logger,
            &options.state_directory,
            bootstrap,
        )
        .await?;
        let settings = crate::qq_tasks::config_service(registry.as_ref())?;
        let policy = Arc::new(Policy::default());
        let _ = policy.settings.set(settings.clone());
        let model: Arc<dyn ChimeJudge> = Arc::new(ModelChimeJudge::new(Arc::new(
            crate::models::CoreModelResolver::new(settings, api_key).with_use(MODEL_USE),
        )));
        let engine = ChimeEngine::new(
            control,
            Arc::new(SwitchJudge {
                policy: policy.clone(),
                model,
            }),
            Arc::new(eve_segment_plugin::ChatPlanner::new(Arc::new(
                AlwaysSentences,
            ))),
            SEGMENT_LIMITS,
            policy,
            options.persona.clone(),
            SESSION_USER,
        );
        let mut summary = ChimeSummary::default();
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let value = match serde_json::from_str::<GroupMessage>(&line) {
                Ok(message) => {
                    summary.messages += 1;
                    let outcome = engine.observe(message, &EchoOutlet).await?;
                    if matches!(outcome.delivery, eve_chime_api::ChimeDelivery::Sent { .. }) {
                        summary.replies += 1;
                    }
                    json!({"type": "outcome", "outcome": outcome})
                }
                Err(error) => json!({"type": "error", "error": format!("群消息格式无效：{error}")}),
            };
            writeln!(output, "{value}")?;
            output.flush()?;
        }
        Ok::<_, AppError>(summary)
    }
    .await;
    finish_core(&kernel, result).await
}
