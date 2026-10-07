use crate::{
    AppError, AppFailure, MessageJudgeMode, core_bootstrap, finish_core, install_core,
    qq_cognition, qq_learning, qq_learning_commands, qq_memory, qq_memory_observer,
    segment_commands,
};
use eve_cognition_loop_api::EndogenousPlannerFactory;
use eve_cognition_loop_plugin::ReflectionPlannerFactory;
use eve_config_api::{CONFIG_SERVICE_ID, ConfigServiceHandle};
use eve_kernel::{Kernel, KernelServices};
use eve_learning_api::{
    AutoConfirmationPolicy, LEARNING_PLUGIN_ID, LearningAdmin, LearningOptions, PreferenceExtractor,
};
use eve_learning_plugin::{EvidenceConfirmationPolicy, LearningPlugin, ModelPreferenceExtractor};
use eve_llm_api::ContextAssembler;
use eve_memory_api::{MEMORY_PLUGIN_ID, MemoryAdmin};
use eve_memory_plugin::{LexicalMemoryRecall, MemoryContext, MemoryPlugin, MemoryRecallContext};
use eve_message_plugin::MessageRouterPlugin;
use eve_plugin_api::{PluginId, PluginResult, ServiceId};
use eve_qqbot_plugin::{
    DEFAULT_QQBOT_APP_ID, QQ_SEGMENT_LIMITS, QQ_SEGMENT_POLICY, QQBOT_PLUGIN_ID,
    QQBOT_STATUS_SERVICE_ID, QqBotConfig, QqBotPlugin, QqBotStatus, QqBotStatusHandle,
    QqCommandHandler, QqCommandInput,
};
use eve_segment_api::SegmentPreferences;
use eve_segment_plugin::{
    ParagraphPlanner, RuleSegmentAdvisor, SEGMENT_PREFERENCES_PLUGIN_ID, SegmentPreferencePlugin,
};
use eve_session_api::{SESSION_SERVICE_ID, SessionServiceHandle};
use eve_training_api::{TRAINING_PLUGIN_ID, TRAINING_SERVICE_ID, TrainingServiceHandle};
use eve_training_plugin::{TrainingContext, TrainingPlugin};
use std::{ffi::OsString, path::PathBuf, sync::Arc};
use tokio::sync::watch;

