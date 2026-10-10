use crate::{
    AppError, AppFailure, MessageJudgeMode, core_bootstrap, finish_core, qq_cognition, qq_interest,
    qq_learning, qq_learning_commands, qq_memory, qq_memory_observer, qq_outreach, qq_practice,
    qq_research, qq_skill, segment_commands,
};
use eve_cognition_loop_api::EndogenousPlannerFactory;
use eve_cognition_loop_plugin::ReflectionPlannerFactory;
use eve_config_api::{CONFIG_SERVICE_ID, ConfigServiceHandle};
use eve_interest_api::{INTEREST_PLUGIN_ID, InterestObserver, ObservationOptions};
use eve_interest_plugin::{InterestPlugin, LearningGoalDeriver, ModelInterestObserver};
use eve_kernel::{Kernel, KernelServices};
use eve_knowledge_api::{
    KNOWLEDGE_PLUGIN_ID, KnowledgeAdmin, KnowledgeExtractor, SourceFetcher, SourcePolicy,
    SourceSelector,
};
use eve_knowledge_plugin::{
    HttpSourceFetcher, KnowledgePlugin, ModelKnowledgeExtractor, ModelSourceSelector, Researcher,
};
use eve_learning_api::{
    AutoConfirmationPolicy, LEARNING_PLUGIN_ID, LearningAdmin, LearningOptions, PreferenceExtractor,
};
use eve_learning_plugin::{EvidenceConfirmationPolicy, LearningPlugin, ModelPreferenceExtractor};
use eve_llm_api::ContextAssembler;
use eve_memory_api::{MEMORY_PLUGIN_ID, MemoryAdmin};
use eve_memory_plugin::{LexicalMemoryRecall, MemoryContext, MemoryPlugin, MemoryRecallContext};
use eve_message_plugin::MessageRouterPlugin;
use eve_outreach_api::{
    InvitationComposer, OUTREACH_PLUGIN_ID, OutreachAdmin, OutreachPolicy, ResponseJudge,
    TimingJudge,
};
use eve_outreach_plugin::{
    ModelInvitationComposer, ModelResponseJudge, ModelTimingJudge, OutreachPlugin, RequestGoals,
};
use eve_plugin_api::{PluginId, PluginResult, ServiceId};
use eve_practice_api::{PRACTICE_PLUGIN_ID, PracticeDrafter, PracticeRunner};
use eve_practice_browser::BrowserRunner;
use eve_practice_mindustry::{MindustryServerRunner, RuntimeCommand};
use eve_practice_plugin::{ModelPracticeDrafter, PracticePlugin, Practitioner};
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
use eve_skill_api::{SKILL_PLUGIN_ID, SkillAdmin, SkillDistiller, SkillSelector};
use eve_skill_plugin::{
    Consolidator, ModelSkillDistiller, ModelSkillSelector, SkillAwareDrafter, SkillPlugin,
    SkillTool,
};
use eve_training_api::{TRAINING_PLUGIN_ID, TRAINING_SERVICE_ID, TrainingServiceHandle};
use eve_training_plugin::{TrainingContext, TrainingPlugin};
use std::{ffi::OsString, path::PathBuf, sync::Arc};
use tokio::sync::watch;

