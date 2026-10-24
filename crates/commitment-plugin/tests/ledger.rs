use eve_commitment_api::*;
use eve_commitment_plugin::*;
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_plugin_api::*;
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

fn plugin_id() -> PluginId {
    PluginId::new(COMMITMENT_PLUGIN_ID).unwrap()
}
async fn start(state: Arc<dyn StateStore>) -> (Kernel, CommitmentController) {
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let plugin = CommitmentPlugin::new().unwrap();
    let controller = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start(&plugin_id()).await.unwrap();
    (kernel, controller)
}
fn owners(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

#[tokio::test]
async fn record_is_idempotent_bounded_and_settled_only_by_linked_owners() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, ledger) = start(state.clone()).await;
    let first = ledger
        .record("qq:a", "周末帮他看地图", "tool:1", 10)
        .unwrap();
    assert_eq!(
        ledger
            .record("qq:a", "周末帮他看地图", "tool:1", 11)
            .unwrap(),
        first,
        "同一来源只记一条"
    );
    let other = ledger
        .record("qq:b", "教她用地图编辑器", "tool:1", 12)
        .unwrap();
    assert_ne!(other.id, first.id, "不同人的同名来源互不影响");
    assert_eq!(ledger.owners().unwrap(), owners(&["qq:a", "qq:b"]));
    assert!(ledger.record("qq:a", " 有空白 ", "tool:2", 13).is_err());
    for n in 2..=MAX_OPEN_PER_OWNER {
        ledger
            .record("qq:a", &format!("第{n}件"), &format!("tool:{n}"), 20)
            .unwrap();
    }
    assert_eq!(
        ledger.record("qq:a", "再多一件", "tool:more", 21),
        Err(CommitmentError::LimitReached),
        "每人未完成的约定有上限"
    );

    assert_eq!(
        ledger.settle(
            &owners(&["qq:b"]),
            &first.id,
            CommitmentStatus::Done,
            None,
            30
        ),
        Err(CommitmentError::NotFound),
        "不是这个人的约定不能了结"
    );
    assert_eq!(
        ledger.settle(
            &owners(&["qq:a"]),
            &first.id,
            CommitmentStatus::Open,
            None,
            30
        ),
        Err(CommitmentError::InvalidInput)
    );
    let done = ledger
        .settle(
            &owners(&["qq:b", "qq:a"]),
            &first.id,
            CommitmentStatus::Done,
            Some("看完了，地图没问题"),
            30,
        )
        .unwrap();
    assert_eq!(
        (done.status, done.settled_at_ms),
        (CommitmentStatus::Done, Some(30))
    );
    assert_eq!(
        ledger.settle(
            &owners(&["qq:a"]),
            &first.id,
            CommitmentStatus::Dropped,
            None,
            31
        ),
        Err(CommitmentError::AlreadySettled)
    );
    ledger.record("qq:a", "再多一件", "tool:more", 32).unwrap();
    let listed = ledger.list(&owners(&["qq:a"])).unwrap();
    assert_eq!(listed.len(), MAX_OPEN_PER_OWNER + 1);
    assert_eq!(listed[0].text, "再多一件", "新的在前");
    assert!(ledger.list(&owners(&["qq:c"])).unwrap().is_empty());

    kernel.stop(&plugin_id()).await.unwrap();
    assert_eq!(
        ledger.list(&owners(&["qq:a"])),
        Err(CommitmentError::Unavailable),
        "停止后旧句柄不可用"
    );
    let (_kernel, ledger) = start(state.clone()).await;
    assert_eq!(
        ledger.list(&owners(&["qq:a"])).unwrap(),
        listed,
        "重启后保留"
    );
}

#[tokio::test]
async fn corrupt_or_unknown_state_refuses_to_start_and_keeps_the_bytes() {
    for bytes in [
        b"not json".to_vec(),
        br#"{"format_version":2,"next":0,"commitments":[]}"#.to_vec(),
        br#"{"format_version":1,"next":0,"commitments":[{"id":"c1","owner":"qq:a","text":"x","source":"s","status":"open","created_at_ms":1}]}"#.to_vec(),
        br#"{"format_version":1,"next":1,"commitments":[{"id":"c1","owner":"qq:a","text":"x","source":"s","status":"done","created_at_ms":1}]}"#.to_vec(),
    ] {
        let state = Arc::new(MemoryStateStore::default());
        state.set(&plugin_id(), COMMITMENT_STATE_KEY.into(), bytes.clone()).unwrap();
        let kernel = Kernel::with_services(KernelServices {
            state: state.clone(),
            ..KernelServices::default()
        });
        kernel.register(Box::new(CommitmentPlugin::new().unwrap())).unwrap();
        assert!(kernel.start(&plugin_id()).await.is_err());
        assert_eq!(state.get(&plugin_id(), COMMITMENT_STATE_KEY).unwrap(), Some(bytes));
    }
}

struct Faulty {
    base: MemoryStateStore,
    failure: AtomicU8,
}
impl StateStore for Faulty {
    fn get(&self, ns: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.base.get(ns, key)
    }
    fn set(&self, ns: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        let mode = self.failure.load(Ordering::SeqCst);
        if mode != 1 {
            self.base.set(ns, key, value)?;
        }
        if mode == 0 {
            Ok(())
        } else {
            Err(PluginError::State("synthetic uncertain commit".into()))
        }
    }
}

#[tokio::test]
async fn failed_writes_close_the_ledger_and_restart_uses_the_actual_bytes() {
    for mode in [1, 2] {
        let state = Arc::new(Faulty {
            base: MemoryStateStore::default(),
            failure: AtomicU8::new(0),
        });
        let (kernel, ledger) = start(state.clone()).await;
        ledger.record("qq:a", "旧约定", "tool:1", 1).unwrap();
        state.failure.store(mode, Ordering::SeqCst);
        assert_eq!(
            ledger.record("qq:a", "新约定", "tool:2", 2),
            Err(CommitmentError::Storage)
        );
        assert_eq!(
            ledger.list(&owners(&["qq:a"])),
            Err(CommitmentError::Unavailable),
            "写入不确定时关闭账本"
        );
        kernel.stop(&plugin_id()).await.unwrap();
        state.failure.store(0, Ordering::SeqCst);
        let (_kernel, ledger) = start(state.clone()).await;
        let texts: Vec<_> = ledger
            .list(&owners(&["qq:a"]))
            .unwrap()
            .into_iter()
            .map(|c| c.text)
            .collect();
        assert_eq!(
            texts,
            if mode == 1 {
                vec!["旧约定"]
            } else {
                vec!["新约定", "旧约定"]
            },
            "以磁盘为准"
        );
    }
}
