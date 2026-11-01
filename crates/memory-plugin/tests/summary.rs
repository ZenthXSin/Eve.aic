use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_memory_api::*;
use eve_memory_plugin::{
    ConversationSummaryPlugin, SUMMARY_STATE_KEY, SummaryController, effective_points,
    effective_points_reader, validate_summary_points,
};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicUsize, Ordering},
};

struct Store {
    inner: FileStateStore,
    failure: AtomicU8,
    writes: AtomicUsize,
}
impl Store {
    fn open(path: &std::path::Path) -> Arc<Self> {
        Arc::new(Self {
            inner: FileStateStore::open(path).unwrap(),
            failure: AtomicU8::new(0),
            writes: AtomicUsize::new(0),
        })
    }
}
impl StateStore for Store {
    fn get(&self, owner: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.inner.get(owner, key)
    }
    fn set(&self, owner: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        let mode = if key == SUMMARY_STATE_KEY {
            self.failure.load(Ordering::SeqCst)
        } else {
            0
        };
        if mode == 1 {
            return Err(PluginError::State("private backend detail".into()));
        }
        self.inner.set(owner, key, value)?;
        if mode == 2 {
            return Err(PluginError::State("unknown commit result".into()));
        }
        Ok(())
    }
}
fn owner() -> PluginId {
    PluginId::new(SUMMARY_PLUGIN_ID).unwrap()
}
async fn open(store: Arc<Store>) -> (Kernel, SummaryController) {
    let kernel = Kernel::with_services(KernelServices {
        state: store,
        ..KernelServices::default()
    });
    let plugin = ConversationSummaryPlugin::new().unwrap();
    let controller = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start(&owner()).await.unwrap();
    (kernel, controller)
}
fn snapshot(n: u64) -> MemorySnapshot {
    MemorySnapshot {
        scope: MemoryScope {
            channel: "qq".into(),
            session_id: "s".into(),
            user_id: "u".into(),
        },
        revision: n,
        preferences: vec![],
        evidence: (1..=n)
            .map(|i| InteractionEvidence {
                id: format!("e-{i}"),
                revision: i,
                at_ms: i,
                source: EvidenceSource::CompletedInteraction {
                    message_id: format!("m-{i}"),
                    turn_id: i,
                    session_revision: i * 2,
                    user_text: format!("第{i}次交流：我喜欢 Mindustry，但不知道怎么创作模组"),
                    assistant_text: "ASSISTANT_CANNOT_BECOME_USER_EVIDENCE".into(),
                },
            })
            .collect(),
        archived_evidence: 0,
    }
}
fn options() -> SummaryOptions {
    SummaryOptions {
        min_new_evidence: 1,
        max_new_evidence: 1,
        cooldown_ms: 0,
    }
}

#[test]
fn distinct_excerpts_from_one_message_are_valid_but_identical_quotes_are_not() {
    let batch = SummaryBatch {
        sequence: 1,
        scope: snapshot(1).scope,
        source_revision: 1,
        started_at_ms: 1,
        extractor_version: "v1".into(),
        consumed_ids: vec!["e-1".into()],
        previous: vec![],
        sources: vec![SummarySource {
            evidence_id: "e-1".into(),
            revision: 1,
            at_ms: 1,
            user_text: "我喜欢 Mindustry，但不知道怎么创作模组".into(),
        }],
    };
    let mut value = SummaryPoint {
        text: "用户想了解模组创作".into(),
        quotes: vec![
            SummaryQuote {
                evidence_id: "e-1".into(),
                text: "我喜欢 Mindustry".into(),
            },
            SummaryQuote {
                evidence_id: "e-1".into(),
                text: "不知道怎么创作模组".into(),
            },
        ],
    };
    assert!(validate_summary_points(&batch, &[value.clone()]).is_ok());
    value.quotes[1] = value.quotes[0].clone();
    assert_eq!(
        validate_summary_points(&batch, &[value]),
        Err(MemoryError::InvalidInput)
    );
}
fn point(id: &str) -> SummaryPoint {
    SummaryPoint {
        text: "用户对 Mindustry 模组创作感兴趣，需要入门帮助".into(),
        quotes: vec![SummaryQuote {
            evidence_id: id.into(),
            text: "我喜欢 Mindustry，但不知道怎么创作模组".into(),
        }],
    }
}

#[tokio::test]
async fn reservation_is_persisted_before_work_and_next_batch_merges_only_current_user_sources() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path());
    let (kernel, admin) = open(store.clone()).await;
    let source = snapshot(1);
    let original = source.clone();
    let batch = admin
        .reserve(&source, 100, "test-v1", options())
        .unwrap()
        .unwrap();
    let saved = store.get(&owner(), SUMMARY_STATE_KEY).unwrap().unwrap();
    assert!(String::from_utf8(saved).unwrap().contains("Running"));
    assert!(
        !serde_json::to_string(&batch)
            .unwrap()
            .contains("ASSISTANT_CANNOT")
    );
    admin.finish(&batch, 101, Ok(vec![point("e-1")])).unwrap();
    assert!(
        admin
            .reserve(&source, 102, "test-v1", options())
            .unwrap()
            .is_none()
    );
    let next = admin
        .reserve(&snapshot(2), 103, "test-v1", options())
        .unwrap()
        .unwrap();
    assert_eq!(next.consumed_ids, vec!["e-2"]);
    assert_eq!(next.previous, vec![point("e-1")]);
    assert_eq!(next.sources.len(), 2);
    let mut other = source.scope.clone();
    other.user_id = "other".into();
    assert!(admin.jobs(&other).unwrap().is_empty());
    assert_eq!(source, original);
    kernel.stop_all().await.unwrap();
    assert_eq!(admin.jobs(&source.scope), Err(MemoryError::Unavailable));
}