pub const QQBOT_HELP: &str = "Eve 官方 QQBot 通道
用法：eve-qqbot [--training] [--cognition] [--memory] [--memory-recall] [--memory-learning] [--self-learning] [--interest-learning] [--research-source URL]... [--practice-mindustry-server jar] [--practice-java 程序] [--practice-java-arg 参数]... [--practice-browser 程序] [--skill-learning] [--outreach] [--outreach-cooldown-ms 毫秒] [--outreach-proactive-after-ms 毫秒] [--segmented] [--message-judge off|primary|jev] [--web-listen 环回IP:端口] [--learning-cooldown-ms 毫秒] [--interest-cooldown-ms 毫秒] [--cognition-max-executions 1至32] [--state-dir 目录] [--database-config 文件] [--agent 文件] [--node 程序] [--bridge-script 文件] [--bridge-arg 参数]
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
--interest-learning 从普通聊天观察用户明确表达的兴趣、经验与困难（同时开启记忆和认知），只保存可逐字核对的原话，并派生低优先级的后台学习目标；同一会话默认间隔 5 分钟（--interest-cooldown-ms 可调整），每批一次无工具请求。
/interests 查看本会话记录的兴趣与学习目标；/forget-interest 兴趣ID 撤回兴趣并取消对应学习目标。
--research-source 需同时 --interest-learning，可重复至多 8 个；为等待中的学习目标在这些入口页面的同源目录内受控研究（只读 GET，每个目标修订至多一次，每个目标累计至多 3 次）。
/knowledge 兴趣ID 查看研究到的资料：来源原文附网址、抓取时间、版本与逐字引用，未验证推测单独标注；研究不重试、中断不重放。
--practice-mindustry-server 需同时 --interest-learning；为等待中的学习目标制作只含数据文件的最小 Mindustry 模组，用操作者提供的无头服务端 jar 在全新目录中实际加载并探测内容属性（--practice-java 默认 java），每个目标修订至多一次、每次至多三次尝试，只有实际加载且全部探测通过才记为已验证。
--practice-browser 需同时 --interest-learning，与 --practice-mindustry-server 二选一；为等待中的学习目标制作只含 HTML 与 CSS 的静态网页，用操作者提供的无头 Chromium 在全新目录中实际打开并探测元素的计算样式与文字（不能含脚本或外部资源，不访问网络），其余规则同上。
/practice 兴趣ID 查看实践记录：每次尝试的产物文件、运行版本、加载状态、警告与探测期望/实际值；中断不重放。
--skill-learning 需同时指定实践运行环境（--practice-mindustry-server 或 --practice-browser）；把实际验证通过的实践提炼为参数化技能，宿主核对能逐字还原原产物，再用与原值不同的参数在同一运行环境中实际运行通过后自动启用；后续任务的第一次尝试可选用已启用的技能，调用结果以实际运行证据为准。
--outreach 需同时指定实践运行环境（--practice-mindustry-server 或 --practice-browser）；学习目标有了实际验证的进展后撰写一条邀请（只用账本中的用户原话、实践证据与已启用技能），在用户下次私聊找 Eve 时先判断此刻是否合适（看用户这条消息与回复），合适才随被动回复附带，以平台回执为准；同一用户默认 24 小时内至多送达一条（--outreach-cooldown-ms 可调整），群聊不附带。
--outreach-proactive-after-ms 需同时 --outreach；邀请在被动窗口等待超过该时间仍未送达时主动私聊一次（受平台配额与用户开关限制，被拒绝时保持待投递）。
/outreach 查看邀请状态与回执；/outreach off 请 Eve 不再主动提起，/outreach on 恢复。
/skills 列出技能；/skill 技能ID 查看版本、验证证据、启用记录与调用；/skill disable|rollback 技能ID、/skill enable 技能ID 版本 停用、回退或启用某个已验证版本。
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
    pub interest_learning: bool,
    pub interest_options: ObservationOptions,
    /// 受控研究的入口页面；为空表示不研究。
    pub research_sources: Vec<String>,
    /// 实践验证使用的 Mindustry 无头服务端 jar；为空表示不实践。
    pub practice_server_jar: Option<PathBuf>,
    /// 静态网页实践使用的无头 Chromium 程序；与 Mindustry 服务端二选一。
    pub practice_browser: Option<PathBuf>,
    /// 启动运行环境的程序；未显式提供时为 java。
    pub practice_java: Option<OsString>,
    pub practice_java_args: Vec<OsString>,
    /// 把已验证的实践固化为技能并在后续任务中复用；需要实践验证。
    pub skill_learning: bool,
    /// 学习目标取得实际验证的进展后择机邀请用户；需要实践验证。
    pub outreach: bool,
    /// 同一用户两次送达之间的最短间隔；未提供时为 24 小时。
    pub outreach_cooldown_ms: Option<u64>,
    /// 邀请在被动窗口等待多久后主动私聊；未提供时不主动私聊。
    pub outreach_proactive_after_ms: Option<u64>,
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
            interest_learning: false,
            interest_options: ObservationOptions::default(),
            research_sources: Vec::new(),
            practice_server_jar: None,
            practice_browser: None,
            practice_java: None,
            practice_java_args: Vec::new(),
            skill_learning: false,
            outreach: false,
            outreach_cooldown_ms: None,
            outreach_proactive_after_ms: None,
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
            if arg == "--interest-learning" {
                options.interest_learning = true;
                continue;
            }
            if arg == "--skill-learning" {
                options.skill_learning = true;
                continue;
            }
            if arg == "--outreach" {
                options.outreach = true;
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
                Some("--practice-browser") => {
                    options.practice_browser = Some(value.into());
                }
                Some("--practice-mindustry-server") => {
                    options.practice_server_jar = Some(value.into());
                }
                Some("--practice-java") => options.practice_java = Some(value),
                Some("--practice-java-arg") => options.practice_java_args.push(value),
                Some("--research-source") => options.research_sources.push(
                    value
                        .into_string()
                        .map_err(|_| "研究入口页面必须为 UTF-8 URL")?,
                ),
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
                Some("--outreach-cooldown-ms") => {
                    options.outreach_cooldown_ms = Some(
                        value
                            .to_str()
                            .and_then(|v| v.parse().ok())
                            .filter(|v| *v <= 30 * 86_400_000)
                            .ok_or("邀请冷却必须为 0 至 2592000000 的毫秒整数")?,
                    );
                }
                Some("--outreach-proactive-after-ms") => {
                    options.outreach_proactive_after_ms = Some(
                        value
                            .to_str()
                            .and_then(|v| v.parse().ok())
                            .filter(|v| *v <= 30 * 86_400_000)
                            .ok_or("主动私聊等待必须为 0 至 2592000000 的毫秒整数")?,
                    );
                }
                Some("--interest-cooldown-ms") => {
                    options.interest_options.cooldown_ms = value
                        .to_str()
                        .and_then(|v| v.parse().ok())
                        .filter(|v| *v <= 86_400_000)
                        .ok_or("兴趣观察间隔必须为 0 至 86400000 的毫秒整数")?;
                }
                _ => return Err("未知 QQBot 参数；使用 --help".into()),
            }
        }
        if options.self_learning {
            options.memory = true;
            options.memory_learning = true;
            options.segmented = true;
        }
        if options.interest_learning {
            options.memory = true;
            options.cognition = true;
        }
        if options.memory_learning && !options.memory {
            return Err("--memory-learning 需要同时开启 --memory".into());
        }
        if options.memory_recall && !options.memory {
            return Err("--memory-recall 需要同时开启 --memory".into());
        }
        research_policy(&options)?;
        if options.practice_server_jar.is_none()
            && (options.practice_java.is_some() || !options.practice_java_args.is_empty())
        {
            return Err("--practice-java 需要同时指定 --practice-mindustry-server".into());
        }
        if options.practice_server_jar.is_some() && !options.interest_learning {
            return Err("--practice-mindustry-server 需要同时开启 --interest-learning".into());
        }
        if options.practice_browser.is_some() && !options.interest_learning {
            return Err("--practice-browser 需要同时开启 --interest-learning".into());
        }
        if options.practice_server_jar.is_some() && options.practice_browser.is_some() {
            return Err(
                "--practice-mindustry-server 与 --practice-browser 只能选一个实践运行环境".into(),
            );
        }
        let runtime = options.practice_server_jar.is_some() || options.practice_browser.is_some();
        if options.skill_learning && !runtime {
            return Err("--skill-learning 需要同时指定实践运行环境（--practice-mindustry-server 或 --practice-browser）".into());
        }
        if (options.outreach_cooldown_ms.is_some() || options.outreach_proactive_after_ms.is_some())
            && !options.outreach
        {
            return Err(
                "--outreach-cooldown-ms 与 --outreach-proactive-after-ms 需要同时开启 --outreach"
                    .into(),
            );
        }
        if options.outreach && !runtime {
            return Err("--outreach 需要同时指定实践运行环境（--practice-mindustry-server 或 --practice-browser）".into());
        }
        Ok(Some(options))
    }
}
/// 研究入口页面只在显式开启兴趣学习时生效；校验不发起网络请求。
fn research_policy(options: &QqBotOptions) -> Result<Option<SourcePolicy>, AppError> {
    if options.research_sources.is_empty() {
        return Ok(None);
    }
    if !options.interest_learning {
        return Err("--research-source 需要同时开启 --interest-learning".into());
    }
    Ok(Some(SourcePolicy::new(&options.research_sources).map_err(
        |_| "研究入口页面无效：至多 8 个互不重复、不含用户信息与片段的 http/https URL",
    )?))
}

