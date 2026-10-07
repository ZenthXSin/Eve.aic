//! 真实 Kernel 与内存/故障状态存储；验证账本的持久化、CAS、恢复与失败语义。
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_plan_api::*;
use eve_plan_plugin::{PLAN_STATE_KEY, PlanController, PlanPlugin};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[derive(Default)]
struct FaultStore {
    memory: MemoryStateStore,
    writes: AtomicUsize,
    fail_on: AtomicUsize,
}
impl StateStore for FaultStore {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.memory.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        let call = self.writes.fetch_add(1, Ordering::SeqCst) + 1;
        if call == self.fail_on.load(Ordering::SeqCst) {
            return Err(PluginError::State("private storage fault".into()));
        }
        self.memory.set(namespace, key, value)
    }
}

fn plugin_id() -> PluginId {
    PluginId::new(PLAN_PLUGIN_ID).unwrap()
}
fn capabilities() -> Vec<CapabilitySpec> {
    vec![CapabilitySpec {
        id: "observe".into(),
        description: "读取文件".into(),
        max_attempts: 2,
        max_timeout_ms: 30_000,
        requires_input: false,
    }]
}
fn spec(goal: &str, title: &str) -> PlanSpec {
    PlanSpec {
        binding: PlanBinding {
            goal_id: goal.into(),
            goal_revision: 1,
            input_sha256: Some(A.into()),
        },
        steps: vec![
            StepSpec {
                id: "first".into(),
                title: title.into(),
                capability: "observe".into(),
                depends_on: vec![],
                max_attempts: 1,
                timeout_ms: 1000,
                effect: EffectCondition::DigestEquals { sha256: A.into() },
            },
            StepSpec {
                id: "second".into(),
                title: "等待输入变化".into(),
                capability: "observe".into(),
                depends_on: vec!["first".into()],
                max_attempts: 2,
                timeout_ms: 1000,
                effect: EffectCondition::DigestDiffers { sha256: A.into() },
            },
        ],
    }
}
fn evidence(sha256: &str, at: u64) -> StepOutcome {
    StepOutcome::Evidence(StepEvidence {
        capability: "observe".into(),
        source_id: "file-source".into(),
        sha256: sha256.into(),
        bytes: 4,
        verified_at_ms: at,
    })
}
async fn open(store: Arc<dyn StateStore>, subject: &str) -> PluginResult<(Kernel, PlanController)> {
    let kernel = Kernel::with_services(KernelServices {
        state: store,
        ..KernelServices::default()
    });
    let plugin = PlanPlugin::new(subject).unwrap();
    let journal = plugin.controller();
    assert_eq!(journal.snapshot().unwrap_err(), PlanError::Unavailable);
    kernel.register(Box::new(plugin))?;
    kernel.start_all().await?;
    Ok((kernel, journal))
}
fn stored(store: &dyn StateStore) -> Option<Vec<u8>> {
    store.get(&plugin_id(), PLAN_STATE_KEY).unwrap()
}

