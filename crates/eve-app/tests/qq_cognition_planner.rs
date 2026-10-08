//! 在隔离进程中调用公开 Rust 宿主入口；替身 QQ 桥接不发送任何模型请求。
use eve_app::{CognitionOptions, QqBotOptions, run_cognition, run_qqbot_with_planner_factory};
use eve_cognition_api::{CognitionAdmin, GoalStatus, ReadAccess, SourceKind};
use eve_cognition_loop_api::{
    EndogenousOptions, EndogenousPlannerFactory, EndogenousPlanning, EndogenousReport, LoopError,
    LoopResult,
};
use eve_kernel::backends::FileStateStore;
use serde_json::Value;
use std::{
    ffi::OsString,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

const SECRET: &str = "qq-planner-fixture-secret";
const BRIDGE: &str = r#"
import fs from 'node:fs';
import net from 'node:net';
import readline from 'node:readline';
const [started, stopped, mode] = process.argv.slice(2);
const probe = net.createServer();
readline.createInterface({input:process.stdin}).on('line', line => {
  const command = JSON.parse(line);
  if (command.type === 'stop' && mode !== 'reconcile-unresponsive') {
    fs.writeFileSync(stopped, 'stopped');
    process.exit(0);
  }
});
probe.listen(0, '127.0.0.1', () => {
  // 已监听 stdin 并持有探测端口后，才允许测试注入运行期故障。
  fs.writeFileSync(started, String(probe.address().port));
  process.stdout.write(JSON.stringify({type:'ready',version:1})+'\n');
});
setTimeout(() => process.exit(mode === 'success' || mode === 'off' ? 0 : 2),
  mode === 'success' || mode === 'off' ? 1000 : 20000);
"#;

struct Process(Option<Child>);
impl Drop for Process {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 每个用例都启动完整宿主和 Node 桥接子进程；并行运行时，负载较高的运行器（例如 Windows CI）
/// 可能让桥接来不及在宿主的停止宽限期内响应。串行执行只消除测试之间的资源竞争，不放宽宿主行为。
static PROCESS_CASES: Mutex<()> = Mutex::new(());

fn process_case(case: &str) {
    let _serial = PROCESS_CASES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "planner_process_fixture",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    // 子进程独立设置配置，不在并行 Rust 测试之间修改全局环境。
    for (name, _) in std::env::vars_os() {
        if name
            .to_str()
            .is_some_and(|name| name.starts_with("EVE_") || name.starts_with("QQBOT_"))
        {
            command.env_remove(name);
        }
    }
    command
        .env("EVE_QQ_PLANNER_CASE", case)
        .env("QQBOT_APP_SECRET", SECRET)
        .env("QQBOT_APP_ID", "planner-fixture")
        .env("QQBOT_SANDBOX", "true")
        .env("EVE_OPENAI_API_KEY", SECRET)
        .env(
            "EVE_OPENAI_BASE_URL",
            format!("http://{}", listener.local_addr().unwrap()),
        )
        .env("EVE_OPENAI_PROTOCOL", "responses")
        .env("EVE_OPENAI_MODEL", "planner-fixture")
        .env("EVE_OPENAI_TIMEOUT_SECONDS", "1")
        .env("EVE_OPENAI_MAX_OUTPUT_TOKENS", "128")
        .env("EVE_OPENAI_REASONING_EFFORT", "none")
        .env("EVE_LLM_RESPONSE_MODE", "complete")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut process = Process(Some(command.spawn().unwrap()));
    // 只是防止测试无限挂起的安全期限；宿主的停止语义由下方断言检查。
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if process.0.as_mut().unwrap().try_wait().unwrap().is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "QQ 规划宿主未及时关闭");
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = process.0.take().unwrap().wait_with_output().unwrap();
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "持有待办的替代规划器不应调用模型"
    );
    for bytes in [&output.stdout, &output.stderr] {
        assert!(
            !String::from_utf8_lossy(bytes).contains(SECRET),
            "不得输出凭据"
        );
    }
    assert!(
        output.status.success(),
        "case={case}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn disabled_cognition_never_creates_the_injected_factory() {
    process_case("off");
}

#[test]
fn custom_planner_controls_qq_after_recovery_with_no_model_or_state_mutation() {
    process_case("success");
}

#[test]
fn factory_error_stops_startup_and_releases_state() {
    process_case("create-error");
}

#[test]
fn planner_error_stops_bridge_and_releases_state() {
    process_case("reconcile-error");
}

#[test]
fn factory_panic_stops_startup_and_releases_state() {
    process_case("create-panic");
}

#[test]
fn planner_panic_stops_bridge_and_releases_state() {
    process_case("reconcile-panic");
}

#[test]
fn planner_error_reaps_unresponsive_bridge_and_releases_state() {
    process_case("reconcile-unresponsive");
}

struct Factory {
    case: String,
    creates: Arc<AtomicUsize>,
    ticks: Arc<AtomicUsize>,
    marker: PathBuf,
}
impl EndogenousPlannerFactory for Factory {
    fn create(
        &self,
        admin: Arc<dyn CognitionAdmin>,
        options: EndogenousOptions,
    ) -> LoopResult<Arc<dyn EndogenousPlanning>> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        assert_ne!(self.case, "off", "关闭时不得调用工厂");
        assert!(!self.marker.exists(), "工厂在 QQ 进程启动前装配");
        options.validate()?;
        assert_eq!(options.scope.subject_id, "eve");
        assert_eq!(options.scope.access, ReadAccess::Internal);
        assert_eq!(options.scope.sources.len(), 1);
        assert_eq!(options.scope.sources[0].kind, SourceKind::User);
        assert_eq!(options.scope.sources[0].channel, "qq.goal");
        assert_eq!(options.max_derivations, 2);
        assert_eq!(options.timeout_ms, 30_000);
        let restored = admin.snapshot()?;
        assert_eq!(restored.state.goals["restored"].status, GoalStatus::Waiting);
        match self.case.as_str() {
            "create-error" => return Err(LoopError::Unavailable),
            "create-panic" => panic!("injected factory panic"),
            _ => {}
        }
        Ok(Arc::new(Planner {
            admin,
            case: self.case.clone(),
            ticks: self.ticks.clone(),
            marker: self.marker.clone(),
        }))
    }
}

