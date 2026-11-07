use super::*;
use eve_kernel::backends::MemoryStateStore;
use eve_qqbot_plugin::QqBeforeIngress;
use serde_json::{Value, json};
use std::sync::Mutex;

struct Fixture {
    _directory: tempfile::TempDir,
    request_path: PathBuf,
    request: Value,
    state: StartupState,
}

impl Fixture {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let state_directory = root.join("state");
        fs::create_dir(&state_directory).unwrap();
        let runtime_path = root.join("runtime.py");
        fs::write(&runtime_path, b"# owned test runtime\n").unwrap();
        let request_path = root.join("request.json");
        let request = json!({
            "format_version": 1,
            "nonce": "a".repeat(64),
            "source_revision": "b".repeat(40),
            "binary_sha256": file_sha(&std::env::current_exe().unwrap(), MAX_BINARY_BYTES).unwrap(),
            "runtime_path": runtime_path,
            "runtime_sha256": file_sha(&runtime_path, MAX_CONTROL_BYTES).unwrap(),
            "state_directory": state_directory,
            "checkpoint_path": root.join("checkpoint.json"),
            "acknowledgement_path": root.join("ack.json"),
            "timeout_ms": 1000,
        });
        let state = StartupState {
            state_directory,
            automatic_capabilities: true,
            ledgers: StartupLedgers::read(&MemoryStateStore::default()).unwrap(),
        };
        let result = Self {
            _directory: directory,
            request_path,
            request,
            state,
        };
        result.save();
        result
    }

    fn save(&self) {
        fs::write(
            &self.request_path,
            serde_json::to_vec(&self.request).unwrap(),
        )
        .unwrap();
    }

    fn checkpoint(&self) -> PathBuf {
        PathBuf::from(self.request["checkpoint_path"].as_str().unwrap())
    }

    fn ack(&self) -> PathBuf {
        PathBuf::from(self.request["acknowledgement_path"].as_str().unwrap())
    }

    fn sink(&self) -> FileStartupCheckpoint {
        FileStartupCheckpoint::open(&self.request_path).unwrap()
    }
}

async fn acknowledge(path: PathBuf, ack_path: PathBuf, change: impl FnOnce(&mut Value)) -> Value {
    let bytes = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if path.exists() {
                break fs::read(&path).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let checkpoint: Value = serde_json::from_slice(&bytes).unwrap();
    let mut ack = json!({
        "format_version": 1,
        "nonce": checkpoint["nonce"],
        "pid": checkpoint["pid"],
        "process_start_ticks": checkpoint["process_start_ticks"],
        "checkpoint_sha256": sha(&bytes),
        "preservation_sha256": "c".repeat(64),
        "accepted": true,
    });
    change(&mut ack);
    publish(&ack_path, &serde_json::to_vec(&ack).unwrap()).unwrap();
    checkpoint
}

#[test]
fn summaries_distinguish_missing_from_empty_and_keep_persisted_bytes() {
    let state = MemoryStateStore::default();
    let first = StartupLedgers::read(&state).unwrap();
    assert!(!first.tasks.present && !first.phrases.present && !first.receipts.present);
    let bytes = b"{\n  \"version\":10,\"entries\":[]\n}".to_vec();
    state
        .set(
            &PluginId::new(eve_qqbot_plugin::QQBOT_PLUGIN_ID).unwrap(),
            "receipts.v1".into(),
            bytes.clone(),
        )
        .unwrap();
    let saved = StartupLedgers::read(&state).unwrap().receipts;
    assert_eq!(saved.version, Some(10));
    assert_eq!(saved.bytes, bytes.len() as u64);
    assert_eq!(saved.sha256, Some(sha(&bytes)));
    assert!(saved.present);
    assert_ne!(saved, first.receipts);
}

#[tokio::test]
async fn checkpoint_binds_actual_executable_process_and_exact_request() {
    let fixture = Fixture::new();
    let sink = fixture.sink();
    let (result, checkpoint) = tokio::join!(
        sink.checkpoint(fixture.state.clone()),
        acknowledge(fixture.checkpoint(), fixture.ack(), |_| {}),
    );
    result.unwrap();
    assert_eq!(
        checkpoint["request_sha256"],
        sha(&fs::read(&fixture.request_path).unwrap())
    );
    assert_eq!(
        checkpoint["binary_sha256"],
        fixture.request["binary_sha256"]
    );
    assert_eq!(
        checkpoint["runtime_sha256"],
        fixture.request["runtime_sha256"]
    );
    assert_eq!(checkpoint["pid"], std::process::id());
    assert_eq!(
        checkpoint["process_start_ticks"],
        process_start_ticks().unwrap()
    );
    assert_eq!(
        checkpoint["ledgers"],
        serde_json::to_value(&fixture.state.ledgers).unwrap()
    );
    assert!(FileStartupCheckpoint::open(&fixture.request_path).is_err());
    assert!(sink.checkpoint(fixture.state.clone()).await.is_err());
}

#[tokio::test]
async fn no_acknowledgement_fails_within_the_explicit_deadline() {
    let fixture = Fixture::new();
    let sink = fixture.sink();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        sink.checkpoint(fixture.state.clone()),
    )
    .await
    .unwrap();
    assert!(result.unwrap_err().to_string().contains("超时"));
    assert!(fixture.checkpoint().is_file());
    assert!(!fixture.ack().exists());
}

