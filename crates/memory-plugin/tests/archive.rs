mod support;

use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{ChatMessage, ChatRole};
use eve_memory_api::*;
use eve_memory_plugin::{
    ArchivePolicy, FileEvidenceArchive, LexicalMemoryRecall, MEMORY_STATE_KEY, MemoryController,
    MemoryPlugin,
};
use eve_plugin_api::{ServiceId, StateStore};
use eve_session_api::{
    SESSION_SERVICE_ID, SessionKey, SessionService, SessionServiceHandle, SessionSnapshot,
    SessionTurn, SessionTurnStatus,
};
use eve_session_plugin::SessionPlugin;
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use support::{RecordingStore, complete_session};

fn scope(session: &str) -> MemoryScope {
    MemoryScope {
        channel: "qq".into(),
        session_id: session.into(),
        user_id: "alice".into(),
    }
}

fn key(scope: &MemoryScope) -> SessionKey {
    SessionKey::new(&scope.session_id, &scope.user_id).unwrap()
}

fn small_policy() -> ArchivePolicy {
    ArchivePolicy {
        trigger_evidence: 6,
        pressure_evidence: 20,
        keep_recent: 2,
        min_age_ms: 0,
        min_chunk: 2,
        max_chunk_evidence: 4,
        max_chunk_bytes: 64 * 1024,
        ..ArchivePolicy::default()
    }
}

/// 可注入写入失败的归档存储；读取与列出仍走真实文件实现。
struct FlakyArchive {
    inner: FileEvidenceArchive,
    fail_put: AtomicBool,
}

impl EvidenceArchiveStore for FlakyArchive {
    fn put(&self, name: &str, bytes: &[u8]) -> MemoryResult<()> {
        if self.fail_put.load(Ordering::SeqCst) {
            return Err(MemoryError::Storage);
        }
        self.inner.put(name, bytes)
    }
    fn get(&self, name: &str) -> MemoryResult<Option<Vec<u8>>> {
        self.inner.get(name)
    }
    fn list(&self) -> MemoryResult<Vec<String>> {
        self.inner.list()
    }
}

fn flaky(directory: &Path) -> Arc<FlakyArchive> {
    Arc::new(FlakyArchive {
        inner: FileEvidenceArchive::open(directory).unwrap(),
        fail_put: AtomicBool::new(false),
    })
}

struct Rig {
    kernel: Kernel,
    admin: MemoryController,
    sessions: Arc<dyn SessionService>,
}

