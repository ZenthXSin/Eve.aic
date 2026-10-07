use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginId, PluginManifest, StateStore,
};
use std::fs;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

fn pid(id: &str) -> PluginId {
    PluginId::new(id).unwrap()
}

#[test]
fn namespaces_and_arbitrary_bytes_survive_reopening() {
    let directory = tempfile::tempdir().unwrap();
    let store = FileStateStore::open(directory.path()).unwrap();
    let bytes: Vec<u8> = (0..=255).collect();
    store
        .set(&pid("one"), "进度/文件名".into(), bytes.clone())
        .unwrap();
    store
        .set(&pid("two"), "进度/文件名".into(), b"other".to_vec())
        .unwrap();
    store
        .set(&pid("one"), "empty-value".into(), Vec::new())
        .unwrap();
    assert_eq!(store.get(&pid("missing"), "进度/文件名").unwrap(), None);
    assert_eq!(store.get(&pid("one"), "missing").unwrap(), None);
    drop(store);

    let store = FileStateStore::open(directory.path()).unwrap();
    assert_eq!(store.get(&pid("one"), "进度/文件名").unwrap(), Some(bytes));
    assert_eq!(
        store.get(&pid("two"), "进度/文件名").unwrap(),
        Some(b"other".to_vec())
    );
    assert_eq!(
        store.get(&pid("one"), "empty-value").unwrap(),
        Some(Vec::new())
    );
}

#[test]
fn opening_creates_missing_directories_and_reads_the_versioned_format() {
    let directory = tempfile::tempdir().unwrap();
    let nested = directory.path().join("missing").join("state");
    let store = FileStateStore::open(&nested).unwrap();
    store
        .set(&pid("owner"), "key".into(), vec![0, 255])
        .unwrap();
    drop(store);

    let document: serde_json::Value =
        serde_json::from_slice(&fs::read(nested.join("state.json")).unwrap()).unwrap();
    assert_eq!(document["version"], 1);
    assert_eq!(
        document["entries"]["owner"]["key"],
        serde_json::json!([0, 255])
    );

    fs::write(
        nested.join("state.json"),
        br#"{"version":1,"entries":{"owner":{"imported":[42]}}}"#,
    )
    .unwrap();
    let store = FileStateStore::open(&nested).unwrap();
    assert_eq!(
        store.get(&pid("owner"), "imported").unwrap(),
        Some(vec![42])
    );
}

#[test]
fn directory_lock_is_exclusive_and_released_on_drop() {
    let directory = tempfile::tempdir().unwrap();
    let store = FileStateStore::open(directory.path()).unwrap();
    assert!(matches!(
        FileStateStore::open(directory.path()),
        Err(PluginError::State(_))
    ));
    store.set(&pid("owner"), "saved".into(), vec![1]).unwrap();
    drop(store);

    let reopened = FileStateStore::open(directory.path()).unwrap();
    assert_eq!(reopened.get(&pid("owner"), "saved").unwrap(), Some(vec![1]));
    assert!(matches!(
        FileStateStore::open(directory.path()),
        Err(PluginError::State(_))
    ));
}

#[test]
fn corrupted_or_unsupported_documents_are_not_overwritten() {
    for original in [
        b"not-json".as_slice(),
        br#"{"version":2,"entries":{}}"#,
        br#"{"version":1,"entries":{"owner":{"key":[256]}}}"#,
        br#"{"version":1}"#,
        br#"{"version":1,"entries":{"owner":{},"owner":{}}}"#,
        br#"{"version":1,"entries":{"owner":{"key":[1],"key":[2]}}}"#,
        br#"{"version":1,"entries":{"":{"key":[1]}}}"#,
        br#"{"version":1,"entries":{},"unexpected":true}"#,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        fs::write(&path, original).unwrap();
        assert!(matches!(
            FileStateStore::open(directory.path()),
            Err(PluginError::State(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), original);

        // 失败的打开也必须释放目录锁，修好数据后无需重启进程。
        fs::write(&path, br#"{"version":1,"entries":{}}"#).unwrap();
        assert!(FileStateStore::open(directory.path()).is_ok());
    }
}

#[test]
fn filesystem_errors_are_reported_instead_of_becoming_empty_state() {
    let directory = tempfile::tempdir().unwrap();
    let not_directory = directory.path().join("regular-file");
    fs::write(&not_directory, b"keep").unwrap();
    assert!(matches!(
        FileStateStore::open(&not_directory),
        Err(PluginError::State(_))
    ));
    assert_eq!(fs::read(&not_directory).unwrap(), b"keep");

    for name in ["state.json", "state.lock"] {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join(name)).unwrap();
        assert!(matches!(
            FileStateStore::open(directory.path()),
            Err(PluginError::State(_))
        ));
    }
}

#[cfg(any(unix, windows))]
#[test]
fn valid_and_dangling_state_symlinks_are_rejected_without_modifying_links_or_targets() {
    #[cfg(unix)]
    use std::os::unix::fs::symlink as create_symlink;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_file as create_symlink;

    const ORIGINAL: &[u8] = br#"{"version":1,"entries":{"owner":{"key":[42]}}}"#;
    for target_exists in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let state_directory = directory.path().join("state");
        fs::create_dir(&state_directory).unwrap();
        let target = directory.path().join("target.json");
        if target_exists {
            fs::write(&target, ORIGINAL).unwrap();
        }
        let link = state_directory.join("state.json");
        create_symlink(&target, &link)
            .expect("必须实际创建符号链接；权限不足时应明确失败，不能跳过此验收");
        let destination = fs::read_link(&link).unwrap();

        assert!(
            matches!(
                FileStateStore::open(&state_directory),
                Err(PluginError::State(_))
            ),
            "state.json 符号链接必须拒绝打开，目标存在：{target_exists}"
        );
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(&link).unwrap(), destination);
        if target_exists {
            assert_eq!(fs::read(&target).unwrap(), ORIGINAL);
        } else {
            assert_eq!(
                fs::symlink_metadata(&target).unwrap_err().kind(),
                std::io::ErrorKind::NotFound
            );
        }
    }
}

