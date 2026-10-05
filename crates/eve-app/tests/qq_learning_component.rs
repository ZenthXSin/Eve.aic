//! 公开宿主组件替换：真实 Session/Memory 恢复、Node 生命周期与离线提炼器。
use eve_app::{QqBotOptions, run_qqbot_with_components};
use eve_cognition_loop_plugin::ReflectionPlannerFactory;
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_learning_api::{
    CandidateDraft, JobStatus, LEARNING_PLUGIN_ID, LEARNING_STATE_KEY, LearningBatch,
    LearningFuture, LearningJob, PreferenceExtractor,
};
use eve_llm_api::{ChatMessage, ChatRole};
use eve_memory_api::{
    CompletedInteraction, MEMORY_PLUGIN_ID, MemoryAdmin, MemoryScope, MemorySnapshot,
};
use eve_memory_plugin::{MEMORY_STATE_KEY, MemoryPlugin};
use eve_plugin_api::ServiceId;
use eve_session_api::{SESSION_SERVICE_ID, SessionInput, SessionKey, SessionServiceHandle};
use eve_session_plugin::SessionPlugin;
use serde_json::Value;
use std::{
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

const SECRET: &str = "qq-learning-component-fixture-secret";
const BRIDGE: &str = r#"
import fs from 'node:fs';
import net from 'node:net';
import readline from 'node:readline';
const [started, stopped, state, mode] = process.argv.slice(2);
const probe = net.createServer();
readline.createInterface({input:process.stdin}).on('line', line => {
  if (JSON.parse(line).type === 'stop') {
    fs.writeFileSync(stopped, 'stopped');
    process.exit(0);
  }
});
probe.listen(0, '127.0.0.1', () => {
  fs.writeFileSync(started, String(probe.address().port));
  process.stdout.write(JSON.stringify({type:'ready',version:1})+'\n');
  if (mode === 'off') setTimeout(() => process.exit(0), 500);
  if (mode === 'success') setInterval(() => {
    const entries = JSON.parse(fs.readFileSync(state)).entries;
    const bytes = entries['eve.learning']?.['learning.v1'];
    if (bytes && JSON.parse(Buffer.from(bytes)).jobs.some(item => item.job.status === 'Completed')) {
      process.exit(0);
    }
  }, 10);
});
setTimeout(() => process.exit(2), 15000);
"#;

// 仅结束本测试创建并持有的子进程；不读取或发送任意 PID 信号。
struct Process(Option<Child>);
impl Drop for Process {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn process_case(case: &str) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "learning_component_process_fixture",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    // 环境变量仅在拥有的隔离进程内生效，不影响并行宿主测试。
    for (name, _) in std::env::vars_os() {
        if name
            .to_str()
            .is_some_and(|name| name.starts_with("EVE_") || name.starts_with("QQBOT_"))
        {
            command.env_remove(name);
        }
    }
    command
        .env("EVE_QQ_LEARNING_COMPONENT_CASE", case)
        .env("QQBOT_APP_SECRET", SECRET)
        .env("QQBOT_APP_ID", "learning-component-fixture")
        .env("QQBOT_SANDBOX", "true")
        .env("EVE_OPENAI_API_KEY", SECRET)
        .env(
            "EVE_OPENAI_BASE_URL",
            format!("http://{}", listener.local_addr().unwrap()),
        )
        .env("EVE_OPENAI_PROTOCOL", "responses")
        .env("EVE_OPENAI_MODEL", "learning-component-fixture")
        .env("EVE_OPENAI_TIMEOUT_SECONDS", "1")
        .env("EVE_OPENAI_MAX_OUTPUT_TOKENS", "128")
        .env("EVE_OPENAI_REASONING_EFFORT", "none")
        .env("EVE_LLM_RESPONSE_MODE", "complete")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut process = Process(Some(command.spawn().unwrap()));
    let deadline = Instant::now() + Duration::from_secs(20);
    while process.0.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "提炼组件宿主未及时关闭");
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = process.0.take().unwrap().wait_with_output().unwrap();
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "替代提炼器和关闭的认知不得调用模型"
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
fn disabled_learning_never_touches_injected_extractor_despite_saved_evidence() {
    process_case("off");
}

#[test]
fn injected_extractor_reads_recovered_evidence_and_persists_only_a_candidate() {
    process_case("success");
}

#[test]
fn invalid_injected_candidate_stops_bridge_and_preserves_reserved_batch() {
    process_case("invalid");
}

fn stored(directory: &Path, owner: &str, key: &str) -> Option<Value> {
    let state: Value =
        serde_json::from_slice(&std::fs::read(directory.join("state.json")).unwrap()).unwrap();
    let bytes: Vec<u8> =
        serde_json::from_value(state["entries"].get(owner)?.get(key)?.clone()).unwrap();
    Some(serde_json::from_slice(&bytes).unwrap())
}

async fn seed_memory(directory: &Path) -> MemorySnapshot {
    let services = KernelServices {
        state: Arc::new(FileStateStore::open(directory).unwrap()),
        ..KernelServices::default()
    };
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
    let memory = MemoryPlugin::new().unwrap();
    let admin = memory.controller();
    kernel.register(Box::new(memory)).unwrap();
    kernel
        .register(Box::new(SessionPlugin::new().unwrap()))
        .unwrap();
    kernel.start_all().await.unwrap();
    let sessions = registry
        .get(&ServiceId::new(SESSION_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<SessionServiceHandle>()
        .unwrap()
        .0
        .clone();
    let scope = MemoryScope {
        channel: "qq".into(),
        session_id: "component-session".into(),
        user_id: "component-user".into(),
    };
    let key = SessionKey::new(&scope.session_id, &scope.user_id).unwrap();
    for index in 0..3 {
        let input = format!("第 {} 次请求：请先给结论。", index + 1);
        let turn = sessions
            .begin(SessionInput {
                key: key.clone(),
                text: input.clone(),
            })
            .unwrap();
        sessions
            .complete(
                &turn.lease,
                vec![
                    ChatMessage::text(ChatRole::User, input),
                    ChatMessage::text(ChatRole::Assistant, "结论：可以。"),
                ],
            )
            .unwrap();
        admin
            .import_completed(
                &scope,
                index,
                CompletedInteraction {
                    evidence_id: format!("component-evidence-{index}"),
                    message_id: format!("component-message-{index}"),
                    at_ms: index + 1,
                    snapshot: sessions.snapshot(&key).unwrap().unwrap(),
                    turn_id: turn.lease.turn_id,
                },
            )
            .unwrap();
    }
    let snapshot = admin.reader(scope).unwrap().snapshot().unwrap();
    kernel.stop_all().await.unwrap();
    snapshot
}

struct Extractor {
    case: String,
    calls: Arc<AtomicUsize>,
    versions: Arc<AtomicUsize>,
    directory: PathBuf,
    started: PathBuf,
    expected: MemorySnapshot,
}
impl PreferenceExtractor for Extractor {
    fn version(&self) -> &str {
        assert_ne!(self.case, "off", "关闭提炼时不得查询组件版本");
        self.versions.fetch_add(1, Ordering::SeqCst);
        "component-test-v1"
    }
    fn extract(&self, batch: LearningBatch) -> LearningFuture<'_, Vec<CandidateDraft>> {
        assert_ne!(self.case, "off", "关闭提炼时不得调用组件");
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(batch.scope, self.expected.scope);
        assert_eq!(batch.evidence, self.expected.evidence);
        // 替代实现也只能在 Running 已落盘后执行。
        let saved = stored(&self.directory, LEARNING_PLUGIN_ID, LEARNING_STATE_KEY).unwrap();
        let job: LearningJob = serde_json::from_value(saved["jobs"][0]["job"].clone()).unwrap();
        assert_eq!(job.batch, batch);
        assert_eq!(job.status, JobStatus::Running);
        Box::pin(async move {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !self.started.exists() {
                assert!(Instant::now() < deadline, "替身桥接未就绪");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok(vec![CandidateDraft {
                text: "用户可能偏好先看结论".into(),
                confidence: 73,
                evidence_ids: if self.case == "invalid" {
                    vec!["unobserved-evidence".into()]
                } else {
                    vec![batch.evidence[0].id.clone(), batch.evidence[2].id.clone()]
                },
            }])
        })
    }
}

#[tokio::test]
#[ignore = "仅由父测试在隔离环境调用，不连接实际 QQ 或模型"]
async fn learning_component_process_fixture() {
    let case = std::env::var("EVE_QQ_LEARNING_COMPONENT_CASE").expect("只由隔离父测试调用");
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("state");
    let agent = root.path().join("AGENT.md");
    let bridge = root.path().join("bridge.mjs");
    let started = root.path().join("started");
    let stopped = root.path().join("stopped");
    std::fs::write(&agent, "只生成待确认的偏好候选。").unwrap();
    std::fs::write(&bridge, BRIDGE).unwrap();
    let expected = seed_memory(&directory).await;
    let before = stored(&directory, MEMORY_PLUGIN_ID, MEMORY_STATE_KEY);
    let calls = Arc::new(AtomicUsize::new(0));
    let versions = Arc::new(AtomicUsize::new(0));
    let extractor = Arc::new(Extractor {
        case: case.clone(),
        calls: calls.clone(),
        versions: versions.clone(),
        directory: directory.clone(),
        started: started.clone(),
        expected: expected.clone(),
    });
    let result = run_qqbot_with_components(
        QqBotOptions {
            state_directory: directory.clone(),
            agent_path: agent,
            bridge_script: bridge,
            bridge_args: vec![
                started.as_os_str().into(),
                stopped.as_os_str().into(),
                directory.join("state.json").into_os_string(),
                case.clone().into(),
            ],
            memory: true,
            memory_learning: case != "off",
            ..QqBotOptions::default()
        },
        Arc::new(ReflectionPlannerFactory),
        Some(extractor),
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), usize::from(case != "off"));
    assert_eq!(versions.load(Ordering::SeqCst) > 0, case != "off");
    if case == "invalid" {
        assert!(result.is_err(), "非法替代组件输出必须关闭宿主");
        assert!(stopped.exists(), "后台错误后须实际停止 Node 桥接");
    } else {
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
    }
    let port: u16 = std::fs::read_to_string(&started).unwrap().parse().unwrap();
    let _probe =
        TcpListener::bind(("127.0.0.1", port)).expect("宿主关闭必须回收拥有的 Node 子进程");
    let saved = stored(&directory, LEARNING_PLUGIN_ID, LEARNING_STATE_KEY);
    if case == "off" {
        assert!(saved.is_none());
    } else {
        let saved = saved.unwrap();
        assert_eq!(saved["jobs"].as_array().unwrap().len(), 1);
        let job: LearningJob = serde_json::from_value(saved["jobs"][0]["job"].clone()).unwrap();
        assert_eq!(job.batch.scope, expected.scope);
        assert_eq!(job.batch.evidence, expected.evidence);
        assert_eq!(job.batch.extractor_version, "component-test-v1");
        if case == "invalid" {
            assert_eq!(job.status, JobStatus::Running);
            assert!(job.finished_at_ms.is_none() && job.candidates.is_empty());
        } else {
            assert_eq!(job.status, JobStatus::Completed);
            assert_eq!(job.candidates.len(), 1);
            assert_eq!(job.candidates[0].draft.text, "用户可能偏好先看结论");
            assert_eq!(job.candidates[0].draft.confidence, 73);
            assert_eq!(
                job.candidates[0].draft.evidence_ids,
                vec![
                    expected.evidence[0].id.clone(),
                    expected.evidence[2].id.clone()
                ]
            );
        }
    }
    assert_eq!(
        stored(&directory, MEMORY_PLUGIN_ID, MEMORY_STATE_KEY),
        before,
        "候选和失败均不得修改真实记忆或隐式确认偏好"
    );
    let _reopened = FileStateStore::open(&directory).expect("正常和失败出口均须释放状态锁");
}
