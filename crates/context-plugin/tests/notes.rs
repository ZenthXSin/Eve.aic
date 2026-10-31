use eve_context_api::*;
use eve_context_plugin::{CONTEXT_INDEX_KEY, ContextController, ContextPlugin};
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Store {
    files: FileStateStore,
    fault: AtomicUsize,
    writes: AtomicUsize,
}
impl StateStore for Store {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        let value = self.files.get(namespace, key)?;
        Ok(value)
    }
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        let fault = self.fault.load(Ordering::SeqCst);
        if fault == 3 && key == CONTEXT_INDEX_KEY {
            return Err(PluginError::State("private-store-error".into()));
        }
        let after = fault == 4 && key == CONTEXT_INDEX_KEY;
        self.files.set(namespace, key, value)?;
        if after {
            return Err(PluginError::State("private-store-error".into()));
        }
        Ok(())
    }
}
struct Archive {
    files: eve_context_plugin::FileContextArchive,
    store: Arc<Store>,
    missing: std::sync::Mutex<Option<String>>,
}
impl ContextArchiveStore for Archive {
    fn put(&self, key: &str, bytes: &[u8]) -> ContextResult<()> {
        self.store.writes.fetch_add(1, Ordering::SeqCst);
        let fault = self.store.fault.load(Ordering::SeqCst);
        if fault == 1 {
            return Err(ContextError::Storage);
        }
        self.files.put(key, bytes)?;
        if fault == 2 {
            return Err(ContextError::Storage);
        }
        Ok(())
    }
    fn get(&self, key: &str) -> ContextResult<Vec<u8>> {
        if self.missing.lock().unwrap().as_deref() == Some(key) {
            return Err(ContextError::CorruptState);
        }
        if self.store.fault.load(Ordering::SeqCst) == 5 {
            return Err(ContextError::Storage);
        }
        self.files.get(key)
    }
}
struct World {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    archive: Arc<Archive>,
    archive_dir: std::path::PathBuf,
}
impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store {
            files: FileStateStore::open(dir.path()).unwrap(),
            fault: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
        });
        let archive_dir = dir.path().join("bodies");
        let archive = Arc::new(Archive {
            files: eve_context_plugin::FileContextArchive::open(&archive_dir).unwrap(),
            store: store.clone(),
            missing: std::sync::Mutex::new(None),
        });
        Self {
            _dir: dir,
            store,
            archive,
            archive_dir,
        }
    }
    async fn open(&self) -> PluginResult<(Kernel, ContextController)> {
        let kernel = Kernel::with_services(KernelServices {
            state: self.store.clone(),
            ..KernelServices::default()
        });
        let plugin = ContextPlugin::new(self.archive.clone())?;
        let admin = plugin.controller();
        kernel.register(Box::new(plugin))?;
        kernel.start_all().await?;
        Ok((kernel, admin))
    }
    fn raw(&self, key: &str) -> Option<Vec<u8>> {
        if let Some(hash) = key
            .strip_prefix("body-")
            .and_then(|key| key.strip_suffix(".v1"))
        {
            return Some(std::fs::read(self.archive_dir.join(format!("{hash}.json"))).unwrap());
        }
        self.store
            .files
            .get(&PluginId::new(CONTEXT_PLUGIN_ID).unwrap(), key)
            .unwrap()
    }
    fn set(&self, key: &str, bytes: Vec<u8>) {
        if let Some(hash) = key
            .strip_prefix("body-")
            .and_then(|key| key.strip_suffix(".v1"))
        {
            std::fs::write(self.archive_dir.join(format!("{hash}.json")), bytes).unwrap();
            return;
        }
        self.store
            .files
            .set(
                &PluginId::new(CONTEXT_PLUGIN_ID).unwrap(),
                key.into(),
                bytes,
            )
            .unwrap();
    }
}
fn owner() -> ContextOwner {
    ContextOwner {
        channel: "qq".into(),
        session_id: "群甲".into(),
        user_id: "用户甲".into(),
    }
}