pub const QQBOT_HELP: &str = "Eve 官方 QQBot 通道
用法：eve-qqbot [--training] [--cognition] [--memory] [--memory-recall] [--memory-learning] [--self-learning] [--segmented] [--message-judge off|primary|jev] [--web-listen 环回IP:端口] [--learning-cooldown-ms 毫秒] [--cognition-max-executions 1至32] [--state-dir 目录] [--database-config 文件] [--agent 文件] [--node 程序] [--bridge-script 文件] [--bridge-arg 参数]
--database-config 显式选择本地 PostgreSQL；默认文件状态，已有状态目录不自动迁移。
AppID 默认 1904159860；可通过 QQBOT_APP_ID 覆盖。
必填环境：QQBOT_APP_SECRET、EVE_OPENAI_API_KEY；QQBOT_SANDBOX=true 使用测试环境。
QQ 普通文字排队开始新轮；逐行 /add 内容、/correct 内容、/cancel 控制当前任务。
--message-judge 默认 off；primary 开启在途自然消息的主模型判断，jev 先用独立 Jev 判断并至多回退主模型一次。
jev 需 EVE_JEV_API_KEY 与已启用的 runtime.models Jev 角色；接口 EVE_JEV_BASE_URL 独立配置。实验判断尚待真实语义评估。
--web-listen 127.0.0.1:8765 开启本机控制面板，默认关闭；专用 EVE_WEB_TOKEN 为 32 至 256 字节可见 ASCII，不使用模型密钥。
浏览器访问启动时打印的本机地址，输入令牌后查看会话、任务与请求取消；面板不启动新任务，不提供停服或删除入口。
--training 默认开启主动提问；/train start、/train stop、/train status 按会话启停/查询。
--cognition 开启本地内生反思；/goal 内容保存待办，/goals 查看版本，/mind [目标ID] 查询当前草稿；/goal-feedback 目标ID 版本 反馈内容触发重新评估。
--memory 开启有来源的交互记忆；/remember 内容、/memories [页码]、/correct-memory ID 内容、/forget ID、/recall 关键词。
--memory-recall 需同时 --memory；按本轮输入检索当前可信会话的已保存交互与有效偏好，最多 3 条低优先级来源片段，默认关闭。
--memory-learning 需同时 --memory；每会话至少 3 条新经历触发首批，后续默认间隔 5 分钟（--learning-cooldown-ms 可调整），单次启动最多 4 次请求。
--self-learning 开启持续自主学习（同时开启记忆、提炼和分段）；内置策略要求自评至少 80、至少两条真实交互，并复核重复、手动及撤销冲突。
/self-learning status 查看模式、已关联候选与容量；自动节奏跟随有效偏好，手动设置优先；/segment reset 清除手动设置并恢复跟随学习。
/memory-candidates [页码] 查看候选；普通提炼模式用 /accept-memory ID 确认，自主模式按策略自动确认。
/memory-decision 候选ID 查看学习决策、来源与目标版本，并核对实际保存状态。
明确偏好只用于本会话后续聊天，原始经历与修正历史保留；内部反思不读取聊天偏好。
--segmented 把模型回复按自然段分成至多 3 条消息，段间停顿至多 2.5 秒；命令确认整条发送。
/segment 查看本会话分段；/segment on|off|reset、/segment parts 2至5、/segment pace 0至200（%）按会话保存，从下一条回复生效。
/segment suggestions [页码] 从本会话已确认偏好查看节奏建议；/segment adopt 偏好ID 版本 明确采用，需同时 --memory 与 --segmented。
分段前重新核对当前代：/cancel、/add、/correct 后不再发送剩余片段；已发片段不撤回、重启不补发。
反思默认关闭，每次启动最多执行 32 项；每项一次模型请求、零工具，草稿不代表父目标完成。
修订先取消并等待；已有工具操作时只澄清，/new 内容明确开始独立任务。
Ctrl+C 或 SIGTERM 取消在途轮次、等待保存并停止桥接子进程。";
/// 密钥只在创建插件时从环境读取，不包含在启动参数和 Debug 中。
#[derive(Clone, Debug)]
pub struct QqBotOptions {
    pub state_directory: PathBuf,
    pub database_config: Option<PathBuf>,
    pub agent_path: PathBuf,
    pub node_program: OsString,
    pub bridge_script: PathBuf,
    pub bridge_args: Vec<OsString>,
    pub training: bool,
    pub cognition: bool,
    pub memory: bool,
    pub memory_recall: bool,
    pub memory_learning: bool,
    pub self_learning: bool,
    pub learning_options: LearningOptions,
    pub segmented: bool,
    pub message_judge: MessageJudgeMode,
    pub web_listen: Option<std::net::SocketAddr>,
    pub cognition_max_executions: u16,
}
impl Default for QqBotOptions {
    fn default() -> Self {
        Self {
            state_directory: ".eve".into(),
            database_config: None,
            agent_path: "AGENT.md".into(),
            node_program: "node".into(),
            bridge_script: "connectors/qqbot/bridge.mjs".into(),
            bridge_args: Vec::new(),
            training: false,
            cognition: false,
            memory: false,
            memory_recall: false,
            memory_learning: false,
            self_learning: false,
            learning_options: LearningOptions::default(),
            segmented: false,
            message_judge: MessageJudgeMode::Off,
            web_listen: None,
            cognition_max_executions: 32,
        }
    }
}
impl QqBotOptions {
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Option<Self>, AppError> {
        let mut args = args.into_iter();
        let mut options = Self::default();
        while let Some(arg) = args.next() {
            if arg == "--help" || arg == "-h" {
                return Ok(None);
            }
            if arg == "--training" {
                options.training = true;
                continue;
            }
            if arg == "--cognition" {
                options.cognition = true;
                continue;
            }
            if arg == "--memory" {
                options.memory = true;
                continue;
            }
            if arg == "--memory-recall" {
                options.memory_recall = true;
                continue;
            }
            if arg == "--memory-learning" {
                options.memory_learning = true;
                continue;
            }
            if arg == "--segmented" {
                options.segmented = true;
                continue;
            }
            if arg == "--self-learning" {
                options.self_learning = true;
                continue;
            }
            let value = args.next().ok_or("QQBot 参数缺少值")?;
            if value.is_empty() {
                return Err("QQBot 参数值不能为空".into());
            }
            match arg.to_str() {
                Some("--state-dir") => options.state_directory = value.into(),
                Some("--database-config") => options.database_config = Some(value.into()),
                Some("--agent") => options.agent_path = value.into(),
                Some("--node") => options.node_program = value,
                Some("--bridge-script") => options.bridge_script = value.into(),
                Some("--bridge-arg") => options.bridge_args.push(value),
                Some("--web-listen") => {
                    options.web_listen = Some(
                        value
                            .to_str()
                            .and_then(|value| value.parse::<std::net::SocketAddr>().ok())
                            .filter(|address| address.ip().is_loopback())
                            .ok_or("--web-listen 必须为环回 IP 与端口，例如 127.0.0.1:8765")?,
                    );
                }
                Some("--message-judge") => {
                    options.message_judge = match value.to_str() {
                        Some("off") => MessageJudgeMode::Off,
                        Some("primary") => MessageJudgeMode::Primary,
                        Some("jev") => MessageJudgeMode::Jev,
                        _ => return Err("--message-judge 必须为 off、primary 或 jev".into()),
                    };
                }
                Some("--cognition-max-executions") => {
                    options.cognition_max_executions = value
                        .to_str()
                        .and_then(|value| value.parse().ok())
                        .filter(|value| (1..=32).contains(value))
                        .ok_or("认知执行上限必须为 1 至 32 的整数")?;
                }
                Some("--learning-cooldown-ms") => {
                    options.learning_options.cooldown_ms = value
                        .to_str()
                        .and_then(|v| v.parse().ok())
                        .filter(|v| *v <= 86_400_000)
                        .ok_or("提炼间隔必须为 0 至 86400000 的毫秒整数")?;
                }
                _ => return Err("未知 QQBot 参数；使用 --help".into()),
            }
        }
        if options.self_learning {
            options.memory = true;
            options.memory_learning = true;
            options.segmented = true;
        }
        if options.memory_learning && !options.memory {
            return Err("--memory-learning 需要同时开启 --memory".into());
        }
        if options.memory_recall && !options.memory {
            return Err("--memory-recall 需要同时开启 --memory".into());
        }
        Ok(Some(options))
    }
}
async fn interrupted() -> Result<(), AppError> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
pub async fn run_qqbot(options: QqBotOptions) -> Result<QqBotStatus, AppError> {
    run_qqbot_with_planner_factory(options, Arc::new(ReflectionPlannerFactory)).await
}

