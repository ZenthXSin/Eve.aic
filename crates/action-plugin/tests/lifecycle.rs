use eve_action_api::*;
use eve_action_plugin::{
    ACTION_STATE_KEY, ActionCancellation, ActionController, ActionPlugin, execute_document_action,
};
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;

const DOCUMENT: &[u8] = b"validated plan\n";
const DOCUMENT_SHA: &str = "62e1713dcd19a03ea74bc9aac1d88839b4df7ddb38401b6bc95348438ed8fbb4";

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

async fn open(store: Arc<dyn StateStore>) -> (Kernel, ActionController) {
    let kernel = Kernel::with_services(KernelServices {
        state: store,
        ..KernelServices::default()
    });
    let plugin = ActionPlugin::new("eve").unwrap();
    let journal = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, journal)
}

fn proposal() -> DocumentActionProposal {
    DocumentActionProposal {
        schema_version: ACTION_SCHEMA_VERSION,
        action_id: derive_action_id("eve", "alice", "parent", 2, "reflection").unwrap(),
        subject_id: "eve".into(),
        user_id: "alice".into(),
        goal_id: "parent".into(),
        goal_revision: 2,
        reflection_goal_id: "reflection".into(),
        reflection_goal_revision: 3,
        observation_event_id: format!("file-observation:{}", "1".repeat(64)),
        observation_source_id: format!("file-source:{}", "2".repeat(64)),
        input_sha256: "3".repeat(64),
        input_byte_count: 10,
        artifact_source_id: format!("artifact-file:{}", "4".repeat(64)),
        artifact_sha256: DOCUMENT_SHA.into(),
        artifact_byte_count: DOCUMENT.len() as u64,
        created_at_ms: 1,
        timeout_ms: 5_000,
    }
}

#[derive(Default)]
struct Gate {
    entered: Notify,
    released: Mutex<bool>,
    wake: Condvar,
}

impl Gate {
    fn block(&self) {
        self.entered.notify_one();
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.wake.wait(released).unwrap();
        }
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}

struct Target {
    source_id: String,
    writes: AtomicUsize,
    reads: AtomicUsize,
    write_finished: AtomicBool,
    mismatch: bool,
    panic_on_write: bool,
    write_error: Option<ArtifactError>,
    gate: Option<Arc<Gate>>,
}

impl Default for Target {
    fn default() -> Self {
        Self {
            source_id: proposal().artifact_source_id,
            writes: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
            write_finished: AtomicBool::new(false),
            mismatch: false,
            panic_on_write: false,
            write_error: None,
            gate: None,
        }
    }
}

impl ArtifactTarget for Target {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    fn write_new(&self, bytes: &[u8]) -> ArtifactResult<()> {
        assert_eq!(bytes, DOCUMENT);
        self.writes.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic_on_write, "本测试注入文件工作线程 panic");
        if let Some(gate) = &self.gate {
            gate.block();
        }
        self.write_finished.store(true, Ordering::SeqCst);
        self.write_error.map_or(Ok(()), Err)
    }

    fn read_back(&self, verified_at_ms: u64) -> ArtifactResult<ArtifactReceipt> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        assert!(self.write_finished.load(Ordering::SeqCst));
        Ok(ArtifactReceipt {
            artifact_source_id: self.source_id.clone(),
            sha256: if self.mismatch {
                "0".repeat(64)
            } else {
                DOCUMENT_SHA.into()
            },
            byte_count: DOCUMENT.len() as u64,
            verified_at_ms,
        })
    }
}

#[derive(Default)]
struct Precondition {
    calls: AtomicUsize,
    fail_on: usize,
}

impl ActionPrecondition for Precondition {
    fn check(&self, _: &DocumentActionProposal) -> ActionResult<()> {
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.fail_on {
            Err(ActionError::StaleRevision)
        } else {
            Ok(())
        }
    }
}