#[tokio::test]
async fn preview_exact_injection_cas_rollback_and_restart_preserve_all_versions() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let scope = owner();
    assert_eq!(admin.inspect(&scope).unwrap().revision, 0);
    let preview = admin
        .preview(&scope, "下一步：编译后独立读回验证\n未发送文件")
        .unwrap();
    assert!(preview.changed);
    assert!(world.raw(CONTEXT_INDEX_KEY).is_none());
    assert_eq!(world.store.writes.load(Ordering::SeqCst), 0);
    let v1 = admin
        .update(&scope, preview.base_revision, &preview.proposed.text)
        .unwrap();
    assert_eq!(v1.rendered().unwrap(), preview.rendered);
    let writes = world.store.writes.load(Ordering::SeqCst);
    assert_eq!(admin.update(&scope, 1, &v1.text).unwrap(), v1);
    assert!(!admin.preview(&scope, &v1.text).unwrap().changed);
    assert_eq!(world.store.writes.load(Ordering::SeqCst), writes);
    assert_eq!(
        admin.update(&scope, 0, "过时修改"),
        Err(ContextError::StaleRevision)
    );
    let v2 = admin.update(&scope, 1, "已验证产物，等待用户反馈").unwrap();
    let v3 = admin.rollback(&scope, 2, 1).unwrap();
    assert_eq!(v3.revision, 3);
    assert_eq!(v3.text, v1.text);
    assert_eq!(
        v3.version.as_ref().unwrap().change,
        ContextChange::Rollback { target_revision: 1 }
    );
    assert_eq!(admin.read_version(&scope, 2).unwrap(), v2);
    assert_eq!(
        admin.rollback(&scope, 2, 0),
        Err(ContextError::StaleRevision)
    );
    assert_eq!(admin.rollback(&scope, 3, 99), Err(ContextError::NotFound));
    assert_eq!(
        admin
            .versions(&scope, None, 2)
            .unwrap()
            .iter()
            .map(|v| v.revision)
            .collect::<Vec<_>>(),
        vec![3, 2]
    );
    assert_eq!(
        admin
            .versions(&scope, Some(2), 2)
            .unwrap()
            .iter()
            .map(|v| v.revision)
            .collect::<Vec<_>>(),
        vec![1]
    );
    kernel.stop_all().await.unwrap();
    assert_eq!(admin.inspect(&scope), Err(ContextError::Unavailable));
    let writes = world.store.writes.load(Ordering::SeqCst);
    let (kernel, admin) = world.open().await.unwrap();
    assert_eq!(admin.inspect(&scope).unwrap(), v3);
    assert_eq!(world.store.writes.load(Ordering::SeqCst), writes);
    let cleared = admin.rollback(&scope, 3, 0).unwrap();
    assert_eq!(cleared.revision, 4);
    assert_eq!(cleared.rendered().unwrap(), None);
    assert_eq!(admin.read_version(&scope, 1).unwrap(), v1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn every_identity_component_isolated_and_notes_are_cold_not_in_the_index() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let scope = owner();
    let text = format!("这是不能串会话的独特原文：{}", "文".repeat(4000));
    for number in 0..24 {
        admin
            .update(&scope, number, &format!("{number}{text}"))
            .unwrap();
    }
    for other in [
        ContextOwner {
            channel: "web".into(),
            ..scope.clone()
        },
        ContextOwner {
            session_id: "群乙".into(),
            ..scope.clone()
        },
        ContextOwner {
            user_id: "用户乙".into(),
            ..scope.clone()
        },
    ] {
        assert_eq!(admin.inspect(&other).unwrap().revision, 0);
        assert_eq!(admin.read_version(&other, 1), Err(ContextError::NotFound));
        assert!(admin.versions(&other, None, 16).unwrap().is_empty());
        admin.update(&other, 0, "其他范围").unwrap();
    }
    let raw = world.raw(CONTEXT_INDEX_KEY).unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains("独特原文"));
    assert!(raw.len() < 16 * 1024);
    let before = raw.clone();
    for number in 1..=24 {
        assert_eq!(
            admin.read_version(&scope, number).unwrap().text,
            format!("{}{text}", number - 1)
        );
    }
    assert_eq!(world.raw(CONTEXT_INDEX_KEY).unwrap(), before);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn all_commit_failure_points_close_and_reopen_actual_index_without_replaying() {
    for fault in 1..=5 {
        let world = World::new();
        let (kernel, admin) = world.open().await.unwrap();
        let scope = owner();
        let original = admin.update(&scope, 0, "原便笺").unwrap();
        // fault=5 在提交前独立读取当前正文就失败，也必须关闭，不能猜测可以继续。
        world.store.fault.store(fault, Ordering::SeqCst);
        assert_eq!(
            admin.update(&scope, 1, "候选便笺"),
            Err(ContextError::Storage),
            "fault={fault}"
        );
        assert_eq!(admin.inspect(&scope), Err(ContextError::Unavailable));
        assert_eq!(
            admin.update(&scope, 1, "另一个候选"),
            Err(ContextError::Unavailable)
        );
        kernel.stop_all().await.unwrap();
        world.store.fault.store(0, Ordering::SeqCst);
        let writes = world.store.writes.load(Ordering::SeqCst);
        let (kernel, admin) = world.open().await.unwrap();
        let restored = admin.inspect(&scope).unwrap();
        if fault == 4 {
            assert_eq!(restored.text, "候选便笺");
            assert_eq!(restored.revision, 2);
        } else {
            assert_eq!(restored, original);
        }
        assert_eq!(world.store.writes.load(Ordering::SeqCst), writes);
        kernel.stop_all().await.unwrap();
    }
}

