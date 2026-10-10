use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use eve_practice_api::*;
use eve_practice_plugin::{PracticeController, PracticePlugin, Practitioner};
use eve_toolforge_api::*;
use eve_toolforge_plugin::{Forge, ForgedChecks, ToolForgeController, ToolForgePlugin};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

/// 真实文件库；可注入提交前失败。
struct Store {
    inner: FileStateStore,
    fail_before: AtomicBool,
}
impl Store {
    fn open(path: &Path) -> Arc<Self> {
        Arc::new(Self {
            inner: FileStateStore::open(path).unwrap(),
            fail_before: AtomicBool::new(false),
        })
    }
    fn raw(&self) -> Option<Vec<u8>> {
        self.inner
            .get(
                &PluginId::new(TOOLFORGE_PLUGIN_ID).unwrap(),
                TOOLFORGE_STATE_KEY,
            )
            .unwrap()
    }
    fn saved(&self) -> Value {
        self.raw()
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
            .unwrap_or(Value::Null)
    }
    fn overwrite(&self, value: &[u8]) {
        self.inner
            .set(
                &PluginId::new(TOOLFORGE_PLUGIN_ID).unwrap(),
                TOOLFORGE_STATE_KEY.into(),
                value.to_vec(),
            )
            .unwrap();
    }
}
impl StateStore for Store {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.inner.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        if self.fail_before.load(Ordering::SeqCst) {
            return Err(PluginError::State("private-storage-detail".into()));
        }
        self.inner.set(namespace, key, value)
    }
}

fn profile() -> RunnerProfile {
    RunnerProfile {
        runner_id: "fake-runner:v1".into(),
        runtime: "fake runtime".into(),
        domain: "测试用数据文件".into(),
        layout: "items/<名称>.hjson".into(),
        properties: vec!["value".into()],
    }
}

fn task(goal: &str) -> PracticeTask {
    PracticeTask {
        goal_id: goal.into(),
        goal_revision: 1,
        owner: "user-a".into(),
        brief: format!("学习目标 {goal}"),
        brief_truncated: false,
        notes: vec![],
    }
}

fn item(name: &str, extra: &str) -> PracticeDraft {
    PracticeDraft {
        applicable: true,
        files: vec![ArtifactFile {
            path: format!("items/{name}.hjson"),
            content: format!("name: {name}\n{extra}value: 7\n"),
        }],
        probes: vec![Probe {
            subject: format!("item-{name}"),
            property: "value".into(),
            expected: "7".into(),
        }],
        rationale: "最小产物".into(),
        notes_used: vec![],
    }
}

fn legacy(name: &str) -> PracticeDraft {
    item(name, "legacy: true\n")
}

/// 替身运行环境：内容含 legacy 时给出两条警告（行号随内容变化），其余正常加载。
struct FakeRunner {
    profile: RunnerProfile,
    runs: AtomicU64,
}
impl PracticeRunner for FakeRunner {
    fn profile(&self) -> &RunnerProfile {
        &self.profile
    }
    fn check(&self, _draft: &PracticeDraft) -> Vec<String> {
        vec![]
    }
    fn run<'a>(
        &'a self,
        draft: &'a PracticeDraft,
        _workspace: &'a Path,
    ) -> eve_practice_api::BoxFuture<'a, RunEvidence> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let content = &draft.files[0].content;
        let warnings = match content.lines().position(|line| line.starts_with("legacy")) {
            Some(line) => vec![
                format!("field legacy is deprecated (line {})", line + 1),
                "legacy block skipped".into(),
            ],
            None => vec![],
        };
        let probes = draft
            .probes
            .iter()
            .map(|probe| ProbeResult {
                probe: probe.clone(),
                actual: Some("7".into()),
                passed: probe.expected == "7",
            })
            .collect();
        Box::pin(async move {
            RunEvidence {
                runtime_version: "fake build 1".into(),
                exit: RunExit::Completed,
                loaded: true,
                warnings,
                probes,
                log_excerpt: "loaded".into(),
                log_sha256: "a".repeat(64),
                log_bytes: 6,
                duration_ms: 5,
            }
        })
    }
}