/// 受信宿主的可替换规划入口；仅在显式启用认知时创建规划器。
pub async fn run_qqbot_with_planner_factory(
    options: QqBotOptions,
    factory: Arc<dyn EndogenousPlannerFactory>,
) -> Result<QqBotStatus, AppError> {
    run_qqbot_with_components(options, factory, None).await
}

/// 受信宿主替换规划和偏好提炼实现。关闭提炼时不调用所传提炼器；
/// 提炼仍受持久化准入、单次启动预算、超时与停止规则约束。
pub async fn run_qqbot_with_components(
    options: QqBotOptions,
    factory: Arc<dyn EndogenousPlannerFactory>,
    extractor: Option<Arc<dyn PreferenceExtractor>>,
) -> Result<QqBotStatus, AppError> {
    run_qqbot_with_learning_policy(options, factory, extractor, None).await
}

/// 受信宿主替换自动确认策略；只有 --self-learning 启用时调用。
pub async fn run_qqbot_with_learning_policy(
    mut options: QqBotOptions,
    factory: Arc<dyn EndogenousPlannerFactory>,
    extractor: Option<Arc<dyn PreferenceExtractor>>,
    confirmation: Option<Arc<dyn AutoConfirmationPolicy>>,
) -> Result<QqBotStatus, AppError> {
    if options.self_learning {
        options.memory = true;
        options.memory_learning = true;
        options.segmented = true;
    }
    if options.memory_learning && !options.memory {
        return Err("--memory-learning 需要同时开启 --memory".into());
    }
    if options.memory_recall && !options.memory {
        return Err("--memory-recall 需要同时开启 --memory".into());
    }
    options.learning_options.validate()?;
    let panel_config = options
        .web_listen
        .map(|address| -> Result<_, AppError> {
            let token =
                std::env::var("EVE_WEB_TOKEN").map_err(|_| "开启本机面板需要 EVE_WEB_TOKEN")?;
            let config = eve_web_panel::PanelConfig { address, token };
            config.validate()?;
            Ok(config)
        })
        .transpose()?;
    if !(1..=32).contains(&options.cognition_max_executions) {
        return Err("认知执行上限必须为 1 至 32 的整数".into());
    }
    let app_secret = std::env::var("QQBOT_APP_SECRET").map_err(|_| "缺少 QQBOT_APP_SECRET")?;
    let sandbox = match std::env::var("QQBOT_SANDBOX").as_deref() {
        Ok("true") => true,
        Ok("" | "false") | Err(_) => false,
        _ => return Err("QQBOT_SANDBOX 必须为 true 或 false".into()),
    };
    let mut bootstrap = core_bootstrap(&options.agent_path)?;
    let plugin = QqBotPlugin::new(QqBotConfig {
        node_program: options.node_program,
        bridge_script: options.bridge_script,
        bridge_args: options.bridge_args,
        app_id: std::env::var("QQBOT_APP_ID").unwrap_or_else(|_| DEFAULT_QQBOT_APP_ID.into()),
        app_secret,
        sandbox,
    })?
    .with_training()?
    .with_natural_message_judgement(options.message_judge != MessageJudgeMode::Off);
    let plugin = if options.segmented {
        plugin.with_segmenter(Arc::new(ParagraphPlanner::default()), QQ_SEGMENT_LIMITS)?
    } else {
        plugin
    };
    let backends = KernelServices {
        state: crate::storage::open_state_store(
            &options.state_directory,
            options.database_config.as_deref(),
        )?,
        ..KernelServices::default()
    };
    let registry = backends.registry.clone();
    let permissions = backends.permissions.clone();
    let logger = backends.logger.clone();
    let kernel = Kernel::with_services(KernelServices {
        events: backends.events.clone(),
        registry: backends.registry.clone(),
        state: backends.state.clone(),
        permissions: backends.permissions.clone(),
        tasks: backends.tasks.clone(),
        logger: backends.logger.clone(),
    });
    let mut background: Option<qq_cognition::Background> = None;
    let mut learning_background: Option<qq_learning::Background> = None;
    let mut channel: Option<Arc<QqBotStatusHandle>> = None;
    let mut panel: Option<eve_web_panel::LocalPanel> = None;
    let result: Result<(), AppError> = async {
        kernel.register(Box::new(TrainingPlugin::new(options.training)?))?;
        kernel.start(&PluginId::new(TRAINING_PLUGIN_ID)?).await?;
        // 分段设置先于模型与通道加载；损坏或版本不兼容时在这里拒绝启动并保留原字节。
        let segment_preferences: Option<Arc<dyn SegmentPreferences>> = if options.segmented {
            let plugin = SegmentPreferencePlugin::new()?;
            let controller = plugin.controller();
            kernel.register(Box::new(plugin))?;
            kernel
                .start(&PluginId::new(SEGMENT_PREFERENCES_PLUGIN_ID)?)
                .await?;
            Some(Arc::new(controller))
        } else {
            None
        };
        let training = registry
            .get(&ServiceId::new(TRAINING_SERVICE_ID)?)?
            .ok_or("训练服务缺失")?
            .value
            .downcast::<TrainingServiceHandle>()
            .map_err(|_| "训练服务类型错误")?;
        let memory: Option<Arc<dyn MemoryAdmin>> = if options.memory {
            let plugin = MemoryPlugin::new()?;
            let controller = plugin.controller();
            kernel.register(Box::new(plugin))?;
            kernel.start(&PluginId::new(MEMORY_PLUGIN_ID)?).await?;
            Some(Arc::new(controller))
        } else {
            None
        };
        let context: Arc<dyn ContextAssembler> = Arc::new(TrainingContext(training.0.clone()));
        let learning: Option<Arc<dyn LearningAdmin>> = if options.memory_learning {
            let plugin = LearningPlugin::new()?;
            let controller = plugin.controller();
            kernel.register(Box::new(plugin))?;
            kernel.start(&PluginId::new(LEARNING_PLUGIN_ID)?).await?;
            Some(Arc::new(controller))
        } else {
            None
        };
        let context: Arc<dyn ContextAssembler> = if let Some(memory) = &memory {
            let context = MemoryContext::new("qq", memory.clone(), context)?;
            Arc::new(if options.self_learning {
                context.prefer_recent()
            } else {
                context
            })
        } else {
            context
        };
        bootstrap.context = Some(if options.memory_recall {
            let memory = memory.clone().ok_or("记忆召回缺少记忆服务")?;
            Arc::new(MemoryRecallContext::new(
                "qq",
                Arc::new(LexicalMemoryRecall::new(memory)),
                context,
            )?)
        } else {
            context
        });
        let control = install_core(
            &kernel,
            registry.clone(),
            permissions,
            logger,
            &options.state_directory,
            bootstrap,
        )
        .await?;
        let learning_commands = if let (Some(learning), Some(memory)) = (&learning, &memory) {
            let extractor = match extractor {
                Some(extractor) => extractor,
                None => {
                    let settings = registry
                        .get(&ServiceId::new(CONFIG_SERVICE_ID)?)?
                        .ok_or("偏好提炼配置服务缺失")?
                        .value
                        .downcast::<ConfigServiceHandle>()
                        .map_err(|_| "偏好提炼配置服务类型错误")?;
                    let key = std::env::var("EVE_OPENAI_API_KEY").map_err(|_| "缺少模型凭据")?;
                    let resolver = Arc::new(crate::models::CoreModelResolver::new(
                        settings.0.clone(),
                        key,
                    ));
                    Arc::new(ModelPreferenceExtractor::new(resolver))
                }
            };
            learning_background = Some(qq_learning::Background::start(
                memory.clone(),
                learning.clone(),
                extractor,
                options.learning_options.clone(),
                if options.self_learning {
                    Some(confirmation.unwrap_or_else(|| Arc::new(EvidenceConfirmationPolicy)))
                } else {
                    None
                },
            )?);
            if options.self_learning {
                qq_learning_commands::Commands::autonomous(learning.clone(), memory.clone())
            } else {
                qq_learning_commands::Commands::new(learning.clone(), memory.clone())
            }
        } else {
            qq_learning_commands::Commands::disabled()
        };
        let commands = if options.cognition {
            let started = qq_cognition::start(
                &kernel,
                &backends,
                &options.agent_path,
                options.cognition_max_executions,
                factory,
            )
            .await?;
            let commands = started.commands.clone();
            background = Some(started);
            commands
        } else {
            qq_cognition::Commands::disabled()
        };
        let memory_commands = memory
            .as_ref()
            .map_or_else(qq_memory::Commands::disabled, |memory| {
                qq_memory::Commands::new(memory.clone())
            });
        let segment_preferences = if options.self_learning {
            Some(Arc::new(crate::segment_advice::AutomaticPreferences {
                manual: segment_preferences.ok_or("自主学习缺少分段设置")?,
                memory: memory.clone().ok_or("自主学习缺少记忆")?,
                advisor: Arc::new(RuleSegmentAdvisor),
                policy: QQ_SEGMENT_POLICY,
            }) as Arc<dyn SegmentPreferences>)
        } else {
            segment_preferences
        };
        let segment_commands = segment_preferences.clone().map_or_else(
            segment_commands::QqCommands::disabled,
            |store| match &memory {
                Some(memory) => segment_commands::QqCommands::new_with_advice(
                    store,
                    QQ_SEGMENT_POLICY,
                    memory.clone(),
                    Arc::new(RuleSegmentAdvisor),
                    options.self_learning,
                ),
                None => segment_commands::QqCommands::new(store, QQ_SEGMENT_POLICY),
            },
        );
        let mut plugin = plugin.with_command_handler(Arc::new(CommandHandlers(vec![
            commands,
            memory_commands,
            learning_commands,
            segment_commands,
        ])));
        if let Some(store) = segment_preferences {
            plugin = plugin.with_segment_preferences(store, QQ_SEGMENT_POLICY)?;
        }
        let panel_memory = memory.clone();
        if let Some(memory) = memory {
            let sessions = registry
                .get(&ServiceId::new(SESSION_SERVICE_ID)?)?
                .ok_or("交互记忆需要会话服务")?
                .value
                .downcast::<SessionServiceHandle>()
                .map_err(|_| "交互记忆会话服务类型错误")?;
            plugin = plugin.with_interaction_observer(Arc::new(qq_memory_observer::Observer {
                memory,
                sessions: sessions.0.clone(),
            }))?;
        }
        let settings = registry
            .get(&ServiceId::new(CONFIG_SERVICE_ID)?)?
            .ok_or("消息判断配置服务缺失")?
            .value
            .downcast::<ConfigServiceHandle>()
            .map_err(|_| "消息判断配置服务类型错误")?;
        let relation =
            crate::qq_message_judge::relation_plugin(options.message_judge, settings.0.clone())?;
        // 只有开启本机面板时才记录实时判断；记录只在内存中，重启后清空。
        let judgments = panel_config
            .as_ref()
            .map(|_| Arc::new(eve_message_diagnostics::RecentRelationJudgments::default()));
        let relation = match &judgments {
            Some(recent) => {
                let recent = recent.clone();
                relation.with_judge_decorator(Arc::new(move |judge| {
                    Arc::new(eve_message_diagnostics::RecordingRelationJudge::new(
                        judge,
                        recent.clone(),
                    ))
                }))
            }
            None => relation,
        };
        kernel.register(Box::new(relation))?;
        kernel.register(Box::new(MessageRouterPlugin::builtin()?))?;
        kernel.register(Box::new(plugin))?;
        kernel.start(&PluginId::new(QQBOT_PLUGIN_ID)?).await?;
        let handle = registry
            .get(&ServiceId::new(QQBOT_STATUS_SERVICE_ID)?)?
            .ok_or("QQBot 状态服务缺失")?
            .value
            .downcast::<QqBotStatusHandle>()
            .map_err(|_| "QQBot 状态服务类型错误")?;
        channel = Some(handle.clone());
        if let Some(config) = panel_config {
            let sessions = registry
                .get(&ServiceId::new(SESSION_SERVICE_ID)?)?
                .ok_or("面板会话服务缺失")?
                .value
                .downcast::<SessionServiceHandle>()
                .map_err(|_| "面板会话服务类型错误")?;
            let started_at_unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis()
                .try_into()
                .map_err(|_| "系统时间超出面板记录范围")?;
            let started = eve_web_panel::LocalPanel::bind(
                config,
                Arc::new(crate::web_panel::QqPanel {
                    sessions: sessions.0.clone(),
                    control,
                    channel: handle.clone(),
                    started_at_unix_ms,
                    judgments: judgments
                        .clone()
                        .map(|recent| (recent, options.message_judge.name())),
                    cognition: background
                        .as_ref()
                        .map(qq_cognition::Background::reader)
                        .transpose()?,
                    memory: panel_memory.map(crate::web_panel_memory::MemoryView::new),
                    learning: learning.clone().map(|admin| {
                        crate::web_panel_learning::LearningRead::new(admin, options.self_learning)
                    }),
                }),
            )
            .await?;
            eprintln!("EVE_WEB_READY http://{}", started.address());
            panel = Some(started);
        }
        if let Some(background) = &background {
            background.activate();
        }
        if let Some(learning) = &learning_background {
            learning.activate();
        }
        wait_channel(
            handle.status.clone(),
            background.as_ref().map(qq_cognition::Background::finished),
            learning_background
                .as_ref()
                .map(qq_learning::Background::finished),
            panel.as_ref().map(eve_web_panel::LocalPanel::finished),
        )
        .await
    }
    .await;
    // 先关闭通道准入并结束反思执行，再等待桥接收尾，最后进入 Kernel 生命周期写准入。
    // 启动失败、后台异常、EOF 和系统信号全部经过这里。
    let mut secondary = Vec::<AppError>::new();
    // 先撤销管理入口，再收尾 QQ 和 Kernel；HTTP 请求不拥有后台轮次。
    if let Some(panel) = panel.take() {
        panel.request_stop();
        if let Err(error) = panel.stop().await {
            secondary.push(error.into());
        }
    }
    if channel.is_none() {
        match registry.get(&ServiceId::new(QQBOT_STATUS_SERVICE_ID).expect("有效 QQ 状态 ID")) {
            Ok(Some(entry)) => match entry.value.downcast::<QqBotStatusHandle>() {
                Ok(handle) => channel = Some(handle),
                Err(_) => secondary.push("QQBot 收尾状态服务类型错误".into()),
            },
            Ok(None) => {}
            Err(error) => secondary.push(error.into()),
        }
    }
    if let Some(handle) = &channel {
        handle.request_stop();
    }
    if let Some(learning) = &learning_background {
        learning.request_stop();
    }
    if let Some(background) = background
        && let Err(error) = background.stop().await
    {
        secondary.push(error);
    }
    if let Some(learning) = learning_background
        && let Err(error) = learning.stop().await
    {
        secondary.push(error);
    }
    let summary = if let Some(handle) = channel {
        let mut status = handle.status.clone();
        while !status.borrow().closed {
            if status.changed().await.is_err() {
                secondary.push("QQBot 收尾通知丢失".into());
                break;
            }
        }
        let summary = *status.borrow();
        if summary.terminal_error {
            secondary.push("QQBot 通道异常结束；状态已保留".into());
        }
        summary
    } else {
        QqBotStatus::default()
    };
    let result = match (result, secondary.is_empty()) {
        (Ok(()), true) => Ok(summary),
        (Ok(()), false) => {
            let primary = secondary.remove(0);
            Err(Box::new(AppFailure { primary, secondary }) as AppError)
        }
        (Err(primary), _) => Err(Box::new(AppFailure { primary, secondary }) as AppError),
    };
    finish_core(&kernel, result).await
}