async fn start(
    state: Option<Arc<dyn StateStore>>,
    archive: Option<(Arc<dyn EvidenceArchiveStore>, ArchivePolicy)>,
) -> Result<Rig, String> {
    let mut backends = KernelServices::default();
    if let Some(state) = state {
        backends.state = state;
    }
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    let mut plugin = MemoryPlugin::new().unwrap();
    if let Some((store, policy)) = archive {
        plugin = plugin.with_archive(store, policy).unwrap();
    }
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel
        .register(Box::new(SessionPlugin::new().unwrap()))
        .unwrap();
    kernel
        .start_all()
        .await
        .map_err(|error| error.to_string())?;
    let sessions = registry
        .get(&ServiceId::new(SESSION_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<SessionServiceHandle>()
        .unwrap()
        .0
        .clone();
    Ok(Rig {
        kernel,
        admin,
        sessions,
    })
}

fn texts(scope: &MemoryScope, number: usize) -> (String, String) {
    (
        format!(
            "{} 第 {number} 次提问：原文 {}",
            scope.session_id,
            "保留".repeat(number % 7)
        ),
        format!("{} 第 {number} 次送达的回复。", scope.session_id),
    )
}

fn prepare(rig: &Rig, scope: &MemoryScope, number: usize) -> CompletedInteraction {
    let (user, reply) = texts(scope, number);
    let snapshot = complete_session(rig.sessions.as_ref(), &key(scope), &user, &reply);
    let turn_id = snapshot.turns.last().unwrap().id;
    CompletedInteraction {
        evidence_id: format!("{}-e-{number}", scope.session_id),
        message_id: format!("{}-m-{number}", scope.session_id),
        at_ms: 1000 + number as u64,
        snapshot,
        turn_id,
    }
}

fn commit(
    rig: &Rig,
    scope: &MemoryScope,
    interaction: CompletedInteraction,
) -> MemoryResult<MemorySnapshot> {
    let expected = rig.admin.reader(scope.clone())?.snapshot()?.revision;
    rig.admin.import_completed(scope, expected, interaction)
}

fn import(rig: &Rig, scope: &MemoryScope, number: usize) -> MemoryResult<MemorySnapshot> {
    let interaction = prepare(rig, scope, number);
    commit(rig, scope, interaction)
}

/// 停止并释放内核，使文件状态目录锁随之释放。
async fn stop(rig: Rig) {
    rig.kernel.stop_all().await.unwrap();
}

/// 读出作用域的全部归档证据，核对分页顺序。
fn all_archived(admin: &MemoryController, scope: &MemoryScope) -> Vec<InteractionEvidence> {
    let reader = admin.reader(scope.clone()).unwrap();
    let mut after = 0;
    let mut all = Vec::new();
    loop {
        let page = reader.archived(after, 5).unwrap();
        assert!(page.evidence.len() <= 5);
        for item in &page.evidence {
            assert!(item.revision > after);
            after = item.revision;
        }
        all.extend(page.evidence);
        match page.next_after {
            Some(next) => after = next,
            None => break,
        }
    }
    all
}

fn completed_texts(evidence: &InteractionEvidence) -> (String, String) {
    match &evidence.source {
        EvidenceSource::CompletedInteraction {
            user_text,
            assistant_text,
            ..
        } => (user_text.clone(), assistant_text.clone()),
        EvidenceSource::UserStatement { .. } => panic!("偏好命令不应归档"),
    }
}

/// 热层与归档合起来正好是导入的全部来源，修订不重不漏。
fn assert_complete(admin: &MemoryController, scope: &MemoryScope, count: usize) {
    let snapshot = admin.reader(scope.clone()).unwrap().snapshot().unwrap();
    let archived = all_archived(admin, scope);
    assert_eq!(archived.len() as u64, snapshot.archived_evidence);
    let mut all: Vec<_> = archived.iter().chain(snapshot.evidence.iter()).collect();
    all.sort_by_key(|item| item.revision);
    assert_eq!(all.len(), count);
    for (index, item) in all.iter().enumerate() {
        assert_eq!(item.revision, index as u64 + 1);
        assert_eq!(item.id, format!("{}-e-{}", scope.session_id, index + 1));
        assert_eq!(completed_texts(item), texts(scope, index + 1));
    }
}

#[tokio::test]
async fn crossing_the_old_hot_capacity_archives_instead_of_refusing_and_keeps_every_source() {
    let directory = tempfile::tempdir().unwrap();
    let archive = Arc::new(FileEvidenceArchive::open(directory.path()).unwrap());
    let rig = start(None, Some((archive, ArchivePolicy::default())))
        .await
        .unwrap();
    // 21 × 200 = 4200，超过旧的全局 4096 条上限。会话快照在本地构造，
    // 只验证记忆层；会话持久化与送达边界由 state.rs 与 QQ 进程测试覆盖。
    let scopes: Vec<_> = (0..21).map(|i| scope(&format!("s{i:02}"))).collect();
    let mut sessions: Vec<SessionSnapshot> = scopes
        .iter()
        .map(|scope| SessionSnapshot {
            key: key(scope),
            revision: 0,
            turns: vec![],
        })
        .collect();
    for number in 1..=200 {
        for (scope, session) in scopes.iter().zip(sessions.iter_mut()) {
            let (user, reply) = texts(scope, number);
            session.turns.push(SessionTurn {
                id: number as u64,
                input: user.clone(),
                status: SessionTurnStatus::Completed {
                    messages: vec![
                        ChatMessage::text(ChatRole::User, &user),
                        ChatMessage::text(ChatRole::Assistant, &reply),
                    ],
                },
            });
            session.revision += 2;
            commit(
                &rig,
                scope,
                CompletedInteraction {
                    evidence_id: format!("{}-e-{number}", scope.session_id),
                    message_id: format!("{}-m-{number}", scope.session_id),
                    at_ms: 1000 + number as u64,
                    snapshot: session.clone(),
                    turn_id: number as u64,
                },
            )
            .unwrap();
        }
    }
    let status = rig.admin.archive_status().unwrap();
    assert!(status.enabled);
    assert!(status.hot_evidence <= 1024 + 16, "{status:?}");
    assert_eq!(status.hot_evidence as u64 + status.archived_evidence, 4200);
    assert!(status.chunks > 0 && status.last_error.is_none());
    assert_eq!(status.unreferenced_chunks, 0);
    for scope in [&scopes[0], &scopes[20]] {
        assert_complete(&rig.admin, scope, 200);
    }
    // 召回只读热层，但修订与归档计数一致，不会把归档当作损坏。
    let recall = LexicalMemoryRecall::new(Arc::new(rig.admin.clone()))
        .reader(scopes[0].clone())
        .unwrap();
    recall
        .recall(&MemoryRecallRequest {
            query: "送达".into(),
            limit: 3,
        })
        .unwrap();
    stop(rig).await;
}

#[tokio::test]
async fn restart_keeps_archive_and_missing_or_tampered_chunks_are_never_skipped() {
    let state_dir = tempfile::tempdir().unwrap();
    let archive_dir = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(state_dir.path());
    let owner = scope("restart");
    let (before, chunks) = {
        let archive = flaky(archive_dir.path());
        let rig = start(Some(state.clone()), Some((archive, small_policy())))
            .await
            .unwrap();
        for number in 1..=12 {
            import(&rig, &owner, number).unwrap();
        }
        let before = rig.admin.reader(owner.clone()).unwrap().snapshot().unwrap();
        assert!(before.archived_evidence >= 4);
        assert_complete(&rig.admin, &owner, 12);
        stop(rig).await;
        let chunks = FileEvidenceArchive::open(archive_dir.path())
            .unwrap()
            .list()
            .unwrap();
        (before, chunks)
    };
    drop(state);
    // 新分块都经无损压缩，文档写格式 3；旧读取器遇到它按未知版本拒绝。
    let raw = support::raw_state(state_dir.path(), MEMORY_PLUGIN_ID, MEMORY_STATE_KEY);
    assert!(raw.contains("\"format_version\":3"), "{raw}");
    assert!(raw.contains("\"compressed\":true"), "{raw}");
    assert!(
        chunks.iter().all(|name| name.ends_with(".json.zz")),
        "{chunks:?}"
    );

    let state = RecordingStore::open(state_dir.path());
    let rig = start(
        Some(state.clone()),
        Some((flaky(archive_dir.path()), small_policy())),
    )
    .await
    .unwrap();
    assert!(rig.admin.reader(owner.clone()).unwrap().snapshot().unwrap() == before);
    assert_complete(&rig.admin, &owner, 12);
    stop(rig).await;
    drop(state);

    // 篡改分块：可以启动（只核对存在），读到时逐字节校验失败且停止新建分块。
    let tampered = archive_dir.path().join(&chunks[0]);
    let original = std::fs::read(&tampered).unwrap();
    let mut changed = original.clone();
    let last = changed.len() - 3;
    changed[last] ^= 1;
    std::fs::write(&tampered, &changed).unwrap();
    let state = RecordingStore::open(state_dir.path());
    let rig = start(
        Some(state.clone()),
        Some((flaky(archive_dir.path()), small_policy())),
    )
    .await
    .unwrap();
    let reader = rig.admin.reader(owner.clone()).unwrap();
    assert!(matches!(
        reader.archived(0, 64),
        Err(MemoryError::CorruptState)
    ));
    let status = rig.admin.archive_status().unwrap();
    assert!(status.last_error.is_some());
    let chunks_before = status.chunks;
    for number in 13..=20 {
        import(&rig, &owner, number).unwrap();
    }
    assert_eq!(rig.admin.archive_status().unwrap().chunks, chunks_before);
    stop(rig).await;
    drop(state);
    std::fs::write(&tampered, &original).unwrap();

    // 缺块：拒绝启动，不把缺失的历史当作不存在，也不改写状态。
    let moved = archive_dir.path().join("moved-away");
    std::fs::rename(&tampered, &moved).unwrap();
    let state = RecordingStore::open(state_dir.path());
    let writes = state.writes();
    let error = start(
        Some(state.clone()),
        Some((flaky(archive_dir.path()), small_policy())),
    )
    .await
    .err()
    .unwrap();
    assert!(error.contains("损坏"), "{error}");
    assert_eq!(state.writes(), writes);
    drop(state);
    std::fs::rename(&moved, &tampered).unwrap();

    // 已有归档却未装配存储：拒绝启动并说明原因。
    let state = RecordingStore::open(state_dir.path());
    let error = start(Some(state.clone()), None).await.err().unwrap();
    assert!(error.contains("未装配归档存储"), "{error}");
    drop(state);

    let state = RecordingStore::open(state_dir.path());
    let rig = start(
        Some(state),
        Some((flaky(archive_dir.path()), small_policy())),
    )
    .await
    .unwrap();
    assert_complete(&rig.admin, &owner, 20);
    stop(rig).await;
}

#[tokio::test]
async fn uncertain_commits_leave_at_most_an_unreferenced_chunk_and_never_lose_evidence() {
    let state_dir = tempfile::tempdir().unwrap();
    let archive_dir = tempfile::tempdir().unwrap();
    let owner = scope("commit");
    let state = RecordingStore::open(state_dir.path());
    let rig = start(
        Some(state.clone()),
        Some((flaky(archive_dir.path()), small_policy())),
    )
    .await
    .unwrap();
    for number in 1..=6 {
        import(&rig, &owner, number).unwrap();
    }
    assert_eq!(rig.admin.archive_status().unwrap().chunks, 0);
    // 第 7 条触发归档：分块已写，但热层提交在落盘前失败。
    let seventh = prepare(&rig, &owner, 7);
    state.fail_writes(true);
    assert!(matches!(
        commit(&rig, &owner, seventh.clone()),
        Err(MemoryError::Storage)
    ));
    assert!(matches!(
        rig.admin.reader(owner.clone()),
        Err(MemoryError::Unavailable)
    ));
    stop(rig).await;
    drop(state);
    assert_eq!(
        FileEvidenceArchive::open(archive_dir.path())
            .unwrap()
            .list()
            .unwrap()
            .len(),
        1
    );

    let state = RecordingStore::open(state_dir.path());
    let rig = start(
        Some(state.clone()),
        Some((flaky(archive_dir.path()), small_policy())),
    )
    .await
    .unwrap();
    let status = rig.admin.archive_status().unwrap();
    assert_eq!((status.chunks, status.unreferenced_chunks), (0, 1));
    assert_complete(&rig.admin, &owner, 6);
    // 第 7 条未导入成功；重新导入同一来源触发同一分块，内容一致即复用。
    commit(&rig, &owner, seventh).unwrap();
    let status = rig.admin.archive_status().unwrap();
    assert_eq!((status.chunks, status.unreferenced_chunks), (1, 0));
    // 已提交但确认丢失：调用方得到存储错误，重开后看到已提交的记录。
    let eighth = prepare(&rig, &owner, 8);
    state.fail_after_writes(true);
    assert!(matches!(
        commit(&rig, &owner, eighth),
        Err(MemoryError::Storage)
    ));
    stop(rig).await;
    drop(state);
    let state = RecordingStore::open(state_dir.path());
    let rig = start(
        Some(state),
        Some((flaky(archive_dir.path()), small_policy())),
    )
    .await
    .unwrap();
    assert_complete(&rig.admin, &owner, 8);
    stop(rig).await;
}

#[tokio::test]
async fn failed_chunk_writes_keep_the_hot_layer_and_report_without_blocking_imports() {
    let archive_dir = tempfile::tempdir().unwrap();
    let archive = flaky(archive_dir.path());
    archive.fail_put.store(true, Ordering::SeqCst);
    let owner = scope("put-fails");
    let rig = start(None, Some((archive.clone(), small_policy())))
        .await
        .unwrap();
    for number in 1..=10 {
        import(&rig, &owner, number).unwrap();
    }
    let status = rig.admin.archive_status().unwrap();
    assert_eq!((status.hot_evidence, status.chunks), (10, 0));
    assert!(status.last_error.is_some());
    archive.fail_put.store(false, Ordering::SeqCst);
    import(&rig, &owner, 11).unwrap();
    let status = rig.admin.archive_status().unwrap();
    assert!(status.chunks == 1 && status.last_error.is_none());
    assert_complete(&rig.admin, &owner, 11);
    stop(rig).await;
}

#[tokio::test]
async fn archived_turns_replay_idempotently_and_aliases_conflict() {
    let archive_dir = tempfile::tempdir().unwrap();
    let owner = scope("replay");
    let rig = start(None, Some((flaky(archive_dir.path()), small_policy())))
        .await
        .unwrap();
    for number in 1..=10 {
        import(&rig, &owner, number).unwrap();
    }
    let before = rig.admin.reader(owner.clone()).unwrap().snapshot().unwrap();
    assert!(before.evidence.iter().all(|e| e.id != "replay-e-1"));
    let session = rig.sessions.snapshot(&key(&owner)).unwrap().unwrap();
    let first_turn = session.turns.first().unwrap().id;
    let interaction = |id: &str, message: &str| CompletedInteraction {
        evidence_id: id.into(),
        message_id: message.into(),
        at_ms: 9_999,
        snapshot: session.clone(),
        turn_id: first_turn,
    };
    let replayed = rig
        .admin
        .import_completed(
            &owner,
            before.revision,
            interaction("replay-e-1", "replay-m-1"),
        )
        .unwrap();
    assert!(replayed == before);
    assert!(matches!(
        rig.admin.import_completed(
            &owner,
            before.revision,
            interaction("other-id", "other-message")
        ),
        Err(MemoryError::Conflict)
    ));
    stop(rig).await;
}

#[tokio::test]
async fn referenced_and_recent_evidence_stay_hot_and_preferences_keep_working() {
    let archive_dir = tempfile::tempdir().unwrap();
    let owner = scope("referenced");
    let rig = start(None, Some((flaky(archive_dir.path()), small_policy())))
        .await
        .unwrap();
    let snapshot = import(&rig, &owner, 1).unwrap();
    rig.admin
        .update_preference(
            &owner,
            snapshot.revision,
            PreferenceChange {
                operation_id: "op-1".into(),
                at_ms: 2_000,
                evidence: PreferenceEvidence::Existing("referenced-e-1".into()),
                action: PreferenceAction::Confirm {
                    id: "p-1".into(),
                    text: "先给结论".into(),
                },
            },
        )
        .unwrap();
    for number in 2..=14 {
        import(&rig, &owner, number).unwrap();
    }
    let snapshot = rig.admin.reader(owner.clone()).unwrap().snapshot().unwrap();
    assert!(snapshot.archived_evidence > 0);
    let hot: Vec<_> = snapshot.evidence.iter().map(|e| e.id.as_str()).collect();
    assert!(hot.contains(&"referenced-e-1"));
    assert!(hot.contains(&"referenced-e-13") && hot.contains(&"referenced-e-14"));
    let revoked = rig
        .admin
        .update_preference(
            &owner,
            snapshot.revision,
            PreferenceChange {
                operation_id: "op-2".into(),
                at_ms: 3_000,
                evidence: PreferenceEvidence::Statement(UserStatement {
                    evidence_id: "statement-2".into(),
                    message_id: "statement-m-2".into(),
                    text: "/forget p-1".into(),
                    at_ms: 3_000,
                }),
                action: PreferenceAction::Revoke { id: "p-1".into() },
            },
        )
        .unwrap();
    assert_eq!(revoked.preferences[0].status, PreferenceStatus::Revoked);
    // 偏好修订计入作用域修订；召回校验仍接受热层与归档合计。
    LexicalMemoryRecall::new(Arc::new(rig.admin.clone()))
        .reader(owner.clone())
        .unwrap()
        .recall(&MemoryRecallRequest {
            query: "结论".into(),
            limit: 3,
        })
        .unwrap();
    stop(rig).await;
}

#[tokio::test]
async fn documents_without_archives_stay_in_format_one() {
    let state_dir = tempfile::tempdir().unwrap();
    let archive_dir = tempfile::tempdir().unwrap();
    let owner = scope("format-one");
    let state = RecordingStore::open(state_dir.path());
    let rig = start(
        Some(state.clone()),
        Some((flaky(archive_dir.path()), small_policy())),
    )
    .await
    .unwrap();
    for number in 1..=5 {
        import(&rig, &owner, number).unwrap();
    }
    stop(rig).await;
    drop(state);
    let raw = support::raw_state(state_dir.path(), MEMORY_PLUGIN_ID, MEMORY_STATE_KEY);
    assert!(raw.contains("\"format_version\":1"), "{raw}");
    assert!(
        !raw.contains("archive") && !raw.contains("next_chunk"),
        "{raw}"
    );
    // 未装配归档的旧组合也能继续打开。
    let rig = start(Some(RecordingStore::open(state_dir.path())), None)
        .await
        .unwrap();
    assert_eq!(
        rig.admin
            .reader(owner.clone())
            .unwrap()
            .snapshot()
            .unwrap()
            .evidence
            .len(),
        5
    );
    stop(rig).await;
}

#[tokio::test]
async fn concurrent_imports_across_scopes_archive_consistently() {
    let state_dir = tempfile::tempdir().unwrap();
    let archive_dir = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(state_dir.path());
    let rig = Arc::new(
        start(
            Some(state.clone()),
            Some((flaky(archive_dir.path()), small_policy())),
        )
        .await
        .unwrap(),
    );
    let scopes: Vec<_> = (0..4).map(|i| scope(&format!("c{i}"))).collect();
    let handles: Vec<_> = scopes
        .iter()
        .cloned()
        .map(|owner| {
            let rig = rig.clone();
            std::thread::spawn(move || {
                for number in 1..=15 {
                    import(&rig, &owner, number).unwrap();
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    for owner in &scopes {
        assert_complete(&rig.admin, owner, 15);
    }
    let rig = Arc::try_unwrap(rig).ok().unwrap();
    stop(rig).await;
    drop(state);
    let rig = start(
        Some(RecordingStore::open(state_dir.path())),
        Some((flaky(archive_dir.path()), small_policy())),
    )
    .await
    .unwrap();
    for owner in &scopes {
        assert_complete(&rig.admin, owner, 15);
    }
    stop(rig).await;
}

/// 只用本地会话快照构造第 `number` 次完成交互，不经会话插件；用于大量作用域。
fn local(
    scope: &MemoryScope,
    session: &mut SessionSnapshot,
    number: usize,
) -> CompletedInteraction {
    let (user, reply) = texts(scope, number);
    session.turns.push(SessionTurn {
        id: number as u64,
        input: user.clone(),
        status: SessionTurnStatus::Completed {
            messages: vec![
                ChatMessage::text(ChatRole::User, &user),
                ChatMessage::text(ChatRole::Assistant, &reply),
            ],
        },
    });
    session.revision += 2;
    CompletedInteraction {
        evidence_id: format!("{}-e-{number}", scope.session_id),
        message_id: format!("{}-m-{number}", scope.session_id),
        at_ms: 1000 + number as u64,
        snapshot: session.clone(),
        turn_id: number as u64,
    }
}

fn empty_session(scope: &MemoryScope) -> SessionSnapshot {
    SessionSnapshot {
        key: key(scope),
        revision: 0,
        turns: vec![],
    }
}

#[tokio::test]
async fn pressure_keeps_only_the_latest_per_scope_so_many_scopes_cannot_pin_the_hot_layer() {
    let directory = tempfile::tempdir().unwrap();
    let archive = Arc::new(FileEvidenceArchive::open(directory.path()).unwrap());
    // 证据都很新：只有压力才会归档。平时每个作用域保留 4 条，10 个作用域就要 40 条，
    // 超过压力线 20；压力下每个作用域只保留最近 1 条，热层才能回落。
    let policy = ArchivePolicy {
        trigger_evidence: 6,
        pressure_evidence: 20,
        keep_recent: 4,
        pressure_keep_recent: 1,
        min_age_ms: u64::MAX / 2,
        min_chunk: 2,
        max_chunk_evidence: 64,
        max_chunk_bytes: 64 * 1024,
        ..ArchivePolicy::default()
    };
    let rig = start(None, Some((archive, policy))).await.unwrap();
    let scopes: Vec<_> = (0..10).map(|i| scope(&format!("p{i:02}"))).collect();
    let mut sessions: Vec<_> = scopes.iter().map(empty_session).collect();
    for number in 1..=6 {
        for (scope, session) in scopes.iter().zip(sessions.iter_mut()) {
            commit(&rig, scope, local(scope, session, number)).unwrap();
            let hot = rig.admin.archive_status().unwrap().hot_evidence;
            assert!(hot <= 20, "压力下热层应回落到压力线以内：{hot}");
        }
    }
    let status = rig.admin.archive_status().unwrap();
    assert_eq!(status.hot_evidence as u64 + status.archived_evidence, 60);
    assert!(status.archived_evidence > 0 && status.last_error.is_none());
    for scope in &scopes {
        let snapshot = rig.admin.reader(scope.clone()).unwrap().snapshot().unwrap();
        // 每个作用域的最新一条始终留在热层。
        assert_eq!(snapshot.evidence.last().unwrap().revision, 6);
        assert_complete(&rig.admin, scope, 6);
    }
    stop(rig).await;
}

#[tokio::test]
async fn full_scope_capacity_refuses_new_scopes_without_writing_and_keeps_existing_ones() {
    let state_dir = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(state_dir.path());
    let rig = start(Some(state.clone()), None).await.unwrap();
    let scopes: Vec<_> = (0..MAX_SCOPES)
        .map(|i| scope(&format!("g{i:04}")))
        .collect();
    let mut sessions: Vec<_> = scopes.iter().map(empty_session).collect();
    for (scope, session) in scopes.iter().zip(sessions.iter_mut()) {
        commit(&rig, scope, local(scope, session, 1)).unwrap();
    }
    assert_eq!(rig.admin.scopes().unwrap().len(), MAX_SCOPES);
    let writes = state.writes();
    let newcomer = scope("newcomer");
    let mut session = empty_session(&newcomer);
    assert_eq!(
        commit(&rig, &newcomer, local(&newcomer, &mut session, 1)),
        Err(MemoryError::LimitReached)
    );
    assert_eq!(state.writes(), writes, "容量满时不写状态");
    assert_eq!(rig.admin.scopes().unwrap().len(), MAX_SCOPES);
    let snapshot = rig
        .admin
        .reader(newcomer.clone())
        .unwrap()
        .snapshot()
        .unwrap();
    assert_eq!((snapshot.revision, snapshot.evidence.len()), (0, 0));
    // 已有作用域照常采集。
    commit(&rig, &scopes[7], local(&scopes[7], &mut sessions[7], 2)).unwrap();
    assert_eq!(state.writes(), writes + 1);
    stop(rig).await;
    let rig = start(Some(state.clone()), None).await.unwrap();
    assert_eq!(rig.admin.scopes().unwrap().len(), MAX_SCOPES);
    assert_eq!(
        rig.admin
            .reader(scopes[7].clone())
            .unwrap()
            .snapshot()
            .unwrap()
            .revision,
        2
    );
    stop(rig).await;
}

/// 把状态目录里本插件的热层文档读成 JSON，或写回。
fn read_document(directory: &Path) -> serde_json::Value {
    serde_json::from_str(&support::raw_state(
        directory,
        MEMORY_PLUGIN_ID,
        MEMORY_STATE_KEY,
    ))
    .unwrap()
}
fn write_document(directory: &Path, document: &serde_json::Value) {
    let path = directory.join("state.json");
    let mut state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    state["entries"][MEMORY_PLUGIN_ID][MEMORY_STATE_KEY] =
        serde_json::to_value(serde_json::to_vec(document).unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
}
fn sha256(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[tokio::test]
async fn compressed_archives_stay_part_of_memory_and_old_plain_chunks_still_read() {
    use std::io::Read;
    let state_dir = tempfile::tempdir().unwrap();
    let archive_dir = tempfile::tempdir().unwrap();
    let owner = scope("whole");
    let state = RecordingStore::open(state_dir.path());
    let rig = start(
        Some(state.clone()),
        Some((flaky(archive_dir.path()), small_policy())),
    )
    .await
    .unwrap();
    for number in 1..=12 {
        import(&rig, &owner, number).unwrap();
    }
    let reader = rig.admin.reader(owner.clone()).unwrap();
    let hot = reader.snapshot().unwrap();
    assert!(hot.archived_evidence >= 4);
    // 完整记忆：热层与归档按修订合并，一条不少、原文不变，看起来与从未归档一样。
    let complete = reader.complete_snapshot().unwrap();
    assert_eq!(complete.archived_evidence, 0);
    assert_eq!(complete.revision, hot.revision);
    assert_eq!(
        complete
            .evidence
            .iter()
            .map(|e| e.revision)
            .collect::<Vec<_>>(),
        (1..=12).collect::<Vec<u64>>()
    );
    for (index, item) in complete.evidence.iter().enumerate() {
        assert_eq!(completed_texts(item), texts(&owner, index + 1));
    }
    // 召回能找到已经归档的旧原文。
    let archived = complete
        .evidence
        .iter()
        .find(|e| !hot.evidence.iter().any(|h| h.id == e.id))
        .unwrap();
    let recall = LexicalMemoryRecall::new(Arc::new(rig.admin.clone()))
        .reader(owner.clone())
        .unwrap()
        .recall(&MemoryRecallRequest {
            query: completed_texts(archived).0,
            limit: 8,
        })
        .unwrap();
    assert!(recall.hits.iter().any(|hit| matches!(
        &hit.source,
        MemoryRecallSource::CompletedInteraction { evidence_id, .. } if evidence_id == &archived.id
    )));
    stop(rig).await;
    drop(state);

    // 压缩是无损的，且比原 JSON 小。
    let document = read_document(state_dir.path());
    let mut raw_total = 0;
    let mut stored_total = 0;
    for chunk in document["scopes"][0]["archive"].as_array().unwrap() {
        assert_eq!(chunk["compressed"], true);
        let name = format!(
            "evidence-{:010}-{}.json.zz",
            chunk["sequence"].as_u64().unwrap(),
            chunk["sha256"].as_str().unwrap()
        );
        let stored = std::fs::read(archive_dir.path().join(name)).unwrap();
        let mut raw = Vec::new();
        flate2::read::ZlibDecoder::new(stored.as_slice())
            .read_to_end(&mut raw)
            .unwrap();
        raw_total += raw.len();
        stored_total += stored.len();
    }
    assert!(stored_total < raw_total, "{stored_total} >= {raw_total}");

    // 模拟 #222 时期的状态：分块都是未压缩 JSON、文档格式 2。重启后照常完整可读。
    let mut document = read_document(state_dir.path());
    for chunk in document["scopes"][0]["archive"].as_array_mut().unwrap() {
        let old_name = format!(
            "evidence-{:010}-{}.json.zz",
            chunk["sequence"].as_u64().unwrap(),
            chunk["sha256"].as_str().unwrap()
        );
        let stored = std::fs::read(archive_dir.path().join(&old_name)).unwrap();
        let mut plain = Vec::new();
        flate2::read::ZlibDecoder::new(stored.as_slice())
            .read_to_end(&mut plain)
            .unwrap();
        let sha = sha256(&plain);
        std::fs::write(
            archive_dir.path().join(format!(
                "evidence-{:010}-{sha}.json",
                chunk["sequence"].as_u64().unwrap()
            )),
            &plain,
        )
        .unwrap();
        std::fs::remove_file(archive_dir.path().join(old_name)).unwrap();
        let object = chunk.as_object_mut().unwrap();
        object.remove("compressed");
        object.insert("sha256".into(), sha.into());
        object.insert("bytes".into(), plain.len().into());
    }
    document["format_version"] = 2.into();
    write_document(state_dir.path(), &document);
    let state = RecordingStore::open(state_dir.path());
    let rig = start(
        Some(state.clone()),
        Some((flaky(archive_dir.path()), small_policy())),
    )
    .await
    .unwrap();
    let reader = rig.admin.reader(owner.clone()).unwrap();
    assert!(reader.complete_snapshot().unwrap() == complete);
    // 之后新归档的分块照常压缩；新旧分块合起来仍是完整记忆。
    for number in 13..=20 {
        import(&rig, &owner, number).unwrap();
    }
    assert_complete(&rig.admin, &owner, 20);
    let complete = reader.complete_snapshot().unwrap();
    assert_eq!(complete.evidence.len(), 20);
    stop(rig).await;
    drop(state);
    let document = read_document(state_dir.path());
    assert_eq!(document["format_version"], 3);
    let archive = document["scopes"][0]["archive"].as_array().unwrap();
    assert!(
        archive
            .iter()
            .any(|chunk| chunk.get("compressed").is_none())
    );
    assert!(archive.iter().any(|chunk| chunk["compressed"] == true));
}

#[tokio::test]
async fn archived_evidence_can_confirm_a_preference_and_replay_after_restart_without_copying_it() {
    let state_dir = tempfile::tempdir().unwrap();
    let archive_dir = tempfile::tempdir().unwrap();
    let state = RecordingStore::open(state_dir.path());
    let archive = Arc::new(FileEvidenceArchive::open(archive_dir.path()).unwrap());
    let owner = scope("archived-candidate");
    let rig = start(Some(state.clone()), Some((archive.clone(), small_policy())))
        .await
        .unwrap();
    for n in 1..=12 {
        import(&rig, &owner, n).unwrap();
    }
    let before = rig.admin.reader(owner.clone()).unwrap().snapshot().unwrap();
    let old = all_archived(&rig.admin, &owner)[0].clone();
    assert!(!before.evidence.iter().any(|e| e.id == old.id));
    let change = PreferenceChange {
        operation_id: "confirm-archived".into(),
        at_ms: 2000,
        evidence: PreferenceEvidence::Existing(old.id.clone()),
        action: PreferenceAction::Confirm {
            id: "from-archive".into(),
            text: "回复先给结论".into(),
        },
    };
    let saved = rig
        .admin
        .update_preference(&owner, before.revision, change.clone())
        .unwrap();
    assert_eq!(saved.evidence, before.evidence);
    assert_eq!(saved.archived_evidence, before.archived_evidence);
    assert_eq!(saved.preferences[0].history[0].evidence_id, old.id);
    let writes = state.writes();
    assert!(
        rig.admin
            .update_preference(&owner, before.revision, change.clone())
            .unwrap()
            == saved
    );
    assert_eq!(state.writes(), writes);
    stop(rig).await;
    let raw = support::raw_state(state_dir.path(), MEMORY_PLUGIN_ID, MEMORY_STATE_KEY);
    assert!(raw.contains("\"format_version\":4"));
    let rig = start(Some(state.clone()), Some((archive.clone(), small_policy())))
        .await
        .unwrap();
    assert!(rig.admin.reader(owner.clone()).unwrap().snapshot().unwrap() == saved);
    let complete = rig
        .admin
        .reader(owner.clone())
        .unwrap()
        .complete_snapshot()
        .unwrap();
    assert_eq!(
        complete.evidence.iter().filter(|e| e.id == old.id).count(),
        1
    );
    stop(rig).await;
    // 引用的归档被篡改时不能恢复出一个看似有效的偏好，不写盘、不删除现场。
    let path = archive_dir.path().join(&archive.list().unwrap()[0]);
    std::fs::write(&path, b"corrupted").unwrap();
    let writes = state.writes();
    assert!(
        start(Some(state.clone()), Some((archive, small_policy())))
            .await
            .is_err()
    );
    assert_eq!(state.writes(), writes);
    assert_eq!(
        support::raw_state(state_dir.path(), MEMORY_PLUGIN_ID, MEMORY_STATE_KEY),
        raw
    );
}

#[tokio::test]
async fn archived_single_scope_over_hot_byte_limit_still_recalls_old_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let archive = Arc::new(FileEvidenceArchive::open(directory.path()).unwrap());
    let rig = start(None, Some((archive, ArchivePolicy::default())))
        .await
        .unwrap();
    let scope = scope("large-single-scope");
    let mut session = SessionSnapshot {
        key: key(&scope),
        revision: 0,
        turns: vec![],
    };
    for number in 1..=80 {
        let user = format!("unique-original-{number} {}", "x".repeat(30000));
        let reply = "y".repeat(30000);
        session.turns.push(SessionTurn {
            id: number,
            input: user.clone(),
            status: SessionTurnStatus::Completed {
                messages: vec![
                    ChatMessage::text(ChatRole::User, user),
                    ChatMessage::text(ChatRole::Assistant, reply),
                ],
            },
        });
        session.revision += 2;
        commit(
            &rig,
            &scope,
            CompletedInteraction {
                evidence_id: format!("e-{number}"),
                message_id: format!("m-{number}"),
                at_ms: number,
                snapshot: session.clone(),
                turn_id: number,
            },
        )
        .unwrap();
    }
    assert!(rig.admin.archive_status().unwrap().chunks > 0);
    let full = rig
        .admin
        .reader(scope.clone())
        .unwrap()
        .complete_snapshot()
        .unwrap();
    assert!(serde_json::to_vec(&full).unwrap().len() > MAX_STATE_BYTES);
    let recall = LexicalMemoryRecall::new(Arc::new(rig.admin.clone()))
        .reader(scope)
        .unwrap();
    let result = recall
        .recall(&MemoryRecallRequest {
            query: "unique-original-1".into(),
            limit: 1,
        })
        .unwrap();
    assert_eq!(result.hits.len(), 1);
    assert!(result.hits[0].excerpt.contains("unique-original-1"));
    stop(rig).await;
}

#[tokio::test]
async fn archived_identity_aliases_cannot_be_reused_by_a_new_turn() {
    for alias_message in [false,true] {
        let directory=tempfile::tempdir().unwrap();
        let archive=Arc::new(FileEvidenceArchive::open(directory.path()).unwrap());
        let rig=start(None,Some((archive,small_policy()))).await.unwrap();
        let scope=scope("archive-alias");
        for number in 1..=8 { import(&rig,&scope,number).unwrap(); }
        let archived=all_archived(&rig.admin,&scope);
        assert!(!archived.is_empty());
        let original=rig.admin.reader(scope.clone()).unwrap().complete_snapshot().unwrap();
        let mut next=prepare(&rig,&scope,9);
        if alias_message {
            let EvidenceSource::CompletedInteraction{message_id,..}=&archived[0].source else {panic!()};
            next.message_id=message_id.clone();
        } else { next.evidence_id=archived[0].id.clone(); }
        assert!(matches!(commit(&rig,&scope,next),Err(MemoryError::Conflict)),"旧归档身份不得由新轮次复用");
        assert!(rig.admin.reader(scope).unwrap().complete_snapshot().unwrap()==original);
        stop(rig).await;
    }
}