struct FakeDrafter {
    replies: Mutex<VecDeque<PracticeDraft>>,
}
impl PracticeDrafter for FakeDrafter {
    fn version(&self) -> &str {
        "fake-drafter:v1"
    }
    fn draft(&self, _request: DraftRequest) -> PracticeFuture<'_, PracticeDraft> {
        let reply = self.replies.lock().unwrap().pop_front();
        Box::pin(async move { reply.ok_or(PracticeError::Practice(PracticeFailure::Provider)) })
    }
}

struct FakeForger {
    replies: Mutex<VecDeque<Result<ForgeOutput, ForgeFailure>>>,
    requests: Mutex<Vec<(ForgeRequest, Value)>>,
    store: Arc<Store>,
}
impl ToolForger for FakeForger {
    fn version(&self) -> &str {
        "fake-forger:v1"
    }
    fn forge(&self, request: ForgeRequest) -> ToolFuture<'_, ForgeOutput> {
        self.requests
            .lock()
            .unwrap()
            .push((request, self.store.saved()));
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected forge request");
        Box::pin(async move { reply.map_err(ToolError::Forge) })
    }
}

fn check(name: &str, text: &str) -> ForgeOutput {
    ForgeOutput::Check(CheckSpec {
        name: name.into(),
        summary: "检查已废弃的字段".into(),
        message: "去掉已废弃的 legacy 字段".into(),
        rules: vec![CheckRule::ForbidText {
            pattern: "*.hjson".into(),
            text: text.into(),
        }],
    })
}

struct World {
    _directory: tempfile::TempDir,
    work: std::path::PathBuf,
    store: Arc<Store>,
    kernel: Kernel,
    practice: PracticeController,
    tools: ToolForgeController,
    runner: Arc<FakeRunner>,
    drafter: Arc<FakeDrafter>,
    forger: Arc<FakeForger>,
    clock: Arc<AtomicU64>,
}
impl World {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("state"));
        let (kernel, practice, tools) = open(store.clone()).await.unwrap();
        Self {
            work: directory.path().join("work"),
            _directory: directory,
            kernel,
            practice,
            tools,
            runner: Arc::new(FakeRunner {
                profile: profile(),
                runs: AtomicU64::new(0),
            }),
            drafter: Arc::new(FakeDrafter {
                replies: Mutex::new(VecDeque::new()),
            }),
            forger: Arc::new(FakeForger {
                replies: Mutex::new(VecDeque::new()),
                requests: Mutex::new(vec![]),
                store: store.clone(),
            }),
            store,
            clock: Arc::new(AtomicU64::new(1_000)),
        }
    }
    async fn reopen(&mut self) {
        self.kernel.stop_all().await.unwrap();
        let (kernel, practice, tools) = open(self.store.clone()).await.unwrap();
        (self.kernel, self.practice, self.tools) = (kernel, practice, tools);
    }
    fn now(&self) -> u64 {
        self.clock.fetch_add(10, Ordering::SeqCst)
    }
    async fn practice(&self, goal: &str, drafts: Vec<PracticeDraft>, checked: bool) -> PracticeRun {
        self.drafter.replies.lock().unwrap().extend(drafts);
        let mut practitioner = Practitioner::new(
            Arc::new(self.practice.clone()),
            self.drafter.clone(),
            self.runner.clone(),
        );
        if checked {
            practitioner =
                practitioner.with_check(Arc::new(ForgedChecks::new(Arc::new(self.tools.clone()))));
        }
        let run = self
            .practice
            .begin(task(goal), &profile(), "fake-drafter:v1", self.now())
            .unwrap()
            .unwrap();
        let clock = self.clock.clone();
        practitioner
            .practice(&run, &self.work, move || {
                clock.fetch_add(10, Ordering::SeqCst)
            })
            .await
            .unwrap()
    }
    fn forge(&self) -> Forge {
        Forge::new(Arc::new(self.tools.clone()), self.forger.clone(), profile())
    }
    fn candidate(&self) -> Option<eve_toolforge_plugin::Candidate> {
        self.forge().candidate(
            &self.tools.snapshot().unwrap(),
            &self.practice.snapshot().unwrap(),
        )
    }
    /// 一轮锻造：取候选、准入或复用，再推进 Running 锻造到结局。
    async fn forge_once(&self) -> Option<ForgeAttempt> {
        let forge = self.forge();
        let candidate = self.candidate()?;
        let entry = forge.begin(&candidate, self.now()).unwrap()?;
        if entry.status != ForgeStatus::Running {
            return Some(entry);
        }
        let clock = self.clock.clone();
        Some(
            forge
                .complete(&entry, &candidate, move || {
                    clock.fetch_add(10, Ordering::SeqCst)
                })
                .await
                .unwrap(),
        )
    }
    fn reply(&self, reply: Result<ForgeOutput, ForgeFailure>) {
        self.forger.replies.lock().unwrap().push_back(reply);
    }
    fn snapshot(&self) -> ToolSnapshot {
        self.tools.snapshot().unwrap()
    }
    /// 两项独立实践各自先用 legacy 字段被警告、修正后验证通过：两条反复出现的缺口。
    async fn two_warned_practices(&self) {
        for (goal, name) in [("goal-1", "lamp"), ("goal-2", "bulb")] {
            let run = self
                .practice(goal, vec![legacy(name), item(name, "")], false)
                .await;
            assert_eq!(run.status, PracticeStatus::Verified);
        }
    }
}

