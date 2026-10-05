use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use eve_segment_api::{
    SegmentChange, SegmentPreference, SegmentPreferenceError, SegmentPreferences, SegmentScope,
};
use eve_segment_plugin::{
    MAX_PREFERENCE_SCOPES, SEGMENT_PREFERENCE_STATE_KEY, SEGMENT_PREFERENCES_PLUGIN_ID,
    SegmentPreferenceController, SegmentPreferencePlugin,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

/// 包装真实文件库，分别模拟提交前失败和已提交但确认丢失。
struct RecordingStore {
    inner: FileStateStore,
    fail_write: AtomicBool,
    fail_after_write: AtomicBool,
    writes: AtomicUsize,
}
impl RecordingStore {
    fn open(path: &std::path::Path) -> Arc<Self> {
        Arc::new(Self {
            inner: FileStateStore::open(path).unwrap(),
            fail_write: AtomicBool::new(false),
            fail_after_write: AtomicBool::new(false),
            writes: AtomicUsize::new(0),
        })
    }
    fn writes(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }
    fn bytes(&self) -> Option<Vec<u8>> {
        self.inner
            .get(&owner(), SEGMENT_PREFERENCE_STATE_KEY)
            .unwrap()
    }
    fn put(&self, value: &[u8]) {
        self.inner
            .set(
                &owner(),
                SEGMENT_PREFERENCE_STATE_KEY.into(),
                value.to_vec(),
            )
            .unwrap();
    }
}
impl StateStore for RecordingStore {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.inner.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        if self.fail_write.load(Ordering::SeqCst) {
            return Err(PluginError::State("test-storage-private-detail".into()));
        }
        self.inner.set(namespace, key, value)?;
        if self.fail_after_write.load(Ordering::SeqCst) {
            return Err(PluginError::State("test-storage-private-detail".into()));
        }
        Ok(())
    }
}

fn owner() -> PluginId {
    PluginId::new(SEGMENT_PREFERENCES_PLUGIN_ID).unwrap()
}
fn scope(channel: &str, session: &str, user: &str) -> SegmentScope {
    SegmentScope {
        channel: channel.into(),
        session_id: session.into(),
        user_id: user.into(),
    }
}
async fn open(store: Arc<RecordingStore>) -> PluginResult<(Kernel, SegmentPreferenceController)> {
    let kernel = Kernel::with_services(KernelServices {
        state: store,
        ..KernelServices::default()
    });
    let plugin = SegmentPreferencePlugin::new()?;
    let controller = plugin.controller();
    kernel.register(Box::new(plugin))?;
    kernel.start(&owner()).await?;
    Ok((kernel, controller))
}

#[tokio::test]
async fn explicit_settings_persist_in_canonical_json_and_scopes_stay_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(dir.path());
    let (kernel, prefs) = open(store.clone()).await.unwrap();
    let qq = scope("qq", "s-1", "u-1");
    assert_eq!(prefs.get(&qq).unwrap(), SegmentPreference::default());
    assert_eq!(
        prefs.update(&qq, SegmentChange::Enabled(false)).unwrap(),
        SegmentPreference {
            enabled: Some(false),
            ..SegmentPreference::default()
        }
    );
    prefs.update(&qq, SegmentChange::PausePercent(50)).unwrap();
    prefs
        .update(
            &scope("console", "default", "owner"),
            SegmentChange::MaxSegments(2),
        )
        .unwrap();
    for other in [
        scope("console", "s-1", "u-1"),
        scope("qq", "s-1", "u-2"),
        scope("qq", "s-2", "u-1"),
    ] {
        assert_eq!(prefs.get(&other).unwrap(), SegmentPreference::default());
    }
    let document: Value = serde_json::from_slice(&store.bytes().unwrap()).unwrap();
    assert_eq!(
        document,
        json!({"version": 1, "scopes": [
            {"scope": {"channel": "console", "session_id": "default", "user_id": "owner"},
             "enabled": null, "max_segments": 2, "pause_percent": null},
            {"scope": {"channel": "qq", "session_id": "s-1", "user_id": "u-1"},
             "enabled": false, "max_segments": null, "pause_percent": 50}
        ]})
    );
    kernel.stop_all().await.unwrap();
    assert_eq!(prefs.get(&qq), Err(SegmentPreferenceError::Unavailable));
    let (_kernel, reopened) = open(store.clone()).await.unwrap();
    assert_eq!(
        reopened.get(&qq).unwrap(),
        SegmentPreference {
            enabled: Some(false),
            max_segments: None,
            pause_percent: Some(50),
        }
    );
    // 停止前的控制器不会连到新实例，避免旧句柄读写重新打开后的状态。
    assert_eq!(prefs.get(&qq), Err(SegmentPreferenceError::Unavailable));
}

