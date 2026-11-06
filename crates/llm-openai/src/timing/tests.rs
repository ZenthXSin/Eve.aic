use super::{CallDiagnostics, RequestIdentity, RequestTiming, TransportPhase, io_details};
use serde_json::json;
use std::{
    error::Error,
    fmt, io,
    net::SocketAddr,
    time::{Duration, Instant},
};

#[derive(Debug)]
struct WrappedIo(io::Error);
impl fmt::Display for WrappedIo {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        panic!("诊断不得格式化原始错误消息")
    }
}
impl Error for WrappedIo {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

#[test]
fn io_cause_keeps_category_and_errno_without_formatting_messages() {
    let timeout = WrappedIo(io::Error::new(
        io::ErrorKind::TimedOut,
        "fixture-private-body",
    ));
    assert_eq!(io_details(&timeout), (Some("TimedOut".into()), None));
    let system = WrappedIo(io::Error::from_raw_os_error(110));
    assert_eq!(io_details(&system).1, Some(110));
}

#[test]
fn transport_fields_include_only_safe_metadata_and_the_existing_request_id() {
    let error = reqwest::Client::new()
        .get("https://fixture.invalid/private")
        .header("fixture", "fixture-private-header\n")
        .build()
        .unwrap_err()
        .with_url(
            reqwest::Url::parse("https://fixture.invalid/private?credential=fixture-secret")
                .unwrap(),
        );
    let timing = RequestTiming {
        identity: RequestIdentity {
            call: 21,
            request: Some(42),
            attempt: Some(2),
            role: Some("dialogue"),
        },
        started: Instant::now(),
        finished: true,
    };
    let fields = timing.transport_fields(TransportPhase::BeforeHeaders, &error);
    assert_eq!(fields["request"], 42);
    assert_eq!(fields["call"], 21);
    assert_eq!(fields["attempt"], 2);
    assert_eq!(fields["role"], "dialogue");
    assert_eq!(fields["phase"], "before_headers");
    assert_eq!(fields["is_connect"], false);
    assert_eq!(fields["is_timeout"], false);
    assert!(fields["io_kind"].is_null());
    assert!(fields["io_os_error"].is_null());
    assert!(fields["at_ms"].is_u64());
    assert!(fields["elapsed_ms"].is_u64());
    assert_eq!(fields.as_object().unwrap().len(), 11);
    for private in ["fixture", "credential", "https://", "private", "header\n"] {
        assert!(!fields.to_string().contains(private));
    }
    assert_eq!(json!(TransportPhase::ResponseBody), "response_body");
    assert_eq!(json!(TransportPhase::StreamBody), "stream_body");
}

#[tokio::test]
async fn real_transport_errors_distinguish_connect_failure_and_waiting_timeout() {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    // 占住端口但不监听，避免释放临时端口后被其他测试抢占。
    let unopened = tokio::net::TcpSocket::new_v4().unwrap();
    unopened
        .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .unwrap();
    let url = format!("http://{}/fixture-private", unopened.local_addr().unwrap());
    let error = client.get(url).send().await.unwrap_err();
    let timing = RequestTiming {
        identity: CallDiagnostics::start(Some("speech")).next_attempt(),
        started: Instant::now(),
        finished: true,
    };
    let fields = timing.transport_fields(TransportPhase::BeforeHeaders, &error);
    assert_eq!(fields["is_connect"], true);
    assert_eq!(fields["is_timeout"], false);
    assert_eq!(fields["io_kind"], "ConnectionRefused");
    assert!(fields["io_os_error"].is_i64());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/fixture-private", listener.local_addr().unwrap());
    let pending = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let _socket = socket;
        std::future::pending::<()>().await;
    });
    let error = client
        .get(url)
        .timeout(Duration::from_millis(100))
        .send()
        .await
        .unwrap_err();
    pending.abort();
    let _ = pending.await;
    let fields = timing.transport_fields(TransportPhase::BeforeHeaders, &error);
    assert_eq!(fields["is_connect"], false);
    assert_eq!(fields["is_timeout"], true);
    assert!(fields["io_kind"].is_null());
    assert!(fields["io_os_error"].is_null());
}

#[test]
fn calls_and_attempts_have_distinct_local_ids_and_only_fixed_roles() {
    let mut call = CallDiagnostics::start(Some("speech"));
    let before = call.identity().fields();
    assert_eq!(before["role"], "speech");
    assert!(before["request"].is_null());
    assert!(before["attempt"].is_null());
    let first = call.next_attempt().fields();
    let second = call.next_attempt().fields();
    assert_eq!(first["call"], second["call"]);
    assert_ne!(first["request"], second["request"]);
    assert_eq!(first["attempt"], 1);
    assert_eq!(second["attempt"], 2);
    assert_eq!(call.identity().fields(), second);
    let other = CallDiagnostics::start(Some("dialogue"))
        .next_attempt()
        .fields();
    assert_ne!(first["call"], other["call"]);
    assert_ne!(first["request"], other["request"]);
    assert_eq!(other["attempt"], 1);
    for role in [
        None,
        Some("fixture-private-role"),
        Some("dialogue\nfixture-private"),
    ] {
        let fields = CallDiagnostics::start(role).next_attempt().fields();
        assert!(fields["role"].is_null());
        assert!(!fields.to_string().contains("fixture"));
    }
    assert_eq!(
        CallDiagnostics::start(Some("exploration"))
            .identity()
            .fields()["role"],
        "exploration"
    );
}