async fn open(
    state: Arc<Store>,
) -> PluginResult<(Kernel, PracticeController, ToolForgeController)> {
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let practice = PracticePlugin::new()?;
    let tools = ToolForgePlugin::new()?;
    let (practice_admin, tool_admin) = (practice.controller(), tools.controller());
    kernel.register(Box::new(practice))?;
    kernel.register(Box::new(tools))?;
    kernel.start_all().await?;
    Ok((kernel, practice_admin, tool_admin))
}

#[tokio::test]
async fn repeated_warning_becomes_an_enabled_check_that_stops_the_next_draft_before_running() {
    let world = World::new().await;
    world.two_warned_practices().await;
    assert_eq!(world.runner.runs.load(Ordering::SeqCst), 4);

    // 行号不同的同一警告归为一条缺口；两侧各有两份真实草稿。
    let candidate = world.candidate().unwrap();
    assert_eq!(candidate.occurrences, 2);
    assert_eq!((candidate.failing.len(), candidate.passing.len()), (2, 2));
    assert!(candidate.reuse.is_none() && candidate.current.is_none());

    world.reply(Ok(check("no-legacy", "legacy:")));
    let forged = world.forge_once().await.unwrap();
    assert_eq!(forged.status, ForgeStatus::Verified);
    // 请求发出前已保存 Running；请求只含草稿文件与问题原文，不含任务描述。
    let (request, saved) = world.forger.requests.lock().unwrap()[0].clone();
    assert_eq!(saved["forges"][0]["status"], "Running");
    assert!(
        !serde_json::to_string(&request)
            .unwrap()
            .contains("学习目标")
    );
    let reference = forged.tool.clone().unwrap();
    let tool = world.snapshot().tool(&reference.tool_id).unwrap().clone();
    assert_eq!((tool.enabled, reference.version), (Some(1), 1));
    assert_eq!(tool.changes[0].actor, Actor::Automatic);
    let verification = &tool.versions[0].verification;
    assert!(verification.passed() && verification.examples.len() == 4);

    // 另一条缺口由已启用的工具回放即覆盖：只记录复用，不请求锻造器。
    let reused = world.forge_once().await.unwrap();
    assert_eq!(reused.status, ForgeStatus::Reused);
    assert_eq!(reused.tool, Some(reference.clone()));
    assert_eq!(world.forger.requests.lock().unwrap().len(), 1);
    assert!(world.forge_once().await.is_none(), "同一出现次数不再处理");

    // 后续独立任务：第一份草稿在运行前被工具拦下，没有实际运行；修正后实际运行验证通过。
    let run = world
        .practice("goal-3", vec![legacy("cup"), item("cup", "")], true)
        .await;
    assert_eq!(run.status, PracticeStatus::Verified);
    assert_eq!(world.runner.runs.load(Ordering::SeqCst), 5);
    let first = &run.attempts[0];
    assert_eq!(first.outcome, Some(AttemptOutcome::Rejected));
    assert!(first.evidence.is_none());
    assert!(first.issues[0].starts_with(DRAFT_CHECK_MARK));
    assert!(first.issues[0].contains("工具 no-legacy 第 1 版"));
    let calls: Vec<_> = world
        .snapshot()
        .calls_for(&reference.tool_id)
        .cloned()
        .collect();
    assert_eq!(calls.len(), 2);
    assert_eq!((calls[0].attempt, calls[1].attempt), (1, 2));
    assert!(!calls[0].findings.is_empty() && calls[1].findings.is_empty());

    // 被拦下的尝试不算新的缺口，也不算样例；没有新的锻造。
    let gaps = capability_gaps(&world.practice.snapshot().unwrap(), "user-a");
    assert!(gaps.iter().all(|gap| gap.occurrences == 2));
    assert!(world.candidate().is_none());
}

