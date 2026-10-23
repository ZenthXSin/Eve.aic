use crate::{
    AppError, AppFailure, MessageJudgeMode, core_bootstrap, finish_core, qq_agenda, qq_budget,
    qq_clarify, qq_cognition, qq_completion, qq_interest, qq_learning, qq_learning_commands,
    qq_memory, qq_memory_observer, qq_outreach, qq_plan, qq_practice, qq_pursuit, qq_reminder,
    qq_research, qq_semantic, qq_skill, qq_summary, qq_tasks, qq_tools, qq_triggers,
    segment_commands,
};
use eve_budget_api::{AutonomyBudget, BUDGET_PLUGIN_ID, BudgetPolicy};
use eve_budget_plugin::BudgetPlugin;
use eve_clarify_api::CLARIFY_PLUGIN_ID;
use eve_clarify_plugin::{ClarifyPlugin, ModelClarificationComposer};
use eve_cognition_loop_api::EndogenousPlannerFactory;
use eve_cognition_loop_plugin::ReflectionPlannerFactory;
use eve_completion_api::{COMPLETION_PLUGIN_ID, CompletionLedger};
use eve_completion_plugin::CompletionPlugin;
use eve_config_api::{CONFIG_SERVICE_ID, ConfigServiceHandle};
use eve_interest_api::{INTEREST_PLUGIN_ID, InterestObserver, ObservationOptions};
use eve_interest_plugin::{InterestPlugin, LearningGoalDeriver, ModelInterestObserver};
use eve_kernel::{Kernel, KernelServices};
use eve_knowledge_api::{
    CORPUS_SCHEME, KNOWLEDGE_PLUGIN_ID, KnowledgeAdmin, KnowledgeExtractor, SourceFetcher,
    SourcePolicy, SourceSelector,
};
use eve_knowledge_plugin::{
    CorpusSourceFetcher, HttpSourceFetcher, KnowledgePlugin, ModelKnowledgeExtractor,
    ModelSourceSelector, Researcher, RoutingSourceFetcher,
};
use eve_learning_api::{
    AutoConfirmationPolicy, LEARNING_PLUGIN_ID, LearningAdmin, LearningOptions, PreferenceExtractor,
};
use eve_learning_plugin::{EvidenceConfirmationPolicy, LearningPlugin, ModelPreferenceExtractor};
use eve_llm_api::ContextAssembler;
use eve_memory_api::{
    ConversationSummarizer, MEMORY_PLUGIN_ID, MemoryAdmin, SUMMARY_PLUGIN_ID, SummaryAdmin,
};
use eve_memory_plugin::{
    ConversationSummaryPlugin, LexicalMemoryRecall, MemoryContext, MemoryPlugin,
    MemoryRecallContext, ModelConversationSummarizer,
};
use eve_message_plugin::MessageRouterPlugin;
use eve_outreach_api::{
    InvitationComposer, OUTREACH_PLUGIN_ID, OutreachAdmin, OutreachPolicy, ResponseJudge,
    TimingJudge,
};
use eve_outreach_plugin::{
    ModelInvitationComposer, ModelResponseJudge, ModelTimingJudge, OutreachPlugin, RequestGoals,
};
use eve_plan_api::{PLAN_PLUGIN_ID, PlanJournal, PlanProposer};
use eve_plan_plugin::{ModelPlanProposer, PlanPlugin};
use eve_plugin_api::{PluginId, PluginResult, ServiceId};
use eve_practice_api::{
    PRACTICE_PLUGIN_ID, PracticeAdmin, PracticeDrafter, PracticeRunner, PracticeStrategy,
    STRATEGY_PLUGIN_ID,
};
use eve_practice_browser::BrowserRunner;
use eve_practice_mindustry::{MindustryServerRunner, RuntimeCommand};
use eve_practice_plugin::{ModelPracticeDrafter, PracticePlugin, Practitioner, StrategyPlugin};
use eve_qqbot_plugin::{
    DEFAULT_QQBOT_APP_ID, QQ_SEGMENT_LIMITS, QQ_SEGMENT_POLICY, QQBOT_PLUGIN_ID,
    QQBOT_STATUS_SERVICE_ID, QqBotConfig, QqBotPlugin, QqBotStatus, QqBotStatusHandle,
    QqCommandHandler, QqCommandInput, QqOutreach,
};
use eve_reminder_api::{REMINDER_PLUGIN_ID, ReminderAdmin};
use eve_reminder_plugin::ReminderPlugin;
use eve_segment_api::SegmentPreferences;
use eve_segment_plugin::{
    RuleSegmentAdvisor, SEGMENT_PREFERENCES_PLUGIN_ID, SegmentPreferencePlugin,
};
use eve_semantic_plugin::{HybridRecall, Indexer, SEMANTIC_PLUGIN_ID, SemanticPlugin};
use eve_session_api::{SESSION_SERVICE_ID, SessionServiceHandle};
use eve_skill_api::{SKILL_PLUGIN_ID, SkillAdmin, SkillDistiller, SkillSelector};
use eve_skill_plugin::{
    Consolidator, ModelSkillDistiller, ModelSkillSelector, SkillAwareDrafter, SkillPlugin,
    SkillTool,
};
use eve_toolforge_api::{TOOLFORGE_PLUGIN_ID, ToolAdmin, ToolForger};
use eve_toolforge_plugin::{Forge, ForgedChecks, ModelToolForger, ToolForgePlugin};
use eve_training_api::{TRAINING_PLUGIN_ID, TRAINING_SERVICE_ID, TrainingServiceHandle};
use eve_training_plugin::{TrainingContext, TrainingPlugin};
use std::{ffi::OsString, path::PathBuf, sync::Arc};
use tokio::sync::watch;