#[tokio::test]
async fn unchanged_values_rejected_inputs_and_default_reset_do_not_write() {
    let dir = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(dir.path());
    let (_kernel, prefs) = open(store.clone()).await.unwrap();
    let qq = scope("qq", "s", "u");
    prefs.update(&qq, SegmentChange::Reset).unwrap();
    assert_eq!(store.writes(), 0);
    prefs.update(&qq, SegmentChange::Enabled(true)).unwrap();
    prefs.update(&qq, SegmentChange::Enabled(true)).unwrap();
    assert_eq!(store.writes(), 1);
    for bad in [
        SegmentChange::MaxSegments(1),
        SegmentChange::MaxSegments(9),
        SegmentChange::PausePercent(201),
    ] {
        assert_eq!(
            prefs.update(&qq, bad),
            Err(SegmentPreferenceError::InvalidInput)
        );
    }
    assert_eq!(
        prefs.update(&scope("qq", " s", "u"), SegmentChange::Enabled(false)),
        Err(SegmentPreferenceError::InvalidInput)
    );
    assert_eq!(store.writes(), 1);
    prefs.update(&qq, SegmentChange::Reset).unwrap();
    assert_eq!(store.writes(), 2);
    let document: Value = serde_json::from_slice(&store.bytes().unwrap()).unwrap();
    assert_eq!(document, json!({"version": 1, "scopes": []}));
}

#[tokio::test]
async fn capacity_rejects_new_scope_without_eviction_and_reset_frees_a_slot() {
    let dir = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(dir.path());
    let (_kernel, prefs) = open(store.clone()).await.unwrap();
    for index in 0..MAX_PREFERENCE_SCOPES {
        prefs
            .update(
                &scope("qq", &format!("s-{index}"), "u"),
                SegmentChange::Enabled(false),
            )
            .unwrap();
    }
    let before = store.bytes();
    assert_eq!(
        prefs.update(&scope("qq", "new", "u"), SegmentChange::Enabled(false)),
        Err(SegmentPreferenceError::LimitReached)
    );
    assert_eq!(store.bytes(), before);
    prefs
        .update(&scope("qq", "s-0", "u"), SegmentChange::PausePercent(0))
        .unwrap();
    prefs
        .update(&scope("qq", "s-1", "u"), SegmentChange::Reset)
        .unwrap();
    prefs
        .update(&scope("qq", "new", "u"), SegmentChange::Enabled(false))
        .unwrap();
}

