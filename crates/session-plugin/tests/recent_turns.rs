use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_llm_api::{ChatMessage, ChatRole};
use eve_plugin_api::{PluginId, ServiceId, StateStore};
use eve_session_api::*;
use eve_session_plugin::{SESSION_STATE_KEY, SessionPlugin};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct CountingArchive {
    records: Mutex<BTreeMap<String, Vec<u8>>>,
    reads: Mutex<Vec<String>>,
}
impl SessionArchiveStore for CountingArchive {
    fn put(&self, key: &str, bytes: &[u8]) -> SessionResult<()> {
        let mut records = self.records.lock().unwrap();
        if let Some(existing) = records.get(key) {
            assert_eq!(existing, bytes, "immutable archive");
        } else {
            records.insert(key.into(), bytes.into());
        }
        Ok(())
    }
    fn get(&self, key: &str) -> SessionResult<Vec<u8>> {
        self.reads.lock().unwrap().push(key.into());
        self.records
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .ok_or(SessionError::Storage)
    }
}
impl CountingArchive {
    fn take_reads(&self) -> Vec<String> {
        std::mem::take(&mut *self.reads.lock().unwrap())
    }
}
fn key() -> SessionKey {
    SessionKey::new("conversation", "owner").unwrap()
}
fn plugin_id() -> PluginId {
    PluginId::new(SESSION_PLUGIN_ID).unwrap()
}
async fn open(
    state: Arc<MemoryStateStore>,
    archive: Option<Arc<CountingArchive>>,
) -> (Kernel, Arc<dyn SessionService>) {
    let services = KernelServices {
        state,
        ..KernelServices::default()
    };
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
    let mut plugin = SessionPlugin::new().unwrap();
    if let Some(archive) = archive {
        plugin = plugin.with_archive(archive);
    }
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let service = registry
        .get(&ServiceId::new(SESSION_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<SessionServiceHandle>()
        .unwrap()
        .0
        .clone();
    (kernel, service)
}
fn completed(service: &dyn SessionService, text: &str) {
    let started = service
        .begin(SessionInput {
            key: key(),
            text: text.into(),
        })
        .unwrap();
    service
        .complete(
            &started.lease,
            vec![
                ChatMessage::text(ChatRole::User, text),
                ChatMessage::text(ChatRole::Assistant, format!("原始答复：{text}")),
            ],
        )
        .unwrap();
}
fn ids(turns: &[SessionTurn]) -> Vec<u64> {
    turns.iter().map(|turn| turn.id).collect()
}
fn hashes(state: &dyn StateStore) -> Vec<String> {
    let index: serde_json::Value =
        serde_json::from_slice(&state.get(&plugin_id(), SESSION_STATE_KEY).unwrap().unwrap())
            .unwrap();
    index["sessions"]["conversation"]["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["hash"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn pages_keep_all_statuses_and_stable_before_boundaries_when_new_turns_arrive() {
    for cold in [false, true] {
        let state = Arc::new(MemoryStateStore::default());
        let archive = cold.then(|| Arc::new(CountingArchive::default()));
        let (kernel, service) = open(state.clone(), archive.clone()).await;
        for number in 1..=18 {
            completed(service.as_ref(), &format!("第 {number} 轮"));
        }
        let failed = service
            .begin(SessionInput {
                key: key(),
                text: "失败输入仍应可见".into(),
            })
            .unwrap();
        service
            .fail(
                &failed.lease,
                SessionFailure {
                    code: SessionFailureCode::Provider,
                    started_tools: Some(0),
                    evidence: None,
                },
            )
            .unwrap();
        let pending = service
            .begin(SessionInput {
                key: key(),
                text: "当前输入完整保留".into(),
            })
            .unwrap();
        let before_read = state.get(&plugin_id(), SESSION_STATE_KEY).unwrap();
        let page = service.recent_turns(&key(), None, 6).unwrap();
        assert_eq!(ids(&page), [20, 19, 18, 17, 16, 15]);
        assert_eq!(page[0].status, SessionTurnStatus::Pending);
        assert!(matches!(page[1].status, SessionTurnStatus::Failed { .. }));
        assert_eq!(page[0].input, "当前输入完整保留");
        assert_eq!(
            state.get(&plugin_id(), SESSION_STATE_KEY).unwrap(),
            before_read
        );
        service
            .complete(
                &pending.lease,
                vec![
                    ChatMessage::text(ChatRole::User, "当前输入完整保留"),
                    ChatMessage::text(ChatRole::Assistant, "已结束"),
                ],
            )
            .unwrap();
        completed(service.as_ref(), "后来的新轮次");
        assert_eq!(
            ids(&service
                .recent_turns(&key(), Some(page.last().unwrap().id), 6)
                .unwrap()),
            [14, 13, 12, 11, 10, 9]
        );
        assert_eq!(
            page[0].status,
            SessionTurnStatus::Pending,
            "已返回的页不能随后来完成而改变"
        );
        for cursor in [0, 1] {
            assert!(
                service
                    .recent_turns(&key(), Some(cursor), 6)
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(
            ids(&service.recent_turns(&key(), Some(3), 16).unwrap()),
            [2, 1]
        );
        assert_eq!(
            ids(&service.recent_turns(&key(), Some(u64::MAX), 1).unwrap()),
            [21]
        );
        assert_eq!(
            service.recent_turns(&key(), None, 0),
            Err(SessionError::InvalidInput)
        );
        assert_eq!(
            service.recent_turns(&key(), None, 17),
            Err(SessionError::InvalidInput)
        );
        let other = SessionKey::new("conversation", "another-owner").unwrap();
        assert_eq!(
            service.recent_turns(&other, None, 6),
            Err(SessionError::OwnerMismatch)
        );
        assert_eq!(
            service.recent_turns(&other, Some(0), 6),
            Err(SessionError::OwnerMismatch)
        );
        assert!(
            service
                .recent_turns(&SessionKey::new("absent", "owner").unwrap(), None, 6)
                .unwrap()
                .is_empty()
        );
        service
            .begin(SessionInput {
                key: key(),
                text: "等待中重启".into(),
            })
            .unwrap();
        kernel.stop_all().await.unwrap();
        assert_eq!(
            service.recent_turns(&key(), None, 6),
            Err(SessionError::Unavailable)
        );
        let (kernel, service) = open(state, archive).await;
        let page = service.recent_turns(&key(), None, 1).unwrap();
        assert_eq!(page[0].id, 22);
        assert_eq!(page[0].status, SessionTurnStatus::Interrupted);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn cold_recent_page_reads_only_requested_records_and_checks_owner_before_archive_access() {
    let state = Arc::new(MemoryStateStore::default());
    let archive = Arc::new(CountingArchive::default());
    let (kernel, service) = open(state.clone(), Some(archive.clone())).await;
    for number in 1..=64 {
        completed(service.as_ref(), &format!("原始工具轮 {number}"));
    }
    let hashes = hashes(state.as_ref());
    archive.take_reads();
    let page = service.recent_turns(&key(), None, 6).unwrap();
    assert_eq!(ids(&page), [64, 63, 62, 61, 60, 59]);
    assert_eq!(
        archive.take_reads(),
        hashes[58..64].iter().rev().cloned().collect::<Vec<_>>()
    );
    assert_eq!(
        ids(&service.recent_turns(&key(), Some(20), 3).unwrap()),
        [19, 18, 17]
    );
    assert_eq!(
        archive.take_reads(),
        hashes[16..19].iter().rev().cloned().collect::<Vec<_>>()
    );
    let other = SessionKey::new("conversation", "another-owner").unwrap();
    assert_eq!(
        service.recent_turns(&other, None, 6),
        Err(SessionError::OwnerMismatch)
    );
    assert!(archive.take_reads().is_empty());
    assert!(service.recent_turns(&key(), Some(1), 6).unwrap().is_empty());
    assert!(archive.take_reads().is_empty());
    assert_eq!(
        service.recent_turns(&key(), None, 17),
        Err(SessionError::InvalidInput)
    );
    assert!(archive.take_reads().is_empty());
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn corrupted_selected_record_is_an_error_and_never_an_empty_or_repaired_page() {
    let state = Arc::new(MemoryStateStore::default());
    let archive = Arc::new(CountingArchive::default());
    let (kernel, service) = open(state.clone(), Some(archive.clone())).await;
    for number in 1..=8 {
        completed(service.as_ref(), &format!("record {number}"));
    }
    let hashes = hashes(state.as_ref());
    let index = state.get(&plugin_id(), SESSION_STATE_KEY).unwrap();
    // 不在所请求页的旧损坏正文不会导致 recent_turns 偷读全部历史。
    archive
        .records
        .lock()
        .unwrap()
        .insert(hashes[0].clone(), b"old-corruption".to_vec());
    archive.take_reads();
    assert_eq!(ids(&service.recent_turns(&key(), None, 2).unwrap()), [8, 7]);
    assert_eq!(archive.take_reads(), [hashes[7].clone(), hashes[6].clone()]);
    let corrupt = b"selected-corruption".to_vec();
    archive
        .records
        .lock()
        .unwrap()
        .insert(hashes[7].clone(), corrupt.clone());
    assert_eq!(
        service.recent_turns(&key(), None, 2),
        Err(SessionError::CorruptState)
    );
    assert_eq!(archive.take_reads(), [hashes[7].clone()]);
    assert_eq!(state.get(&plugin_id(), SESSION_STATE_KEY).unwrap(), index);
    assert_eq!(
        archive.records.lock().unwrap().get(&hashes[7]),
        Some(&corrupt)
    );
    assert_eq!(
        service.recent_turns(&key(), None, 2),
        Err(SessionError::Unavailable)
    );
    assert!(archive.take_reads().is_empty());
    kernel.stop_all().await.unwrap();
}