#[tokio::test]
async fn corrupted_old_body_or_missing_body_closes_and_refuses_restart_without_clearing() {
    for corrupted in [true, false] {
        let world = World::new();
        let (kernel, admin) = world.open().await.unwrap();
        let scope = owner();
        let first = admin.update(&scope, 0, "旧正文").unwrap();
        admin.update(&scope, 1, "新正文").unwrap();
        let key = format!("body-{}.v1", first.version.unwrap().sha256);
        let original_body = world.raw(&key).unwrap();
        if corrupted {
            world.set(&key, b"corrupt".to_vec());
        } else {
            *world.archive.missing.lock().unwrap() = Some(
                key.strip_prefix("body-")
                    .unwrap()
                    .strip_suffix(".v1")
                    .unwrap()
                    .into(),
            );
        }
        let bytes = world.raw(CONTEXT_INDEX_KEY).unwrap();
        assert_eq!(
            admin.read_version(&scope, 1),
            Err(ContextError::CorruptState)
        );
        assert_eq!(admin.inspect(&scope), Err(ContextError::Unavailable));
        kernel.stop_all().await.unwrap();
        assert!(world.open().await.is_err());
        assert_eq!(world.raw(CONTEXT_INDEX_KEY).unwrap(), bytes);
        assert_eq!(
            world.raw(&key).unwrap(),
            if corrupted {
                b"corrupt".to_vec()
            } else {
                original_body
            }
        );
    }
}