/// 兴趣学习的可替换实现；未提供的部分使用默认的模型观察器、HTTP 抓取器与模型选择/提炼器。
#[derive(Clone, Default)]
pub struct InterestComponents {
    pub observer: Option<Arc<dyn InterestObserver>>,
    pub fetcher: Option<Arc<dyn SourceFetcher>>,
    pub selector: Option<Arc<dyn SourceSelector>>,
    pub extractor: Option<Arc<dyn KnowledgeExtractor>>,
    /// 替换实践草稿器；默认使用主模型的单次无工具请求。
    pub drafter: Option<Arc<dyn PracticeDrafter>>,
    /// 替换实践运行器；提供时即使未配置服务端 jar 也开启实践。
    pub runner: Option<Arc<dyn PracticeRunner>>,
    /// 替换技能提炼器与选择器；默认各使用主模型的单次无工具请求。是否固化技能仍由 --skill-learning 决定。
    pub distiller: Option<Arc<dyn SkillDistiller>>,
    pub skill_selector: Option<Arc<dyn SkillSelector>>,
    /// 替换邀请撰写器、时机判断器与回应识别器；默认各使用主模型的单次无工具请求。
    /// 是否主动交流仍由 --outreach 决定。
    pub composer: Option<Arc<dyn InvitationComposer>>,
    pub timing_judge: Option<Arc<dyn TimingJudge>>,
    pub response_judge: Option<Arc<dyn ResponseJudge>>,
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
    options: QqBotOptions,
    factory: Arc<dyn EndogenousPlannerFactory>,
    extractor: Option<Arc<dyn PreferenceExtractor>>,
    confirmation: Option<Arc<dyn AutoConfirmationPolicy>>,
) -> Result<QqBotStatus, AppError> {
    run_qqbot_composed(
        options,
        factory,
        extractor,
        confirmation,
        InterestComponents::default(),
    )
    .await
}