#[tokio::test]
async fn restart_preserves_completed_summary_and_interrupts_without_replaying_consumed_inputs() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path());
    let (kernel, admin) = open(store.clone()).await;
    let first = admin
        .reserve(&snapshot(1), 10, "v1", options())
        .unwrap()
        .unwrap();
    admin.finish(&first, 11, Ok(vec![point("e-1")])).unwrap();
    admin
        .reserve(&snapshot(2), 12, "v1", options())
        .unwrap()
        .unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, restored) = open(store.clone()).await;
    let jobs = restored.jobs(&snapshot(2).scope).unwrap();
    assert_eq!(jobs[1].status, SummaryStatus::Interrupted);
    assert_eq!(
        effective_points(&jobs[0], &snapshot(2)).unwrap(),
        vec![point("e-1")]
    );
    // 不完整热快照不能证明归档来源；必须读取真实固定来源流。
    let mut archived = snapshot(2);
    archived.evidence.remove(0);
    archived.archived_evidence = 1;
    assert_eq!(
        effective_points(&jobs[0], &archived),
        Err(MemoryError::Conflict)
    );
    assert_eq!(
        effective_points_reader(&jobs[0], &reader(2))
            .unwrap()
            .points,
        vec![point("e-1")]
    );
    let mut rewritten = snapshot(2);
    rewritten.archived_evidence = 1;
    rewritten.evidence[0].at_ms += 1;
    assert_eq!(
        effective_points(&jobs[0], &rewritten),
        Err(MemoryError::Conflict)
    );
    let writes = store.writes.load(Ordering::SeqCst);
    assert!(
        restored
            .reserve(&snapshot(2), 100, "v1", options())
            .unwrap()
            .is_none()
    );
    assert_eq!(writes, store.writes.load(Ordering::SeqCst));
    // 下一批次继承旧观点：归档来源由完整流核对，不采用旧副本猜测。
    let mut third = snapshot(3);
    third.evidence.remove(0);
    third.archived_evidence = 1;
    assert_eq!(
        restored.reserve(&third, 101, "v1", options()),
        Err(MemoryError::Conflict)
    );
    let next = restored
        .reserve_reader(&third.scope, &reader(3), 101, "v1", options())
        .unwrap()
        .unwrap();
    assert_eq!(next.previous, vec![point("e-1")]);
    assert_eq!(next.sources[0].evidence_id, "e-1");
    restored
        .finish(&next, 102, Err(SummaryFailure::Provider))
        .unwrap();
    assert!(
        restored
            .reserve_reader(&third.scope, &reader(3), 103, "v1", options())
            .unwrap()
            .is_none()
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn unknown_commit_closes_handles_and_reopen_keeps_the_actual_admission() {
    for failure in [1, 2] {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path());
        let (kernel, admin) = open(store.clone()).await;
        store.failure.store(failure, Ordering::SeqCst);
        assert_eq!(
            admin.reserve(&snapshot(1), 10, "v1", options()),
            Err(MemoryError::Storage)
        );
        assert_eq!(
            admin.jobs(&snapshot(1).scope),
            Err(MemoryError::Unavailable)
        );
        kernel.stop_all().await.unwrap();
        store.failure.store(0, Ordering::SeqCst);
        let (kernel, restored) = open(store).await;
        let jobs = restored.jobs(&snapshot(1).scope).unwrap();
        assert_eq!(jobs.len(), usize::from(failure == 2));
        if failure == 2 {
            assert_eq!(jobs[0].status, SummaryStatus::Interrupted);
        }
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn forged_quotes_are_rejected_and_revoked_sources_cannot_reappear_as_summary_preferences() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path());
    let (kernel, admin) = open(store).await;
    let source = snapshot(1);
    let batch = admin
        .reserve(&source, 10, "v1", options())
        .unwrap()
        .unwrap();
    for bad in [
        point("other-user-source"),
        SummaryPoint {
            text: "伪造".into(),
            quotes: vec![SummaryQuote {
                evidence_id: "e-1".into(),
                text: "用户已经成功发布模组".into(),
            }],
        },
    ] {
        assert_eq!(
            admin.finish(&batch, 11, Ok(vec![bad])),
            Err(MemoryError::InvalidInput)
        );
    }
    admin.finish(&batch, 11, Ok(vec![point("e-1")])).unwrap();
    let job = &admin.jobs(&source.scope).unwrap()[0];
    let mut revoked = source;
    revoked.preferences.push(Preference {
        id: "p".into(),
        text: "旧偏好".into(),
        status: PreferenceStatus::Revoked,
        revision: 1,
        history: vec![PreferenceVersion {
            revision: 1,
            evidence_id: "e-1".into(),
            at_ms: 11,
            text: "旧偏好".into(),
            status: PreferenceStatus::Revoked,
        }],
    });
    assert!(effective_points(job, &revoked).unwrap().is_empty());
    revoked.scope.user_id = "different".into();
    assert_eq!(
        effective_points(job, &revoked),
        Err(MemoryError::StaleRevision)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn oversized_batches_keep_whole_user_messages_and_full_ledger_preserves_prior_records() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path());
    let (kernel, admin) = open(store.clone()).await;
    let mut source = snapshot(4);
    for e in &mut source.evidence {
        if let EvidenceSource::CompletedInteraction { user_text, .. } = &mut e.source {
            *user_text = "中".repeat(10_000);
        }
    }
    let batch = admin
        .reserve(&source, 10, "v1", SummaryOptions::default())
        .unwrap()
        .unwrap();
    assert!(batch.sources.len() < 4);
    assert!(
        batch
            .sources
            .iter()
            .all(|s| s.user_text.chars().count() == 10_000)
    );
    kernel.stop_all().await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path());
    let (kernel, admin) = open(store.clone()).await;
    for i in 1..=MAX_SUMMARY_JOBS as u64 {
        let batch = admin
            .reserve(&snapshot(i), i, "v1", options())
            .unwrap()
            .unwrap();
        admin.finish(&batch, i, Ok(vec![])).unwrap();
    }
    let bytes = store.get(&owner(), SUMMARY_STATE_KEY).unwrap();
    assert_eq!(
        admin.reserve(&snapshot(129), 129, "v1", options()),
        Err(MemoryError::LimitReached)
    );
    assert_eq!(store.get(&owner(), SUMMARY_STATE_KEY).unwrap(), bytes);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn corrupt_unknown_and_duplicate_key_documents_fail_without_clearing_bytes() {
    for bytes in [
        b"broken".to_vec(),
        br#"{"format_version":2,"jobs":[]}"#.to_vec(),
        br#"{"format_version":1,"format_version":1,"jobs":[]}"#.to_vec(),
    ] {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path());
        store
            .set(&owner(), SUMMARY_STATE_KEY.into(), bytes.clone())
            .unwrap();
        let kernel = Kernel::with_services(KernelServices {
            state: store.clone(),
            ..KernelServices::default()
        });
        kernel
            .register(Box::new(ConversationSummaryPlugin::new().unwrap()))
            .unwrap();
        assert!(kernel.start(&owner()).await.is_err());
        assert_eq!(store.get(&owner(), SUMMARY_STATE_KEY).unwrap(), Some(bytes));
        kernel.stop_all().await.unwrap();
    }
}

