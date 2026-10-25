use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_plugin_api::*;
use eve_provision_api::*;
use eve_provision_plugin::*;
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 文件以 `PK` 开头才算可用；记录被调用的次数。
struct JarCheck(AtomicUsize);
impl ConditionVerifier for JarCheck {
    fn verify<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            if bytes.starts_with(b"PK") {
                Ok(format!("jar {} 字节", bytes.len()))
            } else {
                Err("不是 jar".into())
            }
        })
    }
}

fn spec() -> ConditionSpec {
    ConditionSpec {
        id: "sample-server".into(),
        description: "测试用服务端".into(),
        file_name: "server.jar".into(),
        max_bytes: 64,
    }
}

async fn start(
    state: Arc<dyn StateStore>,
    root: &Path,
    check: Arc<JarCheck>,
) -> PluginResult<(Kernel, ProvisionController)> {
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let plugin = ProvisionPlugin::new(root.to_path_buf(), vec![(spec(), check)])?;
    let controller = plugin.controller();
    kernel.register(Box::new(plugin))?;
    kernel
        .start(&PluginId::new(PROVISION_PLUGIN_ID).unwrap())
        .await?;
    Ok((kernel, controller))
}

fn sha(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn request(source: AcquireSource, expected: Option<String>, at: u64) -> AcquireRequest {
    AcquireRequest {
        condition: "sample-server".into(),
        source,
        expected_sha256: expected,
        reason: "实践验证需要服务端".into(),
        now_ms: at,
    }
}

fn local(dir: &Path, name: &str, bytes: &[u8]) -> AcquireSource {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    AcquireSource::LocalFile(path.to_string_lossy().into())
}

fn staging_empty(root: &Path) -> bool {
    std::fs::read_dir(root.join(".staging")).map_or(true, |mut d| d.next().is_none())
}

#[tokio::test]
async fn local_file_is_verified_then_enabled_and_survives_restart() {
    let root = tempfile::tempdir().unwrap();
    let sources = tempfile::tempdir().unwrap();
    let state: Arc<dyn StateStore> = Arc::new(MemoryStateStore::default());
    let check = Arc::new(JarCheck(AtomicUsize::new(0)));
    let (kernel, provider) = start(state.clone(), root.path(), check.clone())
        .await
        .unwrap();
    assert_eq!(provider.ready_path("sample-server").unwrap(), None);

    let good = b"PK\x03\x04good";
    let acquired = provider
        .acquire(request(
            local(sources.path(), "a.jar", good),
            Some(sha(good)),
            10,
        ))
        .await
        .unwrap();
    assert_eq!(acquired.sha256, sha(good));
    assert_eq!(acquired.bytes, good.len() as u64);
    assert_eq!(
        acquired.path,
        format!("sample-server/{}/server.jar", sha(good))
    );
    assert!(acquired.evidence.contains("jar"));
    let path = provider.ready_path("sample-server").unwrap().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), good);
    assert!(staging_empty(root.path()));
    assert_eq!(check.0.load(Ordering::SeqCst), 1);
    kernel.stop_all().await.unwrap();

    let (_kernel, provider) = start(state, root.path(), check).await.unwrap();
    assert_eq!(provider.ready_path("sample-server").unwrap(), Some(path));
    let view = &provider.conditions().unwrap()[0];
    assert_eq!(view.ready.as_ref().unwrap().reason, "实践验证需要服务端");
    assert!(view.attempts[0].ready);
}

