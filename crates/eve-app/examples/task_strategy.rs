//! 仅用于 task_strategy_process_test.py 的可信固定目录宿主，不是生产 CLI。
//! 本地模型替身、fake bridge、临时工作区和合成身份均须匹配；不创建任务。
use eve_app::{AppError, QqBotOptions};
use eve_developer_api::DeveloperConfig;
use eve_task_api::*;
use eve_task_plugin::{PairedTaskStrategyPolicy, task_strategy_views};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs, path::PathBuf, sync::Arc};

const STEPS: u32 = 4;
const CASES: [&str; 3] = ["train", "holdout-a", "holdout-b"];
fn hash(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn owner() -> TaskOwner {
    let identity = format!(
        "qq:{}",
        task_strategy_digest(&("1904159860", "c2c", "user-1", "user-1"))
    );
    TaskOwner {
        session_id: identity.clone(),
        user_id: identity,
    }
}
#[derive(Debug)]
struct FixtureEnvironment {
    agent: PathBuf,
    developer: PathBuf,
    provider: String,
}
impl FixtureEnvironment {
    fn current(&self) -> TaskResult<TaskStrategyScope> {
        let agent = fs::read(&self.agent).map_err(|_| TaskError::Unavailable)?;
        let developer: Value =
            serde_json::from_slice(&fs::read(&self.developer).map_err(|_| TaskError::Unavailable)?)
                .map_err(|_| TaskError::InvalidInput)?;
        Ok(TaskStrategyScope {
            owner: owner(),
            family: "isolated-file-repair".into(),
            runtime_sha256: task_strategy_digest(&(
                "task-strategy-fixture:v1",
                env!("CARGO_PKG_VERSION"),
                hash(&agent),
                developer,
                &self.provider,
            )),
            evaluator_version: RECEIPT_EVALUATOR_VERSION.into(),
        })
    }
}
impl TaskStrategyEnvironment for FixtureEnvironment {
    fn scope(&self, task: &Task) -> TaskResult<Option<TaskStrategyScope>> {
        if task.owner != owner() {
            return Ok(None);
        }
        self.current().map(Some)
    }
}
fn case(scope: &TaskStrategyScope, name: &str) -> TaskStrategyCase {
    let instructions = format!("task-strategy-fixture:{name}");
    TaskStrategyCase {
        scope: scope.clone(),
        input_sha256: task_strategy_digest(&(instructions.trim(), STEPS, None::<Value>)),
        contract: TaskEvaluationContract {
            require_final_batch: true,
            checks: vec![TaskReceiptCheck {
                tool: "read_file".into(),
                arguments: json!({"path":format!("{name}.txt"),"offset":0}),
                output_pointer: "/sha256".into(),
                expected: json!(hash(b"correct\n")),
            }],
        },
    }
}
async fn run() -> Result<(), AppError> {
    let mut options =
        QqBotOptions::parse(std::env::args_os().skip(1))?.ok_or("测试宿主需要实际启动参数")?;
    let provider = std::env::var("EVE_OPENAI_BASE_URL")?;
    let local_port = provider
        .strip_prefix("http://127.0.0.1:")
        .and_then(|p| p.parse::<u16>().ok());
    let expected_bridge = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../connectors/qqbot/test/fake-bridge.mjs")
        .canonicalize()?;
    if std::env::var("EVE_TASK_STRATEGY_FIXTURE").as_deref() != Ok("1")
        || std::env::var("QQBOT_APP_ID").as_deref() != Ok("1904159860")
        || std::env::var("QQBOT_APP_SECRET").as_deref() != Ok("test-app-secret")
        || std::env::var("EVE_OPENAI_API_KEY").as_deref() != Ok("test-model-secret")
        || local_port.is_none_or(|p| p == 0)
        || options.bridge_script.canonicalize()? != expected_bridge
        || !options.background_tasks
        || options.database_config.is_some()
    {
        return Err("只允许隔离的本地策略测试配置".into());
    }
    let developer_path =
        PathBuf::from(std::env::var_os("EVE_DEVELOPER_CONFIG").ok_or("缺少测试开发配置")?)
            .canonicalize()?;
    let root = developer_path.parent().ok_or("测试目录无效")?;
    let developer: DeveloperConfig = serde_json::from_slice(&fs::read(&developer_path)?)?;
    if fs::read(root.join("task-strategy-fixture.marker"))? != b"isolated-task-strategy-v1"
        || options.state_directory != root.join("state")
        || developer.workspace.canonicalize()? != root.join("source").canonicalize()?
        || developer.commands
        || developer.github
        || developer.self_upgrade
        || developer.live_upgrade.is_some()
        || developer.ordinary_access
        || developer.operators != BTreeSet::from([owner().user_id])
    {
        return Err("测试宿主拒绝非固定目录或额外开发能力".into());
    }
    let environment = Arc::new(FixtureEnvironment {
        agent: options.agent_path.clone(),
        developer: developer_path,
        provider,
    });
    let scope = environment.current()?;
    let policy = PairedTaskStrategyPolicy::new(
        environment,
        vec![],
        CASES[1..].iter().map(|name| case(&scope, name)).collect(),
    )?
    .with_training_cases(vec![case(&scope, CASES[0])])?;
    options.task_strategy_policy = Some(Arc::new(policy));
    let state_path = options.state_directory.join("state.json");
    let mut summary = serde_json::to_value(eve_app::run_qqbot(options).await?)?;
    // 进程结束后只读输出真实账本推导的状态，测试不自行假定两对成功就是已晋升。
    let state: Value = serde_json::from_slice(&fs::read(state_path)?)?;
    let bytes: Vec<u8> = serde_json::from_value(
        state["entries"][TASK_STRATEGY_PLUGIN_ID][TASK_STRATEGY_STATE_KEY].clone(),
    )?;
    let ledger: Value = serde_json::from_slice(&bytes)?;
    let snapshot: TaskStrategySnapshot = serde_json::from_value(ledger["snapshot"].clone())?;
    summary["task_strategy_fixture"] = json!({"views": task_strategy_views(&snapshot)?.iter().map(|v|
        json!({"version":v.strategy.version,"status":v.status,"paired_successes":v.paired_successes})).collect::<Vec<_>>()});
    println!("{}", serde_json::to_string(&summary)?);
    Ok(())
}
#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("策略隔离验收：{error}");
        std::process::exit(1);
    }
}
