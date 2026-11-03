//! 产品入口自动装配可用能力；最小组合仍供嵌入宿主与显式隔离测试使用。
//!
//! 自动发现不下载资源，不授予身份、预算或额外权限。资料必须通过清单校验，浏览器
//! 必须完成有期限的离线探测；资源缺失或损坏只影响相应能力，不冒充可用或清空原文件。
use crate::{AppError, QqBotOptions};
use eve_llm_api::{
    Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext, ToolExecutionError,
    ToolFuture, ToolValidationError,
};
use serde_json::{Value, json};
use std::{
    ffi::{OsStr, OsString},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

const PRACTICE_MISSING: &str = "未找到通过离线探测的实践运行环境；可由主人或管理员通过开发工具补齐浏览器，或用 acquire_condition 取得并验证 Mindustry 服务端，再重启装配。已有技能记录仍可查看，尚未验证的运行不能报告成功。";
const CORPUS_MISSING: &str = "未找到通过清单校验的随包本地资料；仍可按用户任务读取公开网页。可由开发工具补齐固定版本资料及清单，重新校验后装配。";
const DEFAULT_FEATURE_FLAGS: &[&str] = &[
    "--memory",
    "--training",
    "--memory-recall",
    "--self-learning",
    "--interest-learning",
    "--cognition",
    "--agenda-context",
    "--reminders",
    "--background-tasks",
    "--subagents",
    "--triggers",
    "--sleep",
    "--workflow-tools",
    "--health-watch",
];

#[derive(Clone, Debug)]
pub(crate) struct DiscoveredCorpus {
    pub(crate) directory: PathBuf,
    pub(crate) seed: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ResourceDiscovery {
    pub(crate) corpus: Option<DiscoveredCorpus>,
    pub(crate) browser: Option<PathBuf>,
    pub(crate) unavailable: Vec<String>,
}

impl QqBotOptions {
    /// 产品二进制使用的入口：无需功能开关。`--minimal-profile` 显式选择隔离组合。
    /// `parse` 与 `Default` 继续保持宿主逐项组合语义，避免嵌入程序意外启动后台。
    pub fn parse_product(
        args: impl IntoIterator<Item = OsString>,
    ) -> Result<Option<Self>, AppError> {
        let (mut args, minimal) = product_arguments(args);
        if !minimal {
            // 先交给原 parser 解析默认能力，使时区、研究来源、后台验收目录等参数
            // 不再错误要求用户重复开启产品本已提供的依赖开关。
            args.splice(0..0, DEFAULT_FEATURE_FLAGS.iter().map(OsString::from));
        }
        Ok(Self::parse(args)?.map(|options| {
            if minimal {
                options
            } else {
                options.with_automatic_capabilities()
            }
        }))
    }

    /// 使用当前宿主实际存在且通过校验的资源，补齐产品默认能力。
    pub fn with_automatic_capabilities(mut self) -> Self {
        let mut roots = Vec::new();
        if let Ok(directory) = std::env::current_dir() {
            roots.push(directory);
        }
        if let Ok(binary) = std::env::current_exe()
            && let Some(directory) = binary.parent()
            && !roots.iter().any(|root| root == directory)
        {
            roots.push(directory.to_path_buf());
        }
        let discovery = discover_resources(&roots, std::env::var_os("PATH").as_deref());
        self.apply_automatic_capabilities(discovery);
        self
    }

    fn apply_automatic_capabilities(&mut self, resources: ResourceDiscovery) {
        self.automatic_capabilities = true;
        self.training = true;
        self.group_chat = true;
        self.memory = true;
        self.memory_recall = true;
        self.memory_learning = true;
        self.self_learning = true;
        self.interest_learning = true;
        self.cognition = true;
        self.segmented = true;
        self.agenda_context = true;
        self.reminders = true;
        self.background_tasks = true;
        self.subagents = true;
        self.triggers = true;
        self.sleep = true;
        self.workflow_tools = true;
        self.health_watch = true;
        self.max_tool_rounds.get_or_insert(8);
        // 不添加公网种子，也不替换操作者明确指定的资料范围。
        if self.research_sources.is_empty()
            && self.research_corpus.is_none()
            && let Some(corpus) = resources.corpus
        {
            self.research_sources.push(corpus.seed);
            self.research_corpus = Some(corpus.directory);
        }
        if self.practice_browser.is_none() && self.practice_server_jar.is_none() {
            self.practice_browser = resources.browser;
        }
        self.capability_notices = resources.unavailable;
        if self.research_corpus.is_some() {
            self.capability_notices
                .retain(|notice| notice != CORPUS_MISSING);
        }
        self.enable_automatic_practice_capabilities();
        if !self.research_sources.is_empty() {
            self.plans = true;
            self.clarify = true;
            self.goal_completion = true;
        }
    }

    /// 条件账本恢复出已验证运行环境后再调用；没有运行环境时保持账本管理可用。
    pub(crate) fn enable_automatic_practice_capabilities(&mut self) {
        if !self.automatic_capabilities {
            return;
        }
        if self.practice_server_jar.is_some() || self.practice_browser.is_some() {
            self.skill_learning = true;
            self.strategy_learning = true;
            self.tool_forging = true;
            self.plans = true;
            self.clarify = true;
            self.goal_completion = true;
            self.outreach = true;
            self.capability_notices
                .retain(|notice| notice != PRACTICE_MISSING);
        }
    }
}

/// 不把 `--bridge-arg --minimal-profile` 这种参数值误认为产品模式开关。
fn product_arguments(args: impl IntoIterator<Item = OsString>) -> (Vec<OsString>, bool) {
    let mut args = args.into_iter();
    let mut explicit = Vec::new();
    let mut minimal = false;
    while let Some(argument) = args.next() {
        if argument == "--minimal-profile" {
            minimal = true;
            continue;
        }
        let flag = matches!(
            argument.to_str(),
            Some(
                "--help"
                    | "-h"
                    | "--sleep"
                    | "--triggers"
                    | "--group-chat"
                    | "--training"
                    | "--cognition"
                    | "--memory"
                    | "--memory-recall"
                    | "--semantic-recall"
                    | "--memory-learning"
                    | "--segmented"
                    | "--markdown"
                    | "--self-learning"
                    | "--interest-learning"
                    | "--strategy-learning"
                    | "--skill-learning"
                    | "--tool-forging"
                    | "--workflow-tools"
                    | "--health-watch"
                    | "--plans"
                    | "--outreach"
                    | "--clarify"
                    | "--agenda-context"
                    | "--stickers"
                    | "--reminders"
                    | "--background-tasks"
                    | "--no-error-recovery"
                    | "--subagents"
                    | "--goal-completion"
                    | "--learning-stop"
            )
        );
        explicit.push(argument);
        if !flag && let Some(value) = args.next() {
            explicit.push(value);
        }
    }
    (explicit, minimal)
}

/// 只检查宿主指定的根目录和 PATH，便于其它产品入口复用并进行确定性的隔离验收。
pub(crate) fn discover_resources(roots: &[PathBuf], path: Option<&OsStr>) -> ResourceDiscovery {
    let mut found = ResourceDiscovery::default();
    for root in roots {
        let directory = root.join("vendor/eve-portable-knowledge");
        if !directory.join("MANIFEST.json").is_file() {
            continue;
        }
        match eve_knowledge_plugin::CorpusSourceFetcher::open(&directory) {
            Ok(corpus) => {
                found.corpus = Some(DiscoveredCorpus {
                    directory,
                    seed: format!("eve-corpus://{}/", corpus.name()),
                });
                break;
            }
            Err(_) => found.unavailable.push(
                "随包本地资料未通过清单与内容校验；原资料保留，可由开发工具核对并修复后重启。"
                    .into(),
            ),
        }
    }
    if found.corpus.is_none() {
        found.unavailable.push(CORPUS_MISSING.into());
    }
    found.browser = browser_candidates(path)
        .into_iter()
        .take(3)
        .find(|program| browser_probe(program, Duration::from_secs(5)));
    if found.browser.is_none() {
        found.unavailable.push(PRACTICE_MISSING.into());
    }
    found
}

fn browser_candidates(path: Option<&OsStr>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let Some(path) = path else {
        return candidates;
    };
    for directory in std::env::split_paths(path).filter(|directory| directory.is_absolute()) {
        for name in [
            "chromium",
            "chromium-browser",
            "google-chrome",
            "chrome.exe",
            "msedge.exe",
        ] {
            let candidate = directory.join(name);
            if !executable_file(&candidate) {
                continue;
            }
            if let Ok(candidate) = candidate.canonicalize()
                && !candidates.contains(&candidate)
            {
                candidates.push(candidate);
            }
        }
    }
    candidates
}

fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > 512 * 1024 * 1024 {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// 浏览器能启动并输出离线页面才算可用；禁止后台网络，所有临时文件限定在临时目录。
fn browser_probe(program: &Path, timeout: Duration) -> bool {
    const MARKER: &str = "eve-browser-profile-probe";
    let Ok(workspace) = tempfile::tempdir() else {
        return false;
    };
    let mut command = Command::new(program);
    command
        .args([
            "--headless=new",
            "--no-sandbox",
            "--disable-gpu",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-sync",
            "--host-resolver-rules=MAP * ~NOTFOUND",
        ])
        .arg(format!(
            "--user-data-dir={}",
            workspace.path().join("profile").display()
        ))
        .arg("--dump-dom")
        .arg(format!("data:text/html,<p>{MARKER}</p>"))
        .current_dir(workspace.path())
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for name in ["PATH", "SystemRoot", "SYSTEMROOT"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    for name in ["HOME", "USERPROFILE", "TEMP", "TMP"] {
        command.env(name, workspace.path());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };
    // 持续排空但只保留固定前缀，坏运行环境不能把探测日志写满磁盘或内存。
    let (sender, output) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut kept = Vec::with_capacity(16 * 1024);
        let mut buffer = [0; 4096];
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let count = count.min((16 * 1024_usize).saturating_sub(kept.len()));
                    kept.extend_from_slice(&buffer[..count]);
                }
            }
        }
        let _ = sender.send(kept);
    });
    let deadline = Instant::now() + timeout;
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                #[cfg(unix)]
                // SAFETY: pid 来自本次创建的独立进程组，只清理本次有期限探测的进程。
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    #[cfg(unix)]
    // SAFETY: 正常退出时也清理本次独立进程组，防止浏览器留下探测子进程。
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    if !success {
        return false;
    }
    output
        .recv_timeout(Duration::from_millis(250))
        .is_ok_and(|contents| String::from_utf8_lossy(&contents).contains(MARKER))
}