struct Planner {
    admin: Arc<dyn CognitionAdmin>,
    case: String,
    ticks: Arc<AtomicUsize>,
    marker: PathBuf,
}
impl EndogenousPlanning for Planner {
    fn reconcile(&self, now_ms: u64) -> LoopResult<EndogenousReport> {
        assert!(now_ms > 0);
        if !self.marker.exists() {
            // 后台规划可能早于 Node ready；运行期故障测试须等桥接已可接收 stop。
            return Ok(EndogenousReport {
                created_goal_ids: Vec::new(),
                invalidated_goal_ids: Vec::new(),
                revision: self.admin.snapshot()?.revision,
            });
        }
        self.ticks.fetch_add(1, Ordering::SeqCst);
        match self.case.as_str() {
            "reconcile-error" | "reconcile-unresponsive" => return Err(LoopError::Unavailable),
            "reconcile-panic" => panic!("injected reconcile panic"),
            _ => {}
        }
        Ok(EndogenousReport {
            created_goal_ids: Vec::new(),
            invalidated_goal_ids: Vec::new(),
            revision: self.admin.snapshot()?.revision,
        })
    }
}

fn stored_cognition(directory: &Path) -> Value {
    let state: Value =
        serde_json::from_slice(&std::fs::read(directory.join("state.json")).unwrap()).unwrap();
    state["entries"]["eve.cognition"].clone()
}

#[tokio::test]
#[ignore = "由父测试使用隔离环境调用，不单独连接实际 QQ 或模型"]
async fn planner_process_fixture() {
    let case = std::env::var("EVE_QQ_PLANNER_CASE").expect("只由隔离父测试调用");
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("state");
    let agent = root.path().join("AGENT.md");
    let bridge = root.path().join("bridge.mjs");
    let started = root.path().join("started");
    let stopped = root.path().join("stopped");
    std::fs::write(&agent, "只生成本地受限草稿。").unwrap();
    std::fs::write(&bridge, BRIDGE).unwrap();
    let seed = CognitionOptions::parse([
        OsString::from("--state-dir"),
        directory.as_os_str().into(),
        OsString::from("add"),
        OsString::from("--id"),
        OsString::from("restored"),
        OsString::from("--text"),
        OsString::from("待用户确认的既有事项"),
    ])
    .unwrap()
    .unwrap();
    run_cognition(seed).await.unwrap();
    let before = stored_cognition(&directory);
    let creates = Arc::new(AtomicUsize::new(0));
    let ticks = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(Factory {
        case: case.clone(),
        creates: creates.clone(),
        ticks: ticks.clone(),
        marker: started.clone(),
    });
    let options = QqBotOptions {
        state_directory: directory.clone(),
        agent_path: agent,
        bridge_script: bridge,
        bridge_args: vec![
            started.as_os_str().into(),
            stopped.as_os_str().into(),
            case.clone().into(),
        ],
        cognition: case != "off",
        cognition_max_executions: 2,
        ..QqBotOptions::default()
    };
    let result = run_qqbot_with_planner_factory(options, factory).await;
    assert_eq!(creates.load(Ordering::SeqCst), usize::from(case != "off"));
    if case == "success" || case == "off" {
        let summary = result.unwrap();
        assert!(summary.ready && summary.closed && !summary.terminal_error);
        assert_eq!(
            (
                summary.received,
                summary.completed,
                summary.sent,
                summary.failed
            ),
            (0, 0, 0, 0)
        );
        assert!(started.exists());
        assert_eq!(ticks.load(Ordering::SeqCst) > 0, case == "success");
    } else {
        assert!(result.is_err(), "工厂/规划错误必须关闭宿主");
        if case.starts_with("create-") {
            assert!(!started.exists());
            assert_eq!(ticks.load(Ordering::SeqCst), 0);
        } else {
            assert_eq!(ticks.load(Ordering::SeqCst), 1);
            assert!(started.exists());
            assert_eq!(stopped.exists(), case != "reconcile-unresponsive");
        }
    }
    if started.exists() {
        let port: u16 = std::fs::read_to_string(&started).unwrap().parse().unwrap();
        let probe = TcpListener::bind(("127.0.0.1", port))
            .expect("桥接必须实际退出并释放探测端口，包括不响应 stop 的子进程");
        drop(probe);
    }
    assert_eq!(stored_cognition(&directory), before);
    let reopened = FileStateStore::open(&directory).expect("所有失败和正常出口均须释放状态目录锁");
    drop(reopened);
}