/// 大历史逐页生成；若消费者退回全量接口，测试立即失败。
struct PagedReader {
    summary: MemoryScanSummary,
    fixed: Option<Vec<InteractionEvidence>>,
    fail_after: Option<usize>,
    reads: Arc<AtomicUsize>,
}
struct PagedCursor {
    summary: MemoryScanSummary,
    fixed: Option<Vec<InteractionEvidence>>,
    fail_after: Option<usize>,
    next: usize,
    reads: Arc<AtomicUsize>,
}
fn generated(number: u64) -> InteractionEvidence {
    InteractionEvidence {
        id: format!("e-{number}"),
        revision: number,
        at_ms: number,
        source: EvidenceSource::CompletedInteraction {
            message_id: format!("m-{number}"),
            turn_id: number,
            session_revision: number * 2,
            user_text: format!("第{number}次交流：我喜欢 Mindustry，但不知道怎么创作模组"),
            assistant_text: "ASSISTANT_CANNOT_BECOME_USER_EVIDENCE".into(),
        },
    }
}
impl MemoryEvidenceCursor for PagedCursor {
    fn summary(&self) -> &MemoryScanSummary {
        &self.summary
    }
    fn next_page(&mut self) -> MemoryResult<Vec<InteractionEvidence>> {
        if self.fail_after.is_some_and(|after| self.next >= after) {
            return Err(MemoryError::Storage);
        }
        let end = (self.next + MAX_ARCHIVE_PAGE).min(self.summary.evidence_count as usize);
        let page = (self.next..end)
            .map(|index| {
                self.reads.fetch_add(1, Ordering::SeqCst);
                self.fixed.as_ref().map_or_else(
                    || generated(index as u64 + 1),
                    |records| records[index].clone(),
                )
            })
            .collect();
        self.next = end;
        Ok(page)
    }
}
impl MemoryService for PagedReader {
    fn snapshot(&self) -> MemoryResult<MemorySnapshot> {
        panic!("summary must not snapshot")
    }
    fn complete_snapshot(&self) -> MemoryResult<MemorySnapshot> {
        panic!("summary must not restore full memory")
    }
    fn scan(&self) -> MemoryResult<Box<dyn MemoryEvidenceCursor>> {
        Ok(Box::new(PagedCursor {
            summary: self.summary.clone(),
            fixed: self.fixed.clone(),
            fail_after: self.fail_after,
            next: 0,
            reads: self.reads.clone(),
        }))
    }
}
fn reader(count: u64) -> PagedReader {
    PagedReader {
        summary: MemoryScanSummary {
            scope: snapshot(0).scope,
            revision: count,
            evidence_count: count,
            preferences: vec![],
        },
        fixed: None,
        fail_after: None,
        reads: Arc::new(AtomicUsize::new(0)),
    }
}