#[tokio::test]
async fn false_positive_is_rejected_and_a_missed_variant_yields_a_new_version_with_rollback() {
    let world = World::new().await;
    world.two_warned_practices().await;
    // 拦下所有草稿的规则误报了验证通过的草稿：不成为工具。
    world.reply(Ok(check("too-broad", "name:")));
    let rejected = world.forge_once().await.unwrap();
    assert_eq!(rejected.status, ForgeStatus::Rejected);
    assert!(world.snapshot().tools.is_empty());
    let verification = rejected.verification.unwrap();
    assert!(!verification.passed());
    assert!(
        verification
            .examples
            .iter()
            .any(|example| !example.expect_flag && !example.findings.is_empty())
    );
    // 另一条缺口的锻造器说不可用只读规则识别；记录理由。
    world.reply(Ok(ForgeOutput::NotForgeable {
        reason: "需要实际运行才能发现".into(),
    }));
    assert_eq!(
        world.forge_once().await.unwrap().status,
        ForgeStatus::NotForgeable
    );
    assert!(world.forge_once().await.is_none());

    // 第三次出现后再次锻造：只禁止 "legacy: true" 的第 1 版。
    world
        .practice("goal-3", vec![legacy("cup"), item("cup", "")], false)
        .await;
    world.reply(Ok(check("no-legacy", "legacy: true")));
    let first = world.forge_once().await.unwrap();
    assert_eq!(first.status, ForgeStatus::Verified);
    let tool_id = first.tool.unwrap().tool_id;
    // 另一条缺口回放已被第 1 版覆盖：复用。
    assert_eq!(
        world.forge_once().await.unwrap().status,
        ForgeStatus::Reused
    );

    // 换一种写法的 legacy 没被第 1 版拦下，实际运行再次警告；改进时以第 1 版为基础。
    let run = world
        .practice(
            "goal-4",
            vec![item("pen", "legacy: yes\n"), item("pen", "")],
            true,
        )
        .await;
    assert_eq!(run.attempts[0].outcome, Some(AttemptOutcome::Failed));
    let candidate = world.candidate().unwrap();
    assert_eq!(candidate.occurrences, 4);
    assert!(candidate.reuse.is_none(), "第 1 版漏掉了新的写法");
    assert_eq!(candidate.current.as_ref().unwrap().name, "no-legacy");
    world.reply(Ok(check("no-legacy", "legacy:")));
    let second = world.forge_once().await.unwrap();
    assert_eq!(second.tool.as_ref().unwrap().version, 2);
    let tool = world.snapshot().tool(&tool_id).unwrap().clone();
    assert_eq!(tool.enabled, Some(2));

    // 回退到第 1 版、停用后新验证的版本不自动启用；启用不存在的版本被拒绝。
    let at = world.now();
    let rolled = world
        .tools
        .set_enabled(&tool_id, at, Actor::Owner, Some(1))
        .unwrap();
    assert_eq!(rolled.enabled, Some(1));
    assert_eq!(
        world
            .tools
            .set_enabled(&tool_id, world.now(), Actor::Owner, Some(9)),
        Err(ToolError::InvalidInput)
    );
    let disabled = world
        .tools
        .set_enabled(&tool_id, world.now(), Actor::Owner, None)
        .unwrap();
    assert!(disabled.disabled_by_owner());
    // 停用的工具不参与运行前检查，也不再为它锻造。
    let run = world
        .practice("goal-5", vec![legacy("box"), item("box", "")], true)
        .await;
    assert_eq!(run.attempts[0].outcome, Some(AttemptOutcome::Failed));
    let candidate = world.candidate();
    assert!(candidate.is_none_or(|candidate| candidate.gap.key != tool.gap_key));
    // 调用已停用的版本被拒绝。
    let call = ToolCall {
        id: call_id(&tool_id, "practice-x", 1),
        tool: ToolRef {
            tool_id: tool_id.clone(),
            version: 2,
        },
        owner: "user-a".into(),
        run_id: "practice-x".into(),
        attempt: 1,
        at_ms: world.now(),
        findings: vec![],
    };
    assert_eq!(world.tools.record_call(call), Err(ToolError::Conflict));
}