#[tokio::test]
async fn create_is_persisted_idempotent_and_one_active_plan_per_goal() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, journal) = open(store.clone(), "eve").await.unwrap();
    let created = journal
        .create(spec("goal", "核对输入"), &capabilities(), 10)
        .unwrap();
    assert!(!created.duplicate);
    assert_eq!(created.plan.ready_steps(), ["first"]);
    let bytes = stored(store.as_ref()).unwrap();
    let again = journal
        .create(spec("goal", "核对输入"), &capabilities(), 99)
        .unwrap();
    assert!(again.duplicate);
    assert_eq!(again.plan.created_at_ms, 10, "重复创建不改写原记录");
    assert_eq!(stored(store.as_ref()).unwrap(), bytes, "重复创建零写入");
    assert_eq!(
        journal
            .create(spec("goal", "另一份计划"), &capabilities(), 11)
            .unwrap_err(),
        PlanError::Conflict
    );
    assert_eq!(
        journal
            .create(spec("goal", "核对输入"), &[], 11)
            .unwrap_err(),
        PlanError::UnknownCapability
    );
    // 旧计划过时后可以为同一目标的新修订建立计划。
    let mut current = created.plan.binding.clone();
    current.goal_revision = 2;
    let stale = journal
        .invalidate(&created.plan.id, 1, &current, 12)
        .unwrap();
    assert_eq!(stale.status, PlanStatus::Stale);
    let mut next = spec("goal", "核对输入");
    next.binding.goal_revision = 2;
    assert!(!journal.create(next, &capabilities(), 13).unwrap().duplicate);
    kernel.stop_all().await.unwrap();
    assert_eq!(journal.snapshot().unwrap_err(), PlanError::Unavailable);

    let (kernel, reopened) = open(store, "eve").await.unwrap();
    let snapshot = reopened.snapshot().unwrap();
    assert_eq!(snapshot.plans.len(), 2);
    assert_eq!(snapshot.revision, 3);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn steps_require_current_revision_and_effects_are_judged_from_evidence() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, journal) = open(store.clone(), "eve").await.unwrap();
    let plan = journal
        .create(spec("goal", "核对输入"), &capabilities(), 10)
        .unwrap()
        .plan;
    assert_eq!(
        journal
            .begin_step(&plan.id, "second", plan.revision, 11)
            .unwrap_err(),
        PlanError::NotReady
    );
    let begun = journal
        .begin_step(&plan.id, "first", plan.revision, 11)
        .unwrap();
    assert_eq!(begun.step("first").unwrap().status, StepStatus::Executing);
    assert_eq!(
        journal
            .finish_step(&plan.id, "first", plan.revision, evidence(A, 12))
            .unwrap_err(),
        PlanError::StaleRevision
    );
    let done = journal
        .finish_step(&plan.id, "first", begun.revision, evidence(A, 12))
        .unwrap();
    assert_eq!(done.ready_steps(), ["second"]);
    let waiting = journal
        .begin_step(&plan.id, "second", done.revision, 13)
        .unwrap();
    let retry = journal
        .finish_step(&plan.id, "second", waiting.revision, evidence(A, 14))
        .unwrap();
    assert_eq!(retry.step("second").unwrap().status, StepStatus::Pending);
    let last = journal
        .begin_step(&plan.id, "second", retry.revision, 15)
        .unwrap();
    let completed = journal
        .finish_step(&plan.id, "second", last.revision, evidence(B, 16))
        .unwrap();
    assert_eq!(completed.status, PlanStatus::Completed);
    assert_eq!(
        journal
            .begin_step(&plan.id, "second", completed.revision, 17)
            .unwrap_err(),
        PlanError::NotReady
    );
    assert_eq!(
        journal
            .begin_step("plan:missing", "first", 1, 17)
            .unwrap_err(),
        PlanError::NotFound
    );
    kernel.stop_all().await.unwrap();
    let (kernel, reopened) = open(store, "eve").await.unwrap();
    assert_eq!(
        reopened.snapshot().unwrap().plans[0].status,
        PlanStatus::Completed
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn restart_interrupts_executing_step_without_replay() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, journal) = open(store.clone(), "eve").await.unwrap();
    let plan = journal
        .create(spec("goal", "核对输入"), &capabilities(), 10)
        .unwrap()
        .plan;
    journal
        .begin_step(&plan.id, "first", plan.revision, 11)
        .unwrap();
    kernel.stop_all().await.unwrap();

    let (kernel, reopened) = open(store.clone(), "eve").await.unwrap();
    let recovered = reopened.snapshot().unwrap().plans.remove(0);
    assert_eq!(recovered.status, PlanStatus::Blocked);
    let step = recovered.step("first").unwrap();
    assert_eq!(step.status, StepStatus::Blocked);
    assert_eq!(step.attempts[0].failure, Some(StepFailure::Interrupted));
    assert!(recovered.ready_steps().is_empty(), "中断后不重放");
    let bytes = stored(store.as_ref()).unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, _) = open(store.clone(), "eve").await.unwrap();
    assert_eq!(stored(store.as_ref()).unwrap(), bytes, "已封存记录不再改写");
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn failed_saves_keep_previous_state_and_recovery_failure_does_not_publish() {
    let store = Arc::new(FaultStore::default());
    let (kernel, journal) = open(store.clone(), "eve").await.unwrap();
    let plan = journal
        .create(spec("goal", "核对输入"), &capabilities(), 10)
        .unwrap()
        .plan;
    store.fail_on.store(2, Ordering::SeqCst);
    assert_eq!(
        journal
            .begin_step(&plan.id, "first", plan.revision, 11)
            .unwrap_err(),
        PlanError::Storage
    );
    let unchanged = journal.snapshot().unwrap().plans.remove(0);
    assert_eq!(unchanged.revision, plan.revision);
    assert_eq!(unchanged.step("first").unwrap().status, StepStatus::Pending);
    journal
        .begin_step(&plan.id, "first", plan.revision, 12)
        .unwrap();
    kernel.stop_all().await.unwrap();

    let executing = stored(store.as_ref()).unwrap();
    store
        .fail_on
        .store(store.writes.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
    assert!(open(store.clone(), "eve").await.is_err());
    assert_eq!(
        stored(store.as_ref()).unwrap(),
        executing,
        "恢复保存失败时保留原字节"
    );
    let (kernel, reopened) = open(store, "eve").await.unwrap();
    assert_eq!(
        reopened.snapshot().unwrap().plans[0].status,
        PlanStatus::Blocked
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn corrupt_duplicate_key_and_foreign_subject_state_is_preserved_and_rejected() {
    for bytes in [
        b"{not json".to_vec(),
        br#"{"schema_version":1,"schema_version":1,"subject_id":"eve","revision":0,"plans":[]}"#
            .to_vec(),
        br#"{"schema_version":3,"subject_id":"eve","revision":0,"plans":[]}"#.to_vec(),
        br#"{"schema_version":1,"subject_id":"eve","revision":0,"plans":[],"proposals":[]}"#
            .to_vec(),
        br#"{"schema_version":2,"subject_id":"eve","revision":0,"plans":[],"extra":1}"#.to_vec(),
    ] {
        let store = Arc::new(MemoryStateStore::default());
        store
            .set(&plugin_id(), PLAN_STATE_KEY.into(), bytes.clone())
            .unwrap();
        assert!(open(store.clone(), "eve").await.is_err());
        assert_eq!(stored(store.as_ref()).unwrap(), bytes);
    }
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, journal) = open(store.clone(), "eve").await.unwrap();
    journal
        .create(spec("goal", "核对输入"), &capabilities(), 10)
        .unwrap();
    kernel.stop_all().await.unwrap();
    let bytes = stored(store.as_ref()).unwrap();
    assert!(open(store.clone(), "other").await.is_err());
    assert_eq!(stored(store.as_ref()).unwrap(), bytes);
}

#[tokio::test]
async fn full_ledger_refuses_new_plans_without_pruning() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, journal) = open(store.clone(), "eve").await.unwrap();
    for index in 0..MAX_PLANS {
        journal
            .create(
                spec(&format!("goal-{index}"), "核对输入"),
                &capabilities(),
                10,
            )
            .unwrap();
    }
    assert_eq!(
        journal
            .create(spec("goal-new", "核对输入"), &capabilities(), 10)
            .unwrap_err(),
        PlanError::LimitReached
    );
    let first = journal
        .create(spec("goal-0", "核对输入"), &capabilities(), 10)
        .unwrap();
    assert!(first.duplicate, "已有记录仍可幂等读取");
    assert_eq!(journal.snapshot().unwrap().plans.len(), MAX_PLANS);
    assert_eq!(
        journal
            .reserve_proposal(&spec("goal-new", "核对输入").binding, "proposer:v1", 10)
            .unwrap_err(),
        PlanError::LimitReached,
        "建议无处保存时不请求模型"
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn unchanged_binding_is_a_zero_write_invalidation() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, journal) = open(store.clone(), "eve").await.unwrap();
    let plan = journal
        .create(spec("goal", "核对输入"), &capabilities(), 10)
        .unwrap()
        .plan;
    let bytes = stored(store.as_ref()).unwrap();
    let same = journal
        .invalidate(&plan.id, plan.revision, &plan.binding, 11)
        .unwrap();
    assert_eq!(same.revision, plan.revision);
    assert_eq!(stored(store.as_ref()).unwrap(), bytes);
    let mut changed = plan.binding.clone();
    changed.input_sha256 = Some(B.into());
    let stale = journal
        .invalidate(&plan.id, plan.revision, &changed, 12)
        .unwrap();
    assert_eq!(stale.status, PlanStatus::Stale);
    assert_eq!(
        stale.steps.iter().map(|s| s.status).collect::<Vec<_>>(),
        [StepStatus::Invalidated, StepStatus::Invalidated]
    );
    kernel.stop_all().await.unwrap();
}