#[tokio::test]
async fn paged_summary_scans_six_thousand_sources_and_commits_only_the_bounded_batch() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path());
    let (kernel, admin) = open(store).await;
    let source = reader(6000);
    let options = SummaryOptions {
        max_new_evidence: 8,
        ..options()
    };
    let batch = admin
        .reserve_reader(&source.summary.scope, &source, 10, "v1", options)
        .unwrap()
        .unwrap();
    assert_eq!(source.reads.load(Ordering::SeqCst), 6000);
    assert_eq!(batch.sources.len(), 8);
    assert_eq!(batch.consumed_ids.len(), 8);
    assert_eq!(batch.source_revision, 6000);
    assert_eq!(
        batch.sources[0].user_text,
        "第1次交流：我喜欢 Mindustry，但不知道怎么创作模组"
    );
    assert_eq!(batch.sources[7].evidence_id, "e-8");
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn paged_summary_never_commits_partial_input_or_hides_a_late_page_error() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path());
    let (kernel, admin) = open(store.clone()).await;
    let mut source = reader(MAX_ARCHIVE_PAGE as u64 * 2 + 1);
    source.fail_after = Some(MAX_ARCHIVE_PAGE);
    let writes = store.writes.load(Ordering::SeqCst);
    assert_eq!(
        admin.reserve_reader(&source.summary.scope, &source, 10, "v1", options()),
        Err(MemoryError::Storage)
    );
    assert_eq!(store.writes.load(Ordering::SeqCst), writes);
    assert!(admin.jobs(&source.summary.scope).unwrap().is_empty());
    source.fail_after = None;
    let batch = admin
        .reserve_reader(&source.summary.scope, &source, 10, "v1", options())
        .unwrap()
        .unwrap();
    let writes = store.writes.load(Ordering::SeqCst);
    source.fail_after = Some(MAX_ARCHIVE_PAGE);
    assert_eq!(
        admin.reserve_reader(&source.summary.scope, &source, 11, "v1", options()),
        Err(MemoryError::Storage),
        "even Running must not silently accept a broken view"
    );
    assert_eq!(store.writes.load(Ordering::SeqCst), writes);
    admin.finish(&batch, 12, Ok(vec![point("e-1")])).unwrap();
    let writes = store.writes.load(Ordering::SeqCst);
    assert_eq!(
        admin.reserve_reader(
            &source.summary.scope,
            &source,
            13,
            "v1",
            SummaryOptions {
                cooldown_ms: 1000,
                ..options()
            }
        ),
        Err(MemoryError::Storage)
    );
    assert_eq!(store.writes.load(Ordering::SeqCst), writes);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn paged_and_snapshot_summaries_match_and_reject_changed_previous_quotes() {
    let left_root = tempfile::tempdir().unwrap();
    let right_root = tempfile::tempdir().unwrap();
    let (left, legacy) = open(Store::open(left_root.path())).await;
    let state = Store::open(right_root.path());
    let (right, paged) = open(state.clone()).await;
    for now in [10, 20] {
        let source = snapshot(if now == 10 { 1 } else { 2 });
        let paged_source = reader(source.revision);
        let a = legacy
            .reserve(&source, now, "v1", options())
            .unwrap()
            .unwrap();
        let b = paged
            .reserve_reader(&source.scope, &paged_source, now, "v1", options())
            .unwrap()
            .unwrap();
        assert_eq!(a, b);
        legacy.finish(&a, now + 1, Ok(vec![point("e-1")])).unwrap();
        paged.finish(&b, now + 1, Ok(vec![point("e-1")])).unwrap();
    }
    let mut source = reader(3);
    let mut altered = snapshot(3).evidence;
    if let EvidenceSource::CompletedInteraction { user_text, .. } = &mut altered[0].source {
        *user_text = "来源已被改变".into();
    }
    source.fixed = Some(altered);
    let writes = state.writes.load(Ordering::SeqCst);
    assert_eq!(
        paged.reserve_reader(&source.summary.scope, &source, 30, "v1", options()),
        Err(MemoryError::Conflict)
    );
    assert_eq!(state.writes.load(Ordering::SeqCst), writes);
    left.stop_all().await.unwrap();
    right.stop_all().await.unwrap();
}