struct CommandHandlers(Vec<Arc<dyn QqCommandHandler>>);
impl QqCommandHandler for CommandHandlers {
    fn handle(&self, input: QqCommandInput<'_>) -> PluginResult<Option<String>> {
        for handler in &self.0 {
            if let Some(reply) = handler.handle(QqCommandInput {
                message_id: input.message_id,
                session: input.session,
                text: input.text,
            })? {
                return Ok(Some(reply));
            }
        }
        Ok(None)
    }
}

async fn background_finished(mut receiver: Option<watch::Receiver<bool>>) {
    let Some(receiver) = &mut receiver else {
        std::future::pending::<()>().await;
        return;
    };
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

async fn wait_channel(
    mut status: watch::Receiver<QqBotStatus>,
    background: Option<watch::Receiver<bool>>,
    learning: Option<watch::Receiver<bool>>,
    panel: Option<watch::Receiver<bool>>,
) -> Result<(), AppError> {
    let stop = interrupted();
    let stopped_background = background_finished(background);
    let stopped_learning = background_finished(learning);
    let stopped_panel = background_finished(panel);
    tokio::pin!(stop, stopped_background, stopped_learning, stopped_panel);
    let mut ready_announced = false;
    loop {
        if status.borrow().ready && !status.borrow().closed && !ready_announced {
            eprintln!("EVE_QQBOT_READY");
            ready_announced = true;
        }
        if status.borrow().closed {
            return Ok(());
        }
        tokio::select! {
            biased;
            _ = &mut stopped_background => return Err("认知后台已结束；QQ 通道停止准入并保留状态".into()),
            _ = &mut stopped_learning => return Err("偏好提炼后台已结束；QQ 通道停止准入并保留状态".into()),
            _ = &mut stopped_panel => return Err("本机面板异常结束；QQ 通道停止准入并保留状态".into()),
            result = &mut stop => return result,
            result = status.changed() => result.map_err(|_| "QQBot 状态通知丢失")?,
        }
    }
}

#[cfg(test)]
mod recall_options_tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<QqBotOptions, AppError> {
        Ok(QqBotOptions::parse(args.iter().map(OsString::from))?.unwrap())
    }

    #[test]
    fn recall_requires_explicit_opt_in_even_for_autonomous_learning() {
        for args in [
            vec![],
            vec!["--memory"],
            vec!["--self-learning"],
            vec!["--memory", "--memory-learning"],
        ] {
            assert!(!parse(&args).unwrap().memory_recall);
        }
        assert!(parse(&["--memory-recall"]).is_err());
        for args in [
            vec!["--memory", "--memory-recall"],
            vec!["--memory-recall", "--memory"],
            vec!["--self-learning", "--memory-recall"],
        ] {
            let options = parse(&args).unwrap();
            assert!(options.memory && options.memory_recall);
        }
        assert!(parse(&["--memory-recall", "true"]).is_err());
    }

    #[tokio::test]
    async fn direct_host_options_reject_recall_without_memory_before_opening_state() {
        let directory = tempfile::tempdir().unwrap();
        let state_directory = directory.path().join("state-not-created");
        let result = run_qqbot(QqBotOptions {
            memory_recall: true,
            state_directory: state_directory.clone(),
            ..QqBotOptions::default()
        })
        .await;
        assert!(result.unwrap_err().to_string().contains("--memory"));
        assert!(!state_directory.exists());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<QqBotOptions, AppError> {
        Ok(QqBotOptions::parse(args.iter().map(OsString::from))?.unwrap())
    }

    #[test]
    fn natural_message_judgement_requires_explicit_mode() {
        assert_eq!(parse(&[]).unwrap().message_judge, MessageJudgeMode::Off);
        assert_eq!(
            parse(&["--self-learning"]).unwrap().message_judge,
            MessageJudgeMode::Off
        );
        for (name, expected) in [
            ("off", MessageJudgeMode::Off),
            ("primary", MessageJudgeMode::Primary),
            ("jev", MessageJudgeMode::Jev),
        ] {
            assert_eq!(
                parse(&["--message-judge", name]).unwrap().message_judge,
                expected
            );
        }
        for args in [
            vec!["--message-judge"],
            vec!["--message-judge", ""],
            vec!["--message-judge", "auto"],
        ] {
            assert!(parse(&args).is_err());
        }
    }

    #[test]
    fn autonomous_learning_is_explicit_and_composes_required_capabilities() {
        assert!(!parse(&[]).unwrap().self_learning);
        for cooldown in ["0", "86400000"] {
            assert_eq!(
                parse(&["--self-learning", "--learning-cooldown-ms", cooldown])
                    .unwrap()
                    .learning_options
                    .cooldown_ms,
                cooldown.parse::<u64>().unwrap()
            );
        }
        for cooldown in ["-1", "86400001", "bad"] {
            assert!(parse(&["--learning-cooldown-ms", cooldown]).is_err());
        }
        for args in [
            vec!["--self-learning"],
            vec!["--memory-learning", "--self-learning"],
        ] {
            let options = parse(&args).unwrap();
            assert!(
                options.self_learning
                    && options.memory
                    && options.memory_learning
                    && options.segmented
            );
            assert!(!options.cognition && !options.training);
        }
    }

    #[test]
    fn cognition_is_explicit_and_has_bounded_independent_budget() {
        let options = parse(&[]).unwrap();
        assert!(!options.cognition);
        assert!(!options.training);
        assert_eq!(options.cognition_max_executions, 32);
        let options = parse(&[
            "--training",
            "--cognition",
            "--cognition-max-executions",
            "1",
        ])
        .unwrap();
        assert!(options.cognition && options.training);
        assert_eq!(options.cognition_max_executions, 1);
        let options = parse(&["--cognition-max-executions", "32"]).unwrap();
        assert!(!options.cognition);
        assert_eq!(options.cognition_max_executions, 32);
    }

    #[test]
    fn invalid_cognition_budget_is_rejected_before_startup() {
        for value in ["0", "33", "-1", "65536", "1.5", "one", ""] {
            assert!(parse(&["--cognition-max-executions", value]).is_err());
        }
        assert!(parse(&["--cognition-max-executions"]).is_err());
        assert!(
            QqBotOptions::parse([OsString::from("--help")])
                .unwrap()
                .is_none()
        );
    }
}