#[tokio::test]
async fn interrupted_failed_and_abandoned_forges_are_recorded_without_replay() {
    let mut world = World::new().await;
    world.two_warned_practices().await;
    let forge = world.forge();
    let candidate = world.candidate().unwrap();
    let entry = forge.begin(&candidate, world.now()).unwrap().unwrap();
    assert_eq!(entry.status, ForgeStatus::Running);
    // 同一时间只有一次锻造。
    assert!(
        world
            .tools
            .begin_forge("user-a", candidate.gap.clone(), 9, "v", world.now())
            .unwrap()
            .is_none()
    );
    // 进程在请求锻造器前退出：重启记为中断，不重放。
    world.reopen().await;
    let snapshot = world.snapshot();
    assert_eq!(snapshot.forges[0].status, ForgeStatus::Interrupted);
    assert_eq!(world.store.saved()["forges"][0]["status"], "Interrupted");
    assert!(snapshot.forges[0].finished_at_ms.is_none());

    // 另一条缺口：锻造器失败与输出不合规都写入失败结局。
    world.reply(Err(ForgeFailure::Provider));
    assert_eq!(
        world.forge_once().await.unwrap().status,
        ForgeStatus::Failed(ForgeFailure::Provider)
    );
    assert!(world.forge_once().await.is_none());
    world
        .practice("goal-3", vec![legacy("cup"), item("cup", "")], false)
        .await;
    world.reply(Ok(check("Bad-Name", "legacy:")));
    assert_eq!(
        world.forge_once().await.unwrap().status,
        ForgeStatus::Failed(ForgeFailure::InvalidOutput)
    );
    // 停止或超时：宿主放弃进行中的锻造，只能结束一次。
    let candidate = world.candidate().unwrap();
    let entry = world
        .forge()
        .begin(&candidate, world.now())
        .unwrap()
        .unwrap();
    let at = world.now();
    let abandoned = world
        .tools
        .abandon_forge(&entry.id, at, ForgeFailure::Cancelled)
        .unwrap();
    assert_eq!(
        abandoned.status,
        ForgeStatus::Failed(ForgeFailure::Cancelled)
    );
    assert_eq!(
        world
            .tools
            .abandon_forge(&entry.id, at, ForgeFailure::Cancelled),
        Err(ToolError::Conflict)
    );
    // 每个缺口至多 MAX_FORGES_PER_GAP 次。
    world
        .practice("goal-4", vec![legacy("pen"), item("pen", "")], false)
        .await;
    let keys: Vec<_> = world
        .snapshot()
        .forges
        .iter()
        .map(|forge| forge.gap.key.clone())
        .collect();
    let saturated = keys
        .iter()
        .find(|key| keys.iter().filter(|other| other == key).count() >= MAX_FORGES_PER_GAP);
    if let Some(key) = saturated {
        assert!(
            world
                .candidate()
                .is_none_or(|candidate| &candidate.gap.key != key)
        );
    }
}