#[tokio::test]
async fn mismatched_or_failed_candidates_are_discarded_and_keep_the_enabled_version() {
    let root = tempfile::tempdir().unwrap();
    let sources = tempfile::tempdir().unwrap();
    let state: Arc<dyn StateStore> = Arc::new(MemoryStateStore::default());
    let check = Arc::new(JarCheck(AtomicUsize::new(0)));
    let (_kernel, provider) = start(state, root.path(), check.clone()).await.unwrap();
    let good = b"PK-first";
    provider
        .acquire(request(local(sources.path(), "a.jar", good), None, 1))
        .await
        .unwrap();
    let enabled = provider.ready_path("sample-server").unwrap();

    let other = b"PK-second";
    let mismatch = provider
        .acquire(request(
            local(sources.path(), "b.jar", other),
            Some(sha(good)),
            2,
        ))
        .await;
    assert_eq!(
        mismatch,
        Err(ProvisionError::DigestMismatch { actual: sha(other) })
    );
    assert_eq!(check.0.load(Ordering::SeqCst), 1, "摘要不符不运行验证");

    let broken = provider
        .acquire(request(
            local(sources.path(), "c.jar", b"not a jar"),
            None,
            3,
        ))
        .await;
    assert!(matches!(broken, Err(ProvisionError::VerifyFailed(ref r)) if r.contains("不是 jar")));
    let too_large = provider
        .acquire(request(
            local(sources.path(), "d.jar", &[b'P'; 65]),
            None,
            4,
        ))
        .await;
    assert_eq!(too_large, Err(ProvisionError::TooLarge));
    let relative = provider
        .acquire(request(AcquireSource::LocalFile("x.jar".into()), None, 5))
        .await;
    assert!(matches!(relative, Err(ProvisionError::InvalidInput(_))));

    assert_eq!(
        provider.ready_path("sample-server").unwrap(),
        enabled,
        "失败不影响已启用版本"
    );
    assert!(staging_empty(root.path()));
    let attempts = &provider.conditions().unwrap()[0].attempts;
    assert_eq!(attempts.len(), 5);
    assert!(!attempts[0].ready && attempts[4].ready, "从新到旧");
    assert_eq!(
        provider
            .acquire(AcquireRequest {
                condition: "unknown".into(),
                ..request(AcquireSource::Url("https://a/b".into()), None, 6)
            })
            .await,
        Err(ProvisionError::UnknownCondition)
    );
}

/// 回环地址上的简易 HTTP 服务：`/redirect` 跳到 `/file`，`/file` 返回给定内容。
async fn serve(body: Vec<u8>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let body = body.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0; 4096];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buffer[..read]).to_string();
                let response = if head.starts_with("GET /redirect") {
                    b"HTTP/1.1 302 Found\r\nLocation: /file\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                } else if head.starts_with("GET /file") {
                    let mut r = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    r.extend_from_slice(&body);
                    r
                } else {
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_vec()
                };
                let _ = socket.write_all(&response).await;
            });
        }
    });
    format!("http://{address}")
}

#[tokio::test]
async fn download_follows_redirects_and_reports_http_failures() {
    let root = tempfile::tempdir().unwrap();
    let state: Arc<dyn StateStore> = Arc::new(MemoryStateStore::default());
    let check = Arc::new(JarCheck(AtomicUsize::new(0)));
    let (_kernel, provider) = start(state, root.path(), check).await.unwrap();
    let body = b"PK-downloaded".to_vec();
    let base = serve(body.clone()).await;
    let acquired = provider
        .acquire(request(
            AcquireSource::Url(format!("{base}/redirect")),
            Some(sha(&body)),
            1,
        ))
        .await
        .unwrap();
    assert_eq!(
        acquired.source,
        AcquireSource::Url(format!("{base}/redirect"))
    );
    let missing = provider
        .acquire(request(
            AcquireSource::Url(format!("{base}/missing")),
            None,
            2,
        ))
        .await;
    assert_eq!(missing, Err(ProvisionError::Fetch("HTTP 404".into())));
    let plain = provider
        .acquire(request(
            AcquireSource::Url("http://example.com/x.jar".into()),
            None,
            3,
        ))
        .await;
    assert!(matches!(plain, Err(ProvisionError::InvalidInput(_))));
    assert_eq!(
        provider.ready_path("sample-server").unwrap(),
        Some(root.path().join(acquired.path))
    );
}

#[tokio::test]
async fn corrupt_or_unknown_ledger_refuses_to_start_and_keeps_the_bytes() {
    let root = tempfile::tempdir().unwrap();
    let id = PluginId::new(PROVISION_PLUGIN_ID).unwrap();
    for bytes in [
        b"{not json".to_vec(),
        br#"{"format_version":9,"conditions":{}}"#.to_vec(),
        br#"{"format_version":1,"conditions":{"Bad Id":{}}}"#.to_vec(),
    ] {
        let state = Arc::new(MemoryStateStore::default());
        state
            .set(&id, PROVISION_STATE_KEY.into(), bytes.clone())
            .unwrap();
        let check = Arc::new(JarCheck(AtomicUsize::new(0)));
        assert!(start(state.clone(), root.path(), check).await.is_err());
        assert_eq!(state.get(&id, PROVISION_STATE_KEY).unwrap(), Some(bytes));
    }
}