#[tokio::test]
async fn paged_summary_respects_revocation_without_reintroducing_the_old_point() {
    let root = tempfile::tempdir().unwrap();
    let (kernel, admin) = open(Store::open(root.path())).await;
    let initial = reader(1);
    let batch = admin
        .reserve_reader(&initial.summary.scope, &initial, 10, "v1", options())
        .unwrap()
        .unwrap();
    admin.finish(&batch, 11, Ok(vec![point("e-1")])).unwrap();
    let mut source = reader(2);
    source.summary.revision = 4;
    let mut evidence = snapshot(2).evidence;
    evidence[1].revision = 3;
    source.fixed = Some(evidence);
    source.summary.preferences = vec![Preference {
        id: "p".into(),
        text: "旧偏好".into(),
        status: PreferenceStatus::Revoked,
        revision: 2,
        history: vec![
            PreferenceVersion {
                revision: 1,
                evidence_id: "e-1".into(),
                at_ms: 2,
                text: "旧偏好".into(),
                status: PreferenceStatus::Confirmed,
            },
            PreferenceVersion {
                revision: 2,
                evidence_id: "e-2".into(),
                at_ms: 4,
                text: "旧偏好".into(),
                status: PreferenceStatus::Revoked,
            },
        ],
    }];
    let batch = admin
        .reserve_reader(&source.summary.scope, &source, 20, "v1", options())
        .unwrap()
        .unwrap();
    assert!(batch.previous.is_empty());
    assert_eq!(batch.sources.len(), 1);
    assert_eq!(batch.sources[0].evidence_id, "e-2");
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn effective_context_scans_exact_archived_sources_and_refuses_missing_changed_or_late_failed_data()
 {
    let root = tempfile::tempdir().unwrap();
    let (kernel, admin) = open(Store::open(root.path())).await;
    let batch = admin
        .reserve(&snapshot(1), 10, "v1", options())
        .unwrap()
        .unwrap();
    admin.finish(&batch, 11, Ok(vec![point("e-1")])).unwrap();
    let job = admin.latest_completed(&batch.scope).unwrap().unwrap();
    let source = reader(6000);
    let effective = effective_points_reader(&job, &source).unwrap();
    assert_eq!(effective.source_revision, 6000);
    assert_eq!(effective.points, vec![point("e-1")]);
    assert_eq!(
        source.reads.load(Ordering::SeqCst),
        6000,
        "来源命中后仍验证末页，且不读取全快照"
    );
    let mut source = reader(65);
    source.fail_after = Some(64);
    assert_eq!(
        effective_points_reader(&job, &source).err(),
        Some(MemoryError::Storage)
    );
    let mut source = reader(2);
    let mut values = snapshot(2).evidence;
    values[0].at_ms += 1;
    source.fixed = Some(values);
    assert_eq!(
        effective_points_reader(&job, &source).err(),
        Some(MemoryError::Conflict)
    );
    let mut source = reader(1);
    let mut missing = snapshot(1).evidence;
    missing[0].id = "different-evidence".into();
    source.fixed = Some(missing);
    assert_eq!(
        effective_points_reader(&job, &source).err(),
        Some(MemoryError::Conflict),
        "有别的归档条目不证明本条存在"
    );
    let mut source = reader(1);
    source.summary.scope.user_id = "other".into();
    assert_eq!(
        effective_points_reader(&job, &source).err(),
        Some(MemoryError::CorruptState)
    );
    kernel.stop_all().await.unwrap();
    assert_eq!(
        admin.latest_completed(&batch.scope).err(),
        Some(MemoryError::Unavailable)
    );
}

#[tokio::test]
async fn real_archive_sources_remain_verifiable_after_hot_eviction_and_restart() {
    use eve_memory_plugin::{ArchivePolicy, FileEvidenceArchive, MemoryPlugin};
    use eve_plugin_api::ServiceId;
    use eve_session_api::{SESSION_SERVICE_ID, SessionKey, SessionServiceHandle};
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path());
    let archive = Arc::new(FileEvidenceArchive::open(root.path().join("evidence")).unwrap());
    let policy = ArchivePolicy {
        trigger_evidence: 4,
        pressure_evidence: 8,
        keep_recent: 1,
        pressure_keep_recent: 1,
        min_age_ms: 0,
        min_chunk: 2,
        max_chunk_evidence: 2,
        ..ArchivePolicy::default()
    };
    let scope = snapshot(0).scope;
    let key = SessionKey::new(&scope.session_id, &scope.user_id).unwrap();
    let open_all = || {
        let backends = KernelServices {
            state: store.clone(),
            ..KernelServices::default()
        };
        let registry = backends.registry.clone();
        let kernel = Kernel::with_services(backends);
        let memory = MemoryPlugin::new()
            .unwrap()
            .with_archive(archive.clone(), policy.clone())
            .unwrap();
        let admin = memory.controller();
        let summary = ConversationSummaryPlugin::new().unwrap();
        let summaries = summary.controller();
        kernel
            .register(Box::new(eve_session_plugin::SessionPlugin::new().unwrap()))
            .unwrap();
        kernel.register(Box::new(memory)).unwrap();
        kernel.register(Box::new(summary)).unwrap();
        (kernel, registry, admin, summaries)
    };
    let (kernel, registry, memory, summaries) = open_all();
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
    for number in 1..=8 {
        let text = format!("第{number}次交流：我喜欢 Mindustry，但不知道怎么创作模组");
        let started = sessions
            .begin(eve_session_api::SessionInput {
                key: key.clone(),
                text: text.clone(),
            })
            .unwrap();
        sessions
            .complete(
                &started.lease,
                vec![
                    eve_llm_api::ChatMessage::text(eve_llm_api::ChatRole::User, &text),
                    eve_llm_api::ChatMessage::text(
                        eve_llm_api::ChatRole::Assistant,
                        "已送达的助手内容",
                    ),
                ],
            )
            .unwrap();
        let saved = sessions.snapshot(&key).unwrap().unwrap();
        memory
            .import_completed(
                &scope,
                number - 1,
                CompletedInteraction {
                    evidence_id: format!("e-{number}"),
                    message_id: format!("m-{number}"),
                    at_ms: number,
                    snapshot: saved,
                    turn_id: number,
                },
            )
            .unwrap();
    }
    let reader = memory.reader(scope.clone()).unwrap();
    let hot = reader.snapshot().unwrap();
    assert!(hot.archived_evidence > 0);
    assert!(!hot.evidence.iter().any(|e| e.id == "e-1"));
    let batch = summaries
        .reserve_reader(&scope, reader.as_ref(), 100, "v1", options())
        .unwrap()
        .unwrap();
    assert_eq!(batch.consumed_ids, vec!["e-1"]);
    summaries
        .finish(&batch, 101, Ok(vec![point("e-1")]))
        .unwrap();
    let latest = summaries.latest_completed(&scope).unwrap().unwrap();
    assert_eq!(effective_points(&latest, &hot), Err(MemoryError::Conflict));
    assert_eq!(
        effective_points_reader(&latest, reader.as_ref())
            .unwrap()
            .points,
        vec![point("e-1")]
    );
    let before = store.writes.load(Ordering::SeqCst);
    assert!(!archive.list().unwrap().is_empty());
    drop(reader);
    drop(sessions);
    kernel.stop_all().await.unwrap();
    drop(kernel);
    drop(memory);
    drop(summaries);
    drop(registry);
    let (kernel, _, memory, summaries) = open_all();
    kernel.start_all().await.unwrap();
    let latest = summaries.latest_completed(&scope).unwrap().unwrap();
    let reader = memory.reader(scope.clone()).unwrap();
    assert_eq!(
        effective_points_reader(&latest, reader.as_ref())
            .unwrap()
            .points,
        vec![point("e-1")]
    );
    assert_eq!(
        store.writes.load(Ordering::SeqCst),
        before,
        "只读重启与查询不改写来源和摘要账本"
    );
    kernel.stop_all().await.unwrap();
}

