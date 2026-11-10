//! 真实文件根与不可变归档：迁移、后续准入、原来源、分页及确认失败均不能丢记录。
mod support;

use eve_interest_api::*;
use eve_interest_plugin::{FileInterestJobArchive, InterestController, InterestPlugin};
use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{ChatMessage, ChatRole};
use eve_memory_api::{
    CompletedInteraction, InteractionEvidence, MemoryAdmin, MemoryError, MemoryEvidenceCursor,
    MemoryResult, MemoryScanSummary, MemoryScope, MemoryService, MemorySnapshot,
    ValidatedMemoryCursor,
};
use eve_memory_plugin::MemoryPlugin;
use eve_plugin_api::{PluginId, ServiceId, StateStore};
use eve_session_api::{
    SESSION_SERVICE_ID, SessionInput, SessionKey, SessionServiceHandle, SessionTurnStatus,
};
use eve_session_plugin::SessionPlugin;
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use support::*;

fn owner() -> PluginId {
    PluginId::new(INTEREST_PLUGIN_ID).unwrap()
}
fn raw(state: &dyn StateStore) -> Vec<u8> {
    state.get(&owner(), INTEREST_STATE_KEY).unwrap().unwrap()
}
fn document(state: &dyn StateStore) -> Value {
    serde_json::from_slice(&raw(state)).unwrap()
}
fn bytes_tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn collect(root: &Path, path: &Path, rows: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(root, &path, rows);
            } else {
                rows.push((
                    path.strip_prefix(root).unwrap().into(),
                    fs::read(&path).unwrap(),
                ));
            }
        }
    }
    let mut rows = vec![];
    collect(root, root, &mut rows);
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    rows
}
async fn start(
    state: Arc<dyn StateStore>,
    archive: Option<Arc<dyn InterestJobArchiveStore>>,
) -> (Kernel, InterestController, bool) {
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let mut plugin = InterestPlugin::new().unwrap();
    if let Some(archive) = archive {
        plugin = plugin.with_archive(archive);
    }
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    let started = kernel.start_all().await.is_ok();
    (kernel, admin, started)
}
async fn open(
    state: Arc<dyn StateStore>,
    archive: Option<Arc<dyn InterestJobArchiveStore>>,
) -> (Kernel, InterestController) {
    let (kernel, admin, started) = start(state, archive).await;
    assert!(started, "合法文件账本应能启动");
    (kernel, admin)
}
fn source(scope: &MemoryScope, count: usize) -> MemorySnapshot {
    let mut result = memory(scope.clone(), &vec![INTEREST; count]);
    result.revision = count as u64;
    result
}
fn all_jobs(admin: &dyn InterestAdmin, scope: &MemoryScope, limit: usize) -> Vec<ObservationJob> {
    let mut after = 0;
    let mut jobs = vec![];
    loop {
        let page = admin.jobs_page(scope, after, limit).unwrap();
        assert!(page.jobs.len() <= limit);
        assert!(page.jobs.iter().all(|job| job.batch.scope == *scope));
        jobs.extend(page.jobs);
        match page.next_after {
            None => return jobs,
            Some(next) => {
                assert!(next > after);
                after = next;
            }
        }
    }
}

/// 无归档的真实旧 API；最后一项经真实重启成为 Interrupted，不手拼 JobRecord 或摘要。
async fn legacy(state: Arc<RecordingStore>, count: usize) -> (MemoryScope, Vec<ObservationJob>) {
    let alice = scope("legacy-session", "alice");
    let (kernel, admin) = open(state.clone(), None).await;
    for index in 1..=count {
        let batch = reserve(&admin, &source(&alice, index), 1000 + index as u64);
        if index == count {
            continue;
        }
        let outcome = match index {
            1 => ObservationOutcome::Completed(vec![new_interest(&batch, "模组创作")]),
            2 => ObservationOutcome::Failed(ObservationFailure::Provider),
            3 => ObservationOutcome::Failed(ObservationFailure::Cancelled),
            _ => ObservationOutcome::Completed(vec![]),
        };
        admin
            .finish(&batch, batch.started_at_ms + 1, outcome)
            .unwrap();
    }
    kernel.stop_all().await.unwrap();
    let (kernel, admin) = open(state.clone(), None).await;
    let saved = admin.snapshot(&alice).unwrap();
    assert_eq!(saved.jobs.len(), count);
    assert_eq!(saved.jobs.last().unwrap().status, JobStatus::Interrupted);
    assert_eq!(saved.jobs.last().unwrap().finished_at_ms, None);
    assert_eq!(document(state.as_ref())["format_version"], 1);
    kernel.stop_all().await.unwrap();
    (alice, saved.jobs)
}