/// 工具清单直接来自最终组合，缺环境数据不能被误读为已装配工具或授权。
pub(crate) fn install_discovery_tool(
    tools: &mut Vec<Arc<dyn Tool>>,
    options: &QqBotOptions,
    excluded: &std::collections::BTreeSet<String>,
) {
    install_scoped_report_tool(
        tools,
        options.practice_server_jar.is_some() || options.practice_browser.is_some(),
        options.research_corpus.is_some(),
        options.capability_notices.clone(),
        excluded,
    );
}

/// Web 与其它入口共享实际注册清单，不依赖 QQ 的组合参数。
pub(crate) fn install_scoped_report_tool(
    tools: &mut Vec<Arc<dyn Tool>>,
    practice_available: bool,
    corpus_available: bool,
    notices: Vec<String>,
    excluded: &std::collections::BTreeSet<String>,
) {
    let mut installed_tools: Vec<_> = std::iter::once(crate::services::echo_definition().name)
        .chain(tools.iter().map(|tool| tool.definition().name))
        .chain(std::iter::once("inspect_capabilities".into()))
        .filter(|name| !excluded.contains(name))
        .collect();
    installed_tools.sort();
    installed_tools.dedup();
    let mut unavailable = Vec::new();
    if !practice_available {
        unavailable.push(json!({"capability":"practice","reason":PRACTICE_MISSING,
            "recovery":"主人或管理员可使用现有开发工具或 acquire_condition 补齐并验证运行环境；重启后自动装配。"}));
    }
    if !corpus_available {
        unavailable.push(json!({"capability":"local_corpus","reason":CORPUS_MISSING,
            "recovery":"修复或补齐随包固定版本资料后，重新通过清单校验。"}));
    }
    tools.push(Arc::new(DiscoveryTool {
        installed_tools,
        unavailable,
        notices,
    }));
}

