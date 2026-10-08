use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use eve_practice_api::*;
use eve_practice_plugin::{PracticeController, PracticePlugin, Practitioner};
use eve_skill_api::*;
use eve_skill_plugin::{Consolidator, SkillAwareDrafter, SkillController, SkillPlugin, settle};
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
            .get(&PluginId::new(SKILL_PLUGIN_ID).unwrap(), SKILL_STATE_KEY)
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
                &PluginId::new(SKILL_PLUGIN_ID).unwrap(),
                SKILL_STATE_KEY.into(),
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

struct Host {
    kernel: Kernel,
    practice: PracticeController,
    skills: SkillController,
}

async fn open(state: Arc<Store>) -> PluginResult<Host> {
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let practice = PracticePlugin::new()?;
    let skills = SkillPlugin::new()?;
    let (practice_admin, skill_admin) = (practice.controller(), skills.controller());
    kernel.register(Box::new(practice))?;
    kernel.register(Box::new(skills))?;
    kernel.start_all().await?;
    Ok(Host {
        kernel,
        practice: practice_admin,
        skills: skill_admin,
    })
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

fn item(name: &str, value: &str) -> PracticeDraft {
    PracticeDraft {
        applicable: true,
        files: vec![ArtifactFile {
            path: format!("items/{name}.hjson"),
            content: format!("name: {name}\nvalue: {value}\n"),
        }],
        probes: vec![Probe {
            subject: format!("item-{name}"),
            property: "value".into(),
            expected: value.into(),
        }],
        rationale: "最小产物".into(),
        notes_used: vec![],
    }
}

fn template(max: i64) -> SkillTemplate {
    SkillTemplate {
        title: "带数值的物品".into(),
        summary: "生成一个指定名称与数值的物品".into(),
        parameters: vec![
            SkillParameter {
                name: "name".into(),
                description: "物品名称".into(),
                kind: ParameterKind::Identifier,
            },
            SkillParameter {
                name: "value".into(),
                description: "数值".into(),
                kind: ParameterKind::Integer { min: 1, max },
            },
        ],
        files: vec![ArtifactFile {
            path: "items/{{name}}.hjson".into(),
            content: "name: {{name}}\nvalue: {{value}}\n".into(),
        }],
        probes: vec![Probe {
            subject: "item-{{name}}".into(),
            property: "value".into(),
            expected: "{{value}}".into(),
        }],
    }
}

fn args(name: &str, value: &str) -> Arguments {
    [("name", name), ("value", value)]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn proposal(extends: Option<String>, max: i64, original: &str) -> Proposal {
    Proposal::Skill(SkillProposal {
        extends,
        name: "valued-item".into(),
        template: template(max),
        arguments: args("lamp", original),
    })
}

/// 替身运行环境：读取文件中的 name 与 value；数值超过 1000 时给出警告，内容含 bad 时结构检查不通过。
struct FakeRunner {
    profile: RunnerProfile,
    runs: Mutex<Vec<(PracticeDraft, Value)>>,
    store: Arc<Store>,
}
impl PracticeRunner for FakeRunner {
    fn profile(&self) -> &RunnerProfile {
        &self.profile
    }
    fn check(&self, draft: &PracticeDraft) -> Vec<String> {
        if draft.files[0].content.contains("bad") {
            vec!["内容不合规".into()]
        } else {
            vec![]
        }
    }
    fn run<'a>(
        &'a self,
        draft: &'a PracticeDraft,
        workspace: &'a Path,
    ) -> eve_practice_api::BoxFuture<'a, RunEvidence> {
        assert!(std::fs::read_dir(workspace).unwrap().next().is_none());
        self.runs
            .lock()
            .unwrap()
            .push((draft.clone(), self.store.saved()));
        let content = &draft.files[0].content;
        let field = |key: &str| {
            content
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{key}: ")))
                .unwrap_or_default()
                .to_string()
        };
        let (name, value) = (field("name"), field("value"));
        let probes = draft
            .probes
            .iter()
            .map(|probe| {
                let actual = (probe.subject == format!("item-{name}")).then(|| value.clone());
                ProbeResult {
                    passed: actual.as_deref() == Some(probe.expected.as_str()),
                    probe: probe.clone(),
                    actual,
                }
            })
            .collect();
        let warnings = if value.parse::<i64>().unwrap_or(0) > 1000 {
            vec!["value too large, clamped".into()]
        } else {
            vec![]
        };
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
    calls: AtomicU64,
}
impl PracticeDrafter for FakeDrafter {
    fn version(&self) -> &str {
        "fake-drafter:v1"
    }
    fn draft(&self, request: DraftRequest) -> PracticeFuture<'_, PracticeDraft> {
        assert_eq!(request.drafter_version, "fake-drafter:v1");
        self.calls.fetch_add(1, Ordering::SeqCst);
        let reply = self.replies.lock().unwrap().pop_front();
        Box::pin(async move { reply.ok_or(PracticeError::Practice(PracticeFailure::Provider)) })
    }
}