async fn cold_open(
    store: Arc<Store>,
    archive: Arc<dyn SummaryArchiveStore>,
) -> PluginResult<(Kernel, SummaryController)> {
    let kernel = Kernel::with_services(KernelServices {
        state: store,
        ..KernelServices::default()
    });
    let plugin = ConversationSummaryPlugin::new()?.with_archive(archive);
    let admin = plugin.controller();
    kernel.register(Box::new(plugin))?;
    kernel.start(&owner()).await?;
    Ok((kernel, admin))
}
fn saved_summary(store: &Store) -> serde_json::Value {
    serde_json::from_slice(&store.get(&owner(), SUMMARY_STATE_KEY).unwrap().unwrap()).unwrap()
}
#[tokio::test]
async fn cold_history_crosses_global_128_jobs_keeps_scopes_sequences_and_consumption_after_restart()
{
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("state"));
    let archive =
        Arc::new(eve_memory_plugin::FileSummaryArchive::open(root.path().join("archive")).unwrap());
    let (kernel, admin) = cold_open(store.clone(), archive.clone()).await.unwrap();
    let alice = snapshot(1);
    let batch = admin.reserve(&alice, 1, "v1", options()).unwrap().unwrap();
    admin.finish(&batch, 1, Ok(vec![point("e-1")])).unwrap();
    for i in 1..=260 {
        let mut source = snapshot(i);
        source.scope.user_id = "bob".into();
        let batch = admin
            .reserve(&source, i + 1, "v1", options())
            .unwrap()
            .unwrap();
        assert_eq!(batch.sequence, i + 1);
        admin.finish(&batch, i + 1, Ok(vec![])).unwrap();
        let saved = saved_summary(&store);
        assert!(saved["jobs"].as_array().unwrap().len() <= MAX_SUMMARY_JOBS);
    }
    let saved = saved_summary(&store);
    assert_eq!(saved["format_version"], 2);
    assert!(saved["tail"]["last"].as_u64().unwrap() >= 128);
    assert_eq!(admin.jobs(&alice.scope).unwrap().len(), 1);
    assert_eq!(
        admin
            .latest_completed(&alice.scope)
            .unwrap()
            .unwrap()
            .points,
        vec![point("e-1")]
    );
    kernel.stop_all().await.unwrap();
    let (kernel, admin) = cold_open(store.clone(), archive).await.unwrap();
    assert_eq!(
        admin
            .latest_completed(&alice.scope)
            .unwrap()
            .unwrap()
            .batch
            .sequence,
        1
    );
    let mut bob = snapshot(260);
    bob.scope.user_id = "bob".into();
    assert_eq!(admin.jobs(&bob.scope).unwrap().len(), 260);
    assert!(
        admin
            .reserve(&bob, 1000, "v1", options())
            .unwrap()
            .is_none()
    );
    bob = snapshot(261);
    bob.scope.user_id = "bob".into();
    assert_eq!(
        admin
            .reserve(&bob, 1001, "v1", options())
            .unwrap()
            .unwrap()
            .sequence,
        262
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn failed_summary_repairs_are_finite_persistent_and_never_replace_newer_same_revision_context()
 {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("state"));
    let archive =
        Arc::new(eve_memory_plugin::FileSummaryArchive::open(root.path().join("archive")).unwrap());
    let (kernel, admin) = cold_open(store.clone(), archive.clone()).await.unwrap();
    let source = reader(4);
    let scope = source.summary.scope.clone();
    let first = admin
        .reserve_reader(&scope, &source, 10, "v1", options())
        .unwrap()
        .unwrap();
    admin
        .finish(&first, 11, Err(SummaryFailure::InvalidOutput))
        .unwrap();
    let second = admin
        .reserve_reader(&scope, &source, 12, "v1", options())
        .unwrap()
        .unwrap();
    admin.finish(&second, 13, Ok(vec![point("e-2")])).unwrap();
    let repair = admin
        .reserve_repair_reader(&scope, &source, 70_000, "v2", options())
        .unwrap()
        .unwrap();
    assert_eq!(repair.consumed_ids, first.consumed_ids);
    assert_eq!(repair.source_revision, first.source_revision);
    assert!(repair.previous.is_empty());
    admin
        .finish(&repair, 70_001, Ok(vec![point("e-1")]))
        .unwrap();
    assert_eq!(
        admin
            .latest_completed(&scope)
            .unwrap()
            .unwrap()
            .batch
            .sequence,
        second.sequence
    );
    let third = admin
        .reserve_reader(&scope, &source, 70_002, "v1", options())
        .unwrap()
        .unwrap();
    admin
        .finish(&third, 70_003, Err(SummaryFailure::Timeout))
        .unwrap();
    for (attempt, at) in [(1, 140_000), (2, 270_000)] {
        let repair = admin
            .reserve_repair_reader(&scope, &source, at, "v2", options())
            .unwrap()
            .unwrap();
        let job = admin.jobs(&scope).unwrap().pop().unwrap();
        assert_eq!(job.repair.unwrap().attempt, attempt);
        admin
            .finish(&repair, at + 1, Err(SummaryFailure::Timeout))
            .unwrap();
    }
    kernel.stop_all().await.unwrap();
    let (kernel, admin) = cold_open(store, archive).await.unwrap();
    assert!(
        admin
            .reserve_repair_reader(&scope, &source, 1_000_000, "v3", options())
            .unwrap()
            .is_none()
    );
    assert_eq!(
        admin
            .jobs(&scope)
            .unwrap()
            .iter()
            .filter(|j| j.batch.sequence == first.sequence
                && j.status == SummaryStatus::Failed(SummaryFailure::InvalidOutput))
            .count(),
        1
    );
    assert_eq!(
        admin
            .latest_completed(&scope)
            .unwrap()
            .unwrap()
            .batch
            .sequence,
        second.sequence
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn legacy_summary_migration_and_uncertain_root_commit_keep_both_recoverable_histories() {
    for failure in [1, 2] {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("state"));
        let (kernel, admin) = open(store.clone()).await;
        for i in 1..=100 {
            let batch = admin
                .reserve(&snapshot(i), i, "v1", options())
                .unwrap()
                .unwrap();
            admin.finish(&batch, i, Ok(vec![])).unwrap();
        }
        kernel.stop_all().await.unwrap();
        let before = store.get(&owner(), SUMMARY_STATE_KEY).unwrap();
        let archive = Arc::new(
            eve_memory_plugin::FileSummaryArchive::open(root.path().join("archive")).unwrap(),
        );
        store.failure.store(failure, Ordering::SeqCst);
        assert!(cold_open(store.clone(), archive.clone()).await.is_err());
        if failure == 1 {
            assert_eq!(store.get(&owner(), SUMMARY_STATE_KEY).unwrap(), before);
        }
        store.failure.store(0, Ordering::SeqCst);
        let (kernel, admin) = cold_open(store.clone(), archive).await.unwrap();
        assert_eq!(admin.jobs(&snapshot(0).scope).unwrap().len(), 100);
        assert_eq!(saved_summary(&store)["format_version"], 2);
        assert!(
            admin
                .reserve(&snapshot(100), 101, "v1", options())
                .unwrap()
                .is_none()
        );
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn late_cold_corruption_is_checked_before_recovering_any_running_summary() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("state"));
    let directory = root.path().join("archive");
    let archive = Arc::new(eve_memory_plugin::FileSummaryArchive::open(&directory).unwrap());
    let (kernel, admin) = cold_open(store.clone(), archive.clone()).await.unwrap();
    for i in 1..=170 {
        let batch = admin
            .reserve(&snapshot(i), i, "v1", options())
            .unwrap()
            .unwrap();
        admin.finish(&batch, i, Ok(vec![])).unwrap();
    }
    admin
        .reserve(&snapshot(171), 171, "v1", options())
        .unwrap()
        .unwrap();
    kernel.stop_all().await.unwrap();
    let before = store.get(&owner(), SUMMARY_STATE_KEY).unwrap();
    let tail = saved_summary(&store)["tail"]["hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let latest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.join(format!("{tail}.json"))).unwrap())
            .unwrap();
    let oldest = latest["previous"]["hash"].as_str().unwrap();
    let path = directory.join(format!("{oldest}.json"));
    std::fs::write(&path, b"damaged").unwrap();
    assert!(cold_open(store.clone(), archive).await.is_err());
    assert_eq!(store.get(&owner(), SUMMARY_STATE_KEY).unwrap(), before);
    assert_eq!(std::fs::read(path).unwrap(), b"damaged");
}

#[tokio::test]
async fn missing_initialized_summary_root_is_not_a_fresh_empty_ledger() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("state"));
    let archive =
        Arc::new(eve_memory_plugin::FileSummaryArchive::open(root.path().join("archive")).unwrap());
    let (kernel, _) = cold_open(store.clone(), archive.clone()).await.unwrap();
    kernel.stop_all().await.unwrap();
    // 模拟根键消失，其他持久状态完整保留。
    let kernel = Kernel::with_services(KernelServices {
        state: Arc::new(HiddenSummary(store.clone())),
        ..KernelServices::default()
    });
    kernel
        .register(Box::new(
            ConversationSummaryPlugin::new()
                .unwrap()
                .with_archive(archive),
        ))
        .unwrap();
    let before = store.get(&owner(), SUMMARY_STATE_KEY).unwrap();
    assert!(kernel.start_all().await.is_err());
    assert_eq!(store.get(&owner(), SUMMARY_STATE_KEY).unwrap(), before);
}
struct HiddenSummary(Arc<Store>);
impl StateStore for HiddenSummary {
    fn get(&self, owner: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        if key == SUMMARY_STATE_KEY {
            Ok(None)
        } else {
            self.0.get(owner, key)
        }
    }
    fn set(&self, owner: &PluginId, key: String, bytes: Vec<u8>) -> PluginResult<()> {
        self.0.set(owner, key, bytes)
    }
}