fn binding(goal: &str, revision: u64) -> PlanBinding {
    PlanBinding {
        goal_id: goal.into(),
        goal_revision: revision,
        input_sha256: Some(A.into()),
    }
}

#[tokio::test]
async fn proposals_are_reserved_before_requests_and_saved_with_their_plan() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, journal) = open(store.clone(), "eve").await.unwrap();
    let reserved = journal
        .reserve_proposal(&binding("goal", 1), "proposer:v1", 10)
        .unwrap();
    assert!(!reserved.duplicate);
    assert_eq!(reserved.record.status, ProposalStatus::Requested);
    let requested = stored(store.as_ref()).unwrap();
    assert!(String::from_utf8_lossy(&requested).contains("\"requested\""));
    // 同一绑定已有记录：幂等返回，零写入，不得再次请求模型。
    let again = journal
        .reserve_proposal(&binding("goal", 1), "proposer:v2", 11)
        .unwrap();
    assert!(again.duplicate);
    assert_eq!(again.record.proposer, "proposer:v1");
    assert_eq!(stored(store.as_ref()).unwrap(), requested);
    // 请求进行中：同一目标不能另建计划或另发请求。
    assert_eq!(
        journal
            .create(spec("goal", "核对输入"), &capabilities(), 11)
            .unwrap_err(),
        PlanError::Conflict
    );
    assert_eq!(
        journal
            .reserve_proposal(&binding("goal", 2), "proposer:v1", 11)
            .unwrap_err(),
        PlanError::Conflict
    );

    let finished = journal
        .finish_proposal(
            &reserved.record.id,
            ProposalOutcome::Steps(spec("ignored", "核对输入").steps),
            &capabilities(),
            12,
        )
        .unwrap();
    let plan = finished.plan.unwrap();
    assert_eq!(plan.status, PlanStatus::Proposed);
    assert_eq!(plan.binding, binding("goal", 1), "计划绑定取自建议记录");
    assert_eq!(
        finished.record.status,
        ProposalStatus::Proposed {
            plan_id: plan.id.clone()
        }
    );
    assert_eq!(
        journal
            .finish_proposal(
                &reserved.record.id,
                ProposalOutcome::Empty,
                &capabilities(),
                13
            )
            .unwrap_err(),
        PlanError::InvalidTransition,
        "每条记录只结束一次"
    );
    assert_eq!(
        journal
            .begin_step(&plan.id, "first", plan.revision, 13)
            .unwrap_err(),
        PlanError::NotReady,
        "未确认不准入"
    );
    assert_eq!(
        journal
            .create(spec("goal", "核对输入"), &capabilities(), 13)
            .unwrap_err(),
        PlanError::Conflict,
        "待确认建议同样占用该目标"
    );
    let confirmed = journal
        .confirm(&plan.id, plan.revision, &plan.binding, 14)
        .unwrap();
    assert_eq!(confirmed.status, PlanStatus::Active);
    assert_eq!(
        journal
            .confirm(&plan.id, plan.revision, &plan.binding, 15)
            .unwrap_err(),
        PlanError::StaleRevision
    );
    let begun = journal
        .begin_step(&plan.id, "first", confirmed.revision, 15)
        .unwrap();
    assert_eq!(
        journal.withdraw(&plan.id, begun.revision, 16).unwrap_err(),
        PlanError::NotReady
    );
    let done = journal
        .finish_step(&plan.id, "first", begun.revision, evidence(A, 16))
        .unwrap();
    let withdrawn = journal.withdraw(&plan.id, done.revision, 17).unwrap();
    assert_eq!(withdrawn.status, PlanStatus::Withdrawn);
    kernel.stop_all().await.unwrap();

    let (kernel, reopened) = open(store, "eve").await.unwrap();
    let snapshot = reopened.snapshot().unwrap();
    assert_eq!(snapshot.schema_version, PLAN_SCHEMA_VERSION);
    assert_eq!(snapshot.plans[0].status, PlanStatus::Withdrawn);
    assert_eq!(snapshot.plans[0].confirmed_at_ms, Some(14));
    assert_eq!(snapshot.proposals.len(), 1);
    // 撤销后同一目标可以为新修订再次请求建议。
    assert!(
        !reopened
            .reserve_proposal(&binding("goal", 2), "proposer:v1", 20)
            .unwrap()
            .duplicate
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn restart_marks_requested_proposals_interrupted_without_replay() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, journal) = open(store.clone(), "eve").await.unwrap();
    let record = journal
        .reserve_proposal(&binding("goal", 1), "proposer:v1", 10)
        .unwrap()
        .record;
    kernel.stop_all().await.unwrap();
    let (kernel, reopened) = open(store.clone(), "eve").await.unwrap();
    let snapshot = reopened.snapshot().unwrap();
    assert_eq!(
        snapshot.proposals[0].status,
        ProposalStatus::Failed {
            failure: ProposalFailure::Interrupted
        }
    );
    assert!(snapshot.proposals[0].finished_at_ms.unwrap() >= 10);
    assert_eq!(
        reopened
            .finish_proposal(&record.id, ProposalOutcome::Empty, &capabilities(), 11)
            .unwrap_err(),
        PlanError::InvalidTransition,
        "中断后迟到的结果不能写回"
    );
    let reserved = reopened
        .reserve_proposal(&binding("goal", 1), "proposer:v1", 12)
        .unwrap();
    assert!(reserved.duplicate, "同一绑定不因中断而重新请求");
    // 中断不再占用目标，操作者仍可建立计划。
    reopened
        .create(spec("goal", "核对输入"), &capabilities(), 13)
        .unwrap();
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn invalid_or_empty_proposals_keep_no_plan_and_capacity_is_never_pruned() {
    let store = Arc::new(FaultStore::default());
    let (kernel, journal) = open(store.clone(), "eve").await.unwrap();
    let mut steps = spec("goal", "核对输入").steps;
    steps[0].capability = "shell".into();
    let id = journal
        .reserve_proposal(&binding("goal", 1), "proposer:v1", 10)
        .unwrap()
        .record
        .id;
    let invalid = journal
        .finish_proposal(&id, ProposalOutcome::Steps(steps), &capabilities(), 11)
        .unwrap();
    assert!(invalid.plan.is_none());
    assert_eq!(
        invalid.record.status,
        ProposalStatus::Failed {
            failure: ProposalFailure::InvalidOutput
        }
    );
    let id = journal
        .reserve_proposal(&binding("goal", 2), "proposer:v1", 12)
        .unwrap()
        .record
        .id;
    // 保存结果失败：记录仍为 Requested，内存不变。
    store
        .fail_on
        .store(store.writes.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
    assert_eq!(
        journal
            .finish_proposal(&id, ProposalOutcome::Empty, &capabilities(), 13)
            .unwrap_err(),
        PlanError::Storage
    );
    assert_eq!(
        journal.snapshot().unwrap().proposals[1].status,
        ProposalStatus::Requested
    );
    let empty = journal
        .finish_proposal(&id, ProposalOutcome::Empty, &capabilities(), 14)
        .unwrap();
    assert_eq!(empty.record.status, ProposalStatus::Empty);
    assert!(journal.snapshot().unwrap().plans.is_empty());

    for revision in 3..=MAX_PROPOSALS as u64 {
        let id = journal
            .reserve_proposal(&binding("goal", revision), "proposer:v1", 20)
            .unwrap()
            .record
            .id;
        journal
            .finish_proposal(
                &id,
                ProposalOutcome::Failed(ProposalFailure::Timeout),
                &capabilities(),
                21,
            )
            .unwrap();
    }
    assert_eq!(
        journal
            .reserve_proposal(&binding("goal", 999), "proposer:v1", 22)
            .unwrap_err(),
        PlanError::LimitReached
    );
    assert!(
        journal
            .reserve_proposal(&binding("goal", 1), "proposer:v1", 22)
            .unwrap()
            .duplicate,
        "已有记录仍可幂等读取"
    );
    assert_eq!(journal.snapshot().unwrap().proposals.len(), MAX_PROPOSALS);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn version_one_ledgers_are_read_without_rewrite_and_saved_as_current_version() {
    let store = Arc::new(MemoryStateStore::default());
    let (kernel, journal) = open(store.clone(), "eve").await.unwrap();
    journal
        .create(spec("goal", "核对输入"), &capabilities(), 10)
        .unwrap();
    kernel.stop_all().await.unwrap();
    // 构造版本 1 写出的字节：没有建议记录与新字段。
    let mut value: serde_json::Value =
        serde_json::from_slice(&stored(store.as_ref()).unwrap()).unwrap();
    value["schema_version"] = 1.into();
    value.as_object_mut().unwrap().remove("proposals");
    for field in ["origin", "confirmed_at_ms", "withdrawn_at_ms"] {
        value["plans"][0].as_object_mut().unwrap().remove(field);
    }
    let v1 = serde_json::to_vec(&value).unwrap();
    store
        .set(&plugin_id(), PLAN_STATE_KEY.into(), v1.clone())
        .unwrap();

    let (kernel, reopened) = open(store.clone(), "eve").await.unwrap();
    let snapshot = reopened.snapshot().unwrap();
    assert_eq!(snapshot.schema_version, PLAN_SCHEMA_VERSION);
    assert_eq!(snapshot.plans[0].origin, PlanOrigin::Operator);
    assert_eq!(
        stored(store.as_ref()).unwrap(),
        v1,
        "只读打开不改写版本 1 字节"
    );
    let plan = snapshot.plans[0].clone();
    reopened
        .begin_step(&plan.id, "first", plan.revision, 11)
        .unwrap();
    let saved: serde_json::Value =
        serde_json::from_slice(&stored(store.as_ref()).unwrap()).unwrap();
    assert_eq!(saved["schema_version"], PLAN_SCHEMA_VERSION);
    assert_eq!(
        saved["plans"][0]["id"], plan.id,
        "操作者计划 ID 不因升级改变"
    );
    kernel.stop_all().await.unwrap();
}