struct DiscoveryTool {
    installed_tools: Vec<String>,
    unavailable: Vec<Value>,
    notices: Vec<String>,
}
impl Tool for DiscoveryTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "inspect_capabilities".into(),
            description: "查询本进程实际装配的对话工具及缺失运行条件。用户直接问能做什么、某能力为何不可用时使用；工具装配不代表当前身份已获授权，所有调用仍经身份、审核和预算检查；缺环境可通过开发工具补齐并验证。".into(),
            argument_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::ParallelSafe),
        }
    }
    fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolValidationError> {
        if arguments
            .as_object()
            .is_none_or(|object| !object.is_empty())
        {
            return Err(ToolValidationError {
                message: "能力查询不接受参数".into(),
            });
        }
        Ok(())
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            self.validate_arguments(&call.arguments)
                .map_err(|error| ToolExecutionError::Rejected(error.message))?;
            if context.is_cancel_requested() {
                return Err(ToolExecutionError::Cancelled("能力查询已取消".into()));
            }
            Ok(
                json!({"installed_tools":self.installed_tools,"unavailable":self.unavailable,
                "notices":self.notices,"authorization":"工具仍按当前可信身份、动作审核和预算准入；不依据此清单授予权限。"}),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_profile_keeps_missing_resources_distinct_from_registered_capabilities() {
        let mut options = QqBotOptions::default();
        options.apply_automatic_capabilities(ResourceDiscovery::default());
        assert!(
            options.memory && options.memory_recall && options.cognition && options.self_learning
        );
        assert!(options.reminders && options.background_tasks && options.subagents);
        assert!(
            options.triggers && options.sleep && options.workflow_tools && options.health_watch
        );
        assert!(options.automatic_capabilities);
        assert!(!options.skill_learning && !options.tool_forging && !options.plans);
        assert!(options.group_chat && options.training && !options.markdown);
        assert!(options.autonomy_budget.is_none() && options.outreach_proactive_after_ms.is_none());
        assert!(options.research_sources.is_empty());
    }

    #[test]
    fn verified_provision_runtime_enables_full_practice_chain_without_overwriting_limits() {
        let mut options = QqBotOptions {
            autonomy_budget: Some(9),
            max_tool_rounds: Some(3),
            ..Default::default()
        };
        options.apply_automatic_capabilities(ResourceDiscovery::default());
        options.practice_server_jar = Some("verified-server.jar".into());
        options.enable_automatic_practice_capabilities();
        assert!(options.skill_learning && options.strategy_learning && options.tool_forging);
        assert!(options.plans && options.goal_completion && options.clarify && options.outreach);
        assert_eq!(options.autonomy_budget, Some(9));
        assert_eq!(options.max_tool_rounds, Some(3));
    }

    #[test]
    fn explicit_debug_profile_is_not_inferred_from_credentials_or_model_host() {
        let options = QqBotOptions::parse_product([OsString::from("--minimal-profile")])
            .unwrap()
            .unwrap();
        assert!(!options.automatic_capabilities && !options.background_tasks);
        let (args, minimal) =
            product_arguments(["--bridge-arg", "--minimal-profile"].map(OsString::from));
        assert!(!minimal);
        assert_eq!(
            args,
            ["--bridge-arg", "--minimal-profile"].map(OsString::from)
        );
    }

    #[test]
    fn product_default_dependencies_accept_settings_without_feature_switches() {
        for args in [
            vec!["--reminder-utc-offset", "+08:00"],
            vec!["--autonomy-budget", "9"],
            vec!["--task-strategy-catalog", "catalog.json"],
            vec!["--research-source", "https://example.com/docs/"],
            vec!["--practice-browser", "browser"],
        ] {
            let (mut arguments, minimal) = product_arguments(args.into_iter().map(OsString::from));
            assert!(!minimal);
            arguments.splice(0..0, DEFAULT_FEATURE_FLAGS.iter().map(OsString::from));
            assert!(QqBotOptions::parse(arguments).is_ok());
        }
    }

    #[test]
    fn invalid_corpus_and_absent_program_leave_original_data_and_report_missing() {
        let directory = tempfile::tempdir().unwrap();
        let corpus = directory.path().join("vendor/eve-portable-knowledge");
        std::fs::create_dir_all(&corpus).unwrap();
        std::fs::write(corpus.join("MANIFEST.json"), "broken").unwrap();
        let resources = discover_resources(&[directory.path().to_path_buf()], Some(OsStr::new("")));
        assert!(resources.corpus.is_none() && resources.browser.is_none());
        assert!(
            resources
                .unavailable
                .iter()
                .any(|notice| notice.contains("校验"))
        );
        assert_eq!(
            std::fs::read_to_string(corpus.join("MANIFEST.json")).unwrap(),
            "broken"
        );
    }

    #[test]
    fn known_good_bundled_corpus_is_discovered_without_a_network_seed() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let resources = discover_resources(&[root], None);
        let corpus = resources.corpus.expect("仓库随包资料必须通过清单校验");
        assert_eq!(corpus.seed, "eve-corpus://eve-portable/");
    }

    #[cfg(unix)]
    #[test]
    fn browser_presence_is_not_execution_evidence_and_probe_has_a_deadline() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("chromium");
        std::fs::write(&program, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!browser_probe(&program, Duration::from_secs(1)));
        std::fs::write(&program, "#!/bin/sh\nwhile :; do :; done\n").unwrap();
        let began = Instant::now();
        assert!(!browser_probe(&program, Duration::from_millis(50)));
        assert!(began.elapsed() < Duration::from_secs(2));
        std::fs::write(
            &program,
            "#!/bin/sh\nprintf '<p>eve-browser-profile-probe</p>'\n",
        )
        .unwrap();
        assert!(browser_probe(&program, Duration::from_secs(1)));
    }

    #[cfg(unix)]
    #[test]
    fn browser_probe_drains_large_output_and_reaps_successful_programs_children() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("chromium");
        std::fs::write(&program, concat!(
            "#!/bin/sh\nprintf '<p>eve-browser-profile-probe</p>'\n",
            "i=0; while [ $i -lt 5000 ]; do printf '0123456789012345678901234567890123456789'; i=$((i+1)); done\n"
        )).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(browser_probe(&program, Duration::from_secs(2)));
        // stdout 被后台子进程继承；只结束父进程会使读取端持续等待。
        std::fs::write(
            &program,
            concat!(
                "#!/bin/sh\n(while :; do :; done) &\n",
                "printf '<p>eve-browser-profile-probe</p>'\nexit 0\n"
            ),
        )
        .unwrap();
        let began = Instant::now();
        assert!(browser_probe(&program, Duration::from_secs(1)));
        assert!(began.elapsed() < Duration::from_secs(2));
    }
}