#[test]
fn concurrent_writes_preserve_every_key_in_memory_and_on_disk() {
    const WRITERS: usize = 8;
    const KEYS: usize = 8;
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(FileStateStore::open(directory.path()).unwrap());
    let barrier = Arc::new(Barrier::new(WRITERS));
    std::thread::scope(|threads| {
        for worker in 0..WRITERS {
            let store = store.clone();
            let barrier = barrier.clone();
            threads.spawn(move || {
                barrier.wait();
                for key in 0..KEYS {
                    store
                        .set(
                            &pid("shared"),
                            format!("{worker}/{key}"),
                            vec![worker as u8, key as u8],
                        )
                        .unwrap();
                }
            });
        }
    });
    for worker in 0..WRITERS {
        for key in 0..KEYS {
            assert_eq!(
                store
                    .get(&pid("shared"), &format!("{worker}/{key}"))
                    .unwrap(),
                Some(vec![worker as u8, key as u8])
            );
        }
    }
    drop(store);

    let store = FileStateStore::open(directory.path()).unwrap();
    for worker in 0..WRITERS {
        for key in 0..KEYS {
            assert_eq!(
                store
                    .get(&pid("shared"), &format!("{worker}/{key}"))
                    .unwrap(),
                Some(vec![worker as u8, key as u8])
            );
        }
    }
}

#[test]
fn failed_atomic_replacement_preserves_memory_and_previous_document() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let saved_path = directory.path().join("previous.json");
    let store = FileStateStore::open(directory.path()).unwrap();
    store.set(&pid("owner"), "key".into(), vec![1]).unwrap();
    let original = fs::read(&path).unwrap();

    // 同名目录使原子替换可靠失败，不依赖当前账户是否有管理员权限。
    fs::rename(&path, &saved_path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(matches!(
        store.set(&pid("owner"), "key".into(), vec![2]),
        Err(PluginError::State(_))
    ));
    assert!(matches!(
        store.set(&pid("owner"), "new-key".into(), vec![3]),
        Err(PluginError::State(_))
    ));
    assert_eq!(store.get(&pid("owner"), "key").unwrap(), Some(vec![1]));
    assert_eq!(store.get(&pid("owner"), "new-key").unwrap(), None);
    assert_eq!(fs::read(&saved_path).unwrap(), original);

    fs::remove_dir(&path).unwrap();
    fs::rename(&saved_path, &path).unwrap();
    store
        .set(&pid("owner"), "after-repair".into(), vec![4])
        .unwrap();
    drop(store);
    let store = FileStateStore::open(directory.path()).unwrap();
    assert_eq!(store.get(&pid("owner"), "key").unwrap(), Some(vec![1]));
    assert_eq!(store.get(&pid("owner"), "new-key").unwrap(), None);
    assert_eq!(
        store.get(&pid("owner"), "after-repair").unwrap(),
        Some(vec![4])
    );
}

#[cfg(windows)]
#[test]
fn denied_windows_replacement_preserves_the_existing_file_and_memory() {
    use std::os::windows::fs::OpenOptionsExt;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let store = FileStateStore::open(directory.path()).unwrap();
    store.set(&pid("owner"), "key".into(), vec![1]).unwrap();
    let original = fs::read(&path).unwrap();

    // 允许读写但不共享 DELETE 权限，可靠阻止替换已有文件。
    let held_file = fs::OpenOptions::new()
        .read(true)
        .share_mode(0x0000_0001 | 0x0000_0002)
        .open(&path)
        .unwrap();
    assert!(matches!(
        store.set(&pid("owner"), "key".into(), vec![2]),
        Err(PluginError::State(_))
    ));
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(store.get(&pid("owner"), "key").unwrap(), Some(vec![1]));
    drop(held_file);

    store.set(&pid("owner"), "key".into(), vec![3]).unwrap();
    drop(store);
    let store = FileStateStore::open(directory.path()).unwrap();
    assert_eq!(store.get(&pid("owner"), "key").unwrap(), Some(vec![3]));
}