#[tokio::test]
async fn full_metadata_index_keeps_every_version_readable_and_never_deletes_to_make_space() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let scope = owner();
    let first = admin.update(&scope, 0, "甲正文").unwrap().version.unwrap();
    let second = admin.update(&scope, 1, "乙正文").unwrap().version.unwrap();
    kernel.stop_all().await.unwrap();
    let mut raw: Value = serde_json::from_slice(&world.raw(CONTEXT_INDEX_KEY).unwrap()).unwrap();
    let record = raw["records"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap();
    record["versions"] = serde_json::to_value(
        (0..eve_context_plugin::MAX_CONTEXT_VERSIONS)
            .map(|i| {
                let mut version = if i % 2 == 0 {
                    first.clone()
                } else {
                    second.clone()
                };
                version.revision = i as u64 + 1;
                version
            })
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let full = serde_json::to_vec(&raw).unwrap();
    world.set(CONTEXT_INDEX_KEY, full.clone());
    let (kernel, admin) = world.open().await.unwrap();
    let max = eve_context_plugin::MAX_CONTEXT_VERSIONS as u64;
    assert_eq!(admin.inspect(&scope).unwrap().revision, max);
    assert_eq!(
        admin.update(&scope, max, "新正文"),
        Err(ContextError::LimitReached)
    );
    assert_eq!(
        admin.rollback(&scope, max, 1),
        Err(ContextError::LimitReached)
    );
    assert_eq!(admin.read_version(&scope, 1).unwrap().text, "甲正文");
    assert_eq!(admin.update(&scope, max, "乙正文").unwrap().revision, max);
    assert_eq!(world.raw(CONTEXT_INDEX_KEY).unwrap(), full);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn invalid_input_future_rollback_and_pagination_do_not_mutate_or_disable() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let scope = owner();
    for text in [" ".into(), "\0bad".into(), "文".repeat(MAX_NOTE_BYTES)] {
        assert_eq!(
            admin.preview(&scope, &text),
            Err(ContextError::InvalidInput)
        );
        assert_eq!(
            admin.update(&scope, 0, &text),
            Err(ContextError::InvalidInput)
        );
    }
    let mut bad = scope.clone();
    bad.user_id = "bad\n".into();
    assert_eq!(admin.inspect(&bad), Err(ContextError::InvalidInput));
    for (before, limit) in [(None, 0), (None, 65), (Some(0), 1)] {
        assert_eq!(
            admin.versions(&scope, before, limit),
            Err(ContextError::InvalidInput)
        );
    }
    assert_eq!(admin.rollback(&scope, 0, 1), Err(ContextError::NotFound));
    assert_eq!(admin.update(&scope, 0, "").unwrap().revision, 0);
    assert!(world.raw(CONTEXT_INDEX_KEY).is_none());
    let exact = "x".repeat(MAX_NOTE_BYTES);
    assert_eq!(admin.update(&scope, 0, &exact).unwrap().text, exact);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn duplicate_keys_unknown_fields_versions_and_rollback_forgery_refuse_start() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let scope = owner();
    admin.update(&scope, 0, "原文").unwrap();
    admin.update(&scope, 1, "后文").unwrap();
    let raw = world.raw(CONTEXT_INDEX_KEY).unwrap();
    kernel.stop_all().await.unwrap();
    let original: Value = serde_json::from_slice(&raw).unwrap();
    let mut mutations = vec![];
    for version in [json!(2), json!("1")] {
        let mut v = original.clone();
        v["format_version"] = version;
        mutations.push(serde_json::to_vec(&v).unwrap());
    }
    let mut v = original.clone();
    v["unknown"] = json!(true);
    mutations.push(serde_json::to_vec(&v).unwrap());
    let key = original["records"]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap()
        .clone();
    let mut v = original.clone();
    v["records"][&key]["versions"][0]["change"]["unknown"] = json!(true);
    mutations.push(serde_json::to_vec(&v).unwrap());
    let mut v = original.clone();
    v["records"][&key]["versions"][1]["change"] = json!({"kind":"Rollback","target_revision":1});
    mutations.push(serde_json::to_vec(&v).unwrap());
    let mut v = original.clone();
    v["records"][&key]["owner"]["user_id"] = json!("别人");
    mutations.push(serde_json::to_vec(&v).unwrap());
    mutations.push(
        String::from_utf8(raw.clone())
            .unwrap()
            .replacen(
                "\"format_version\":1",
                "\"format_version\":1,\"format_version\":1",
                1,
            )
            .into_bytes(),
    );
    for bytes in mutations {
        world.set(CONTEXT_INDEX_KEY, bytes.clone());
        assert!(world.open().await.is_err());
        assert_eq!(world.raw(CONTEXT_INDEX_KEY).unwrap(), bytes);
    }
    world.set(CONTEXT_INDEX_KEY, raw);
    let (kernel, admin) = world.open().await.unwrap();
    assert_eq!(admin.inspect(&scope).unwrap().text, "后文");
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn concurrent_same_revision_allows_one_writer_and_keeps_no_lost_updates() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(12));
    let handles: Vec<_> = (0..12)
        .map(|n| {
            let (admin, barrier) = (admin.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                admin.update(&owner(), 0, &format!("候选{n}"))
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| **r == Err(ContextError::StaleRevision))
            .count(),
        11
    );
    assert_eq!(admin.versions(&owner(), None, 64).unwrap().len(), 1);
    kernel.stop_all().await.unwrap();
}