pub const QQBOT_HELP: &str = "--group-chat 接入全量群消息观察、说法学习与现有插话引擎；群回复首段艾特原发送者。需要平台提供 GROUP_MESSAGE_CREATE。
与 --message-judge jev 同时启用时，Jev 判断群话题关联；含混消息持久积攒，由独立主动回复和经验整理处理，不即时主模型兜底。
--sleep 开启可取消的短暂等待与本轮任务状态工具。
--triggers 开启原会话条件唤醒工具（时间、文件、后台任务、消息匹配、Linux 进程）。
Eve 官方 QQBot 通道
用法：eve-qqbot [--training] [--cognition] [--memory] [--memory-recall] [--semantic-recall] [--memory-learning] [--self-learning] [--interest-learning] [--research-source URL]... [--research-corpus 目录] [--reference 名称=目录]... [--max-tool-rounds 1至64] [--practice-mindustry-server jar] [--practice-java 程序] [--practice-java-arg 参数]... [--practice-browser 程序] [--skill-learning] [--strategy-learning] [--tool-forging] [--plans] [--outreach] [--outreach-cooldown-ms 毫秒] [--outreach-proactive-after-ms 毫秒] [--clarify] [--agenda-context] [--reminders] [--reminder-utc-offset ±HH:MM] [--stickers] [--background-tasks] [--no-error-recovery] [--subagents] [--goal-completion] [--learning-stop] [--autonomy-budget 次数] [--autonomy-budget-window-hours 小时] [--segmented] [--markdown] [--message-judge off|primary|jev] [--web-listen 环回IP:端口] [--learning-cooldown-ms 毫秒] [--interest-cooldown-ms 毫秒] [--cognition-max-executions 1至32] [--state-dir 目录] [--database-config 文件] [--agent 文件] [--node 程序] [--bridge-script 文件] [--bridge-arg 参数]
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
--semantic-recall 需同时 --memory-recall，并启用 embedding 语义模型角色（EVE_MODELS_SEMANTIC_ENABLED、_PROVIDER=openai、_MODEL、_DIMENSIONS）；后台为已保存的记忆建立向量索引，召回时把词项排名与向量相似度排名融合，没有共同字词的相近说法也能找到；向量模型不可用时改用词项召回并在资料中标明。/semantic 查看索引进度。
--memory-learning 需同时 --memory；每会话至少 3 条新经历触发首批，后续默认间隔 5 分钟（--learning-cooldown-ms 可调整），单次启动最多 4 次请求。
--self-learning 开启持续自主学习（同时开启记忆、提炼和分段）；内置策略要求自评至少 80、至少两条真实交互，并复核重复、手动及撤销冲突。
/self-learning status 查看模式、已关联候选与容量；自动节奏跟随有效偏好，手动设置优先；/segment reset 清除手动设置并恢复跟随学习。
/memory-candidates [页码] 查看候选；普通提炼模式用 /accept-memory ID 确认，自主模式按策略自动确认。
/memory-decision 候选ID 查看学习决策、来源与目标版本，并核对实际保存状态。
--interest-learning 从普通聊天观察用户明确表达的兴趣、经验与困难（同时开启记忆和认知），只保存可逐字核对的原话，并派生低优先级的后台学习目标；同一会话默认间隔 5 分钟（--interest-cooldown-ms 可调整），每批一次无工具请求。
/interests 查看本会话记录的兴趣与学习目标；/forget-interest 兴趣ID 撤回兴趣并取消对应学习目标。
--research-source 需同时 --interest-learning，可重复至多 8 个；为等待中的学习目标在这些入口页面的同源目录内受控研究（只读 GET，每个目标修订至多一次，每个目标累计至多 3 次）。
--research-corpus 指定宿主随附的固定版本本地资料目录（例如 vendor/eve-portable-knowledge，启动时核对清单摘要，不符拒绝启动）；--research-source 可用 eve-corpus://资料名/目录/ 指向其中的目录，研究照常选文、逐字引用，来源记为 eve-corpus 地址。
--reference 名称=目录 装配回复时可查的只读参考资料（至多 8 份，启动时读入内存，目录无效或超过上限拒绝启动）；模型用 search_reference 搜索、read_reference 按行读取并引用“资料名/路径:行号”。目录是 git 检出时记录其提交。--max-tool-rounds 设定单轮回复至多的工具批次（默认 1）；用尽时不让本轮失败，而是不再提供工具，让模型依据已有结果作答并说明未核实的部分。
/knowledge 兴趣ID 查看研究到的资料：来源原文附网址、抓取时间、版本与逐字引用，未验证推测单独标注；研究不重试、中断不重放。
--practice-mindustry-server 需同时 --interest-learning；为等待中的学习目标制作只含数据文件的最小 Mindustry 模组，用操作者提供的无头服务端 jar 在全新目录中实际加载并探测内容属性（--practice-java 默认 java），每个目标修订至多一次、每次至多三次尝试，只有实际加载且全部探测通过才记为已验证。
--practice-browser 需同时 --interest-learning，与 --practice-mindustry-server 二选一；为等待中的学习目标制作只含 HTML 与 CSS 的静态网页，用操作者提供的无头 Chromium 在全新目录中实际打开并探测元素的计算样式与文字（不能含脚本或外部资源，不访问网络），其余规则同上。
/practice 兴趣ID 查看实践记录：每次尝试的产物文件、运行版本、加载状态、警告与探测期望/实际值；中断不重放。
--skill-learning 需同时指定实践运行环境（--practice-mindustry-server 或 --practice-browser）；把实际验证通过的实践提炼为参数化技能，宿主核对能逐字还原原产物，再用与原值不同的参数在同一运行环境中实际运行通过后自动启用；后续任务的第一次尝试可选用已启用的技能，调用结果以实际运行证据为准。
--outreach 需同时指定实践运行环境（--practice-mindustry-server 或 --practice-browser）；学习目标有了实际验证的进展后撰写一条邀请（只用账本中的用户原话、实践证据与已启用技能），在用户下次私聊找 Eve 时先判断此刻是否合适（看用户这条消息与回复），合适才随被动回复附带，以平台回执为准；同一用户默认 24 小时内至多送达一条（--outreach-cooldown-ms 可调整），群聊不附带。
--outreach-proactive-after-ms 需同时 --outreach；邀请在被动窗口等待超过该时间仍未送达时主动私聊一次（受平台配额与用户开关限制，被拒绝时保持待投递）。
/outreach 查看邀请状态与回执；/outreach off 请 Eve 不再主动提起，/outreach on 恢复。
/skills 列出技能；/skill 技能ID 查看版本、验证证据、启用记录与调用；/skill disable|rollback 技能ID、/skill enable 技能ID 版本 停用、回退或启用某个已验证版本。
--tool-forging 需同时指定实践运行环境；同一运行环境下反复出现的问题，由一次无工具模型请求锻造成只读草稿文件的检查规则，宿主用实践账本中的真实草稿回放验证（出现过问题的全部拦下、验证通过的一个不误报）后自动启用；之后的实践在实际运行前先经它检查，拦下的草稿不运行、带着说明交给草稿器修正。
--plans 需同时开启受控研究或实践运行环境；/goal 保存的待办可以请模型建议多步计划（/plan propose 待办ID，每个待办版本至多一次、只建议不执行），确认后（/plan confirm 待办ID）由后台按依赖推进：计划只能使用已开启的受控研究与实践验证，步骤效果按研究与实践账本中的证据判定；/plans [待办ID] 查看进度与证据，/plan withdraw 待办ID 撤销。计划完成不改变待办状态。
--clarify 需同时开启受控研究或实践运行环境；学习目标在当前版本卡住时（实践尝试用尽仍未验证、运行环境做不了，或只开启研究时没有找到可引用的资料），写一个具体问题，在用户下次私聊时先判断时机，合适才随回复附带，不主动私聊；每个目标版本至多问一次，同一用户默认 24 小时内至多送达一个问题，之后的下一轮私聊记为回复，由兴趣观察照常处理。/clarify 查看，/clarify off|on 关闭或恢复。
--agenda-context 需同时 --interest-learning；用户私聊时，把 Eve 为这位用户的兴趣在后台做到哪一步（研究、实践、请求澄清，按记录整理）作为低优先级数据交给对话，用户问起时据实回答；只读记录，不改变后台行为。
--stickers 开启表情包：开启识图时，从用户直接发来的图片里收集候选，经视觉模型审看为表情包后收下（其余删除）；对话中模型可用 send_sticker 按心情挑一张，跟在回复文字后面作为图片发出。runtime.stickers 可分别关闭收集与发送。
--reminders 开启私聊提醒：用户自然地说“半小时后提醒我喝水”或“明早 9 点提醒我交作业”，模型调用提醒工具记下并如实回复，想取消时直接说；到时先主动私聊一次，发不出去时随用户下一条私聊的回复附带，到期 7 天仍未送达不再投递。没有命令；只在私聊中成立。--reminder-utc-offset 设置本地时区（默认 +08:00）。
--background-tasks 让 Eve 在对话中把需要多步、较长时间的事派发到后台：执行器可用对话里的工具（不含派发与子代理），按步数预算（工具调用次数，默认 50，runtime.tasks 可调）独立完成，结束后私聊通知派发它的用户，发不出去时随下一条私聊回复附带；用户可以让 Eve 查看或取消任务。重启时执行中的任务记为中断，不自动重跑。
--subagents 让 Eve 在一轮回复中把可以独立完成的子任务委托给子代理并等它交回结果；子代理同样按步数预算执行，须在 runtime.tasks.subagent_timeout_ms（默认 3 分钟）内完成，不能再委托或派发。
--goal-completion 需同时 --plans；待办的计划各步都被证据满足后，随用户下一次私聊的回复问一句这件事算不算完成，每份计划至多问一次；用户明确说完成了，模型调用确认工具，宿主核对计划仍对应当前版本后把待办标为已完成，并记下依据的计划与确认来源。没有命令；只在私聊中成立。
--learning-stop 需同时 --interest-learning；兴趣学习目标的研究与实践次数都用完、也没有在问你或等你回复时，后台把它标为已取消并记下原因（目标的停止条件“学习预算耗尽”），兴趣本身保留；不开启时目标一直等待。无论是否开启，/interests 与议程上下文都会说明每个学习目标接下来会做什么、在等什么。
--strategy-learning 需同时指定实践运行环境；从真实失败修正记录形成方法候选，经后续不同目标验证后自动启用，效果退化时回退；不新增模型请求或尝试次数。
--autonomy-budget 需同时 --interest-learning；兴趣观察、受控研究、实践草稿、技能、工具锻造、主动交流、请求澄清与情景语义摘要的后台模型请求在每个窗口（--autonomy-budget-window-hours 默认 24 小时，按 UTC 整点对齐）合计至多该次数；每次请求前先保存计数，重启不清零、失败不退还，余额不够一项工作时后台不开始新工作，重置后继续；回复用户消息、/plan propose、反思、偏好提炼与语义索引不计入。/budget 查看用量与重置时间。
/tools 列出锻造的工具；/tool 工具ID 查看版本、回放验证与调用；/tool disable|rollback 工具ID、/tool enable 工具ID 版本 停用、回退或启用某个已验证版本。
EVE_DEVELOPER_CONFIG 指定独立开发工具 JSON：源码工作区、操作者、命令/GitHub/升级开关与 token 环境引用；未配置时不注册。命令以宿主权限执行，仅供明确操作者；不是 QQ 测试环境开关。/help tools 查看实际已装配能力。
记忆保留原始会话来源；跨会话个人记忆按可信平台身份或面板账号绑定读取，默认不把私聊带入群。面板 runtime.memory_continuity 可热配置；内部反思不读取聊天偏好。
--segmented 把模型回复按自然段分成至多 3 条消息，段间停顿至多 2.5 秒；命令确认整条发送。
--markdown 让被动回复与主动私聊以 markdown 消息（msg_type=2）发送，需要机器人已通过平台 Markdown 权限审核；未审核时平台会拒绝发送，默认关闭。
/segment 查看本会话分段；/segment on|off|reset、/segment parts 2至5、/segment pace 0至200（%）按会话保存，从下一条回复生效。
/segment suggestions [页码] 从本会话已确认偏好查看节奏建议；/segment adopt 偏好ID 版本 明确采用，需同时 --memory 与 --segmented。
分段前重新核对当前代：/cancel、/add、/correct 后不再发送剩余片段；已发片段不撤回、重启不补发。
反思默认关闭，每次启动最多执行 32 项；每项一次模型请求、零工具，草稿不代表父目标完成。
修订先取消并等待；已有工具操作时只澄清，/new 内容明确开始独立任务。
Ctrl+C 或 SIGTERM 取消在途轮次、等待保存并停止桥接子进程。";
/// 密钥只在创建插件时从环境读取，不包含在启动参数和 Debug 中。
#[derive(Clone, Debug)]
pub struct QqBotOptions {
    pub group_chat: bool,
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
    /// 记忆召回同时按语义模型角色的向量相似度排序；需要 --memory-recall。
    pub semantic_recall: bool,
    pub memory_learning: bool,
    pub self_learning: bool,
    pub learning_options: LearningOptions,
    pub interest_learning: bool,
    pub interest_options: ObservationOptions,
    /// 受控研究的入口页面；为空表示不研究。
    pub research_sources: Vec<String>,
    /// 宿主随附的固定版本本地资料目录（含 MANIFEST.json）；研究入口可用 eve-corpus:// 指向其中的目录。
    pub research_corpus: Option<PathBuf>,
    /// 回复时可查的只读参考资料（名称、目录），装配 search_reference 与 read_reference。
    pub references: Vec<(String, PathBuf)>,
    /// 单轮回复至多的工具批次；None 保持宿主默认值。
    pub max_tool_rounds: Option<usize>,
    /// 实践验证使用的 Mindustry 无头服务端 jar；为空表示不实践。
    pub practice_server_jar: Option<PathBuf>,
    /// 静态网页实践使用的无头 Chromium 程序；与 Mindustry 服务端二选一。
    pub practice_browser: Option<PathBuf>,
    /// 启动运行环境的程序；未显式提供时为 java。
    pub practice_java: Option<OsString>,
    pub practice_java_args: Vec<OsString>,
    /// 把已验证的实践固化为技能并在后续任务中复用；需要实践验证。
    pub skill_learning: bool,
    /// 从实践修正实例中学习、验证和回退方法策略。
    pub strategy_learning: bool,
    /// 把反复出现的问题锻造成运行前检查工具；需要实践验证。
    pub tool_forging: bool,
    /// 用户待办的多步计划入口；需要受控研究或实践验证作为计划能力。
    pub plans: bool,
    /// 学习目标取得实际验证的进展后择机邀请用户；需要实践验证。
    pub outreach: bool,
    /// 学习目标卡住时请用户补充信息；需要受控研究或实践验证。
    pub clarify: bool,
    /// 把自己为当前用户在后台的学习进展作为对话上下文；需要兴趣学习。
    pub agenda_context: bool,
    /// 私聊提醒：模型按用户自然的说法调用提醒工具，到时私聊提醒。
    pub reminders: bool,
    pub stickers: bool,
    /// 模型可以把多步任务派发到后台执行，结束后私聊通知。
    pub background_tasks: bool,
    pub disable_error_recovery: bool,
    /// 模型可以把子任务委托给子代理并在本轮拿回结果。
    pub subagents: bool,
    pub triggers: bool,
    pub sleep: bool,
    /// 计划各步都被证据满足后，随用户下一次私聊的回复问一句待办是否完成；用户确认后标为完成。需要计划入口。
    pub goal_completion: bool,
    /// 兴趣学习目标的研究与实践次数都用完、再无可做时，后台把它标为已取消并记下原因。需要兴趣学习。
    pub learning_stop: bool,
    /// 提醒使用的本地时区与 UTC 的偏移（分钟）；未提供时为东八区。
    pub reminder_utc_offset_minutes: Option<i32>,
    /// 同一用户两次送达之间的最短间隔；未提供时为 24 小时。
    pub outreach_cooldown_ms: Option<u64>,
    /// 邀请在被动窗口等待多久后主动私聊；未提供时不主动私聊。
    pub outreach_proactive_after_ms: Option<u64>,
    /// 后台学习组件在每个窗口内合计可发起的模型请求数；未提供时不限总量。
    pub autonomy_budget: Option<u32>,
    /// 预算窗口长度（小时）；未提供时为 24。
    pub autonomy_budget_window_hours: Option<u64>,
    pub segmented: bool,
    /// 机器人拥有平台审核通过的 Markdown 权限时，被动回复与主动私聊改用 markdown 消息。
    pub markdown: bool,
    pub message_judge: MessageJudgeMode,
    pub web_listen: Option<std::net::SocketAddr>,
    pub cognition_max_executions: u16,
}
impl Default for QqBotOptions {
    fn default() -> Self {
        Self {
            group_chat: false,
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
            semantic_recall: false,
            memory_learning: false,
            self_learning: false,
            learning_options: LearningOptions::default(),
            interest_learning: false,
            interest_options: ObservationOptions::default(),
            research_sources: Vec::new(),
            research_corpus: None,
            references: Vec::new(),
            max_tool_rounds: None,
            practice_server_jar: None,
            practice_browser: None,
            practice_java: None,
            practice_java_args: Vec::new(),
            skill_learning: false,
            strategy_learning: false,
            tool_forging: false,
            plans: false,
            outreach: false,
            clarify: false,
            agenda_context: false,
            reminders: false,
            stickers: false,
            background_tasks: false,
            disable_error_recovery: false,
            subagents: false,
            triggers: false,
            sleep: false,
            goal_completion: false,
            learning_stop: false,
            reminder_utc_offset_minutes: None,
            outreach_cooldown_ms: None,
            outreach_proactive_after_ms: None,
            autonomy_budget: None,
            autonomy_budget_window_hours: None,
            segmented: false,
            markdown: false,
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
            if arg == "--sleep" {
                options.sleep = true;
                continue;
            }
            if arg == "--triggers" {
                options.triggers = true;
                continue;
            }
            if arg == "--group-chat" {
                options.group_chat = true;
                continue;
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
            if arg == "--semantic-recall" {
                options.semantic_recall = true;
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
            if arg == "--markdown" {
                options.markdown = true;
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
            if arg == "--strategy-learning" {
                options.strategy_learning = true;
                continue;
            }
            if arg == "--skill-learning" {
                options.skill_learning = true;
                continue;
            }
            if arg == "--tool-forging" {
                options.tool_forging = true;
                continue;
            }
            if arg == "--plans" {
                options.plans = true;
                continue;
            }
            if arg == "--outreach" {
                options.outreach = true;
                continue;
            }
            if arg == "--clarify" {
                options.clarify = true;
                continue;
            }
            if arg == "--agenda-context" {
                options.agenda_context = true;
                continue;
            }
            if arg == "--stickers" {
                options.stickers = true;
                continue;
            }
            if arg == "--reminders" {
                options.reminders = true;
                continue;
            }
            if arg == "--background-tasks" {
                options.background_tasks = true;
                continue;
            }
            if arg == "--no-error-recovery" {
                options.disable_error_recovery = true;
                continue;
            }
            if arg == "--subagents" {
                options.subagents = true;
                continue;
            }
            if arg == "--goal-completion" {
                options.goal_completion = true;
                continue;
            }
            if arg == "--learning-stop" {
                options.learning_stop = true;
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
                Some("--research-corpus") => options.research_corpus = Some(value.into()),
                Some("--reference") => {
                    let (name, directory) = value
                        .to_str()
                        .and_then(|value| value.split_once('='))
                        .filter(|(name, directory)| {
                            eve_reference_api::valid_root_name(name) && !directory.is_empty()
                        })
                        .ok_or("--reference 须为 名称=目录，名称为小写字母、数字或连字符")?;
                    if options
                        .references
                        .iter()
                        .any(|(existing, _)| existing == name)
                    {
                        return Err(format!("--reference 名称 {name} 重复").into());
                    }
                    options.references.push((name.into(), directory.into()));
                }
                Some("--max-tool-rounds") => {
                    options.max_tool_rounds = Some(
                        value
                            .to_str()
                            .and_then(|value| value.parse::<usize>().ok())
                            .filter(|rounds| (1..=64).contains(rounds))
                            .ok_or("--max-tool-rounds 须为 1 至 64")?,
                    );
                }
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
                Some("--reminder-utc-offset") => {
                    options.reminder_utc_offset_minutes = Some(
                        value
                            .to_str()
                            .and_then(parse_utc_offset)
                            .ok_or("提醒时区须为 -12:00 至 +14:00 的 ±HH:MM")?,
                    );
                }
                Some("--autonomy-budget") => {
                    options.autonomy_budget = Some(
                        value
                            .to_str()
                            .and_then(|v| v.parse().ok())
                            .filter(|v| (1..=eve_budget_api::MAX_LIMIT).contains(v))
                            .ok_or("自主运行预算必须为 1 至 1000000 的整数")?,
                    );
                }
                Some("--autonomy-budget-window-hours") => {
                    options.autonomy_budget_window_hours = Some(
                        value
                            .to_str()
                            .and_then(|v| v.parse().ok())
                            .filter(|v| (1..=720).contains(v))
                            .ok_or("自主运行预算窗口必须为 1 至 720 小时的整数")?,
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
        if options.semantic_recall && !options.memory_recall {
            return Err("--semantic-recall 需要同时开启 --memory-recall".into());
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
        if options.strategy_learning && !runtime {
            return Err("--strategy-learning 需要同时指定实践运行环境".into());
        }
        if options.skill_learning && !runtime {
            return Err("--skill-learning 需要同时指定实践运行环境（--practice-mindustry-server 或 --practice-browser）".into());
        }
        if options.tool_forging && !runtime {
            return Err("--tool-forging 需要同时指定实践运行环境（--practice-mindustry-server 或 --practice-browser）".into());
        }
        if options.plans && !runtime && options.research_sources.is_empty() {
            return Err("--plans 需要同时开启受控研究（--research-source）或实践运行环境，作为计划可用的能力".into());
        }
        if (options.outreach_cooldown_ms.is_some() || options.outreach_proactive_after_ms.is_some())
            && !options.outreach
        {
            return Err(
                "--outreach-cooldown-ms 与 --outreach-proactive-after-ms 需要同时开启 --outreach"
                    .into(),
            );
        }
        if options.agenda_context && !options.interest_learning {
            return Err("--agenda-context 需要同时开启 --interest-learning".into());
        }
        if options.reminder_utc_offset_minutes.is_some() && !options.reminders {
            return Err("--reminder-utc-offset 需要同时开启 --reminders".into());
        }
        if options.learning_stop && !options.interest_learning {
            return Err("--learning-stop 需要同时开启 --interest-learning".into());
        }
        if options.goal_completion && !options.plans {
            return Err("--goal-completion 需要同时开启 --plans：只有计划各步都被证据满足后才问用户是否完成".into());
        }
        if options.clarify && !runtime && options.research_sources.is_empty() {
            return Err("--clarify 需要同时开启受控研究（--research-source）或实践运行环境，作为判断目标是否卡住的依据".into());
        }
        if options.autonomy_budget_window_hours.is_some() && options.autonomy_budget.is_none() {
            return Err("--autonomy-budget-window-hours 需要同时指定 --autonomy-budget".into());
        }
        if options.autonomy_budget.is_some() && !options.interest_learning {
            return Err("--autonomy-budget 需要同时开启 --interest-learning".into());
        }
        if options.outreach && !runtime {
            return Err("--outreach 需要同时指定实践运行环境（--practice-mindustry-server 或 --practice-browser）".into());
        }
        Ok(Some(options))
    }
}
/// 解析 `±HH:MM` 时区偏移为分钟，范围 -12:00 至 +14:00。
fn parse_utc_offset(text: &str) -> Option<i32> {
    let (sign, rest) = match text.as_bytes().first()? {
        b'+' => (1, &text[1..]),
        b'-' => (-1, &text[1..]),
        _ => return None,
    };
    let (hours, minutes) = rest.split_once(':')?;
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    let (hours, minutes): (i32, i32) = (hours.parse().ok()?, minutes.parse().ok()?);
    let total = sign * (hours * 60 + minutes);
    (minutes < 60 && (-12 * 60..=14 * 60).contains(&total)).then_some(total)
}
/// 研究入口页面只在显式开启兴趣学习时生效；校验不发起网络请求。
fn research_policy(options: &QqBotOptions) -> Result<Option<SourcePolicy>, AppError> {
    if options.research_sources.is_empty() {
        return Ok(None);
    }
    if !options.interest_learning {
        return Err("--research-source 需要同时开启 --interest-learning".into());
    }
    let corpus_seeds = options
        .research_sources
        .iter()
        .any(|seed| seed.starts_with(&format!("{CORPUS_SCHEME}:")));
    if corpus_seeds && options.research_corpus.is_none() {
        return Err("eve-corpus:// 研究入口需要同时指定 --research-corpus".into());
    }
    if !corpus_seeds && options.research_corpus.is_some() {
        return Err("--research-corpus 需要至少一个 eve-corpus:// 研究入口".into());
    }
    Ok(Some(SourcePolicy::new(&options.research_sources).map_err(
        |_| "研究入口页面无效：至多 8 个互不重复、不含用户信息与片段的 http/https 或 eve-corpus:// URL",
    )?))
}

/// 打开本地资料并核对清单摘要；每个 eve-corpus:// 入口都必须指向这份资料。
fn research_corpus(options: &QqBotOptions) -> Result<Option<Arc<CorpusSourceFetcher>>, AppError> {
    let Some(directory) = &options.research_corpus else {
        return Ok(None);
    };
    let corpus =
        CorpusSourceFetcher::open(directory).map_err(|error| format!("本地资料无效：{error}"))?;
    let prefix = format!("{CORPUS_SCHEME}://{}/", corpus.name());
    if let Some(seed) = options
        .research_sources
        .iter()
        .find(|seed| seed.starts_with(&format!("{CORPUS_SCHEME}:")) && !seed.starts_with(&prefix))
    {
        return Err(format!("研究入口 {seed} 不属于本地资料 {}", corpus.name()).into());
    }
    Ok(Some(Arc::new(corpus)))
}

/// 兴趣学习的可替换实现；未提供的部分使用默认的模型观察器、HTTP 抓取器与模型选择/提炼器。
#[derive(Clone, Default)]
pub struct InterestComponents {
    /// 替换用户情景摘要器；装配 --memory 后仍由 runtime.memory_summary 控制准入。
    pub summarizer: Option<Arc<dyn ConversationSummarizer>>,
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
    /// 替换工具锻造器；默认使用主模型的单次无工具请求。是否锻造仍由 --tool-forging 决定。
    pub forger: Option<Arc<dyn ToolForger>>,
    /// 替换计划建议器；默认使用主模型的单次无工具请求。是否开启计划入口仍由 --plans 决定。
    pub plan_proposer: Option<Arc<dyn PlanProposer>>,
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
    #[cfg(windows)]
    {
        let mut stop = tokio::signal::windows::ctrl_break()?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = stop.recv() => {},
        }
    }
    #[cfg(not(any(unix, windows)))]
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
    // 明确的启动依赖错误仍在打开存储前拒绝；面板允许预装热启用所需记忆。
    let memory_available = options.memory
        || options.self_learning
        || options.interest_learning
        || options.web_listen.is_some();
    if options.memory_learning && !memory_available {
        return Err("--memory-learning 需要同时开启 --memory".into());
    }
    if options.memory_recall && !memory_available {
        return Err("--memory-recall 需要同时开启 --memory".into());
    }
    options.learning_options.validate()?;
    options.interest_options.validate()?;
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
    let mut clarify_background: Option<qq_clarify::Background> = None;
    let mut pursuit_background: Option<qq_pursuit::Background> = None;
    let mut plan_background: Option<qq_plan::Background> = None;
    let mut semantic_background: Option<qq_semantic::Background> = None;
    let mut semantic_queries: Option<Arc<qq_semantic::QueryRecallEmbeddings>> = None;
    let mut summary_background: Option<qq_summary::Background> = None;
    let mut digest_background: Option<crate::qq_history_digest::Background> = None;
    let mut sticker_background: Option<crate::qq_stickers::Background> = None;
    let mut impression_background: Option<crate::qq_impressions::Background> = None;
    let mut phrase_background: Option<crate::qq_phrases::Background> = None;
    let mut task_background: Option<qq_tasks::Background> = None;
    let mut group_background: Option<crate::qq_group::Background> = None;
    let mut group_jev = None;
    let mut trigger_background: Option<qq_triggers::Background> = None;
    let mut channel: Option<Arc<QqBotStatusHandle>> = None;
    let mut panel: Option<eve_web_panel::LocalPanel> = None;
    let page_permit = eve_web_panel_api::PageWritePermit::default();
    // 插件只能弱引用此宿主诊断句柄，避免 QQ 插件与 Kernel 形成引用环。
    let runtime_inspector: Arc<dyn eve_plugin_api::RuntimeInspector> = Arc::new(kernel.clone());
    let result: Result<(), AppError> = async {
        // 面板只保存配置；独立后台通过只读契约订阅，所有异常仍走统一收尾。
        let settings = eve_interest_plugin::InterestSettingsPlugin::new(
            eve_interest_api::InterestLearningSettings {
                enabled: options.interest_learning,
                observation: options.interest_options.clone(),
            },
        )?;
        let interest_settings = settings.controller();
        let settings = if options.web_listen.is_some() {
            settings.with_web_pages(page_permit.clone())
        } else {
            settings
        };
        kernel.register(Box::new(settings))?;
        kernel
            .start(&PluginId::new(
                eve_interest_api::INTEREST_SETTINGS_PLUGIN_ID,
            )?)
            .await?;
        let snapshot = eve_interest_api::InterestSettingsReader::snapshot(&interest_settings)?;
        // 有面板时预装记忆、认知和账本，观察默认仍关闭；启用无需重建 QQ 通道。
        options.interest_learning |= options.web_listen.is_some() || snapshot.settings.enabled;
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
        let research_corpus = research_corpus(&options)?;
        let practice_enabled = options.practice_server_jar.is_some()
            || options.practice_browser.is_some()
            || interest_components.runner.is_some();
        if practice_enabled && !options.interest_learning {
            return Err("实践验证需要同时开启 --interest-learning".into());
        }
        if options.strategy_learning && !practice_enabled {
            return Err("策略学习需要同时开启实践验证".into());
        }
        if options.skill_learning && !practice_enabled {
            return Err("技能固化需要同时开启实践验证".into());
        }
        if options.tool_forging && !practice_enabled {
            return Err("工具锻造需要同时开启实践验证".into());
        }
        if options.plans && !practice_enabled && research_policy.is_none() {
            return Err("计划入口需要同时开启受控研究或实践验证".into());
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
        // 按用途记录本进程实际模型请求的结局，供面板区分“通道已连接”与“模型能完成请求”；只在内存中。
        let model_health = Arc::new(eve_model_health::HealthBook::new());
        model_health.register("dialogue");
        bootstrap.health = Some(model_health.clone());
        // 历史窗口省略较早轮次时用摘要代替；摘要是派生数据，原始会话历史完整保留。
        let digest_policy = Arc::new(crate::qq_history_digest::Policy::default());
        let impression_policy = Arc::new(crate::qq_impressions::Policy::default());
        let phrase_policy = Arc::new(crate::qq_phrases::Policy::default());
        let history_digests = Arc::new(eve_history_digest::HistoryDigestService::new(
            eve_history_digest::DigestStore::open(options.state_directory.join("history-digest"))?,
            Box::new(crate::qq_history_digest::SharedPolicy(
                digest_policy.clone(),
            )),
        ));
        bootstrap.history_digests = Some(history_digests.clone());
        let people_plugin = eve_people_plugin::PeoplePlugin::new()?.with_impression_learning();
        let people: Arc<dyn eve_people_api::PeopleDirectory> = Arc::new(people_plugin.controller());
        kernel.register(Box::new(people_plugin))?;
        kernel
            .start(&PluginId::new(eve_people_api::PEOPLE_PLUGIN_ID)?)
            .await?;
        let archive_path = options.state_directory.join("qq-receipts.sqlite3");
        let previous_receipts = backends.state.get(
            &PluginId::new(eve_qqbot_plugin::QQBOT_PLUGIN_ID)?,
            "receipts.v1",
        )?;
        let bound_archive = previous_receipts
            .as_deref()
            .map(serde_json::from_slice::<serde_json::Value>)
            .transpose()?
            .is_some_and(|v| v.get("archive").is_some_and(|a| !a.is_null()));
        if bound_archive && !archive_path.is_file() {
            return Err("QQ 回执归档数据库缺失；保留现场，请恢复原数据库。".into());
        }
        let receipt_archive: Arc<dyn eve_qqbot_plugin::QqReceiptArchive> = Arc::new(
            eve_qqbot_receipt_sqlite::SqliteReceiptArchive::open(&archive_path)?,
        );
        let plugin = QqBotPlugin::new(QqBotConfig {
            node_program: options.node_program,
            bridge_script: options.bridge_script,
            bridge_args: options.bridge_args,
            app_id: std::env::var("QQBOT_APP_ID").unwrap_or_else(|_| DEFAULT_QQBOT_APP_ID.into()),
            app_secret,
            sandbox,
            markdown: options.markdown,
        })?
        .with_receipt_archive(receipt_archive.clone())
        .with_training()?
        .with_participant_observer(Arc::new(crate::qq_people::Observer(people.clone())))
        .with_natural_message_judgement(options.message_judge != MessageJudgeMode::Off)
        .with_timing_diagnostics(crate::models::timing_enabled());
        // 会话是私聊还是群聊由通道登记；只在私聊中成立的能力据此判断。
        let conversations = plugin.conversations();
        // 分段开启时，短的闲聊回复按句拆成几条短消息；runtime.chat_style 可热关闭。
        let chat_style = Arc::new(crate::qq_chat_style::ChatStyle::default());
        let mut plugin = if options.segmented {
            plugin.with_segmenter(
                Arc::new(eve_segment_plugin::ChatPlanner::new(chat_style.clone())),
                QQ_SEGMENT_LIMITS,
            )?
        } else {
            plugin
        };

        kernel.register(Box::new(
            TrainingPlugin::new(options.training)?.with_phrase_learning(),
        ))?;
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
        let group = if options.group_chat {
            bootstrap.tool_scope_policy = Some(Arc::new(crate::qq_group::ToolPolicy));
            let store = Arc::new(eve_group_observation_sqlite::SqliteGroupObservations::open(
                &options.state_directory.join("group-observations.sqlite3"),
            )?);
            let group = crate::qq_group::Group::new(
                std::env::var("QQBOT_APP_ID").unwrap_or_else(|_| DEFAULT_QQBOT_APP_ID.into()),
                store,
                training.0.clone(),
            );
            plugin = plugin.with_group_observer(group.clone());
            Some(group)
        } else {
            None
        };
        let memory: Option<Arc<dyn MemoryAdmin>> = if options.memory {
            let archive = eve_memory_plugin::FileEvidenceArchive::open(
                crate::storage::memory_archive_directory(&options.state_directory),
            )?;
            let plugin = MemoryPlugin::new()?.with_archive(
                Arc::new(archive),
                eve_memory_plugin::ArchivePolicy::default(),
            )?;
            let controller = plugin.controller();
            kernel.register(Box::new(plugin))?;
            kernel.start(&PluginId::new(MEMORY_PLUGIN_ID)?).await?;
            Some(Arc::new(controller))
        } else {
            None
        };
        let memory_continuity: Option<Arc<dyn eve_memory_api::MemoryContinuityFactory>> =
            memory.as_ref().map(|memory| {
                Arc::new(eve_memory_plugin::MemoryContinuity::new(
                    memory.clone(),
                    Arc::new(crate::qq_memory_continuity::Policy {
                        registry: registry.clone(),
                        conversations: conversations.clone(),
                    }),
                )) as Arc<dyn eve_memory_api::MemoryContinuityFactory>
            });
        let summaries: Option<Arc<dyn SummaryAdmin>> = if memory.is_some() {
            let plugin = ConversationSummaryPlugin::new()?;
            let controller = plugin.controller();
            kernel.register(Box::new(plugin))?;
            kernel.start(&PluginId::new(SUMMARY_PLUGIN_ID)?).await?;
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
        let strategies: Option<Arc<dyn PracticeStrategy>> = if options.strategy_learning {
            let plugin = StrategyPlugin::new()?;
            let controller = plugin.controller();
            kernel.register(Box::new(plugin))?;
            kernel.start(&PluginId::new(STRATEGY_PLUGIN_ID)?).await?;
            controller.reconcile(&practice.as_ref().ok_or("策略缺少实践账本")?.snapshot()?)?;
            Some(Arc::new(controller))
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
        // 工具账本同样先于通道加载；损坏时拒绝启动并保留原字节。
        let tools = if options.tool_forging {
            let plugin = ToolForgePlugin::new()?;
            let controller = plugin.controller();
            kernel.register(Box::new(plugin))?;
            kernel.start(&PluginId::new(TOOLFORGE_PLUGIN_ID)?).await?;
            Some(controller)
        } else {
            None
        };
        // 计划账本同样先于通道加载；损坏时拒绝启动并保留原字节，遗留执行中的步骤封存为中断。
        let plans = if options.plans {
            let plugin = PlanPlugin::new("eve")?;
            let controller = plugin.controller();
            kernel.register(Box::new(plugin))?;
            kernel.start(&PluginId::new(PLAN_PLUGIN_ID)?).await?;
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
        // 提醒账本同样先于通道加载；损坏时拒绝启动并保留原字节，发送中的提醒记为结果未知。
        let reminders = if options.reminders {
            let plugin = ReminderPlugin::new()?;
            let controller = plugin.controller();
            kernel.register(Box::new(plugin))?;
            kernel.start(&PluginId::new(REMINDER_PLUGIN_ID)?).await?;
            Some(controller)
        } else {
            None
        };
        // 后台任务账本同样先于通道加载；损坏时拒绝启动并保留原字节，执行中的任务记为中断，发送中的通知记为结果未知。
        let tasks = if options.background_tasks {
            let plugin = eve_task_plugin::TaskPlugin::new()?;
            let controller = plugin.controller();
            kernel.register(Box::new(plugin))?;
            kernel
                .start(&PluginId::new(eve_task_api::TASK_PLUGIN_ID)?)
                .await?;
            Some(controller)
        } else {
            None
        };
        let trigger_admin: Option<Arc<dyn eve_trigger_api::TriggerAdmin>> = if options.triggers {
            let trigger_plugin = eve_trigger_plugin::TriggerPlugin::new()?;
            let controller = Arc::new(trigger_plugin.controller());
            kernel.register(Box::new(trigger_plugin))?;
            kernel
                .start(&PluginId::new(eve_trigger_api::PLUGIN_ID)?)
                .await?;
            Some(controller)
        } else {
            None
        };
        let trigger_observations = if trigger_admin.is_some() {
            // 核心服务稍后装配；工具通过一次性句柄绑定同一个观察器。
            Some(Arc::new(std::sync::OnceLock::new()))
        } else {
            None
        };
        let task_late = Arc::new(qq_tasks::Late::default());
        if options.subagents {
            let profiles = eve_subagent_profile_sqlite::SqliteSubagentProfiles::open(
                &options.state_directory.join("subagent-profiles.sqlite3"),
            )?;
            let _ = task_late.profiles.set(Arc::new(profiles));
        }
        // 完成确认提问账本同样先于通道加载；损坏时拒绝启动并保留原字节，发送中的提问记为结果未知。
        let completion = if options.goal_completion {
            let plugin = CompletionPlugin::new()?;
            let controller = plugin.controller();
            kernel.register(Box::new(plugin))?;
            kernel.start(&PluginId::new(COMPLETION_PLUGIN_ID)?).await?;
            Some(controller)
        } else {
            None
        };
        // 请求澄清账本同样先于通道加载；损坏时拒绝启动并保留原字节。
        let clarify = if options.clarify {
            let plugin = ClarifyPlugin::new()?;
            let controller = plugin.controller();
            kernel.register(Box::new(plugin))?;
            kernel.start(&PluginId::new(CLARIFY_PLUGIN_ID)?).await?;
            Some(controller)
        } else {
            None
        };
        // 预算账本同样先于后台加载；损坏时拒绝启动并保留原字节。
        let budget_gate = match options.autonomy_budget {
            Some(limit) => {
                let plugin = BudgetPlugin::new(BudgetPolicy {
                    window_ms: options.autonomy_budget_window_hours.unwrap_or(24) * 3_600_000,
                    limit,
                })?;
                let controller = plugin.controller();
                kernel.register(Box::new(plugin))?;
                kernel.start(&PluginId::new(BUDGET_PLUGIN_ID)?).await?;
                qq_budget::Gate::new(Some(Arc::new(controller) as Arc<dyn AutonomyBudget>))
            }
            None => qq_budget::Gate::default(),
        };
        // 认知服务启动后才绑定；对话工具与推进判断在此之前不读写目标。
        let dialogue_cognition: Arc<
            std::sync::OnceLock<Arc<dyn eve_cognition_api::CognitionAdmin>>,
        > = Arc::default();
        // 学习目标的推进判断只读各账本，交给议程上下文与 /interests；开启 --learning-stop 时由后台执行停止。
        let pursuit = interests.as_ref().map(|interests| {
            Arc::new(qq_pursuit::Pursuit {
                cognition: dialogue_cognition.clone(),
                interests: Arc::new(interests.clone()),
                knowledge: knowledge
                    .clone()
                    .map(|knowledge| Arc::new(knowledge) as Arc<dyn KnowledgeAdmin>),
                practice: practice
                    .clone()
                    .map(|practice| Arc::new(practice) as Arc<dyn PracticeAdmin>),
                clarify: clarify
                    .clone()
                    .map(|clarify| Arc::new(clarify) as Arc<dyn eve_clarify_api::ClarifyAdmin>),
                outreach: outreach
                    .clone()
                    .map(|outreach| Arc::new(outreach) as Arc<dyn eve_outreach_api::OutreachAdmin>),
                gate: budget_gate.clone(),
                policy: Arc::new(eve_pursuit_plugin::RulePursuitPolicy),
            })
        });
        let context: Arc<dyn ContextAssembler> =
            if let (Some(memory), Some(summaries)) = (&memory, &summaries) {
                Arc::new(qq_summary::SummaryContext {
                    wrapped: context,
                    memory: memory.clone(),
                    summaries: summaries.clone(),
                    registry: registry.clone(),
                })
            } else {
                context
            };
        let context: Arc<dyn ContextAssembler> = if let Some(memory) = &memory {
            let context = Arc::new(crate::qq_conversation::ConversationContext {
                wrapped: context,
                memory: memory.clone(),
                training: training.0.clone(),
                registry: registry.clone(),
            });
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
        // 自己为当前用户在后台的学习进展作为数据交给对话；只读研究、实践与澄清记录。
        let context: Arc<dyn ContextAssembler> = match (&interests, options.agenda_context) {
            (Some(interests), true) => Arc::new(qq_agenda::AgendaContext {
                wrapped: context,
                interests: Arc::new(interests.clone()),
                knowledge: knowledge
                    .clone()
                    .map(|knowledge| Arc::new(knowledge) as Arc<dyn KnowledgeAdmin>),
                practice: practice
                    .clone()
                    .map(|practice| Arc::new(practice) as Arc<dyn PracticeAdmin>),
                clarify: clarify
                    .clone()
                    .map(|clarify| Arc::new(clarify) as Arc<dyn eve_clarify_api::ClarifyAdmin>),
                pursuit: pursuit.clone(),
            }),
            _ => context,
        };
        // 私聊提醒：模型按用户自然的说法调用提醒工具；私聊时交给对话当前本地时间与等待中的提醒。
        let reminder_time = qq_reminder::LocalTime {
            offset_minutes: options.reminder_utc_offset_minutes.unwrap_or(8 * 60),
        };
        let context: Arc<dyn ContextAssembler> = match &reminders {
            Some(reminders) => {
                let admin: Arc<dyn ReminderAdmin> = Arc::new(reminders.clone());
                bootstrap.tools.push(Arc::new(qq_reminder::SetReminderTool {
                    reminders: admin.clone(),
                    conversations: conversations.clone(),
                    local: reminder_time,
                }));
                bootstrap
                    .tools
                    .push(Arc::new(qq_reminder::CancelReminderTool {
                        reminders: admin.clone(),
                        conversations: conversations.clone(),
                        local: reminder_time,
                    }));
                Arc::new(qq_reminder::ReminderContext {
                    wrapped: context,
                    reminders: admin,
                    conversations: conversations.clone(),
                    local: reminder_time,
                })
            }
            None => context,
        };
        // 后台任务：模型可以派发、查看与取消；对话里能看到这位用户最近的任务与结果。
        let context: Arc<dyn ContextAssembler> = match &tasks {
            Some(tasks) => {
                let admin: Arc<dyn eve_task_api::TaskAdmin> = Arc::new(tasks.clone());
                bootstrap.tools.push(Arc::new(qq_tasks::DispatchTool {
                    tasks: admin.clone(),
                    late: task_late.clone(),
                }));
                bootstrap.tools.push(Arc::new(qq_tasks::ListTool {
                    tasks: admin.clone(),
                }));
                bootstrap.tools.push(Arc::new(qq_tasks::CancelTool {
                    tasks: admin.clone(),
                }));
                Arc::new(qq_tasks::TaskContext {
                    wrapped: context,
                    tasks: admin,
                })
            }
            None => context,
        };
        if options.subagents {
            bootstrap.tools.push(Arc::new(qq_tasks::SubagentTool {
                late: task_late.clone(),
            }));
            for kind in 0..3 {
                bootstrap
                    .tools
                    .push(Arc::new(crate::qq_subagents::ProfileTool {
                        late: task_late.clone(),
                        kind,
                    }));
            }
        }
        if let (Some(admin), Some(observations)) = (&trigger_admin, &trigger_observations) {
            for operation in [
                "set_wakeup_trigger",
                "list_wakeup_triggers",
                "cancel_wakeup_trigger",
            ] {
                bootstrap.tools.push(Arc::new(qq_triggers::Tools {
                    admin: admin.clone(),
                    observations: observations.clone(),
                    conversations: conversations.clone(),
                    operation,
                }));
            }
        }
        // 宿主实际保存了什么、单次回复上限多少，作为数据交给对话，避免模型对用户说错或长文被截断。
        let context: Arc<dyn ContextAssembler> = Arc::new(crate::qq_people::Context {
            wrapped: context,
            people: people.clone(),
            registry: registry.clone(),
        });
        let context: Arc<dyn ContextAssembler> = Arc::new(crate::qq_host_facts::HostFactsContext {
            wrapped: context,
            registry: registry.clone(),
            memory: options.memory,
            interests: interests.as_ref().map(|_| {
                Arc::new(interest_settings.clone())
                    as Arc<dyn eve_interest_api::InterestSettingsReader>
            }),
        });
        // 已启用的技能作为对话工具：只能调用当前用户自己的技能，每次调用都实际运行验证。
        let skill_runner: Arc<std::sync::OnceLock<Arc<dyn PracticeRunner>>> = Arc::default();
        // 待办完成确认：私聊时交给对话等待用户确认的待办；用户明确说完成了才调用确认工具。
        let completion_plans: Option<Arc<dyn eve_plan_api::PlanJournal>> = plans
            .clone()
            .map(|plans| Arc::new(plans) as Arc<dyn eve_plan_api::PlanJournal>);
        let context: Arc<dyn ContextAssembler> = match (&completion, &completion_plans) {
            (Some(completion), Some(plans)) => {
                bootstrap.tools.push(Arc::new(qq_completion::ConfirmTool {
                    cognition: dialogue_cognition.clone(),
                    plans: plans.clone(),
                    conversations: conversations.clone(),
                }));
                Arc::new(qq_completion::CompletionContext {
                    wrapped: context,
                    cognition: dialogue_cognition.clone(),
                    plans: plans.clone(),
                    ledger: Arc::new(completion.clone()) as Arc<dyn CompletionLedger>,
                    conversations: conversations.clone(),
                })
            }
            _ => context,
        };
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
        let context: Arc<dyn ContextAssembler> = if let Some(group) = &group {
            Arc::new(crate::qq_group::Context {
                wrapped: context,
                group: group.clone(),
                conversations: conversations.clone(),
            })
        } else {
            context
        };
        // 语义索引账本同样先于通道加载；损坏时拒绝启动并保留原字节。
        let semantic = if options.semantic_recall {
            // 已归档的旧原文也建索引：它们的向量写成状态目录下独立的压缩分段，不进入状态文件。
            let segments = eve_semantic_plugin::SegmentStore::open(
                options.state_directory.join("semantic-archive"),
            )
            .map_err(|_| "语义归档分段目录不可用")?;
            let plugin = SemanticPlugin::new()?.with_segments(Arc::new(segments));
            let index = plugin.index();
            kernel.register(Box::new(plugin))?;
            kernel.start(&PluginId::new(SEMANTIC_PLUGIN_ID)?).await?;
            let embeddings = Arc::new(qq_semantic::DeferredEmbeddings::new());
            semantic_queries = Some(Arc::new(qq_semantic::QueryRecallEmbeddings::new(
                embeddings.clone(),
                registry.clone(),
                crate::models::timing_enabled(),
            )));
            Some((index, embeddings))
        } else {
            None
        };
        bootstrap.context = Some(if options.memory_recall {
            let memory = memory.clone().ok_or("记忆召回缺少记忆服务")?;
            let lexical: Arc<dyn eve_memory_api::MemoryRecallFactory> =
                Arc::new(LexicalMemoryRecall::new(memory.clone()));
            match &semantic {
                // 词项与语义混合召回；向量模型在核心装配后注入，通道收消息前完成。
                Some((index, _)) => Arc::new(MemoryRecallContext::with_async_recall(
                    "qq",
                    Arc::new(HybridRecall::new(
                        memory,
                        lexical.clone(),
                        index.clone(),
                        semantic_queries.clone().ok_or("语义查询服务缺失")?,
                    )),
                    lexical,
                    context,
                )?),
                None => Arc::new(MemoryRecallContext::new("qq", lexical, context)?),
            }
        } else {
            context
        });
        if let Some(continuity) = &memory_continuity {
            bootstrap.context = Some(Arc::new(eve_memory_plugin::MemoryContinuityContext::new(
                "qq",
                continuity.clone(),
                bootstrap.context.take().ok_or("对话上下文缺失")?,
            )?));
        }
        // 后台能力不自动成为对话工具：显式装配公开 Tool，并让帮助和能力上下文共用注册源。
        // 本地资料地址交给资料源，其他地址交给网页抓取器（或替换实现）。
        let routed = |web: Arc<dyn SourceFetcher>| -> Arc<dyn SourceFetcher> {
            match &research_corpus {
                Some(corpus) => Arc::new(RoutingSourceFetcher::new(Some(corpus.clone()), web)),
                None => web,
            }
        };
        let dialogue_fetcher: Option<Arc<dyn SourceFetcher>> = if research_policy.is_some() {
            Some(routed(match interest_components.fetcher.clone() {
                Some(fetcher) => fetcher,
                None => Arc::new(HttpSourceFetcher::new()?),
            }))
        } else {
            None
        };
        let dialogue_tools = eve_dialogue_tools::Services {
            memory: memory.clone(),
            memory_continuity: memory_continuity.clone(),
            interests: interests
                .clone()
                .map(|v| Arc::new(v) as Arc<dyn eve_interest_api::InterestAdmin>),
            cognition: options.cognition.then(|| dialogue_cognition.clone()),
            knowledge: knowledge
                .clone()
                .map(|v| Arc::new(v) as Arc<dyn KnowledgeAdmin>),
            practice: practice
                .clone()
                .map(|v| Arc::new(v) as Arc<dyn PracticeAdmin>),
            skills: skills.clone().map(|v| Arc::new(v) as Arc<dyn SkillAdmin>),
            checks: tools.clone().map(|v| Arc::new(v) as Arc<dyn ToolAdmin>),
            plans: plans.clone().map(|v| Arc::new(v) as Arc<dyn PlanJournal>),
            outreach: outreach
                .clone()
                .map(|v| Arc::new(v) as Arc<dyn OutreachAdmin>),
            budget: budget_gate.budget(),
            source: research_policy.clone().zip(dialogue_fetcher.clone()),
            runner: practice.is_some().then(|| skill_runner.clone()),
            draft_check: tools.clone().map(|v| {
                Arc::new(ForgedChecks::new(Arc::new(v))) as Arc<dyn eve_practice_api::DraftCheck>
            }),
            workspace_root: options.state_directory.join("dialogue-practice-work"),
            owner_check: None,
        }
        .tools();
        let (dialogue_registered, preference_services) = dialogue_tools;
        bootstrap.tools.extend(dialogue_registered);
        // 文件发送默认开启：发件目录固定在状态目录下，模型只能报文件名，路径由宿主解析。
        let outbox = Arc::new(crate::qq_files::Outbox::open(
            options.state_directory.join("qq-outbox"),
        )?);
        plugin = plugin.with_files(outbox.clone(), std::time::Duration::from_secs(2))?;
        bootstrap
            .tools
            .push(Arc::new(crate::qq_files::SendFileTool::new(outbox)));
        let inbox = Arc::new(crate::qq_files::AttachmentInbox::new());
        plugin = plugin.with_attachment_observer(inbox.clone());
        // 慢步骤进度：某一步超过 runtime.progress.slow_after_ms 仍未结束时补发一条真实进度。
        let slow_progress = Arc::new(crate::qq_progress::SlowProgress::default());
        plugin = plugin.with_slow_progress(slow_progress.clone());
        // 连发合并：同一人连发的纯文字在 runtime.chat_timing.debounce_ms 内到达的合成一轮回复。
        let chat_timing = Arc::new(crate::qq_chat_timing::ChatTiming::default());
        plugin = plugin.with_debounce(chat_timing.clone());
        // 视觉输入：runtime.vision 开启时把本轮图片作为图像交给模型，像素不写入任何账本。
        // 表情包：从本轮直接附带的图片里收集候选，审看后由 send_sticker 挑选，随回复正文之后发出。
        // 库损坏时拒绝启动并保留原文件；不开启时不打开库、不注册工具。
        let stickers = if options.stickers {
            let stickers = Arc::new(crate::qq_stickers::Stickers::new(Arc::new(
                eve_sticker_plugin::FileStickerLibrary::open(
                    options.state_directory.join("stickers"),
                )
                .map_err(|error| format!("表情包库无法打开：{error}"))?,
            )));
            plugin = plugin.with_stickers(stickers.clone());
            bootstrap
                .tools
                .push(Arc::new(crate::qq_stickers::SendStickerTool(
                    stickers.clone(),
                )));
            Some(stickers)
        } else {
            None
        };
        let vision = Arc::new(crate::qq_vision::QqTurnImages {
            stickers: stickers.clone(),
            ..crate::qq_vision::QqTurnImages::new(inbox.clone())
        });
        bootstrap.image_source = Some(vision.clone());
        bootstrap
            .tools
            .push(Arc::new(crate::qq_files::ReadAttachmentTool::new(
                inbox.clone(),
            )));
        if let Some(entry) = registry
            .get(&ServiceId::new(CONFIG_SERVICE_ID)?)
            .map_err(|error| format!("识图配置服务不可用：{error}"))?
        {
            let vision_settings = entry
                .value
                .downcast::<ConfigServiceHandle>()
                .map_err(|_| "识图配置服务类型错误")?
                .0
                .clone();
            bootstrap
                .tools
                .push(Arc::new(crate::qq_files::DescribeImageTool::new(
                    inbox,
                    vision_settings,
                    bootstrap.api_key.clone(),
                )));
        }
        let raw = Arc::new(crate::qq_files::RawInbox::new());
        plugin = plugin.with_raw_observer(raw.clone());
        bootstrap.tools.push(Arc::new(
            crate::qq_files::ReadRawMessageTool::new(raw).with_archive(
                receipt_archive,
                std::env::var("QQBOT_APP_ID").unwrap_or_else(|_| DEFAULT_QQBOT_APP_ID.into()),
            ),
        ));
        // 参考资料启动时一次读入内存，之后只读；目录无效或超过上限时拒绝启动，不静默缺项。
        if !options.references.is_empty() {
            let specs: Vec<_> = options
                .references
                .iter()
                .map(|(name, directory)| eve_reference_plugin::RootSpec {
                    name: name.clone(),
                    directory: directory.clone(),
                })
                .collect();
            let library = eve_reference_plugin::LocalReferenceLibrary::open(&specs)
                .map_err(|error| format!("参考资料无效：{error}"))?;
            bootstrap
                .tools
                .extend(eve_reference_plugin::tools(Arc::new(library)));
        }
        if options.sleep {
            bootstrap
                .tools
                .push(Arc::new(eve_dialogue_tools::SleepTool));
            bootstrap
                .tools
                .push(Arc::new(eve_dialogue_tools::TaskProgressTool));
            bootstrap
                .host_config
                .tool_timeout_overrides
                .insert("sleep".into(), std::time::Duration::from_secs(305));
        }
        // QQ 多步工具任务默认可连续执行；单次调用总数、取消与超时仍独立约束。
        bootstrap.host_config.max_tool_rounds = 0;
        if let Some(rounds) = options.max_tool_rounds {
            bootstrap.host_config.max_tool_rounds = rounds;
        }
        // 工具批次用尽时不让本轮失败：不再提供工具，模型依据已有结果作答并说明未核实部分。
        bootstrap.host_config.final_answer_on_round_limit = true;
        bootstrap.host_config.max_tool_calls = 50;
        bootstrap.host_config.tool_progress_reports = true;
        let developer_settings = std::env::var_os("EVE_DEVELOPER_CONFIG")
            .map(std::fs::read)
            .transpose()?
            .map(|b| serde_json::from_slice::<eve_developer_api::DeveloperConfig>(&b))
            .transpose()?;
        let public_github = developer_settings
            .as_ref()
            .is_some_and(|c| c.github_all_users);
        let legacy =
            developer_settings.map_or_else(std::collections::BTreeSet::new, |c| c.operators);
        let qq_admin = crate::qq_admin::Admin::open(
            &options.state_directory,
            conversations.clone(),
            legacy,
            registry.clone(),
        )?;
        crate::developer::install_with_authorization(
            &mut bootstrap,
            &options.state_directory,
            Some(qq_admin.clone()),
        )?;
        if let Some(services) = preference_services {
            let mut services = (*services).clone();
            let admin = qq_admin.clone();
            services.owner_check = Some(Arc::new(eve_dialogue_tools::OwnerCheck(
                move |scope: &eve_llm_api::ContextScope| admin.is_owner(scope),
            )));
            bootstrap
                .tools
                .push(Arc::new(eve_dialogue_tools::SavePreferenceTool {
                    services: Arc::new(services),
                }));
        }
        for tool in &mut bootstrap.tools {
            *tool = Arc::new(crate::qq_access::GuardedTool {
                inner: tool.clone(),
                admin: qq_admin.clone(),
                public_github,
            });
        }
        bootstrap.context = Some(Arc::new(crate::qq_admin::AccessContext {
            inner: bootstrap.context.take().ok_or("对话上下文缺失")?,
            admin: qq_admin.clone(),
        }));
        // 子代理在工具执行期内等待结果；按配置上限放宽这一个工具的期限，实际期限由工具按配置控制。
        if options.subagents {
            bootstrap.host_config.tool_timeout_overrides.insert(
                qq_tasks::SUBAGENT_TOOL.into(),
                std::time::Duration::from_millis(qq_tasks::MAX_SUBAGENT_TIMEOUT_MS + 10_000),
            );
        }
        // 执行器与对话共用工具服务和宿主设置；在核心装配之后建立。
        let worker_tools: Vec<String> = bootstrap
            .tools
            .iter()
            .map(|tool| tool.definition().name)
            .collect();
        let worker_config = bootstrap.host_config.clone();
        let worker_permissions = permissions.clone();
        let tool_definitions: Vec<_> = std::iter::once(crate::services::echo_definition())
            .chain(bootstrap.tools.iter().map(|tool| tool.definition()))
            .collect();
        let tools_registered = tool_definitions.len();
        let help_commands = crate::qq_help::Help::handler(tool_definitions.clone());
        bootstrap.context = Some(Arc::new(eve_dialogue_tools::CapabilitiesContext {
            inner: bootstrap.context.take().ok_or("对话上下文缺失")?,
            definitions: tool_definitions,
            source_seeds: research_policy
                .as_ref()
                .map_or_else(Vec::new, |policy| policy.seeds().to_vec()),
            runner: practice.is_some().then(|| skill_runner.clone()),
        }));
        let (control, model_settings_admin) = crate::install_core_with_pages_and_admin(
            &kernel,
            registry.clone(),
            permissions,
            logger,
            &options.state_directory,
            bootstrap,
            options.web_listen.map(|_| page_permit.clone()),
        )
        .await?;
        if let (Some(admin), Some(late)) = (&trigger_admin, &trigger_observations) {
            let sessions = registry
                .get(&ServiceId::new(eve_session_api::SESSION_SERVICE_ID)?)?
                .ok_or("条件唤醒会话服务缺失")?
                .value
                .downcast::<eve_session_api::SessionServiceHandle>()
                .map_err(|_| "条件唤醒会话服务类型错误")?;
            let observations = Arc::new(
                qq_triggers::Observations::new(
                    sessions.0.clone(),
                    tasks
                        .clone()
                        .map(|t| Arc::new(t) as Arc<dyn eve_task_api::TaskAdmin>),
                )?
                .with_authorization(qq_admin.clone()),
            );
            let _ = late.set(observations.clone());
            trigger_background = Some(qq_triggers::Background::start(
                admin.clone(),
                observations,
                control.clone(),
                conversations.clone(),
            ));
        }
        // 配置服务此时可用：读取语义模型角色，无效时拒绝启动；注入后再启动建索引后台。
        let semantic_commands = match &semantic {
            Some((index, embeddings)) => {
                let settings = registry
                    .get(&ServiceId::new(CONFIG_SERVICE_ID)?)?
                    .ok_or("语义召回配置服务缺失")?
                    .value
                    .downcast::<ConfigServiceHandle>()
                    .map_err(|_| "语义召回配置服务类型错误")?;
                let key = std::env::var("EVE_OPENAI_API_KEY").map_err(|_| "缺少模型凭据")?;
                let semantic_key = std::env::var("EVE_SEMANTIC_API_KEY").ok();
                let provider: Arc<dyn eve_semantic_api::EmbeddingProvider> = Arc::new(
                    qq_semantic::configured(settings.0.as_ref(), &key, semantic_key.as_deref())?,
                );
                embeddings.set(provider.clone());
                let started = qq_semantic::Background::start(Indexer::new(
                    memory.clone().ok_or("语义召回缺少记忆服务")?,
                    index.clone(),
                    provider,
                ));
                let commands = started.commands();
                semantic_background = Some(started);
                commands
            }
            None => qq_semantic::Commands::disabled(),
        };
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
                    model_health.register("preference");
                    let resolver = Arc::new(eve_model_health::ObservedResolver::new(
                        Arc::new(
                            crate::models::CoreModelResolver::new(settings.0.clone(), key)
                                .with_use("preference"),
                        ),
                        "preference",
                        model_health.clone(),
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
                interests.as_ref().map(|_| {
                    Arc::new(interest_settings.clone())
                        as Arc<dyn eve_interest_api::InterestSettingsReader>
                }),
                model_health.clone(),
            )
            .await?;
            let commands = started.commands.clone();
            let _ = dialogue_cognition.set(Arc::new(started.admin()?));
            background = Some(started);
            commands
        } else {
            qq_cognition::Commands::disabled()
        };
        // 兴趣观察与受控研究共用主模型配置；只在需要默认模型实现时读取凭据。
        let core_resolver =
            |role: &str| -> Result<Arc<dyn eve_llm_api::LlmModelResolver>, AppError> {
                let settings = registry
                    .get(&ServiceId::new(CONFIG_SERVICE_ID)?)?
                    .ok_or("兴趣学习配置服务缺失")?
                    .value
                    .downcast::<ConfigServiceHandle>()
                    .map_err(|_| "兴趣学习配置服务类型错误")?;
                let key = std::env::var("EVE_OPENAI_API_KEY").map_err(|_| "缺少模型凭据")?;
                let resolver =
                    crate::models::CoreModelResolver::new(settings.0.clone(), key).with_use(role);
                Ok(Arc::new(if role == "summary" {
                    resolver.with_timeout_cap(std::time::Duration::from_secs(29))
                } else {
                    resolver
                }))
            };
        // 后台学习组件的模型请求按用途计入自主运行预算；未启用预算时原样使用主模型。
        // 健康记录包在预算之内：预算拒绝没有发出请求，不算模型的结局。
        let observed = |role: &str| -> Result<Arc<dyn eve_llm_api::LlmModelResolver>, AppError> {
            model_health.register(role);
            Ok(Arc::new(eve_model_health::ObservedResolver::new(
                core_resolver(role)?,
                role,
                model_health.clone(),
            )))
        };
        let budgeted = |category: &str| budget_gate.wrap(observed(category)?, category);
        if let Some(group) = &group {
            let _ = group
                .policy
                .settings
                .set(qq_tasks::config_service(registry.as_ref())?);
            let relations = if options.message_judge == MessageJudgeMode::Jev {
                let judge = crate::qq_message_judge::group_judge(qq_tasks::config_service(
                    registry.as_ref(),
                )?)?;
                group_jev = Some(judge.clone());
                let store = Arc::new(eve_relation_experience_sqlite::SqliteGroupRelations::open(
                    &options.state_directory.join("jev-group-experience.sqlite3"),
                )?);
                Some(Arc::new(eve_chime_plugin::GroupRelationPipeline::new(
                    store,
                    group.store.clone(),
                    judge,
                    Arc::new(eve_chime_plugin::ModelGroupRelationReviewer::new(
                        budget_gate.wrap(observed("group_relation_learning")?, "group-relation")?,
                    )),
                )))
            } else {
                None
            };
            group_background = Some(crate::qq_group::Background::start(
                group.clone(),
                control.clone(),
                budgeted(crate::chime::MODEL_USE)?,
                relations,
            )?);
        }
        // 历史摘要服务于对话本身，与主对话一样不计入自主运行预算；用途 digest 单独记健康与输出上限。
        let _ = chat_style
            .settings
            .set(qq_tasks::config_service(registry.as_ref())?);
        let _ = chat_timing
            .settings
            .set(qq_tasks::config_service(registry.as_ref())?);
        let _ = slow_progress
            .settings
            .set(qq_tasks::config_service(registry.as_ref())?);
        let _ = vision
            .settings
            .set(qq_tasks::config_service(registry.as_ref())?);
        if let Some(stickers) = &stickers {
            let _ = stickers
                .settings
                .set(qq_tasks::config_service(registry.as_ref())?);
        }
        let _ = digest_policy
            .settings
            .set(qq_tasks::config_service(registry.as_ref())?);
        let active_digester = Arc::new(eve_history_digest::ModelHistoryDigester::new(observed(
            crate::qq_history_digest::MODEL_USE,
        )?));
        history_digests.install_compactor(active_digester.clone())?;
        digest_background = Some(crate::qq_history_digest::Background::start(
            history_digests.clone(),
            active_digester,
            digest_policy.clone(),
        ));
        // 表情包审看：用途 sticker 单独记健康与输出上限；模型需要支持视觉输入。
        if let Some(stickers) = &stickers {
            sticker_background = Some(crate::qq_stickers::Background::start(
                stickers.clone(),
                Arc::new(eve_sticker_plugin::ModelStickerReviewer::new(observed(
                    crate::qq_stickers::MODEL_USE,
                )?)),
            ));
        }
        // 人物印象：此人谈及自己的原话攒够一批后学习一次；用途 impression 单独记健康与输出上限。
        let _ = impression_policy
            .settings
            .set(qq_tasks::config_service(registry.as_ref())?);
        impression_background = Some(crate::qq_impressions::Background::start(
            people.clone(),
            Arc::new(eve_people_plugin::ModelImpressionLearner::new(observed(
                crate::qq_impressions::MODEL_USE,
            )?)),
            impression_policy.clone(),
        ));
        // 说法学习：训练中的会话攒够一批原话后学习一次；用途 phrase 单独记健康与输出上限。
        let _ = phrase_policy
            .settings
            .set(qq_tasks::config_service(registry.as_ref())?);
        phrase_background = Some(crate::qq_phrases::Background::start(
            training.0.clone(),
            Arc::new(eve_training_plugin::ModelPhraseLearner::new(observed(
                crate::qq_phrases::MODEL_USE,
            )?)),
            phrase_policy.clone(),
        ));
        // 后台任务与子代理共用执行器：用途 task 单独记健康与输出上限，不计入自主运行预算。
        if tasks.is_some() || options.subagents {
            let _ = task_late
                .config
                .set(qq_tasks::config_service(registry.as_ref())?);
            let worker = qq_tasks::Worker::install(
                &kernel,
                registry.clone(),
                worker_permissions.clone(),
                &worker_tools,
                worker_config.clone(),
                observed(qq_tasks::MODEL_USE)?,
                history_digests.clone(),
            )
            .await?;
            worker.authorize_with(qq_admin.clone());
            let _ = task_late.worker.set(worker);
        }
        let failure_recovery = if let Some(tasks) = &tasks {
            let recovery = Arc::new(crate::qq_failure_recovery::Recovery {
                tasks: Arc::new(tasks.clone()),
                sessions: registry
                    .get(&ServiceId::new(SESSION_SERVICE_ID)?)?
                    .ok_or("失败恢复所需会话服务缺失")?
                    .value
                    .downcast::<SessionServiceHandle>()
                    .map_err(|_| "失败恢复会话服务类型错误")?
                    .0
                    .clone(),
                settings: qq_tasks::config_service(registry.as_ref())?,
                authorization: qq_admin.clone(),
                digests: history_digests.clone(),
            });
            if !options.disable_error_recovery {
                plugin = plugin.with_failure_recovery(recovery.clone());
            }
            task_background = Some(qq_tasks::Background::start(
                Arc::new(tasks.clone()),
                task_late.clone(),
            ));
            Some(recovery)
        } else {
            None
        };
        let InterestComponents {
            summarizer,
            observer: interest_observer,
            fetcher,
            selector,
            extractor: knowledge_extractor,
            drafter: practice_drafter,
            runner: practice_runner,
            distiller: skill_distiller,
            skill_selector,
            forger: tool_forger,
            plan_proposer,
            composer: invitation_composer,
            timing_judge,
            response_judge,
        } = interest_components;
        if let (Some(memory), Some(summaries)) = (&memory, &summaries) {
            let summarizer = match summarizer {
                Some(summarizer) => summarizer,
                None => Arc::new(ModelConversationSummarizer::new(budgeted("summary")?)),
            };
            summary_background = Some(qq_summary::Background::start(
                memory.clone(),
                summaries.clone(),
                summarizer,
                registry.clone(),
                budget_gate.clone(),
            )?);
        }
        // 计划只能使用操作者已开启的受控研究与实践验证；步骤由对应后台执行。
        let plan_journal: Option<Arc<dyn PlanJournal>> = plans
            .clone()
            .map(|plans| Arc::new(plans) as Arc<dyn PlanJournal>);
        let plan_commands = if let Some(journal) = &plan_journal {
            let capabilities = qq_plan::capabilities(knowledge.is_some(), practice.is_some());
            let proposer: Arc<dyn PlanProposer> = match plan_proposer {
                Some(proposer) => proposer,
                None => Arc::new(ModelPlanProposer::new(observed("plan")?)),
            };
            let cognition: Arc<dyn eve_cognition_api::CognitionAdmin> =
                Arc::new(background.as_ref().ok_or("计划入口缺少认知服务")?.admin()?);
            let sessions = registry
                .get(&ServiceId::new(SESSION_SERVICE_ID)?)?
                .ok_or("计划入口需要会话服务")?
                .value
                .downcast::<SessionServiceHandle>()
                .map_err(|_| "计划入口会话服务类型错误")?;
            let started = qq_plan::Background::start(qq_plan::Services {
                plans: journal.clone(),
                cognition: cognition.clone(),
                knowledge: knowledge
                    .clone()
                    .map(|knowledge| Arc::new(knowledge) as Arc<dyn KnowledgeAdmin>),
                practice: practice
                    .clone()
                    .map(|practice| Arc::new(practice) as Arc<dyn PracticeAdmin>),
                proposer: proposer.clone(),
                capabilities: capabilities.clone(),
            });
            let commands = qq_plan::Commands::enabled(
                journal.clone(),
                cognition,
                sessions.0.clone(),
                capabilities,
                proposer.version().into(),
                started.requests(),
            );
            plan_background = Some(started);
            commands
        } else {
            qq_plan::Commands::disabled()
        };
        let interest_commands = if let Some(interests) = &interests {
            let memory = memory.clone().ok_or("兴趣观察缺少记忆服务")?;
            let cognition = Arc::new(background.as_ref().ok_or("兴趣观察缺少认知服务")?.admin()?);
            let observer = match interest_observer {
                Some(observer) => observer,
                None => Arc::new(ModelInterestObserver::new(budgeted(qq_budget::INTEREST)?)),
            };
            let deriver = Arc::new(LearningGoalDeriver::new(cognition.clone(), "eve")?);
            let dirty = Arc::new(std::sync::atomic::AtomicBool::new(true));
            interest_background = Some(qq_interest::Background::start(
                memory,
                Arc::new(interests.clone()),
                observer,
                deriver.clone(),
                Arc::new(interest_settings.clone()),
                dirty.clone(),
                budget_gate.clone(),
            )?);
            qq_interest::Commands::enabled(
                Arc::new(interests.clone()),
                deriver,
                cognition.clone(),
                dirty,
                pursuit.clone(),
            )
        } else {
            qq_interest::Commands::disabled()
        };
        let research_commands = if let (Some(knowledge), Some(policy), Some(interests)) =
            (&knowledge, research_policy, &interests)
        {
            let cognition = Arc::new(background.as_ref().ok_or("受控研究缺少认知服务")?.admin()?);
            let fetcher: Arc<dyn SourceFetcher> = match fetcher {
                Some(fetcher) => routed(fetcher),
                None => dialogue_fetcher.clone().ok_or("受控来源读取器缺失")?,
            };
            let selector: Arc<dyn SourceSelector> = match selector {
                Some(selector) => selector,
                None => Arc::new(ModelSourceSelector::new(budgeted(qq_budget::RESEARCH)?)),
            };
            let extractor: Arc<dyn KnowledgeExtractor> = match knowledge_extractor {
                Some(extractor) => extractor,
                None => Arc::new(ModelKnowledgeExtractor::new(budgeted(qq_budget::RESEARCH)?)),
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
                plan_journal.clone(),
                budget_gate.clone(),
            ));
            qq_research::Commands::enabled(
                Arc::new(interests.clone()),
                Arc::new(knowledge.clone()),
                practice
                    .clone()
                    .map(|practice| Arc::new(practice) as Arc<dyn PracticeAdmin>),
            )
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
                None => Arc::new(ModelPracticeDrafter::new(budgeted(qq_budget::PRACTICE)?)),
            };
            let skill_parts = match &skills {
                Some(skills) => {
                    let admin: Arc<dyn SkillAdmin> = Arc::new(skills.clone());
                    let selector: Arc<dyn SkillSelector> = match skill_selector {
                        Some(selector) => selector,
                        None => Arc::new(ModelSkillSelector::new(budgeted(qq_budget::SKILL)?)),
                    };
                    let distiller: Arc<dyn SkillDistiller> = match skill_distiller {
                        Some(distiller) => distiller,
                        None => Arc::new(ModelSkillDistiller::new(budgeted(qq_budget::SKILL)?)),
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
                        consolidator: Arc::new(
                            Consolidator::new(admin, distiller, runner.clone()).with_correction(),
                        ),
                    })
                }
                None => None,
            };
            let mut practitioner =
                Practitioner::new(Arc::new(practice.clone()), drafter, runner.clone());
            if let Some(strategy) = &strategies {
                practitioner = practitioner.with_strategy(strategy.clone())?;
            }
            let mut forged_check: Option<Arc<dyn eve_practice_api::DraftCheck>> = None;
            let forging = match &tools {
                Some(tools) => {
                    let admin: Arc<dyn ToolAdmin> = Arc::new(tools.clone());
                    let forger: Arc<dyn ToolForger> = match tool_forger {
                        Some(forger) => forger,
                        None => Arc::new(ModelToolForger::new(budgeted(qq_budget::FORGE)?)),
                    };
                    // 已启用的锻造工具在实际运行前检查草稿；拦下的草稿不运行。
                    forged_check = Some(Arc::new(ForgedChecks::new(admin.clone())));
                    Some(qq_practice::Forging {
                        forge: Arc::new(Forge::new(
                            admin.clone(),
                            forger,
                            runner.profile().clone(),
                        )),
                        admin,
                    })
                }
                None => None,
            };
            if let Some(check) = forged_check {
                practitioner = practitioner.with_check(check);
            }
            let practitioner = Arc::new(practitioner);
            practice_background = Some(qq_practice::Background::start(qq_practice::Services {
                practice: Arc::new(practice.clone()),
                cognition,
                knowledge: knowledge
                    .clone()
                    .map(|knowledge| Arc::new(knowledge) as Arc<dyn KnowledgeAdmin>),
                practitioner,
                workspace_root: options.state_directory.join("practice-work"),
                skills: skill_parts,
                strategy: strategies.clone(),
                forging,
                plans: plan_journal.clone(),
                gate: budget_gate.clone(),
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
        let tool_commands = tools
            .as_ref()
            .map_or_else(qq_tools::Commands::disabled, |tools| {
                qq_tools::Commands::enabled(Arc::new(tools.clone()))
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
                None => Arc::new(ModelInvitationComposer::new(budgeted(qq_budget::OUTREACH)?)),
            };
            let responder: Arc<dyn ResponseJudge> = match response_judge {
                Some(judge) => judge,
                None => Arc::new(ModelResponseJudge::new(budgeted(qq_budget::OUTREACH)?)),
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
                gate: budget_gate.clone(),
            }));
            qq_outreach::Commands::enabled(Arc::new(outreach.clone()), Arc::new(practice.clone()))
        } else {
            qq_outreach::Commands::disabled()
        };
        let clarify_commands = if let (Some(clarify), Some(interests)) = (&clarify, &interests) {
            let cognition: Arc<dyn eve_cognition_api::CognitionAdmin> =
                Arc::new(background.as_ref().ok_or("请求澄清缺少认知服务")?.admin()?);
            clarify_background = Some(qq_clarify::Background::start(qq_clarify::Services {
                clarify: Arc::new(clarify.clone()),
                cognition,
                interests: Arc::new(interests.clone()),
                memory: memory.clone().ok_or("请求澄清缺少记忆服务")?,
                knowledge: knowledge
                    .clone()
                    .map(|knowledge| Arc::new(knowledge) as Arc<dyn KnowledgeAdmin>),
                practice: practice
                    .clone()
                    .map(|practice| Arc::new(practice) as Arc<dyn PracticeAdmin>),
                composer: Arc::new(ModelClarificationComposer::new(budgeted(
                    qq_budget::CLARIFY,
                )?)),
                gate: budget_gate.clone(),
            }));
            qq_clarify::Commands::enabled(Arc::new(clarify.clone()))
        } else {
            qq_clarify::Commands::disabled()
        };
        if options.learning_stop {
            let pursuit = pursuit.clone().ok_or("学习目标停止缺少兴趣学习")?;
            pursuit_background = Some(qq_pursuit::Background::start(pursuit));
        }
        let memory_commands =
            memory
                .as_ref()
                .map_or_else(qq_memory::Commands::disabled, |memory| {
                    qq_memory::Commands::with_continuity(memory.clone(), memory_continuity.clone())
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
        let mut plugin = plugin.with_command_handler(Arc::new(crate::qq_access::GuardedCommands {
            admin: qq_admin.clone(),
            inner: Arc::new(CommandHandlers(vec![
                qq_admin.clone(),
                Arc::new(crate::qq_model::Commands {
                    settings: qq_tasks::config_service(registry.as_ref())?,
                    admin: Arc::new(model_settings_admin),
                    authorization: qq_admin.clone(),
                }),
                Arc::new(crate::qq_people::Commands(people)),
                Arc::new(crate::qq_failure_recovery::Commands(failure_recovery)),
                help_commands,
                commands,
                memory_commands,
                learning_commands,
                interest_commands,
                research_commands,
                practice_commands,
                skill_commands,
                tool_commands,
                plan_commands,
                semantic_commands,
                budget_gate.commands(),
                outreach_commands,
                clarify_commands,
                segment_commands,
            ])),
        }));
        // 邀请与请求澄清共用通道的投递点：邀请优先判断，投递结果按 ID 前缀交回各自账本。
        let mut sources: Vec<(&'static str, Arc<dyn QqOutreach>)> = Vec::new();
        // 提醒是用户自己要的，最先投递。
        if let Some(reminders) = &reminders {
            sources.push((
                qq_reminder::ID_PREFIX,
                Arc::new(qq_reminder::ChannelAdapter {
                    reminders: Arc::new(reminders.clone()),
                }),
            ));
        }
        // 后台任务的结束通知同样是用户要的，紧随提醒投递。
        if let Some(tasks) = &tasks {
            sources.push((
                qq_tasks::ID_PREFIX,
                Arc::new(qq_tasks::ChannelAdapter {
                    tasks: Arc::new(tasks.clone()),
                }),
            ));
        }
        // 待办完成确认只随回复附带，不主动私聊。
        if let (Some(completion), Some(plans)) = (&completion, &completion_plans) {
            sources.push((
                eve_completion_api::ID_PREFIX,
                Arc::new(qq_completion::ChannelAdapter {
                    cognition: dialogue_cognition.clone(),
                    plans: plans.clone(),
                    ledger: Arc::new(completion.clone()),
                }),
            ));
        }
        if let Some(outreach) = &outreach {
            let judge: Arc<dyn TimingJudge> = match timing_judge {
                Some(judge) => judge,
                None => Arc::new(ModelTimingJudge::new(budgeted(qq_budget::OUTREACH)?)),
            };
            sources.push((
                "invite-",
                Arc::new(qq_outreach::ChannelAdapter {
                    outreach: Arc::new(outreach.clone()) as Arc<dyn OutreachAdmin>,
                    judge,
                    policy: outreach_policy,
                    gate: budget_gate.clone(),
                }),
            ));
        }
        if let Some(clarify) = &clarify {
            sources.push((
                qq_clarify::ID_PREFIX,
                Arc::new(qq_clarify::ChannelAdapter {
                    clarify: Arc::new(clarify.clone()),
                    judge: Arc::new(ModelTimingJudge::new(budgeted(qq_budget::CLARIFY)?)),
                    policy: eve_clarify_api::ClarifyPolicy::default(),
                    gate: budget_gate.clone(),
                }),
            ));
        }
        if let Some(admin) = &trigger_admin {
            sources.push((
                "wake-",
                Arc::new(qq_triggers::ChannelAdapter(
                    admin.clone(),
                    trigger_observations.clone().ok_or("条件观察服务缺失")?,
                )),
            ));
        }
        // 群插话最后投递，优先保障提醒、后台任务结果与已请求的条件通知。
        if let Some(group) = &group {
            sources.push((crate::qq_group::PREFIX, group.clone()));
        }
        if !sources.is_empty() {
            // 主动私聊开启时，通道空闲后每 2 秒询问一次；是否发送由宿主策略决定。请求澄清不主动私聊。
            let push = (group.is_some()
                || trigger_admin.is_some()
                || reminders.is_some()
                || tasks.is_some()
                || outreach.is_some() && outreach_policy.proactive_after_ms.is_some())
            .then(|| std::time::Duration::from_secs(2));
            let adapter: Arc<dyn QqOutreach> = if sources.len() == 1 {
                sources.remove(0).1
            } else {
                Arc::new(qq_clarify::Combined(sources))
            };
            plugin = plugin.with_outreach(
                Arc::new(crate::qq_access::GuardedOutreach {
                    inner: adapter,
                    admin: qq_admin.clone(),
                }),
                push,
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
        let mut relation = match group_jev {
            Some(judge) => {
                crate::qq_message_judge::relation_plugin_shared(settings.0.clone(), judge)?
            }
            None => {
                crate::qq_message_judge::relation_plugin(options.message_judge, settings.0.clone())?
            }
        };
        if options.message_judge != crate::qq_message_judge::MessageJudgeMode::Off {
            let experience = eve_relation_experience_sqlite::SqliteRelationExperience::open(
                &options.state_directory.join("jev-experience.sqlite3"),
            )
            .map_err(|_| "Jev 经验数据库不可用；保留原文件，不自动清空或重建")?;
            relation = relation.with_experience(Arc::new(experience));
        }
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
                    autonomy: interests.clone().map(|interests| {
                        crate::web_panel_autonomy::AutonomyRead {
                            interests: Arc::new(interests),
                            knowledge: knowledge.clone().map(|knowledge| {
                                Arc::new(knowledge) as Arc<dyn eve_knowledge_api::KnowledgeAdmin>
                            }),
                            practice: practice.clone().map(|practice| {
                                Arc::new(practice) as Arc<dyn eve_practice_api::PracticeAdmin>
                            }),
                            skills: skills
                                .clone()
                                .map(|skills| Arc::new(skills) as Arc<dyn SkillAdmin>),
                            outreach: outreach
                                .clone()
                                .map(|outreach| Arc::new(outreach) as Arc<dyn OutreachAdmin>),
                        }
                    }),
                    plans: plan_journal.clone(),
                    readiness: crate::web_panel::PanelReadiness {
                        health: model_health.clone(),
                        settings: registry
                            .get(&ServiceId::new(CONFIG_SERVICE_ID)?)?
                            .ok_or("面板配置服务缺失")?
                            .value
                            .downcast::<ConfigServiceHandle>()
                            .map_err(|_| "面板配置服务类型错误")?
                            .0
                            .clone(),
                        tools_registered,
                    },
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
        if let Some(clarify) = &clarify_background {
            clarify.activate();
        }
        if let Some(pursuit) = &pursuit_background {
            pursuit.activate();
        }
        if let Some(plans) = &plan_background {
            plans.activate();
        }
        if let Some(semantic) = &semantic_background {
            semantic.activate();
        }
        if let Some(summary) = &summary_background {
            summary.activate();
        }
        if let Some(digests) = &digest_background {
            digests.activate();
        }
        if let Some(impressions) = &impression_background {
            impressions.activate();
        }
        if let Some(stickers) = &sticker_background {
            stickers.activate();
        }
        if let Some(group) = &group_background {
            group.activate();
        }
        if let Some(phrases) = &phrase_background {
            phrases.activate();
        }
        if let Some(tasks) = &task_background {
            tasks.activate();
        }
        if let Some(triggers) = &trigger_background {
            triggers.activate();
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
                .chain(clarify_background.as_ref().map(|clarify| {
                    (
                        clarify.finished(),
                        "请求澄清后台已结束；QQ 通道停止准入并保留状态",
                    )
                }))
                .chain(pursuit_background.as_ref().map(|pursuit| {
                    (
                        pursuit.finished(),
                        "学习目标停止后台已结束；QQ 通道停止准入并保留状态",
                    )
                }))
                .chain(plan_background.as_ref().map(|plans| {
                    (
                        plans.finished(),
                        "计划后台已结束；QQ 通道停止准入并保留状态",
                    )
                }))
                .chain(semantic_background.as_ref().map(|semantic| {
                    (
                        semantic.finished(),
                        "语义索引后台已结束；QQ 通道停止准入并保留状态",
                    )
                }))
                .chain(summary_background.as_ref().map(|summary| {
                    (
                        summary.finished(),
                        "语义摘要后台已结束；QQ 通道停止准入并保留状态",
                    )
                }))
                .chain(digest_background.as_ref().map(|digests| {
                    (
                        digests.finished(),
                        "历史摘要后台已结束；QQ 通道停止准入并保留状态",
                    )
                }))
                .chain(impression_background.as_ref().map(|impressions| {
                    (
                        impressions.finished(),
                        "印象学习后台已结束；QQ 通道停止准入并保留状态",
                    )
                }))
                .chain(sticker_background.as_ref().map(|stickers| {
                    (
                        stickers.finished(),
                        "表情包审看后台已结束；QQ 通道停止准入并保留状态",
                    )
                }))
                .chain(group_background.as_ref().map(|group| {
                    (
                        group.finished(),
                        "群聊观察后台已结束；QQ 通道停止准入并保留状态",
                    )
                }))
                .chain(phrase_background.as_ref().map(|phrases| {
                    (
                        phrases.finished(),
                        "说法学习后台已结束；QQ 通道停止准入并保留状态",
                    )
                }))
                .chain(trigger_background.as_ref().map(|triggers| {
                    (
                        triggers.finished(),
                        "条件唤醒后台已结束；QQ 通道停止准入并保留状态",
                    )
                }))
                .chain(task_background.as_ref().map(|tasks| {
                    (
                        tasks.finished(),
                        "后台任务执行已结束；QQ 通道停止准入并保留状态",
                    )
                }))
                .collect(),
            panel.as_ref().map(eve_web_panel::LocalPanel::finished),
            serde_json::json!({
                "channel": "connected",
                "tools_registered": tools_registered,
                "model": "unverified",
                "vision": vision.readiness(),
            })
            .to_string(),
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
    if let Some(clarify) = &clarify_background {
        clarify.request_stop();
    }
    if let Some(pursuit) = &pursuit_background {
        pursuit.request_stop();
    }
    if let Some(plans) = &plan_background {
        plans.request_stop();
    }
    if let Some(semantic) = &semantic_background {
        semantic.request_stop();
    }
    if let Some(summary) = &summary_background {
        summary.request_stop();
    }
    if let Some(digests) = &digest_background {
        digests.request_stop();
    }
    if let Some(impressions) = &impression_background {
        impressions.request_stop();
    }
    if let Some(group) = &group_background {
        group.request_stop();
    }
    if let Some(stickers) = &sticker_background {
        stickers.request_stop();
    }
    if let Some(phrases) = &phrase_background {
        phrases.request_stop();
    }
    if let Some(triggers) = &trigger_background {
        triggers.request_stop();
    }
    if let Some(tasks) = &task_background {
        tasks.request_stop();
    }
    if let Some(queries) = semantic_queries
        && let Err(error) = queries.close().await
    {
        secondary.push(error.into());
    }
    if let Some(triggers) = trigger_background
        && let Err(error) = triggers.stop().await
    {
        secondary.push(error);
    }
    if let Some(summary) = summary_background
        && let Err(error) = summary.stop().await
    {
        secondary.push(error);
    }
    if let Some(digests) = digest_background
        && let Err(error) = digests.stop().await
    {
        secondary.push(error);
    }
    if let Some(impressions) = impression_background
        && let Err(error) = impressions.stop().await
    {
        secondary.push(error);
    }
    if let Some(stickers) = sticker_background
        && let Err(error) = stickers.stop().await
    {
        secondary.push(error);
    }
    if let Some(group) = group_background
        && let Err(error) = group.stop().await
    {
        secondary.push(error);
    }
    if let Some(phrases) = phrase_background
        && let Err(error) = phrases.stop().await
    {
        secondary.push(error);
    }
    // 后台任务的执行器会调用其他能力的工具；先停下执行中的任务（留在执行中，下次启动记为中断）。
    if let Some(tasks) = task_background
        && let Err(error) = tasks.stop().await
    {
        secondary.push(error);
    }
    // 语义索引后台读取记忆并写索引账本；请求失败不写入，先结束进行中的请求。
    if let Some(semantic) = semantic_background
        && let Err(error) = semantic.stop().await
    {
        secondary.push(error);
    }
    // 计划后台读取研究、实践与认知状态并写计划账本；先结束进行中的建议请求。
    if let Some(plans) = plan_background
        && let Err(error) = plans.stop().await
    {
        secondary.push(error);
    }
    // 学习目标停止只读各账本并写认知状态；先结束，避免与其他后台的收尾交错。
    if let Some(pursuit) = pursuit_background
        && let Err(error) = pursuit.stop().await
    {
        secondary.push(error);
    }
    // 请求澄清读取学习目标、研究与实践并写澄清账本；先写入取消结局。
    if let Some(clarify) = clarify_background
        && let Err(error) = clarify.stop().await
    {
        secondary.push(error);
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

#[allow(clippy::too_many_arguments)]
async fn wait_channel(
    mut status: watch::Receiver<QqBotStatus>,
    background: Option<watch::Receiver<bool>>,
    learning: Option<watch::Receiver<bool>>,
    interest: Option<watch::Receiver<bool>>,
    research: Option<watch::Receiver<bool>>,
    later: Vec<(watch::Receiver<bool>, &'static str)>,
    panel: Option<watch::Receiver<bool>>,
    readiness: String,
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
            // 通道就绪只说明网关已连接；模型要等实际请求才知道能否完成。
            eprintln!("EVE_READINESS {readiness}");
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
    fn tool_forging_is_explicit_and_requires_a_practice_runtime() {
        assert!(!parse(&[]).unwrap().tool_forging);
        let error = parse(&["--interest-learning", "--tool-forging"])
            .unwrap_err()
            .to_string();
        assert!(error.contains("--tool-forging"));
        let options = parse(&[
            "--interest-learning",
            "--practice-browser",
            "chrome",
            "--tool-forging",
        ])
        .unwrap();
        assert!(options.tool_forging && !options.skill_learning);
    }

    #[test]
    fn semantic_recall_is_explicit_and_requires_memory_recall() {
        assert!(!parse(&[]).unwrap().semantic_recall);
        let error = parse(&["--memory", "--semantic-recall"])
            .unwrap_err()
            .to_string();
        assert!(error.contains("--memory-recall"));
        let options = parse(&["--memory", "--memory-recall", "--semantic-recall"]).unwrap();
        assert!(options.semantic_recall && options.memory_recall);
    }

    #[test]
    fn agenda_context_is_explicit_and_requires_interest_learning() {
        assert!(!parse(&[]).unwrap().agenda_context);
        let error = parse(&["--agenda-context"]).unwrap_err().to_string();
        assert!(error.contains("--interest-learning"));
        assert!(
            parse(&["--interest-learning", "--agenda-context"])
                .unwrap()
                .agenda_context
        );
    }

    #[test]
    fn learning_stop_is_explicit_and_requires_interest_learning() {
        assert!(!parse(&[]).unwrap().learning_stop);
        let error = parse(&["--learning-stop"]).unwrap_err().to_string();
        assert!(error.contains("--interest-learning"));
        assert!(
            parse(&["--interest-learning", "--learning-stop"])
                .unwrap()
                .learning_stop
        );
    }

    #[test]
    fn background_tasks_and_subagents_are_explicit() {
        let options = parse(&[]).unwrap();
        assert!(!options.background_tasks && !options.subagents);
        let options = parse(&["--background-tasks", "--subagents"]).unwrap();
        assert!(options.background_tasks && options.subagents);
    }

    #[test]
    fn reminders_are_explicit_and_the_offset_is_bounded() {
        let options = parse(&[]).unwrap();
        assert!(!options.reminders);
        assert!(parse(&["--reminder-utc-offset", "+08:00"]).is_err());
        let options = parse(&["--reminders", "--reminder-utc-offset", "-05:30"]).unwrap();
        assert_eq!(options.reminder_utc_offset_minutes, Some(-330));
        for bad in ["8", "+8:00", "+15:00", "-12:30", "+08:60", "08:00"] {
            assert!(
                parse(&["--reminders", "--reminder-utc-offset", bad]).is_err(),
                "{bad}"
            );
        }
        assert_eq!(parse_utc_offset("+14:00"), Some(840));
    }

    #[test]
    fn goal_completion_is_explicit_and_requires_plans() {
        assert!(!parse(&[]).unwrap().goal_completion);
        let error = parse(&["--goal-completion"]).unwrap_err().to_string();
        assert!(error.contains("--plans"));
        let options = parse(&[
            "--interest-learning",
            "--research-source",
            "https://docs.example/wiki/",
            "--plans",
            "--goal-completion",
        ])
        .unwrap();
        assert!(options.plans && options.goal_completion);
    }

    #[test]
    fn clarify_is_explicit_and_requires_research_or_a_practice_runtime() {
        assert!(!parse(&[]).unwrap().clarify);
        let error = parse(&["--interest-learning", "--clarify"])
            .unwrap_err()
            .to_string();
        assert!(error.contains("--research-source"));
        let options = parse(&[
            "--interest-learning",
            "--research-source",
            "https://example.org/docs/",
            "--clarify",
        ])
        .unwrap();
        assert!(options.clarify);
    }

    #[test]
    fn autonomy_budget_is_explicit_bounded_and_requires_interest_learning() {
        let options = parse(&[]).unwrap();
        assert_eq!(
            (
                options.autonomy_budget,
                options.autonomy_budget_window_hours
            ),
            (None, None)
        );
        let error = parse(&["--autonomy-budget", "10"]).unwrap_err().to_string();
        assert!(error.contains("--interest-learning"));
        let error = parse(&["--interest-learning", "--autonomy-budget-window-hours", "6"])
            .unwrap_err()
            .to_string();
        assert!(error.contains("--autonomy-budget"));
        for bad in ["0", "1000001", "-1", "x"] {
            assert!(parse(&["--interest-learning", "--autonomy-budget", bad]).is_err());
        }
        for bad in ["0", "721", "1.5"] {
            assert!(
                parse(&[
                    "--interest-learning",
                    "--autonomy-budget",
                    "5",
                    "--autonomy-budget-window-hours",
                    bad
                ])
                .is_err()
            );
        }
        let options = parse(&[
            "--interest-learning",
            "--autonomy-budget",
            "40",
            "--autonomy-budget-window-hours",
            "6",
        ])
        .unwrap();
        assert_eq!(
            (
                options.autonomy_budget,
                options.autonomy_budget_window_hours
            ),
            (Some(40), Some(6))
        );
    }

    #[test]
    fn plans_are_explicit_and_require_research_or_a_practice_runtime() {
        assert!(!parse(&[]).unwrap().plans);
        let error = parse(&["--interest-learning", "--plans"])
            .unwrap_err()
            .to_string();
        assert!(error.contains("--plans"));
        let research = parse(&[
            "--interest-learning",
            "--research-source",
            "https://docs.example/wiki/",
            "--plans",
        ])
        .unwrap();
        assert!(research.plans);
        let practice = parse(&[
            "--interest-learning",
            "--practice-mindustry-server",
            "server.jar",
            "--plans",
        ])
        .unwrap();
        assert!(practice.plans);
    }

    #[test]
    fn markdown_is_opt_in_and_off_by_default() {
        assert!(!parse(&[]).unwrap().markdown);
        assert!(!parse(&["--segmented"]).unwrap().markdown);
        assert!(parse(&["--markdown"]).unwrap().markdown);
    }

    #[test]
    fn references_and_tool_rounds_are_parsed_and_validated() {
        let options = parse(&[
            "--reference",
            "mindustry=/src/Mindustry",
            "--reference",
            "eve-portable=vendor/eve-portable-knowledge",
            "--max-tool-rounds",
            "8",
        ])
        .unwrap();
        assert_eq!(
            options.references,
            vec![
                ("mindustry".into(), PathBuf::from("/src/Mindustry")),
                (
                    "eve-portable".into(),
                    PathBuf::from("vendor/eve-portable-knowledge")
                ),
            ]
        );
        assert_eq!(options.max_tool_rounds, Some(8));
        let defaults = parse(&[]).unwrap();
        assert!(defaults.references.is_empty() && defaults.max_tool_rounds.is_none());
        for (args, expected) in [
            (vec!["--reference", "Mindustry=/src"], "名称=目录"),
            (vec!["--reference", "mindustry"], "名称=目录"),
            (vec!["--reference", "mindustry="], "名称=目录"),
            (vec!["--reference", "a=/x", "--reference", "a=/y"], "重复"),
            (vec!["--max-tool-rounds", "0"], "1 至 64"),
            (vec!["--max-tool-rounds", "65"], "1 至 64"),
        ] {
            let error = parse(&args).unwrap_err().to_string();
            assert!(error.contains(expected), "{args:?}: {error}");
        }
    }

    #[test]
    fn local_corpus_seeds_require_the_matching_vendored_corpus() {
        let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../vendor/eve-portable-knowledge")
            .to_string_lossy()
            .into_owned();
        let seed = "eve-corpus://eve-portable/mindustry/";
        assert!(parse(&[]).unwrap().research_corpus.is_none());
        for (args, expected) in [
            (
                vec!["--interest-learning", "--research-source", seed],
                "--research-corpus",
            ),
            (
                vec![
                    "--interest-learning",
                    "--research-corpus",
                    &corpus,
                    "--research-source",
                    "https://docs.example/",
                ],
                "eve-corpus://",
            ),
        ] {
            assert!(parse(&args).unwrap_err().to_string().contains(expected));
        }
        let options = parse(&[
            "--interest-learning",
            "--research-corpus",
            &corpus,
            "--research-source",
            seed,
        ])
        .unwrap();
        let opened = research_corpus(&options).unwrap().unwrap();
        assert_eq!(opened.name(), "eve-portable");
        let foreign = parse(&[
            "--interest-learning",
            "--research-corpus",
            &corpus,
            "--research-source",
            "eve-corpus://other/notes/",
        ])
        .unwrap();
        assert!(
            research_corpus(&foreign)
                .unwrap_err()
                .to_string()
                .contains("不属于本地资料")
        );
        let missing = parse(&[
            "--interest-learning",
            "--research-corpus",
            "/nonexistent",
            "--research-source",
            seed,
        ])
        .unwrap();
        assert!(
            research_corpus(&missing)
                .unwrap_err()
                .to_string()
                .contains("本地资料无效")
        );
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