#[cfg(windows)]
#[test]
fn briefly_held_windows_snapshot_is_replaced_after_the_reader_closes() {
    use std::os::windows::fs::OpenOptionsExt;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    let store = FileStateStore::open(directory.path()).unwrap();
    store.set(&pid("owner"), "key".into(), vec![1]).unwrap();

    // 模拟其他进程短暂读取快照：句柄在退避窗口内关闭，提交应在重试后成功。
    let held_file = fs::OpenOptions::new()
        .read(true)
        .share_mode(0x0000_0001 | 0x0000_0002)
        .open(&path)
        .unwrap();
    let reader = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        drop(held_file);
    });
    store.set(&pid("owner"), "key".into(), vec![2]).unwrap();
    reader.join().unwrap();
    assert_eq!(store.get(&pid("owner"), "key").unwrap(), Some(vec![2]));
    let leftovers: Vec<_> = fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "重试不得遗留临时文件：{leftovers:?}");
    drop(store);
    let store = FileStateStore::open(directory.path()).unwrap();
    assert_eq!(store.get(&pid("owner"), "key").unwrap(), Some(vec![2]));
}

struct RecoveryPlugin {
    manifest: PluginManifest,
    seen: Arc<Mutex<Vec<Option<u8>>>>,
}

impl Plugin for RecoveryPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let previous = ctx.state_get("启动次数")?;
            let count = previous.as_ref().map_or(0, |value| value[0]);
            self.seen
                .lock()
                .unwrap()
                .push(previous.map(|value| value[0]));
            ctx.state_set("启动次数", vec![count + 1])?;
            Ok(None)
        })
    }
}

#[tokio::test]
async fn new_kernel_instances_restore_state_through_the_same_plugin_api() {
    let directory = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    for _ in 0..2 {
        let kernel = Kernel::with_services(KernelServices {
            state: Arc::new(FileStateStore::open(directory.path()).unwrap()),
            ..Default::default()
        });
        kernel
            .register(Box::new(RecoveryPlugin {
                manifest: PluginManifest::new("recovery", "0.1.0").unwrap(),
                seen: seen.clone(),
            }))
            .unwrap();
        kernel.start_all().await.unwrap();
        kernel.stop_all().await.unwrap();
        drop(kernel);
    }
    assert_eq!(*seen.lock().unwrap(), vec![None, Some(1)]);
    let store = FileStateStore::open(directory.path()).unwrap();
    assert_eq!(
        store.get(&pid("recovery"), "启动次数").unwrap(),
        Some(vec![2])
    );
}

struct TestChild(Child);

impl Drop for TestChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run_child(directory: &Path, action: &str) {
    let mut child = TestChild(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "persistence_process_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("EVE_TEST_PERSISTENCE_DIRECTORY", directory)
            .env("EVE_TEST_PERSISTENCE_ACTION", action)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success(), "状态恢复子进程失败：{action}，{status}");
            return;
        }
        assert!(Instant::now() < deadline, "状态恢复子进程超时：{action}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn independent_processes_persist_restore_and_respect_the_directory_lock() {
    let directory = tempfile::tempdir().unwrap();
    run_child(directory.path(), "write");
    let store = FileStateStore::open(directory.path()).unwrap();
    assert_eq!(
        store.get(&pid("process"), "恢复位置").unwrap(),
        Some(vec![0, 127, 255])
    );
    run_child(directory.path(), "locked");
    drop(store);
    run_child(directory.path(), "read");
}

#[test]
#[ignore = "只由独立进程验收调用，数据目录由父测试提供"]
fn persistence_process_helper() {
    let directory = std::env::var_os("EVE_TEST_PERSISTENCE_DIRECTORY").unwrap();
    let action = std::env::var("EVE_TEST_PERSISTENCE_ACTION").unwrap();
    if action == "locked" {
        assert!(matches!(
            FileStateStore::open(&directory),
            Err(PluginError::State(_))
        ));
        return;
    }
    let store = FileStateStore::open(&directory).unwrap();
    match action.as_str() {
        "write" => {
            assert_eq!(store.get(&pid("process"), "恢复位置").unwrap(), None);
            store
                .set(&pid("process"), "恢复位置".into(), vec![0, 127, 255])
                .unwrap();
        }
        "read" => assert_eq!(
            store.get(&pid("process"), "恢复位置").unwrap(),
            Some(vec![0, 127, 255])
        ),
        other => panic!("未知子进程操作：{other}"),
    }
}