struct FakeDistiller {
    replies: Mutex<VecDeque<Result<Proposal, SkillFailure>>>,
    requests: Mutex<Vec<(DistillRequest, Value)>>,
    store: Arc<Store>,
}
impl SkillDistiller for FakeDistiller {
    fn version(&self) -> &str {
        "fake-distiller:v1"
    }
    fn distill(&self, request: DistillRequest) -> SkillFuture<'_, Proposal> {
        self.requests
            .lock()
            .unwrap()
            .push((request, self.store.saved()));
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected distill request");
        Box::pin(async move { reply.map_err(SkillError::Skill) })
    }
}

struct FakeSelector {
    replies: Mutex<VecDeque<Result<Option<Choice>, SkillFailure>>>,
    requests: Mutex<Vec<(SelectRequest, Value)>>,
    store: Arc<Store>,
}
impl SkillSelector for FakeSelector {
    fn version(&self) -> &str {
        "fake-selector:v1"
    }
    fn select(&self, request: SelectRequest) -> SkillFuture<'_, Option<Choice>> {
        self.requests
            .lock()
            .unwrap()
            .push((request, self.store.saved()));
        // 没有排队的应答时表示没有适用的技能。
        let reply = self.replies.lock().unwrap().pop_front().unwrap_or(Ok(None));
        Box::pin(async move { reply.map_err(SkillError::Skill) })
    }
}

struct World {
    _directory: tempfile::TempDir,
    work: std::path::PathBuf,
    store: Arc<Store>,
    host: Host,
    runner: Arc<FakeRunner>,
    drafter: Arc<FakeDrafter>,
    distiller: Arc<FakeDistiller>,
    selector: Arc<FakeSelector>,
    clock: Arc<AtomicU64>,
}
impl World {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("state"));
        let host = open(store.clone()).await.unwrap();
        Self {
            work: directory.path().join("work"),
            _directory: directory,
            runner: Arc::new(FakeRunner {
                profile: profile(),
                runs: Mutex::new(vec![]),
                store: store.clone(),
            }),
            drafter: Arc::new(FakeDrafter {
                replies: Mutex::new(VecDeque::new()),
                calls: AtomicU64::new(0),
            }),
            distiller: Arc::new(FakeDistiller {
                replies: Mutex::new(VecDeque::new()),
                requests: Mutex::new(vec![]),
                store: store.clone(),
            }),
            selector: Arc::new(FakeSelector {
                replies: Mutex::new(VecDeque::new()),
                requests: Mutex::new(vec![]),
                store: store.clone(),
            }),
            clock: Arc::new(AtomicU64::new(1_000)),
            store,
            host,
        }
    }
    fn now(&self) -> u64 {
        self.clock.fetch_add(10, Ordering::SeqCst)
    }
    fn skill_drafter(&self) -> Arc<SkillAwareDrafter> {
        let clock = self.clock.clone();
        Arc::new(SkillAwareDrafter::new(
            Arc::new(self.host.skills.clone()),
            self.selector.clone(),
            self.drafter.clone(),
            Arc::new(move || clock.fetch_add(10, Ordering::SeqCst)),
        ))
    }
    fn consolidator(&self) -> Consolidator {
        Consolidator::new(
            Arc::new(self.host.skills.clone()),
            self.distiller.clone(),
            self.runner.clone(),
        )
    }
    /// 用带技能选择的草稿器为一个目标做一次实践。
    async fn practice(&self, goal: &str, drafts: Vec<PracticeDraft>) -> PracticeRun {
        self.drafter.replies.lock().unwrap().extend(drafts);
        let drafter = self.skill_drafter();
        let practitioner = Practitioner::new(
            Arc::new(self.host.practice.clone()),
            drafter.clone(),
            self.runner.clone(),
        );
        let run = self
            .host
            .practice
            .begin(task(goal), &profile(), drafter.version(), self.now())
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
    /// 提炼最早一次尚未提炼的已验证实践。
    async fn consolidate(&self, reply: Result<Proposal, SkillFailure>) -> Distillation {
        self.distiller.replies.lock().unwrap().push_back(reply);
        let consolidator = self.consolidator();
        let candidate = consolidator
            .candidate(
                &self.host.skills.snapshot().unwrap(),
                &self.host.practice.snapshot().unwrap(),
            )
            .expect("no candidate");
        let entry = self
            .host
            .skills
            .begin_distillation(
                &candidate.owner,
                candidate.origin,
                candidate.source,
                consolidator.runner(),
                consolidator.distiller_version(),
                self.now(),
            )
            .unwrap()
            .unwrap();
        let clock = self.clock.clone();
        consolidator
            .consolidate(&entry, &candidate.evidence, &self.work, move || {
                clock.fetch_add(10, Ordering::SeqCst)
            })
            .await
            .unwrap()
    }
    fn settle(&self) -> usize {
        settle(
            &self.host.skills,
            &self.host.practice.snapshot().unwrap(),
            self.now(),
        )
        .unwrap()
    }
}