#[tokio::test]
async fn summary_repairs_revalidate_all_sources_and_skip_revocation_changed_or_late_failed_scans() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path());
    let (kernel, admin) = open(store.clone()).await;
    let source = reader(1);
    let scope = source.summary.scope.clone();
    let batch = admin
        .reserve_reader(&scope, &source, 10, "v1", options())
        .unwrap()
        .unwrap();
    admin
        .finish(&batch, 11, Err(SummaryFailure::InvalidOutput))
        .unwrap();
    let before = store.get(&owner(), SUMMARY_STATE_KEY).unwrap();
    let mut changed = reader(1);
    let mut record = generated(1);
    if let EvidenceSource::CompletedInteraction { user_text, .. } = &mut record.source {
        *user_text = "内容已被替换".into();
    }
    changed.fixed = Some(vec![record]);
    assert!(
        admin
            .reserve_repair_reader(&scope, &changed, 70_000, "v2", options())
            .unwrap()
            .is_none()
    );
    assert_eq!(store.get(&owner(), SUMMARY_STATE_KEY).unwrap(), before);
    let mut revoked = reader(2);
    revoked.summary.revision = 4;
    let mut evidence = snapshot(2).evidence;
    evidence[1].revision = 3;
    revoked.fixed = Some(evidence);
    revoked.summary.preferences = vec![Preference {
        id: "p".into(),
        text: "旧偏好".into(),
        status: PreferenceStatus::Revoked,
        revision: 2,
        history: vec![
            PreferenceVersion {
                revision: 1,
                evidence_id: "e-1".into(),
                at_ms: 2,
                text: "旧偏好".into(),
                status: PreferenceStatus::Confirmed,
            },
            PreferenceVersion {
                revision: 2,
                evidence_id: "e-2".into(),
                at_ms: 4,
                text: "旧偏好".into(),
                status: PreferenceStatus::Revoked,
            },
        ],
    }];
    assert!(
        admin
            .reserve_repair_reader(&scope, &revoked, 100_000, "v2", options())
            .unwrap()
            .is_none()
    );
    assert_eq!(store.get(&owner(), SUMMARY_STATE_KEY).unwrap(), before);
    // 换一个更晚的固定读取时间，验证读流出错仍不能发布部分补做。
    let mut failing = reader(1);
    failing.fail_after = Some(0);
    assert!(
        admin
            .reserve_repair_reader(&scope, &failing, 140_000, "v2", options())
            .is_err()
    );
    assert_eq!(store.get(&owner(), SUMMARY_STATE_KEY).unwrap(), before);
    kernel.stop_all().await.unwrap();
    let (kernel, admin) = open(store.clone()).await;
    // 取消和重启中断的批次不是可自动补做的已知失败。
    let repair = admin
        .reserve_repair_reader(&scope, &source, 200_000, "v2", options())
        .unwrap()
        .unwrap();
    admin
        .finish(&repair, 200_001, Err(SummaryFailure::Cancelled))
        .unwrap();
    assert!(
        admin
            .reserve_repair_reader(&scope, &source, 1_000_000, "v2", options())
            .unwrap()
            .is_none()
    );
    kernel.stop_all().await.unwrap();
}