struct FaultArchive {
    inner: FileInterestJobArchive,
    fail_before: AtomicBool,
    fail_after: AtomicBool,
    writes: AtomicUsize,
    reads: AtomicUsize,
}
impl FaultArchive {
    fn open(path: &Path) -> Arc<Self> {
        Arc::new(Self {
            inner: FileInterestJobArchive::open(path).unwrap(),
            fail_before: AtomicBool::new(false),
            fail_after: AtomicBool::new(false),
            writes: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
        })
    }
}
impl InterestJobArchiveStore for FaultArchive {
    fn is_empty(&self) -> InterestResult<bool> {
        self.inner.is_empty()
    }
    fn put(&self, hash: &str, bytes: &[u8]) -> InterestResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        if self.fail_before.load(Ordering::SeqCst) {
            return Err(InterestError::Storage);
        }
        self.inner.put(hash, bytes)?;
        if self.fail_after.load(Ordering::SeqCst) {
            return Err(InterestError::Storage);
        }
        Ok(())
    }
    fn get(&self, hash: &str) -> InterestResult<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get(hash)
    }
}

#[tokio::test]
async fn migrate_128_real_legacy_jobs_and_admit_129_without_replaying_or_losing_originals() {
    let directory = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(&directory.path().join("state"));
    let (alice, saved) = legacy(state.clone(), 128).await;
    let before = raw(state.as_ref());
    let (kernel, admin) = open(state.clone(), None).await;
    let writes = state.writes();
    assert_eq!(
        admin.reserve(&source(&alice, 129), 5000, "observer-v1", &options()),
        Err(InterestError::LimitReached)
    );
    assert_eq!(raw(state.as_ref()), before);
    assert_eq!(state.writes(), writes);
    kernel.stop_all().await.unwrap();

    let archive = FaultArchive::open(&directory.path().join("archive"));
    let (kernel, admin) = open(state.clone(), Some(archive.clone())).await;
    assert_eq!(document(state.as_ref())["format_version"], 2);
    let reads = archive.reads.load(Ordering::SeqCst);
    let snapshot = admin.snapshot(&alice).unwrap();
    assert!(snapshot.jobs.is_empty());
    assert_eq!(snapshot.archived_jobs, 128);
    assert_eq!(
        archive.reads.load(Ordering::SeqCst),
        reads,
        "普通快照不读冷正文"
    );
    assert_eq!(all_jobs(&admin, &alice, 20), saved);
    let status = admin.archive_status().unwrap();
    assert!(status.enabled);
    assert_eq!(status.hot_jobs, 0);
    assert_eq!(status.archived_jobs, 128);
    assert!(status.archived_bytes > 0 && status.index_bytes > 0);
    let reads = archive.reads.load(Ordering::SeqCst);
    assert_eq!(
        admin
            .reserve(&source(&alice, 128), 5000, "observer-v1", &options())
            .unwrap(),
        None
    );
    assert_eq!(
        archive.reads.load(Ordering::SeqCst),
        reads,
        "无新增来源的空闲轮询只用已核实的根摘要"
    );
    let writes = state.writes();
    let archive_writes = archive.writes.load(Ordering::SeqCst);
    assert_eq!(
        admin
            .finish(
                &saved[0].batch,
                6000,
                ObservationOutcome::Completed(saved[0].updates.clone())
            )
            .unwrap(),
        saved[0].results
    );
    assert_eq!(
        admin
            .finish(
                &saved[1].batch,
                6000,
                ObservationOutcome::Failed(ObservationFailure::Provider)
            )
            .unwrap(),
        vec![]
    );
    assert_eq!(
        admin.finish(&saved[0].batch, 6000, ObservationOutcome::Completed(vec![])),
        Err(InterestError::Conflict)
    );
    assert_eq!(state.writes(), writes);
    assert_eq!(archive.writes.load(Ordering::SeqCst), archive_writes);
    let reads = archive.reads.load(Ordering::SeqCst);
    let next = reserve(&admin, &source(&alice, 129), 7000);
    assert!(
        archive.reads.load(Ordering::SeqCst) >= reads + 128,
        "真正准入新任务前必须核对全部冷正文"
    );
    assert_eq!(next.evidence.len(), 1);
    assert_eq!(next.evidence[0], source(&alice, 129).evidence[128]);
    admin
        .finish(&next, 7001, ObservationOutcome::Completed(vec![]))
        .unwrap();
    let jobs = all_jobs(&admin, &alice, 7);
    assert_eq!(&jobs[..128], saved.as_slice());
    assert_eq!(jobs.len(), 129);
    assert_eq!(
        jobs[0].updates[0].statements[0].quote,
        "我喜欢 Mindustry 这个游戏的模组"
    );
    assert_eq!(
        jobs[1].status,
        JobStatus::Failed(ObservationFailure::Provider)
    );
    assert_eq!(
        jobs[2].status,
        JobStatus::Failed(ObservationFailure::Cancelled)
    );
    assert_eq!(jobs[127].status, JobStatus::Interrupted);
    let retained = raw(state.as_ref());
    kernel.stop_all().await.unwrap();
    let (kernel, admin) = open(state.clone(), Some(archive)).await;
    assert_eq!(raw(state.as_ref()), retained);
    assert_eq!(all_jobs(&admin, &alice, 20), jobs);
    assert_eq!(
        admin
            .reserve(&source(&alice, 129), 8000, "observer-v1", &options())
            .unwrap(),
        None
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn pages_are_scope_bound_in_original_order_with_strict_limits() {
    let directory = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(&directory.path().join("state"));
    let archive = Arc::new(FileInterestJobArchive::open(directory.path().join("archive")).unwrap());
    let (kernel, admin) = open(state, Some(archive)).await;
    let alice = scope("a", "alice");
    let bob = scope("b", "bob");
    let mut expected = vec![];
    for index in 1..=25 {
        for who in [&alice, &bob] {
            let batch = reserve(&admin, &source(who, index), 1000 + index as u64);
            admin
                .finish(
                    &batch,
                    batch.started_at_ms + 1,
                    ObservationOutcome::Completed(vec![]),
                )
                .unwrap();
            if who == &alice {
                expected.push(batch.id);
            }
        }
    }
    for limit in [1, 7, 20] {
        assert_eq!(
            all_jobs(&admin, &alice, limit)
                .iter()
                .map(|j| j.batch.id.clone())
                .collect::<Vec<_>>(),
            expected
        );
    }
    assert!(admin.jobs_page(&alice, 0, 0).is_err());
    assert!(admin.jobs_page(&alice, 0, 21).is_err());
    assert!(
        admin
            .jobs_page(&alice, u64::MAX, 20)
            .unwrap()
            .jobs
            .is_empty()
    );
    assert!(
        admin
            .jobs_page(&scope("empty", "nobody"), 0, 20)
            .unwrap()
            .jobs
            .is_empty()
    );
    assert_eq!(admin.snapshot(&alice).unwrap().archived_jobs, 25);
    assert_eq!(admin.archive_status().unwrap().archived_jobs, 50);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn idle_and_cooldown_skip_cold_bodies_but_new_source_admission_validates_all_of_them() {
    let directory = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(&directory.path().join("state"));
    let (alice, saved) = legacy(state.clone(), 4).await;
    let archive = FaultArchive::open(&directory.path().join("archive"));
    let (kernel, admin) = open(state.clone(), Some(archive.clone())).await;
    let reads = archive.reads.load(Ordering::SeqCst);
    let writes = state.writes();
    let original = raw(state.as_ref());
    let cooling = ObservationOptions {
        cooldown_ms: 10_000,
        ..options()
    };
    for (count, options) in [(4, options()), (5, cooling)] {
        let memory = source(&alice, count);
        assert_eq!(
            admin
                .reserve(&memory, 5000, "observer-v1", &options)
                .unwrap(),
            None
        );
        let reader = SourceReader {
            snapshot: memory,
            missing: false,
            tamper: false,
        };
        assert_eq!(
            admin
                .reserve_reader(&alice, &reader, 5000, "observer-v1", &options)
                .unwrap(),
            None
        );
        assert_eq!(archive.reads.load(Ordering::SeqCst), reads);
        assert_eq!(state.writes(), writes);
        assert_eq!(raw(state.as_ref()), original);
    }
    let reader = SourceReader {
        snapshot: source(&alice, 5),
        missing: false,
        tamper: false,
    };
    let next = admin
        .reserve_reader(&alice, &reader, 20_000, "observer-v1", &options())
        .unwrap()
        .unwrap();
    assert_eq!(next.evidence, vec![source(&alice, 5).evidence[4].clone()]);
    assert!(archive.reads.load(Ordering::SeqCst) >= reads + 4);
    assert_eq!(state.writes(), writes + 1);
    assert_eq!(&all_jobs(&admin, &alice, 20)[..4], saved.as_slice());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn cold_damage_during_runtime_rejects_new_admission_before_write_and_closes_instance() {
    for (damage, use_reader) in [
        ("missing", false),
        ("body", false),
        ("missing", true),
        ("body", true),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let state = RecordingStore::open(&directory.path().join("state"));
        let archive_path = directory.path().join("archive");
        let (alice, saved) = legacy(state.clone(), 4).await;
        let archive = FaultArchive::open(&archive_path);
        let (kernel, admin) = open(state.clone(), Some(archive.clone())).await;
        let reference = document(state.as_ref())["archive"][3]["sha256"]
            .as_str()
            .unwrap()
            .to_owned();
        let file = archive_path.join(format!("{reference}.json"));
        let original_body = fs::read(&file).unwrap();
        if damage == "missing" {
            fs::remove_file(&file).unwrap();
        } else {
            fs::write(&file, b"runtime corruption").unwrap();
        }
        let original_root = raw(state.as_ref());
        let damaged_tree = bytes_tree(&archive_path);
        let writes = state.writes();
        let archive_writes = archive.writes.load(Ordering::SeqCst);
        let reads = archive.reads.load(Ordering::SeqCst);
        // 轻量元数据不暗示已重读正文；无新增来源仍保持安静，不触发观察或清历史。
        assert_eq!(admin.snapshot(&alice).unwrap().archived_jobs, 4);
        assert_eq!(
            admin
                .reserve(&source(&alice, 4), 5000, "observer-v1", &options())
                .unwrap(),
            None
        );
        assert_eq!(archive.reads.load(Ordering::SeqCst), reads);
        let memory = source(&alice, 5);
        let result = if use_reader {
            admin.reserve_reader(
                &alice,
                &SourceReader {
                    snapshot: memory,
                    missing: false,
                    tamper: false,
                },
                5000,
                "observer-v1",
                &options(),
            )
        } else {
            admin.reserve(&memory, 5000, "observer-v1", &options())
        };
        assert_eq!(
            result.unwrap_err(),
            if damage == "missing" {
                InterestError::Storage
            } else {
                InterestError::CorruptState
            },
            "damage={damage}, use_reader={use_reader}"
        );
        assert!(archive.reads.load(Ordering::SeqCst) >= reads + 4);
        assert_eq!(state.writes(), writes);
        assert_eq!(archive.writes.load(Ordering::SeqCst), archive_writes);
        assert_eq!(raw(state.as_ref()), original_root);
        assert_eq!(bytes_tree(&archive_path), damaged_tree);
        assert_eq!(
            admin.snapshot(&alice).unwrap_err(),
            InterestError::Unavailable
        );
        assert_eq!(
            admin
                .reserve(&source(&alice, 5), 6000, "observer-v1", &options())
                .unwrap_err(),
            InterestError::Unavailable
        );
        let _ = kernel.stop_all().await;
        // 仅恢复备份的真实原件，确认既有消费未清空，失败准入没有留下新任务。
        fs::write(&file, original_body).unwrap();
        let (kernel, admin) = open(state.clone(), Some(archive)).await;
        assert_eq!(all_jobs(&admin, &alice, 20), saved);
        assert_eq!(raw(state.as_ref()), original_root);
        assert_eq!(
            admin
                .reserve(&source(&alice, 4), 6000, "observer-v1", &options())
                .unwrap(),
            None
        );
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn migration_put_or_root_confirmation_failure_preserves_original_and_requires_restart() {
    for failure in ["put-before", "put-after", "root-before", "root-after"] {
        let directory = tempfile::tempdir().unwrap();
        let state = RecordingStore::open(&directory.path().join("state"));
        let (alice, saved) = legacy(state.clone(), 4).await;
        let before = raw(state.as_ref());
        let archive = FaultArchive::open(&directory.path().join("archive"));
        archive
            .fail_before
            .store(failure == "put-before", Ordering::SeqCst);
        archive
            .fail_after
            .store(failure == "put-after", Ordering::SeqCst);
        state.fail_before(failure == "root-before");
        state.fail_after(failure == "root-after");
        let (kernel, admin, started) = start(state.clone(), Some(archive.clone())).await;
        assert!(!started, "{failure}");
        assert_eq!(
            admin.snapshot(&alice).err(),
            Some(InterestError::Unavailable)
        );
        if failure != "root-after" {
            assert_eq!(raw(state.as_ref()), before, "{failure}");
        }
        let _ = kernel.stop_all().await;
        archive.fail_before.store(false, Ordering::SeqCst);
        archive.fail_after.store(false, Ordering::SeqCst);
        state.fail_before(false);
        state.fail_after(false);
        let (kernel, admin) = open(state.clone(), Some(archive)).await;
        assert_eq!(all_jobs(&admin, &alice, 20), saved, "{failure}");
        assert_eq!(
            admin
                .reserve(&source(&alice, 4), 9000, "observer-v1", &options())
                .unwrap(),
            None
        );
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn finish_put_or_root_failure_never_loses_running_or_terminal_record() {
    for failure in ["put-before", "put-after", "root-before", "root-after"] {
        let directory = tempfile::tempdir().unwrap();
        let state = RecordingStore::open(&directory.path().join("state"));
        let archive = FaultArchive::open(&directory.path().join("archive"));
        let (kernel, admin) = open(state.clone(), Some(archive.clone())).await;
        let alice = scope("s", "alice");
        let first = reserve(&admin, &source(&alice, 1), 100);
        admin
            .finish(&first, 101, ObservationOutcome::Completed(vec![]))
            .unwrap();
        let second = reserve(&admin, &source(&alice, 2), 200);
        let before = raw(state.as_ref());
        archive
            .fail_before
            .store(failure == "put-before", Ordering::SeqCst);
        archive
            .fail_after
            .store(failure == "put-after", Ordering::SeqCst);
        state.fail_before(failure == "root-before");
        state.fail_after(failure == "root-after");
        assert_eq!(
            admin
                .finish(
                    &second,
                    201,
                    ObservationOutcome::Failed(ObservationFailure::Cancelled)
                )
                .err(),
            Some(InterestError::Storage)
        );
        assert_eq!(
            admin.snapshot(&alice).err(),
            Some(InterestError::Unavailable)
        );
        if failure != "root-after" {
            assert_eq!(raw(state.as_ref()), before, "{failure}");
        }
        archive.fail_before.store(false, Ordering::SeqCst);
        archive.fail_after.store(false, Ordering::SeqCst);
        state.fail_before(false);
        state.fail_after(false);
        let _ = kernel.stop_all().await;
        let (kernel, admin) = open(state.clone(), Some(archive)).await;
        let jobs = all_jobs(&admin, &alice, 20);
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].batch, first);
        assert_eq!(jobs[0].status, JobStatus::Completed);
        assert_eq!(jobs[1].batch, second);
        assert_eq!(
            jobs[1].status,
            if failure == "root-after" {
                JobStatus::Failed(ObservationFailure::Cancelled)
            } else {
                JobStatus::Interrupted
            }
        );
        assert_eq!(
            admin
                .reserve(&source(&alice, 2), 300, "observer-v1", &options())
                .unwrap(),
            None
        );
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn missing_or_tampered_archive_and_corrupt_root_fail_without_clearing_records() {
    for damage in ["missing", "body", "root", "reference"] {
        let directory = tempfile::tempdir().unwrap();
        let state = RecordingStore::open(&directory.path().join("state"));
        let archive_path = directory.path().join("archive");
        let archive = Arc::new(FileInterestJobArchive::open(&archive_path).unwrap());
        let (alice, _) = legacy(state.clone(), 4).await;
        let (kernel, _) = open(state.clone(), Some(archive.clone())).await;
        kernel.stop_all().await.unwrap();
        if damage == "root" {
            // 损坏真实已提交root，而不是为正向路径伪造记录/hash。
            state
                .set(
                    &owner(),
                    INTEREST_STATE_KEY.into(),
                    b"{broken-root".to_vec(),
                )
                .unwrap();
        } else if damage == "reference" {
            // 正文仍为真实原件；根引用声称的来源摘要必须与原件完全一致。
            let mut value = document(state.as_ref());
            value["archive"][0]["consumed"][0]["sha256"] = Value::String("0".repeat(64));
            state
                .set(
                    &owner(),
                    INTEREST_STATE_KEY.into(),
                    serde_json::to_vec(&value).unwrap(),
                )
                .unwrap();
        } else {
            let file = fs::read_dir(&archive_path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| path.extension().is_some_and(|value| value == "json"))
                .unwrap();
            if damage == "missing" {
                fs::remove_file(file).unwrap();
            } else {
                fs::write(file, b"tampered immutable body").unwrap();
            }
        }
        let before = raw(state.as_ref());
        let tree = bytes_tree(&archive_path);
        let (kernel, admin, started) = start(state.clone(), Some(archive)).await;
        assert!(!started, "{damage}");
        assert_eq!(
            admin.snapshot(&alice).err(),
            Some(InterestError::Unavailable)
        );
        assert_eq!(raw(state.as_ref()), before, "{damage}");
        assert_eq!(bytes_tree(&archive_path), tree, "{damage}");
        let _ = kernel.stop_all().await;
    }
}

#[tokio::test]
async fn no_archive_keeps_v1_unchanged_and_rejects_existing_v2_without_writing() {
    let directory = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(&directory.path().join("state"));
    let (alice, saved) = legacy(state.clone(), 4).await;
    let before = raw(state.as_ref());
    let writes = state.writes();
    let (kernel, admin) = open(state.clone(), None).await;
    assert_eq!(admin.snapshot(&alice).unwrap().jobs, saved);
    assert!(!admin.archive_status().unwrap().enabled);
    assert_eq!(raw(state.as_ref()), before);
    assert_eq!(state.writes(), writes);
    kernel.stop_all().await.unwrap();
    let archive = Arc::new(FileInterestJobArchive::open(directory.path().join("archive")).unwrap());
    let (kernel, _) = open(state.clone(), Some(archive)).await;
    kernel.stop_all().await.unwrap();
    let before = raw(state.as_ref());
    let writes = state.writes();
    let (kernel, admin, started) = start(state.clone(), None).await;
    assert!(!started);
    assert_eq!(
        admin.snapshot(&alice).err(),
        Some(InterestError::Unavailable)
    );
    assert_eq!(raw(state.as_ref()), before);
    assert_eq!(state.writes(), writes);
    let _ = kernel.stop_all().await;
}

/// StateStore 没有删除接口：只在全部句柄释放后删除真实外层文件中的指定测试键。
/// 其余实际提交字节保持不变，再交由 FileStateStore 正常打开，模拟根键丢失。
fn remove_file_state_key(path: &Path, namespace: &str, key: &str) {
    let snapshot = path.join("state.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
    assert!(
        value["entries"][namespace]
            .as_object_mut()
            .unwrap()
            .remove(key)
            .is_some()
    );
    fs::write(snapshot, serde_json::to_vec(&value).unwrap()).unwrap();
}

#[tokio::test]
async fn missing_root_key_with_existing_file_archive_rejects_without_replacing_history() {
    let directory = tempfile::tempdir().unwrap();
    let state_path = directory.path().join("state");
    let archive_path = directory.path().join("archive");
    let archive = Arc::new(FileInterestJobArchive::open(&archive_path).unwrap());
    let sentinel = PluginId::new("test.unrelated").unwrap();
    let (alice, saved, committed_root) = {
        let state = RecordingStore::open(&state_path);
        let (alice, saved) = legacy(state.clone(), 4).await;
        state
            .set(&sentinel, "keep".into(), b"unrelated original".to_vec())
            .unwrap();
        let (kernel, admin) = open(state.clone(), Some(archive.clone())).await;
        assert_eq!(all_jobs(&admin, &alice, 20), saved);
        let committed_root = raw(state.as_ref());
        kernel.stop_all().await.unwrap();
        drop(kernel);
        drop(admin);
        drop(state);
        (alice, saved, committed_root)
    };
    assert!(!archive.is_empty().unwrap());
    remove_file_state_key(&state_path, INTEREST_PLUGIN_ID, INTEREST_STATE_KEY);
    let before_state = fs::read(state_path.join("state.json")).unwrap();
    let before_archive = bytes_tree(&archive_path);
    let state = RecordingStore::open(&state_path);
    assert!(state.get(&owner(), INTEREST_STATE_KEY).unwrap().is_none());
    let (kernel, admin, started) = start(state.clone(), Some(archive.clone())).await;
    assert!(!started, "存在历史归档时缺根必须拒绝启动");
    assert_eq!(
        admin.snapshot(&alice).err(),
        Some(InterestError::Unavailable)
    );
    assert!(state.get(&owner(), INTEREST_STATE_KEY).unwrap().is_none());
    assert_eq!(
        state.get(&sentinel, "keep").unwrap(),
        Some(b"unrelated original".to_vec())
    );
    assert_eq!(state.writes(), 0);
    assert_eq!(
        fs::read(state_path.join("state.json")).unwrap(),
        before_state
    );
    assert_eq!(bytes_tree(&archive_path), before_archive);
    let _ = kernel.stop_all().await;
    // 恢复的是先前真实根原件；失败启动没有破坏依旧可读取的全部历史。
    state
        .set(&owner(), INTEREST_STATE_KEY.into(), committed_root)
        .unwrap();
    let (kernel, admin) = open(state, Some(archive)).await;
    assert_eq!(all_jobs(&admin, &alice, 20), saved);
    assert_eq!(
        admin
            .reserve(&source(&alice, 4), 5000, "observer-v1", &options())
            .unwrap(),
        None
    );
    kernel.stop_all().await.unwrap();
}

/// 兼容替换存储只能读写原件，未实现空库证明时应沿用 trait 的明确失败默认值。
struct ArchiveWithoutEmptyProof(FileInterestJobArchive);
impl InterestJobArchiveStore for ArchiveWithoutEmptyProof {
    fn put(&self, hash: &str, bytes: &[u8]) -> InterestResult<()> {
        self.0.put(hash, bytes)
    }
    fn get(&self, hash: &str) -> InterestResult<Vec<u8>> {
        self.0.get(hash)
    }
}

#[tokio::test]
async fn archive_without_empty_proof_cannot_silently_initialize_a_missing_root() {
    let directory = tempfile::tempdir().unwrap();
    let state_path = directory.path().join("state");
    let archive_path = directory.path().join("archive");
    let state = RecordingStore::open(&state_path);
    let archive = Arc::new(ArchiveWithoutEmptyProof(
        FileInterestJobArchive::open(&archive_path).unwrap(),
    ));
    assert_eq!(archive.is_empty(), Err(InterestError::Unavailable));
    let before_state = bytes_tree(&state_path);
    let before_archive = bytes_tree(&archive_path);
    let (kernel, admin, started) = start(state.clone(), Some(archive)).await;
    assert!(!started);
    assert_eq!(
        admin.snapshot(&scope("s", "alice")).err(),
        Some(InterestError::Unavailable)
    );
    assert!(state.get(&owner(), INTEREST_STATE_KEY).unwrap().is_none());
    assert_eq!(state.writes(), 0);
    assert_eq!(bytes_tree(&state_path), before_state);
    assert_eq!(bytes_tree(&archive_path), before_archive);
    let _ = kernel.stop_all().await;
}

struct SourceReader {
    snapshot: MemorySnapshot,
    missing: bool,
    tamper: bool,
}
struct SourceCursor {
    summary: MemoryScanSummary,
    evidence: Vec<InteractionEvidence>,
    offset: usize,
}
impl MemoryEvidenceCursor for SourceCursor {
    fn summary(&self) -> &MemoryScanSummary {
        &self.summary
    }
    fn next_page(&mut self) -> MemoryResult<Vec<InteractionEvidence>> {
        let end = (self.offset + 3).min(self.evidence.len());
        let page = self.evidence[self.offset..end].to_vec();
        self.offset = end;
        Ok(page)
    }
}
impl MemoryService for SourceReader {
    fn snapshot(&self) -> MemoryResult<MemorySnapshot> {
        panic!("只允许分页来源读取")
    }
    fn scan(&self) -> MemoryResult<Box<dyn MemoryEvidenceCursor>> {
        let mut evidence = self.snapshot.evidence.clone();
        if self.missing {
            // 原 e8 身份丢失、变成未知来源；保留合法的 9 条/修订 9 与唯一交互计数。
            // 单纯删除一条会先违反 Memory 的 revision==交互数，无法测试消费集合缺项。
            let last_consumed = evidence.len() - 2;
            evidence[last_consumed].id = "replacement-source-e-8".into();
        }
        if self.tamper {
            // 最后一条旧来源位于第三页，不能只核对前页便准入末尾新来源。
            let last_consumed = evidence.len() - 2;
            evidence[last_consumed].at_ms += 1;
        }
        Ok(Box::new(SourceCursor {
            summary: MemoryScanSummary {
                scope: self.snapshot.scope.clone(),
                revision: self.snapshot.revision,
                evidence_count: evidence.len() as u64,
                preferences: vec![],
            },
            evidence,
            offset: 0,
        }))
    }
}

#[tokio::test]
async fn archived_consumed_sources_are_checked_across_all_memory_pages_before_new_admission() {
    let directory = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(&directory.path().join("state"));
    let (alice, saved) = legacy(state.clone(), 8).await;
    let archive = Arc::new(FileInterestJobArchive::open(directory.path().join("archive")).unwrap());
    let (kernel, admin) = open(state.clone(), Some(archive)).await;
    let before = raw(state.as_ref());
    for (missing, tamper) in [(true, false), (false, true)] {
        let reader = SourceReader {
            snapshot: source(&alice, 9),
            missing,
            tamper,
        };
        // 两种损伤都须先通过 Memory 全分页契约，再由 Interest 的旧消费原件识别冲突。
        let mut cursor = ValidatedMemoryCursor::new(alice.clone(), reader.scan().unwrap()).unwrap();
        let mut evidence = Vec::new();
        loop {
            let page = cursor
                .next_page()
                .expect("测试来源必须满足 Memory 游标契约");
            if page.is_empty() {
                break;
            }
            evidence.extend(page);
        }
        assert_eq!(evidence.len(), 9);
        if missing {
            assert!(evidence.iter().all(|item| item.id != "e-8"));
        } else {
            assert_eq!(evidence[7].id, "e-8");
            assert_ne!(evidence[7].at_ms, saved[7].batch.evidence[0].at_ms);
        }
        assert_eq!(
            admin
                .reserve_reader(&alice, &reader, 5000, "observer-v1", &options())
                .unwrap_err(),
            InterestError::Conflict,
            "missing={missing}, tamper={tamper}"
        );
        assert_eq!(raw(state.as_ref()), before);
    }
    let reader = SourceReader {
        snapshot: source(&alice, 9),
        missing: false,
        tamper: false,
    };
    let batch = admin
        .reserve_reader(&alice, &reader, 5000, "observer-v1", &options())
        .unwrap()
        .unwrap();
    assert_eq!(batch.evidence, vec![reader.snapshot.evidence[8].clone()]);
    let jobs = all_jobs(&admin, &alice, 20);
    assert_eq!(&jobs[..8], saved.as_slice());
    kernel.stop_all().await.unwrap();
}

struct CountOnlyReader {
    summary: MemoryScanSummary,
    page_reads: Arc<AtomicUsize>,
}
struct CountOnlyCursor {
    summary: MemoryScanSummary,
    page_reads: Arc<AtomicUsize>,
}
impl MemoryEvidenceCursor for CountOnlyCursor {
    fn summary(&self) -> &MemoryScanSummary {
        &self.summary
    }
    fn next_page(&mut self) -> MemoryResult<Vec<InteractionEvidence>> {
        self.page_reads.fetch_add(1, Ordering::SeqCst);
        // 边界内确实走到来源读取；超过上限时此方法不应被调用或分配任何证据。
        Err(MemoryError::Storage)
    }
}
impl MemoryService for CountOnlyReader {
    fn snapshot(&self) -> MemoryResult<MemorySnapshot> {
        panic!("数量上限不能加载完整快照")
    }
    fn scan(&self) -> MemoryResult<Box<dyn MemoryEvidenceCursor>> {
        Ok(Box::new(CountOnlyCursor {
            summary: self.summary.clone(),
            page_reads: self.page_reads.clone(),
        }))
    }
}

#[tokio::test]
async fn source_count_limit_rejects_before_pages_or_cold_reads_and_preserves_root() {
    let directory = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(&directory.path().join("state"));
    let (alice, saved) = legacy(state.clone(), 4).await;
    let archive = FaultArchive::open(&directory.path().join("archive"));
    let (kernel, admin) = open(state.clone(), Some(archive.clone())).await;
    let before = raw(state.as_ref());
    let writes = state.writes();
    let archive_writes = archive.writes.load(Ordering::SeqCst);
    for count in [262_145, u64::MAX, 262_144] {
        let page_reads = Arc::new(AtomicUsize::new(0));
        let reader = CountOnlyReader {
            summary: MemoryScanSummary {
                scope: alice.clone(),
                revision: count,
                preferences: vec![],
                evidence_count: count,
            },
            page_reads: page_reads.clone(),
        };
        let reads = archive.reads.load(Ordering::SeqCst);
        let error = admin
            .reserve_reader(&alice, &reader, 5000, "observer-v1", &options())
            .unwrap_err();
        if count > 262_144 {
            assert_eq!(error, InterestError::LimitReached);
            assert_eq!(page_reads.load(Ordering::SeqCst), 0);
            assert_eq!(archive.reads.load(Ordering::SeqCst), reads);
        } else {
            assert_eq!(error, InterestError::Memory(MemoryError::Storage));
            assert_eq!(page_reads.load(Ordering::SeqCst), 1);
            assert_eq!(
                archive.reads.load(Ordering::SeqCst),
                reads,
                "来源页读取失败尚未形成新准入，不扫描冷正文"
            );
        }
        assert_eq!(raw(state.as_ref()), before);
        assert_eq!(state.writes(), writes);
        assert_eq!(archive.writes.load(Ordering::SeqCst), archive_writes);
    }
    assert_eq!(all_jobs(&admin, &alice, 20), saved);
    assert_eq!(
        admin
            .reserve(&source(&alice, 4), 5000, "observer-v1", &options())
            .unwrap(),
        None
    );
    kernel.stop_all().await.unwrap();
}

/// 仅通过真实 Session、Memory、Interest API 生成 QQ 进程迁移的旧根种子。
/// 此数据均为合成文本；环境变量只控制额外导出，不跳过任何验证或手改业务 JSON。
#[tokio::test]
async fn export_real_completed_qq_seed_for_process_migration() {
    let directory = tempfile::tempdir().unwrap();
    let state_path = directory.path().join("state");
    let state = RecordingStore::open(&state_path);
    let backends = KernelServices {
        state: state.clone(),
        ..KernelServices::default()
    };
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    let memory_plugin = MemoryPlugin::new().unwrap();
    let memory_admin = memory_plugin.controller();
    let interest_plugin = InterestPlugin::new().unwrap();
    let interest_admin = interest_plugin.controller();
    kernel
        .register(Box::new(SessionPlugin::new().unwrap()))
        .unwrap();
    kernel.register(Box::new(memory_plugin)).unwrap();
    kernel.register(Box::new(interest_plugin)).unwrap();
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
    // 与 QQ 假网关 app1904159860 / c2c / user-1 的实际映射一致，由进程测试共同约定。
    let identity = "qq:6109b591485cea14b617bd5db7258a134b97eb816a42cbba42e625b45ae55800";
    let who = scope(identity, identity);
    let key = SessionKey::new(identity, identity).unwrap();
    for index in 1..=128_u64 {
        let user = format!("这是用于迁移验证的第 {index} 条历史消息。");
        let reply = format!("第 {index} 条历史消息已经收到。");
        let begun = sessions
            .begin(SessionInput {
                key: key.clone(),
                text: user.clone(),
            })
            .unwrap();
        assert_eq!(begun.lease.turn_id, index);
        sessions
            .complete(
                &begun.lease,
                vec![
                    ChatMessage::text(ChatRole::User, &user),
                    ChatMessage::text(ChatRole::Assistant, &reply),
                ],
            )
            .unwrap();
        let snapshot = sessions.snapshot(&key).unwrap().unwrap();
        assert!(matches!(
            snapshot.turns.last().unwrap().status,
            SessionTurnStatus::Completed { .. }
        ));
        let imported = memory_admin
            .import_completed(
                &who,
                index - 1,
                CompletedInteraction {
                    evidence_id: format!("seed-e-{index}"),
                    message_id: format!("seed-m-{index}"),
                    at_ms: 1000 + index,
                    snapshot,
                    turn_id: index,
                },
            )
            .unwrap();
        assert_eq!(imported.revision, index);
        let batch = reserve(&interest_admin, &imported, 2000 + index);
        assert_eq!(batch.evidence.len(), 1);
        assert_eq!(batch.evidence[0].id, format!("seed-e-{index}"));
        if index == 128 {
            continue;
        }
        let outcome = match index {
            2 => ObservationOutcome::Failed(ObservationFailure::Provider),
            3 => ObservationOutcome::Failed(ObservationFailure::Cancelled),
            _ => ObservationOutcome::Completed(vec![]),
        };
        interest_admin
            .finish(&batch, 3000 + index, outcome)
            .unwrap();
    }
    assert_eq!(sessions.snapshot(&key).unwrap().unwrap().turns.len(), 128);
    assert_eq!(
        memory_admin
            .reader(who.clone())
            .unwrap()
            .snapshot()
            .unwrap()
            .evidence
            .len(),
        128
    );
    kernel.stop_all().await.unwrap();
    // 真实重启把最后一份已消费但未结束的批次记为中断，不能为了 fixture 手造状态。
    let (kernel, admin) = open(state.clone(), None).await;
    let saved = admin.snapshot(&who).unwrap();
    assert!(saved.interests.is_empty());
    assert_eq!(saved.jobs.len(), 128);
    assert_eq!(
        saved.jobs[1].status,
        JobStatus::Failed(ObservationFailure::Provider)
    );
    assert_eq!(
        saved.jobs[2].status,
        JobStatus::Failed(ObservationFailure::Cancelled)
    );
    assert_eq!(saved.jobs[127].status, JobStatus::Interrupted);
    let root = document(state.as_ref());
    assert_eq!(root["format_version"], 1);
    assert!(
        root["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|record| record.get("sequence").is_none())
    );
    kernel.stop_all().await.unwrap();
    if let Some(destination) = std::env::var_os("EVE_INTEREST_ARCHIVE_SEED_EXPORT") {
        let destination = PathBuf::from(destination);
        fs::create_dir_all(&destination).unwrap();
        let manifest=serde_json::to_vec_pretty(&serde_json::json!({
            "synthetic":true,"generator":"actual SessionPlugin + MemoryPlugin + InterestPlugin APIs",
            "scope":who,"completed_session_turns":128,"memory_evidence":128,"interest_jobs":128,
            "interest_format_version":1,"statuses":{"Completed":125,"Provider":1,"Cancelled":1,"Interrupted":1}
        })).unwrap();
        for (name, bytes) in [
            (
                "state.json",
                fs::read(state_path.join("state.json")).unwrap(),
            ),
            ("manifest.json", manifest),
        ] {
            let output = destination.join(name);
            if output.exists() {
                assert_eq!(fs::read(&output).unwrap(), bytes, "不能覆盖不同的验证材料");
            } else {
                fs::write(output, bytes).unwrap();
            }
        }
    }
}