async fn execute(
    journal: &ActionController,
    target: Arc<Target>,
) -> ActionResult<ActionExecutionReport> {
    execute_document_action(
        Arc::new(journal.clone()),
        target,
        proposal(),
        DOCUMENT.to_vec(),
        Arc::new(Precondition::default()),
        ActionCancellation::new(),
    )
    .await
}

#[tokio::test]
async fn executing_save_failure_never_reaches_the_file_target() {
    let store = Arc::new(FaultStore::default());
    store.fail_on.store(1, Ordering::SeqCst);
    let (kernel, journal) = open(store.clone()).await;
    let target = Arc::new(Target::default());
    assert_eq!(
        execute(&journal, target.clone()).await,
        Err(ActionError::Storage)
    );
    assert_eq!(target.writes.load(Ordering::SeqCst), 0);
    assert_eq!(target.reads.load(Ordering::SeqCst), 0);
    assert!(journal.snapshot().unwrap().records.is_empty());
    assert!(
        store
            .get(&PluginId::new(ACTION_PLUGIN_ID).unwrap(), ACTION_STATE_KEY)
            .unwrap()
            .is_none()
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn finish_save_failure_preserves_executing_and_restart_interrupts_without_retry() {
    let store = Arc::new(FaultStore::default());
    store.fail_on.store(2, Ordering::SeqCst);
    let (kernel, journal) = open(store.clone()).await;
    let target = Arc::new(Target::default());
    assert_eq!(
        execute(&journal, target.clone()).await,
        Err(ActionError::Storage)
    );
    assert_eq!(target.writes.load(Ordering::SeqCst), 1);
    assert_eq!(target.reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        journal.snapshot().unwrap().records[0].status,
        ActionStatus::Executing
    );
    let replay = execute(&journal, target.clone()).await.unwrap();
    assert!(replay.duplicate);
    assert_eq!(replay.record.status, ActionStatus::Executing);
    assert_eq!(target.writes.load(Ordering::SeqCst), 1);
    kernel.stop_all().await.unwrap();

    store.fail_on.store(0, Ordering::SeqCst);
    let (kernel, recovered) = open(store.clone()).await;
    let replay = execute(&recovered, target.clone()).await.unwrap();
    assert!(replay.duplicate);
    assert_eq!(replay.record.status, ActionStatus::Blocked);
    assert_eq!(replay.record.failure, Some(ActionFailure::Interrupted));
    assert_eq!(target.writes.load(Ordering::SeqCst), 1);
    assert_eq!(store.writes.load(Ordering::SeqCst), 3);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn recovery_save_failure_does_not_publish_or_clear_original_executing_bytes() {
    let store = Arc::new(FaultStore::default());
    let (kernel, journal) = open(store.clone()).await;
    journal.begin(proposal()).unwrap();
    kernel.stop_all().await.unwrap();
    let namespace = PluginId::new(ACTION_PLUGIN_ID).unwrap();
    let original = store.get(&namespace, ACTION_STATE_KEY).unwrap();
    store.fail_on.store(2, Ordering::SeqCst);
    let kernel = Kernel::with_services(KernelServices {
        state: store.clone(),
        ..KernelServices::default()
    });
    let plugin = ActionPlugin::new("eve").unwrap();
    let controller = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    assert!(kernel.start_all().await.is_err());
    assert_eq!(controller.snapshot(), Err(ActionError::Unavailable));
    assert_eq!(store.get(&namespace, ACTION_STATE_KEY).unwrap(), original);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn corrupt_or_duplicate_json_is_preserved_and_never_published() {
    for bytes in [
        b"private broken state".to_vec(),
        br#"{"schema_version":1,"schema_version":1,"subject_id":"eve","revision":0,"records":[]}"#
            .to_vec(),
    ] {
        let store = Arc::new(FaultStore::default());
        let namespace = PluginId::new(ACTION_PLUGIN_ID).unwrap();
        store
            .set(&namespace, ACTION_STATE_KEY.into(), bytes.clone())
            .unwrap();
        let kernel = Kernel::with_services(KernelServices {
            state: store.clone(),
            ..KernelServices::default()
        });
        let plugin = ActionPlugin::new("eve").unwrap();
        let controller = plugin.controller();
        kernel.register(Box::new(plugin)).unwrap();
        assert!(kernel.start_all().await.is_err());
        assert_eq!(controller.snapshot(), Err(ActionError::Unavailable));
        assert_eq!(
            store.get(&namespace, ACTION_STATE_KEY).unwrap(),
            Some(bytes)
        );
        assert_eq!(store.writes.load(Ordering::SeqCst), 1);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn changed_preconditions_before_or_after_begin_never_write() {
    for fail_on in [1, 2] {
        let (kernel, journal) = open(Arc::new(FaultStore::default())).await;
        let target = Arc::new(Target::default());
        let result = execute_document_action(
            Arc::new(journal.clone()),
            target.clone(),
            proposal(),
            DOCUMENT.to_vec(),
            Arc::new(Precondition {
                fail_on,
                ..Precondition::default()
            }),
            ActionCancellation::new(),
        )
        .await;
        if fail_on == 1 {
            assert_eq!(result, Err(ActionError::StaleRevision));
            assert!(journal.snapshot().unwrap().records.is_empty());
        } else {
            let report = result.unwrap();
            assert_eq!(report.record.status, ActionStatus::Blocked);
            assert_eq!(
                report.record.failure,
                Some(ActionFailure::PreconditionChanged)
            );
        }
        assert_eq!(target.writes.load(Ordering::SeqCst), 0);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn duplicate_is_zero_write_and_different_target_conflicts() {
    let (kernel, journal) = open(Arc::new(FaultStore::default())).await;
    let target = Arc::new(Target::default());
    let first = execute(&journal, target.clone()).await.unwrap();
    assert_eq!(first.record.status, ActionStatus::Completed);
    let original = journal.snapshot().unwrap();
    let second = execute(&journal, target.clone()).await.unwrap();
    assert!(second.duplicate);
    assert_eq!(second.record, first.record);
    assert_eq!(target.writes.load(Ordering::SeqCst), 1);
    let other = Arc::new(Target {
        source_id: format!("artifact-file:{}", "5".repeat(64)),
        ..Target::default()
    });
    let mut different = proposal();
    different.artifact_source_id = other.source_id.clone();
    assert_eq!(
        execute_document_action(
            Arc::new(journal.clone()),
            other.clone(),
            different,
            DOCUMENT.to_vec(),
            Arc::new(Precondition::default()),
            ActionCancellation::new(),
        )
        .await,
        Err(ActionError::Conflict)
    );
    assert_eq!(other.writes.load(Ordering::SeqCst), 0);
    assert_eq!(journal.snapshot().unwrap(), original);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn a_mismatched_independent_receipt_blocks_the_action() {
    let (kernel, journal) = open(Arc::new(FaultStore::default())).await;
    let target = Arc::new(Target {
        mismatch: true,
        ..Target::default()
    });
    let report = execute(&journal, target.clone()).await.unwrap();
    assert_eq!(report.record.status, ActionStatus::Blocked);
    assert_eq!(
        report.record.failure,
        Some(ActionFailure::VerificationFailed)
    );
    assert!(report.record.receipt.is_none());
    assert_eq!(target.reads.load(Ordering::SeqCst), 1);
    assert!(execute(&journal, target.clone()).await.unwrap().duplicate);
    assert_eq!(target.writes.load(Ordering::SeqCst), 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn already_existing_target_is_blocked_without_retry_or_readback() {
    let (kernel, journal) = open(Arc::new(FaultStore::default())).await;
    let target = Arc::new(Target {
        write_error: Some(ArtifactError::AlreadyExists),
        ..Target::default()
    });
    let report = execute(&journal, target.clone()).await.unwrap();
    assert_eq!(report.record.failure, Some(ActionFailure::WriteFailed));
    assert_eq!(target.reads.load(Ordering::SeqCst), 0);
    assert!(execute(&journal, target.clone()).await.unwrap().duplicate);
    assert_eq!(target.writes.load(Ordering::SeqCst), 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn file_worker_panic_is_interrupted_and_never_replayed() {
    let (kernel, journal) = open(Arc::new(FaultStore::default())).await;
    let target = Arc::new(Target {
        panic_on_write: true,
        ..Target::default()
    });
    let report = execute(&journal, target.clone()).await.unwrap();
    assert_eq!(report.record.status, ActionStatus::Blocked);
    assert_eq!(report.record.failure, Some(ActionFailure::Interrupted));
    assert!(report.record.receipt.is_none());
    assert_eq!(target.writes.load(Ordering::SeqCst), 1);
    assert_eq!(target.reads.load(Ordering::SeqCst), 0);
    assert!(execute(&journal, target.clone()).await.unwrap().duplicate);
    assert_eq!(target.writes.load(Ordering::SeqCst), 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn precancelled_action_never_creates_a_record_or_writes() {
    let (kernel, journal) = open(Arc::new(FaultStore::default())).await;
    let target = Arc::new(Target::default());
    let cancellation = ActionCancellation::new();
    cancellation.cancel();
    assert_eq!(
        execute_document_action(
            Arc::new(journal.clone()),
            target.clone(),
            proposal(),
            DOCUMENT.to_vec(),
            Arc::new(Precondition::default()),
            cancellation,
        )
        .await,
        Err(ActionError::Cancelled)
    );
    assert!(journal.snapshot().unwrap().records.is_empty());
    assert_eq!(target.writes.load(Ordering::SeqCst), 0);
    kernel.stop_all().await.unwrap();
}

async fn interruption_waits_for_owned_write(deadline: bool) {
    let (kernel, journal) = open(Arc::new(FaultStore::default())).await;
    let gate = Arc::new(Gate::default());
    let target = Arc::new(Target {
        gate: Some(gate.clone()),
        ..Target::default()
    });
    let cancellation = ActionCancellation::new();
    let mut request = proposal();
    if deadline {
        request.timeout_ms = 100;
    }
    let replay_request = request.clone();
    let task = tokio::spawn(execute_document_action(
        Arc::new(journal.clone()),
        target.clone(),
        request,
        DOCUMENT.to_vec(),
        Arc::new(Precondition::default()),
        cancellation.clone(),
    ));
    let entered = tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await;
    if entered.is_err() {
        gate.release();
    }
    entered.expect("文件工作线程没有到达本测试持有的门闩");
    if !deadline {
        cancellation.cancel();
    }
    tokio::time::sleep(Duration::from_millis(if deadline { 150 } else { 30 })).await;
    let finished_early = task.is_finished();
    let while_writing = journal.snapshot().unwrap();
    gate.release();
    let report = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!finished_early, "行动不能在持有的写线程退出前宣称完成");
    assert_eq!(while_writing.records[0].status, ActionStatus::Executing);
    assert!(target.write_finished.load(Ordering::SeqCst));
    assert_eq!(report.record.status, ActionStatus::Blocked);
    assert_eq!(
        report.record.failure,
        Some(if deadline {
            ActionFailure::DeadlineExceeded
        } else {
            ActionFailure::Cancelled
        })
    );
    assert_eq!(target.writes.load(Ordering::SeqCst), 1);
    assert!(
        execute_document_action(
            Arc::new(journal.clone()),
            target.clone(),
            replay_request,
            DOCUMENT.to_vec(),
            Arc::new(Precondition::default()),
            ActionCancellation::new(),
        )
        .await
        .unwrap()
        .duplicate
    );
    assert_eq!(target.writes.load(Ordering::SeqCst), 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn cancellation_settles_inflight_write_before_saving_blocked() {
    interruption_waits_for_owned_write(false).await;
}

#[tokio::test]
async fn deadline_settles_inflight_write_before_saving_blocked() {
    interruption_waits_for_owned_write(true).await;
}