#[tokio::test]
async fn failed_or_unacknowledged_writes_close_the_store_until_reopened() {
    let dir = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(dir.path());
    let qq = scope("qq", "s", "u");
    {
        let (kernel, prefs) = open(store.clone()).await.unwrap();
        prefs.update(&qq, SegmentChange::Enabled(false)).unwrap();
        store.fail_write.store(true, Ordering::SeqCst);
        assert_eq!(
            prefs.update(&qq, SegmentChange::Enabled(true)),
            Err(SegmentPreferenceError::Storage)
        );
        let writes = store.writes();
        assert_eq!(prefs.get(&qq), Err(SegmentPreferenceError::Unavailable));
        assert_eq!(
            prefs.update(&qq, SegmentChange::Reset),
            Err(SegmentPreferenceError::Unavailable)
        );
        assert_eq!(store.writes(), writes);
        store.fail_write.store(false, Ordering::SeqCst);
        kernel.stop_all().await.unwrap();
    }
    let (kernel, prefs) = open(store.clone()).await.unwrap();
    assert_eq!(prefs.get(&qq).unwrap().enabled, Some(false));
    // 已经提交但确认丢失：本进程关闭，重新打开读到实际已提交的新值。
    store.fail_after_write.store(true, Ordering::SeqCst);
    assert_eq!(
        prefs.update(&qq, SegmentChange::Enabled(true)),
        Err(SegmentPreferenceError::Storage)
    );
    assert_eq!(prefs.get(&qq), Err(SegmentPreferenceError::Unavailable));
    store.fail_after_write.store(false, Ordering::SeqCst);
    kernel.stop_all().await.unwrap();
    let (_kernel, prefs) = open(store.clone()).await.unwrap();
    assert_eq!(prefs.get(&qq).unwrap().enabled, Some(true));
}

#[tokio::test]
async fn corrupt_or_unknown_state_refuses_start_without_clearing() {
    let valid = r#"{"scope":{"channel":"qq","session_id":"s","user_id":"u"},"enabled":false,"max_segments":null,"pause_percent":null}"#;
    let cases = [
        ("not json".to_string(), "损坏"),
        (
            format!(r#"{{"version":1,"scopes":[{valid}],"extra":1}}"#),
            "损坏",
        ),
        (
            format!(
                r#"{{"version":1,"scopes":[{}]}}"#,
                valid.replace("null}", "null,\"x\":1}")
            ),
            "损坏",
        ),
        (
            format!(r#"{{"version":1,"version":1,"scopes":[{valid}]}}"#),
            "损坏",
        ),
        (
            format!(r#"{{"version":2,"scopes":[{valid}]}}"#),
            "版本不兼容",
        ),
        (
            format!(r#"{{"version":1,"scopes":[{valid},{valid}]}}"#),
            "损坏",
        ),
        (
            format!(
                r#"{{"version":1,"scopes":[{}]}}"#,
                valid.replace("\"s\"", "\" s\"")
            ),
            "损坏",
        ),
        (
            format!(
                r#"{{"version":1,"scopes":[{}]}}"#,
                valid.replace("\"max_segments\":null", "\"max_segments\":1")
            ),
            "损坏",
        ),
        (
            format!(
                r#"{{"version":1,"scopes":[{}]}}"#,
                valid.replace("\"pause_percent\":null", "\"pause_percent\":201")
            ),
            "损坏",
        ),
        (
            format!(
                r#"{{"version":1,"scopes":[{}]}}"#,
                valid.replace("\"enabled\":false", "\"enabled\":null")
            ),
            "损坏",
        ),
        (" ".repeat(1_048_577), "损坏"),
    ];
    for (bytes, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        let store = RecordingStore::open(dir.path());
        store.put(bytes.as_bytes());
        let error = match open(store.clone()).await {
            Ok(_) => panic!("{} must be rejected", &bytes[..bytes.len().min(80)]),
            Err(error) => error.to_string(),
        };
        assert!(error.contains(expected), "{error}");
        assert_eq!(store.bytes().unwrap(), bytes.as_bytes());
        assert_eq!(store.writes(), 0);
    }
    let mut many = Vec::new();
    for index in 0..=MAX_PREFERENCE_SCOPES {
        many.push(valid.replace("\"s\"", &format!("\"s-{index}\"")));
    }
    let dir = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(dir.path());
    let bytes = format!(r#"{{"version":1,"scopes":[{}]}}"#, many.join(","));
    store.put(bytes.as_bytes());
    assert!(open(store.clone()).await.is_err());
    assert_eq!(store.bytes().unwrap(), bytes.as_bytes());
}