/// 受信宿主替换兴趣观察器；只有 --interest-learning 启用时调用。观察仍受持久化准入、
/// 超时、停止规则与原话逐字核对约束，目标派生不调用模型。
pub async fn run_qqbot_with_interest_observer(
    options: QqBotOptions,
    factory: Arc<dyn EndogenousPlannerFactory>,
    observer: Option<Arc<dyn InterestObserver>>,
) -> Result<QqBotStatus, AppError> {
    run_qqbot_with_interest_components(
        options,
        factory,
        InterestComponents {
            observer,
            ..InterestComponents::default()
        },
    )
    .await
}

/// 受信宿主替换兴趣观察与受控研究的实现。研究只在配置 --research-source 时运行，
/// 仍受来源范围、持久化准入、每个目标修订一次、累计次数、超时与停止规则约束。
pub async fn run_qqbot_with_interest_components(
    options: QqBotOptions,
    factory: Arc<dyn EndogenousPlannerFactory>,
    components: InterestComponents,
) -> Result<QqBotStatus, AppError> {
    run_qqbot_composed(options, factory, None, None, components).await
}

async fn run_qqbot_composed(
    mut options: QqBotOptions,
    factory: Arc<dyn EndogenousPlannerFactory>,
    extractor: Option<Arc<dyn PreferenceExtractor>>,
    confirmation: Option<Arc<dyn AutoConfirmationPolicy>>,
    interest_components: InterestComponents,
) -> Result<QqBotStatus, AppError> {
    if options.self_learning {
        options.memory = true;
        options.memory_learning = true;
        options.segmented = true;
    }
    if options.interest_learning {
        options.memory = true;
        options.cognition = true;
    }
    if options.memory_learning && !options.memory {
        return Err("--memory-learning 需要同时开启 --memory".into());
    }
    if options.memory_recall && !options.memory {
        return Err("--memory-recall 需要同时开启 --memory".into());
    }
    options.learning_options.validate()?;
    options.interest_options.validate()?;
    let research_policy = research_policy(&options)?;
    let practice_enabled = options.practice_server_jar.is_some()
        || options.practice_browser.is_some()
        || interest_components.runner.is_some();
    if practice_enabled && !options.interest_learning {
        return Err("实践验证需要同时开启 --interest-learning".into());
    }
    if options.skill_learning && !practice_enabled {
        return Err("技能固化需要同时开启实践验证".into());
    }
    if options.outreach && !practice_enabled {
        return Err("主动交流需要同时开启实践验证".into());
    }
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
    let mut interest_background: Option<qq_interest::Background> = None;
    let mut research_background: Option<qq_research::Background> = None;
    let mut practice_background: Option<qq_practice::Background> = None;
    let mut outreach_background: Option<qq_outreach::Background> = None;
    let mut channel: Option<Arc<QqBotStatusHandle>> = None;
    let mut panel: Option<eve_web_panel::LocalPanel> = None;
    let page_permit = eve_web_panel_api::PageWritePermit::default();
    // 插件只能弱引用此宿主诊断句柄，避免 QQ 插件与 Kernel 形成引用环。
    let runtime_inspector: Arc<dyn eve_plugin_api::RuntimeInspector> = Arc::new(kernel.clone());
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
        // 兴趣账本先于模型与通道加载；损坏或版本不兼容时拒绝启动并保留原字节。
        let interests = if options.interest_learning {
            let plugin = InterestPlugin::new()?;
            let controller = plugin.controller();
            kernel.register(Box::new(plugin))?;
            kernel.start(&PluginId::new(INTEREST_PLUGIN_ID)?).await?;
            Some(controller)
        } else {
            None
        };
        // 知识账本同样先于通道加载；损坏时拒绝启动并保留原字节。
        let knowledge = if research_policy.is_some() {
            let plugin = KnowledgePlugin::new()?;
            let controller = plugin.controller();
            kernel.register(Box::new(plugin))?;
            kernel.start(&PluginId::new(KNOWLEDGE_PLUGIN_ID)?).await?;
            Some(controller)
        } else {
            None
        };
        // 实践账本同样先于通道加载；损坏时拒绝启动并保留原字节。
        let practice = if practice_enabled {
            let plugin = PracticePlugin::new()?;
            let controller = plugin.controller();
            kernel.register(Box::new(plugin))?;
            kernel.start(&PluginId::new(PRACTICE_PLUGIN_ID)?).await?;
            Some(controller)
        } else {
            None
        };
        // 技能账本同样先于通道加载；损坏时拒绝启动并保留原字节。
        let skills = if options.skill_learning {
            let plugin = SkillPlugin::new()?;
            let controller = plugin.controller();
            kernel.register(Box::new(plugin))?;
            kernel.start(&PluginId::new(SKILL_PLUGIN_ID)?).await?;
            Some(controller)
        } else {
            None
        };
        // 邀请账本同样先于通道加载；损坏时拒绝启动并保留原字节。
        let outreach = if options.outreach {
            let plugin = OutreachPlugin::new()?;
            let controller = plugin.controller();
            kernel.register(Box::new(plugin))?;
            kernel.start(&PluginId::new(OUTREACH_PLUGIN_ID)?).await?;
            Some(controller)
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
        // 送达过的邀请作为数据交给后续对话，用户接下来的话可能是在回应它。
        let context: Arc<dyn ContextAssembler> = match &outreach {
            Some(outreach) => Arc::new(qq_outreach::OutreachContext {
                wrapped: context,
                outreach: Arc::new(outreach.clone()),
            }),
            None => context,
        };
        // 已启用的技能作为对话工具：只能调用当前用户自己的技能，每次调用都实际运行验证。
        let skill_runner: Arc<std::sync::OnceLock<Arc<dyn PracticeRunner>>> = Arc::default();
        let context: Arc<dyn ContextAssembler> = match &skills {
            Some(skills) => {
                bootstrap.tools.push(Arc::new(SkillTool::new(
                    Arc::new(skills.clone()),
                    skill_runner.clone(),
                    options.state_directory.join("skill-tool-work"),
                )));
                Arc::new(qq_skill::SkillContext {
                    wrapped: context,
                    skills: Arc::new(skills.clone()),
                })
            }
            None => context,
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
        let control = crate::install_core_with_pages(
            &kernel,
            registry.clone(),
            permissions,
            logger,
            &options.state_directory,
            bootstrap,
            options.web_listen.map(|_| page_permit.clone()),
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
                options.interest_learning,
            )
            .await?;
            let commands = started.commands.clone();
            background = Some(started);
            commands
        } else {
            qq_cognition::Commands::disabled()
        };
        // 兴趣观察与受控研究共用主模型配置；只在需要默认模型实现时读取凭据。
        let core_resolver = || -> Result<Arc<dyn eve_llm_api::LlmModelResolver>, AppError> {
            let settings = registry
                .get(&ServiceId::new(CONFIG_SERVICE_ID)?)?
                .ok_or("兴趣学习配置服务缺失")?
                .value
                .downcast::<ConfigServiceHandle>()
                .map_err(|_| "兴趣学习配置服务类型错误")?;
            let key = std::env::var("EVE_OPENAI_API_KEY").map_err(|_| "缺少模型凭据")?;
            Ok(Arc::new(crate::models::CoreModelResolver::new(
                settings.0.clone(),
                key,
            )))
        };
        let InterestComponents {
            observer: interest_observer,
            fetcher,
            selector,
            extractor: knowledge_extractor,
            drafter: practice_drafter,
            runner: practice_runner,
            distiller: skill_distiller,
            skill_selector,
            composer: invitation_composer,
            timing_judge,
            response_judge,
        } = interest_components;
        let interest_commands = if let Some(interests) = &interests {
            let memory = memory.clone().ok_or("兴趣观察缺少记忆服务")?;
            let cognition = Arc::new(background.as_ref().ok_or("兴趣观察缺少认知服务")?.admin()?);
            let observer = match interest_observer {
                Some(observer) => observer,
                None => Arc::new(ModelInterestObserver::new(core_resolver()?)),
            };
            let deriver = Arc::new(LearningGoalDeriver::new(cognition.clone(), "eve")?);
            let dirty = Arc::new(std::sync::atomic::AtomicBool::new(true));
            interest_background = Some(qq_interest::Background::start(
                memory,
                Arc::new(interests.clone()),
                observer,
                deriver.clone(),
                options.interest_options.clone(),
                dirty.clone(),
            )?);
            qq_interest::Commands::enabled(
                Arc::new(interests.clone()),
                deriver,
                cognition.clone(),
                dirty,
            )
        } else {
            qq_interest::Commands::disabled()
        };
        let research_commands = if let (Some(knowledge), Some(policy), Some(interests)) =
            (&knowledge, research_policy, &interests)
        {
            let cognition = Arc::new(background.as_ref().ok_or("受控研究缺少认知服务")?.admin()?);
            let fetcher: Arc<dyn SourceFetcher> = match fetcher {
                Some(fetcher) => fetcher,
                None => Arc::new(HttpSourceFetcher::new()?),
            };
            let selector: Arc<dyn SourceSelector> = match selector {
                Some(selector) => selector,
                None => Arc::new(ModelSourceSelector::new(core_resolver()?)),
            };
            let extractor: Arc<dyn KnowledgeExtractor> = match knowledge_extractor {
                Some(extractor) => extractor,
                None => Arc::new(ModelKnowledgeExtractor::new(core_resolver()?)),
            };
            let researcher = Arc::new(Researcher::new(
                Arc::new(knowledge.clone()),
                fetcher,
                selector,
                extractor,
            ));
            research_background = Some(qq_research::Background::start(
                Arc::new(knowledge.clone()),
                cognition,
                researcher,
                policy,
            ));
            qq_research::Commands::enabled(Arc::new(interests.clone()), Arc::new(knowledge.clone()))
        } else {
            qq_research::Commands::disabled()
        };
        let practice_commands = if let (Some(practice), Some(interests)) = (&practice, &interests) {
            let cognition = Arc::new(background.as_ref().ok_or("实践验证缺少认知服务")?.admin()?);
            let runner: Arc<dyn PracticeRunner> = match (practice_runner, &options.practice_browser)
            {
                (Some(runner), _) => runner,
                (None, Some(browser)) => Arc::new(
                    BrowserRunner::new(browser.clone())
                        .map_err(|error| format!("实践运行环境无效：{error}"))?,
                ),
                (None, None) => Arc::new(
                    MindustryServerRunner::new(
                        RuntimeCommand {
                            program: options
                                .practice_java
                                .clone()
                                .unwrap_or_else(|| "java".into()),
                            prefix_args: options.practice_java_args.clone(),
                        },
                        options
                            .practice_server_jar
                            .clone()
                            .ok_or("实践验证缺少运行环境")?,
                    )
                    .map_err(|error| format!("实践运行环境无效：{error}"))?,
                ),
            };
            let _ = skill_runner.set(runner.clone());
            let mut drafter: Arc<dyn PracticeDrafter> = match practice_drafter {
                Some(drafter) => drafter,
                None => Arc::new(ModelPracticeDrafter::new(core_resolver()?)),
            };
            let skill_parts = match &skills {
                Some(skills) => {
                    let admin: Arc<dyn SkillAdmin> = Arc::new(skills.clone());
                    let selector: Arc<dyn SkillSelector> = match skill_selector {
                        Some(selector) => selector,
                        None => Arc::new(ModelSkillSelector::new(core_resolver()?)),
                    };
                    let distiller: Arc<dyn SkillDistiller> = match skill_distiller {
                        Some(distiller) => distiller,
                        None => Arc::new(ModelSkillDistiller::new(core_resolver()?)),
                    };
                    // 后续任务的第一次尝试先经一次有记录的技能选择；其余照常草稿。
                    drafter = Arc::new(SkillAwareDrafter::new(
                        admin.clone(),
                        selector,
                        drafter,
                        Arc::new(|| {
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map_or(1, |elapsed| {
                                    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
                                })
                        }),
                    ));
                    Some(qq_practice::Skills {
                        admin: admin.clone(),
                        consolidator: Arc::new(Consolidator::new(admin, distiller, runner.clone())),
                    })
                }
                None => None,
            };
            let practitioner = Arc::new(Practitioner::new(
                Arc::new(practice.clone()),
                drafter,
                runner,
            ));
            practice_background = Some(qq_practice::Background::start(qq_practice::Services {
                practice: Arc::new(practice.clone()),
                cognition,
                knowledge: knowledge
                    .clone()
                    .map(|knowledge| Arc::new(knowledge) as Arc<dyn KnowledgeAdmin>),
                practitioner,
                workspace_root: options.state_directory.join("practice-work"),
                skills: skill_parts,
            }));
            qq_practice::Commands::enabled(Arc::new(interests.clone()), Arc::new(practice.clone()))
        } else {
            qq_practice::Commands::disabled()
        };
        let skill_commands = skills
            .as_ref()
            .map_or_else(qq_skill::Commands::disabled, |skills| {
                qq_skill::Commands::enabled(Arc::new(skills.clone()))
            });
        let outreach_policy = OutreachPolicy {
            cooldown_ms: options
                .outreach_cooldown_ms
                .unwrap_or(OutreachPolicy::default().cooldown_ms),
            proactive_after_ms: options.outreach_proactive_after_ms,
        };
        let outreach_commands = if let (Some(outreach), Some(practice), Some(interests)) =
            (&outreach, &practice, &interests)
        {
            let composer: Arc<dyn InvitationComposer> = match invitation_composer {
                Some(composer) => composer,
                None => Arc::new(ModelInvitationComposer::new(core_resolver()?)),
            };
            let responder: Arc<dyn ResponseJudge> = match response_judge {
                Some(judge) => judge,
                None => Arc::new(ModelResponseJudge::new(core_resolver()?)),
            };
            let cognition: Arc<dyn eve_cognition_api::CognitionAdmin> =
                Arc::new(background.as_ref().ok_or("主动交流缺少认知服务")?.admin()?);
            let requests = Arc::new(
                RequestGoals::new(cognition.clone(), "eve")
                    .map_err(|_| "主动交流无法创建后续创作派生器")?,
            );
            outreach_background = Some(qq_outreach::Background::start(qq_outreach::Services {
                outreach: Arc::new(outreach.clone()),
                cognition,
                interests: Arc::new(interests.clone()),
                practice: Arc::new(practice.clone()),
                skills: skills
                    .clone()
                    .map(|skills| Arc::new(skills) as Arc<dyn SkillAdmin>),
                memory: memory.clone().ok_or("主动交流缺少记忆服务")?,
                composer,
                responder,
                requests,
            }));
            qq_outreach::Commands::enabled(Arc::new(outreach.clone()))
        } else {
            qq_outreach::Commands::disabled()
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
        let segment_preferences = segment_preferences.map(|inner| {
            Arc::new(crate::web_panel_plugins::ManagedSegmentPreferences {
                inspector: Arc::downgrade(&runtime_inspector),
                inner,
            }) as Arc<dyn SegmentPreferences>
        });
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
            interest_commands,
            research_commands,
            practice_commands,
            skill_commands,
            outreach_commands,
            segment_commands,
        ])));
        if let Some(outreach) = &outreach {
            let judge: Arc<dyn TimingJudge> = match timing_judge {
                Some(judge) => judge,
                None => Arc::new(ModelTimingJudge::new(core_resolver()?)),
            };
            // 主动私聊开启时，通道空闲后每 2 秒询问一次；是否发送由宿主策略决定。
            plugin = plugin.with_outreach(
                Arc::new(qq_outreach::ChannelAdapter {
                    outreach: Arc::new(outreach.clone()) as Arc<dyn OutreachAdmin>,
                    judge,
                    policy: outreach_policy,
                }),
                outreach_policy
                    .proactive_after_ms
                    .map(|_| std::time::Duration::from_secs(2)),
            )?;
        }
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
                    plugins: crate::web_panel_plugins::PanelPlugins::new(
                        runtime_inspector.clone(),
                        Arc::new(kernel.clone()),
                        registry.clone(),
                        page_permit.clone(),
                        if options.segmented {
                            std::collections::BTreeSet::from([SEGMENT_PREFERENCES_PLUGIN_ID.into()])
                        } else {
                            Default::default()
                        },
                    )
                    .map_err(|_| "面板插件管理初始化失败")?,
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
        if let Some(interest) = &interest_background {
            interest.activate();
        }
        if let Some(research) = &research_background {
            research.activate();
        }
        if let Some(practice) = &practice_background {
            practice.activate();
        }
        if let Some(outreach) = &outreach_background {
            outreach.activate();
        }
        wait_channel(
            handle.status.clone(),
            background.as_ref().map(qq_cognition::Background::finished),
            learning_background
                .as_ref()
                .map(qq_learning::Background::finished),
            interest_background
                .as_ref()
                .map(qq_interest::Background::finished),
            research_background
                .as_ref()
                .map(qq_research::Background::finished),
            practice_background
                .as_ref()
                .map(|practice| {
                    (
                        practice.finished(),
                        "实践验证后台已结束；QQ 通道停止准入并保留状态",
                    )
                })
                .into_iter()
                .chain(outreach_background.as_ref().map(|outreach| {
                    (
                        outreach.finished(),
                        "主动交流后台已结束；QQ 通道停止准入并保留状态",
                    )
                }))
                .collect(),
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
    if let Some(interest) = &interest_background {
        interest.request_stop();
    }
    if let Some(research) = &research_background {
        research.request_stop();
    }
    if let Some(practice) = &practice_background {
        practice.request_stop();
    }
    if let Some(outreach) = &outreach_background {
        outreach.request_stop();
    }
    // 撰写读取学习目标、实践与技能并写邀请账本；先写入取消结局。
    if let Some(outreach) = outreach_background
        && let Err(error) = outreach.stop().await
    {
        secondary.push(error);
    }
    // 实践读取学习目标与知识并写实践账本；先终止运行中的进程并写入取消结局。
    if let Some(practice) = practice_background
        && let Err(error) = practice.stop().await
    {
        secondary.push(error);
    }
    // 研究读取学习目标并写知识账本；在停止认知与 Kernel 前写入取消结局。
    if let Some(research) = research_background
        && let Err(error) = research.stop().await
    {
        secondary.push(error);
    }
    // 兴趣派生写认知状态；先结束兴趣后台，再停止认知插件。
    if let Some(interest) = interest_background
        && let Err(error) = interest.stop().await
    {
        secondary.push(error);
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

/// 任一后台结束时返回其说明；没有后台时永不返回。
async fn any_finished(receivers: Vec<(watch::Receiver<bool>, &'static str)>) -> &'static str {
    let mut waits: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = &'static str> + Send>>> =
        receivers
            .into_iter()
            .map(|(receiver, message)| {
                Box::pin(async move {
                    background_finished(Some(receiver)).await;
                    message
                })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = &'static str> + Send>>
            })
            .collect();
    std::future::poll_fn(|cx| {
        for wait in &mut waits {
            if let std::task::Poll::Ready(message) = wait.as_mut().poll(cx) {
                return std::task::Poll::Ready(message);
            }
        }
        std::task::Poll::Pending
    })
    .await
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
    interest: Option<watch::Receiver<bool>>,
    research: Option<watch::Receiver<bool>>,
    later: Vec<(watch::Receiver<bool>, &'static str)>,
    panel: Option<watch::Receiver<bool>>,
) -> Result<(), AppError> {
    let stop = interrupted();
    let stopped_background = background_finished(background);
    let stopped_learning = background_finished(learning);
    let stopped_interest = background_finished(interest);
    let stopped_research = background_finished(research);
    let stopped_later = any_finished(later);
    let stopped_panel = background_finished(panel);
    tokio::pin!(
        stop,
        stopped_background,
        stopped_learning,
        stopped_interest,
        stopped_research,
        stopped_later,
        stopped_panel
    );
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
            _ = &mut stopped_interest => return Err("兴趣观察后台已结束；QQ 通道停止准入并保留状态".into()),
            _ = &mut stopped_research => return Err("受控研究后台已结束；QQ 通道停止准入并保留状态".into()),
            message = &mut stopped_later => return Err(message.into()),
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

    #[test]
    fn interest_learning_is_explicit_and_composes_memory_and_cognition() {
        let default = parse(&[]).unwrap();
        assert!(!default.interest_learning);
        assert_eq!(default.interest_options, ObservationOptions::default());
        for args in [vec!["--self-learning"], vec!["--memory", "--cognition"]] {
            assert!(!parse(&args).unwrap().interest_learning);
        }
        let options = parse(&["--interest-learning", "--interest-cooldown-ms", "0"]).unwrap();
        assert!(options.interest_learning && options.memory && options.cognition);
        assert!(!options.memory_learning && !options.self_learning);
        assert_eq!(options.interest_options.cooldown_ms, 0);
        for invalid in ["-1", "86400001", "soon"] {
            assert!(parse(&["--interest-cooldown-ms", invalid]).is_err());
        }
    }

    #[test]
    fn practice_runtime_is_explicit_and_requires_interest_learning() {
        let default = parse(&[]).unwrap();
        assert!(default.practice_server_jar.is_none() && default.practice_java_args.is_empty());
        assert!(default.practice_java.is_none());
        let error = parse(&["--practice-mindustry-server", "server.jar"])
            .unwrap_err()
            .to_string();
        assert!(error.contains("--interest-learning"));
        for orphan in [
            vec!["--interest-learning", "--practice-java", "java"],
            vec!["--interest-learning", "--practice-java-arg", "-Dx=y"],
        ] {
            assert!(
                parse(&orphan)
                    .unwrap_err()
                    .to_string()
                    .contains("--practice-mindustry-server")
            );
        }
        let options = parse(&[
            "--interest-learning",
            "--practice-mindustry-server",
            "server.jar",
            "--practice-java",
            "python3",
            "--practice-java-arg",
            "fake.py",
        ])
        .unwrap();
        assert_eq!(
            options.practice_server_jar,
            Some(PathBuf::from("server.jar"))
        );
        assert_eq!(options.practice_java, Some(OsString::from("python3")));
        assert_eq!(options.practice_java_args, [OsString::from("fake.py")]);
    }

    #[test]
    fn browser_runtime_is_an_alternative_practice_runtime() {
        assert!(parse(&[]).unwrap().practice_browser.is_none());
        assert!(
            parse(&["--practice-browser", "chrome"])
                .unwrap_err()
                .to_string()
                .contains("--interest-learning")
        );
        let both = parse(&[
            "--interest-learning",
            "--practice-browser",
            "chrome",
            "--practice-mindustry-server",
            "server.jar",
        ])
        .unwrap_err()
        .to_string();
        assert!(both.contains("只能选一个"));
        let options = parse(&[
            "--interest-learning",
            "--practice-browser",
            "chrome",
            "--skill-learning",
            "--outreach",
        ])
        .unwrap();
        assert_eq!(options.practice_browser, Some(PathBuf::from("chrome")));
        assert!(options.skill_learning && options.outreach);
    }

    #[test]
    fn skill_learning_is_explicit_and_requires_a_practice_runtime() {
        assert!(!parse(&[]).unwrap().skill_learning);
        let error = parse(&["--interest-learning", "--skill-learning"])
            .unwrap_err()
            .to_string();
        assert!(error.contains("--practice-mindustry-server"));
        let options = parse(&[
            "--interest-learning",
            "--practice-mindustry-server",
            "server.jar",
            "--skill-learning",
        ])
        .unwrap();
        assert!(options.skill_learning);
    }

    #[test]
    fn research_sources_are_explicit_validated_and_require_interest_learning() {
        assert!(parse(&[]).unwrap().research_sources.is_empty());
        let seed = "https://docs.example/wiki/";
        let error = parse(&["--research-source", seed]).unwrap_err().to_string();
        assert!(error.contains("--interest-learning"));
        let options = parse(&[
            "--interest-learning",
            "--research-source",
            seed,
            "--research-source",
            "http://127.0.0.1:8080/watercolor/index.html",
        ])
        .unwrap();
        assert_eq!(options.research_sources.len(), 2);
        assert!(research_policy(&options).unwrap().is_some());
        for invalid in [
            "ftp://docs.example/",
            "https://user@docs.example/",
            "https://docs.example/#top",
            "docs.example/wiki",
        ] {
            assert!(parse(&["--interest-learning", "--research-source", invalid]).is_err());
        }
        let duplicate = [
            "--interest-learning",
            "--research-source",
            seed,
            "--research-source",
            seed,
        ];
        assert!(parse(&duplicate).is_err());
        let mut many = vec!["--interest-learning"];
        let seeds: Vec<String> = (0..9)
            .map(|n| format!("https://docs.example/{n}/"))
            .collect();
        for seed in &seeds {
            many.extend(["--research-source", seed.as_str()]);
        }
        assert!(parse(&many).is_err());
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