#[tokio::test]
async fn storage_failure_closes_the_ledger_and_corrupt_state_is_refused_and_preserved() {
    let mut world = World::new().await;
    world.two_warned_practices().await;
    world.reply(Ok(check("no-legacy", "legacy:")));
    world.forge_once().await.unwrap();
    world
        .practice("goal-3", vec![legacy("cup"), item("cup", "")], true)
        .await;

    // 提交失败后整个实例关闭；不在旧缓存上继续。
    world.store.fail_before.store(true, Ordering::SeqCst);
    let tool_id = world.snapshot().tools[0].id.clone();
    assert_eq!(
        world
            .tools
            .set_enabled(&tool_id, world.now(), Actor::Owner, None),
        Err(ToolError::Storage)
    );
    assert_eq!(world.tools.snapshot(), Err(ToolError::Unavailable));
    world.store.fail_before.store(false, Ordering::SeqCst);
    world.reopen().await;
    assert_eq!(world.snapshot().tools[0].enabled, Some(1));
    world.kernel.stop_all().await.unwrap();
    let valid = world.store.saved();

    let mut cases: Vec<Vec<u8>> = vec![
        b"{\"format_version\":1,\"format_version\":1,\"tools\":[],\"forges\":[],\"calls\":[]}"
            .to_vec(),
        serde_json::to_vec(&json!({"format_version": 2, "tools": [], "forges": [], "calls": []}))
            .unwrap(),
        b"not json".to_vec(),
    ];
    let mutate = |change: &dyn Fn(&mut Value)| {
        let mut value = valid.clone();
        change(&mut value);
        serde_json::to_vec(&value).unwrap()
    };
    // 启用不存在的版本、已验证的锻造没有工具、版本的验证未通过、调用 ID 被改写、规格被改写、多一个字段。
    cases.push(mutate(&|value| value["tools"][0]["enabled"] = json!(7)));
    cases.push(mutate(&|value| value["forges"][0]["tool"] = Value::Null));
    cases.push(mutate(&|value| {
        value["tools"][0]["versions"][0]["verification"]["examples"][0]["findings"] = json!([])
    }));
    cases.push(mutate(&|value| value["calls"][0]["id"] = json!("call-0")));
    cases.push(mutate(&|value| {
        value["tools"][0]["versions"][0]["spec"]["rules"][0]["forbid_text"]["text"] = json!("x")
    }));
    cases.push(mutate(&|value| value["extra"] = json!(1)));
    for case in cases {
        world.store.overwrite(&case);
        let opened = open(world.store.clone()).await;
        assert!(opened.is_err(), "{}", String::from_utf8_lossy(&case));
        // 拒绝打开时不清空或改写原数据。
        assert_eq!(world.store.raw().unwrap(), case);
    }
}