#[tokio::test]
async fn verified_practice_becomes_enabled_skill_and_a_later_task_invokes_it() {
    let world = World::new().await;
    // 没有技能时不请求选择器，照常草稿。
    let first = world.practice("goal-1", vec![item("lamp", "7")]).await;
    assert_eq!(first.status, PracticeStatus::Verified);
    assert!(world.selector.requests.lock().unwrap().is_empty());

    let distilled = world.consolidate(Ok(proposal(None, 900, "7"))).await;
    assert_eq!(distilled.status, DistillStatus::Verified);
    // 提炼请求只含已验证产物与证据，发出前已保存 Running/Proposing。
    let (request, saved) = world.distiller.requests.lock().unwrap()[0].clone();
    assert!(same_artifact(&request.source, &item("lamp", "7")));
    assert!(
        !serde_json::to_string(&request)
            .unwrap()
            .contains("学习目标")
    );
    assert_eq!(saved["distillations"][0]["status"], "Running");
    assert_eq!(saved["distillations"][0]["stage"], "Proposing");
    // 验证实例由宿主选取参数：标识加后缀、整数取边界；运行前已保存 Verifying 与验证参数。
    let runs = world.runner.runs.lock().unwrap().clone();
    let (holdout, saved) = runs.last().unwrap();
    assert!(same_artifact(holdout, &item("lamp-b", "900")));
    assert_eq!(saved["distillations"][0]["stage"], "Verifying");
    assert_eq!(
        saved["distillations"][0]["holdout"],
        json!({"name": "lamp-b", "value": "900"})
    );

    let snapshot = world.host.skills.snapshot().unwrap();
    let [skill] = snapshot.skills.as_slice() else {
        panic!("expected one skill")
    };
    assert_eq!(skill.enabled, Some(1));
    assert_eq!(skill.owner, "user-a");
    assert_eq!(skill.changes[0].actor, Actor::Automatic);
    assert_eq!(
        distilled.skill,
        Some(SkillRef {
            skill_id: skill.id.clone(),
            version: 1
        })
    );
    // 同一次实践不再提炼。
    assert!(
        world
            .consolidator()
            .candidate(&snapshot, &world.host.practice.snapshot().unwrap())
            .is_none()
    );

    // 后续独立任务：选择器选定技能，宿主实例化后作为第一次草稿，不请求草稿器。
    world
        .selector
        .replies
        .lock()
        .unwrap()
        .push_back(Ok(Some(Choice {
            skill: SkillRef {
                skill_id: skill.id.clone(),
                version: 1,
            },
            arguments: args("torch", "12"),
            reason: "任务需要一个物品".into(),
        })));
    let calls = world.drafter.calls.load(Ordering::SeqCst);
    let second = world.practice("goal-2", vec![]).await;
    assert_eq!(second.status, PracticeStatus::Verified);
    assert_eq!(world.drafter.calls.load(Ordering::SeqCst), calls);
    let draft = second.attempts[0].draft.as_ref().unwrap();
    assert!(same_artifact(draft, &item("torch", "12")));
    assert!(draft.rationale.contains(&skill.id) && draft.rationale.contains("第 1 版"));
    let (request, saved) = world.selector.requests.lock().unwrap()[0].clone();
    assert_eq!(request.candidates.len(), 1);
    assert_eq!(saved["selections"][0]["status"], "Running");

    assert_eq!(world.settle(), 1);
    let snapshot = world.host.skills.snapshot().unwrap();
    let selection = snapshot.selection(&second.id).unwrap();
    assert_eq!(selection.status, SelectionStatus::Chosen);
    assert_eq!(selection.outcome, Some(InvocationOutcome::Verified));
    assert_eq!(snapshot.invocations(&skill.id).count(), 1);
    // 技能实例验证通过的实践不再提炼成新技能；结算不会重复写入。
    assert!(
        world
            .consolidator()
            .candidate(&snapshot, &world.host.practice.snapshot().unwrap())
            .is_none()
    );
    assert_eq!(world.settle(), 0);
    world.host.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn unfaithful_templates_and_overstated_ranges_do_not_become_skills() {
    let world = World::new().await;
    world.practice("goal-1", vec![item("lamp", "7")]).await;
    // 原参数不能逐字还原已验证草稿：记为 Rejected，不运行。
    let runs_before = world.runner.runs.lock().unwrap().len();
    let rejected = world.consolidate(Ok(proposal(None, 900, "8"))).await;
    assert_eq!(rejected.status, DistillStatus::Rejected);
    assert!(rejected.issues[0].contains("逐字还原"));
    assert_eq!(world.runner.runs.lock().unwrap().len(), runs_before);
    assert!(world.host.skills.snapshot().unwrap().skills.is_empty());

    // 声称的范围过宽：宿主用边界值实际运行，出现警告即未验证，保留证据，不形成技能。
    world.practice("goal-2", vec![item("lamp", "7")]).await;
    let unverified = world.consolidate(Ok(proposal(None, 5000, "7"))).await;
    assert_eq!(unverified.status, DistillStatus::Unverified);
    assert_eq!(
        unverified.evidence.as_ref().unwrap().warnings,
        vec!["value too large, clamped".to_string()]
    );
    assert!(world.host.skills.snapshot().unwrap().skills.is_empty());

    // 提炼器认为不可复用、或请求失败，都如实记录且不重试。
    world.practice("goal-3", vec![item("lamp", "7")]).await;
    let declined = world
        .consolidate(Ok(Proposal::NotReusable {
            reason: "已被覆盖".into(),
        }))
        .await;
    assert_eq!(declined.status, DistillStatus::NotReusable);
    world.practice("goal-4", vec![item("lamp", "7")]).await;
    let failed = world.consolidate(Err(SkillFailure::InvalidOutput)).await;
    assert_eq!(
        failed.status,
        DistillStatus::Failed(SkillFailure::InvalidOutput)
    );
    world.host.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn owner_disable_holds_new_versions_and_rollback_restores_a_verified_version() {
    let world = World::new().await;
    world.practice("goal-1", vec![item("lamp", "7")]).await;
    let first = world.consolidate(Ok(proposal(None, 900, "7"))).await;
    let skill_id = first.skill.unwrap().skill_id;
    // 同名新技能会被要求作为新版本，不另造一个。
    world.practice("goal-2", vec![item("lamp", "9")]).await;
    let duplicate = world.consolidate(Ok(proposal(None, 900, "9"))).await;
    assert_eq!(duplicate.status, DistillStatus::Rejected);
    assert!(duplicate.issues[0].contains("新版本"));
    world.practice("goal-3", vec![item("lamp", "9")]).await;
    let second = world
        .consolidate(Ok(proposal(Some(skill_id.clone()), 800, "9")))
        .await;
    assert_eq!(second.skill.as_ref().unwrap().version, 2);
    let skill = world
        .host
        .skills
        .snapshot()
        .unwrap()
        .skill(&skill_id)
        .cloned()
        .unwrap();
    assert_eq!(skill.enabled, Some(2));

    // 用户停用后，新验证的版本保持停用，也不再提供给选择器。
    world
        .host
        .skills
        .set_enabled(&skill_id, world.now(), Actor::Owner, None)
        .unwrap();
    world.practice("goal-4", vec![item("lamp", "5")]).await;
    let third = world
        .consolidate(Ok(proposal(Some(skill_id.clone()), 700, "5")))
        .await;
    assert_eq!(third.status, DistillStatus::Verified);
    let skill = world
        .host
        .skills
        .snapshot()
        .unwrap()
        .skill(&skill_id)
        .cloned()
        .unwrap();
    assert_eq!((skill.enabled, skill.versions.len()), (None, 3));
    let calls = world.drafter.calls.load(Ordering::SeqCst);
    let selections = world.selector.requests.lock().unwrap().len();
    world.practice("goal-5", vec![item("rope", "3")]).await;
    assert_eq!(world.selector.requests.lock().unwrap().len(), selections);
    assert_eq!(world.drafter.calls.load(Ordering::SeqCst), calls + 1);

    // 回退到第 1 版：只改变启用版本并留下记录，不删除任何版本。
    let restored = world
        .host
        .skills
        .set_enabled(&skill_id, world.now(), Actor::Owner, Some(1))
        .unwrap();
    assert_eq!(restored.enabled, Some(1));
    assert_eq!(restored.versions.len(), 3);
    assert_eq!(
        restored
            .changes
            .iter()
            .map(|change| (change.actor, change.enabled))
            .collect::<Vec<_>>(),
        vec![
            (Actor::Automatic, Some(1)),
            (Actor::Automatic, Some(2)),
            (Actor::Owner, None),
            (Actor::Owner, Some(1)),
        ]
    );
    assert_eq!(
        world
            .host
            .skills
            .set_enabled(&skill_id, world.now(), Actor::Owner, Some(9))
            .err(),
        Some(SkillError::InvalidInput)
    );
    world.host.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn invalid_or_failed_selection_falls_back_to_drafting_and_is_recorded() {
    let world = World::new().await;
    world.practice("goal-1", vec![item("lamp", "7")]).await;
    let skill_id = world
        .consolidate(Ok(proposal(None, 900, "7")))
        .await
        .skill
        .unwrap()
        .skill_id;
    let choice = |name: &str, value: &str| Choice {
        skill: SkillRef {
            skill_id: skill_id.clone(),
            version: 1,
        },
        arguments: args(name, value),
        reason: "适用".into(),
    };
    world.selector.replies.lock().unwrap().extend([
        Ok(Some(choice("torch", "9000"))),
        Err(SkillFailure::Timeout),
        Ok(None),
    ]);
    for goal in ["goal-2", "goal-3", "goal-4"] {
        let run = world.practice(goal, vec![item("rope", "3")]).await;
        assert_eq!(run.status, PracticeStatus::Verified);
        assert!(same_artifact(
            run.attempts[0].draft.as_ref().unwrap(),
            &item("rope", "3")
        ));
    }
    let snapshot = world.host.skills.snapshot().unwrap();
    let statuses: Vec<_> = snapshot
        .selections
        .iter()
        .map(|selection| selection.status)
        .collect();
    assert_eq!(
        statuses,
        vec![
            SelectionStatus::Rejected,
            SelectionStatus::Failed(SkillFailure::Timeout),
            SelectionStatus::Declined
        ]
    );
    assert!(snapshot.selections[0].issues[0].contains("value"));
    assert_eq!(world.settle(), 0);
    world.host.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn restart_interrupts_running_work_without_replay_and_settles_from_practice() {
    let world = World::new().await;
    world.practice("goal-1", vec![item("lamp", "7")]).await;
    let skill_id = world
        .consolidate(Ok(proposal(None, 900, "7")))
        .await
        .skill
        .unwrap()
        .skill_id;

    // 提炼准入后进程退出：重启记为中断，不重放，也不再为这次实践提炼。
    world.practice("goal-2", vec![item("rope", "3")]).await;
    let consolidator = world.consolidator();
    let candidate = consolidator
        .candidate(
            &world.host.skills.snapshot().unwrap(),
            &world.host.practice.snapshot().unwrap(),
        )
        .unwrap();
    world
        .host
        .skills
        .begin_distillation(
            &candidate.owner,
            candidate.origin,
            candidate.source,
            consolidator.runner(),
            consolidator.distiller_version(),
            world.now(),
        )
        .unwrap()
        .unwrap();

    // 选定技能后、实践记录草稿前进程退出：两边都记为中断，调用结果结算为中断。
    let run = world
        .host
        .practice
        .begin(
            task("goal-3"),
            &profile(),
            world.skill_drafter().version(),
            world.now(),
        )
        .unwrap()
        .unwrap();
    world
        .host
        .skills
        .begin_selection(
            &run.id,
            "user-a",
            "goal-3",
            vec![SkillRef {
                skill_id: skill_id.clone(),
                version: 1,
            }],
            "fake-selector:v1",
            world.now(),
        )
        .unwrap()
        .unwrap();
    world
        .host
        .skills
        .record_selection(
            &run.id,
            world.now(),
            Ok(Some(Choice {
                skill: SkillRef {
                    skill_id,
                    version: 1,
                },
                arguments: args("torch", "12"),
                reason: "适用".into(),
            })),
            vec![],
        )
        .unwrap();
    world.host.kernel.stop_all().await.unwrap();

    let host = open(world.store.clone()).await.unwrap();
    let snapshot = host.skills.snapshot().unwrap();
    assert_eq!(snapshot.distillations[1].status, DistillStatus::Interrupted);
    assert_eq!(snapshot.distillations[1].finished_at_ms, None);
    assert!(
        consolidator
            .candidate(&snapshot, &host.practice.snapshot().unwrap())
            .is_none()
    );
    let practice = host.practice.snapshot().unwrap();
    assert_eq!(practice.runs[2].status, PracticeStatus::Interrupted);
    assert_eq!(settle(&host.skills, &practice, world.now()).unwrap(), 1);
    assert_eq!(
        host.skills
            .snapshot()
            .unwrap()
            .selection(&run.id)
            .unwrap()
            .outcome,
        Some(InvocationOutcome::Interrupted)
    );
    assert!(world.distiller.requests.lock().unwrap().len() == 1);
    host.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn failed_commit_closes_the_ledger_and_reopen_keeps_only_confirmed_state() {
    let world = World::new().await;
    world.practice("goal-1", vec![item("lamp", "7")]).await;
    let consolidator = world.consolidator();
    let candidate = consolidator
        .candidate(
            &world.host.skills.snapshot().unwrap(),
            &world.host.practice.snapshot().unwrap(),
        )
        .unwrap();
    world.store.fail_before.store(true, Ordering::SeqCst);
    assert_eq!(
        world
            .host
            .skills
            .begin_distillation(
                &candidate.owner,
                candidate.origin,
                candidate.source,
                consolidator.runner(),
                consolidator.distiller_version(),
                world.now(),
            )
            .err(),
        Some(SkillError::Storage)
    );
    assert_eq!(
        world.host.skills.snapshot().err(),
        Some(SkillError::Unavailable)
    );
    assert!(world.distiller.requests.lock().unwrap().is_empty());
    world.store.fail_before.store(false, Ordering::SeqCst);
    world.host.kernel.stop_all().await.unwrap();
    let host = open(world.store.clone()).await.unwrap();
    assert!(host.skills.snapshot().unwrap().distillations.is_empty());
    host.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn corrupt_or_inconsistent_ledgers_are_refused_and_preserved() {
    let world = World::new().await;
    world.practice("goal-1", vec![item("lamp", "7")]).await;
    world.consolidate(Ok(proposal(None, 900, "7"))).await;
    world.host.kernel.stop_all().await.unwrap();
    let valid = world.store.saved();

    let mut cases: Vec<Vec<u8>> = vec![
        b"{\"format_version\":1,\"format_version\":1,\"skills\":[],\"distillations\":[],\"selections\":[]}".to_vec(),
        serde_json::to_vec(&json!({"format_version": 2, "skills": [], "distillations": [], "selections": []})).unwrap(),
    ];
    let mutate = |change: &dyn Fn(&mut Value)| {
        let mut value = valid.clone();
        change(&mut value);
        serde_json::to_vec(&value).unwrap()
    };
    // 已验证却没有证据、证据带警告、版本指向不存在的提炼、启用不存在的版本、模板被改写。
    cases.push(mutate(&|value| {
        value["distillations"][0]["evidence"] = Value::Null
    }));
    cases.push(mutate(&|value| {
        value["distillations"][0]["evidence"]["warnings"] = json!(["warning"])
    }));
    cases.push(mutate(&|value| {
        value["skills"][0]["versions"][0]["distillation_id"] = json!("distill-missing")
    }));
    cases.push(mutate(&|value| value["skills"][0]["enabled"] = json!(3)));
    cases.push(mutate(&|value| {
        value["distillations"][0]["proposal"]["skill"]["template"]["files"][0]["content"] =
            json!("name: {{name}}\nvalue: 8\n")
    }));
    cases.push(mutate(&|value| {
        value["distillations"][0]["holdout"]["value"] = json!("899")
    }));
    for bytes in cases {
        world.store.overwrite(&bytes);
        assert!(open(world.store.clone()).await.is_err());
        assert_eq!(world.store.raw().unwrap(), bytes, "原字节保留");
    }
}