#[tokio::test]
async fn stale_or_rejected_confirmation_never_opens_the_barrier() {
    for field in [
        "nonce",
        "pid",
        "process_start_ticks",
        "checkpoint_sha256",
        "accepted",
        "preservation_sha256",
    ] {
        let fixture = Fixture::new();
        let sink = fixture.sink();
        let (result, _) = tokio::join!(
            sink.checkpoint(fixture.state.clone()),
            acknowledge(fixture.checkpoint(), fixture.ack(), |ack| {
                ack[field] = match field {
                    "pid" | "process_start_ticks" => json!(0),
                    "accepted" => json!(false),
                    _ => json!("wrong"),
                };
            }),
        );
        assert!(result.is_err(), "{field}");
    }
}

#[tokio::test]
async fn wrong_identity_or_changed_runtime_has_no_successful_checkpoint() {
    for field in ["binary_sha256", "runtime_sha256", "state_directory"] {
        let mut fixture = Fixture::new();
        if field == "state_directory" {
            let other = fixture.request_path.parent().unwrap().join("other");
            fs::create_dir(&other).unwrap();
            fixture.request[field] = json!(other);
        } else {
            fixture.request[field] = json!("0".repeat(64));
        }
        fixture.save();
        assert!(
            fixture
                .sink()
                .checkpoint(fixture.state.clone())
                .await
                .is_err(),
            "{field}"
        );
        assert!(!fixture.checkpoint().exists());
    }
    let fixture = Fixture::new();
    let sink = fixture.sink();
    let runtime = PathBuf::from(fixture.request["runtime_path"].as_str().unwrap());
    let (result, _) = tokio::join!(
        sink.checkpoint(fixture.state.clone()),
        acknowledge(fixture.checkpoint(), fixture.ack(), |_| {
            fs::write(&runtime, b"changed after checkpoint").unwrap();
        }),
    );
    assert!(result.is_err());
}

#[test]
fn duplicate_fields_old_outputs_and_reused_claim_are_rejected() {
    let fixture = Fixture::new();
    let raw = serde_json::to_string(&fixture.request).unwrap();
    fs::write(
        &fixture.request_path,
        format!("{{\"format_version\":1,{}", &raw[1..]),
    )
    .unwrap();
    assert!(FileStartupCheckpoint::open(&fixture.request_path).is_err());
    fixture.save();
    fs::write(fixture.ack(), b"{}").unwrap();
    assert!(FileStartupCheckpoint::open(&fixture.request_path).is_err());
    fs::remove_file(fixture.ack()).unwrap();
    let first = fixture.sink();
    assert!(FileStartupCheckpoint::open(&fixture.request_path).is_err());
    drop(first);
}

#[cfg(unix)]
#[test]
fn nonprivate_request_directory_and_symlink_control_files_are_rejected() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let fixture = Fixture::new();
    let directory = fixture.request_path.parent().unwrap();
    fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(FileStartupCheckpoint::open(&fixture.request_path).is_err());
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    let request_link = directory.join("link.json");
    symlink(&fixture.request_path, &request_link).unwrap();
    assert!(FileStartupCheckpoint::open(&request_link).is_err());
    symlink(&fixture.request_path, fixture.ack()).unwrap();
    assert!(FileStartupCheckpoint::open(&fixture.request_path).is_err());
}

#[derive(Debug)]
struct RecordingSink(Mutex<Vec<StartupState>>);
impl StartupCheckpointSink for RecordingSink {
    fn checkpoint(&self, state: StartupState) -> PluginFuture<'_, ()> {
        Box::pin(async move {
            self.0.lock().unwrap().push(state);
            Ok(())
        })
    }
}

#[tokio::test]
async fn injected_sink_receives_the_actual_store_without_file_handshake() {
    let directory = tempfile::tempdir().unwrap();
    let state = Arc::new(MemoryStateStore::default());
    let sink = Arc::new(RecordingSink(Mutex::new(vec![])));
    let boundary = Boundary::new(state, directory.path(), false, sink.clone()).unwrap();
    boundary.before_ingress().await.unwrap();
    let seen = sink.0.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(!seen[0].automatic_capabilities);
    assert_eq!(
        seen[0].state_directory,
        fs::canonicalize(directory.path()).unwrap()
    );
}