struct SummaryReply(String);
impl eve_llm_api::LlmProvider for SummaryReply {
    fn complete(
        &self,
        _: eve_llm_api::ModelRequest,
    ) -> eve_llm_api::LlmFuture<'_, eve_llm_api::ModelResponse> {
        Box::pin(async move {
            Ok(eve_llm_api::ModelResponse::Final {
                text: self.0.clone(),
            })
        })
    }
}
struct SummaryResolver(String);
impl eve_llm_api::LlmModelResolver for SummaryResolver {
    fn resolve(&self) -> Result<eve_llm_api::ModelSelection, eve_llm_api::LlmError> {
        Ok(eve_llm_api::ModelSelection {
            provider: Arc::new(SummaryReply(self.0.clone())),
            provider_timeout: std::time::Duration::from_secs(1),
        })
    }
}
#[tokio::test]
async fn model_failure_categories_reject_duplicate_keys_and_never_store_model_output_as_a_reason() {
    let root = tempfile::tempdir().unwrap();
    let (kernel, admin) = open(Store::open(root.path())).await;
    let batch = admin
        .reserve(&snapshot(1), 1, "conversation-summary-v1", options())
        .unwrap()
        .unwrap();
    for (text,expected) in [
        ("SECRET_BAD_RESPONSE".to_string(), SummaryFailure::InvalidOutputJson),
        ("{\"points\":[],\"points\":[]}".into(), SummaryFailure::InvalidOutputJson),
        ("{\"extra\":\"SECRET_BAD_RESPONSE\",\"points\":[]}".into(), SummaryFailure::InvalidOutputShape),
        ("x".repeat(MAX_SUMMARY_OUTPUT_BYTES + 1), SummaryFailure::InvalidOutputTooLarge),
        (serde_json::json!({"points":[{"text":"不存在的来源","quotes":[{"evidence_id":"foreign","text":"SECRET_BAD_RESPONSE"}]}]}).to_string(), SummaryFailure::InvalidOutputSource),
    ] {
        let model = eve_memory_plugin::ModelConversationSummarizer::new(Arc::new(SummaryResolver(text)));
        let failure = model.summarize(batch.clone()).await.unwrap_err();
        assert_eq!(failure,expected);
        assert!(!format!("{failure:?}").contains("SECRET_BAD_RESPONSE"));
    }
    kernel.stop_all().await.unwrap();
}

struct LimitedSummaryArchive {
    inner: eve_memory_plugin::FileSummaryArchive,
    limited: std::sync::atomic::AtomicBool,
}
impl SummaryArchiveStore for LimitedSummaryArchive {
    fn get(&self, hash: &str) -> MemoryResult<Vec<u8>> {
        self.inner.get(hash)
    }
    fn put(&self, hash: &str, bytes: &[u8]) -> MemoryResult<()> {
        if self.limited.load(Ordering::SeqCst) {
            return Err(MemoryError::LimitReached);
        }
        self.inner.put(hash, bytes)
    }
}
#[tokio::test]
async fn cold_capacity_failure_is_rechecked_without_reopening_or_resetting_consumption() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("state"));
    let archive = Arc::new(LimitedSummaryArchive {
        inner: eve_memory_plugin::FileSummaryArchive::open(root.path().join("archive")).unwrap(),
        limited: std::sync::atomic::AtomicBool::new(false),
    });
    let (kernel, admin) = cold_open(store.clone(), archive.clone()).await.unwrap();
    for i in 1..=96 {
        let batch = admin
            .reserve(&snapshot(i), i, "v1", options())
            .unwrap()
            .unwrap();
        admin.finish(&batch, i, Ok(vec![])).unwrap();
    }
    let before = store.get(&owner(), SUMMARY_STATE_KEY).unwrap();
    archive.limited.store(true, Ordering::SeqCst);
    assert!(matches!(
        admin.reserve(&snapshot(97), 97, "v1", options()),
        Err(MemoryError::LimitReached)
    ));
    assert_eq!(store.get(&owner(), SUMMARY_STATE_KEY).unwrap(), before);
    assert_eq!(admin.jobs(&snapshot(0).scope).unwrap().len(), 96);
    archive.limited.store(false, Ordering::SeqCst);
    let batch = admin
        .reserve(&snapshot(97), 98, "v1", options())
        .unwrap()
        .unwrap();
    assert_eq!(batch.sequence, 97);
    assert_eq!(batch.consumed_ids, vec!["e-97"]);
    admin.finish(&batch, 99, Ok(vec![])).unwrap();
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn a_running_prefix_cannot_be_evicted_but_finishing_it_reopens_global_capacity() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("state"));
    let archive =
        Arc::new(eve_memory_plugin::FileSummaryArchive::open(root.path().join("archive")).unwrap());
    let (kernel, admin) = cold_open(store.clone(), archive).await.unwrap();
    let mut first = None;
    for i in 1..=MAX_SUMMARY_JOBS {
        let mut input = snapshot(1);
        input.scope.user_id = format!("user-{i}");
        let batch = admin
            .reserve(&input, i as u64, "v1", options())
            .unwrap()
            .unwrap();
        if first.is_none() {
            first = Some(batch);
        }
    }
    let mut extra = snapshot(1);
    extra.scope.user_id = "overflow".into();
    let before = store.get(&owner(), SUMMARY_STATE_KEY).unwrap();
    assert!(matches!(
        admin.reserve(&extra, 1000, "v1", options()),
        Err(MemoryError::LimitReached)
    ));
    assert_eq!(store.get(&owner(), SUMMARY_STATE_KEY).unwrap(), before);
    admin.finish(&first.unwrap(), 1001, Ok(vec![])).unwrap();
    let admitted = admin
        .reserve(&extra, 1002, "v1", options())
        .unwrap()
        .unwrap();
    assert_eq!(admitted.sequence, 129);
    assert_eq!(saved_summary(&store)["tail"]["last"], 1);
    assert_eq!(saved_summary(&store)["jobs"].as_array().unwrap().len(), 128);
    kernel.stop_all().await.unwrap();
}
